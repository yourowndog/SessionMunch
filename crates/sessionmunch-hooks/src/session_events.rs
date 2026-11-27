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

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sessionmunch_core::{
    ActorContext, AgentKind, AuthLevel, Capability, NewSession, NewSessionEvent, OwnerFilter,
    Sanitizer, SessionId, SessionProvenance,
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
}

/// Build the batch-ingest router. Mounted beside `/hook` and `/workstream/*`,
/// so it receives the same bearer-auth middleware as all machine ingress.
pub fn session_events_router(state: SessionEventState) -> Router {
    Router::new()
        .route("/session-events", post(ingest_batch))
        .route(
            "/session-events/{session_id}/visible-turns",
            get(visible_turns),
        )
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
    (
        StatusCode::ACCEPTED,
        Json(IngestSessionEventsResponse {
            session_id,
            accepted,
        }),
    )
        .into_response()
}

/// Query for `GET /session-events/{session_id}/visible-turns`.
#[derive(Debug, Clone, Deserialize)]
pub struct VisibleTurnsQuery {
    /// Workspace the caller is enrolled for; must own the session.
    pub workspace: String,
    /// Project the caller is enrolled for; must own the session.
    pub project: String,
    /// Cursor sequence from a previous page's `scanned_through`.
    #[serde(default)]
    pub after_seq: Option<i64>,
    /// Cursor anchor from a previous page's `scanned_event_id`.
    #[serde(default)]
    pub after_event_id: Option<String>,
    /// Maximum ledger rows to scan (capped at the store maximum).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /session-events/{session_id}/visible-turns`
///
/// Lossless, bounded, ascending replay of one enrolled session's finalized
/// visible turns after a `(sequence, event_id)` cursor. Scope and owner are
/// enforced server-side; a session outside the named scope is reported as not
/// found. A cursor whose anchor no longer matches returns `status: reset` and
/// no turns, so consumers stop instead of skipping. Never returns tool
/// payloads, hook mirrors or raw metadata.
async fn visible_turns(
    State(state): State<SessionEventState>,
    Path(session_id): Path<String>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ActorContext>>,
    Query(query): Query<VisibleTurnsQuery>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalRead) {
        return response.into_response();
    }
    let Ok(session_id) = SessionId::from_str(session_id.trim()) else {
        return error(StatusCode::BAD_REQUEST, "invalid session_id");
    };
    let after = match (query.after_seq, query.after_event_id) {
        (None, None) => None,
        (Some(seq), Some(id)) if seq >= 1 && !id.is_empty() && id.len() <= MAX_EVENT_ID_BYTES => {
            Some((seq, id))
        }
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "after_seq and after_event_id must be supplied together",
            );
        }
    };
    let not_found = || error(StatusCode::NOT_FOUND, "session not visible in this scope");
    let workspace = match state
        .reader
        .find_workspace(query.workspace.trim().to_owned())
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => return not_found(),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    let project = match state
        .reader
        .find_project(workspace, query.project.trim().to_owned())
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => return not_found(),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    let owner = actor.map_or(OwnerFilter::Unattributed, |Extension(actor)| {
        OwnerFilter::for_actor_context(&actor)
    });
    let limit = query
        .limit
        .unwrap_or(sessionmunch_store::MAX_VISIBLE_TURN_SCAN);
    match state
        .reader
        .visible_turns_after(session_id, workspace, project, owner, after, limit)
        .await
    {
        Ok(Some(page)) => (StatusCode::OK, Json(page)).into_response(),
        Ok(None) => not_found(),
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    }
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

    fn visible_event(
        id: &str,
        kind: WorkstreamEventKind,
        role: &str,
        text: &str,
    ) -> NewSessionEvent {
        NewSessionEvent {
            event_id: id.to_string(),
            kind,
            role: Some(role.into()),
            content: text.to_string(),
            occurred_at: None,
            source: sessionmunch_core::SessionSource::Import,
            metadata: serde_json::json!({"capture": "observed", "tool_input": "never returned"}),
        }
    }

    async fn page_json(
        state: &SessionEventState,
        session_id: &str,
        query: VisibleTurnsQuery,
    ) -> (StatusCode, serde_json::Value) {
        let response = visible_turns(
            State(state.clone()),
            Path(session_id.to_string()),
            None,
            None,
            Query(query),
        )
        .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn query(workspace: &str, project: &str, after: Option<(i64, &str)>) -> VisibleTurnsQuery {
        VisibleTurnsQuery {
            workspace: workspace.into(),
            project: project.into(),
            after_seq: after.map(|a| a.0),
            after_event_id: after.map(|a| a.1.to_string()),
            limit: Some(2),
        }
    }

    /// An owned session is invisible to another authenticated owner and to an
    /// unattributed caller: the same 404 as a missing session, no leak.
    #[tokio::test]
    async fn visible_turns_hide_another_owners_session() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);
        let session_id = SessionId::new().to_string();
        let mut body = request_body(
            "private-history",
            "hosted",
            &session_id,
            "claude-chat",
            vec![visible_event(
                "hosted:a:1",
                WorkstreamEventKind::Message,
                "user",
                "q1",
            )],
        );
        body.actor_user = Some("user:alice".into());
        let ingest = ingest_batch(State(state.clone()), None, Json(body)).await;
        assert_eq!(ingest.status(), StatusCode::ACCEPTED);
        let call = |user: Option<&str>| {
            let actor = user.map(|u| {
                Extension(ActorContext {
                    user: Some(u.to_string()),
                    ..ActorContext::default()
                })
            });
            visible_turns(
                State(state.clone()),
                Path(session_id.clone()),
                None,
                actor,
                Query(query("private-history", "hosted", None)),
            )
        };
        assert_eq!(call(Some("alice")).await.status(), StatusCode::OK);
        let foreign = call(Some("bob")).await;
        assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
        let foreign_body = to_bytes(foreign.into_body(), 4096).await.unwrap();
        let missing = visible_turns(
            State(state.clone()),
            Path(SessionId::new().to_string()),
            None,
            Some(Extension(ActorContext {
                user: Some("bob".into()),
                ..ActorContext::default()
            })),
            Query(query("private-history", "hosted", None)),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let missing_body = to_bytes(missing.into_body(), 4096).await.unwrap();
        assert_eq!(
            foreign_body, missing_body,
            "foreign owner must look like a missing session"
        );
        assert_eq!(call(None).await.status(), StatusCode::NOT_FOUND);
    }

    /// Ascending scoped pages: ineligible rows advance the scan watermark,
    /// replays of the same cursor are identical, a foreign scope is 404, a
    /// half cursor is 400 and a mismatched anchor resets instead of skipping.
    #[tokio::test]
    async fn visible_turns_page_ascending_with_scope_and_anchor_checks() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);
        let session_id = SessionId::new().to_string();
        let mut hook = sample_event("hook-1", "hook mirror text");
        hook.role = Some("assistant".into());
        let body = request_body(
            "private-history",
            "hosted",
            &session_id,
            "claude-chat",
            vec![
                visible_event("hosted:a:1", WorkstreamEventKind::Message, "user", "q1"),
                visible_event("tool-1", WorkstreamEventKind::ToolCall, "assistant", "tool"),
                hook,
                visible_event(
                    "hosted:b:1",
                    WorkstreamEventKind::Message,
                    "assistant",
                    "a1",
                ),
                visible_event("hosted:c:1", WorkstreamEventKind::Message, "user", "q2"),
            ],
        );
        let ingest = ingest_batch(State(state.clone()), None, Json(body)).await;
        assert_eq!(ingest.status(), StatusCode::ACCEPTED);

        let (status, first) = page_json(
            &state,
            &session_id,
            query("private-history", "hosted", None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["status"], "ok");
        assert_eq!(first["turns"].as_array().unwrap().len(), 1, "{first}");
        assert_eq!(first["turns"][0]["event_id"], "hosted:a:1");
        assert_eq!(first["turns"][0]["capture"], "observed");
        assert!(first["turns"][0].get("metadata").is_none());
        assert_eq!(first["scanned_through"], 2);
        assert_eq!(first["high_water"], 5);
        assert_eq!(first["complete"], false);
        let anchor = first["scanned_event_id"].as_str().unwrap().to_string();
        let mut next = query("private-history", "hosted", Some((2, &anchor)));
        next.limit = Some(10);
        let (_, second) = page_json(&state, &session_id, next.clone()).await;
        let ids: Vec<_> = second["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["event_id"].clone())
            .collect();
        assert_eq!(
            ids,
            vec!["hosted:b:1", "hosted:c:1"],
            "hook mirror skipped: {second}"
        );
        assert_eq!(second["scanned_through"], 5);
        assert_eq!(second["complete"], true);
        let (_, replay) = page_json(&state, &session_id, next).await;
        assert_eq!(replay, second, "same cursor must replay identically");
        let text = second.to_string();
        assert!(!text.contains("never returned") && !text.contains("hook mirror"));

        let (status, _) =
            page_json(&state, &session_id, query("private-history", "other", None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = page_json(&state, &session_id, query("nope", "hosted", None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = page_json(
            &state,
            &SessionId::new().to_string(),
            query("private-history", "hosted", None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let mut half = query("private-history", "hosted", None);
        half.after_seq = Some(2);
        let (status, _) = page_json(&state, &session_id, half).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, reset) = page_json(
            &state,
            &session_id,
            query("private-history", "hosted", Some((2, "someone-else"))),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reset["status"], "reset");
        assert_eq!(reset["turns"].as_array().unwrap().len(), 0);
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

    /// Ingest stores events and nothing else: the card for an imported
    /// session is the maintenance tick's job, never the request path's.
    #[tokio::test]
    async fn ingest_writes_no_session_page() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store);
        let session_id = SessionId::new();
        let mut events = vec![sample_event("evt-1", "Fix the upload bug")];
        events[0].source = sessionmunch_core::SessionSource::Import;
        let body = request_body("acme", "app", &session_id.to_string(), "codex", events);
        let response = ingest_batch(State(state), None, Json(body)).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let (ws, proj, _) = store
            .reader
            .find_session_scope(session_id)
            .await
            .unwrap()
            .unwrap();
        let page = store
            .reader
            .page_body_by_ids(ws, proj, &format!("sessions/{session_id}.md"))
            .await
            .unwrap();
        assert!(page.is_none());
        let due = store
            .reader
            .session_cards_due(
                sessionmunch_workstream::CARD_BUILDER_VERSION,
                Some(session_id),
                None,
                1,
            )
            .await
            .unwrap();
        assert_eq!(
            due,
            vec![session_id],
            "the session is left due for the tick"
        );
    }
}
