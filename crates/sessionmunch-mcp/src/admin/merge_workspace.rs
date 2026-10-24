//! Admin routes: merge-workspace.

use super::*;

/// JSON request body for `POST /admin/merge-workspace`.
#[derive(Deserialize)]
pub(super) struct MergeWorkspaceRequest {
    /// Source workspace to drain and delete. Must exist; 404 on a typo.
    from: String,
    /// Destination workspace. Auto-created if absent (each per-project move
    /// get-or-creates it).
    to: String,
    /// Mandatory confirmation. The merge moves every source project (each a
    /// destructive move that purges the source project) and deletes the emptied
    /// source workspace, so without `confirm: true` the server returns 400.
    /// Defaulted so an omitted flag hits that explicit guard rather than a
    /// generic deserialization 422.
    #[serde(default)]
    confirm: bool,
    /// Override the live-session guard on every per-project move.
    #[serde(default)]
    force: bool,
    /// Conflict policy for a source project that already exists in the
    /// destination with colliding page paths (copy-purge merge). Default
    /// `block`. Projects with no destination twin are lossless true-moves and
    /// ignore this.
    #[serde(default)]
    on_conflict: OnConflict,
}

/// Wire-format report returned by `POST /admin/merge-workspace`.
#[derive(Debug, Serialize)]
pub struct MergeWorkspaceReport {
    /// Source workspace label.
    pub from: String,
    /// Destination workspace label.
    pub to: String,
    /// One move report per source project, in the order they were moved.
    pub projects_merged: Vec<MoveProjectReport>,
    /// Whether the now-empty source workspace was deleted at the end.
    pub source_workspace_deleted: bool,
}

/// `POST /admin/merge-workspace` — fold every project of `from` into `to`, then
/// delete the emptied `from`. Sugar over move-project: each source project runs
/// the same validated path, so a project that already exists in `to` merges by
/// content (copy-purge under `on_conflict`) while a fresh one is a lossless
/// re-stamp. Destructive (each move purges its source project and the source
/// workspace is removed), so `confirm=true` is required. Stops at the first
/// failing project — the moves already committed stand, and the failing project
/// plus the source workspace are left intact for the operator to resolve and
/// re-run (moves are idempotent).
pub(super) async fn handle_merge_workspace(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    Json(req): Json<MergeWorkspaceRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "destructive operation requires confirm=true"
            })),
        );
    }
    if req.from == req.to {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "from and to are identical; nothing to merge"
            })),
        );
    }

    // Resolve the SOURCE without auto-creating — 404 on a typo, so a mistyped
    // source can't create then immediately delete a phantom workspace.
    if let Err(e) = lookup_ws_no_create(&state, &req.from).await {
        return e;
    }

    let projects = match state
        .reader
        .list_projects_with_stats_for_workspace(req.from.clone())
        .await
    {
        Ok(p) => p,
        Err(e) => return internal_err(e.to_string()),
    };

    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);

    let mut merged: Vec<MoveProjectReport> = Vec::with_capacity(projects.len());
    for p in &projects {
        let move_req = MoveProjectRequest {
            from_workspace: req.from.clone(),
            project: p.project_name.clone(),
            to_workspace: req.to.clone(),
            confirm: true,
            force: req.force,
            on_conflict: req.on_conflict,
        };
        match move_project_core(&state, &move_req, actor.clone()).await {
            Ok(report) => merged.push(report),
            Err((status, body)) => {
                let reason = body
                    .0
                    .get("error")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                return (
                    status,
                    Json(serde_json::json!({
                        "error": format!(
                            "merge stopped at project '{}' in workspace '{}'",
                            p.project_name, req.from
                        ),
                        "failed_project": p.project_name,
                        "reason": reason,
                        "projects_merged": merged,
                        "source_workspace_deleted": false,
                    })),
                );
            }
        }
    }

    // Every project moved → the source workspace is now empty. Delete the shell
    // with force=false: it must be empty, so a race that repopulated it aborts
    // safely without wiping fresh data.
    let source_workspace_deleted = match delete_workspace_core(
        &state,
        &req.from,
        false,
        actor,
        // The shell is empty by this point; there is nothing to reclaim.
        sessionmunch_store::Compaction::Skip,
    )
    .await
    {
        Ok(_) => true,
        Err((_status, body)) => {
            // The moves succeeded; only the final empty-shell delete failed
            // (e.g. a concurrent write recreated a project). No data was
            // lost, so report success-with-caveat instead of a hard error.
            warn!(
                workspace = %req.from,
                "merge-workspace: source drained but shell delete failed: {:?}",
                body
            );
            false
        }
    };

    let report = MergeWorkspaceReport {
        from: req.from,
        to: req.to,
        projects_merged: merged,
        source_workspace_deleted,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or(serde_json::Value::Null)),
    )
}
