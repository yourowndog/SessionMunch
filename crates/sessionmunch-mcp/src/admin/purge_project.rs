//! Admin routes: purge-project.

use super::*;

/// JSON request body for `POST /admin/purge-project`.
#[derive(Deserialize)]
pub(super) struct PurgeProjectRequest {
    /// Workspace name. The workspace must already exist; we 404 if absent.
    workspace: String,
    /// Project name. The project must already exist; we 404 if absent.
    project: String,
    /// Mandatory confirmation flag. Without `confirm: true` the server
    /// returns 400 — purging is destructive and irreversible.
    confirm: bool,
    /// Purge even when a managed workstream under this project still holds a
    /// live run lease. Off by default: the cascade would delete that lease row
    /// out from under a running agent, which then cannot save its history.
    #[serde(default)]
    force: bool,
    /// Reclaim freed bytes: rebuild the FTS indexes and `VACUUM` after the
    /// delete commits. Off by default; see `sessionmunch_store::Compaction` for
    /// the cost and for what it does not guarantee.
    #[serde(default)]
    compact: bool,
}

/// Wire-format summary returned by `POST /admin/purge-project`.
#[derive(Debug, Serialize)]
pub struct PurgeProjectReport {
    /// Human-readable `workspace/project` label.
    pub label: String,
    /// Number of `pages` rows deleted (all versions).
    pub pages_deleted: u64,
    /// Number of `sessions` rows deleted.
    pub sessions_deleted: u64,
    /// Number of `observations` rows deleted.
    pub observations_deleted: u64,
    /// Number of `handoffs` rows deleted.
    pub handoffs_deleted: u64,
    /// Number of `page_embeddings` rows deleted.
    pub embeddings_deleted: u64,
    /// Number of managed `workstreams` rows deleted via cascade.
    pub workstreams_deleted: u64,
    /// Number of `managed_runs` rows deleted via cascade.
    pub managed_runs_deleted: u64,
    /// Ids of the deleted workstreams whose `raw/workstreams/<id>/` segment
    /// directories were included in the post-commit cleanup.
    pub workstream_ids: Vec<String>,
    /// Paths removed from disk (the project's UUID-namespaced directory).
    pub files_deleted: Vec<String>,
    /// Paths that could not be removed from disk (non-fatal; DB rows are gone).
    pub files_failed: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete: the rows are gone from the API and from search,
    /// but their bytes stay in free pages until the file is next rewritten.
    pub compacted: bool,
    /// Pre-purge checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-purge checkpoint, if the purge changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

pub(super) async fn remove_workstream_segment_storage(
    state: &AdminState,
    workstream_ids: &[String],
    operation: &'static str,
) -> (Vec<String>, Vec<String>) {
    let mut deleted = Vec::with_capacity(workstream_ids.len());
    let mut failed = Vec::new();

    for workstream_id in workstream_ids {
        let path = state
            .data_dir
            .join("raw")
            .join("workstreams")
            .join(workstream_id);
        let path_display = path.display().to_string();
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => deleted.push(path_display),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    operation,
                    path = %path_display,
                    error = %error,
                    "scope purge failed to remove workstream segment directory"
                );
                failed.push(path_display);
            }
        }
    }

    (deleted, failed)
}

pub(super) async fn remove_purged_project_storage(
    state: &AdminState,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    workstream_ids: &[String],
    operation: &'static str,
) -> (Vec<String>, Vec<String>) {
    let project_root = state.wiki.project_root(workspace_id, project_id);
    let project_root_display = project_root.display().to_string();
    let mut deleted = Vec::with_capacity(workstream_ids.len() + 1);
    let mut failed = Vec::new();

    match state
        .wiki
        .remove_project_dir(workspace_id, project_id)
        .await
    {
        Ok(()) => deleted.push(project_root_display.clone()),
        Err(error) => {
            warn!(
                operation,
                path = %project_root_display,
                error = %error,
                "project purge failed to remove wiki directory"
            );
            failed.push(project_root_display);
        }
    }

    let (raw_deleted, raw_failed) =
        remove_workstream_segment_storage(state, workstream_ids, operation).await;
    deleted.extend(raw_deleted);
    failed.extend(raw_failed);

    (deleted, failed)
}

pub(super) async fn handle_purge_project(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    Json(req): Json<PurgeProjectRequest>,
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

    // Look up workspace and project IDs without auto-creating.
    let (ws_id, proj_id) =
        match lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await {
            Ok(ids) => ids,
            Err(e) => return e,
        };

    let label = format!("{}/{}", req.workspace, req.project);

    // Admission must run before any destructive work. A reject-policy webhook
    // is allowed to abort the purge while DB rows and files are still intact.
    // Seed names from the request so mirrors do not depend on DB lookup after
    // the rows are purged below. Carry the authenticated actor so a scope-guard
    // admission webhook can authorize the purge by user — an empty actor is
    // rejected (`403 user '' not allowed to purge_project`).
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let purge_ctx = AdmissionContext {
        workspace: req.workspace.clone(),
        project: req.project.clone(),
        op: AdmissionOp::PurgeProject,
        actor,
        ..Default::default()
    };
    let resolved_purge_ctx = match state
        .wiki
        .admit_purge_project(ws_id, proj_id, Some(purge_ctx))
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => return internal_err(e.to_string()),
    };

    let pre_checkpoint = match checkpoint_or_500(&state.wiki, format!("pre-purge-project {label}"))
    {
        Ok(oid) => oid,
        Err(e) => return e,
    };

    let compaction = if req.compact {
        sessionmunch_store::Compaction::Reclaim
    } else {
        sessionmunch_store::Compaction::Skip
    };

    let summary = match state
        .writer
        .purge_project(ws_id, proj_id, &label, author_id, req.force, compaction)
        .await
    {
        Ok(s) => s,
        // A live managed run is a conflict, not a server fault: the operator
        // can finish the session or retry with `force`. Same status the
        // heartbeat itself returns when a lease is gone, so the two agree.
        Err(e @ StoreError::ManagedRunActive { .. }) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
        Err(e) => return internal_err(e.to_string()),
    };

    // DB cascade already deleted all rows. Remove both the wiki project root
    // and every raw managed-workstream segment directory best-effort, reporting
    // partial failures without disguising the committed database purge.
    let (files_deleted, files_failed) = remove_purged_project_storage(
        &state,
        ws_id,
        proj_id,
        &summary.workstream_ids,
        "purge-project",
    )
    .await;
    // Mirrors that track filesystem reality (a git-push mirror) want to
    // know the on-disk dir is still present even though the DB rows are
    // gone, so they can refuse to drop their own copy in violation of
    // their source of truth. Mirrors that track DB intent can ignore
    // `partial_failure`. Default-skipped on the wire so existing
    // webhook consumers see no extra key.
    let mut dispatch_ctx = resolved_purge_ctx;
    if !files_failed.is_empty()
        && let Some(ref mut c) = dispatch_ctx
    {
        c.partial_failure = true;
    }
    state.wiki.dispatch_purge(dispatch_ctx.as_ref());

    state.active_project.clear_project(proj_id);
    invalidate_scope_cache(&state, ScopeInvalidation::Project(proj_id)).await;

    let checkpoint = checkpoint_or_warn(&state.wiki, format!("purge-project {label}"));

    let report = PurgeProjectReport {
        label: summary.label,
        pages_deleted: summary.pages_deleted,
        sessions_deleted: summary.sessions_deleted,
        observations_deleted: summary.observations_deleted,
        handoffs_deleted: summary.handoffs_deleted,
        embeddings_deleted: summary.embeddings_deleted,
        workstreams_deleted: summary.workstreams_deleted,
        managed_runs_deleted: summary.managed_runs_deleted,
        workstream_ids: summary.workstream_ids,
        compacted: summary.compacted,
        files_deleted,
        files_failed,
        pre_checkpoint,
        checkpoint,
    };

    (
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    )
}
