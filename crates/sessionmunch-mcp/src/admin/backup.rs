//! Admin routes: backup.

use super::*;

/// JSON request body for `POST /admin/embed-passages`.
#[derive(Deserialize)]
pub(super) struct PassageEmbedRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// When true, re-embed passages that already have a matching
    /// `(provider, model, dim)` row.
    #[serde(default)]
    reembed: bool,
    /// When true, count passages that would be embedded without
    /// calling the embedder or writing anything.
    #[serde(default)]
    dry_run: bool,
}

/// Handler for `POST /admin/embed-passages`.
///
/// Backfill passage-embedding vectors for passages that were recently added or
/// missed during the initial embed pass. Uses the configured embedder and
/// processes passages in batches.
pub(super) async fn handle_embed_passages(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<PassageEmbedRequest>,
) -> impl IntoResponse {
    let Some(embedder) = state.embedder.as_ref().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "embedder not configured on server"
            })),
        );
    };
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let opts = EmbedBackfillOptions {
        reembed: req.reembed,
        dry_run: req.dry_run,
    };
    match run_passage_embedding_backfill(&state.reader, &state.writer, &embedder, ws, proj, opts)
        .await
    {
        Ok(counts) => {
            info!(?counts, "passage embedding backfill complete");
            (
                StatusCode::OK,
                Json(serde_json::to_value(counts).unwrap_or_else(|_| serde_json::json!({}))),
            )
        }
        Err(e) => {
            warn!(error = %e, "passage embedding backfill failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        }
    }
}

/// Handler for `POST /admin/commit`.
///
/// Snapshots the live SQLite DB via the online backup API, then
/// tar-gzips `wiki/`, the snapshot, and `config.toml` (if present)
/// into a tempfile. The response streams the file instead of buffering
/// the full archive in memory.
pub(super) async fn handle_backup(State(state): State<Arc<AdminState>>) -> Response {
    match build_backup_tarball_file(&state).await {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/gzip")
            .header(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"backup.tar.gz\"",
            )
            .body(Body::from_stream(ReaderStream::new(file)))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }),
        Err(e) => {
            warn!(error = %e, "backup failed");
            let body = serde_json::to_vec(&serde_json::json!({ "error": e.to_string() }))
                .unwrap_or_default();
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                })
        }
    }
}

pub(super) async fn build_backup_tarball_file(
    state: &AdminState,
) -> anyhow::Result<tokio::fs::File> {
    let staging = tempfile::tempdir()?;
    let snapshot_path = staging.path().join("memory.sqlite");
    info!(snapshot = %snapshot_path.display(), "snapshotting SQLite for backup");
    state
        .reader
        .snapshot_to(snapshot_path.clone())
        .await
        .map_err(|e| anyhow::anyhow!("sqlite snapshot: {e}"))?;

    let mut tar_file = tempfile::tempfile()?;
    {
        let encoder = GzEncoder::new(&mut tar_file, Compression::default());
        let mut tar = tar::Builder::new(encoder);
        tar.mode(tar::HeaderMode::Deterministic);
        tar.follow_symlinks(false);

        let wiki_dir = state.data_dir.join("wiki");
        if wiki_dir.is_dir() {
            tar.append_dir_all("wiki", &wiki_dir)
                .map_err(|e| anyhow::anyhow!("archiving wiki/: {e}"))?;
        }

        tar.append_path_with_name(&snapshot_path, "db/memory.sqlite")
            .map_err(|e| anyhow::anyhow!("archiving db snapshot: {e}"))?;

        let cfg = state.data_dir.join("config.toml");
        if cfg.is_file() {
            tar.append_path_with_name(&cfg, "config.toml")
                .map_err(|e| anyhow::anyhow!("archiving config.toml: {e}"))?;
        }

        let encoder = tar.into_inner()?;
        encoder.finish()?;
    }
    tar_file.sync_data()?;
    tar_file.rewind()?;
    Ok(tokio::fs::File::from_std(tar_file))
}
