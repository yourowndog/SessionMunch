//! Admin routes: forget-sweep.

use super::*;

/// JSON request body for `POST /admin/forget-sweep`.
#[derive(Deserialize)]
pub(super) struct ForgetSweepRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Report what would be evicted without actually mutating.
    #[serde(default)]
    dry_run: bool,
}

pub(super) async fn handle_forget_sweep(
    State(state): State<Arc<AdminState>>,
    axum::Extension(tuning): axum::Extension<SweepTuning>,
    Json(req): Json<ForgetSweepRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    run_sweep_with_options(
        &state.reader,
        &state.writer,
        Some(&state.wiki),
        ws,
        proj,
        &state.decay_params,
        tuning.breadth_weight,
        tuning.retention,
        req.dry_run,
    )
    .await
    .map(|report| {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        )
    })
    .map_err(|e| internal_err(e.to_string()))
}
