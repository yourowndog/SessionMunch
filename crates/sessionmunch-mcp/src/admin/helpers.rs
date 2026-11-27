//! Admin routes: Internal helpers.

use super::*;

/// Build a 500 response carrying the given message.
pub(super) fn internal_err(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

pub(super) fn is_missing_wiki_file(err: &WikiError) -> bool {
    matches!(err, WikiError::Io(e) if e.kind() == std::io::ErrorKind::NotFound)
}

pub(super) fn trimmed_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Resolve workspace + project IDs, creating them if absent. Returns
/// either the IDs or a ready-to-return error response.
pub(super) async fn create_ws_proj(
    state: &AdminState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), (StatusCode, Json<serde_json::Value>)> {
    create_explicit_scope(&state.writer, workspace, project)
        .await
        .map(sessionmunch_store::ResolvedScope::as_tuple)
        .map_err(scope_err)
}

/// Look up workspace + project by name **without** auto-creating them.
/// Returns `(WorkspaceId, ProjectId)` on success, or a ready-to-return
/// 404/500 error response. Used by read, maintenance, and destructive
/// handlers where auto-creation would silently succeed on a typo.
pub(super) async fn lookup_ws_proj_no_create(
    state: &AdminState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), (StatusCode, Json<serde_json::Value>)> {
    lookup_existing_scope(&state.reader, workspace, project)
        .await
        .map(sessionmunch_store::ResolvedScope::as_tuple)
        .map_err(scope_err)
}

/// Identity-changing operations must use the current name, never an alias.
pub(super) async fn require_canonical_project(
    state: &AdminState,
    ids: (WorkspaceId, ProjectId),
    project: &str,
) -> Result<(WorkspaceId, ProjectId), (StatusCode, Json<serde_json::Value>)> {
    let canonical = state
        .reader
        .project_name_by_id(ids.0, ids.1)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "project no longer exists"})),
            )
        })?;
    if canonical != project {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "use the canonical project name for identity-changing operations",
                "canonical_project": canonical,
            })),
        ));
    }
    Ok(ids)
}

pub(super) async fn lookup_ws_proj_canonical(
    state: &AdminState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), (StatusCode, Json<serde_json::Value>)> {
    let ids = lookup_ws_proj_no_create(state, workspace, project).await?;
    require_canonical_project(state, ids, project).await
}

/// Look up a workspace by name **without** auto-creating it.
pub(super) async fn lookup_ws_no_create(
    state: &AdminState,
    workspace: &str,
) -> Result<WorkspaceId, (StatusCode, Json<serde_json::Value>)> {
    lookup_existing_workspace(&state.reader, workspace)
        .await
        .map_err(scope_err)
}

pub(super) fn scope_err(err: ScopeResolutionError) -> (StatusCode, Json<serde_json::Value>) {
    let status = if err.is_bad_request() {
        StatusCode::BAD_REQUEST
    } else if err.is_not_found() {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        status,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
}

pub(super) fn skip_webhooks_for_admin_request(
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: &HeaderMap,
) -> Vec<String> {
    let level = level_ext
        .map(|axum::Extension(level)| level)
        .unwrap_or(sessionmunch_core::AuthLevel::Anonymous);
    if level
        .authorize(Capability::SkipAdmissionChain, true)
        .is_ok()
    {
        crate::actor::skip_webhooks_from_headers(headers)
    } else {
        Vec::new()
    }
}
