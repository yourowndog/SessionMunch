//! Admin routes: embed.

use super::*;

/// JSON request body for `POST /admin/embed`.
#[derive(Deserialize)]
pub(super) struct EmbedRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist). Ignored when
    /// [`Self::all_projects`] is true.
    #[serde(default = "default_project")]
    project: String,
    /// When true, regenerates embeddings even for pages that already
    /// have one matching the current (provider, model, dim).
    #[serde(default)]
    reembed: bool,
    /// When true, count pages that would be embedded/skipped without
    /// calling the embedder or writing anything.
    #[serde(default)]
    dry_run: bool,
    /// When true, embed every project in `workspace` instead of a
    /// single `project`. The CLI sets this for `--force` /
    /// `--reembed` without an explicit `--project` so model migrations
    /// reach stale rows in every namespace.
    #[serde(default)]
    all_projects: bool,
}

/// Summary response from `POST /admin/embed`.
#[derive(Debug, Serialize)]
pub struct EmbedReport {
    /// Pages that were actually embedded (zero in dry-run).
    pub embedded: usize,
    /// Pages skipped because a matching embedding already existed.
    pub skipped: usize,
    /// Pages that failed to embed (read error or provider error).
    pub failed: usize,
    /// Pages that would be embedded in a live run (only meaningful
    /// when `dry_run` was requested).
    pub would_embed: usize,
    /// Provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Embedding dimensionality.
    pub dim: u32,
}

pub(super) async fn embed_project_pages(
    state: &AdminState,
    embedder: &Arc<dyn Embedder>,
    ws: WorkspaceId,
    proj: ProjectId,
    reembed: bool,
    dry_run: bool,
) -> Result<EmbedBackfillCounts, (StatusCode, Json<serde_json::Value>)> {
    run_embedding_backfill(
        &state.reader,
        &state.writer,
        &state.wiki,
        embedder,
        ws,
        proj,
        EmbedBackfillOptions { reembed, dry_run },
    )
    .await
    .map_err(|e| internal_err(e.to_string()))
}

pub(super) async fn handle_embed(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<EmbedRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let embedder = match state.embedder.clone() {
        Some(e) => e,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "embedder not configured on server"
                })),
            ));
        }
    };

    let provider = embedder.provider().to_string();
    let model = embedder.model().to_string();
    let dim = embedder.dim();

    let mut totals = EmbedBackfillCounts::default();

    if req.all_projects {
        if let Some(ws) = state
            .reader
            .find_workspace(req.workspace.clone())
            .await
            .map_err(|e| internal_err(e.to_string()))?
        {
            if req.reembed && !req.dry_run {
                let purged = state
                    .writer
                    .delete_stale_page_embeddings(ws, None, provider.clone(), model.clone(), dim)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                info!(purged, provider = %provider, model = %model, "purged stale page_embeddings before workspace re-embed");
            }

            let summaries = state
                .reader
                .list_projects_with_stats()
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            for summary in summaries
                .into_iter()
                .filter(|p| p.workspace_name == req.workspace)
            {
                let Some(proj) = state
                    .reader
                    .find_project(ws, summary.project_name.clone())
                    .await
                    .map_err(|e| internal_err(e.to_string()))?
                else {
                    continue;
                };
                let partial =
                    embed_project_pages(&state, &embedder, ws, proj, req.reembed, req.dry_run)
                        .await?;
                totals.absorb(partial);
            }
        }
    } else {
        let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
        if req.reembed && !req.dry_run {
            let purged = state
                .writer
                .delete_stale_page_embeddings(ws, Some(proj), provider.clone(), model.clone(), dim)
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            info!(purged, provider = %provider, model = %model, "purged stale page_embeddings before project re-embed");
        }
        totals = embed_project_pages(&state, &embedder, ws, proj, req.reembed, req.dry_run).await?;
    }

    let report = EmbedReport {
        embedded: totals.embedded,
        skipped: totals.skipped,
        failed: totals.failed,
        would_embed: totals.would_embed,
        provider,
        model,
        dim,
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}
