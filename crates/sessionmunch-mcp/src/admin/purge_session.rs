//! Admin routes: purge-session.

use super::*;

/// JSON request body for `POST /admin/purge-session`.
#[derive(Deserialize)]
pub(super) struct PurgeSessionRequest {
    /// Workspace name. Must already exist; 404 otherwise.
    workspace: String,
    /// Project name. Must already exist; 404 otherwise.
    project: String,
    /// Full `sessions.id` UUID. A session that does not belong to the named
    /// workspace/project is a 404 — the id alone is never authority over
    /// another scope.
    session_id: String,
    /// Mandatory confirmation flag. Without `confirm: true` the server
    /// returns 400 — purging is destructive and irreversible.
    confirm: bool,
    /// Reclaim freed bytes: rebuild the FTS indexes and `VACUUM` after the
    /// delete commits. Off by default; see `sessionmunch_store::Compaction` for
    /// the cost and for what it does not guarantee.
    #[serde(default)]
    compact: bool,
}

/// Wire-format summary returned by `POST /admin/purge-session`.
#[derive(Debug, Serialize)]
pub struct PurgeSessionReport {
    /// Session id that was purged.
    pub session_id: String,
    /// Human workspace name.
    pub workspace: String,
    /// Human project name.
    pub project: String,
    /// Number of observations deleted.
    pub observations_deleted: u64,
    /// Number of authored handoffs deleted.
    pub handoffs_deleted: u64,
    /// Number of page rows deleted, counting superseded versions.
    pub pages_deleted: u64,
    /// Number of auto-improvement runs deleted.
    pub auto_improve_runs_deleted: u64,
    /// Distinct wiki page paths whose database rows were deleted.
    pub removed_paths: Vec<PagePath>,
    /// Distinct wiki page paths removed from disk after the database purge.
    pub files_deleted: Vec<PagePath>,
    /// Distinct wiki page paths that could not be removed from disk.
    pub files_failed: Vec<PagePath>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran).
    pub compacted: bool,
    /// Pre-purge checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-purge checkpoint, if the purge changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// `POST /admin/purge-session` — delete one session and everything derived
/// from it, inside a single workspace/project scope.
pub(super) async fn handle_purge_session(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    Json(req): Json<PurgeSessionRequest>,
) -> impl IntoResponse {
    let author_id = author_ext.map(|axum::Extension(u)| u);
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "destructive operation requires confirm=true"
            })),
        );
    }

    let session_id = match req.session_id.trim().parse::<SessionId>() {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "session_id must be a full UUID" })),
            );
        }
    };

    let (ws_id, proj_id) =
        match lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await {
            Ok(ids) => ids,
            Err(e) => return e,
        };

    // Admission runs before anything is deleted so a reject-policy webhook can
    // refuse while every row is still intact — same order as purge-project.
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let ctx = AdmissionContext {
        workspace: req.workspace.clone(),
        project: req.project.clone(),
        op: AdmissionOp::PurgeSession,
        actor,
        ..Default::default()
    };
    let mut dispatch_ctx = match state
        .wiki
        .admit_purge_session(ws_id, proj_id, Some(ctx))
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => return internal_err(e.to_string()),
    };

    let label = format!("{}/{}: {}", req.workspace, req.project, session_id);
    let pre_checkpoint = match checkpoint_or_500(&state.wiki, format!("pre-purge-session {label}"))
    {
        Ok(oid) => oid,
        Err(e) => return e,
    };

    let compaction = if req.compact {
        sessionmunch_store::Compaction::Reclaim
    } else {
        sessionmunch_store::Compaction::Skip
    };

    let outcome = match state
        .wiki
        .purge_session(ws_id, proj_id, session_id, author_id, compaction)
        .await
    {
        Ok(s) => s,
        // Absent from this scope (or already purged) is a 404, not a fault.
        Err(WikiError::Store(e @ StoreError::NotFound(_))) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
        Err(e) => return internal_err(e.to_string()),
    };

    let sessionmunch_wiki::PurgeSessionOutcome {
        summary,
        files_deleted,
        files_failed,
    } = outcome;
    if !files_failed.is_empty()
        && let Some(ref mut ctx) = dispatch_ctx
    {
        ctx.partial_failure = true;
    }
    state.wiki.dispatch_purge(dispatch_ctx.as_ref());

    let checkpoint = checkpoint_or_warn(&state.wiki, format!("purge-session {label}"));

    let report = PurgeSessionReport {
        session_id: session_id.to_string(),
        workspace: req.workspace,
        project: req.project,
        observations_deleted: summary.observations_deleted,
        handoffs_deleted: summary.handoffs_deleted,
        pages_deleted: summary.pages_deleted,
        auto_improve_runs_deleted: summary.auto_improve_runs_deleted,
        removed_paths: summary.removed_paths,
        files_deleted,
        files_failed,
        compacted: summary.compacted,
        pre_checkpoint,
        checkpoint,
    };

    (StatusCode::OK, Json(json_or_empty(&report)))
}
