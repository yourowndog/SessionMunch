//! Admin routes: read-page.

use super::*;

/// Query string for `GET /admin/read-page`.
///
/// Two modes:
/// - Path mode: `workspace` + `project` + `path` — direct lookup.
/// - Query mode: `workspace` + `project` + `q` — FTS5 search scoped to
///   the given project; fetches the top-ranking hit's full body.
#[derive(Debug, Deserialize)]
pub(super) struct ReadPageQuery {
    /// Workspace name (required).
    workspace: String,
    /// Project name (required).
    project: String,
    /// Direct wiki path (e.g. `notes/foo.md`). Takes precedence over `q`.
    #[serde(default)]
    path: Option<String>,
    /// FTS5 query. Used when `path` is absent; fetches the top hit's full body.
    #[serde(default)]
    q: Option<String>,
}

/// Response body for `GET /admin/read-page`.
#[derive(Debug, Serialize)]
pub(super) struct ReadPageResponse {
    path: String,
    workspace: String,
    project: String,
    title: Option<String>,
    body: String,
    frontmatter: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    served_from: Option<&'static str>,
}

pub(super) async fn handle_read_page(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<ReadPageQuery>,
) -> impl IntoResponse {
    let direct_path = if let Some(raw) = query.path {
        match sessionmunch_core::PagePath::new(raw) {
            Ok(p) => Some(p),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
                );
            }
        }
    } else {
        None
    };
    if direct_path.is_none() && query.q.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "provide `path` or `q`" })),
        );
    }

    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };

    // Resolve the page path: direct `path` takes precedence over `q`.
    let page_path = if let Some(path) = direct_path {
        path
    } else if let Some(q) = query.q {
        let hits = match state
            .reader
            .search_pages_for_project(ws, proj, q.clone(), 1, None)
            .await
        {
            Ok(h) => h,
            Err(e) => return internal_err(e.to_string()),
        };
        match hits.into_iter().next() {
            Some(h) => h.path,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": format!("no pages found for query {q:?}") })),
                );
            }
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "provide `path` or `q`" })),
        );
    };

    match state.wiki.read_page(ws, proj, &page_path) {
        Ok(md) => {
            let title = md
                .frontmatter
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let resp = ReadPageResponse {
                path: page_path.to_string(),
                workspace: query.workspace,
                project: query.project,
                title,
                body: md.body,
                frontmatter: md.frontmatter,
                served_from: None,
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(&resp).unwrap_or_else(|_| serde_json::json!({}))),
            )
        }
        // Only a missing markdown file can fall back to the DB copy. Other disk
        // errors belong to the source-of-truth file and must be surfaced.
        Err(disk_err) if is_missing_wiki_file(&disk_err) => match state
            .reader
            .page_body_by_ids(ws, proj, page_path.as_str())
            .await
        {
            Ok(Some(stored)) => {
                let frontmatter: serde_json::Value = serde_json::from_str(&stored.frontmatter_json)
                    .unwrap_or(serde_json::Value::Null);
                let title = frontmatter
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or(Some(stored.title));
                let resp = ReadPageResponse {
                    path: page_path.to_string(),
                    workspace: query.workspace,
                    project: query.project,
                    title,
                    body: stored.body,
                    frontmatter,
                    served_from: Some("db-fallback"),
                };
                (
                    StatusCode::OK,
                    Json(serde_json::to_value(&resp).unwrap_or_else(|_| serde_json::json!({}))),
                )
            }
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": disk_err.to_string() })),
            ),
            Err(e) => internal_err(e.to_string()),
        },
        Err(disk_err) => internal_err(disk_err.to_string()),
    }
}
