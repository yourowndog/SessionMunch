//! Admin routes: search.

use super::*;

/// Query string for `GET /admin/search?q=…&limit=…`.
#[derive(Debug, Deserialize)]
pub(super) struct SearchQuery {
    /// FTS5 query expression.
    q: String,
    /// Workspace name to search within. Must be provided together with `project`.
    #[serde(default)]
    workspace: Option<String>,
    /// Project name to search within. Must be provided together with `workspace`.
    #[serde(default)]
    project: Option<String>,
    /// Max number of hits to return. Capped at 100 server-side.
    #[serde(default = "default_search_limit")]
    limit: usize,
}

pub(super) fn default_search_limit() -> usize {
    10
}

pub(super) async fn handle_search(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 100);
    let search_result = match (
        trimmed_opt(query.workspace.as_deref()),
        trimmed_opt(query.project.as_deref()),
    ) {
        (Some(workspace), Some(project)) => {
            match lookup_ws_proj_no_create(&state, workspace, project).await {
                Ok((ws, proj)) => {
                    state
                        .reader
                        .search_pages_for_project(ws, proj, query.q, limit, None)
                        .await
                }
                Err(e) => return e,
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "workspace and project must be provided together"
                })),
            );
        }
        _ => state.reader.search_pages(query.q, limit).await,
    };
    match search_result {
        Ok(hits) => (
            StatusCode::OK,
            Json(serde_json::to_value(&hits).unwrap_or_else(|_| serde_json::json!([]))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Query string for `GET /admin/search-events`.
#[derive(Debug, Deserialize)]
pub(super) struct SearchEventsQuery {
    /// FTS5 query expression.
    q: String,
    /// Optional workspace; must be provided together with `project`.
    #[serde(default)]
    workspace: Option<String>,
    /// Optional project; must be provided together with `workspace`.
    #[serde(default)]
    project: Option<String>,
    /// Only sessions recorded on this machine.
    #[serde(default)]
    machine: Option<String>,
    /// Only this harness (`agent_kind` wire name).
    #[serde(default)]
    harness: Option<String>,
    /// Only events persisted at or after this time (microseconds).
    #[serde(default)]
    since_micros: Option<i64>,
    /// Order newest first instead of by relevance.
    #[serde(default)]
    newest: bool,
    /// Max hits. Capped at 200 server-side.
    #[serde(default = "default_search_limit")]
    limit: usize,
}

/// `GET /admin/search-events` — lexical search over the session event
/// ledger (hook capture, imported transcripts, ingested batches).
pub(super) async fn handle_search_events(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<SearchEventsQuery>,
) -> impl IntoResponse {
    let scope = match (
        trimmed_opt(query.workspace.as_deref()),
        trimmed_opt(query.project.as_deref()),
    ) {
        (Some(workspace), Some(project)) => {
            match lookup_ws_proj_no_create(&state, workspace, project).await {
                Ok(pair) => Some(pair),
                Err(e) => return e,
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "workspace and project must be provided together"
                })),
            );
        }
        _ => None,
    };
    let search = sessionmunch_core::SessionEventSearch {
        query: query.q,
        limit: query.limit.clamp(1, 200),
        scope,
        machine: trimmed_opt(query.machine.as_deref()).map(str::to_owned),
        harness: trimmed_opt(query.harness.as_deref()).map(str::to_owned),
        since_micros: query.since_micros,
        until_micros: None,
        newest_first: query.newest,
        // `/admin/*` is root-only once users exist, so this is cross-owner.
        owner: None,
    };
    match state.reader.search_session_events_filtered(search).await {
        Ok(hits) => (
            StatusCode::OK,
            Json(serde_json::to_value(&hits).unwrap_or_else(|_| serde_json::json!([]))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}
