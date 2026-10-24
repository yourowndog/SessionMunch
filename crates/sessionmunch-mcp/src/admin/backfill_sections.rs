//! Admin routes: backfill-sections.

use super::*;

/// JSON request body for `POST /admin/backfill-sections`.
#[derive(Deserialize)]
pub(super) struct BackfillSectionsRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Batch size for the bounded processing of latest pages.
    #[serde(default = "default_backfill_batch_size")]
    batch_size: usize,
    /// When true, count pages that would be backfilled without writing.
    #[serde(default)]
    dry_run: bool,
    /// When true, rebuild sections even when a page already has them.
    ///
    /// Currently a no-op: `ops::backfill_sections` (D2) always rebuilds
    /// every latest page's sections/passages unconditionally, so this
    /// flag has no distinct effect until skip-if-exists semantics are
    /// added to that writer path. Kept in the wire format so the CLI's
    /// `--force` flag round-trips without a breaking API change later.
    #[serde(default)]
    #[allow(dead_code)]
    force: bool,
}

/// Summary response from `POST /admin/backfill-sections`.
#[derive(Debug, Serialize)]
pub struct BackfillSectionsReport {
    /// Pages whose sections and passages were rebuilt.
    pub pages_backfilled: u64,
    /// Pages skipped because sections already exist and `force` was false.
    pub pages_skipped: u64,
}

pub(super) fn default_backfill_batch_size() -> usize {
    64
}

pub(super) async fn backfill_sections_for_project(
    state: &AdminState,
    ws_name: &str,
    proj_name: &str,
    ws: WorkspaceId,
    proj: ProjectId,
    batch_size: usize,
    dry_run: bool,
) -> Result<BackfillSectionsReport, (StatusCode, Json<serde_json::Value>)> {
    if dry_run {
        let page_count = state
            .reader
            .list_pages(ws_name, proj_name)
            .await
            .map_err(|e| internal_err(e.to_string()))?
            .len();
        return Ok(BackfillSectionsReport {
            pages_backfilled: u64::try_from(page_count).unwrap_or(0),
            pages_skipped: 0,
        });
    }

    let processed = state
        .writer
        .backfill_sections(ws, proj, batch_size.max(1))
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    Ok(BackfillSectionsReport {
        pages_backfilled: processed,
        pages_skipped: 0,
    })
}

pub(super) async fn handle_backfill_sections(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<BackfillSectionsRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
    let report = backfill_sections_for_project(
        &state,
        &req.workspace,
        &req.project,
        ws,
        proj,
        req.batch_size,
        req.dry_run,
    )
    .await?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}
