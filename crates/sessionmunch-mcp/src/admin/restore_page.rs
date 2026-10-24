//! Admin routes: restore-page.

use super::*;

#[derive(Deserialize)]
pub(super) struct RestorePageRequest {
    /// Workspace name. Must exist.
    workspace: String,
    /// Project name. Must exist within `workspace`.
    project: String,
    /// Relative wiki path to restore.
    path: String,
    /// Git revision/commit to restore from.
    rev: String,
}

#[derive(Serialize)]
pub(super) struct RestorePageResponse {
    /// UUID of the restored page version.
    page_id: String,
    /// Canonical wiki path restored.
    path: String,
    /// Revision requested by the caller.
    restored_from: String,
    /// Pre-restore checkpoint, if the current tree had changes to save.
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_checkpoint: Option<String>,
    /// Post-restore checkpoint, if the restore changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
}

pub(super) async fn handle_restore_page(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<RestorePageRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let path = PagePath::new(req.path.clone()).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
        )
    })?;
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    let pre_checkpoint = checkpoint_or_500(
        &state.wiki,
        format!(
            "pre-restore-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    )?;

    let page_id = state
        .wiki
        .restore_page_from_checkpoint(ws, proj, path.clone(), &req.rev)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "restore-page {}/{}: {} from {}",
            req.workspace,
            req.project,
            path.as_str(),
            req.rev
        ),
    );

    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(RestorePageResponse {
                page_id: page_id.to_string(),
                path: path.to_string(),
                restored_from: req.rev,
                pre_checkpoint,
                checkpoint,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}
