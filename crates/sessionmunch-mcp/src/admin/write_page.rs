//! Admin routes: write-page.

use super::*;

/// JSON request body for `POST /admin/write-page`.
#[derive(Deserialize)]
pub(super) struct WritePageAdminRequest {
    /// Workspace name (auto-created if absent).
    workspace: String,
    /// Project name (auto-created if absent).
    project: String,
    /// Relative wiki path (e.g. `concepts/foo.md`).
    path: String,
    /// Markdown body. The server frames it with frontmatter; pass plain body content.
    body: String,
    /// Optional title; derived from first H1 or path stem when absent.
    #[serde(default)]
    title: Option<String>,
    /// Semantic kind (`fact`, `rule`, `decision`, `gotcha`). Stored in
    /// the page frontmatter; the reader falls back to a path-derived
    /// kind when absent.
    #[serde(default)]
    kind: Option<String>,
    /// Tier name (`working`, `episodic`, `semantic`, `procedural`).
    #[serde(default = "default_write_tier")]
    tier: String,
    /// Tags to attach to the page.
    #[serde(default)]
    tags: Vec<String>,
    /// Pin the page so the decay sweep skips it.
    #[serde(default)]
    pinned: bool,
}

pub(super) fn default_write_tier() -> String {
    "semantic".to_string()
}

/// JSON response body for `POST /admin/write-page`.
#[derive(Serialize)]
pub(super) struct WritePageResponse {
    /// UUID of the written page.
    page_id: String,
    /// Canonical wiki path.
    path: String,
    /// Post-write checkpoint, if the write changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
}

pub(super) async fn handle_write_page(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: HeaderMap,
    Json(req): Json<WritePageAdminRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let tier: Tier = req.tier.parse().map_err(|_| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("unknown tier '{}'", req.tier)
            })),
        )
    })?;

    let path = PagePath::new(req.path.clone()).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
        )
    })?;

    let (ws, proj) = create_ws_proj(&state, &req.workspace, &req.project).await?;

    let mut fm = serde_json::Map::new();
    if let Some(title) = &req.title {
        fm.insert("title".into(), serde_json::Value::String(title.clone()));
    }
    if let Some(kind) = req.kind.as_deref() {
        let kind = kind.trim();
        if !kind.is_empty() {
            fm.insert("kind".into(), serde_json::Value::String(kind.to_string()));
        }
    }
    if !req.tags.is_empty() {
        fm.insert(
            "tags".into(),
            serde_json::Value::Array(
                req.tags
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    if req.pinned {
        fm.insert("pinned".into(), serde_json::Value::Bool(true));
    }
    let frontmatter = if fm.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(fm)
    };

    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(author_id)| author_id);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let admission_ctx = if actor.has_any() || !skip_webhooks.is_empty() {
        Some(AdmissionContext {
            op: AdmissionOp::WritePage,
            skip_webhooks,
            ..AdmissionContext::default()
        })
    } else {
        None
    };

    let page_id = state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: path.clone(),
            frontmatter,
            body: req.body,
            tier,
            pinned: req.pinned,
            title: req.title,
            admission_ctx,
            author_id,
            actor,
            evidence: Vec::new(),
        })
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "write-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    );

    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(WritePageResponse {
                page_id: page_id.to_string(),
                path: path.to_string(),
                checkpoint,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}
