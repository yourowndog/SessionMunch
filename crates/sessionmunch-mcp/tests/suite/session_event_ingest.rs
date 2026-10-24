//! Router-level integration tests for the authenticated session-event batch
//! ingest endpoint (North Star P1.1).
//!
//! The endpoint lives in `sessionmunch-hooks::session_events_router` and is
//! mounted in `serve.rs` under the same `require_bearer` machine middleware as
//! `/hook` and `/workstream/*`. These tests assemble that exact wiring — the
//! hooks router behind `require_bearer` with a configured token — so they pin
//! the real wire contract:
//!
//! - no / bad bearer             → 401
//! - malformed or empty batch    → 400
//! - valid batch                 → 202 Accepted
//! - identical batch twice       → same row count, zero duplication

use std::str::FromStr as _;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sessionmunch_core::SessionId;
use sessionmunch_hooks::{SessionEventState, session_events_router};
use sessionmunch_mcp::auth::{AuthState, require_bearer};
use sessionmunch_store::Store;
use tempfile::TempDir;
use tower::ServiceExt;

/// Assemble the exact machine-router wiring from `serve.rs`: hooks router
/// behind `require_bearer` with a bearer token configured.
fn make_router(store: &Store, token: Option<&str>) -> Router {
    let state = SessionEventState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        sanitizer: sessionmunch_core::Sanitizer::default(),
    };
    let auth = Arc::new(AuthState::new(token.map(str::to_string)));
    Router::new()
        .merge(session_events_router(state))
        .layer(axum::middleware::from_fn_with_state(auth, require_bearer))
}

fn valid_body(session_id: &str) -> serde_json::Value {
    serde_json::json!({
        "workspace": "acme",
        "project": "app",
        "session_id": session_id,
        "harness": "hermes",
        "events": [
            { "event_id": "evt-1", "kind": "message", "role": "user", "content": "hello" },
            { "event_id": "evt-2", "kind": "tool_call", "content": "ls -la" }
        ]
    })
}

async fn post(
    router: &Router,
    token: Option<&str>,
    body: &serde_json::Value,
) -> axum::response::Response {
    let mut builder = Request::builder().method("POST").uri("/session-events");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let req = builder
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    router.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn unauthenticated_batch_is_401() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let router = make_router(&store, Some("secret"));

    // Missing bearer → 401.
    let resp = post(&router, None, &valid_body(&SessionId::new().to_string())).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong bearer → 401.
    let resp = post(
        &router,
        Some("wrong"),
        &valid_body(&SessionId::new().to_string()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn valid_batch_is_accepted_and_idempotent_via_http() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let router = make_router(&store, Some("secret"));
    let session_id = SessionId::new().to_string();

    for _ in 0..2 {
        let resp = post(&router, Some("secret"), &valid_body(&session_id)).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    let rows = store
        .reader
        .session_events(SessionId::from_str(&session_id).unwrap(), 100)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "re-ign same batch must not duplicate session_events rows"
    );
}

#[tokio::test]
async fn malformed_batch_is_400_via_http() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let router = make_router(&store, Some("secret"));

    // Malformed JSON body → 400 from the extractor.
    let req = Request::builder()
        .method("POST")
        .uri("/session-events")
        .header(header::AUTHORIZATION, "Bearer secret")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Valid JSON but schema-invalid (empty events) → 400.
    let resp = post(
        &router,
        Some("secret"),
        &serde_json::json!({
            "workspace": "acme",
            "project": "app",
            "session_id": SessionId::new().to_string(),
            "harness": "hermes",
            "events": []
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn zero_config_loopback_allows_batches_without_a_token() {
    // With no authority configured (`AuthState::new(None)`) the wire gate is
    // disabled — the default single-user loopback keeps working with no token.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let router = make_router(&store, None);
    let resp = post(&router, None, &valid_body(&SessionId::new().to_string())).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}
