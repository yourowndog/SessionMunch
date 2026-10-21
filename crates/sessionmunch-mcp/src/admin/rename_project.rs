//! Admin routes: rename-project.

use super::*;

/// JSON request body for `POST /admin/rename-workspace`.
#[derive(Deserialize)]
pub(super) struct RenameWorkspaceRequest {
    /// Current workspace name (must exist).
    from: String,
    /// New workspace name. Must be non-empty, no slashes, and not already used.
    to: String,
}

/// Wire-format summary returned by `POST /admin/rename-workspace`.
#[derive(Debug, Serialize)]
pub struct RenameWorkspaceResult {
    /// Previous workspace name.
    pub from: String,
    /// New workspace name.
    pub to: String,
    /// Post-rename mirror checkpoint, if one was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Number of scope manifests refreshed after the SQLite rename.
    pub manifests_refreshed: usize,
    /// Non-fatal manifest refresh failure after the SQLite rename committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_warning: Option<String>,
}

/// `POST /admin/rename-workspace` — rename a workspace (column-only; the
/// on-disk dir is UUID-keyed, so nothing moves). 404 unknown, 422 taken/invalid.
pub(super) async fn handle_rename_workspace(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<RenameWorkspaceRequest>,
) -> impl IntoResponse {
    let ws_id = match lookup_ws_no_create(&state, &req.from).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    if let Err(e) = state.writer.rename_workspace(ws_id, req.to.clone()).await {
        let status = match &e {
            StoreError::WorkspaceNameTaken(_) | StoreError::InvalidWorkspaceName(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        return (status, Json(serde_json::json!({ "error": e.to_string() })));
    }
    let (manifests_refreshed, manifest_warning) = match state.wiki.backfill_scope_manifests().await
    {
        Ok(n) => (n, None),
        Err(e) => {
            warn!(error = %e, "rename-workspace: scope-manifest backfill failed after rename");
            (0, Some(e.to_string()))
        }
    };
    invalidate_scope_cache(&state, ScopeInvalidation::Workspace(ws_id)).await;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!("rename-workspace {} -> {}", req.from, req.to),
    );
    let result = RenameWorkspaceResult {
        from: req.from.clone(),
        to: req.to.clone(),
        checkpoint,
        manifests_refreshed,
        manifest_warning,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&result).unwrap_or(serde_json::Value::Null)),
    )
}

/// JSON request body for `POST /admin/delete-workspace`.
#[derive(Deserialize)]
pub(super) struct DeleteWorkspaceRequest {
    /// Workspace name to delete (must exist).
    workspace: String,
    /// Delete even when the workspace still holds projects. Without it, a
    /// non-empty workspace is refused so a typo can't wipe live data.
    #[serde(default)]
    force: bool,
    /// Reclaim freed bytes: rebuild the FTS indexes and `VACUUM` after the
    /// delete commits. Off by default; see `sessionmunch_store::Compaction` for
    /// the cost and for what it does not guarantee.
    #[serde(default)]
    compact: bool,
}

/// Wire-format summary returned by `POST /admin/delete-workspace`.
#[derive(Debug, Serialize)]
pub struct DeleteWorkspaceResult {
    /// Workspace name that was deleted.
    pub workspace: String,
    /// Projects removed via the `workspace_id` cascade.
    pub projects_deleted: u64,
    /// `pages` rows removed via cascade (all versions).
    pub pages_deleted: u64,
    /// Managed `workstreams` rows removed via cascade.
    pub workstreams_deleted: u64,
    /// `managed_runs` rows removed via cascade.
    pub managed_runs_deleted: u64,
    /// Ids of the deleted workstreams whose raw segment directories were
    /// included in the post-commit cleanup.
    pub workstream_ids: Vec<String>,
    /// Paths removed from disk (the workspace directory and raw segments).
    pub files_deleted: Vec<String>,
    /// Paths that could not be removed from disk (non-fatal; DB rows are gone).
    pub files_failed: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete: the rows are gone from the API and from search,
    /// but their bytes stay in free pages until the file is next rewritten.
    pub compacted: bool,
    /// Pre-delete checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-delete mirror checkpoint, if one was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// `POST /admin/delete-workspace` — remove a workspace and, via cascade, every
/// project/page under it. Guarded: refuses a non-empty workspace unless
/// `force` (a typo shouldn't wipe live data). Orphan-workspace cleanup.
/// JSON request body for `POST /admin/compact`.
#[derive(Deserialize)]
pub(super) struct CompactRequest {
    /// Mandatory confirmation. Not because compaction destroys anything — it
    /// deletes nothing — but because it takes an exclusive lock and rewrites
    /// the whole database, so every write blocks until it finishes. That is an
    /// availability decision, and it should be deliberate.
    confirm: bool,
}

/// Wire-format summary returned by `POST /admin/compact`.
#[derive(Debug, Serialize)]
pub struct CompactReport {
    /// Database size in bytes before the rebuild + `VACUUM`.
    pub bytes_before: u64,
    /// Database size in bytes afterwards.
    pub bytes_after: u64,
    /// Bytes returned to the filesystem (saturating at zero).
    pub bytes_reclaimed: u64,
}

/// `POST /admin/compact` — rebuild the FTS indexes and `VACUUM`, deleting
/// nothing. The on-demand form of what the destructive commands offer as
/// `--compact`.
pub(super) async fn handle_compact(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<CompactRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "compact rewrites the whole database under an exclusive \
                          lock; pass confirm: true to proceed"
            })),
        );
    }
    match state.writer.compact().await {
        Ok(summary) => {
            let report = CompactReport {
                bytes_before: summary.bytes_before,
                bytes_after: summary.bytes_after,
                bytes_reclaimed: summary.bytes_reclaimed(),
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

pub(super) async fn handle_delete_workspace(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    Json(req): Json<DeleteWorkspaceRequest>,
) -> impl IntoResponse {
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let compaction = if req.compact {
        sessionmunch_store::Compaction::Reclaim
    } else {
        sessionmunch_store::Compaction::Skip
    };
    match delete_workspace_core(&state, &req.workspace, req.force, actor, compaction).await {
        Ok(result) => (
            StatusCode::OK,
            Json(serde_json::to_value(&result).unwrap_or(serde_json::Value::Null)),
        ),
        Err(resp) => resp,
    }
}

/// Delete a workspace and, via cascade, every project/page under it — returning
/// the summary as data (or the HTTP error a handler surfaces verbatim). Guarded:
/// refuses a non-empty workspace unless `force`. Shared by
/// `handle_delete_workspace` and `handle_merge_workspace` (which drains the
/// source workspace first, then deletes the now-empty shell with `force=false`).
pub(super) async fn delete_workspace_core(
    state: &Arc<AdminState>,
    workspace: &str,
    force: bool,
    actor: sessionmunch_core::ActorContext,
    compaction: sessionmunch_store::Compaction,
) -> Result<DeleteWorkspaceResult, MoveErr> {
    let ws_id = lookup_ws_no_create(state, workspace).await?;

    if !force {
        match state
            .reader
            .list_projects_with_stats_for_workspace(workspace.to_string())
            .await
        {
            Ok(projects) if !projects.is_empty() => {
                return Err((
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": StoreError::WorkspaceNotEmpty(projects.len() as u64).to_string()
                    })),
                ));
            }
            Ok(_) => {}
            Err(e) => return Err(internal_err(e.to_string())),
        }
    }

    let purge_ctx = AdmissionContext {
        workspace: workspace.to_string(),
        project: String::new(),
        op: AdmissionOp::PurgeWorkspace,
        actor,
        ..Default::default()
    };
    let resolved_purge_ctx = match state
        .wiki
        .admit_purge_workspace(ws_id, Some(purge_ctx))
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    let pre_checkpoint =
        checkpoint_or_500(&state.wiki, format!("pre-delete-workspace {workspace}"))?;

    let summary = match state
        .writer
        .delete_workspace(ws_id, force, compaction)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let status = match &e {
                StoreError::WorkspaceNotEmpty(_) => StatusCode::CONFLICT,
                StoreError::NotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return Err((status, Json(serde_json::json!({ "error": e.to_string() }))));
        }
    };

    let ws_root_str = state
        .wiki
        .root()
        .join(ws_id.to_string())
        .display()
        .to_string();
    let mut files_deleted = Vec::with_capacity(summary.workstream_ids.len() + 1);
    let mut files_failed = Vec::new();
    match state.wiki.remove_workspace_dir(ws_id).await {
        Ok(()) => files_deleted.push(ws_root_str.clone()),
        Err(e) => {
            warn!(error = %e, workspace = %workspace, path = %ws_root_str, "delete-workspace: on-disk dir removal failed");
            files_failed.push(ws_root_str);
        }
    }
    let (raw_deleted, raw_failed) =
        remove_workstream_segment_storage(state, &summary.workstream_ids, "delete-workspace").await;
    files_deleted.extend(raw_deleted);
    files_failed.extend(raw_failed);

    let mut dispatch_ctx = resolved_purge_ctx;
    if !files_failed.is_empty()
        && let Some(ref mut c) = dispatch_ctx
    {
        c.partial_failure = true;
    }
    state.wiki.dispatch_purge(dispatch_ctx.as_ref());

    state.active_project.clear_workspace(ws_id);
    invalidate_scope_cache(state, ScopeInvalidation::Workspace(ws_id)).await;

    Ok(DeleteWorkspaceResult {
        workspace: workspace.to_string(),
        projects_deleted: summary.projects_deleted,
        pages_deleted: summary.pages_deleted,
        workstreams_deleted: summary.workstreams_deleted,
        managed_runs_deleted: summary.managed_runs_deleted,
        workstream_ids: summary.workstream_ids,
        compacted: summary.compacted,
        files_deleted,
        files_failed,
        pre_checkpoint,
        checkpoint: checkpoint_or_warn(&state.wiki, format!("delete-workspace {workspace}")),
    })
}

/// JSON request body for `POST /admin/rename-project`.
#[derive(Deserialize)]
pub(super) struct RenameProjectRequest {
    /// Workspace name (must exist; we don't auto-create on rename).
    workspace: String,
    /// Current project name.
    from: String,
    /// New project name. Must be non-empty, no slashes.
    to: String,
}

/// Wire-format summary returned by `POST /admin/rename-project`.
#[derive(Debug, Serialize)]
pub struct RenameProjectSummary {
    /// Workspace name.
    pub workspace: String,
    /// Previous project name.
    pub from: String,
    /// New project name.
    pub to: String,
    /// Number of `is_latest=1` pages now under the renamed project.
    /// No files move — this is purely an informational count.
    pub pages: u64,
    /// Post-rename checkpoint, if `_meta.md` changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

pub(super) async fn handle_rename_project(
    State(state): State<Arc<AdminState>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    Json(req): Json<RenameProjectRequest>,
) -> impl IntoResponse {
    let author_id = author_ext.map(|axum::Extension(u)| u);
    // Look up workspace + source project; 404 if either is absent.
    let (ws_id, proj_id) = match lookup_ws_proj_no_create(&state, &req.workspace, &req.from).await {
        Ok(ids) => ids,
        Err(e) => return e,
    };

    // Step 3: execute the rename. The writer validates the name and
    // returns ProjectNameTaken / InvalidProjectName on conflicts. A
    // `NotFound` here signals a race — typically a concurrent
    // `purge-project` deleted the row between the lookup above and
    // this UPDATE. Map it to 404 so the caller sees an honest failure
    // instead of the previous false-200.
    if let Err(e) = state
        .writer
        .rename_project(ws_id, proj_id, req.to.clone(), author_id)
        .await
    {
        let status = match &e {
            StoreError::ProjectNameTaken(_) | StoreError::InvalidProjectName(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        return (status, Json(serde_json::json!({ "error": e.to_string() })));
    }

    // Step 4: count is_latest pages that now belong to the renamed project.
    // COUNT(*) always produces a row, so no optional() needed. We pass the
    // 16-byte project id as a plain &[u8] slice to avoid importing rusqlite
    // (not a direct dependency of this crate).
    let pid_bytes = *proj_id.as_bytes();
    let pages = match state
        .reader
        .with_conn(move |conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pages WHERE project_id = ?1 AND is_latest = 1",
                [&pid_bytes[..]],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
        .await
    {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    let summary = RenameProjectSummary {
        workspace: req.workspace.clone(),
        from: req.from.clone(),
        to: req.to.clone(),
        pages,
        checkpoint: {
            if let Err(e) = state.wiki.backfill_scope_manifests().await {
                warn!(error = %e, "rename-project: scope-manifest backfill failed after rename");
            }
            checkpoint_or_warn(
                &state.wiki,
                format!(
                    "rename-project {}/{} -> {}",
                    req.workspace, req.from, req.to
                ),
            )
        },
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&summary).unwrap_or_else(|_| serde_json::json!({}))),
    )
}
