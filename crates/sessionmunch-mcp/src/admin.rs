//! Admin HTTP routes — state-touching operations invoked by the CLI
//! over plain HTTP (not MCP). Currently exposes:
//!
//! - `POST /admin/backup`         — snapshot db + wiki into a gzip tarball (binary response).
//! - `POST /admin/bootstrap`      — ingest a pre-collected source bundle
//!   into seed wiki pages via the configured LLM provider.
//! - `POST /admin/auto-improve`   — review one session and apply or stage proposals.
//! - `POST /admin/auto-improve/report` — read-only auto-improve telemetry report.
//! - `POST /admin/curator`        — dry-run or stage a rule-based curator report.
//! - `GET  /admin/status`         — lifetime counts + server data-dir info.
//! - `GET  /admin/projects`       — authoritative `(workspace, project)` list.
//! - `GET  /admin/open-sessions`  — open (not yet ended) sessions for one scope + agent.
//! - `GET  /admin/sessions/by-agent` — session counts per agent CLI for one scope.
//! - `GET  /admin/activity/by-client` — MCP tool-call counts per client (server-wide).
//! - `GET  /admin/audit-log`      — paginated read of the append-only `audit_log`.
//! - `GET  /admin/search?q=`      — FTS5 hits against the wiki index.
//! - `POST /admin/reorg`          — retro-fit sessions to per-cwd projects.
//! - `POST /admin/lint`           — run the M8 lint pass.
//! - `POST /admin/forget-sweep`   — run the M8 retention sweep.
//! - `POST /admin/embed`          — backfill embeddings for latest pages.
//! - `POST /admin/commit`         — stage + commit the wiki tree via git.
//! - `GET  /admin/checkpoints`    — list recent wiki git checkpoints.
//! - `POST /admin/restore-page`   — restore one page from a checkpoint.
//! - `POST /admin/purge-project`  — delete a project's rows and wiki files
//!   (logical unless `compact` is set; see `sessionmunch_store::Compaction`).
//! - `POST /admin/purge-session`  — delete one session and what it derived.
//! - `POST /admin/rename-project` — rename a project (column-only; no files move).
//! - `POST /admin/rename-workspace` — rename a workspace and refresh scope manifests.
//! - `POST /admin/compact`        — reclaim free pages; deletes nothing.
//! - `POST /admin/delete-workspace` — delete a workspace and its projects
//!   (logical unless `compact` is set; see `sessionmunch_store::Compaction`).
//! - `POST /admin/merge-workspace` — fold every project of one workspace into
//!   another, then delete the emptied source workspace.
//! - `POST /admin/move-project`   — move a project into another workspace
//!   (copy latest pages via the write path, then purge the source).
//! - `POST /admin/write-page`     — write or update a wiki page atomically.
//! - `GET  /admin/read-page`      — fetch the full body of a single wiki page by path.
//! - `POST /admin/delete-page`    — delete a single wiki page by path.
//!
//! The CLI is responsible for filesystem access (collecting sources from
//! the project repo, rendering output for humans); the server is
//! responsible for all state reads/writes against the wiki + SQLite.

use std::sync::Arc;

use std::future::Future;
use std::io::Seek;
use std::path::PathBuf;
use std::pin::Pin;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sessionmunch_consolidate::{
    AutoImproveReviewConfig, AutoImproveTelemetryParams, AutoImproveTelemetryReport, Bootstrap,
    BootstrapConfig, BootstrapOutcome, BootstrapSource, CuratorParams, CuratorReport,
    EmbedBackfillCounts, EmbedBackfillOptions, ObservationRetention, SourceCounts,
    prune_sources_to_budget, render_auto_improve_telemetry_report_markdown,
    render_curator_report_markdown, run_auto_improve_review, run_auto_improve_telemetry_report,
    run_curator_report_with_breadth, run_embedding_backfill, run_lint,
    run_passage_embedding_backfill, run_sweep_with_options,
};
use sessionmunch_core::{
    ActiveProject, AgentKind, AutoImproveProposalId, Capability, DEFAULT_PROJECT_NAME,
    DEFAULT_WORKSPACE_NAME, PagePath, ProjectId, SessionId, Tier, WorkspaceId,
};
use sessionmunch_llm::{Embedder, LlmProvider, ProviderHealth, ProviderHealthSnapshot};
use sessionmunch_store::{
    ApproveAutoImproveProposalResult, AuditLogFilter, AutoImproveProposalOperation,
    AutoImproveProposalStatus, DecayParams, NewAutoImproveProposal, PagesMode, ReaderPool,
    RejectAutoImproveProposal, ScopeResolutionError, SkippedProposal, StageAutoImproveRun,
    StoreError, WriterHandle, create_explicit_scope, f32_vec_to_bytes, lookup_existing_scope,
    lookup_existing_workspace,
};
use sessionmunch_wiki::{
    AdmissionContext, AdmissionOp, Markdown, SessionPageFile, Wiki, WikiError, WritePageRequest,
};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

const CONTRIBUTORS_WEBHOOK_NAME: &str = "contributors";

/// Sweep knobs that live beside `DecayParams` rather than inside it, injected
/// as one Extension so adding the next one is not a third router constructor.
#[derive(Clone, Copy, Default)]
struct SweepTuning {
    breadth_weight: f64,
    retention: ObservationRetention,
}

/// Shared state for the admin router.
#[derive(Clone)]
pub struct AdminState {
    /// Writer actor handle — used to get-or-create workspace/project.
    pub writer: WriterHandle,
    /// Reader pool — used by the idempotency check inside Bootstrap.
    pub reader: ReaderPool,
    /// Wiki handle — pages are written here.
    pub wiki: Wiki,
    /// Optional LLM provider. When `None`, bootstrap returns 503.
    pub llm: Option<Arc<dyn LlmProvider>>,
    /// If true, auto-improve leaves staged proposals pending for manual
    /// approval; otherwise it approves them immediately through the normal wiki
    /// write path.
    pub auto_improve_require_approval: bool,
    /// Server-configured defaults used by direct admin auto-improvement.
    /// Request fields remain explicit overrides; omitted eval settings inherit
    /// this operator policy instead of disabling eval silently.
    pub auto_improve_review_config: AutoImproveReviewConfig,
    /// Optional embedder. When `None`, `/admin/embed` returns 503.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Passive process-scoped health recorder for configured providers.
    pub provider_health: ProviderHealth,
    /// Shared hook-ingestion counters, written by the hook path and read
    /// here for status reporting.
    pub ingest_metrics: std::sync::Arc<sessionmunch_core::IngestMetrics>,
    /// Retention-decay parameters forwarded from server config.
    pub decay_params: DecayParams,
    /// Server's resolved data directory (e.g. `/data` in the docker
    /// image). Surfaced via `/admin/status` so the CLI can report
    /// "where the wiki + db actually live".
    pub data_dir: PathBuf,
    /// Resolved SQLite path inside `data_dir`. Same purpose as above.
    pub db_path: PathBuf,
    /// Server's bind address — informational, surfaced in /admin/status.
    pub bind: String,
    /// Server home directory resolved once at config load. Used to keep admin
    /// audits consistent with the hook router's cwd-prefix guard.
    pub home_dir: Option<String>,
    /// Serialises concurrent bootstrap requests. Bootstrap fans out
    /// into an LLM call + a multi-page wiki write + a git commit;
    /// running two in parallel would race the `commit_all` git ops and
    /// stack LLM cost unnecessarily. The mutex is held for the entire
    /// handler so a second caller waits its turn (request stays open
    /// until the lock is acquired, then proceeds normally).
    pub bootstrap_lock: Arc<tokio::sync::Mutex<()>>,
    /// Per-server token pepper, used by `/admin/api-credentials` to hash
    /// freshly-issued `aim_` secrets before they land in
    /// `api_credentials.token_hash`. `None` when the operator hasn't set
    /// `[auth].token_pepper` in config (single-user installs that predate
    /// v0.8); native API-credential endpoints then return 503
    /// `multi-user not enabled`.
    pub token_pepper: Option<sessionmunch_store::TokenPepper>,
    /// Shared in-process pointer to the project the agent is currently
    /// active in (published by the hook router). Read by `move-project` to
    /// refuse moving the live project (unless `force`) and to keep the
    /// pointer correct after a move. Empty `ActiveProject::new()` when no
    /// hook router is attached (admin-only tests).
    pub active_project: ActiveProject,
    /// Optional hook to PROACTIVELY evict the hook router's per-cwd
    /// `(workspace_id, project_id)` cache after admin scope mutations. Admin
    /// handlers await this after successful moves/purges/deletes/renames so
    /// the next hook event re-resolves cleanly instead of racing through a
    /// stale cached pair. `None` when no hook router is attached (stdio /
    /// admin-only tests).
    pub scope_invalidator: Option<ScopeInvalidator>,
    /// True when a trusted authenticating proxy is configured to assert
    /// end-user identities (`[auth].actor_proxy_bearer_token`).
    ///
    /// Proxy-asserted operators never get a `users` row, so `users_exist()`
    /// alone answers "does this deployment distinguish operators" with a
    /// permanent no — and every proxied caller walks through the
    /// single-operator escape hatch on the `/admin/*` gate. Static config, set
    /// once at startup; `false` for every deployment that never configures a
    /// proxy secret, which is what keeps single-operator servers on their
    /// historical behaviour.
    pub trusted_proxy_identity: bool,
}

/// Async hook-router project-cache invalidator installed by the serve command.
pub type ScopeInvalidator = Arc<
    dyn Fn(ScopeInvalidation) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync,
>;

/// Hook-router project-cache invalidation target for admin scope mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeInvalidation {
    /// Evict cache entries whose resolved project id matches.
    Project(ProjectId),
    /// Evict cache entries whose resolved workspace id matches.
    Workspace(WorkspaceId),
}

async fn invalidate_scope_cache(state: &AdminState, target: ScopeInvalidation) {
    if let Some(evict) = &state.scope_invalidator {
        evict(target).await;
    }
}

/// JSON request body for `POST /admin/bootstrap`.
#[derive(Deserialize)]
struct BootstrapRequest {
    /// Workspace name (auto-created if it doesn't exist).
    workspace: String,
    /// Project name (auto-created if it doesn't exist).
    project: String,
    /// Sources pre-collected on the client side.
    sources: Vec<BootstrapSource>,
    /// Original collection size before client-side prune (if any).
    #[serde(default)]
    sources_collected: Option<usize>,
    /// Maximum input tokens for LLM call.
    #[serde(default = "default_max_input_tokens")]
    max_input_tokens: usize,
    /// Per-LLM-call input cap; larger bundles are split into chunks.
    #[serde(default = "default_chunk_input_tokens")]
    chunk_input_tokens: usize,
    /// Skip the LLM call and page writes — returns a dry-run outcome.
    #[serde(default)]
    dry_run: bool,
    /// Allow re-bootstrap when `wiki/bootstrap.md` already exists.
    #[serde(default)]
    force: bool,
    /// Resume an interrupted bootstrap: reuse the chunks already completed
    /// for the same sources instead of re-running them (#621).
    #[serde(default)]
    resume: bool,
}

fn default_max_input_tokens() -> usize {
    50_000
}

fn default_chunk_input_tokens() -> usize {
    sessionmunch_consolidate::DEFAULT_CHUNK_INPUT_TOKENS
}

/// JSON request body for `POST /admin/auto-improve`.
#[derive(Deserialize)]
struct AutoImproveRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Completed session to review.
    session_id: SessionId,
    /// Minimum observations before the LLM review runs.
    #[serde(default = "default_auto_improve_min_observations")]
    min_observations: usize,
    /// Minimum session span before the LLM review runs.
    #[serde(default = "default_auto_improve_min_session_duration_secs")]
    min_session_duration_secs: u64,
    /// Minimum proposal confidence accepted by validation.
    #[serde(default = "default_auto_improve_min_confidence")]
    min_confidence: f32,
    /// Approximate chars/4 prompt budget.
    #[serde(default = "default_auto_improve_max_input_tokens")]
    max_input_tokens: usize,
    /// Maximum validated proposals returned.
    #[serde(default = "default_auto_improve_max_proposals")]
    max_proposals_per_run: usize,
    /// Whether future raw fallback details may be considered.
    #[serde(default)]
    include_raw_fallback: bool,
    /// Maximum existing _rules/ and procedures/ pages included for patch proposals.
    #[serde(default = "default_auto_improve_max_patchable_pages")]
    max_patchable_pages: usize,
    /// Maximum body chars rendered per patchable target page.
    #[serde(default = "default_auto_improve_max_patchable_body_chars")]
    max_patchable_body_chars: usize,
    /// Maximum patch edits per proposal.
    #[serde(default = "default_auto_improve_max_edits_per_proposal")]
    max_edits_per_proposal: usize,
    /// Maximum content chars in one patch edit.
    #[serde(default = "default_auto_improve_max_edit_content_chars")]
    max_edit_content_chars: usize,
    /// Maximum aggregate changed chars in one patch proposal.
    #[serde(default = "default_auto_improve_max_changed_chars_per_proposal")]
    max_changed_chars_per_proposal: usize,
    /// Maximum patch edits accepted across one review run.
    #[serde(default = "default_auto_improve_max_patch_edits_per_run")]
    max_patch_edits_per_run: usize,
    /// Maximum recent rejection-buffer entries rendered into prompt context.
    #[serde(default = "default_auto_improve_max_rejection_context")]
    max_rejection_context: usize,
    /// Maximum age in days for rejection-buffer prompt context.
    #[serde(default = "default_auto_improve_rejection_context_days")]
    rejection_context_days: u32,
    /// Maximum materialized final body size.
    #[serde(default = "default_auto_improve_max_final_body_chars")]
    max_final_body_chars: usize,
    /// Maximum approximate tokens allowed in one _rules/ page.
    #[serde(default = "default_auto_improve_max_rule_page_tokens")]
    max_rule_page_tokens: usize,
    /// Maximum approximate tokens allowed in one procedures/ page.
    #[serde(default = "default_auto_improve_max_procedure_page_tokens")]
    max_procedure_page_tokens: usize,
    /// Synthetic actor used for staged proposal provenance.
    #[serde(default = "default_auto_improve_proposal_actor")]
    proposal_actor: String,
    /// Pending proposal sidecar folder.
    #[serde(default = "default_auto_improve_pending_path")]
    pending_path: String,
    /// Optional executable proposal eval gate. When omitted, direct admin
    /// requests inherit the server's configured eval policy; CLI requests send
    /// their local config explicitly.
    #[serde(default)]
    eval: Option<sessionmunch_consolidate::AutoImproveEvalConfig>,
    /// Removed compatibility field. Requests that still send it fail closed so
    /// old preview callers cannot accidentally write pages.
    #[serde(default)]
    dry_run: Option<bool>,
    /// Removed compatibility field.
    #[serde(default)]
    stage: Option<bool>,
    /// Removed compatibility field.
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct AutoImproveStageResponse {
    run_id: String,
    approval_required: bool,
    approval_policy: String,
    session_id: String,
    summary: String,
    warnings: Vec<String>,
    rejected_candidates_count: usize,
    proposals: Vec<AutoImproveProposalOutcome>,
    /// Proposals the reviewer produced but the store did not stage, with the
    /// reason. Always present (empty on a clean run) so a consumer can tell
    /// "nothing was dropped" from "this build does not report drops".
    skipped: Vec<SkippedProposal>,
}

#[derive(Debug, Serialize)]
struct AutoImproveProposalOutcome {
    id: String,
    sidecar_path: String,
    status: String,
    page_id: Option<String>,
}

/// JSON request body for `POST /admin/auto-improve/report`.
#[derive(Deserialize)]
struct AutoImproveTelemetryReportRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Lookback window in days.
    #[serde(default = "default_auto_improve_telemetry_since_days")]
    since_days: u32,
    /// Maximum rows in each top-N count table.
    #[serde(default = "default_auto_improve_telemetry_top_limit")]
    limit: usize,
    /// Stage one pending telemetry report page instead of returning a read-only report.
    #[serde(default)]
    stage: bool,
}

#[derive(Debug, Serialize)]
struct AutoImproveTelemetryReportStageResponse {
    run_id: String,
    proposal_ids: Vec<String>,
    sidecar_paths: Vec<String>,
    /// Same reason as [`AutoImproveStageResponse::skipped`]: a single-proposal
    /// run that collides otherwise returns an empty `proposal_ids` and no clue.
    skipped: Vec<SkippedProposal>,
    report: AutoImproveTelemetryReport,
}

/// JSON request body for `POST /admin/curator`.
#[derive(Deserialize)]
struct CuratorRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Return a report without staging anything.
    #[serde(default)]
    dry_run: bool,
    /// Stage one pending report page.
    #[serde(default)]
    stage: bool,
    /// Optional mode alias (`dry_run` or `stage`).
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct CuratorStageResponse {
    run_id: String,
    proposal_ids: Vec<String>,
    sidecar_paths: Vec<String>,
    /// Same reason as [`AutoImproveStageResponse::skipped`]: a single-proposal
    /// run that collides otherwise returns an empty `proposal_ids` and no clue.
    skipped: Vec<SkippedProposal>,
    report: CuratorReport,
}

/// Query for `GET /admin/handoffs` (#513).
#[derive(Debug, Deserialize)]
struct OpenHandoffsQuery {
    workspace: String,
    project: String,
    #[serde(default = "default_open_handoffs_limit")]
    limit: usize,
}

const fn default_open_handoffs_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct PendingWritesQuery {
    workspace: String,
    project: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default = "default_pending_writes_limit")]
    limit: usize,
}

fn default_pending_writes_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct PendingWriteScopeQuery {
    workspace: String,
    project: String,
}

#[derive(Debug, Deserialize)]
struct PendingRejectRequest {
    #[serde(default)]
    reason: String,
}

fn default_auto_improve_min_observations() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS
}

fn default_auto_improve_min_session_duration_secs() -> u64 {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS
}

fn default_auto_improve_min_confidence() -> f32 {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE
}

fn default_auto_improve_max_input_tokens() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS
}

fn default_auto_improve_max_proposals() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS
}

fn default_auto_improve_max_patchable_pages() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES
}

fn default_auto_improve_max_patchable_body_chars() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS
}

fn default_auto_improve_max_edits_per_proposal() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL
}

fn default_auto_improve_max_edit_content_chars() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS
}

fn default_auto_improve_max_changed_chars_per_proposal() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL
}

fn default_auto_improve_max_patch_edits_per_run() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN
}

fn default_auto_improve_max_rejection_context() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT
}

fn default_auto_improve_rejection_context_days() -> u32 {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS
}

fn default_auto_improve_max_final_body_chars() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS
}

fn default_auto_improve_max_rule_page_tokens() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS
}

fn default_auto_improve_max_procedure_page_tokens() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS
}

fn default_auto_improve_proposal_actor() -> String {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR.into()
}

fn default_auto_improve_pending_path() -> String {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_PENDING_PATH.into()
}

fn default_auto_improve_telemetry_since_days() -> u32 {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_TELEMETRY_SINCE_DAYS
}

fn default_auto_improve_telemetry_top_limit() -> usize {
    sessionmunch_consolidate::DEFAULT_AUTO_IMPROVE_TELEMETRY_TOP_LIMIT
}

fn hex_to_sha256(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err("expected 64 hex chars".into());
    }
    let mut out = [0_u8; 32];
    for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        out[idx] = u8::from_str_radix(s, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Build the admin axum [`Router`]. Mounts:
/// - `POST /admin/backup`
/// - `POST /admin/bootstrap`
/// - `POST /admin/auto-improve`
/// - `POST /admin/auto-improve/report`
/// - `POST /admin/curator`
/// - `GET  /admin/status`
/// - `GET  /admin/projects`
/// - `GET  /admin/open-sessions`
/// - `GET  /admin/sessions/by-agent`
/// - `GET  /admin/activity/by-client`
/// - `GET  /admin/audit-contamination`
/// - `GET  /admin/search`
/// - `GET  /admin/read-page`
/// - `POST /admin/reorg`
/// - `POST /admin/lint`
/// - `POST /admin/forget-sweep`
/// - `POST /admin/embed`
/// - `POST /admin/commit`
/// - `GET  /admin/checkpoints`
/// - `POST /admin/restore-page`
/// - `POST /admin/purge-project`
/// - `POST /admin/rename-project`
/// - `POST /admin/move-project`
/// - `POST /admin/move-session`
/// - `POST /admin/merge-workspace`
/// - `POST /admin/write-page`
/// - `POST /admin/delete-page`
/// - user-management routes under `/admin/users*`
pub fn admin_router(state: AdminState) -> Router {
    admin_router_with_decay_breadth(state, 0.0)
}

/// Build the admin router with the optional distinct-reader retention weight.
pub fn admin_router_with_decay_breadth(state: AdminState, breadth_weight: f64) -> Router {
    admin_router_with_sweep_tuning(state, breadth_weight, ObservationRetention::default())
}

/// Build the admin router with every sweep knob that lives outside
/// `DecayParams`, including the opt-in observation prune (disabled by default).
pub fn admin_router_with_sweep_tuning(
    state: AdminState,
    breadth_weight: f64,
    retention: ObservationRetention,
) -> Router {
    let state = Arc::new(state);
    let operational = Router::new()
        .route("/admin/backup", post(handle_backup))
        .route("/admin/export-okf", post(handle_export_okf))
        .route("/admin/bootstrap", post(handle_bootstrap))
        .route("/admin/auto-improve", post(handle_auto_improve))
        .route(
            "/admin/auto-improve/report",
            post(handle_auto_improve_report),
        )
        .route("/admin/curator", post(handle_curator))
        .route("/admin/handoffs", get(handle_open_handoffs_list))
        .route("/admin/handoffs/expire", post(handle_expire_handoffs))
        .route("/admin/pending-writes", get(handle_pending_writes_list))
        .route(
            "/admin/pending-writes/{id}",
            get(handle_pending_write_detail),
        )
        .route(
            "/admin/pending-writes/{id}/diff",
            get(handle_pending_write_diff),
        )
        .route(
            "/admin/pending-writes/{id}/approve",
            post(handle_pending_write_approve),
        )
        .route(
            "/admin/pending-writes/{id}/reject",
            post(handle_pending_write_reject),
        )
        .route("/admin/status", get(handle_status))
        .route("/admin/projects", get(handle_list_projects))
        .route("/admin/open-sessions", get(handle_open_sessions))
        .route("/admin/sessions/by-agent", get(handle_sessions_by_agent))
        .route("/admin/activity/by-client", get(handle_activity_by_client))
        .route(
            "/admin/audit-contamination",
            get(handle_audit_contamination),
        )
        .route("/admin/audit-log", get(handle_audit_log))
        .route("/admin/search", get(handle_search))
        .route("/admin/search-events", get(handle_search_events))
        .route("/admin/read-page", get(handle_read_page))
        .route("/admin/reorg", post(handle_reorg))
        .route("/admin/lint", post(handle_lint))
        .route("/admin/forget-sweep", post(handle_forget_sweep))
        .route("/admin/backfill-sections", post(handle_backfill_sections))
        .route("/admin/embed", post(handle_embed))
        .route("/admin/embed-passages", post(handle_embed_passages))
        .route("/admin/commit", post(handle_commit))
        .route("/admin/checkpoints", get(handle_checkpoints))
        .route("/admin/restore-page", post(handle_restore_page))
        .route("/admin/purge-project", post(handle_purge_project))
        .route("/admin/purge-session", post(handle_purge_session))
        .route("/admin/rename-project", post(handle_rename_project))
        .route("/admin/move-project", post(handle_move_project))
        .route("/admin/move-session", post(handle_move_session))
        .route("/admin/delete-workspace", post(handle_delete_workspace))
        .route("/admin/compact", post(handle_compact))
        .route("/admin/rename-workspace", post(handle_rename_workspace))
        .route("/admin/merge-workspace", post(handle_merge_workspace))
        .route("/admin/write-page", post(handle_write_page))
        .route("/admin/delete-page", post(handle_delete_page));
    let users = Router::new()
        .route(
            "/admin/users",
            get(handle_list_users).post(handle_create_user),
        )
        .route("/admin/human-users", post(handle_create_human_user))
        .route("/admin/users/{username}", patch(handle_patch_user))
        .route("/admin/users/{username}/expire", post(handle_expire_user))
        .route("/admin/users/{username}/revive", post(handle_revive_user))
        .route(
            "/admin/users/{username}/rotate-token",
            post(handle_rotate_user_token),
        )
        .route(
            "/admin/users/{username}/reset-password",
            post(handle_reset_password),
        )
        .route("/admin/users/{username}/disable", post(handle_disable_user))
        .route("/admin/users/{username}/enable", post(handle_enable_user))
        .route(
            "/admin/api-credentials",
            get(handle_list_api_credentials).post(handle_create_api_credential),
        )
        .route(
            "/admin/api-credentials/{id}/rotate",
            post(handle_rotate_api_credential),
        )
        .route(
            "/admin/api-credentials/{id}/revoke",
            post(handle_revoke_api_credential),
        );
    operational
        .merge(users)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_root_for_multiuser_admin,
        ))
        .with_state(state)
        .layer(axum::Extension(SweepTuning {
            breadth_weight,
            retention,
        }))
}

async fn require_root_for_multiuser_admin(
    State(state): State<Arc<AdminState>>,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let level = req
        .extensions()
        .get::<sessionmunch_core::AuthLevel>()
        .copied()
        .unwrap_or(sessionmunch_core::AuthLevel::Anonymous);
    // Read this for every request rather than caching a post-creation flag: a
    // committed first user immediately closes bootstrap admin access, including
    // across concurrent requests and without a server restart.
    //
    // Same notion of "this deployment distinguishes operators" the MCP admin
    // gate uses. Keyed on `users_exist()` alone, a trusted-proxy deployment
    // with no `users` rows would wave a proxy-asserted `AuthLevel::User` caller
    // through every operational route below — purge, delete-page, backup — and
    // only `/admin/users*` would survive on its own inner root check.
    let distinguishes_operators = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(exists) => exists,
        Err(error) => {
            tracing::error!(%error, "admin authorization could not determine whether users exist");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "admin authorization unavailable: could not verify user configuration"
                })),
            )
                .into_response();
        }
    };
    match level.authorize(Capability::Admin, distinguishes_operators) {
        Ok(()) => next.run(req).await,
        Err(e) => (
            authz_status(e),
            Json(serde_json::json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

fn authz_status(err: sessionmunch_core::AuthzError) -> StatusCode {
    if err.is_authentication_required() {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::FORBIDDEN
    }
}

mod backup;
use self::backup::*;
mod export_okf;
use self::export_okf::*;
mod status;
pub use self::status::*;
mod open_sessions;
use self::open_sessions::*;
mod search;
use self::search::*;
mod read_page;
use self::read_page::*;
mod helpers;
use self::helpers::*;
mod bootstrap;
use self::bootstrap::*;
mod auto_improve;
use self::auto_improve::*;
mod auto_improve_report;
use self::auto_improve_report::*;
mod reorg;
pub use self::reorg::*;
mod lint;
use self::lint::*;
mod forget_sweep;
use self::forget_sweep::*;
mod backfill_sections;
pub use self::backfill_sections::*;
mod embed;
pub use self::embed::*;
mod commit;
use self::commit::*;
mod restore_page;
use self::restore_page::*;
mod purge_session;
pub use self::purge_session::*;
mod purge_project;
pub use self::purge_project::*;
mod rename_project;
pub use self::rename_project::*;
mod move_project;
pub use self::move_project::*;
mod move_session;
pub use self::move_session::*;
mod merge_workspace;
pub use self::merge_workspace::*;
mod write_page;
use self::write_page::*;
mod delete_page;
use self::delete_page::*;
mod users;
use self::users::*;

#[cfg(test)]
mod tests;
