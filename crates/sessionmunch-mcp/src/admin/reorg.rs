//! Admin routes: reorg.

use super::*;

/// JSON request body for `POST /admin/reorg`.
#[derive(Deserialize)]
pub(super) struct ReorgRequest {
    /// Show what would change without writing.
    #[serde(default)]
    dry_run: bool,
}

/// One entry in the reorg plan (serialised in the response).
#[derive(Debug, Serialize)]
pub struct ReorgPlanEntry {
    /// Session UUID string.
    pub session_id: String,
    /// Working directory the session was started in.
    pub cwd: String,
    /// Basename-derived project name the session will move to.
    pub new_project: String,
}

/// Summary counts returned alongside the plan.
#[derive(Debug, Serialize)]
pub struct ReorgSummaryJson {
    /// Sessions whose `project_id` was changed.
    pub sessions_moved: usize,
    /// Observations updated to match their session's new project.
    pub observations_updated: usize,
    /// `is_latest=1` pages marked `is_latest=0` (mash-up graveyard).
    pub pages_graveyarded: usize,
    /// Number of distinct per-cwd projects referenced in the plan.
    pub distinct_new_projects: usize,
}

/// Full response for `POST /admin/reorg`.
#[derive(Debug, Serialize)]
pub struct ReorgReport {
    /// `true` when `dry_run` was requested.
    pub dry_run: bool,
    /// All sessions that need (or needed) moving, with their target project.
    pub plan: Vec<ReorgPlanEntry>,
    /// Counts after execution (zeros when `dry_run=true`).
    pub summary: ReorgSummaryJson,
}

/// Read every session that has a non-empty `cwd` field, returning
/// `(session_id, project_id, cwd)` triples ordered by `started_at`.
pub(super) async fn list_sessions_with_cwd(
    reader: &sessionmunch_store::ReaderPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(SessionId, ProjectId, String)>, StoreError> {
    reader
        .with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, project_id, cwd \
                     FROM sessions \
                     WHERE workspace_id = ?1 AND cwd IS NOT NULL AND cwd != '' \
                     ORDER BY started_at",
            )?;
            let rows = stmt.query_map([workspace_id.as_bytes().as_slice()], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let proj_bytes: Vec<u8> = row.get(1)?;
                let cwd: String = row.get(2)?;
                Ok((id_bytes, proj_bytes, cwd))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (id_bytes, proj_bytes, cwd) = r?;
                let sid = SessionId::from_slice(&id_bytes).map_err(StoreError::Memory)?;
                let pid = ProjectId::from_slice(&proj_bytes).map_err(StoreError::Memory)?;
                out.push((sid, pid, cwd));
            }
            Ok(out)
        })
        .await
}

/// Build the reorg plan: for each session with a new project basename,
/// resolve-or-create the target project, return the plan entries plus
/// the set of session updates and the distinct-project count.
pub(super) async fn build_reorg_plan(
    state: &AdminState,
    ws: WorkspaceId,
    sessions: Vec<(SessionId, ProjectId, String)>,
) -> Result<(Vec<ReorgPlanEntry>, Vec<(SessionId, ProjectId)>, usize), StoreError> {
    // Resolve target project per distinct cwd (basename-derived).
    let mut cwd_to_proj: std::collections::HashMap<String, (WorkspaceId, ProjectId, String)> =
        std::collections::HashMap::new();
    for (_, _, cwd) in &sessions {
        if cwd_to_proj.contains_key(cwd.as_str()) {
            continue;
        }
        let project_name = std::path::Path::new(cwd.as_str())
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let proj = state
            .writer
            .get_or_create_project(ws, project_name.clone(), repo_path_from_reorg_cwd(cwd))
            .await?;
        cwd_to_proj.insert(cwd.clone(), (ws, proj, project_name));
    }

    // Build plan — sessions whose project_id already matches are skipped.
    let mut plan_entries: Vec<ReorgPlanEntry> = Vec::new();
    let mut writer_plan: Vec<(SessionId, ProjectId)> = Vec::new();
    for (session_id, old_project_id, cwd) in &sessions {
        let (_, new_project_id, project_name) = &cwd_to_proj[cwd.as_str()];
        if *new_project_id == *old_project_id {
            continue;
        }
        plan_entries.push(ReorgPlanEntry {
            session_id: session_id.to_string(),
            cwd: cwd.clone(),
            new_project: project_name.clone(),
        });
        writer_plan.push((*session_id, *new_project_id));
    }

    let distinct_new_projects: std::collections::HashSet<ProjectId> =
        writer_plan.iter().map(|(_, pid)| *pid).collect();

    Ok((plan_entries, writer_plan, distinct_new_projects.len()))
}

pub(super) fn repo_path_from_reorg_cwd(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    let repo_root = sessionmunch_consolidate::discover_repo_root(path).ok()?;
    cwd_is_repo_root(path, &repo_root).then(|| repo_root.to_string_lossy().into_owned())
}

pub(super) fn cwd_is_repo_root(cwd: &std::path::Path, repo_root: &std::path::Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(cwd), std::fs::canonicalize(repo_root)) {
        return a == b;
    }
    let strip = |p: &std::path::Path| p.to_string_lossy().trim_end_matches('/').to_string();
    strip(cwd) == strip(repo_root)
}

pub(super) async fn handle_reorg(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<ReorgRequest>,
) -> impl IntoResponse {
    let ws = match state
        .writer
        .get_or_create_workspace(DEFAULT_WORKSPACE_NAME)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("workspace: {e}") })),
            );
        }
    };

    let sessions = match list_sessions_with_cwd(&state.reader, ws).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    if sessions.is_empty() {
        let report = ReorgReport {
            dry_run: req.dry_run,
            plan: Vec::new(),
            summary: ReorgSummaryJson {
                sessions_moved: 0,
                observations_updated: 0,
                pages_graveyarded: 0,
                distinct_new_projects: 0,
            },
        };
        return (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        );
    }

    let (plan_entries, writer_plan, distinct_count) =
        match build_reorg_plan(&state, ws, sessions).await {
            Ok(t) => t,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("project: {e}") })),
                );
            }
        };

    if req.dry_run || writer_plan.is_empty() {
        let report = ReorgReport {
            dry_run: req.dry_run,
            plan: plan_entries,
            summary: ReorgSummaryJson {
                sessions_moved: 0,
                observations_updated: 0,
                pages_graveyarded: 0,
                distinct_new_projects: distinct_count,
            },
        };
        return (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        );
    }

    let summary = match state.writer.reorg_sessions(ws, writer_plan).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    let report = ReorgReport {
        dry_run: false,
        plan: plan_entries,
        summary: ReorgSummaryJson {
            sessions_moved: summary.sessions_moved,
            observations_updated: summary.observations_updated,
            pages_graveyarded: summary.pages_graveyarded,
            distinct_new_projects: distinct_count,
        },
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    )
}
