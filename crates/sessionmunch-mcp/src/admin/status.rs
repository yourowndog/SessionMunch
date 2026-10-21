//! Admin routes: status.

use super::*;

/// Query string for `GET /admin/audit-contamination`. Supplying BOTH
/// `workspace` and `project` scopes the audit to that one landed bucket;
/// omit both to audit every project.
#[derive(Deserialize)]
pub(super) struct AuditContaminationQuery {
    workspace: Option<String>,
    project: Option<String>,
}

/// `GET /admin/audit-contamination` — read-only structural contamination audit
/// (see [`sessionmunch_store::ReaderPool::audit_contamination`]). Reports likely
/// cross-project mislandings (a session whose cwd resolves elsewhere; an
/// observation whose project disagrees with its session); never mutates, so it
/// is safe to run on any cadence (e.g. a cron probe alerting on non-zero counts).
pub(super) async fn handle_audit_contamination(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<AuditContaminationQuery>,
) -> impl IntoResponse {
    let scope = match (
        trimmed_opt(q.workspace.as_deref()),
        trimmed_opt(q.project.as_deref()),
    ) {
        (Some(ws), Some(proj)) => match lookup_ws_proj_no_create(&state, ws, proj).await {
            Ok(ids) => Some(ids),
            Err(e) => return e,
        },
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
    match state
        .reader
        .audit_contamination(scope, state.home_dir.as_deref())
        .await
    {
        Ok(report) => (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Query string for `GET /admin/audit-log`. Names are optional independent
/// filters; when both workspace and project are present they are resolved
/// through [`lookup_existing_scope`] so a typo fails closed (404) instead of
/// looking like an empty log.
#[derive(Debug, Deserialize)]
pub(super) struct AuditLogQuery {
    workspace: Option<String>,
    project: Option<String>,
    op: Option<String>,
    before_id: Option<i64>,
    #[serde(default = "default_audit_log_limit")]
    limit: usize,
}

pub(super) fn default_audit_log_limit() -> usize {
    50
}

/// `GET /admin/audit-log` — read-only paginated trail of the existing
/// `audit_log` table. `detail` is returned for schema fidelity; it is not a
/// payload (the only writer stores the literal `{}`).
pub(super) async fn handle_audit_log(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<AuditLogQuery>,
) -> impl IntoResponse {
    let workspace = trimmed_opt(q.workspace.as_deref()).map(str::to_owned);
    let project = trimmed_opt(q.project.as_deref()).map(str::to_owned);
    if let (Some(ws), Some(proj)) = (workspace.as_deref(), project.as_deref()) {
        match lookup_ws_proj_no_create(&state, ws, proj).await {
            Ok(_) => {}
            Err(e) => return e,
        }
    }
    let filter = AuditLogFilter {
        workspace,
        project,
        op: trimmed_opt(q.op.as_deref()).map(str::to_owned),
        before_id: q.before_id,
        limit: q.limit,
    };
    match state.reader.list_audit_events(filter).await {
        Ok(events) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(&events)
                    .map(|list| serde_json::json!({ "events": list }))
                    .unwrap_or_else(|_| serde_json::json!({ "events": [] })),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// JSON response body for `GET /admin/status`. The CLI's `status`
/// subcommand renders this either as JSON (`--json`) or as a small
/// human-friendly text block.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    /// Server binary version (Cargo package version).
    pub version: String,
    /// Absolute data directory the server uses (server-side path).
    pub data_dir: String,
    /// Bind address the HTTP transport is listening on.
    pub bind: String,
    /// Absolute SQLite path inside `data_dir`.
    pub db_path: String,
    /// Lifetime counts: pages_latest, pages_all, sessions, observations.
    pub counts: sessionmunch_store::StatusCounts,
    /// Derived-index and retrieval-readiness diagnostics.
    pub derived: sessionmunch_store::DerivedIndexStatus,
    /// Physical storage figures — file size and reclaimable free pages — so an
    /// operator can decide whether a `VACUUM` is worth its exclusive lock.
    pub storage: sessionmunch_store::StorageStatus,
    /// Passive process-scoped provider health.
    pub providers: ProviderHealthSnapshot,
    /// Hook-ingestion counters for this server process. Counts and one
    /// timestamp only — never captured content (#428).
    pub ingest: sessionmunch_core::IngestMetricsSnapshot,
    /// Instantaneous write-queue depth `(queued, capacity)` — the
    /// wedged-writer signal (2.0 item 7).
    pub write_queue: (usize, usize),
    /// Wiki-format state: whether the OKF migration has run, and the
    /// pre-migration backup archive if its receipt still exists.
    pub wiki_format: WikiFormatStatus,
}

/// Wiki-format section of [`StatusReport`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct WikiFormatStatus {
    /// True once the OKF v0.2 wiki migration has been applied.
    pub okf_migrated: bool,
    /// Pre-migration backup archive path, when the receipt exists AND
    /// the archive file is still on disk.
    pub backup_archive: Option<String>,
    /// Size of that archive in bytes.
    pub backup_archive_bytes: Option<u64>,
}

/// `GET /admin/projects` — the authoritative list of `(workspace, project)`
/// pairs (with page counts + last-updated), read-only. Useful to any external
/// consumer that needs the live project inventory: a dashboard, an export, or
/// a backup/mirror that lays the wiki out as `wiki/<workspace>/<project>/` and
/// wants to reconcile against it — any directory whose project no longer
/// appears here is an orphan (e.g. from a delete that bypassed the admission
/// chain, such as an offline `--data-dir` purge or a direct DB edit) and can
/// be pruned so the copy reflects the live server.
pub(super) async fn handle_list_projects(
    State(state): State<Arc<AdminState>>,
) -> impl IntoResponse {
    match state.reader.list_projects_with_stats().await {
        Ok(projects) => (
            StatusCode::OK,
            Json(serde_json::json!({ "projects": projects })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

pub(super) async fn handle_status(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    match state.reader.status_counts().await {
        Ok(counts) => match state.reader.derived_index_status().await {
            Ok(derived) => {
                // Three pragma reads; a failure here must not take down the
                // whole status response, which is also the health probe.
                let storage = state.reader.storage_status().await.unwrap_or_default();
                let okf_migrated = state
                    .reader
                    .wiki_migration_names()
                    .await
                    .map(|names| names.iter().any(|n| n.contains("okf")))
                    .unwrap_or(false);
                let backup = sessionmunch_wiki::backup::BackupReceipt::load(&state.data_dir)
                    .filter(sessionmunch_wiki::backup::BackupReceipt::archive_present);
                let report = StatusReport {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    data_dir: state.data_dir.display().to_string(),
                    bind: state.bind.clone(),
                    db_path: state.db_path.display().to_string(),
                    counts,
                    derived,
                    storage,
                    providers: state.provider_health.snapshot(),
                    ingest: state.ingest_metrics.snapshot(),
                    write_queue: state.writer.queue_depth(),
                    wiki_format: WikiFormatStatus {
                        okf_migrated,
                        backup_archive: backup
                            .as_ref()
                            .map(|r| r.archive_path.display().to_string()),
                        backup_archive_bytes: backup.as_ref().map(|r| r.size_bytes),
                    },
                };
                (
                    StatusCode::OK,
                    Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
                )
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}
