use super::*;
use axum::body::to_bytes;
use axum::http::Request;
use sessionmunch_core::{
    ActorContext, AgentKind, IdentityKey, NewObservation, NewSession, ObservationKind,
};
use sessionmunch_core::{Sanitized, Sanitizer};
use sessionmunch_llm::{ChatRequest, ChatResponse, LlmResult};
use sessionmunch_store::Store;
use tempfile::TempDir;
use tower::ServiceExt;

struct FakeAutoImproveLlm;

#[async_trait::async_trait]
impl LlmProvider for FakeAutoImproveLlm {
    fn name(&self) -> &'static str {
        "fake-auto-improve"
    }

    fn model(&self) -> &str {
        "fake-model"
    }

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            text: String::new(),
            usage: None,
            model: self.model().to_string(),
        })
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        Ok(serde_json::json!({
            "summary": "two staged proposals",
            "proposals": [
                {
                    "operation": "create_or_update",
                    "path": "_slots/current-focus.md",
                    "title": "Current Focus",
                    "kind": "slot",
                    "confidence": 0.95,
                    "rationale": "The current focus should be updated from the session.",
                    "evidence": [{"page":"session", "quote":"focus changed"}],
                    "body_markdown": "# Current Focus\n\nupdated focus proposal"
                },
                {
                    "operation": "create_or_update",
                    "path": "notes/new-auto-improve.md",
                    "title": "New Auto Improve Lesson",
                    "kind": "note",
                    "confidence": 0.93,
                    "rationale": "The session contains a durable lesson worth adding.",
                    "evidence": [{"page":"session", "quote":"durable lesson"}],
                    "body_markdown": "# New Auto Improve Lesson\n\nnew page proposal"
                }
            ],
            "rejected_candidates": []
        }))
    }
}

#[tokio::test]
async fn status_reports_provider_health_block() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["providers"]["llm"]["status"], "disabled");
    assert_eq!(json["providers"]["embedding"]["status"], "disabled");
    // 2.0 item 7: the report carries the wedged-writer gauge and the
    // wiki-format state — a fresh store is natively conformant (no
    // migration ran) with no backup archive.
    assert!(json["write_queue"].is_array(), "{json}");
    assert_eq!(json["wiki_format"]["okf_migrated"], false);
    assert!(json["wiki_format"]["backup_archive"].is_null());
}

/// The MCP-only complement: per-client tool-call counters served
/// back with the same window semantics as by-agent.
#[tokio::test]
async fn activity_by_client_aggregates_and_bounds_the_window() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let today = jiff::Timestamp::now()
        .as_microsecond()
        .div_euclid(US_PER_DAY);
    store
        .writer
        .bump_client_activity(vec![
            ("vscode".into(), today, 4, 1),
            ("vscode".into(), today - 40, 7, 0),
            ("claude-desktop".into(), today, 1, 0),
        ])
        .await
        .unwrap();

    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49376".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    // Whole history: the 40-day-old bucket counts.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/activity/by-client")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["by_client"],
        serde_json::json!([
            { "client": "vscode", "reads": 11, "writes": 1 },
            { "client": "claude-desktop", "reads": 1, "writes": 0 },
        ]),
        "volume-desc across all history: {json}"
    );

    // A 7-day window drops the old bucket.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/activity/by-client?since_days=7")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["by_client"][0],
        serde_json::json!({ "client": "vscode", "reads": 4, "writes": 1 }),
        "{json}"
    );
}

/// The dashboard read: session counts grouped per agent CLI, with the
/// scope failing closed and never auto-created.
#[tokio::test]
async fn sessions_by_agent_counts_per_agent_and_fails_closed_on_unknown_scope() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let target = store
        .writer
        .get_or_create_project(ws, "target".to_string(), None)
        .await
        .unwrap();
    let other_project = store
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();
    for (project_id, agent) in [
        (target, AgentKind::ClaudeCode),
        (target, AgentKind::ClaudeCode),
        (target, AgentKind::Cursor),
        // A different scope must not leak into the target's totals.
        (other_project, AgentKind::Codex),
    ] {
        store
            .writer
            .begin_session(NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id,
                agent_kind: agent,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
    }
    for (agent, owner) in [(AgentKind::ClaudeCode, "alice"), (AgentKind::Codex, "bob")] {
        store
            .writer
            .begin_session(NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: target,
                agent_kind: agent,
                cwd: None,
                actor_user: Some(IdentityKey::User(owner.into()).storage_key()),
            })
            .await
            .unwrap();
    }

    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49375".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/sessions/by-agent?workspace=default&project=target")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["by_agent"],
        serde_json::json!([
            { "agent": "claude-code", "sessions": 2 },
            { "agent": "cursor", "sessions": 1 },
        ]),
        "counts are per agent, scoped, count-desc: {json}"
    );

    // A named operator sees their rows plus shared legacy rows, but not a
    // colleague's. The recovery switch deliberately includes all owners.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/sessions/by-agent?workspace=default&project=target")
                .extension(ActorContext {
                    user: Some("alice".into()),
                    ..ActorContext::default()
                })
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["by_agent"],
        serde_json::json!([
            { "agent": "claude-code", "sessions": 3 },
            { "agent": "cursor", "sessions": 1 },
        ]),
        "named callers see own plus shared sessions only: {json}"
    );

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/sessions/by-agent?workspace=default&project=target&all_owners=true")
                .extension(ActorContext {
                    user: Some("alice".into()),
                    ..ActorContext::default()
                })
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["by_agent"],
        serde_json::json!([
            { "agent": "claude-code", "sessions": 3 },
            { "agent": "codex", "sessions": 1 },
            { "agent": "cursor", "sessions": 1 },
        ]),
        "all_owners includes every operator: {json}"
    );

    // Zero is the explicit all-history spelling and must not become an
    // empty time window.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/sessions/by-agent?workspace=default&project=target&since_days=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["by_agent"].as_array().unwrap().len(),
        2,
        "since_days=0 means the whole history, not an empty window: {json}"
    );

    // A window wider than the epoch saturates into "all history" rather
    // than overflowing: `u32::MAX` days times a day of microseconds does
    // not fit in the i64 the cutoff is computed in.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(
                    "/admin/sessions/by-agent\
                         ?workspace=default&project=target&since_days=4294967295",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["by_agent"].as_array().unwrap().len(),
        2,
        "a saturating window counts everything, it does not panic or empty: {json}"
    );

    // Unknown scope fails closed with a 404 — never auto-created.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/sessions/by-agent?workspace=default&project=ghost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        store
            .reader
            .find_project(ws, "ghost".to_string())
            .await
            .unwrap()
            .is_none(),
        "read route must not auto-create scopes"
    );
}

#[tokio::test]
async fn open_sessions_filters_by_scope_and_agent() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let target = store
        .writer
        .get_or_create_project(ws, "target".to_string(), None)
        .await
        .unwrap();
    let other_project = store
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();
    let older = SessionId::new();
    let latest = SessionId::new();
    let other_agent = SessionId::new();
    let other_scope = SessionId::new();
    let ended = SessionId::new();
    let alice = SessionId::new();
    let bob = SessionId::new();
    for (id, project_id, agent) in [
        (older, target, AgentKind::Codex),
        (other_agent, target, AgentKind::ClaudeCode),
        (other_scope, other_project, AgentKind::Codex),
        (ended, target, AgentKind::Codex),
        (latest, target, AgentKind::Codex),
    ] {
        store
            .writer
            .begin_session(NewSession {
                id,
                workspace_id: ws,
                project_id,
                agent_kind: agent,
                cwd: Some(std::path::PathBuf::from("/tmp/target")),
                actor_user: None,
            })
            .await
            .unwrap();
    }
    store.writer.end_session(ended, None).await.unwrap();
    for (id, user) in [(alice, "alice"), (bob, "bob")] {
        store
            .writer
            .begin_session(NewSession {
                id,
                workspace_id: ws,
                project_id: target,
                agent_kind: AgentKind::Codex,
                cwd: Some(std::path::PathBuf::from("/tmp/target")),
                actor_user: Some(IdentityKey::User(user.into()).storage_key()),
            })
            .await
            .unwrap();
    }

    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    // Default: only the newest open session for the scope + agent.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/open-sessions?workspace=default&project=target&agent=codex")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sessions = json["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], latest.to_string());
    assert_eq!(sessions[0]["cwd"], "/tmp/target");

    // `all=true`: every open codex session in the scope, ended and
    // other-scope/other-agent sessions excluded.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/open-sessions?workspace=default&project=target&agent=codex&all=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sessions = json["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let ids: Vec<&str> = sessions
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&latest.to_string().as_str()));
    assert!(ids.contains(&older.to_string().as_str()));

    // A named operator gets their own open sessions plus shared legacy
    // sessions, never a colleague's. This is the exact route consumed by
    // the destructive `finalize-session` command.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/open-sessions?workspace=default&project=target&agent=codex&all=true")
                .extension(ActorContext {
                    user: Some("alice".into()),
                    ..ActorContext::default()
                })
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let ids = json["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.contains(&alice.to_string().as_str()));
    assert!(ids.contains(&latest.to_string().as_str()));
    assert!(ids.contains(&older.to_string().as_str()));
    assert!(!ids.contains(&bob.to_string().as_str()));

    // Unknown agent kind fails closed with a 400.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/open-sessions?workspace=default&project=target&agent=nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Unknown scope fails closed with a 404 — never auto-created.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/open-sessions?workspace=default&project=ghost&agent=codex")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        store
            .reader
            .find_project(ws, "ghost".to_string())
            .await
            .unwrap()
            .is_none(),
        "read route must not auto-create scopes"
    );
}

/// Regression for the race a naive wrapper hits with several concurrent
/// Kiro CLI tabs against one repo: two open sessions for the same
/// agent+scope exist (`older`, `latest`), and `?session_id=` must return
/// exactly the one requested — never falling back to "newest open" —
/// while still composing with the owner filter (a session_id belonging
/// to another operator must not be reachable just by knowing its id,
/// same invariant `all_owners` already protects for the default query).
#[tokio::test]
async fn open_sessions_session_id_targets_exact_session() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let target = store
        .writer
        .get_or_create_project(ws, "target".to_string(), None)
        .await
        .unwrap();
    let other_project = store
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();
    let older = SessionId::new();
    let latest = SessionId::new();
    let other_scope = SessionId::new();
    let ended = SessionId::new();
    let bobs_session = SessionId::new();
    let wrong_agent = SessionId::new();
    for (id, project_id, agent_kind, actor_user) in [
        (older, target, AgentKind::KiroCli, None),
        (latest, target, AgentKind::KiroCli, None),
        (other_scope, other_project, AgentKind::KiroCli, None),
        (ended, target, AgentKind::KiroCli, None),
        (wrong_agent, target, AgentKind::Codex, None),
        (
            bobs_session,
            target,
            AgentKind::KiroCli,
            Some(IdentityKey::User("bob".into()).storage_key()),
        ),
    ] {
        store
            .writer
            .begin_session(NewSession {
                id,
                workspace_id: ws,
                project_id,
                agent_kind,
                cwd: Some(std::path::PathBuf::from("/tmp/target")),
                actor_user,
            })
            .await
            .unwrap();
    }
    store.writer.end_session(ended, None).await.unwrap();

    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    // Targeting `older` by id must return only `older`, not `latest`.
    let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={older}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sessions = json["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "must not fall back to newest-open");
    assert_eq!(sessions[0]["session_id"], older.to_string());

    // A session id that belongs to a different project must not be
    // reachable just by knowing the id — scope is still enforced.
    let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={other_scope}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["sessions"].as_array().unwrap().len(),
        0,
        "a session from another project must not be reachable by id"
    );

    // An already-ended session id is not "open" and must not match.
    let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={ended}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["sessions"].as_array().unwrap().len(), 0);

    // An exact id from another agent remains outside the requested
    // agent boundary even when its scope matches.
    let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={wrong_agent}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["sessions"].as_array().unwrap().len(), 0);

    // A colleague's session id is not reachable without all_owners, even
    // when the exact id is known — session_id composes with the owner
    // filter (AND), it doesn't bypass it.
    let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={bobs_session}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["sessions"].as_array().unwrap().len(),
        0,
        "session_id must not bypass the owner filter"
    );

    // ...but composes correctly WITH all_owners set.
    let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={bobs_session}&all_owners=true"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["sessions"].as_array().unwrap().len(), 1);

    // Selection modes are mutually exclusive instead of silently giving
    // one of them precedence.
    let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&all=true&session_id={older}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A malformed session_id fails closed with a 400, not a silent
    // empty/ignored filter.
    let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id=not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

fn read_page_test_router() -> (TempDir, Router) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });
    (tmp, router)
}

/// The exported tarball is a valid OKF v0.2 bundle: fresh index.md
/// with okf_version, every concept file conformant (docs/okf.md).
#[tokio::test]
async fn export_okf_streams_a_conformant_bundle() {
    let (_tmp, router) = read_page_test_router();
    post_write_page(
        &router,
        "default",
        "scratch",
        "gotchas/build.md",
        "watch out",
    )
    .await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/export-okf?workspace=default&project=scratch")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();

    let dec = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes.to_vec()));
    let mut ar = tar::Archive::new(dec);
    let mut names = Vec::new();
    let mut index_body = String::new();
    let mut page_body = String::new();
    for entry in ar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().display().to_string();
        use std::io::Read as _;
        let mut content = String::new();
        entry.read_to_string(&mut content).unwrap();
        if name == "index.md" {
            index_body = content.clone();
        }
        if name == "gotchas/build.md" {
            page_body = content.clone();
        }
        names.push(name);
    }
    assert!(names.contains(&"index.md".to_string()), "{names:?}");
    assert!(names.contains(&"gotchas/build.md".to_string()), "{names:?}");
    assert!(index_body.contains("okf_version: \"0.2\""));
    let fm = sessionmunch_wiki::parse(&page_body).unwrap().frontmatter;
    assert!(sessionmunch_core::okf::is_conformant(&fm));
    assert_eq!(fm["type"], "Gotcha");
}

/// Post-audit regression: the things a REAL deployment's tree holds
/// beside concept pages — typed scope manifests and _pending
/// proposal sidecars — must not block the export. Sidecars are
/// excluded; manifests ship typed.
#[tokio::test]
async fn export_okf_tolerates_manifests_and_skips_pending() {
    let (tmp, router) = read_page_test_router();
    post_write_page(&router, "default", "scratch", "notes/a.md", "fine").await;
    let ws_dir = std::fs::read_dir(tmp.path().join("wiki"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().is_none_or(|n| n != ".git"))
        .unwrap();
    let proj_dir = std::fs::read_dir(&ws_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    // Typed manifest (what the fixed backfill writes) + a pending
    // sidecar with no frontmatter (what staging writes).
    std::fs::write(
        proj_dir.join("_meta.md"),
        "---\nproject: scratch\ntype: Scope Manifest\n---\n",
    )
    .unwrap();
    std::fs::create_dir_all(proj_dir.join("_pending/auto-improve")).unwrap();
    std::fs::write(
        proj_dir.join("_pending/auto-improve/proposal.md"),
        "# A staged proposal\n\nno frontmatter at all",
    )
    .unwrap();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/export-okf?workspace=default&project=scratch")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let dec = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes.to_vec()));
    let mut ar = tar::Archive::new(dec);
    let names: Vec<String> = ar
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().display().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "_meta.md"), "{names:?}");
    assert!(
        !names.iter().any(|n| n.starts_with("_pending")),
        "pending sidecars must not ship: {names:?}"
    );
}

/// A bundle with a non-conformant page must fail the export rather
/// than ship something a strict reader rejects.
#[tokio::test]
async fn export_okf_refuses_a_nonconformant_page() {
    let (tmp, router) = read_page_test_router();
    post_write_page(&router, "default", "scratch", "notes/a.md", "fine").await;
    // Fabricate a pre-migration file next to it. The wiki root also
    // holds `.git`; read_dir order is arbitrary, so select the
    // UUID-named scope dirs explicitly.
    let uuid_dir = |parent: &std::path::Path| {
        std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_dir() && p.file_name().is_none_or(|n| n != ".git"))
            .expect("scope dir")
    };
    let ws_dir = uuid_dir(&tmp.path().join("wiki"));
    let proj_dir = uuid_dir(&ws_dir);
    std::fs::write(
        proj_dir.join("notes/legacy.md"),
        "---\ntitle: Legacy\n---\nno type here",
    )
    .unwrap();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/export-okf?workspace=default&project=scratch")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

fn admin_state_for_store(tmp: &TempDir, store: &Store, wiki: Wiki) -> AdminState {
    admin_state_for_store_with_llm(tmp, store, wiki, None)
}

fn admin_state_for_store_with_llm(
    tmp: &TempDir,
    store: &Store,
    wiki: Wiki,
    llm: Option<Arc<dyn LlmProvider>>,
) -> AdminState {
    AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    }
}

async fn stage_pending_write(
    store: &Store,
    workspace: &str,
    project: &str,
    path: &str,
    body: &str,
) -> (WorkspaceId, ProjectId, AutoImproveProposalId) {
    let ws = store
        .writer
        .get_or_create_workspace(workspace)
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project, None)
        .await
        .unwrap();
    let staged = store
        .writer
        .stage_auto_improve_run(StageAutoImproveRun {
            workspace_id: ws,
            project_id: proj,
            session_id: None,
            provider: Some("test".into()),
            model: Some("model".into()),
            summary: Some("summary".into()),
            warnings_json: serde_json::json!([]),
            rejected_candidates_json: serde_json::json!([]),
            config_json: serde_json::json!({"mode":"stage"}),
            proposal_actor: sessionmunch_core::ActorContext {
                agent: Some("auto_improve".into()),
                ..sessionmunch_core::ActorContext::default()
            },
            proposals: vec![NewAutoImproveProposal {
                operation: AutoImproveProposalOperation::Create,
                target_path: PagePath::new(path).unwrap(),
                kind: "note".into(),
                title: "Pending title".into(),
                confidence: 0.9,
                rationale: "rationale".into(),
                evidence_json: serde_json::json!([{"source":"test"}]),
                body_markdown: body.into(),
                artifact_sha256: None,
                edit_mode: None,
                patch_json: None,
                expected_base_body_sha256: None,
            }],
        })
        .await
        .unwrap();
    (ws, proj, staged.proposal_ids[0])
}

async fn post_write_page(router: &Router, ws: &str, project: &str, path: &str, body: &str) {
    post_write_page_with_actor(router, ws, project, path, body, None).await;
}

async fn post_write_page_with_actor(
    router: &Router,
    ws: &str,
    project: &str,
    path: &str,
    body: &str,
    actor: Option<&'static str>,
) {
    let req_body = serde_json::json!({
        "workspace": ws,
        "project": project,
        "path": path,
        "body": body,
        "title": "Read-back fixture",
    });
    let mut builder = Request::builder()
        .method("POST")
        .uri("/admin/write-page")
        .header("content-type", "application/json");
    if let Some(user) = actor {
        builder = builder.extension(sessionmunch_core::ActorContext {
            user: Some(user.into()),
            ..sessionmunch_core::ActorContext::default()
        });
    }
    let resp = router
        .clone()
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "write-page setup failed");
}

#[tokio::test]
async fn read_page_path_mode_returns_full_body() {
    let (_tmp, router) = read_page_test_router();
    post_write_page(&router, "default", "audit", "notes/foo.md", "hello body").await;

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/read-page?workspace=default&project=audit&path=notes/foo.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["path"], "notes/foo.md");
    assert_eq!(json["title"], "Read-back fixture");
    assert!(
        json["body"]
            .as_str()
            .unwrap_or_default()
            .contains("hello body"),
        "body must round-trip; got {:?}",
        json["body"]
    );
    assert_eq!(json["workspace"], "default");
    assert_eq!(json["project"], "audit");
}

#[tokio::test]
async fn auto_improve_requires_llm_provider() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "audit", None)
        .await
        .unwrap();
    let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "audit",
                        "session_id": "00000000-0000-0000-0000-000000000000"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("LLM provider not configured")
    );
}

#[tokio::test]
async fn auto_improve_missing_scope_does_not_fall_back_before_llm_check() {
    let (_tmp, router) = read_page_test_router();

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "missing",
                        "project": "missing",
                        "session_id": "00000000-0000-0000-0000-000000000000"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_contamination_partial_scope_fails_closed() {
    let (_tmp, router) = read_page_test_router();

    for uri in [
        "/admin/audit-contamination?workspace=default",
        "/admin/audit-contamination?project=sessionmunch",
    ] {
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("workspace and project must be provided together")
        );
    }
}

#[tokio::test]
async fn auto_improve_auto_approves_by_default_and_uses_concrete_operations() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("_slots/current-focus.md").unwrap(),
        frontmatter: serde_json::json!({"kind":"slot"}),
        body: "# Current Focus\n\nold focus".into(),
        tier: Tier::Working,
        pinned: false,
        title: Some("Current Focus".into()),
        admission_ctx: None,
        author_id: None,
        actor: sessionmunch_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Other,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "focus changed and durable lesson".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let router = admin_router(admin_state_for_store_with_llm(
        &tmp,
        &store,
        wiki,
        Some(Arc::new(FakeAutoImproveLlm)),
    ));
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch",
                        "session_id": session_id.to_string(),
                        "min_observations": 1,
                        "min_session_duration_secs": 0,
                        "min_confidence": 0.75,
                        "max_proposals_per_run": 5
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["approval_required"], false);
    assert_eq!(json["approval_policy"], "auto_approve");
    let proposals = json["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2);
    for proposal in proposals {
        assert_eq!(proposal["status"], "approved");
        assert!(proposal["page_id"].as_str().is_some());
        let path = proposal["sidecar_path"].as_str().unwrap();
        assert!(
            std::path::Path::new(path).exists(),
            "sidecar file must be written: {path}"
        );
    }

    let staged = store
        .reader
        .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Approved), 10)
        .await
        .unwrap();
    assert_eq!(staged.len(), 2, "DB proposal rows must be approved");
    let slot = staged
        .iter()
        .find(|p| p.target_path.as_str() == "_slots/current-focus.md")
        .expect("slot proposal staged");
    let note = staged
        .iter()
        .find(|p| p.target_path.as_str() == "notes/new-auto-improve.md")
        .expect("note proposal staged");
    assert_eq!(slot.operation, AutoImproveProposalOperation::Update);
    assert_eq!(note.operation, AutoImproveProposalOperation::Create);

    let existing = store
        .reader
        .page_body_by_ids(ws, proj, "_slots/current-focus.md")
        .await
        .unwrap()
        .expect("existing target remains present");
    assert_eq!(existing.body, "# Current Focus\n\nupdated focus proposal");
    let note_page = store
        .reader
        .page_body_by_ids(ws, proj, "notes/new-auto-improve.md")
        .await
        .unwrap()
        .expect("auto-approve writes absent target page");
    assert_eq!(
        note_page.body,
        "# New Auto Improve Lesson\n\nnew page proposal"
    );
}

/// The V42 staging bucket is keyed on
/// [`sessionmunch_core::ActorContext::identity_key`] through
/// [`sessionmunch_core::owner_identity`], not on `actor.user`, so an operator
/// identified by a complete OIDC issuer/subject pair lands in an
/// issuer-qualified bucket rather than the unattributed one. The
/// `memory_auto_improve` MCP tool computes the same bucket, and the V42
/// one-pending-per-target promise is what breaks when it does.
#[tokio::test]
async fn auto_improve_buckets_an_oidc_operator_by_qualified_identity() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("_slots/current-focus.md").unwrap(),
        frontmatter: serde_json::json!({"kind":"slot"}),
        body: "# Current Focus\n\nold focus".into(),
        tier: Tier::Working,
        pinned: false,
        title: Some("Current Focus".into()),
        admission_ctx: None,
        author_id: None,
        actor: sessionmunch_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Other,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "focus changed and durable lesson".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    // A trusted proxy is configured, so the deployment distinguishes its
    // operators even with no `users` rows — which is exactly the case where
    // reaching for `actor.user` sees nobody.
    let mut state =
        admin_state_for_store_with_llm(&tmp, &store, wiki, Some(Arc::new(FakeAutoImproveLlm)));
    state.trusted_proxy_identity = true;
    let router = admin_router(state);

    let mut req = Request::builder()
        .method("POST")
        .uri("/admin/auto-improve")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "workspace": "default",
                "project": "scratch",
                "session_id": session_id.to_string(),
                "min_observations": 1,
                "min_session_duration_secs": 0,
                "min_confidence": 0.75,
                "max_proposals_per_run": 5
            }))
            .unwrap(),
        ))
        .unwrap();
    req.extensions_mut()
        .insert(sessionmunch_core::ActorContext {
            issuer: Some("https://issuer.example".into()),
            sub: Some("subject-42".into()),
            ..sessionmunch_core::ActorContext::anonymous()
        });
    req.extensions_mut()
        .insert(sessionmunch_core::AuthLevel::Root);

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The bucket the contract stamps, built through the API rather than
    // hand-written, so this test follows the encoding if it ever moves.
    let expected_bucket = sessionmunch_core::IdentityKey::Subject {
        issuer: "https://issuer.example".into(),
        subject: "subject-42".into(),
    }
    .storage_key();
    let staged = store
        .reader
        .list_auto_improve_proposals(ws, proj, None, 10)
        .await
        .unwrap();
    assert!(!staged.is_empty(), "the fake LLM proposes two pages");
    for proposal in &staged {
        let detail = store
            .reader
            .auto_improve_proposal_detail_with_owner(ws, proj, proposal.id)
            .await
            .unwrap()
            .expect("staged proposal readable in its own scope");
        assert_eq!(
            detail.staged_by_actor_user.as_deref(),
            Some(expected_bucket.as_str()),
            "the subject claim is the operator, not `None`: {}",
            detail.detail.summary.target_path.as_str()
        );
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["staged_by_actor_user"], expected_bucket);
        assert!(json.get("summary").is_some(), "detail fields stay flat");
        assert!(
            json.get("detail").is_none(),
            "no nested compatibility wrapper"
        );
    }
}

/// The admin report must name the proposals the store refused to stage.
/// Returning only the staged ids hands the operator a successful run of
/// N-1 proposals with no trace of the Nth — the silent drop the
/// per-proposal skip was introduced to end.
#[tokio::test]
async fn auto_improve_report_names_the_proposal_a_collision_skipped() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let (ws, proj, _pending) = stage_pending_write(
        &store,
        "default",
        "scratch",
        "notes/new-auto-improve.md",
        "# New Auto Improve Lesson\n\nstaged earlier",
    )
    .await;

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("_slots/current-focus.md").unwrap(),
        frontmatter: serde_json::json!({"kind":"slot"}),
        body: "# Current Focus\n\nold focus".into(),
        tier: Tier::Working,
        pinned: false,
        title: Some("Current Focus".into()),
        admission_ctx: None,
        author_id: None,
        actor: sessionmunch_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Other,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "focus changed and durable lesson".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let router = admin_router(admin_state_for_store_with_llm(
        &tmp,
        &store,
        wiki,
        Some(Arc::new(FakeAutoImproveLlm)),
    ));
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch",
                        "session_id": session_id.to_string(),
                        "min_observations": 1,
                        "min_session_duration_secs": 0,
                        "min_confidence": 0.75,
                        "max_proposals_per_run": 5
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["proposals"].as_array().unwrap().len(),
        1,
        "the sibling proposal still stages",
    );
    let skipped = json["skipped"].as_array().expect("skipped array");
    assert_eq!(skipped.len(), 1, "the collided proposal must be reported");
    assert_eq!(skipped[0]["target_path"], "notes/new-auto-improve.md");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .is_some_and(|r| !r.trim().is_empty()),
        "a skipped proposal must say why: {}",
        skipped[0]
    );
}

/// `[auth].root_username` alone does not make a server multi-operator: the
/// scheduler and the report handlers stage unattributed, so bucketing this
/// call by its actor would leave TWO pending proposals for one page on a
/// single-operator server — the collision V42 promises cannot happen.
#[tokio::test]
async fn single_operator_admin_auto_improve_shares_the_unattributed_bucket() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let (ws, proj, _pending) = stage_pending_write(
        &store,
        "default",
        "scratch",
        "notes/new-auto-improve.md",
        "# New Auto Improve Lesson\n\nstaged earlier",
    )
    .await;

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("_slots/current-focus.md").unwrap(),
        frontmatter: serde_json::json!({"kind":"slot"}),
        body: "# Current Focus\n\nold focus".into(),
        tier: Tier::Working,
        pinned: false,
        title: Some("Current Focus".into()),
        admission_ctx: None,
        author_id: None,
        actor: sessionmunch_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Other,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "focus changed and durable lesson".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    // The root actor `[auth].root_username` produces, as the auth
    // middleware would stamp it.
    let router = admin_router(admin_state_for_store_with_llm(
        &tmp,
        &store,
        wiki,
        Some(Arc::new(FakeAutoImproveLlm)),
    ))
    .layer(axum::middleware::from_fn(
        |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(sessionmunch_core::ActorContext {
                    user: Some("the-operator".into()),
                    ..sessionmunch_core::ActorContext::default()
                });
            next.run(req).await
        },
    ));
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch",
                        "session_id": session_id.to_string(),
                        "min_observations": 1,
                        "min_session_duration_secs": 0,
                        "min_confidence": 0.75,
                        "max_proposals_per_run": 5
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let skipped = json["skipped"].as_array().expect("skipped array");
    assert_eq!(
        skipped.len(),
        1,
        "a named root operator must not open a second pending bucket for the same page"
    );
    assert_eq!(skipped[0]["target_path"], "notes/new-auto-improve.md");
}

#[tokio::test]
async fn auto_improve_admin_omitted_eval_inherits_server_defaults() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Other,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "focus changed and durable lesson".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    let mut state =
        admin_state_for_store_with_llm(&tmp, &store, wiki, Some(Arc::new(FakeAutoImproveLlm)));
    state.auto_improve_review_config.eval = sessionmunch_consolidate::AutoImproveEvalConfig {
        enabled: true,
        command: String::new(),
        timeout_secs: 1,
        targets: vec!["notes".into()],
        min_delta: 0.0,
    };
    let router = admin_router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch",
                        "session_id": session_id.to_string(),
                        "min_observations": 1,
                        "min_session_duration_secs": 0,
                        "min_confidence": 0.75,
                        "max_proposals_per_run": 5
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["rejected_candidates_count"], 1);
    let proposals = json["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(
        store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn auto_improve_require_approval_keeps_proposals_pending() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Other,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "durable lesson".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    let mut state =
        admin_state_for_store_with_llm(&tmp, &store, wiki, Some(Arc::new(FakeAutoImproveLlm)));
    state.auto_improve_require_approval = true;
    let router = admin_router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch",
                        "session_id": session_id.to_string(),
                        "min_observations": 1,
                        "min_session_duration_secs": 0,
                        "min_confidence": 0.75,
                        "max_proposals_per_run": 5
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["approval_required"], true);
    assert_eq!(json["approval_policy"], "manual");
    let proposals = json["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2);
    assert!(proposals.iter().all(|p| p["status"] == "pending"));

    let pending = store
        .reader
        .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Pending), 10)
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert!(
        store
            .reader
            .page_body_by_ids(ws, proj, "notes/new-auto-improve.md")
            .await
            .unwrap()
            .is_none(),
        "manual approval mode must not write target pages"
    );
}

#[tokio::test]
async fn auto_improve_removed_mode_fields_fail_closed() {
    let (_tmp, router) = read_page_test_router();
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/auto-improve")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch",
                        "session_id": "00000000-0000-0000-0000-000000000000",
                        "dry_run": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("removed")
    );
}

#[tokio::test]
async fn curator_dry_run_returns_report_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/curator")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["workspace"], "default");
    assert_eq!(json["project"], "scratch");
    assert_eq!(json["dry_run"], true);
    assert!(json["findings"].as_array().unwrap().is_empty());
    assert!(
        store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "dry-run must not stage proposals"
    );
    assert!(
        store
            .reader
            .list_pages("default", "scratch")
            .await
            .unwrap()
            .is_empty(),
        "dry-run must not write pages"
    );
}

#[tokio::test]
async fn curator_stage_creates_one_report_proposal_and_approval_writes_report_only() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/curator")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "scratch",
                        "stage": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let proposal_ids = json["proposal_ids"].as_array().unwrap();
    assert_eq!(proposal_ids.len(), 1);
    let sidecar_paths = json["sidecar_paths"].as_array().unwrap();
    assert_eq!(sidecar_paths.len(), 1);
    let sidecar_path = sidecar_paths[0].as_str().unwrap();
    assert!(std::path::Path::new(sidecar_path).exists());

    let staged = store
        .reader
        .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Pending), 10)
        .await
        .unwrap();
    assert_eq!(staged.len(), 1);
    let summary = &staged[0];
    assert_eq!(summary.kind, "curator_report");
    assert_eq!(summary.title, "Curator Report");
    assert_eq!(summary.operation, AutoImproveProposalOperation::Create);
    assert!(summary.target_path.as_str().starts_with("notes/curator-"));
    assert!(summary.target_path.as_str().ends_with(".md"));
    assert!(
        store
            .reader
            .page_body_by_ids(ws, proj, summary.target_path.as_str())
            .await
            .unwrap()
            .is_none(),
        "stage must not write target page"
    );

    let detail = store
        .reader
        .auto_improve_proposal_detail(ws, proj, summary.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.events[0].event, "staged");
    assert_eq!(detail.events[0].actor_json["agent"], "curator");
    assert!(detail.body_markdown.starts_with("# Curator Report"));
    assert!(detail.body_markdown.contains("Report-only"));

    let approve_resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/admin/pending-writes/{}/approve?workspace=default&project=scratch",
                    summary.id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(approve_resp.status(), StatusCode::OK);
    let page = store
        .reader
        .page_body_by_ids(ws, proj, summary.target_path.as_str())
        .await
        .unwrap()
        .expect("approval writes curator report page");
    assert!(page.body.starts_with("# Curator Report"));
    assert_eq!(
        store
            .reader
            .list_pages("default", "scratch")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn pending_writes_list_detail_diff_and_reject_use_stored_proposal() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let (_ws, _proj, proposal_id) = stage_pending_write(
        &store,
        "default",
        "scratch",
        "notes/pending.md",
        "# Pending\n\nproposed body",
    )
    .await;
    let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

    let list_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/pending-writes?workspace=default&project=scratch")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let body = to_bytes(list_resp.into_body(), usize::MAX).await.unwrap();
    let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], proposal_id.to_string());
    assert_eq!(list[0]["status"], "pending");

    let detail_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/admin/pending-writes/{proposal_id}?workspace=default&project=scratch"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail_resp.status(), StatusCode::OK);
    let body = to_bytes(detail_resp.into_body(), usize::MAX).await.unwrap();
    let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(detail["summary"]["target_path"], "notes/pending.md");
    assert_eq!(detail["body_markdown"], "# Pending\n\nproposed body");

    let diff_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/admin/pending-writes/{proposal_id}/diff?workspace=default&project=scratch"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(diff_resp.status(), StatusCode::OK);
    let body = to_bytes(diff_resp.into_body(), usize::MAX).await.unwrap();
    let diff: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(diff["diff"].as_str().unwrap().contains("proposed body"));

    let reject_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/admin/pending-writes/{proposal_id}/reject?workspace=default&project=scratch"
                ))
                .header("content-type", "application/json")
                .extension(sessionmunch_core::ActorContext {
                    user: Some("reviewer".into()),
                    ..sessionmunch_core::ActorContext::default()
                })
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"reason":"not now"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reject_resp.status(), StatusCode::OK);
    assert!(
        store
            .reader
            .page_body_by_ids(_ws, _proj, "notes/pending.md")
            .await
            .unwrap()
            .is_none(),
        "reject must not write the target page"
    );
}

#[tokio::test]
async fn pending_write_approve_writes_stored_body() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let (ws, proj, proposal_id) = stage_pending_write(
        &store,
        "default",
        "scratch",
        "notes/approved.md",
        "# Stored\n\nfrom stored proposal",
    )
    .await;
    wiki.write_auto_improve_sidecar(ws, proj, proposal_id)
        .await
        .unwrap();
    let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/admin/pending-writes/{proposal_id}/approve?workspace=default&project=scratch"
                ))
                .extension(sessionmunch_core::ActorContext {
                    user: Some("reviewer".into()),
                    ..sessionmunch_core::ActorContext::default()
                })
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let page = store
        .reader
        .page_body_by_ids(ws, proj, "notes/approved.md")
        .await
        .unwrap()
        .expect("approve writes target page");
    assert_eq!(page.body, "# Stored\n\nfrom stored proposal");
}

#[tokio::test]
async fn read_page_missing_path_returns_404() {
    let (_tmp, router) = read_page_test_router();

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/read-page?workspace=default&project=audit&path=notes/nope.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn read_page_traversal_rejected_with_400() {
    let (_tmp, router) = read_page_test_router();

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/read-page?workspace=default&project=audit&path=../etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn read_page_without_path_or_query_returns_400() {
    let (_tmp, router) = read_page_test_router();

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/read-page?workspace=default&project=audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Bug E regression: purge deletes the project's DB rows first, so by the
/// time the admission chain fires, name-resolution from the (now gone)
/// project row would yield an empty name and a name-based mirror would
/// purge the wrong path. The handler must seed the admission context with
/// the request's workspace/project names. We capture the `purge_project`
/// webhook and assert it carries the real project name.
#[tokio::test]
async fn purge_project_admission_carries_the_project_name() {
    use axum::http::HeaderMap;
    use axum::routing::post;
    use sessionmunch_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};
    use std::sync::Mutex;

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();
    let app = Router::new().route(
        "/sync",
        post(
            move |headers: HeaderMap, Json(payload): Json<serde_json::Value>| {
                let cap = cap.clone();
                async move {
                    if headers.get("X-Memory-Op").and_then(|v| v.to_str().ok())
                        == Some("purge_project")
                    {
                        *cap.lock().unwrap() = Some(
                            payload["ctx"]["project"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }
                    StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "mirror".into(),
        url: format!("{base}/sync"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage, AdmissionOp::PurgeProject],
        blocking: true,
    }])
    .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_admission_chain(chain)
        .with_store_reader(store.reader.clone());
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    post_write_page(&router, "default", "doomed", "notes/x.md", "bye").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/purge-project")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "doomed",
                        "confirm": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "purge should succeed");

    // Let the async notify land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        captured.lock().unwrap().as_deref(),
        Some("doomed"),
        "purge admission must carry the real project name, not an empty/_unscoped placeholder"
    );
}

/// Scope-guard / attribution regression: a destructive admin op must carry
/// the authenticated actor into the admission context, so a scope-guard
/// webhook can authorize by user. In prod a purge ran with an empty actor
/// (`AdmissionContext { ..Default::default() }`) and scope-guard rejected it
/// with `403 user '' not allowed to purge_project`.
#[tokio::test]
async fn purge_project_admission_carries_the_actor() {
    use axum::http::HeaderMap;
    use axum::routing::post;
    use sessionmunch_core::ActorContext;
    use sessionmunch_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};
    use std::sync::Mutex;

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();
    let app = Router::new().route(
        "/sync",
        post(
            move |headers: HeaderMap, Json(payload): Json<serde_json::Value>| {
                let cap = cap.clone();
                async move {
                    if headers.get("X-Memory-Op").and_then(|v| v.to_str().ok())
                        == Some("purge_project")
                    {
                        *cap.lock().unwrap() = Some(
                            payload["ctx"]["actor"]["user"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }
                    StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "mirror".into(),
        url: format!("{base}/sync"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage, AdmissionOp::PurgeProject],
        blocking: true,
    }])
    .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_admission_chain(chain)
        .with_store_reader(store.reader.clone());
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    post_write_page(&router, "default", "doomed", "notes/x.md", "bye").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/purge-project")
                .header("content-type", "application/json")
                .extension(ActorContext {
                    user: Some("alice".into()),
                    ..ActorContext::default()
                })
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "doomed",
                        "confirm": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "purge should succeed");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        captured.lock().unwrap().as_deref(),
        Some("alice"),
        "purge admission must carry the authenticated actor (empty actor → scope-guard 403 in prod)"
    );
}

/// End-to-end reproduction of the prod scope-guard scenario: a
/// `Reject`-policy admission webhook that authorizes `purge_project` by
/// `ctx.actor.user` (the real scope-guard shape). Before the fix the handler
/// sent an empty actor, so the guard returned 403 and the purge 500'd; with
/// the actor propagated, a matching user is allowed and the purge succeeds.
/// Both branches run in one test so the second (allowed) purge returning
/// `200` — not `404` — also proves the first (denied) purge left the project
/// intact.
#[tokio::test]
async fn scope_guard_blocks_purge_without_actor_allows_with_actor() {
    use axum::routing::post;
    use sessionmunch_core::ActorContext;
    use sessionmunch_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};

    // scope-guard emulation: allow `purge_project` only when the admission
    // payload's actor is "alice"; deny (403) otherwise.
    let app = Router::new().route(
        "/guard",
        post(|Json(payload): Json<serde_json::Value>| async move {
            if payload["ctx"]["actor"]["user"].as_str() == Some("alice") {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::FORBIDDEN
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "scope-guard".into(),
        url: format!("{base}/guard"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::PurgeProject],
        blocking: true,
    }])
    .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_admission_chain(chain)
        .with_store_reader(store.reader.clone());
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    post_write_page(&router, "default", "doomed", "notes/x.md", "bye").await;

    let purge_req = |actor: Option<&'static str>| {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/admin/purge-project")
            .header("content-type", "application/json");
        if let Some(user) = actor {
            builder = builder.extension(ActorContext {
                user: Some(user.into()),
                ..ActorContext::default()
            });
        }
        builder
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "workspace": "default",
                    "project": "doomed",
                    "confirm": true,
                }))
                .unwrap(),
            ))
            .unwrap()
    };

    // Case A — empty actor → scope-guard 403 → handler 500, project intact.
    let denied = router.clone().oneshot(purge_req(None)).await.unwrap();
    assert_eq!(
        denied.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "empty actor must be rejected by scope-guard (the prod bug)"
    );

    // Case B — actor=alice → scope-guard 204 → purge succeeds (200). A 200
    // here (not 404) also proves Case A left the project intact.
    let allowed = router
        .clone()
        .oneshot(purge_req(Some("alice")))
        .await
        .unwrap();
    assert_eq!(
        allowed.status(),
        StatusCode::OK,
        "matching actor must be authorized and the purge succeed"
    );
}

/// Merge moves copy source pages through `Wiki::write_page` before purging
/// the source. That copy path must carry the authenticated actor too;
/// otherwise a scope-guard webhook that gates `write_page` by user still
/// rejects `/admin/move-project` even though purge/move admission was fixed.
#[tokio::test]
async fn move_project_merge_copy_admission_carries_the_actor() {
    use axum::routing::post;
    use sessionmunch_core::ActorContext;
    use sessionmunch_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};

    let app = Router::new().route(
        "/guard",
        post(|Json(payload): Json<serde_json::Value>| async move {
            if payload["ctx"]["actor"]["user"].as_str() == Some("alice") {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::FORBIDDEN
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "scope-guard".into(),
        url: format!("{base}/guard"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::WritePage, AdmissionOp::PurgeProject],
        blocking: true,
    }])
    .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_admission_chain(chain)
        .with_store_reader(store.reader.clone());
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    post_write_page_with_actor(
        &router,
        "src",
        "merged",
        "notes/source.md",
        "source body",
        Some("alice"),
    )
    .await;
    // Same project name in the destination workspace forces the copy-purge
    // merge path instead of the true-move path.
    post_write_page_with_actor(
        &router,
        "dst",
        "merged",
        "notes/existing.md",
        "existing body",
        Some("alice"),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/move-project")
                .header("content-type", "application/json")
                .extension(ActorContext {
                    user: Some("alice".into()),
                    ..ActorContext::default()
                })
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "from_workspace": "src",
                        "project": "merged",
                        "to_workspace": "dst",
                        "confirm": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "merge move should succeed");
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["merged_into_existing"], true);
    assert_eq!(json["moved_via"], "copy-purge");
    assert_eq!(json["source_purged"], true);
}

#[tokio::test]
async fn move_project_guard_and_true_move_retarget_keyed_active_entries() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let active =
        sessionmunch_core::ActiveProject::with_mode(sessionmunch_core::ActiveProjectMode::PerActor);
    let mut state = admin_state_for_store(&tmp, &store, wiki);
    state.active_project = active.clone();
    let router = admin_router(state);

    post_write_page(&router, "src", "live", "notes/source.md", "source body").await;
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    let src_proj = store
        .reader
        .find_project(src_ws, "live".to_string())
        .await
        .unwrap()
        .unwrap();
    let actor = sessionmunch_core::ActorKey {
        user: Some("alice".into()),
        session_id: Some("session-1".into()),
    };
    active.set_for(&actor, src_ws, src_proj, false);
    active.set(WorkspaceId::new(), ProjectId::new());

    let move_body = serde_json::json!({
        "from_workspace": "src",
        "project": "live",
        "to_workspace": "dst",
        "confirm": true,
    });
    let guarded = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/move-project")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&move_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(guarded.status(), StatusCode::CONFLICT);

    let forced_body = serde_json::json!({
        "from_workspace": "src",
        "project": "live",
        "to_workspace": "dst",
        "confirm": true,
        "force": true,
    });
    let moved = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/move-project")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&forced_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(moved.status(), StatusCode::OK);
    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.get_for(&actor), Some((dst_ws, src_proj)));
}

#[tokio::test]
async fn delete_workspace_clears_single_and_keyed_active_entries() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let active =
        sessionmunch_core::ActiveProject::with_mode(sessionmunch_core::ActiveProjectMode::PerActor);
    let mut state = admin_state_for_store(&tmp, &store, wiki);
    state.active_project = active.clone();
    let router = admin_router(state);

    post_write_page(&router, "doomed-ws", "p", "notes/x.md", "bye").await;
    let ws = store
        .reader
        .find_workspace("doomed-ws".to_string())
        .await
        .unwrap()
        .unwrap();
    let proj = store
        .reader
        .find_project(ws, "p".to_string())
        .await
        .unwrap()
        .unwrap();
    let actor = sessionmunch_core::ActorKey {
        user: Some("alice".into()),
        session_id: Some("session-1".into()),
    };
    active.set_for(&actor, ws, proj, false);
    active.set(ws, proj);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/delete-workspace")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "doomed-ws",
                        "force": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(active.get().is_none());
    assert!(active.get_for(&actor).is_none());
    assert!(!active.contains_workspace(ws));
}

#[tokio::test]
async fn merge_workspace_folds_projects_and_deletes_empty_source() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let state = admin_state_for_store(&tmp, &store, wiki);
    let router = admin_router(state);

    // Two fresh projects in the source; the destination has neither.
    post_write_page(&router, "src", "alpha", "notes/a.md", "alpha body").await;
    post_write_page(&router, "src", "beta", "notes/b.md", "beta body").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/merge-workspace")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "from": "src",
                        "to": "dst",
                        "confirm": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["source_workspace_deleted"], true);
    let merged = json["projects_merged"].as_array().unwrap();
    assert_eq!(merged.len(), 2);
    // A fresh destination → each project is a lossless true-move.
    assert!(merged.iter().all(|r| r["moved_via"] == "true-move"));

    // The source workspace is gone; both projects now live under dst.
    assert!(
        store
            .reader
            .find_workspace("src".to_string())
            .await
            .unwrap()
            .is_none()
    );
    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .reader
            .find_project(dst_ws, "alpha".to_string())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .reader
            .find_project(dst_ws, "beta".to_string())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn merge_workspace_requires_confirm() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let state = admin_state_for_store(&tmp, &store, wiki);
    let router = admin_router(state);

    post_write_page(&router, "src", "alpha", "notes/a.md", "alpha body").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/merge-workspace")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "from": "src",
                        "to": "dst",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Nothing moved: the source workspace + project are untouched.
    assert!(
        store
            .reader
            .find_workspace("src".to_string())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn merge_workspace_merges_colliding_project_by_content() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let state = admin_state_for_store(&tmp, &store, wiki);
    let router = admin_router(state);

    // `shared` exists in BOTH workspaces (different page paths) → the move
    // for that project takes the copy-purge merge path, not a true-move.
    // `solo` exists only in the source → lossless true-move.
    post_write_page(&router, "dst", "shared", "notes/dst.md", "dst body").await;
    post_write_page(&router, "src", "shared", "notes/src.md", "src body").await;
    post_write_page(&router, "src", "solo", "notes/solo.md", "solo body").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/merge-workspace")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "from": "src",
                        "to": "dst",
                        "confirm": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["source_workspace_deleted"], true);
    let merged = json["projects_merged"].as_array().unwrap();
    assert_eq!(merged.len(), 2);
    let vias: std::collections::HashSet<&str> = merged
        .iter()
        .filter_map(|r| r["moved_via"].as_str())
        .collect();
    assert!(
        vias.contains("copy-purge"),
        "the colliding project merged by copy-purge"
    );
    assert!(
        vias.contains("true-move"),
        "the solo project was a true-move"
    );

    // Source is gone; the merged `shared` project holds BOTH pages.
    assert!(
        store
            .reader
            .find_workspace("src".to_string())
            .await
            .unwrap()
            .is_none()
    );
    let pages = store.reader.list_pages("dst", "shared").await.unwrap();
    let paths: std::collections::HashSet<String> = pages.into_iter().map(|p| p.path).collect();
    assert!(paths.contains("notes/dst.md"));
    assert!(paths.contains("notes/src.md"));
}

fn recording_scope_invalidator(
    seen: Arc<std::sync::Mutex<Vec<ScopeInvalidation>>>,
) -> ScopeInvalidator {
    Arc::new(move |target| {
        let seen = seen.clone();
        Box::pin(async move {
            // Prove admin handlers await the invalidator before returning;
            // constructing and dropping this future would not record.
            tokio::task::yield_now().await;
            seen.lock().unwrap().push(target);
        })
    })
}

#[tokio::test]
async fn delete_workspace_awaits_workspace_cache_invalidation() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut state = admin_state_for_store(&tmp, &store, wiki);
    state.scope_invalidator = Some(recording_scope_invalidator(seen.clone()));
    let router = admin_router(state);

    post_write_page(&router, "doomed-ws", "p", "notes/x.md", "bye").await;
    let ws = store
        .reader
        .find_workspace("doomed-ws".to_string())
        .await
        .unwrap()
        .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/delete-workspace")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "doomed-ws",
                        "force": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![ScopeInvalidation::Workspace(ws)]
    );
}

#[tokio::test]
async fn copy_purge_move_clears_keyed_active_entries_and_awaits_project_invalidation() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let active =
        sessionmunch_core::ActiveProject::with_mode(sessionmunch_core::ActiveProjectMode::PerActor);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut state = admin_state_for_store(&tmp, &store, wiki);
    state.active_project = active.clone();
    state.scope_invalidator = Some(recording_scope_invalidator(seen.clone()));
    let router = admin_router(state);

    post_write_page(&router, "src", "merged", "notes/source.md", "source body").await;
    post_write_page(
        &router,
        "dst",
        "merged",
        "notes/existing.md",
        "existing body",
    )
    .await;
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    let src_proj = store
        .reader
        .find_project(src_ws, "merged".to_string())
        .await
        .unwrap()
        .unwrap();
    let actor = sessionmunch_core::ActorKey {
        user: Some("alice".into()),
        session_id: Some("session-1".into()),
    };
    active.set_for(&actor, src_ws, src_proj, false);
    active.set(src_ws, src_proj);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/move-project")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "from_workspace": "src",
                        "project": "merged",
                        "to_workspace": "dst",
                        "confirm": true,
                        "force": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(active.get().is_none());
    assert!(active.get_for(&actor).is_none());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![ScopeInvalidation::Project(src_proj)]
    );
}

// ── user-management endpoints (P1.4) ──────────────────────────

/// Test router that has a token pepper configured (so user-management
/// endpoints don't return 503), runs the standard auth middleware
/// upstream so `Extension<AuthLevel>` is populated, and is reachable
/// as Root via a fixed bearer token.
fn user_admin_test_router(root_token: &'static str) -> (TempDir, Router) {
    use sessionmunch_core::ActorContext;
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let pepper = sessionmunch_store::TokenPepper::new("test-pepper-admin");
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: Some(pepper),
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });
    // Wrap in a middleware that stamps the AuthLevel ourselves —
    // the real auth middleware lives in sessionmunch-cli, and this
    // crate can't depend on it. The wrapper matches the Bearer
    // header against `root_token`: match → Root, present-but-no-
    // match → User (so we can test 403), absent → Anonymous.
    let router = router.layer(axum::middleware::from_fn(
        move |mut req: Request<Body>, next: axum::middleware::Next| async move {
            let bearer = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .map(str::to_string);
            let level = match bearer.as_deref() {
                Some(t) if t == root_token => sessionmunch_core::AuthLevel::Root,
                Some(_) => sessionmunch_core::AuthLevel::User,
                None => sessionmunch_core::AuthLevel::Anonymous,
            };
            req.extensions_mut().insert(level);
            req.extensions_mut().insert(ActorContext::anonymous());
            next.run(req).await
        },
    ));
    (tmp, router)
}

/// Same AdminState as [`user_admin_test_router`], but AuthLevel comes from
/// the production dual-auth Bearer lookup (`authenticate_bearer` +
/// `api_credentials` hash), not a stub that stamps Root/User by string
/// compare. Configured `root_token` is AuthLevel::Root; a DB native key
/// authenticates as AuthLevel::User even when `users.role` is root.
fn user_admin_dual_auth_router(root_token: &'static str) -> (TempDir, Router) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let pepper = sessionmunch_store::TokenPepper::new("test-pepper-admin");
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: Some(pepper.clone()),
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });
    let auth_state = Arc::new(
        crate::auth::AuthState::new(Some(root_token.to_string())).with_multiuser(
            pepper,
            store.reader.clone(),
            store.writer.clone(),
        ),
    );
    let router = router.layer(axum::middleware::from_fn_with_state(
        auth_state,
        crate::human_auth::require_dual_auth,
    ));
    (tmp, router)
}

async fn post_disable_user(
    router: &Router,
    root_token: &str,
    username: &str,
) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/users/{username}/disable"))
                .header("authorization", format!("Bearer {root_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn patch_user(
    router: &Router,
    root_token: &str,
    username: &str,
    body: serde_json::Value,
) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/admin/users/{username}"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {root_token}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn post_create_user(
    router: &Router,
    root_token: &str,
    body: serde_json::Value,
) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {root_token}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn post_create_human_user(
    router: &Router,
    root_token: &str,
    body: serde_json::Value,
) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/human-users")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {root_token}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn user_token_admin_status(router: &Router, token: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/users")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

fn admin_route_samples() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    vec![
        ("POST", "/admin/backup", serde_json::Value::Null),
        (
            "POST",
            "/admin/export-okf?workspace=default&project=scratch",
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/admin/bootstrap",
            serde_json::json!({
                "workspace": "default",
                "project": "scratch",
                "sources": [],
                "dry_run": true
            }),
        ),
        (
            "POST",
            "/admin/auto-improve",
            serde_json::json!({
                "workspace": "default",
                "project": "scratch",
                "session_id": "00000000-0000-0000-0000-000000000000",
                "dry_run": true
            }),
        ),
        (
            "POST",
            "/admin/auto-improve/report",
            serde_json::json!({"workspace": "default", "project": "scratch"}),
        ),
        ("GET", "/admin/status", serde_json::Value::Null),
        (
            "GET",
            "/admin/sessions/by-agent?workspace=default&project=scratch",
            serde_json::Value::Null,
        ),
        (
            "GET",
            "/admin/activity/by-client?since_days=7",
            serde_json::Value::Null,
        ),
        ("GET", "/admin/audit-contamination", serde_json::Value::Null),
        ("GET", "/admin/audit-log", serde_json::Value::Null),
        ("GET", "/admin/search?q=test", serde_json::Value::Null),
        (
            "GET",
            "/admin/read-page?workspace=default&project=scratch&path=notes/x.md",
            serde_json::Value::Null,
        ),
        ("POST", "/admin/reorg", serde_json::json!({"dry_run": true})),
        (
            "POST",
            "/admin/lint",
            serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
        ),
        (
            "POST",
            "/admin/forget-sweep",
            serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
        ),
        (
            "POST",
            "/admin/embed",
            serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
        ),
        (
            "POST",
            "/admin/curator",
            serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
        ),
        (
            "POST",
            "/admin/commit",
            serde_json::json!({"message": "test"}),
        ),
        ("GET", "/admin/checkpoints?limit=1", serde_json::Value::Null),
        (
            "GET",
            "/admin/pending-writes?workspace=default&project=scratch",
            serde_json::Value::Null,
        ),
        (
            "GET",
            "/admin/pending-writes/00000000-0000-0000-0000-000000000000?workspace=default&project=scratch",
            serde_json::Value::Null,
        ),
        (
            "GET",
            "/admin/pending-writes/00000000-0000-0000-0000-000000000000/diff?workspace=default&project=scratch",
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/admin/pending-writes/00000000-0000-0000-0000-000000000000/approve?workspace=default&project=scratch",
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/admin/pending-writes/00000000-0000-0000-0000-000000000000/reject?workspace=default&project=scratch",
            serde_json::json!({"reason": "no"}),
        ),
        (
            "POST",
            "/admin/restore-page",
            serde_json::json!({
                "workspace": "default",
                "project": "scratch",
                "path": "notes/x.md",
                "rev": "HEAD"
            }),
        ),
        (
            "POST",
            "/admin/purge-project",
            serde_json::json!({"workspace": "default", "project": "scratch", "confirm": true}),
        ),
        (
            "POST",
            "/admin/rename-project",
            serde_json::json!({"workspace": "default", "from": "scratch", "to": "renamed"}),
        ),
        (
            "POST",
            "/admin/move-project",
            serde_json::json!({
                "from_workspace": "default",
                "to_workspace": "archive",
                "project": "scratch",
                "confirm": true
            }),
        ),
        (
            "POST",
            "/admin/delete-workspace",
            serde_json::json!({"workspace": "default", "force": true}),
        ),
        (
            "POST",
            "/admin/rename-workspace",
            serde_json::json!({"from": "default", "to": "renamed"}),
        ),
        (
            "POST",
            "/admin/write-page",
            serde_json::json!({
                "workspace": "default",
                "project": "scratch",
                "path": "notes/x.md",
                "body": "body"
            }),
        ),
        (
            "POST",
            "/admin/delete-page",
            serde_json::json!({
                "workspace": "default",
                "project": "scratch",
                "path": "notes/x.md"
            }),
        ),
        ("GET", "/admin/users", serde_json::Value::Null),
        (
            "POST",
            "/admin/users",
            serde_json::json!({"username": "alice"}),
        ),
        (
            "POST",
            "/admin/human-users",
            serde_json::json!({"username": "human"}),
        ),
        ("POST", "/admin/users/alice/expire", serde_json::Value::Null),
        ("POST", "/admin/users/alice/revive", serde_json::Value::Null),
        (
            "POST",
            "/admin/users/alice/rotate-token",
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/admin/users/alice/disable",
            serde_json::Value::Null,
        ),
        ("POST", "/admin/users/alice/enable", serde_json::Value::Null),
        ("GET", "/admin/api-credentials", serde_json::Value::Null),
    ]
}

#[tokio::test]
async fn multiuser_admin_routes_reject_db_user_tier() {
    let (_tmp, router) = user_admin_test_router("root-token");
    post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;

    for (method, uri, payload) in admin_route_samples() {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer db-user-token");
        let body = if payload.is_null() {
            Body::empty()
        } else {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&payload).unwrap())
        };
        let resp = router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} must be root-only for DB users in multi-user mode"
        );
    }
}

#[tokio::test]
async fn multiuser_admin_routes_reject_anonymous() {
    let (_tmp, router) = user_admin_test_router("root-token");
    post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;
    for (method, uri, payload) in admin_route_samples() {
        let mut builder = Request::builder().method(method).uri(uri);
        let body = if payload.is_null() {
            Body::empty()
        } else {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&payload).unwrap())
        };
        let resp = router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must require authentication in multi-user mode"
        );
    }
}

#[tokio::test]
async fn multiuser_operational_admin_routes_allow_root() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/status")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/activity/by-client?since_days=7")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The scope does not exist in this fixture, so reaching the handler
    // produces 404. A rejected root request would instead be 401/403.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/sessions/by-agent?workspace=default&project=scratch")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn trusted_proxy_topology_gates_admin_without_db_users() {
    use sessionmunch_core::ActorContext;

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: Some(sessionmunch_store::TokenPepper::new("test-pepper-admin")),
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: true,
    })
    .layer(axum::middleware::from_fn(
        |mut req: Request<Body>, next: axum::middleware::Next| async move {
            let level = match req
                .headers()
                .get("x-test-auth-level")
                .and_then(|value| value.to_str().ok())
            {
                Some("root") => sessionmunch_core::AuthLevel::Root,
                Some("user") => sessionmunch_core::AuthLevel::User,
                _ => sessionmunch_core::AuthLevel::Anonymous,
            };
            req.extensions_mut().insert(level);
            req.extensions_mut().insert(ActorContext::anonymous());
            next.run(req).await
        },
    ));

    for (level, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("user"), StatusCode::FORBIDDEN),
        (Some("root"), StatusCode::OK),
    ] {
        let mut request = Request::builder().uri("/admin/status");
        if let Some(level) = level {
            request = request.header("x-test-auth-level", level);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "level={level:?}");
    }
}

#[tokio::test]
async fn admin_mode_switches_after_first_user_without_restart() {
    let (_tmp, router) = user_admin_test_router("root-token");

    let anonymous_before = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous_before.status(), StatusCode::OK);

    let create = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;
    assert_eq!(create.status(), StatusCode::OK);

    for (token, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("db-user-token"), StatusCode::FORBIDDEN),
        (Some("root-token"), StatusCode::OK),
    ] {
        let mut request = Request::builder().uri("/admin/status");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "token {token:?}");
    }

    let expire = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users/alice/disable")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(expire.status(), StatusCode::OK);

    let anonymous_after_expire = router
        .oneshot(
            Request::builder()
                .uri("/admin/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous_after_expire.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_mode_store_read_failure_fails_closed() {
    use sessionmunch_core::ActorContext;

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let missing_db_reader =
        sessionmunch_store::ReaderPool::new(&tmp.path().join("missing.sqlite"), 1).unwrap();
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: missing_db_reader,
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: Some(sessionmunch_store::TokenPepper::new("test-pepper-admin")),
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    })
    .layer(axum::middleware::from_fn(
        |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(sessionmunch_core::AuthLevel::Anonymous);
            req.extensions_mut().insert(ActorContext::anonymous());
            next.run(req).await
        },
    ));

    let response = router
        .oneshot(
            Request::builder()
                .uri("/admin/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("could not verify user configuration"));
}

#[tokio::test]
async fn multiuser_workspace_admin_routes_reach_handler_for_root() {
    let (_tmp, router) = user_admin_test_router("root-token");
    for (uri, payload) in [
        (
            "/admin/delete-workspace",
            serde_json::json!({"workspace": "missing", "force": true}),
        ),
        (
            "/admin/rename-workspace",
            serde_json::json!({"from": "missing", "to": "renamed"}),
        ),
    ] {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("authorization", "Bearer root-token")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} rejected root auth"
        );
        assert_ne!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{uri} rejected root auth"
        );
    }
}

#[tokio::test]
async fn create_user_happy_path_returns_token_once() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let resp = post_create_user(
        &router,
        "root-token",
        serde_json::json!({
            "username": "alice",
            "name": "Alice Smith",
            "email": "Alice@Example.com"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["user"]["username"], "alice");
    // Email was normalised to lowercase by NewUser::validate.
    assert_eq!(json["user"]["email"], "alice@example.com");
    assert_eq!(json["user"]["name"], "Alice Smith");
    // Plaintext token is surfaced exactly once.
    let token = json["token"].as_str().unwrap();
    assert_eq!(token.len(), 43);
    assert!(json["user"]["token_expired_at"].is_null());
    assert!(json.get("temporary_password").is_none());
}

#[tokio::test]
async fn create_human_user_is_additive_and_returns_temporary_password_once() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let resp = post_create_human_user(
        &router,
        "root-token",
        serde_json::json!({
            "username": "alice",
            "role": "root"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["temporary_password"].as_str().unwrap().len() >= 12);
    assert_eq!(json["user"]["role"], "root");
    assert!(json["user"]["must_change_password"].as_bool().unwrap());
    assert!(json.get("token").is_none());
}

#[tokio::test]
async fn deprecated_user_token_routes_drive_the_legacy_api_credential() {
    let (_tmp, router) = user_admin_dual_auth_router("root-token");
    let create = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;
    assert_eq!(create.status(), StatusCode::OK);
    let body = to_bytes(create.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let original = json["token"].as_str().unwrap().to_string();
    assert_eq!(
        user_token_admin_status(&router, &original).await,
        StatusCode::FORBIDDEN,
        "active legacy token must authenticate as a non-root DB user"
    );

    let expire = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users/alice/expire")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(expire.status(), StatusCode::OK);
    assert_eq!(
        user_token_admin_status(&router, &original).await,
        StatusCode::UNAUTHORIZED
    );

    let revive = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users/alice/revive")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revive.status(), StatusCode::OK);
    assert_eq!(
        user_token_admin_status(&router, &original).await,
        StatusCode::FORBIDDEN
    );

    let rotate = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users/alice/rotate-token")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rotate.status(), StatusCode::OK);
    let body = to_bytes(rotate.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let rotated = json["token"].as_str().unwrap();
    assert_ne!(rotated, original);
    assert_eq!(
        user_token_admin_status(&router, &original).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        user_token_admin_status(&router, rotated).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn create_user_rejects_duplicate_username_with_409() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let _ = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;
    let resp = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn create_user_rejects_invalid_email_with_400() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let resp = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice", "email": "not-an-email"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_user_as_anonymous_returns_401() {
    let (_tmp, router) = user_admin_test_router("root-token");
    // No Authorization header → middleware stamps Anonymous tier.
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"username": "alice"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_user_as_user_tier_returns_403() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let resp = post_create_user(
        &router,
        "not-the-root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn list_users_returns_added_users_in_insertion_order() {
    let (_tmp, router) = user_admin_test_router("root-token");
    for n in ["alice", "bob", "carol"] {
        let _ = post_create_user(&router, "root-token", serde_json::json!({"username": n})).await;
    }
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/users")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let users = json["users"].as_array().unwrap();
    assert_eq!(users.len(), 3);
    assert_eq!(users[0]["username"], "alice");
    assert_eq!(users[1]["username"], "bob");
    assert_eq!(users[2]["username"], "carol");
    // Tokens are NEVER surfaced by the list endpoint.
    for u in users {
        assert!(
            u.get("token").is_none() && u.get("temporary_password").is_none(),
            "list must not leak secrets"
        );
    }
}

#[tokio::test]
async fn disable_then_enable_round_trips() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let _ = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users/alice/disable")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["user"]["disabled_at"].is_i64());

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users/alice/enable")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["user"]["disabled_at"].is_null());
}

#[tokio::test]
async fn disable_unknown_user_returns_404() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users/ghost/disable")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rotate_api_credential_issues_a_distinct_secret() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let _ = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "alice"}),
    )
    .await;
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api-credentials")
                .header("authorization", "Bearer root-token")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "username": "alice",
                        "label": "laptop"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);
    let body = to_bytes(create_resp.into_body(), usize::MAX).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let original_token = created["token"].as_str().unwrap().to_string();
    let id = created["credential"]["id"].as_str().unwrap();
    assert!(original_token.starts_with("aim_"));

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/api-credentials/{id}/rotate"))
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let new_token = json["token"].as_str().unwrap();
    assert!(new_token.starts_with("aim_"));
    assert_ne!(new_token, original_token, "rotate must change the secret");
}

#[tokio::test]
async fn create_human_user_works_without_pepper_but_api_keys_return_503() {
    // Humans do not need a pepper. Native API keys still do.
    use sessionmunch_core::ActorContext;
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let router = admin_router(AdminState {
        ingest_metrics: std::sync::Arc::new(sessionmunch_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: sessionmunch_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });
    // Inject Root so the human create reaches its handler; only native
    // API credentials require the pepper.
    let router = router.layer(axum::middleware::from_fn(
        |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(sessionmunch_core::AuthLevel::Root);
            req.extensions_mut().insert(ActorContext::anonymous());
            next.run(req).await
        },
    ));

    let created = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/human-users")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"username": "alice"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api-credentials")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "username": "alice",
                        "label": "cli"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = tmp;
}

#[tokio::test]
async fn patch_demotion_of_last_recoverable_root_returns_409() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let created = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "keeper", "role": "root"}),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["user"]["role"], "root");
    assert_eq!(json["user"]["has_password"].as_bool(), Some(true));

    let resp = patch_user(
        &router,
        "root-token",
        "keeper",
        serde_json::json!({"role": "user"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("last recoverable root"),
        "409 body must surface the store InvalidState mapping; got {json:?}"
    );
}

#[tokio::test]
async fn disable_of_last_recoverable_root_returns_409() {
    let (_tmp, router) = user_admin_test_router("root-token");
    let created = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "keeper", "role": "root"}),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);

    let resp = post_disable_user(&router, "root-token", "keeper").await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("last recoverable root"),
        "409 body must surface the store InvalidState mapping; got {json:?}"
    );
}

#[tokio::test]
async fn concurrent_root_removals_leave_one_recoverable_root() {
    let (_tmp, router) = user_admin_test_router("root-token");
    for name in ["alice", "bob"] {
        let created = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": name, "role": "root"}),
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK, "setup {name}");
    }

    let (demote, disable) = tokio::join!(
        patch_user(
            &router,
            "root-token",
            "alice",
            serde_json::json!({"role": "user"}),
        ),
        post_disable_user(&router, "root-token", "bob"),
    );
    let statuses = [demote.status(), disable.status()];
    let ok = statuses.iter().filter(|s| **s == StatusCode::OK).count();
    assert!(
        ok <= 1,
        "at most one concurrent root-removal may succeed; got {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::OK || *s == StatusCode::CONFLICT),
        "root-removal must be 200 or 409; got {statuses:?}"
    );

    let listed = router
        .oneshot(
            Request::builder()
                .uri("/admin/users")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let body = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let recoverable = json["users"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|u| {
            u["role"] == "root"
                && u["disabled_at"].is_null()
                && u["has_password"].as_bool() == Some(true)
        })
        .count();
    assert!(
        recoverable >= 1,
        "a recoverable root must remain after concurrent removals; got {json:?}"
    );
}

#[tokio::test]
async fn native_api_token_for_root_role_user_cannot_manage_users() {
    let (_tmp, router) = user_admin_dual_auth_router("root-token");
    let created = post_create_user(
        &router,
        "root-token",
        serde_json::json!({"username": "boss", "role": "root"}),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["user"]["role"], "root");

    let issued = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api-credentials")
                .header("authorization", "Bearer root-token")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "username": "boss",
                        "label": "legacy-user-token"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(issued.status(), StatusCode::OK);
    let body = to_bytes(issued.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let native_token = json["token"].as_str().unwrap();
    assert!(native_token.starts_with("aim_"));

    let as_native = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/users")
                .header("authorization", format!("Bearer {native_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        as_native.status(),
        StatusCode::FORBIDDEN,
        "role=root must not elevate a looked-up native API credential"
    );

    let unknown = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/users")
                .header("authorization", "Bearer aim_not-a-stored-credential")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        unknown.status(),
        StatusCode::UNAUTHORIZED,
        "unknown bearer must 401 through real lookup, not a User stub"
    );

    let as_root = router
        .oneshot(
            Request::builder()
                .uri("/admin/users")
                .header("authorization", "Bearer root-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(as_root.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------
// delete-page
// ---------------------------------------------------------------------

/// Helper: POST /admin/delete-page and return (status, body json).
async fn post_delete_page(
    router: &Router,
    ws: &str,
    project: &str,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let req_body = serde_json::json!({
        "workspace": ws,
        "project": project,
        "path": path,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/delete-page")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
    (status, json)
}

#[tokio::test]
async fn delete_page_removes_existing_page() {
    let (_tmp, router) = read_page_test_router();
    post_write_page(&router, "default", "audit", "notes/doomed.md", "bye body").await;

    // Confirm the page is reachable before delete.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/read-page?workspace=default&project=audit&path=notes/doomed.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "setup precondition: page must exist before delete"
    );

    let (status, json) = post_delete_page(&router, "default", "audit", "notes/doomed.md").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["path"], "notes/doomed.md");
    assert_eq!(json["deleted"], true);

    // Read-back must now 404.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/read-page?workspace=default&project=audit&path=notes/doomed.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "page must be gone after delete"
    );
}

/// A delete request whose `(workspace, project)` doesn't resolve to a
/// real `(WorkspaceId, ProjectId)` must NOT report success. The shared
/// `resolve_ws_proj` returns the unresolved-scope error; the handler
/// surfaces it as a 4xx/5xx — never as `deleted: true`. This guards the
/// Bug 5 regression where MCP `memory_delete_page` returned `true` for
/// a scope it never touched.
#[tokio::test]
async fn delete_page_unknown_workspace_does_not_fake_success() {
    let (_tmp, router) = read_page_test_router();
    let (status, json) =
        post_delete_page(&router, "no-such-ws", "audit", "notes/whatever.md").await;
    assert_ne!(
        status,
        StatusCode::OK,
        "delete on unresolved scope must not return 200/deleted=true; got body {json:?}",
    );
    assert!(
        json.get("deleted").and_then(|v| v.as_bool()) != Some(true),
        "body must not claim deleted=true on unresolved scope; got {json:?}"
    );
}

/// `Wiki::delete_page` is idempotent for a path that doesn't exist
/// inside an EXISTING (workspace, project) — the file is just not there
/// to quarantine. The handler reports `deleted: true` (i.e. "the call
/// succeeded") rather than 404, matching the documented MCP semantics.
#[tokio::test]
async fn delete_page_idempotent_for_missing_file_in_existing_scope() {
    let (_tmp, router) = read_page_test_router();
    // Seed the project so (workspace, project) resolves, but skip the
    // page we'll try to delete.
    post_write_page(&router, "default", "audit", "notes/keep.md", "keeper").await;

    let (status, json) =
        post_delete_page(&router, "default", "audit", "notes/never-existed.md").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["deleted"], true);
}

#[tokio::test]
async fn delete_page_traversal_rejected_with_422() {
    let (_tmp, router) = read_page_test_router();
    let (status, _) = post_delete_page(&router, "default", "audit", "../etc/passwd").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
