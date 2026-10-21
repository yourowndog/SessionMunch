//! Admin routes: delete-page.

use super::*;

/// JSON request body for `POST /admin/delete-page`.
///
/// Unlike `memory_delete_page` (MCP), this endpoint REQUIRES explicit
/// `workspace` so cross-workspace ambiguity can never silently route a
/// delete to the wrong slot.
#[derive(Deserialize)]
pub(super) struct DeletePageAdminRequest {
    /// Workspace name. Required (no auto-create — delete acts on existing data).
    workspace: String,
    /// Project name within the workspace. Required.
    project: String,
    /// Relative wiki path (e.g. `concepts/foo.md`).
    path: String,
}

/// JSON response body for `POST /admin/delete-page`.
#[derive(Serialize)]
pub(super) struct DeletePageResponse {
    /// Canonical wiki path of the deletion target.
    path: String,
    /// Always `true` on a successful (resolved-scope) call. `Wiki::delete_page`
    /// itself is idempotent — a missing file is treated as already-deleted —
    /// so the boolean reports "the call succeeded", not "a row was removed".
    /// The structural defense is in the 404 returned when `(workspace, project)`
    /// fails to resolve (so a stale or wrong-scope call never returns a misleading
    /// `deleted: true`).
    deleted: bool,
    /// Pre-delete checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_checkpoint: Option<String>,
    /// Post-delete checkpoint, if the delete changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
}

pub(super) async fn handle_delete_page(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    headers: HeaderMap,
    Json(req): Json<DeletePageAdminRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let path = PagePath::new(req.path.clone()).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
        )
    })?;

    // Use the no-create lookup (same as purge/rename/move): a delete on a
    // typo'd workspace/project must return 404, NOT silently auto-create
    // empty containers and then return `deleted: true` for nothing.
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    let pre_checkpoint = checkpoint_or_500(
        &state.wiki,
        format!(
            "pre-delete-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    )?;

    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let admission_ctx = if actor.has_any() || !skip_webhooks.is_empty() {
        Some(AdmissionContext {
            actor,
            op: AdmissionOp::Delete,
            skip_webhooks,
            ..AdmissionContext::default()
        })
    } else {
        None
    };

    let author_id = author_ext.map(|axum::Extension(u)| u);
    state
        .wiki
        .delete_page(ws, proj, &path, admission_ctx, author_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "delete-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    );

    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(DeletePageResponse {
                path: path.to_string(),
                deleted: true,
                pre_checkpoint,
                checkpoint,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}
