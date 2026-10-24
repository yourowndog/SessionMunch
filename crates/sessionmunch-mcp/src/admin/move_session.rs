//! Admin routes: move-session.

use super::*;

/// JSON request body for `POST /admin/move-session`.
///
/// Two shapes share the route: a single move names `session_id`; a batch
/// names `from_project` (plus optional `from_workspace`) and moves every
/// session touching that scope (a `sessions` row there OR observations
/// stamped into it). Exactly one of the two must be present. A session whose
/// row already sits in the destination is re-homed: only its rows still
/// lying elsewhere move (`session_moved: false`).
#[derive(Deserialize)]
pub(super) struct MoveSessionRequest {
    /// The session to move (single form).
    #[serde(default)]
    session_id: Option<SessionId>,
    /// Source workspace of the batch form. Defaults to `default`.
    #[serde(default)]
    from_workspace: Option<String>,
    /// Source project of the batch form: every session touching it moves.
    #[serde(default)]
    from_project: Option<String>,
    /// Destination workspace. Defaults to the source workspace.
    #[serde(default)]
    workspace: Option<String>,
    /// Destination project. Must already exist unless `create` is set.
    project: String,
    /// What happens to `sessions/<id>.md`: `move` (default) carries the page
    /// and its history along; `regenerate` retires it in the source so the
    /// next consolidation writes a fresh one in the destination.
    #[serde(default)]
    pages: PagesMode,
    /// Without `confirm: true` the server runs the move inside a rolled-back
    /// transaction and reports what would change (`dry_run: true`).
    #[serde(default)]
    confirm: bool,
    /// Skip the live-session guards: an open session (no session end
    /// recorded; not applied to a re-home, whose row does not move), a
    /// `pending`/`running` consolidation job, and, for the batch form, the
    /// hook router's active-project pointer.
    #[serde(default)]
    force: bool,
    /// Create the destination workspace/project when absent instead of 404.
    /// A dry run never creates: it reports `would_create_project: true` and
    /// the counts computed against an absent target.
    #[serde(default)]
    create: bool,
}

/// `workspace/project` pair as names, for report labels.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeLabel {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
}

/// Row counts of one session move; identical for the dry run.
#[derive(Debug, Clone, Serialize)]
pub struct MoveSessionCounts {
    /// `observations` rows re-stamped.
    pub observations: u64,
    /// `handoffs` rows (produced by the session) re-stamped.
    pub handoffs: u64,
    /// `session_consolidation_jobs` rows re-stamped.
    pub consolidation_jobs: u64,
    /// `auto_improve_runs` rows re-stamped.
    pub auto_improve_runs: u64,
    /// `auto_improve_scheduler_claims` rows re-stamped.
    pub auto_improve_claims: u64,
    /// `pages` versions of `sessions/<id>.md` re-stamped (`pages: move`).
    pub page_versions_moved: u64,
    /// `pages` rows of `sessions/<id>.md` retired (`pages: regenerate`).
    pub pages_regenerated: u64,
}

/// One scope a move gathers observations out of, for the operator's report.
#[derive(Debug, Serialize)]
pub struct MoveSessionScopeCount {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Observations of the session currently stamped into that scope.
    pub observations: u64,
}

/// Wire-format report for one session in `POST /admin/move-session`.
#[derive(Debug, Serialize)]
pub struct MoveSessionReport {
    /// The moved session.
    pub session_id: String,
    /// `true` when nothing was written (no `confirm`).
    pub dry_run: bool,
    /// Whether the `sessions` row changed scope. `false` for a re-home: the
    /// row already sat in the destination and only stray rows (the counts
    /// below) were gathered into it.
    pub session_moved: bool,
    /// Dry run with `create` against a destination that does not exist yet:
    /// the confirm run will create it. Never set on a confirmed move.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub would_create_project: bool,
    /// Every scope holding observations of this session other than the
    /// destination, with counts. More than one entry — or one that is not
    /// `from` — means the move drains a project the caller did not name.
    /// Empty when everything already sits in the destination.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_scopes: Vec<MoveSessionScopeCount>,
    /// Scope the session came from.
    pub from: ScopeLabel,
    /// Scope the session now belongs to.
    pub to: ScopeLabel,
    /// Row counts.
    pub summary: MoveSessionCounts,
    /// What happened (or would happen) to `sessions/<id>.md`: `"moved"`,
    /// `"regenerated"`, `"already in destination"` (re-home whose page rows
    /// all sit in the destination), or `"none"` when the session has no page
    /// anywhere.
    pub page: &'static str,
    /// The session's recorded working directory, left untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Set when `basename(cwd)` differs from the destination project name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_warning: Option<String>,
    /// Pre-move checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-move checkpoint, if the move changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// Wire-format report for the batch form of `POST /admin/move-session`.
#[derive(Debug, Serialize)]
pub struct MoveSessionBatchReport {
    /// `true` when nothing was written (no `confirm`).
    pub dry_run: bool,
    /// Dry run with `create` against a destination that does not exist yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub would_create_project: bool,
    /// Source scope.
    pub from: ScopeLabel,
    /// Destination scope.
    pub to: ScopeLabel,
    /// Sessions touching the source scope (a `sessions` row there or at
    /// least one observation stamped into it).
    pub total: usize,
    /// Sessions moved or re-homed (or, for a dry run, that would be).
    pub moved: usize,
    /// One report per session, oldest first (row `started_at` or first
    /// observation in the source).
    pub sessions: Vec<MoveSessionReport>,
    /// Pre-move checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-move checkpoint, if the batch changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// One session move, resolved and validated, before any guard runs.
pub(super) struct MoveSessionPlan {
    session_id: SessionId,
    /// Scope the rows are gathered from: the session row's own scope in the
    /// single form, the batch source in the batch form (where the row may
    /// well sit elsewhere, even in `to`).
    from: (WorkspaceId, ProjectId),
    from_label: ScopeLabel,
    to: MoveTarget,
    to_label: ScopeLabel,
    /// Where the `sessions` row sits right now.
    row_scope: (WorkspaceId, ProjectId),
    cwd: Option<String>,
}

impl MoveSessionPlan {
    /// The row already sits in the destination: only stray rows move.
    fn is_rehome(&self) -> bool {
        self.to.existing() == Some(self.row_scope)
    }
}

/// The destination of a move: an existing scope, or (dry run with `create`
/// only) a scope that does not exist yet and would be created on confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MoveTarget {
    Existing((WorkspaceId, ProjectId)),
    WouldCreate,
}

impl MoveTarget {
    fn existing(self) -> Option<(WorkspaceId, ProjectId)> {
        match self {
            Self::Existing(scope) => Some(scope),
            Self::WouldCreate => None,
        }
    }
}

pub(super) async fn handle_move_session(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    Json(req): Json<MoveSessionRequest>,
) -> Response {
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(u)| u);
    match (req.session_id, req.from_project.as_deref()) {
        (Some(session_id), None) => {
            match move_single_session(&state, &req, session_id, author_id, actor).await {
                Ok(report) => (StatusCode::OK, Json(json_or_empty(&report))).into_response(),
                Err(resp) => resp.into_response(),
            }
        }
        (None, Some(from_project)) => {
            move_session_batch(&state, &req, from_project, author_id, actor).await
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "provide exactly one of session_id (single move) or from_project (batch)"
            })),
        )
            .into_response(),
    }
}

pub(super) fn json_or_empty<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({}))
}

/// Best-effort names for a scope; ids stand in when a row vanished
/// underneath us (a concurrent purge), so a report is still produced.
pub(super) async fn scope_label(
    state: &AdminState,
    ws: WorkspaceId,
    proj: ProjectId,
) -> ScopeLabel {
    let workspace = state
        .reader
        .workspace_name_by_id(ws)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| ws.to_string());
    let project = state
        .reader
        .project_name_by_id(ws, proj)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| proj.to_string());
    ScopeLabel { workspace, project }
}

/// Resolve the destination scope: existing only (404) unless `create`. A
/// dry run never writes, so with `create` and no `confirm` an absent
/// destination resolves to [`MoveTarget::WouldCreate`] instead of a row.
pub(super) async fn resolve_move_session_target(
    state: &AdminState,
    workspace: &str,
    project: &str,
    create: bool,
    confirm: bool,
) -> Result<MoveTarget, MoveErr> {
    if create && confirm {
        return create_ws_proj(state, workspace, project)
            .await
            .map(MoveTarget::Existing);
    }
    match lookup_existing_scope(&state.reader, workspace, project).await {
        Ok(scope) => Ok(MoveTarget::Existing(scope.as_tuple())),
        Err(e) if create && e.is_not_found() => Ok(MoveTarget::WouldCreate),
        Err(e) => Err(scope_err(e)),
    }
}

/// Dry-run counts of a move into a destination that does not exist yet:
/// nothing of the session can already be there, so every row keyed to it
/// moves, and the page rows are those of `from` (moved or retired per
/// `pages`). Read-only; the store never sees the absent target.
pub(super) async fn preview_move_into_absent_target(
    reader: &ReaderPool,
    session_id: SessionId,
    from: (WorkspaceId, ProjectId),
    pages: PagesMode,
) -> Result<MoveSessionCounts, StoreError> {
    let rows = reader.session_dependent_rows(session_id, from).await?;
    let (page_versions_moved, pages_regenerated) = match pages {
        PagesMode::Move => (rows.page_versions, 0),
        PagesMode::Regenerate => (0, rows.page_latest),
    };
    Ok(MoveSessionCounts {
        observations: rows.observations,
        handoffs: rows.handoffs,
        consolidation_jobs: rows.consolidation_jobs,
        auto_improve_runs: rows.auto_improve_runs,
        auto_improve_claims: rows.auto_improve_claims,
        page_versions_moved,
        pages_regenerated,
    })
}

/// `(session still open, consolidation job pending or running)` for the
/// live-session guards.
pub(super) async fn session_move_blockers(
    reader: &ReaderPool,
    session_id: SessionId,
) -> Result<(bool, bool), StoreError> {
    reader
        .with_conn(move |conn| {
            let open: bool = conn.query_row(
                "SELECT ended_at IS NULL FROM sessions WHERE id = ?1",
                [session_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            let pending: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_consolidation_jobs \
                 WHERE session_id = ?1 AND state IN ('pending', 'running'))",
                [session_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            Ok((open, pending))
        })
        .await
}

/// `(session_id, row scope, cwd)` of every session touching a scope (a
/// `sessions` row there or observations stamped into it), oldest first. The
/// row scope may differ from the queried scope: that is the phantom-bucket
/// case the batch re-homes.
pub(super) async fn list_scope_sessions(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> Result<Vec<(SessionId, (WorkspaceId, ProjectId), Option<String>)>, StoreError> {
    let ids = reader
        .session_ids_touching_scope(workspace_id, project_id)
        .await?;
    let mut out = Vec::with_capacity(ids.len());
    for sid in ids {
        // Observations reference `sessions` (FK on), so a listed id always
        // has a row; a concurrent purge is the only way to lose it here.
        if let Some((ws, proj, cwd)) = reader.find_session_scope(sid).await? {
            out.push((sid, (ws, proj), cwd));
        }
    }
    Ok(out)
}

pub(super) fn move_session_store_err(e: StoreError) -> MoveErr {
    match e {
        StoreError::NotFound(msg) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": msg })),
        ),
        e @ StoreError::PagePathTaken { .. } => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{e}; re-run with pages=regenerate or resolve the destination page first"
                )
            })),
        ),
        other => internal_err(other.to_string()),
    }
}

pub(super) fn move_session_wiki_err(e: WikiError) -> MoveErr {
    match e {
        WikiError::Store(store_err) => move_session_store_err(store_err),
        e @ WikiError::DestinationPageExists(_) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
        other => internal_err(format!("move-session aborted: {other}")),
    }
}

/// The session's cwd is historical truth and stays put; say so when the
/// hook router's basename heuristic would not map it to the destination.
pub(super) fn move_session_cwd_warning(cwd: Option<&str>, to_project: &str) -> Option<String> {
    let cwd = cwd?;
    let base = std::path::Path::new(cwd).file_name()?.to_str()?;
    if base == to_project {
        return None;
    }
    Some(format!(
        "session cwd '{cwd}' resolves to project '{base}' by basename, not '{to_project}'; \
         the cwd was left as recorded. New sessions started there still land in '{base}' \
         unless a .sessionmunch.toml marker pins project = \"{to_project}\"; with \
         [routing] mid_session = \"sticky\" the remaining events of this session follow it."
    ))
}

pub(super) fn move_session_source_scopes(
    summary: &sessionmunch_store::MoveSessionSummary,
) -> Vec<MoveSessionScopeCount> {
    summary
        .source_scopes
        .iter()
        .map(|scope| MoveSessionScopeCount {
            workspace: scope.workspace_name.clone(),
            project: scope.project_name.clone(),
            observations: scope.observations,
        })
        .collect()
}

pub(super) fn move_session_counts(
    summary: &sessionmunch_store::MoveSessionSummary,
) -> MoveSessionCounts {
    MoveSessionCounts {
        observations: summary.observations,
        handoffs: summary.handoffs,
        consolidation_jobs: summary.consolidation_jobs,
        auto_improve_runs: summary.auto_improve_runs,
        auto_improve_claims: summary.auto_improve_claims,
        page_versions_moved: summary.page_versions_moved,
        pages_regenerated: summary.pages_regenerated,
    }
}

pub(super) fn move_session_page_label(
    pages: PagesMode,
    touched_rows: bool,
    touched_file: bool,
    already_in_target: bool,
) -> &'static str {
    match (pages, touched_rows || touched_file) {
        (_, false) if already_in_target => "already in destination",
        (_, false) => "none",
        (PagesMode::Move, true) => "moved",
        (PagesMode::Regenerate, true) => "regenerated",
    }
}

/// Guards, then the dry run or the real move of one planned session. The
/// caller wraps the call in the pre/post checkpoints so a batch takes one
/// pair, not one per session.
pub(super) async fn move_planned_session(
    state: &Arc<AdminState>,
    req: &MoveSessionRequest,
    plan: MoveSessionPlan,
    author_id: Option<sessionmunch_core::UserId>,
    actor: &sessionmunch_core::ActorContext,
) -> Result<MoveSessionReport, MoveErr> {
    let sid = plan.session_id;
    let rehome = plan.is_rehome();

    // Live-session guards. An open session may still receive events, and a
    // queued consolidation would write the page under the old scope; both
    // are the operator's call with `force`. A re-home leaves the row where
    // it is, so an open session is no reason to refuse gathering its strays.
    let (open, pending_job) = session_move_blockers(&state.reader, sid)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    if !req.force {
        if open && !rehome {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!(
                        "session {sid} is still open (no session end recorded); \
                         finish it or force the move (--force / force: true)"
                    )
                })),
            ));
        }
        if pending_job {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!(
                        "session {sid} has a pending or running consolidation job; \
                         wait for it or force the move (--force / force: true)"
                    )
                })),
            ));
        }
    }

    let cwd_warning = move_session_cwd_warning(plan.cwd.as_deref(), &plan.to_label.project);
    let file_name = format!("{sid}.md");
    let src_file = state
        .wiki
        .project_root(plan.from.0, plan.from.1)
        .join("sessions")
        .join(&file_name);

    // Dry run with `create` against an absent destination: nothing exists to
    // collide with, and the store cannot re-stamp into a scope that has no
    // row, so the counts come from a read-only preview.
    let to = match plan.to {
        MoveTarget::Existing(to) => to,
        MoveTarget::WouldCreate => {
            debug_assert!(
                !req.confirm,
                "an absent target is only planned for a dry run"
            );
            let counts = preview_move_into_absent_target(&state.reader, sid, plan.from, req.pages)
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            let touched_rows = counts.page_versions_moved > 0 || counts.pages_regenerated > 0;
            return Ok(MoveSessionReport {
                session_id: sid.to_string(),
                dry_run: true,
                session_moved: true,
                would_create_project: true,
                source_scopes: Vec::new(),
                from: plan.from_label,
                to: plan.to_label,
                page: move_session_page_label(req.pages, touched_rows, src_file.is_file(), false),
                summary: counts,
                cwd: plan.cwd,
                cwd_warning,
                pre_checkpoint: None,
                checkpoint: None,
            });
        }
    };

    // A page file already at the destination blocks a page move; checked here
    // too so the dry run predicts what the wiki re-checks under its lock. When
    // the source scope IS the destination (single-form re-home) there is no
    // file to move and the wiki skips the file step too.
    let dst_file = state
        .wiki
        .project_root(to.0, to.1)
        .join("sessions")
        .join(&file_name);
    let file_moves = plan.from != to && src_file.is_file();
    if req.pages == PagesMode::Move && file_moves && dst_file.exists() {
        return Err(move_session_wiki_err(WikiError::DestinationPageExists(
            dst_file.display().to_string(),
        )));
    }

    if !req.confirm {
        let summary = state
            .writer
            .move_session(sid, to.0, to.1, req.pages, author_id, false)
            .await
            .map_err(move_session_store_err)?;
        let touched_rows = summary.page_versions_moved > 0 || summary.pages_regenerated > 0;
        return Ok(MoveSessionReport {
            session_id: sid.to_string(),
            dry_run: true,
            session_moved: summary.session_moved,
            would_create_project: false,
            source_scopes: move_session_source_scopes(&summary),
            from: plan.from_label,
            to: plan.to_label,
            summary: move_session_counts(&summary),
            page: move_session_page_label(
                req.pages,
                touched_rows,
                file_moves,
                summary.page_versions_in_target > 0,
            ),
            cwd: plan.cwd,
            cwd_warning,
            pre_checkpoint: None,
            checkpoint: None,
        });
    }

    let move_ctx = AdmissionContext {
        workspace: plan.from_label.workspace.clone(),
        project: plan.from_label.project.clone(),
        destination_workspace: Some(plan.to_label.workspace.clone()),
        destination_project: Some(plan.to_label.project.clone()),
        op: AdmissionOp::MoveSession,
        actor: actor.clone(),
        ..Default::default()
    };
    // The wiki owns the critical section: admission, file move, SQL
    // re-stamp, and the file rollback on SQL failure.
    let outcome = state
        .wiki
        .move_session_page(sid, plan.from, to, req.pages, author_id, Some(move_ctx))
        .await
        .map_err(move_session_wiki_err)?;
    let touched_rows =
        outcome.summary.page_versions_moved > 0 || outcome.summary.pages_regenerated > 0;
    Ok(MoveSessionReport {
        session_id: sid.to_string(),
        dry_run: false,
        session_moved: outcome.summary.session_moved,
        would_create_project: false,
        source_scopes: move_session_source_scopes(&outcome.summary),
        from: plan.from_label,
        to: plan.to_label,
        summary: move_session_counts(&outcome.summary),
        page: move_session_page_label(
            req.pages,
            touched_rows,
            outcome.file != SessionPageFile::Absent,
            outcome.summary.page_versions_in_target > 0,
        ),
        cwd: plan.cwd,
        cwd_warning,
        pre_checkpoint: None,
        checkpoint: None,
    })
}

pub(super) async fn move_single_session(
    state: &Arc<AdminState>,
    req: &MoveSessionRequest,
    session_id: SessionId,
    author_id: Option<sessionmunch_core::UserId>,
    actor: sessionmunch_core::ActorContext,
) -> Result<MoveSessionReport, MoveErr> {
    let (from_ws, from_proj, cwd) = state
        .reader
        .find_session_scope(session_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("session {session_id} not found") })),
            )
        })?;
    let from_label = scope_label(state, from_ws, from_proj).await;
    let to_workspace = req
        .workspace
        .clone()
        .unwrap_or_else(|| from_label.workspace.clone());
    let to =
        resolve_move_session_target(state, &to_workspace, &req.project, req.create, req.confirm)
            .await?;
    let plan = MoveSessionPlan {
        session_id,
        from: (from_ws, from_proj),
        from_label,
        to,
        to_label: ScopeLabel {
            workspace: to_workspace,
            project: req.project.clone(),
        },
        row_scope: (from_ws, from_proj),
        cwd,
    };

    if !req.confirm {
        return move_planned_session(state, req, plan, author_id, &actor).await;
    }
    let label = format!(
        "move-session {session_id} {}/{} -> {}/{}",
        plan.from_label.workspace,
        plan.from_label.project,
        plan.to_label.workspace,
        plan.to_label.project
    );
    let pre_checkpoint = checkpoint_or_500(&state.wiki, format!("pre-{label}"))?;
    let mut report = move_planned_session(state, req, plan, author_id, &actor).await?;
    if let Err(e) = state.wiki.backfill_scope_manifests().await {
        warn!(error = %e, "move-session: scope-manifest backfill failed after move");
    }
    report.pre_checkpoint = pre_checkpoint;
    report.checkpoint = checkpoint_or_warn(&state.wiki, label);
    Ok(report)
}

pub(super) async fn move_session_batch(
    state: &Arc<AdminState>,
    req: &MoveSessionRequest,
    from_project: &str,
    author_id: Option<sessionmunch_core::UserId>,
    actor: sessionmunch_core::ActorContext,
) -> Response {
    let from_workspace = req
        .from_workspace
        .clone()
        .unwrap_or_else(|| DEFAULT_WORKSPACE_NAME.to_string());
    let from = match lookup_ws_proj_no_create(state, &from_workspace, from_project).await {
        Ok(ids) => ids,
        Err(e) => return e.into_response(),
    };
    let to_workspace = req
        .workspace
        .clone()
        .unwrap_or_else(|| from_workspace.clone());
    let to = match resolve_move_session_target(
        state,
        &to_workspace,
        &req.project,
        req.create,
        req.confirm,
    )
    .await
    {
        Ok(target) => target,
        Err(e) => return e.into_response(),
    };
    let from_label = ScopeLabel {
        workspace: from_workspace,
        project: from_project.to_string(),
    };
    let to_label = ScopeLabel {
        workspace: to_workspace,
        project: req.project.clone(),
    };
    if to.existing() == Some(from) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "source and destination scopes are identical"
            })),
        )
            .into_response();
    }
    // Same guard as move-project: the project the hook router is writing to
    // is not emptied out from under it without an explicit `force`.
    if !req.force && state.active_project.contains_project(from.1) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{}/{} is the active session's project; force the move (--force / \
                     force: true) to move its sessions anyway",
                    from_label.workspace, from_label.project
                )
            })),
        )
            .into_response();
    }
    let sessions = match list_scope_sessions(&state.reader, from.0, from.1).await {
        Ok(v) => v,
        Err(e) => return internal_err(e.to_string()).into_response(),
    };

    let label = format!(
        "move-session {}/{} -> {}/{}",
        from_label.workspace, from_label.project, to_label.workspace, to_label.project
    );
    let pre_checkpoint = if req.confirm && !sessions.is_empty() {
        match checkpoint_or_500(&state.wiki, format!("pre-{label}")) {
            Ok(oid) => oid,
            Err(e) => return e.into_response(),
        }
    } else {
        None
    };

    let total = sessions.len();
    let mut reports = Vec::with_capacity(total);
    let mut failure: Option<(SessionId, MoveErr)> = None;
    for (session_id, row_scope, cwd) in sessions {
        // The page file follows the page rows: a session rooted in a third
        // scope (only some observations in the source) is moved whole from
        // where its row sits; a session already rooted in the destination is
        // re-homed and its stray page file, if any, is looked for in the
        // source.
        let (plan_from, plan_from_label) = if row_scope == from || to.existing() == Some(row_scope)
        {
            (from, from_label.clone())
        } else {
            (
                row_scope,
                scope_label(state, row_scope.0, row_scope.1).await,
            )
        };
        let plan = MoveSessionPlan {
            session_id,
            from: plan_from,
            from_label: plan_from_label,
            to,
            to_label: to_label.clone(),
            row_scope,
            cwd,
        };
        match move_planned_session(state, req, plan, author_id, &actor).await {
            Ok(report) => reports.push(report),
            Err(e) => {
                failure = Some((session_id, e));
                break;
            }
        }
    }

    let checkpoint = if req.confirm && !reports.is_empty() {
        if let Err(e) = state.wiki.backfill_scope_manifests().await {
            warn!(error = %e, "move-session: scope-manifest backfill failed after batch");
        }
        checkpoint_or_warn(&state.wiki, label)
    } else {
        None
    };

    // Stop at the first error, but say how far the batch got: the sessions
    // already moved stay moved (each was its own transaction).
    if let Some((session_id, (status, Json(mut body)))) = failure {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "session_id".into(),
                serde_json::json!(session_id.to_string()),
            );
            obj.insert("dry_run".into(), serde_json::json!(!req.confirm));
            obj.insert("total".into(), serde_json::json!(total));
            obj.insert("moved".into(), serde_json::json!(reports.len()));
            obj.insert("sessions".into(), json_or_empty(&reports));
            if let Some(oid) = checkpoint {
                obj.insert("checkpoint".into(), serde_json::json!(oid));
            }
        }
        return (status, Json(body)).into_response();
    }
    let report = MoveSessionBatchReport {
        dry_run: !req.confirm,
        would_create_project: to == MoveTarget::WouldCreate,
        from: from_label,
        to: to_label,
        total,
        moved: reports.len(),
        sessions: reports,
        pre_checkpoint,
        checkpoint,
    };
    (StatusCode::OK, Json(json_or_empty(&report))).into_response()
}

/// One source page, with everything the copy loop and the block
/// pre-check both need. Built in ONE pass over the source listing so
/// the previous two-pass implementation's double-IO (pre-scan + copy
/// loop each calling `page_meta` + `page_body_by_ids` + `read_page`)
/// collapses to a single read per page.
pub(super) struct PreparedSourcePage {
    path: PagePath,
    /// Path as a String — only needed for reports / `pages_skipped`
    /// and `dedup` lookups; kept here so we don't keep recomputing.
    path_str: String,
    title: String,
    tier: Tier,
    pinned: bool,
    /// `Some(md)` when the on-disk source survived parsing; `None` when
    /// reading or parsing failed and the page must be skipped (the
    /// safety guard at the end of the merge will refuse to purge).
    md: Option<Markdown>,
    /// `true` when the destination already holds this path with a
    /// DIFFERENT page (body / frontmatter / title / tier / pinned).
    /// `false` when there's no destination row, the destination
    /// matches verbatim (no-op supersession), or the source itself
    /// couldn't be parsed and we'd skip it anyway.
    dest_conflict: bool,
}

/// Load every source page (metadata + body) and pre-classify whether
/// its path collides with a different page at the destination. Both
/// the `Block` pre-check and the copy loop drive off the returned
/// vector, so each page is read at most once.
pub(super) async fn prepare_source_pages(
    state: &AdminState,
    req: &MoveProjectRequest,
    src_ws: WorkspaceId,
    src_proj: ProjectId,
    dst_ws: WorkspaceId,
    dst_proj: ProjectId,
    summaries: &[sessionmunch_store::PageSummary],
) -> Vec<PreparedSourcePage> {
    let mut out = Vec::with_capacity(summaries.len());
    for s in summaries {
        let Ok(path) = PagePath::new(s.path.clone()) else {
            // Unparseable path — record a "skip" entry so the copy
            // loop can report it without re-trying the lookup.
            out.push(PreparedSourcePage {
                path: PagePath::new("invalid.md".to_string()).expect("valid placeholder"),
                path_str: s.path.clone(),
                title: s.title.clone(),
                tier: Tier::Semantic,
                pinned: false,
                md: None,
                dest_conflict: false,
            });
            continue;
        };
        let tier: Tier = s.tier.parse().unwrap_or(Tier::Semantic);
        let pinned = matches!(
            state.reader.page_meta(&req.from_workspace, &req.project, &s.path).await,
            Ok(Some(ref m)) if m.pinned
        );
        let md = state.wiki.read_page(src_ws, src_proj, &path).ok();
        // Compute the conflict decision once. The check is "is there a
        // DIFFERENT page at this path?", which requires both the dest
        // body and the source markdown — when either is missing, treat
        // as no-conflict and let the copy loop's natural-path branch
        // handle it (it'll either supersede a no-op or skip with a
        // missing-md guard).
        let dest_conflict = matches!(
            (
                state
                    .reader
                    .page_body_by_ids(dst_ws, dst_proj, s.path.as_str())
                    .await,
                &md,
            ),
            (Ok(Some(existing)), Some(md_ref))
                if page_copy_differs(&existing, md_ref, &s.title, tier, pinned)
        );
        out.push(PreparedSourcePage {
            path,
            path_str: s.path.clone(),
            title: s.title.clone(),
            tier,
            pinned,
            md,
            dest_conflict,
        });
    }
    out
}

/// Execute the copy-purge merge once the destination has been
/// resolved. Pulled out of `handle_move_project` so the orchestrator
/// reads as "validate → branch → copy_purge_merge", and the copy
/// loop's per-page IO runs through a pre-computed `PreparedSourcePage`
/// instead of fetching the same metadata twice.
// Merge-path orchestration: state + req + the four namespaced ids
// (src/dst ws+proj) + pre-checkpoint + the authenticated actor. Bundling
// the ids into a struct would obscure more than the arity warning, matching
// the existing `#[allow]`s on the equivalent store-layer helpers.
#[allow(clippy::too_many_arguments)]
pub(super) async fn copy_purge_merge(
    state: &AdminState,
    req: &MoveProjectRequest,
    src_ws: WorkspaceId,
    src_proj: ProjectId,
    dst_ws: WorkspaceId,
    dst_proj: ProjectId,
    pre_checkpoint: Option<String>,
    actor: sessionmunch_core::ActorContext,
) -> Result<MoveProjectReport, MoveErr> {
    // Enumerate the source's latest pages (authoritative on is_latest).
    let summaries = match state
        .reader
        .list_pages(&req.from_workspace, &req.project)
        .await
    {
        Ok(s) => s,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // Single pass over the source: load each page's metadata + body
    // AND classify the destination conflict, so the block pre-check
    // and the copy loop don't re-query the same rows.
    let prepared =
        prepare_source_pages(state, req, src_ws, src_proj, dst_ws, dst_proj, &summaries).await;

    // Under the default `block` policy, abort the WHOLE move now —
    // before anything is copied — so the source stays intact and the
    // operator resolves the conflicts or re-runs with an explicit
    // overwrite/duplicate. Drives off the cached `dest_conflict_body`
    // computed by `prepare_source_pages`.
    if req.on_conflict == OnConflict::Block {
        let blocking: Vec<String> = prepared
            .iter()
            .filter(|p| p.dest_conflict)
            .map(|p| p.path_str.clone())
            .collect();
        if !blocking.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "destination already has pages at these paths with different content; \
                              resolve them or re-run with on_conflict=overwrite or on_conflict=duplicate",
                    "conflicts": blocking,
                })),
            ));
        }
    }

    // Carry the source page embeddings over verbatim instead of recomputing
    // them — embedding is the dominant per-page cost of a bulk move. Writes go
    // through an embedder-less Wiki so `write_page` never re-embeds; we then
    // store each source vector against the new page id. Only embeddings
    // computed with the CURRENTLY configured embedder ({provider,model,dim})
    // are loaded (load_embeddings filters on them), so the "one model per
    // index" invariant holds; any page lacking a current-model embedding is
    // simply copied without one (backfill later via `sessionmunch embed`).
    let copy_wiki = state.wiki.clone().without_embedder();
    let mut src_embeddings: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    let embed_meta: Option<(String, String, u32)> = if let Some(embedder) = &state.embedder {
        let (provider, model, dim) = (
            embedder.provider().to_string(),
            embedder.model().to_string(),
            embedder.dim(),
        );
        match state
            .reader
            .load_embeddings(src_ws, src_proj, provider.clone(), model.clone(), dim)
            .await
        {
            Ok(rows) => {
                for e in rows {
                    src_embeddings.insert(e.path.to_string(), f32_vec_to_bytes(&e.vector));
                }
                Some((provider, model, dim))
            }
            Err(e) => {
                warn!(error = %e, "move-project: failed to load source embeddings; copies land without vectors");
                None
            }
        }
    } else {
        None
    };

    // COPY each page through the (embedder-less) write path so sanitization,
    // link re-resolution, FTS upsert (and admission/git-mirror on deploy) all
    // fire — minus the per-page embed, which we carry over below. Drives off
    // the prebuilt `PreparedSourcePage` vec so each page is read at most
    // once across the whole merge (the previous version re-fetched in the
    // pre-scan + the copy loop for Block-policy callers).
    let mut pages_copied = 0u64;
    let mut pages_skipped: Vec<String> = Vec::new();
    let mut conflicts: Vec<PathConflict> = Vec::new();
    let mut used_dest_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in prepared {
        let Some(md) = p.md else {
            // Either the path was unparseable or `read_page` failed —
            // either way we cannot copy it. The presence of a skip
            // here aborts the purge below.
            warn!(
                path = %p.path_str,
                "move-project: source page unreadable or invalid path; skipping"
            );
            pages_skipped.push(p.path_str.clone());
            continue;
        };
        // Apply the on_conflict policy using the cached classification.
        // `block` was already handled by the pre-scan above so we never
        // get here with `dest_conflict == true` AND `policy == Block`.
        let dest_path = if p.dest_conflict {
            match req.on_conflict {
                OnConflict::Duplicate => {
                    let deduped = dedup_dest_path(
                        state,
                        dst_ws,
                        dst_proj,
                        &p.path_str,
                        &req.from_workspace,
                        &used_dest_paths,
                    )
                    .await;
                    conflicts.push(PathConflict {
                        path: p.path_str.clone(),
                        moved_to: deduped.as_str().to_string(),
                    });
                    deduped
                }
                OnConflict::Overwrite => {
                    conflicts.push(PathConflict {
                        path: p.path_str.clone(),
                        moved_to: p.path_str.clone(),
                    });
                    p.path.clone()
                }
                OnConflict::Block => p.path.clone(), // unreachable
            }
        } else if used_dest_paths.contains(p.path_str.as_str()) {
            // The natural path is already claimed by an earlier
            // de-duplicated page; pick another to avoid clobbering it.
            let deduped = dedup_dest_path(
                state,
                dst_ws,
                dst_proj,
                &p.path_str,
                &req.from_workspace,
                &used_dest_paths,
            )
            .await;
            conflicts.push(PathConflict {
                path: p.path_str.clone(),
                moved_to: deduped.as_str().to_string(),
            });
            deduped
        } else {
            p.path.clone()
        };
        used_dest_paths.insert(dest_path.as_str().to_string());
        let new_page_id = match copy_wiki
            .write_page(WritePageRequest {
                workspace_id: dst_ws,
                project_id: dst_proj,
                path: dest_path.clone(),
                frontmatter: md.frontmatter,
                body: md.body,
                tier: p.tier,
                pinned: p.pinned,
                // Preserve the stored title verbatim (PageSummary.title is
                // the DB-derived title), rather than re-deriving it.
                title: Some(p.title.clone()),
                // Skip the named contributors enrich webhook on move copies:
                // the frontmatter (including the contributors list) is copied
                // verbatim, so re-enriching each copy adds nothing but blocking
                // per-page latency to a bulk move. write_page still resolves
                // the destination NAMES and runs the other hooks (git-mirror),
                // so the copy lands under the destination path.
                admission_ctx: Some(AdmissionContext {
                    skip_webhooks: vec![CONTRIBUTORS_WEBHOOK_NAME.to_string()],
                    ..AdmissionContext::default()
                }),
                author_id: None,
                actor: actor.clone(),
                evidence: Vec::new(),
            })
            .await
        {
            Ok(pid) => pid,
            // ANY copy failure aborts BEFORE the purge — the source survives.
            Err(e) => return Err(internal_err(format!("copy of {} failed: {e}", p.path_str))),
        };
        // Carry the source embedding over (skip the re-embed) when the source
        // had one for the current model.
        if let (Some((provider, model, dim)), Some(bytes)) =
            (&embed_meta, src_embeddings.get(&p.path_str))
            && let Err(e) = state
                .writer
                .store_embedding(
                    new_page_id,
                    bytes.clone(),
                    provider.clone(),
                    model.clone(),
                    *dim,
                )
                .await
        {
            warn!(path = %p.path_str, error = %e, "move-project: failed to carry embedding; page copied without it");
        }
        pages_copied += 1;
    }

    // Safety: a skipped (unreadable) source page blocks the purge — purging
    // now would destroy data we failed to copy. Report and let the operator
    // fix + re-run (re-running is idempotent: copied pages just supersede).
    if !pages_skipped.is_empty() {
        let checkpoint = checkpoint_or_warn(
            &state.wiki,
            format!(
                "move-project-partial {}/{} -> {}/{}",
                req.from_workspace, req.project, req.to_workspace, req.project
            ),
        );
        let report = MoveProjectReport {
            from: format!("{}/{}", req.from_workspace, req.project),
            to: format!("{}/{}", req.to_workspace, req.project),
            // copy_purge_merge is only reached from the merge branch
            // of handle_move_project; the destination project pre-existed.
            merged_into_existing: true,
            moved_via: "copy-purge",
            pages_copied,
            workstreams_moved: 0,
            pages_skipped,
            source_purged: false,
            source_pages_deleted: 0,
            source_sessions_deleted: 0,
            source_observations_deleted: 0,
            source_handoffs_deleted: 0,
            source_embeddings_deleted: 0,
            files_deleted: Vec::new(),
            files_failed: Vec::new(),
            conflicts,
            pre_checkpoint,
            checkpoint,
        };
        return Ok(report);
    }

    // PURGE the source — only reached when every page copied successfully.
    // Admission must run BEFORE the DB destruction so a `failure_policy =
    // reject` webhook can still abort the second leg of the move while the
    // source is intact (mirrors `handle_purge_project`'s ordering). The
    // previous version called `Wiki::purge_project` AFTER `writer.purge_project`,
    // which ran admit AFTER the rows were gone — reject came too late.
    let label = format!("{}/{}", req.from_workspace, req.project);
    let purge_ctx = AdmissionContext {
        workspace: req.from_workspace.clone(),
        project: req.project.clone(),
        op: AdmissionOp::PurgeProject,
        actor,
        ..Default::default()
    };
    let resolved_purge_ctx = match state
        .wiki
        .admit_purge_project(src_ws, src_proj, Some(purge_ctx))
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // The source purge is an internal step of move-project (a distinct op that
    // records its own move report); it is not attributed as a standalone
    // `purge_project` here, so the audit author is left NULL.
    // `Skip`: compaction is an operator's explicit choice on a destructive
    // command, not a side effect of moving a project. A `VACUUM` here would
    // rewrite the whole database in the middle of a move the caller asked to
    // be cheap.
    let summary = match state
        .writer
        .purge_project(
            src_ws,
            src_proj,
            &label,
            None,
            false,
            sessionmunch_store::Compaction::Skip,
        )
        .await
    {
        Ok(s) => s,
        // A live managed run under the source always blocks the destructive
        // second leg. `force` only overrides the active-project guard; unlike
        // a true move, copy-purge would delete the lease and strand the raw
        // transcript. The pages are already copied, so a later retry is
        // idempotent and leaves the source intact meanwhile.
        Err(e @ StoreError::ManagedRunActive { .. }) => {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!(
                        "{pages_copied} page(s) were copied to {}/{}, but the source \
                         {label} could not be purged: {e}. The source is intact. \
                         move-project --force cannot delete a live managed \
                         workstream during a copy-purge merge; finish or cancel \
                         the managed run, then retry.",
                        req.to_workspace, req.project,
                    )
                })),
            ));
        }
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // Remove the source's wiki and raw-workstream directories, then dispatch
    // the non-blocking purge webhook. Pass the workspace/project names cached
    // in `resolved_purge_ctx`; the DB rows no longer exist for lookup.
    let (files_deleted, files_failed) = remove_purged_project_storage(
        state,
        src_ws,
        src_proj,
        &summary.workstream_ids,
        "move-project",
    )
    .await;
    // See `handle_purge_project` for the rationale on `partial_failure`.
    let mut dispatch_ctx = resolved_purge_ctx;
    if !files_failed.is_empty()
        && let Some(ref mut c) = dispatch_ctx
    {
        c.partial_failure = true;
    }
    state.wiki.dispatch_purge(dispatch_ctx.as_ref());

    // The source project_id was just purged; if it was the published active
    // project, the pointer now dangles — clear it so the next hook re-resolves
    // to the (new) project rather than the deleted id.
    state.active_project.clear_project(src_proj);
    // Proactively drop any stale per-cwd cache entry for the purged source
    // project (its project_id no longer exists).
    invalidate_scope_cache(state, ScopeInvalidation::Project(src_proj)).await;

    if let Err(e) = state.wiki.backfill_scope_manifests().await {
        warn!(error = %e, "copy-purge move: scope-manifest backfill failed after move");
    }
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "move-project {}/{} -> {}/{}",
            req.from_workspace, req.project, req.to_workspace, req.project
        ),
    );

    let report = MoveProjectReport {
        from: label,
        to: format!("{}/{}", req.to_workspace, req.project),
        // copy_purge_merge is only reached from the merge branch.
        merged_into_existing: true,
        moved_via: "copy-purge",
        pages_copied,
        workstreams_moved: 0,
        pages_skipped: Vec::new(),
        source_purged: true,
        source_pages_deleted: summary.pages_deleted,
        source_sessions_deleted: summary.sessions_deleted,
        source_observations_deleted: summary.observations_deleted,
        source_handoffs_deleted: summary.handoffs_deleted,
        source_embeddings_deleted: summary.embeddings_deleted,
        files_deleted,
        files_failed,
        conflicts,
        pre_checkpoint,
        checkpoint,
    };
    Ok(report)
}
