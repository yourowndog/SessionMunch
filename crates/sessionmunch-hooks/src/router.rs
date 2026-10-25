//! axum router exposing `POST /hook`.
//!
//! Returns 202 immediately unless the in-flight hook limit is saturated,
//! in which case it returns 429. Heavy work (DB writes, session-page
//! synthesis) happens *after* the response is sent — but we still `await`
//! the writer ack to honour the cross-cutting invariant that "indexes commit
//! in the same transaction as the data" (no background-task-indexing-after-return,
//! basic-memory #763). The agent never blocks on us thanks to the
//! fire-and-forget client side.

use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Weak};

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sessionmunch_consolidate::{Consolidator, ConsolidatorError};
use sessionmunch_core::{
    ActiveProject, ActorKey, AgentKind, DEFAULT_WORKSPACE_NAME, Handoff, IdentityKey,
    MANAGED_WORKSTREAM_PACKET_MARKER, ManagedRunId, MidSessionRouting, NewHandoff, NewObservation,
    NewSession, ObservationKind, ProjectId, Sanitized, Sanitizer, SessionId, WorkspaceId,
    WorkstreamEvent, WorkstreamEventKind,
};
use sessionmunch_store::{
    HookSessionAdmission, IngestObservationOutcome, StoreError, WriterHandle,
};
use sessionmunch_wiki::{AdmissionContext, AdmissionOp, Wiki};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::capture_policy::{
    CaptureConfig, CaptureDisposition, CapturePolicy, CaptureProtocol, CaptureSource, PolicyState,
    ToolFamily, metadata_only_body, tool_observation_outcome, valid_call_id,
};
use crate::log;
use crate::payload::{
    HookEnvelope, HookEvent, HookQuery, ProjectSource, ProjectStrategy, body_is_subagent,
    parse_agent,
};
use crate::synth::synthesize_session_page;
use sessionmunch_consolidate::session_card_page;

/// Default maximum number of hook events allowed to be processing at once.
///
/// This matches the writer queue order of magnitude and prevents unbounded
/// background tasks during tool-heavy bursts. Saturated servers return 429 so
/// callers can drop or retry instead of growing memory without bound.
pub const DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT: usize = 1024;

/// Maximum keyed-ingest gates retained before dead weak entries are pruned.
///
/// Live gates are bounded by the global ingest semaphore; dead entries carry
/// no mutex allocation and are removed opportunistically.
pub const DEFAULT_INGEST_GATE_MAX_ENTRIES: usize = 4096;

/// Maximum events accepted in one `POST /hook/batch` request. This matches the
/// client drain cap so a single request cannot monopolize ingest capacity or
/// allocate/process an unbounded vector of hook events.
pub const MAX_HOOK_BATCH_ITEMS: usize = 256;

/// Upper bound for reject-policy admission on the synchronous `/handoff`
/// path. The shortest shipped client deadline is the shell hook's one-second
/// curl timeout, so the server must decide earlier or leave the baton open;
/// otherwise an approved response can consume context after the client has
/// already disconnected and can no longer receive it.
const AUTOMATIC_HANDOFF_ADMISSION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(750);

/// Maximum cwd-resolution cache entries kept per server process. The cache is
/// an optimization only; evicted entries are re-resolved through the writer.
pub const DEFAULT_PROJECT_CACHE_MAX_ENTRIES: usize = 4096;

/// Cap on scoped session ids tracked as subagents for the
/// `drop_subagent_captures` tail-drop. Mirrors the project-cache order of
/// magnitude: enough for high fan-out harnesses, still bounded if a client never
/// sends a terminal `SessionEnd`.
const SUBAGENT_SESSIONS_MAX: usize = 4096;

mod limits;
pub use self::limits::*;
mod ingest;
pub use self::ingest::*;
mod handoff;
pub use self::handoff::*;
mod brief;
pub(crate) use self::brief::render_managed_context;
use self::brief::*;
mod scope;
use self::scope::*;
mod process;
use self::process::*;

/// Shared state passed to the hook handler.
#[derive(Clone)]
pub struct HookState {
    /// Default workspace to use when a hook event lacks a `cwd` field.
    pub workspace_id: WorkspaceId,
    /// Default project to use when a hook event lacks a `cwd` field.
    pub project_id: ProjectId,
    /// Writer actor handle.
    pub writer: WriterHandle,
    /// Reader pool — needed for session-end synthesis.
    pub reader: sessionmunch_store::ReaderPool,
    /// Wiki handle — used to write the session-summary page.
    pub wiki: Wiki,
    /// Optional LLM-driven consolidator. When set, PreCompact uses it
    /// to refresh `sessions/<id>.md` before the agent loses its
    /// working context. When `None`, falls back to the deterministic
    /// rule-based synth (still useful, just lower-signal).
    pub consolidator: Option<Arc<Consolidator>>,
    /// Privacy strip applied to every observation before it lands in
    /// the store. Same handle is also held by the wiki and consolidator
    /// so scrubbing happens at every write boundary.
    pub sanitizer: Sanitizer,
    /// Cache of `(cwd, workspace_override, project_override, project_strategy) → ids`.
    /// The composite key avoids poisoning between callers that resolve
    /// the same `cwd` with and without an override during a hook-script
    /// upgrade window. Each tuple element defaults to the empty string
    /// when absent so missing overrides collapse into a single slot.
    pub project_cache: ProjectCache,
    /// Pointer shared with the MCP server. Every cwd-resolved event
    /// publishes its project here so the read tools (which have no cwd
    /// of their own) default to the project the user is actually in
    /// rather than the server's static `--project` (issue #2).
    pub active_project: ActiveProject,
    /// Process-lifetime ingestion counters for operator status reporting.
    pub ingest_metrics: Arc<sessionmunch_core::IngestMetrics>,
    /// In-flight hook processing limiter. Requests acquire one permit before
    /// spawning work and return 429 immediately when saturated.
    pub ingest_semaphore: Arc<tokio::sync::Semaphore>,
    /// Project/key gates that serialize an original delivery with an
    /// overlapping retry until the first processor marks completion or exits.
    pub ingest_gates: IngestGates,
    /// Per-source ingest rate limiter. The global `ingest_semaphore` is acquired
    /// first for stored events so globally rejected events do not spend source
    /// tokens. Disabled (pass-through) unless configured by the CLI.
    pub ingest_rate: Arc<tokio::sync::Mutex<IngestRateLimiter>>,
    /// Opt-in (`SESSIONMUNCH_CONSOLIDATE_ON_SESSION_END`): when true and a
    /// `consolidator` is present, SessionEnd also runs LLM consolidation on
    /// top of the always-written heuristic session page. Off by default so
    /// session close stays cheap; the LLM checkpoint otherwise happens on
    /// PreCompact and via manual `memory_consolidate`.
    pub consolidate_on_session_end: bool,
    /// `SESSIONMUNCH_CONSOLIDATE_ON_COMPACT` (default true): when false,
    /// PreCompact / PostCompaction write the rule-based checkpoint even with
    /// a `consolidator` present, so a provider kept for session-card AI
    /// summaries does not rewrite session pages at every compaction.
    pub consolidate_on_compact: bool,
    /// Coalescing wake-up for the durable SessionEnd consolidation worker.
    /// The database is the queue; notifications only reduce pickup latency.
    pub session_consolidation_notify: Option<Arc<tokio::sync::Notify>>,
    /// Opt-in (`SESSIONMUNCH_CAPTURE_ASSISTANT`): when true, the server honors the
    /// client's `_sessionmunch_assistant` protocol on a `Stop` event and persists
    /// the sanitized excerpt as the Stop body. Off by default; when off the
    /// marker is stripped and the Stop stays empty. Double opt-in: the client
    /// must also have been installed with `--capture-assistant` (#196).
    pub capture_assistant_enabled: bool,
    /// Scoped session keys known to be subagents (seeded by `SubagentStart` / any
    /// marker-bearing event). For a project that opted into
    /// `drop_subagent_captures` (via its `.sessionmunch.toml`, forwarded as the
    /// per-event `drop_subagent` flag), every event of a tracked session is
    /// dropped — closing the unmarked tail
    /// (`user_prompt_submit`/`stop`/`session_end`) the per-event marker misses.
    pub subagent_sessions: SubagentSessions,
    /// Operator home directory, sourced from `Config` once at startup. The
    /// cwd->project resolver never prefix-matches a stored `repo_path` equal
    /// to this, so `$HOME` cannot become a catch-all (issue #103). `None`
    /// disables the guard. Held here so the hooks crate makes no env reads.
    pub home_dir: Option<String>,
    /// `[auth].actor_proxy_bearer_token`: can a trusted proxy
    /// assert identities on this server?
    ///
    /// Half of "does this deployment distinguish operators" — the other half
    /// (`users` rows) is a store read. A proxied deployment never writes a
    /// `users` row, so counting only rows would report a multi-operator server
    /// as single-operator forever. Held here because the hooks crate makes no
    /// config reads.
    pub trusted_proxy_identity: bool,
    /// Namespace slot injection by the qualified request identity.
    pub per_user_slots: bool,
    /// `[routing] mid_session`: whether a mid-session event that wandered out
    /// of the session's tree re-resolves from its own cwd (`follow-cwd`, the
    /// default and historical behavior) or inherits the session's project
    /// (`sticky`). Held here because the hooks crate makes no config reads.
    pub mid_session_routing: sessionmunch_core::MidSessionRouting,
}

/// The owner to stamp on the session and handoff rows this event creates
/// ([`sessionmunch_core::IdentityKey::storage_key`] TEXT).
///
/// One rule, one place: the stamp is `None` unless the deployment actually
/// tells its operators apart, because a single-operator server whose HTTP
/// requests carry `[auth].root_username` would otherwise write rows that the
/// SAME operator's stdio / in-process transport — which carries no actor —
/// cannot see. See [`sessionmunch_core::owner_stamp`].
///
/// The topology is asked per event rather than cached, exactly as the admin
/// gates ask it, so committing a first user starts bucketing without a restart.
/// A request that names nobody short-circuits before the store, so an
/// unauthenticated server pays nothing.
async fn owner_stamp_for_event(
    state: &HookState,
    identity: Option<&IdentityKey>,
) -> Option<String> {
    let identity = identity?;
    let distinguishes = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(distinguishes) => distinguishes,
        // Toward bucketing, not away from it: a row wrongly left shared is
        // readable by every operator on the server and cannot be un-shared
        // afterwards, while a row wrongly stamped is still reachable through
        // the `any_owner` / `all_owners` recovery switches.
        Err(e) => {
            warn!(error = %e, "operator-topology lookup failed; stamping this row per operator");
            true
        }
    };
    sessionmunch_core::owner_stamp(Some(identity), distinguishes)
}

/// The human this request names, if any — as the typed [`IdentityKey`].
///
/// [`sessionmunch_core::ActorContext::identity_key`] rather than `ctx.user`, so
/// the ingress that forwards an OIDC issuer/subject pair is identified here in
/// the same way the auth middleware already treats it. Reading `user` alone
/// would file every operator behind such a proxy as anonymous and hand them all
/// one shared bucket.
fn actor_identity(
    actor: Option<axum::Extension<sessionmunch_core::ActorContext>>,
) -> Option<IdentityKey> {
    actor.and_then(|axum::Extension(ctx)| ctx.identity_key())
}

/// Webhook names this request opted out of, read from the
/// `X-Memory-Skip-Admission-Chain` header.
///
/// The hook ingress runs the admission chain for the two ops that carry no
/// page — `HandoffBegin` on SessionEnd, `HandoffAccept` on session start — so a
/// webhook that reacts to one of them by calling back in through a hook has to
/// be able to exclude itself here, exactly as it can on the MCP and admin
/// paths. Gating is the shared core rule: the header is client-controlled, so a
/// regular DB user must not use it to walk past a reject-policy webhook.
fn admission_skips(
    level: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: &HeaderMap,
) -> Vec<String> {
    sessionmunch_core::skip_admission_chain_for(
        level.map_or(sessionmunch_core::AuthLevel::Anonymous, |ext| ext.0),
        headers
            .get(sessionmunch_core::SKIP_ADMISSION_CHAIN_HEADER)
            .and_then(|value| value.to_str().ok()),
    )
}

/// Build a router with `POST /hook` (event ingress) and `GET /handoff`
/// (synchronous handoff-fetch for session-start hooks).
pub fn hook_router(state: HookState) -> Router {
    Router::new()
        .route("/hook", post(handle_hook))
        .route("/hook/batch", post(handle_hook_batch))
        .route("/handoff", get(handle_handoff))
        .with_state(Arc::new(state))
}

/// Unix milliseconds for an ingest metric stamp, saturating rather than
/// failing: a clock before the epoch or past `u64` must not cost an event.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
