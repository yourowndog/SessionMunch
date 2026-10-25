//! Authenticated batch ingest for the promoted session-owned event ledger.
//!
//! Remote nodes spool session events locally and drain them here (North Star
//! P1). This mirrors the managed-workstream ingress in [`workstream`]: the
//! router is mounted beside `/hook` and `/workstream/*` so it receives the
//! same bearer-auth middleware as every other machine ingress, then each
//! handler re-checks the capability on the resolved [`AuthLevel`].
//!
//! Contract:
//! - `POST /session-events` accepts a batch of session events plus session
//!   provenance, ensures the `sessions` row exists, and appends the events to
//!   `session_events` via the single-writer actor.
//! - The `session_id` is caller-supplied and idempotent, exactly like the
//!   portable concept: re-draining the same `event_id`s is a no-op and yields
//!   identical row counts (`UNIQUE(session_id, event_id)`).
//! - The session is `begin_session_with_provenance`-d in a transaction, so
//!   provenance and the base row converge on one identity.
//! - Untrusted remote payload text (content, metadata, provenance free-text)
//!   is scrubbed through the [`Sanitizer`] before the store write — the same
//!   typed boundary the workstream and hook paths use.
//!
//! Response is `202 Accepted` with `{ session_id, accepted }` where `accepted`
//! counts genuinely-new rows; duplicates are silently skipped so drains are
//! idempotent.

use std::path::PathBuf;
use std::str::FromStr as _;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sessionmunch_core::{
    AgentKind, AuthLevel, Capability, NewSession, NewSessionEvent, Sanitizer, SessionId,
    SessionProvenance,
};
use sessionmunch_store::{ReaderPool, WriterHandle, create_explicit_scope};
use tracing::debug;

/// Hard cap on events in one batch, matching the managed-workstream finish.
const MAX_EVENTS_PER_BATCH: usize = 4_096;
/// Per-event free-text content cap, matching the store's own 16 KiB bound so
/// the endpoint rejects rather than silently relying on truncation at the
/// durable boundary.
const MAX_EVENT_CONTENT_BYTES: usize = 16 * 1024;
/// Stable event identifier cap.
const MAX_EVENT_ID_BYTES: usize = 512;
/// Per-event metadata cap.
const MAX_METADATA_BYTES: usize = 16 * 1024;
/// Harness wire string cap.
const MAX_HARNESS_BYTES: usize = 64;
/// Session provenance path / free-text field cap.
const MAX_PROVENANCE_TEXT_BYTES: usize = 16 * 1024;

/// State shared by the session-event ingest endpoints.
#[derive(Clone)]
pub struct SessionEventState {
    /// Single-writer store actor (all `session_events` writes funnel through).
    pub writer: WriterHandle,
    /// Read pool if a handler needs to look up a scope or row.
    pub reader: ReaderPool,
    /// Privacy scrubber applied before any raw or indexed persistence.
    pub sanitizer: Sanitizer,
    /// Wiki for refreshing session cards after a batch lands. `None` stores
    /// events only (ingest-only test harnesses).
    pub wiki: Option<sessionmunch_wiki::Wiki>,
}

/// Build the batch-ingest router. Mounted beside `/hook` and `/workstream/*`,
/// so it receives the same bearer-auth middleware as all machine ingress.
pub fn session_events_router(state: SessionEventState) -> Router {
    Router::new()
        .route("/session-events", post(ingest_batch))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

type ApiFailure = (StatusCode, Json<ApiError>);

fn api_failure(status: StatusCode, message: impl Into<String>) -> ApiFailure {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    api_failure(status, message).into_response()
}

fn authorize(
    level: Option<Extension<AuthLevel>>,
    capability: Capability,
) -> Result<(), ApiFailure> {
    let level = level.map_or(AuthLevel::Anonymous, |Extension(level)| level);
    level.authorize(capability, true).map_err(|failure| {
        let status = if failure.is_authentication_required() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::FORBIDDEN
        };
        api_failure(status, failure.message())
    })
}

/// Wire shape of one batch-ingest request. Provenance is optional (a remote
/// node replaying a bare session-id drain may not carry git detail); the
/// session row and its events are the required payload.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestSessionEventsRequest {
    /// Workspace the session belongs to. Created if absent (write path).
    #[serde(default)]
    pub workspace: String,
    /// Project the session belongs to. Created if absent (write path).
    #[serde(default)]
    pub project: String,
    /// Caller-supplied, idempotent session identifier.
    pub session_id: String,
    /// Harness wire string (e.g. `hermes`, `open-code`). Unknown values map to
    /// `other` and are stored as free text — never the empty harness.
    #[serde(default)]
    pub harness: String,
    /// Working directory at session start.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional operator identity (`IdentityKey` storage key).
    #[serde(default)]
    pub actor_user: Option<String>,
    /// Optional provenance written best-effort alongside the base row.
    #[serde(default)]
    pub provenance: SessionProvenance,
    /// Events to append. Idempotent on each `event_id`.
    pub events: Vec<NewSessionEvent>,
}

#[derive(Debug, Serialize)]
struct IngestSessionEventsResponse {
    session_id: SessionId,
    accepted: usize,
}

/// `POST /session-events`
async fn ingest_batch(
    State(state): State<SessionEventState>,
    level: Option<Extension<AuthLevel>>,
    Json(mut request): Json<IngestSessionEventsRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }

    // --- Schema validation before any store work (fail closed on 400) ---
    if request.workspace.trim().is_empty() || request.project.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "workspace and project cannot be empty",
        );
    }
    if request.session_id.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "session_id cannot be empty");
    }
    let session_id = match SessionId::from_str(request.session_id.trim()) {
        Ok(id) => id,
        Err(failure) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("invalid session_id: {failure}"),
            );
        }
    };
    let harness = request.harness.trim();
    if harness.is_empty() || harness.len() > MAX_HARNESS_BYTES {
        return error(StatusCode::BAD_REQUEST, "invalid harness");
    }
    let harness_kind = AgentKind::from_wire(harness);
    if request.events.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "batch must contain at least one event",
        );
    }
    if request.events.len() > MAX_EVENTS_PER_BATCH {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {MAX_EVENTS_PER_BATCH} events per batch"),
        );
    }
    if let Some(cwd) = &request.cwd
        && cwd.len() > MAX_PROVENANCE_TEXT_BYTES
    {
        return error(StatusCode::BAD_REQUEST, "cwd is too long");
    }
    if let Err(message) = sanitize_batch(&state.sanitizer, &mut request) {
        return error(StatusCode::BAD_REQUEST, message);
    }

    // Resolve (or create) the explicit scope on the authoring side.
    let scope = match create_explicit_scope(
        &state.writer,
        request.workspace.trim(),
        request.project.trim(),
    )
    .await
    {
        Ok(scope) => scope,
        Err(failure) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string());
        }
    };

    // Ensure the session row exists with provenance in one transaction.
    let new_session = NewSession {
        id: session_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        agent_kind: harness_kind,
        cwd: request.cwd.as_deref().map(PathBuf::from),
        actor_user: request.actor_user.clone(),
    };
    if let Err(failure) = state
        .writer
        .begin_session_with_provenance(new_session, request.provenance.clone())
        .await
    {
        // A collision is this batch's fault and never heals on retry; 409 lets
        // a drainer set it aside instead of wedging every batch behind it.
        let status = if matches!(failure, sessionmunch_store::StoreError::SessionCollision) {
            StatusCode::CONFLICT
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        return error(status, failure.to_string());
    }

    // Idempotent append: duplicates are skipped at the store (`UNIQUE
    // (session_id, event_id)`), so re-draining an identical batch yields the
    // same count and zero duplication.
    let accepted = match state
        .writer
        .append_session_events(session_id, request.events)
        .await
    {
        Ok(n) => n,
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    debug!(session_id = %session_id, accepted, "session event batch ingested");
    // The events are committed; the card follows them (bounded, never fails
    // the batch). Only new events can change a card.
    if accepted > 0
        && let Some(wiki) = &state.wiki
    {
        crate::session_card::refresh_cards_after_ingest(wiki, &state.reader, session_id).await;
    }
    (
        StatusCode::ACCEPTED,
        Json(IngestSessionEventsResponse {
            session_id,
            accepted,
        }),
    )
        .into_response()
}

/// Validate and scrub an untrusted remote batch before it reaches the store.
///
/// This is the typed privacy boundary for session-event ingest: every
/// free-text carrier an attacker controls (`content`, serialized `metadata`,
/// provenance free-text) passes through the [`Sanitizer`] before persistence,
/// exactly like the workstream and hook paths.
fn sanitize_batch(
    sanitizer: &Sanitizer,
    request: &mut IngestSessionEventsRequest,
) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for event in &mut request.events {
        if event.event_id.trim().is_empty() || event.event_id.len() > MAX_EVENT_ID_BYTES {
            return Err(format!("invalid event_id '{}'", event.event_id));
        }
        // Reject duplicate ids *within* one batch (the column stays unique).
        if !seen.insert(event.event_id.clone()) {
            return Err(format!("duplicate event_id '{}' in batch", event.event_id));
        }
        if event.content.len() > MAX_EVENT_CONTENT_BYTES {
            let mut end = MAX_EVENT_CONTENT_BYTES;
            while !event.content.is_char_boundary(end) {
                end -= 1;
            }
            event.content.truncate(end);
            event.content.push_str("\n[truncated by sessionmunch]");
        }
        event.content = sanitizer.scrub(&event.content);
        if let Some(role) = &event.role
            && (role.len() > 32
                || !role
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')))
        {
            return Err("invalid session event message role".to_string());
        }
        let raw_metadata = serde_json::to_string(&event.metadata).map_err(|e| e.to_string())?;
        if raw_metadata.len() > MAX_METADATA_BYTES {
            event.metadata = serde_json::json!({ "truncated": true });
        } else {
            let scrubbed = sanitizer.scrub(&raw_metadata);
            event.metadata = serde_json::from_str(&scrubbed)
                .unwrap_or_else(|_| serde_json::json!({ "redacted": true }));
        }
    }

    // Provenance free-text is also scrubbed (best-effort, per-field).
    let provenance = &mut request.provenance;
    for value in [
        &mut provenance.native_session_id,
        &mut provenance.machine_id,
        &mut provenance.cwd,
        &mut provenance.git_root,
        &mut provenance.git_branch,
        &mut provenance.commit_at_start,
        &mut provenance.import_cursor,
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > MAX_PROVENANCE_TEXT_BYTES {
            *value = value.chars().take(MAX_PROVENANCE_TEXT_BYTES).collect();
        }
        *value = sanitizer.scrub(value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use sessionmunch_core::WorkstreamEventKind;
    use sessionmunch_store::Store;
    use tempfile::TempDir;

    use super::*;

    fn test_state(store: &Store) -> SessionEventState {
        SessionEventState {
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            sanitizer: Sanitizer::default(),
            wiki: None,
        }
    }

    fn request_body(
        workspace: &str,
        project: &str,
        session_id: &str,
        harness: &str,
        events: Vec<NewSessionEvent>,
    ) -> IngestSessionEventsRequest {
        IngestSessionEventsRequest {
            workspace: workspace.into(),
            project: project.into(),
            session_id: session_id.into(),
            harness: harness.into(),
            cwd: None,
            actor_user: None,
            provenance: SessionProvenance::default(),
            events,
        }
    }

    fn sample_event(id: &str, content: &str) -> NewSessionEvent {
        NewSessionEvent {
            event_id: id.to_string(),
            kind: WorkstreamEventKind::Message,
            role: Some("user".into()),
            content: content.to_string(),
            occurred_at: Some("2026-10-02T00:00:00Z".into()),
            source: sessionmunch_core::SessionSource::Hook,
            metadata: serde_json::json!({"model": "deepseek-v4"}),
        }
    }

    #[tokio::test]
    async fn valid_batch_returns_202_and_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);
        let session_id = SessionId::new().to_string();

        let body = request_body(
            "acme",
            "app",
            &session_id,
            "hermes",
            vec![
                sample_event("evt-1", "hello"),
                sample_event("evt-2", "world"),
            ],
        );
        let first = ingest_batch(State(state.clone()), None, Json(body.clone())).await;
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let bytes = to_bytes(first.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["accepted"], 2);

        // Same session, same factory-defined events → could be a re-drain.
        let second = ingest_batch(State(state), None, Json(body)).await;
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        let bytes = to_bytes(second.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["accepted"], 0, "re-drained events must be no-ops");
    }

    #[tokio::test]
    async fn idempotency_yields_identical_row_count_across_two_identical_batches() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);
        let session_id = SessionId::new().to_string();
        let body = request_body(
            "acme",
            "app",
            &session_id,
            "hermes",
            vec![sample_event("evt-1", "one"), sample_event("evt-2", "two")],
        );

        for _ in 0..2 {
            let resp = ingest_batch(State(state.clone()), None, Json(body.clone())).await;
            assert_eq!(resp.status(), StatusCode::ACCEPTED);
        }
        let events = store
            .reader
            .session_events(SessionId::from_str(&session_id).unwrap(), 100)
            .await
            .unwrap();
        assert_eq!(
            events.len(),
            2,
            "two identical drains must not duplicate rows"
        );
    }

    #[tokio::test]
    async fn malformed_batch_is_400() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);

        // Empty events → 400.
        let empty = request_body(
            "acme",
            "app",
            &SessionId::new().to_string(),
            "hermes",
            vec![],
        );
        let resp = ingest_batch(State(state.clone()), None, Json(empty)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Bad session id → 400.
        let bad_sid = request_body(
            "acme",
            "app",
            "not-a-uuid",
            "hermes",
            vec![sample_event("e", "x")],
        );
        let resp = ingest_batch(State(state.clone()), None, Json(bad_sid)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Empty workspace → 400.
        let bad_ws = request_body(
            "",
            "app",
            &SessionId::new().to_string(),
            "hermes",
            vec![sample_event("e", "x")],
        );
        let resp = ingest_batch(State(state), None, Json(bad_ws)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn untrusted_content_is_scrubbed() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);
        let session_id = SessionId::new().to_string();
        // A bare API key pattern the default sanitizer redacts (`sk-…{16,}`).
        let body = request_body(
            "acme",
            "app",
            &session_id,
            "hermes",
            vec![sample_event("evt-1", "sk-abcdef1234567890abcdef")],
        );
        let resp = ingest_batch(State(state.clone()), None, Json(body)).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let events = store
            .reader
            .session_events(SessionId::from_str(&session_id).unwrap(), 100)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            !events[0].content.contains("sk-abcdef1234567890abcdef"),
            "content must be scrubbed before persistence: {}",
            events[0].content
        );
    }

    #[tokio::test]
    async fn duplicate_event_ids_within_one_batch_are_rejected() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);
        let body = request_body(
            "acme",
            "app",
            &SessionId::new().to_string(),
            "hermes",
            vec![sample_event("evt-1", "a"), sample_event("evt-1", "b")],
        );
        let resp = ingest_batch(State(state), None, Json(body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // --- session cards -------------------------------------------------

    fn event(
        id: &str,
        kind: WorkstreamEventKind,
        role: &str,
        content: &str,
        meta: serde_json::Value,
    ) -> NewSessionEvent {
        NewSessionEvent {
            event_id: id.to_string(),
            kind,
            role: Some(role.into()),
            content: content.to_string(),
            occurred_at: None,
            source: sessionmunch_core::SessionSource::Import,
            metadata: meta,
        }
    }

    fn work_session() -> Vec<NewSessionEvent> {
        vec![
            event(
                "u1",
                WorkstreamEventKind::Message,
                "user",
                "Fix the upload bug",
                serde_json::json!({}),
            ),
            event(
                "c1",
                WorkstreamEventKind::ToolCall,
                "assistant",
                "Bash: {\"command\":\"cargo t -p uploader\"}",
                serde_json::json!({"tool": "Bash"}),
            ),
            event(
                "r1",
                WorkstreamEventKind::ToolResult,
                "tool",
                "Summary 3 passed",
                serde_json::json!({"is_error": false}),
            ),
            event(
                "a1",
                WorkstreamEventKind::Message,
                "assistant",
                "Fixed: uploads retry now.",
                serde_json::json!({}),
            ),
        ]
    }

    fn card_state(store: &Store, temp: &TempDir) -> SessionEventState {
        SessionEventState {
            wiki: Some(sessionmunch_wiki::Wiki::new(temp.path(), store.writer.clone()).unwrap()),
            ..test_state(store)
        }
    }

    async fn session_page(
        store: &Store,
        session_id: SessionId,
    ) -> Option<sessionmunch_store::StoredPageBody> {
        let (ws, proj, _) = store.reader.find_session_scope(session_id).await.unwrap()?;
        store
            .reader
            .page_body_by_ids(ws, proj, &format!("sessions/{session_id}.md"))
            .await
            .unwrap()
    }

    fn frontmatter(page: &sessionmunch_store::StoredPageBody) -> serde_json::Value {
        serde_json::from_str(&page.frontmatter_json).unwrap()
    }

    #[tokio::test]
    async fn an_ingested_batch_writes_the_session_card() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = card_state(&store, &temp);
        let session_id = SessionId::new();
        let body = request_body(
            "acme",
            "app",
            &session_id.to_string(),
            "claude-code",
            work_session(),
        );
        let response = ingest_batch(State(state), None, Json(body)).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let page = session_page(&store, session_id).await.expect("card page");
        let fm = frontmatter(&page);
        assert_eq!(fm["generator"], "session-card");
        assert_eq!(
            fm["card"]["builder_version"],
            sessionmunch_workstream::CARD_BUILDER_VERSION
        );
        assert_eq!(fm["card"]["events"], 4);
        assert_eq!(
            fm["card"]["last_sequence"], 4,
            "refs are ledger sequence numbers"
        );
        assert_eq!(page.title, "Fix the upload bug");
        assert!(
            page.body
                .contains("**Final assistant message (verbatim, e4):**")
        );
        assert!(page.body.contains("> Fixed: uploads retry now."));
        assert!(page.body.contains("- Fix the upload bug (e1)"));
        assert!(
            page.body
                .contains("test passed: `cargo t -p uploader` (e2, e3)")
        );
    }

    #[tokio::test]
    async fn a_later_batch_refreshes_the_card_and_keeps_stable_refs() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = card_state(&store, &temp);
        let session_id = SessionId::new();
        let sid = session_id.to_string();
        let mut events = work_session();
        let later = events.split_off(2);
        ingest_batch(
            State(state.clone()),
            None,
            Json(request_body("acme", "app", &sid, "codex", events)),
        )
        .await;
        let first = session_page(&store, session_id).await.expect("card");
        assert_eq!(frontmatter(&first)["card"]["last_sequence"], 2);
        ingest_batch(
            State(state),
            None,
            Json(request_body("acme", "app", &sid, "codex", later)),
        )
        .await;
        let second = session_page(&store, session_id).await.expect("card");
        assert_eq!(frontmatter(&second)["card"]["last_sequence"], 4);
        assert!(second.body.contains("(verbatim, e4)"));
        assert!(
            second.body.contains("- Fix the upload bug (e1)"),
            "earlier refs do not move"
        );
    }

    #[tokio::test]
    async fn ingest_never_replaces_a_consolidated_or_annotated_page() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = card_state(&store, &temp);
        let wiki = state.wiki.clone().unwrap();
        let session_id = SessionId::new();
        let sid = session_id.to_string();
        let mut events = work_session();
        let later = events.split_off(2);
        ingest_batch(
            State(state.clone()),
            None,
            Json(request_body("acme", "app", &sid, "claude", events)),
        )
        .await;
        let (ws, proj, _) = store
            .reader
            .find_session_scope(session_id)
            .await
            .unwrap()
            .unwrap();
        wiki.write_page(sessionmunch_wiki::WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: sessionmunch_core::PagePath::new(format!("sessions/{sid}.md")).unwrap(),
            frontmatter: serde_json::json!({"title": "Upload fix", "tier": "episodic", "consolidated": true}),
            body: "A consolidated narrative.".into(),
            tier: sessionmunch_core::Tier::Episodic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: sessionmunch_core::ActorContext::default(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
        ingest_batch(
            State(state),
            None,
            Json(request_body("acme", "app", &sid, "claude", later)),
        )
        .await;
        let page = session_page(&store, session_id).await.unwrap();
        assert_eq!(page.body.trim(), "A consolidated narrative.");
    }

    #[tokio::test]
    async fn stale_cards_are_refreshed_in_bounded_batches_during_import() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        // Ledger rows land without cards (no wiki), as for data that predates them.
        let plain = test_state(&store);
        let total = crate::session_card::STALE_CARDS_PER_BATCH + 2;
        let mut stale = Vec::new();
        for _ in 0..total {
            let id = SessionId::new();
            ingest_batch(
                State(plain.clone()),
                None,
                Json(request_body(
                    "acme",
                    "app",
                    &id.to_string(),
                    "codex",
                    work_session(),
                )),
            )
            .await;
            stale.push(id);
        }
        let cards = card_state(&store, &temp);
        let trigger = SessionId::new();
        ingest_batch(
            State(cards.clone()),
            None,
            Json(request_body(
                "acme",
                "app",
                &trigger.to_string(),
                "codex",
                work_session(),
            )),
        )
        .await;
        assert!(
            session_page(&store, trigger).await.is_some(),
            "the batch's own session first"
        );
        let mut refreshed = 0;
        for id in &stale {
            refreshed += usize::from(session_page(&store, *id).await.is_some());
        }
        assert_eq!(
            refreshed,
            crate::session_card::STALE_CARDS_PER_BATCH,
            "one bounded batch"
        );
        let again = SessionId::new();
        ingest_batch(
            State(cards),
            None,
            Json(request_body(
                "acme",
                "app",
                &again.to_string(),
                "codex",
                work_session(),
            )),
        )
        .await;
        for id in &stale {
            assert!(
                session_page(&store, *id).await.is_some(),
                "the next batch finishes the backlog"
            );
        }
    }
}
