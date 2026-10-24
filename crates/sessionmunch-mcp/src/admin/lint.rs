//! Admin routes: lint.

use super::*;

/// JSON request body for `POST /admin/lint`.
#[derive(Deserialize)]
pub(super) struct LintRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Don't write the lint report page.
    #[serde(default)]
    dry_run: bool,
    /// Skip the LLM contradiction pass (rule-based findings only).
    /// When absent, defaults to `false` (LLM pass runs if a provider
    /// is configured).
    #[serde(default)]
    no_llm: bool,
}

pub(super) fn default_workspace() -> String {
    DEFAULT_WORKSPACE_NAME.to_string()
}

pub(super) fn default_project() -> String {
    DEFAULT_PROJECT_NAME.to_string()
}

pub(super) async fn handle_lint(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<LintRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    run_lint(
        &state.reader,
        &state.wiki,
        state.llm.as_ref(),
        ws,
        proj,
        sessionmunch_consolidate::LintOptions {
            dry_run: req.dry_run,
            use_llm: !req.no_llm,
            decay_lambda: state.decay_params.lambda,
        },
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
