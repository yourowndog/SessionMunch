use super::*;
use sessionmunch_core::{Sanitized, Sanitizer};
use std::collections::BTreeSet;

#[test]
fn stdio_default_parts_resolves_to_anonymous_actor() {
    // The stdio transport injects no `Parts`, so the `OptionalParts`
    // extractor falls back to `default_parts()`. That synthetic default
    // must carry no actor identity — a local unauthenticated caller —
    // rather than error, which is what let tools/call fail on stdio.
    let key = SessionMunchServer::actor_key_from_parts(Some(&default_parts()));
    assert!(
        key.user.is_none() && key.session_id.is_none(),
        "stdio default must be anonymous"
    );
}

#[test]
fn optional_parts_missing_extension_uses_anonymous_stdio_default() {
    let extensions = rmcp::model::Extensions::new();

    let OptionalParts(parts) = OptionalParts::from_extensions(&extensions);

    assert_eq!(parts.method, axum::http::Method::POST);
    assert_eq!(parts.uri, axum::http::Uri::from_static("/mcp"));
    assert_eq!(
        SessionMunchServer::actor_key_from_parts(Some(&parts)),
        sessionmunch_core::ActorKey::default(),
        "missing request parts must degrade to anonymous stdio context instead of failing extraction"
    );
    assert!(parts.extensions.get::<AuthLevel>().is_none());
    assert!(
        parts
            .extensions
            .get::<sessionmunch_core::UserId>()
            .is_none()
    );
    assert!(parts.extensions.get::<ActorContext>().is_none());
}

#[test]
fn optional_parts_preserves_real_http_parts_and_auth_context() {
    let user_id = sessionmunch_core::UserId::new();
    let mut real_parts = test_parts_default();
    real_parts
        .headers
        .insert("mcp-session-id", "session-from-header".parse().unwrap());
    real_parts.extensions.insert(AuthLevel::User);
    real_parts.extensions.insert(user_id);
    real_parts.extensions.insert(ActorContext {
        user: Some("alice".into()),
        name: Some("Alice Smith".into()),
        email: Some("alice@example.com".into()),
        ..ActorContext::default()
    });

    let mut extensions = rmcp::model::Extensions::new();
    extensions.insert(real_parts);

    let OptionalParts(parts) = OptionalParts::from_extensions(&extensions);

    assert_eq!(parts.extensions.get::<AuthLevel>(), Some(&AuthLevel::User));
    assert_eq!(
        parts.extensions.get::<sessionmunch_core::UserId>(),
        Some(&user_id)
    );
    assert_eq!(
        parts
            .extensions
            .get::<ActorContext>()
            .and_then(|ctx| ctx.user.as_deref()),
        Some("alice")
    );
    assert_eq!(
        SessionMunchServer::actor_key_from_parts(Some(&parts)),
        sessionmunch_core::ActorKey {
            // Qualified: the same storage key the hook ingress publishes
            // under, so set and get land on the same slot.
            user: Some("user:alice".into()),
            session_id: Some("session-from-header".into()),
        },
        "real HTTP request parts must preserve auth identity and routing session"
    );
}

use sessionmunch_core::{
    ActorContext, AuthLevel, NewObservation, NewPage, NewSession, NewUser, ObservationKind,
    PagePath, Tier,
};
use sessionmunch_store::Store;
use sessionmunch_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

async fn setup_server() -> (TempDir, Store, SessionMunchServer, WorkspaceId, ProjectId) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("foo.md").unwrap(),
            title: "Foo".into(),
            body: "Karpathy says compile, not retrieve.".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
    (tmp, store, server, ws, proj)
}

fn installed_sessionmunch_prompt_surface() -> String {
    let mut prompt = String::from(sessionmunch_core::SNIPPET_BODY);
    for skill in sessionmunch_core::routing_skills::MANAGED_SKILLS {
        prompt.push_str("\n\n");
        prompt.push_str(skill.content);
    }
    prompt
}

fn combined_sessionmunch_prompt_surface() -> String {
    let mut prompt = String::from(MEMORY_INSTRUCTIONS);
    prompt.push_str("\n\n");
    prompt.push_str(&installed_sessionmunch_prompt_surface());
    prompt
}

fn assert_detailed_prompt_surfaces(mut assert_prompt: impl FnMut(&str, &str)) {
    assert_prompt("MCP handshake instructions", MEMORY_INSTRUCTIONS);
    let combined = combined_sessionmunch_prompt_surface();
    assert_prompt(
        "combined MCP, snippet, and managed skill prompts",
        &combined,
    );
}

fn call_tool_json(result: CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_else(|| panic!("expected text content"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("invalid JSON response: {e}\n{text}"))
}

enum StubRerankOutcome {
    Scores(Vec<sessionmunch_llm::RerankScore>),
    Reverse,
    Fail,
}

struct StubReranker {
    outcome: StubRerankOutcome,
    delay: Duration,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    candidate_counts: Arc<std::sync::Mutex<Vec<usize>>>,
}

#[async_trait::async_trait]
impl sessionmunch_llm::Reranker for StubReranker {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn model(&self) -> &str {
        "stub-model"
    }

    async fn rerank(
        &self,
        _query: &str,
        candidates: &[sessionmunch_llm::RerankCandidate],
    ) -> sessionmunch_llm::LlmResult<Vec<sessionmunch_llm::RerankScore>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.candidate_counts.lock().unwrap().push(candidates.len());
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        match &self.outcome {
            StubRerankOutcome::Scores(scores) => Ok(scores.clone()),
            StubRerankOutcome::Reverse => {
                let denominator = candidates.len().saturating_sub(1).max(1) as f32;
                Ok(candidates
                    .iter()
                    .enumerate()
                    .map(|(idx, candidate)| sessionmunch_llm::RerankScore {
                        id: candidate.id.clone(),
                        relevance: idx as f32 / denominator,
                    })
                    .collect())
            }
            StubRerankOutcome::Fail => Err(sessionmunch_llm::LlmError::UnexpectedShape(
                "stub failure".into(),
            )),
        }
    }
}

fn stub_reranker(
    outcome: StubRerankOutcome,
    delay: Duration,
) -> (
    Arc<StubReranker>,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::Mutex<Vec<usize>>>,
) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let candidate_counts = Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        Arc::new(StubReranker {
            outcome,
            delay,
            calls: calls.clone(),
            candidate_counts: candidate_counts.clone(),
        }),
        calls,
        candidate_counts,
    )
}

fn rerank_test_hits(count: usize) -> Vec<(PageHit, Option<sessionmunch_store::SearchExplain>)> {
    (0..count)
        .map(|idx| {
            (
                PageHit {
                    id: PageId::new(),
                    path: PagePath::new(format!("notes/{idx:03}.md")).unwrap(),
                    title: format!("Page {idx}"),
                    snippet: format!("candidate {idx}"),
                    rank: idx as f64,
                },
                Some(sessionmunch_store::SearchExplain::default()),
            )
        })
        .collect()
}

#[tokio::test]
async fn reranker_reorders_one_bounded_prefix_without_shrinking_large_limits() {
    let (_tmp, _store, server, _ws, _proj) = setup_server().await;
    let (reranker, calls, candidate_counts) =
        stub_reranker(StubRerankOutcome::Reverse, Duration::ZERO);
    let server = server.with_reranker(reranker);
    let hits = rerank_test_hits(40);
    let original_ids: Vec<PageId> = hits.iter().map(|(hit, _)| hit.id).collect();

    assert_eq!(server.rerank_fetch_limit(5), 15);
    assert_eq!(server.rerank_fetch_limit(20), 30);
    assert_eq!(server.rerank_fetch_limit(35), 35);
    let reranked = server.rerank_hits("query", hits, 35).await;

    assert_eq!(reranked.len(), 35);
    assert_eq!(reranked[0].0.id, original_ids[29]);
    assert_eq!(reranked[29].0.id, original_ids[0]);
    assert_eq!(
        reranked[30..]
            .iter()
            .map(|(hit, _)| hit.id)
            .collect::<Vec<_>>(),
        original_ids[30..35]
    );
    assert!(
        reranked[..30]
            .iter()
            .all(|(_, explain)| explain.as_ref().unwrap().rerank_score.is_some())
    );
    assert!(
        reranked[30..]
            .iter()
            .all(|(_, explain)| explain.as_ref().unwrap().rerank_score.is_none())
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(*candidate_counts.lock().unwrap(), vec![30]);
}

#[tokio::test]
async fn reranker_degrades_on_partial_malformed_error_and_timeout() {
    let (_tmp, _store, server, _ws, _proj) = setup_server().await;
    let hits = rerank_test_hits(4);
    let original_ids: Vec<PageId> = hits.iter().map(|(hit, _)| hit.id).collect();
    let partial = hits[..2]
        .iter()
        .map(|(hit, _)| sessionmunch_llm::RerankScore {
            id: hit.id.to_string(),
            relevance: 1.0,
        })
        .collect();
    let (reranker, _, _) = stub_reranker(StubRerankOutcome::Scores(partial), Duration::ZERO);
    let result = server
        .clone()
        .with_reranker(reranker)
        .rerank_hits("query", hits.clone(), 4)
        .await;
    assert_eq!(
        result.iter().map(|(hit, _)| hit.id).collect::<Vec<_>>(),
        original_ids
    );

    let malformed = vec![
        sessionmunch_llm::RerankScore {
            id: hits[0].0.id.to_string(),
            relevance: 0.0,
        },
        sessionmunch_llm::RerankScore {
            id: hits[0].0.id.to_string(),
            relevance: 1.0,
        },
        sessionmunch_llm::RerankScore {
            id: "unknown-page-version".into(),
            relevance: 1.0,
        },
        sessionmunch_llm::RerankScore {
            id: hits[3].0.id.to_string(),
            relevance: f32::NAN,
        },
    ];
    let (reranker, _, _) = stub_reranker(StubRerankOutcome::Scores(malformed), Duration::ZERO);
    let result = server
        .clone()
        .with_reranker(reranker)
        .rerank_hits("query", hits.clone(), 4)
        .await;
    assert_eq!(
        result.iter().map(|(hit, _)| hit.id).collect::<Vec<_>>(),
        original_ids
    );

    let (reranker, _, _) = stub_reranker(StubRerankOutcome::Fail, Duration::ZERO);
    let result = server
        .clone()
        .with_reranker(reranker)
        .rerank_hits("query", hits.clone(), 4)
        .await;
    assert_eq!(
        result.iter().map(|(hit, _)| hit.id).collect::<Vec<_>>(),
        original_ids
    );

    let (reranker, calls, _) = stub_reranker(StubRerankOutcome::Reverse, Duration::from_millis(50));
    let result = server
        .with_reranker(reranker)
        .rerank_hits_with_timeout("query", hits, 4, Duration::from_millis(1))
        .await;
    assert_eq!(
        result.iter().map(|(hit, _)| hit.id).collect::<Vec<_>>(),
        original_ids
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reranker_degrades_without_calling_provider_when_saturated() {
    let (_tmp, _store, server, _ws, _proj) = setup_server().await;
    let (reranker, calls, _) = stub_reranker(StubRerankOutcome::Reverse, Duration::ZERO);
    let server = server.with_reranker(reranker);
    let _permits: Vec<_> = (0..RERANK_MAX_IN_FLIGHT)
        .map(|_| server.rerank_gate.clone().try_acquire_owned().unwrap())
        .collect();
    let hits = rerank_test_hits(4);
    let original_ids: Vec<PageId> = hits.iter().map(|(hit, _)| hit.id).collect();

    let result = server.rerank_hits("query", hits, 4).await;

    assert_eq!(
        result.iter().map(|(hit, _)| hit.id).collect::<Vec<_>>(),
        original_ids
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn multi_scope_query_invokes_the_reranker_once_after_fusion() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: other,
            path: PagePath::new("foo.md").unwrap(),
            title: "Other".into(),
            body: "Karpathy also says compile durable context.".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let (reranker, calls, candidate_counts) =
        stub_reranker(StubRerankOutcome::Reverse, Duration::ZERO);
    let server = server.with_reranker(reranker);

    let response = call_tool_json(
        server
            .memory_query(
                Parameters(QueryArgs {
                    query: "Karpathy".into(),
                    limit: Some(10),
                    project: None,
                    workspace: None,
                    scopes: vec![
                        MemoryScopeArg {
                            workspace: "default".into(),
                            project: "scratch".into(),
                        },
                        MemoryScopeArg {
                            workspace: "default".into(),
                            project: "other".into(),
                        },
                    ],
                    global: None,
                    include_expired: None,
                    explain: Some(true),
                    as_of: None,
                    unit: None,
                    parent_expansion: None,
                    content_budget: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(response["hits"].as_array().unwrap().len(), 2);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(*candidate_counts.lock().unwrap(), vec![2]);
    assert_ne!(proj, other);
}

#[tokio::test]
async fn global_query_does_not_invoke_the_project_reranker() {
    let (_tmp, _store, server, _ws, _proj) = setup_server().await;
    let (reranker, calls, _) = stub_reranker(StubRerankOutcome::Reverse, Duration::ZERO);
    let server = server.with_reranker(reranker);

    let response = call_tool_json(
        server
            .memory_query(
                Parameters(QueryArgs {
                    query: "Karpathy".into(),
                    limit: Some(10),
                    project: None,
                    workspace: None,
                    scopes: Vec::new(),
                    global: Some(true),
                    include_expired: None,
                    explain: None,
                    as_of: None,
                    unit: None,
                    parent_expansion: None,
                    content_budget: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );

    assert_eq!(response["global_hits"].as_array().unwrap().len(), 1);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

async fn insert_test_observation(
    store: &Store,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    title: &str,
    body: &str,
) {
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id,
            project_id,
            agent_kind: AgentKind::OpenCode,
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
                workspace_id,
                project_id,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: title.into(),
                body: body.into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
}

const MCP_TOOL_NAMES: &[&str] = &[
    "memory_query",
    "memory_recent",
    "memory_status",
    "memory_briefing",
    "memory_explore",
    "memory_handoff_accept",
    "memory_handoff_begin",
    "memory_handoff_cancel",
    "memory_handoff_list",
    "memory_consolidate",
    "memory_auto_improve",
    "memory_write_page",
    "memory_read_page",
    "memory_read_session_observations",
    "memory_search_events",
    "memory_delete_page",
    "memory_feedback",
    "memory_lint",
    "memory_forget_sweep",
    "memory_install_self_routing",
];

const DETAILED_ROUTING_TOOL_NAMES: &[&str] = &[
    "memory_query",
    "memory_recent",
    "memory_status",
    "memory_briefing",
    "memory_explore",
    "memory_handoff_accept",
    "memory_handoff_begin",
    "memory_handoff_cancel",
    "memory_handoff_list",
    "memory_consolidate",
    "memory_auto_improve",
    "memory_write_page",
    "memory_read_page",
    "memory_read_session_observations",
    "memory_delete_page",
    "memory_feedback",
    "memory_lint",
    "memory_forget_sweep",
];

#[test]
fn actor_key_uses_memory_session_header() {
    let mut parts = test_parts_default();
    parts.headers.insert(
        "x-memory-actor-session-id",
        axum::http::HeaderValue::from_static("hook-session"),
    );

    let actor = SessionMunchServer::actor_key_from_parts(Some(&parts));

    assert_eq!(actor.user, None);
    assert_eq!(actor.session_id.as_deref(), Some("hook-session"));
}

#[test]
fn actor_key_accepts_standard_mcp_session_header() {
    let mut parts = test_parts_default();
    parts.headers.insert(
        "mcp-session-id",
        axum::http::HeaderValue::from_static("mcp-session"),
    );

    let actor = SessionMunchServer::actor_key_from_parts(Some(&parts));

    assert_eq!(actor.user, None);
    assert_eq!(actor.session_id.as_deref(), Some("mcp-session"));
}

#[test]
fn actor_key_prefers_middleware_context_over_headers() {
    let mut parts = test_parts_default();
    parts.headers.insert(
        "x-memory-actor-session-id",
        axum::http::HeaderValue::from_static("header-session"),
    );
    parts.headers.insert(
        "mcp-session-id",
        axum::http::HeaderValue::from_static("mcp-session"),
    );
    parts.extensions.insert(sessionmunch_core::ActorContext {
        user: Some("alice".into()),
        session_id: Some("context-session".into()),
        ..sessionmunch_core::ActorContext::default()
    });

    let actor = SessionMunchServer::actor_key_from_parts(Some(&parts));

    // The map is keyed by the QUALIFIED identity — the same storage key
    // the hook ingress publishes under — never the raw username.
    assert_eq!(actor.user.as_deref(), Some("user:alice"));
    assert_eq!(actor.session_id.as_deref(), Some("context-session"));
}

#[test]
fn actor_key_uses_issuer_qualified_subject() {
    let mut parts = test_parts_default();
    parts.extensions.insert(sessionmunch_core::ActorContext {
        user: Some("display-name".into()),
        issuer: Some("https://idp.example".into()),
        sub: Some("subject-123".into()),
        ..sessionmunch_core::ActorContext::default()
    });

    let actor = SessionMunchServer::actor_key_from_parts(Some(&parts));

    assert_eq!(
        actor.user.as_deref(),
        Some("oidc:19:https://idp.examplesubject-123")
    );
}

#[tokio::test]
async fn server_constructs_with_tool_router() {
    let (_tmp, _store, _server, _ws, _pj) = setup_server().await;
}

#[tokio::test]
async fn prompts_cover_every_registered_mcp_tool() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let actual_tools: BTreeSet<String> = server
        .tool_router
        .list_all()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    let expected_tools: BTreeSet<String> = MCP_TOOL_NAMES
        .iter()
        .map(|tool| (*tool).to_string())
        .collect();
    assert_eq!(
        actual_tools, expected_tools,
        "MCP_TOOL_NAMES must match the registered tool router set"
    );

    for tool in &actual_tools {
        assert!(
            MEMORY_INSTRUCTIONS.contains(tool.as_str()),
            "MCP handshake instructions omit {tool}"
        );
    }

    let installed = installed_sessionmunch_prompt_surface();
    for tool in &actual_tools {
        assert!(
            installed.contains(tool.as_str()),
            "installed snippet and managed skills omit {tool}"
        );
    }
}

#[test]
fn snippet_keeps_always_loaded_invariants() {
    let snippet = sessionmunch_core::SNIPPET_BODY;
    assert!(snippet.contains("Long-term memory (sessionmunch)"));
    assert!(snippet.contains("Choose project scope"));
    assert!(
        snippet.contains("Session-aware MCP clients")
            && snippet.contains("Static MCP clients")
            && snippet.contains("must pass `workspace` and")
            && snippet.contains("`project` together on every project-scoped call"),
        "snippet must distinguish session-aware and static-client scope routing"
    );
    assert!(
        snippet.contains("nearest\n  `.sessionmunch.toml`")
            && snippet.contains("never rely on the server's last active project"),
        "snippet must require exact, repository-owned scope names"
    );
    assert!(
        snippet.contains("`global=true` must omit") && snippet.contains("`scope: \"global\"`"),
        "snippet must preserve global-mode scope exceptions"
    );
    assert!(
        snippet.contains("Lifecycle hooks already capture"),
        "snippet must keep automatic lifecycle capture guidance"
    );
    assert!(
        snippet.contains("durable")
            && snippet.contains("explicitly asks")
            && (snippet.contains("permanent") || snippet.contains("permanently")),
        "snippet must say durable writes require an explicit user request"
    );
    assert!(
        snippet.contains("Agent Skills") && snippet.contains("installed"),
        "snippet must route detailed guidance through installed Agent Skills"
    );
    assert!(
        snippet.contains("canonical agent instruction file"),
        "snippet must keep canonical project-rule placement guidance"
    );
    assert!(
        snippet.contains("memory_install_self_routing")
            && snippet.contains("sessionmunch install-instructions"),
        "snippet must preserve refresh/install guidance"
    );
    let refresh_guidance = snippet
        .split("### Refreshing this snippet")
        .nth(1)
        .expect("snippet must keep refresh guidance");
    assert!(
        refresh_guidance.contains("managed_skills")
            && refresh_guidance.contains("target_hints")
            && refresh_guidance.contains("relative_path"),
        "snippet must tell agents to refresh managed skill files"
    );
    assert!(
        snippet.contains("start/end HTML-comment markers")
            && sessionmunch_core::full_block().contains(sessionmunch_core::MARKER_START)
            && sessionmunch_core::full_block().contains(sessionmunch_core::MARKER_END),
        "snippet must preserve marker replacement guidance"
    );
}

#[test]
fn routing_prompt_surfaces_share_the_client_aware_scope_contract() {
    let installed = installed_sessionmunch_prompt_surface();
    for (label, prompt) in [
        ("MCP handshake instructions", MEMORY_INSTRUCTIONS),
        ("installed routing", installed.as_str()),
    ] {
        for required in [
            "Session-aware MCP clients",
            "Static MCP clients",
            "must pass `workspace`",
            "`project` together on every project-scoped call",
            "nearest `.sessionmunch.toml`",
            "server's last active project",
            "`global=true`",
            "`scope: \"global\"`",
        ] {
            assert!(
                prompt.contains(required),
                "{label} is missing scope guidance: {required}"
            );
        }
        for contradictory in [
            "Do NOT pass `project`, `workspace`, or `cwd`",
            "together only when",
            "Default to the current project",
        ] {
            assert!(
                !prompt.contains(contradictory),
                "{label} contains contradictory scope guidance: {contradictory}"
            );
        }
    }
}

#[test]
fn snippet_omits_detailed_tool_routing_table() {
    let snippet = sessionmunch_core::SNIPPET_BODY;
    assert!(!snippet.contains("### When to reach for each tool"));
    assert!(!snippet.contains("| User says / situation | Tool |"));
    for tool in DETAILED_ROUTING_TOOL_NAMES {
        assert!(
            !snippet.contains(tool),
            "slim snippet must leave detailed {tool} routing to managed skills"
        );
    }
}

#[test]
fn installed_prompt_surface_routes_grok_to_agents_md() {
    let installed = installed_sessionmunch_prompt_surface();
    assert!(
        installed.contains("Grok Build CLI") && installed.contains("Grok -> `AGENTS.md`"),
        "installed snippet and managed skills must route Grok to AGENTS.md"
    );
}
#[test]
fn prompts_separate_briefing_from_handoff_lifecycle() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        let lower = prompt.to_ascii_lowercase();
        assert!(
            prompt.contains("memory_briefing") && lower.contains("read-only"),
            "{label} must say briefing is read-only"
        );
        assert!(
            prompt.contains("memory_handoff_begin")
                && (lower.contains("session-end")
                    || (lower.contains("ending") && lower.contains("session")))
                && (lower.contains("do not use") || lower.contains("do **not** use"))
                && lower.contains("status")
                && lower.contains("briefing"),
            "{label} must make handoff-begin session-end only and reject status/briefing use"
        );
        assert!(
            prompt.contains("memory_handoff_list")
                && lower.contains("read-only")
                && (lower.contains("does not claim")
                    || lower.contains("without claiming")
                    || lower.contains("not claim")),
            "{label} must expose inspect-without-claim list as read-only"
        );
        assert!(
            prompt.contains("memory_handoff_cancel") && prompt.contains("handoff_id"),
            "{label} must expose exact-id cleanup for mistaken handoffs"
        );
    });
}
#[test]
fn prompts_teach_cross_project_search_strategy() {
    // Regression: a single-project miss must not read as "never recorded".
    // Both surfaces must point the agent at `scopes` **and** at
    // `global=true` (the two broadening modes), warn that query returns
    // snippets (not full page bodies), and NOT contain the contradictory
    // legacy "no global mode" phrasing that briefly shipped in #56.
    // (Learned the hard way when cluster-access info lived in a sibling
    // `infra` project.)
    assert_detailed_prompt_surfaces(|label, prompt| {
        assert!(
            prompt.contains("scopes"),
            "{label} must teach broadening via `scopes`"
        );
        assert!(
            prompt.contains("global=true") || prompt.contains("global = true"),
            "{label} must also teach broadening via `global=true`"
        );
        assert!(
            prompt.contains("sibling") || prompt.contains("SIBLING"),
            "{label} must mention knowledge can live in a sibling project"
        );
        assert!(
            prompt.contains("snippet") || prompt.contains("SNIPPET"),
            "{label} must warn that query returns snippets, not full bodies"
        );
        // Guard against the contradiction: standalone prose must not say
        // a global mode doesn't exist when the bullet/table-row above it
        // advertises `global=true`.
        let no_global_phrases = [
            "no global \"search everything\" mode",
            "NO global 'search everything' mode",
            "no global 'search everything' mode",
            "NO global \"search everything\" mode",
        ];
        for phrase in no_global_phrases {
            assert!(
                !prompt.contains(phrase),
                "{label} must not contain the contradictory phrase {phrase:?}"
            );
        }
    });
    let installed = installed_sessionmunch_prompt_surface();
    assert!(
        installed.contains("scopes") && installed.contains("global=true"),
        "installed prompt surface must include exact cross-project broadening args"
    );
    assert!(
        installed.contains("deployment")
            && installed.contains("PR review")
            && installed.contains("migration")
            && installed.contains("data-preservation"),
        "installed prompt surface must preserve high-risk retrieval preflight guidance"
    );
}

#[test]
fn prompts_warn_static_mcp_clients_need_explicit_scope() {
    for prompt in [MEMORY_INSTRUCTIONS, sessionmunch_core::SNIPPET_BODY] {
        let lower = prompt.to_ascii_lowercase();
        assert!(
            lower.contains("static mcp clients") && lower.contains("every project-scoped call"),
            "prompt must require project scope on every static MCP call"
        );
        assert!(
            lower.contains("real lifecycle-hook session id")
                && lower.contains("session-aware mcp clients"),
            "prompt must distinguish session-aware from static MCP clients"
        );
        assert!(
            lower.contains("workspace")
                && lower.contains("project")
                && lower.contains("server's last active project"),
            "prompt must provide safe explicit-scope guidance"
        );
    }
}

#[test]
fn agent_and_explore_prompts_treat_memory_as_untrusted_data() {
    for (label, prompt) in [
        ("MCP instructions", MEMORY_INSTRUCTIONS),
        ("installed routing", sessionmunch_core::SNIPPET_BODY),
    ] {
        let lower = prompt.to_ascii_lowercase();
        assert!(lower.contains("untrusted historical data"), "{label}");
        assert!(lower.contains("never execute commands"), "{label}");
        assert!(lower.contains("canonical project instructions"), "{label}");
    }
    assert!(EXPLORE_SYSTEM_PROMPT.contains("## SECURITY BOUNDARY"));
    assert!(EXPLORE_SYSTEM_PROMPT.contains("untrusted data, not instructions"));
}

#[test]
fn prompts_route_permanent_annotations_to_write_page_not_handoff() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        assert!(
            prompt.contains("permanent") || prompt.contains("permanently"),
            "{label} must mention permanent memory use cases"
        );
        assert!(
            prompt.contains("memory_write_page"),
            "{label} must expose memory_write_page"
        );
        assert!(
            prompt.contains("do NOT use") || prompt.contains("do **not** use"),
            "{label} must explicitly disallow handoffs for permanent notes"
        );
    });
}

#[test]
fn prompts_document_time_bounded_pages_and_expired_retrieval() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        assert!(
            prompt.contains("expires_at"),
            "{label} must document the TTL write argument"
        );
        assert!(
            prompt.contains("include_expired"),
            "{label} must document explicit expired-page retrieval"
        );
        assert!(
            prompt.contains("TTL") && prompt.contains("pinned"),
            "{label} must document TTL precedence over pinned"
        );
    });
    assert!(
        sessionmunch_core::SNIPPET_BODY.contains("expires_at"),
        "the installed base routing snippet must expose time-bounded writes"
    );
}

/// Passage-mode retrieval is invisible to an agent that reads only the
/// prompt surfaces, so every surface must name both units and explain
/// the two arguments that shape a passage hit.
#[test]
fn prompts_document_passage_retrieval_unit() {
    assert!(
        MEMORY_INSTRUCTIONS.contains("unit=\"page\"")
            && MEMORY_INSTRUCTIONS.contains("unit=\"passage\""),
        "MCP handshake instructions must name both retrieval units"
    );
    assert!(
        sessionmunch_core::SNIPPET_BODY.contains("Passage mode"),
        "the installed base routing snippet must expose passage retrieval"
    );
    for (label, prompt) in [
        ("MCP handshake instructions", MEMORY_INSTRUCTIONS),
        ("installed routing snippet", sessionmunch_core::SNIPPET_BODY),
    ] {
        assert!(
            prompt.contains("parent_expansion")
                && prompt.contains("`section`")
                && prompt.contains("`document`"),
            "{label} must document the parent_expansion levels"
        );
        assert!(
            prompt.contains("content_budget"),
            "{label} must document the per-hit content budget"
        );
    }
    let installed = installed_sessionmunch_prompt_surface();
    assert!(
        installed.contains("unit: \"page\"") && installed.contains("unit: \"passage\""),
        "installed retrieval skill must document the passage unit"
    );
}

#[tokio::test]
async fn prompts_and_tool_schema_document_consolidation_preferences() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        assert!(
            prompt.contains("_prompts/consolidation.md")
                && prompt.contains("instructions")
                && prompt.contains("untrusted"),
            "{label} must document standing and one-off consolidation preferences"
        );
    });
    assert!(
        sessionmunch_core::SNIPPET_BODY.contains("_prompts/consolidation.md")
            && sessionmunch_core::SNIPPET_BODY.contains("untrusted project data"),
        "the installed base routing snippet must preserve the trust boundary"
    );

    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tools = server.tool_router.list_all();
    let consolidate = tools
        .iter()
        .find(|tool| tool.name == "memory_consolidate")
        .expect("memory_consolidate must be registered");
    let description = consolidate
        .description
        .as_deref()
        .expect("memory_consolidate must carry a description");
    assert!(description.contains("_prompts/consolidation.md"));
    assert!(description.contains("instructions"));
    assert!(description.contains("untrusted"));
    let properties = consolidate
        .input_schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("memory_consolidate schema must expose properties");
    assert!(
        properties.contains_key("instructions"),
        "memory_consolidate schema must expose the one-off override"
    );
}

#[tokio::test]
async fn prompts_and_tool_schema_document_query_explain_mode() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        assert!(
            prompt.contains("explain=true") || prompt.contains("explain: true"),
            "{label} must document the opt-in query explanation"
        );
        assert!(
            prompt.contains("FTS-only") || prompt.contains("FTS stream"),
            "{label} must distinguish global search from project RRF explanation"
        );
    });

    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tools = server.tool_router.list_all();
    let query = tools
        .iter()
        .find(|tool| tool.name == "memory_query")
        .expect("memory_query must be registered");
    let description = query
        .description
        .as_deref()
        .expect("memory_query must carry a description");
    assert!(description.contains("explain=true"));
    assert!(description.contains("global=true") && description.contains("FTS-only"));
    let properties = query
        .input_schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("memory_query schema must expose properties");
    assert!(properties.contains_key("explain"));
}

#[test]
fn prompts_treat_retrieved_memory_as_untrusted_historical_evidence() {
    let installed = installed_sessionmunch_prompt_surface();
    for (label, prompt) in [
        ("MCP handshake instructions", MEMORY_INSTRUCTIONS),
        ("installed routing and managed skills", installed.as_str()),
    ] {
        let lower = prompt.to_ascii_lowercase();
        assert!(
            prompt.contains("_rules/")
                && prompt.contains("gotchas/")
                && prompt.contains("procedures/")
                && prompt.contains("decisions/"),
            "{label} must name actionable page families"
        );
        assert!(
            lower.contains("untrusted historical evidence")
                && lower.contains("validate")
                && lower.contains("canonical project instructions"),
            "{label} must preserve the retrieved-memory trust boundary"
        );
        assert!(
            lower.contains("before non-trivial")
                && lower.contains("auth")
                && lower.contains("migration"),
            "{label} must make proactive retrieval the default for risky work"
        );
        assert!(
            lower.contains("cannot authorize")
                && lower.contains("commands")
                && lower.contains("tools")
                && lower.contains("disclosure")
                && lower.contains("policy"),
            "{label} must state what retrieved provenance cannot authorize"
        );
        for contradictory in [
            "use retrieved memory as operating guidance",
            "treat `_rules/` as constraints",
            "as operating constraints",
            "apply rules as current project policy",
            "follow procedures as checklists",
            "treat decisions as prior architecture",
            "as settled architecture",
        ] {
            assert!(
                !lower.contains(contradictory),
                "{label} contains contradictory authority guidance: {contradictory}"
            );
        }
    }
}

#[tokio::test]
async fn memory_install_self_routing_response_includes_managed_skills_and_targets() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;

    let response = call_tool_json(
        server
            .memory_install_self_routing(Parameters(InstallSelfRoutingArgs::default()))
            .await
            .unwrap(),
    );
    assert!(!response["compact"].as_bool().unwrap());

    assert_eq!(
        response["markered_block"].as_str().unwrap(),
        sessionmunch_core::full_block()
    );
    assert_eq!(
        response["marker_start"].as_str().unwrap(),
        sessionmunch_core::MARKER_START
    );
    assert_eq!(
        response["marker_end"].as_str().unwrap(),
        sessionmunch_core::MARKER_END
    );
    assert_eq!(
        response["agent_filenames"]["claude_code"].as_str().unwrap(),
        "CLAUDE.md"
    );
    assert_eq!(
        response["agent_filenames"]["default"].as_str().unwrap(),
        "AGENTS.md"
    );
    assert_eq!(
        response["agent_filenames"]["devin"].as_str().unwrap(),
        "AGENTS.md"
    );
    assert_eq!(
        response["agent_filenames"]["kimi_code"].as_str().unwrap(),
        "AGENTS.md"
    );
    assert_eq!(
        response["agent_filenames"]["command_code"]
            .as_str()
            .unwrap(),
        "AGENTS.md"
    );
    // Proposed symmetrically alongside the devin assertion above:
    // upstream added "zero" to this same payload (issue #156) without a
    // matching assertion. Not validated against a live Zero agent in
    // this environment — for the Zero team to confirm "AGENTS.md" is
    // still the intended target file before relying on this.
    assert_eq!(
        response["agent_filenames"]["zero"].as_str().unwrap(),
        "AGENTS.md"
    );

    let managed_skills = response["managed_skills"]
        .as_array()
        .expect("managed_skills must be an array");
    assert_eq!(
        managed_skills.len(),
        sessionmunch_core::routing_skills::MANAGED_SKILLS.len()
    );
    for expected in sessionmunch_core::routing_skills::MANAGED_SKILLS {
        let skill = managed_skills
            .iter()
            .find(|skill| skill["name"].as_str() == Some(expected.name))
            .unwrap_or_else(|| panic!("missing managed skill {}", expected.name));
        assert_eq!(skill["description"].as_str().unwrap(), expected.description);
        assert_eq!(
            skill["relative_path"].as_str().unwrap(),
            expected.relative_path
        );
        assert_eq!(skill["content"].as_str().unwrap(), expected.content);
        assert!(
            skill["content"]
                .as_str()
                .unwrap()
                .contains(sessionmunch_core::routing_skills::MANAGED_MARKER),
            "managed skill {} must include the ownership marker",
            expected.name
        );
    }
    let routing_install_skill = managed_skills
        .iter()
        .find(|skill| skill["name"].as_str() == Some("sessionmunch-routing-install"))
        .expect("routing-install skill must be included in the install payload");
    let routing_install_content = routing_install_skill["content"]
        .as_str()
        .expect("routing-install skill content must be text");
    assert!(
        routing_install_content.contains("target_hints")
            && routing_install_content.contains(".grok/skills")
            && routing_install_content.contains("$GROK_HOME/skills")
            && routing_install_content.contains("~/.grok/skills"),
        "routing-install skill must treat target_hints as authoritative and include Grok roots"
    );

    assert_eq!(
        response["target_hints"]["project"]["claude_code"]
            .as_str()
            .unwrap(),
        ".claude/skills"
    );
    assert_eq!(
        response["target_hints"]["project"]["agents"]
            .as_str()
            .unwrap(),
        ".agents/skills"
    );
    assert_eq!(
        response["target_hints"]["project"]["devin"]
            .as_str()
            .unwrap(),
        ".devin/skills"
    );
    assert_eq!(
        response["target_hints"]["project"]["grok"]
            .as_str()
            .unwrap(),
        ".grok/skills"
    );
    assert_eq!(
        response["target_hints"]["global"]["claude_code"]
            .as_str()
            .unwrap(),
        "~/.claude/skills"
    );
    assert_eq!(
        response["target_hints"]["global"]["agents"]
            .as_str()
            .unwrap(),
        "~/.agents/skills"
    );
    assert_eq!(
        response["target_hints"]["global"]["devin"]["windows"]
            .as_str()
            .unwrap(),
        "%APPDATA%\\devin\\skills"
    );
    assert_eq!(
        response["target_hints"]["global"]["devin"]["non_windows"]
            .as_str()
            .unwrap(),
        "~/.devin/skills"
    );
    assert_eq!(
        response["target_hints"]["global"]["grok"].as_str().unwrap(),
        "$GROK_HOME/skills (default: ~/.grok/skills)"
    );
    assert_eq!(
        response["agent_filenames"]["grok"].as_str().unwrap(),
        "AGENTS.md"
    );

    let notes = response["notes"]
        .as_array()
        .expect("notes must remain an array")
        .iter()
        .map(|note| note.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(notes.contains(sessionmunch_core::routing_skills::MANAGED_MARKER));
    assert!(notes.contains("unmanaged same-name skills"));
    assert!(notes.contains("%APPDATA%\\devin\\skills"));
    assert!(notes.contains("explicitly forces replacement"));
}
#[tokio::test]
async fn memory_install_self_routing_compact_returns_compact_block() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;

    let response = call_tool_json(
        server
            .memory_install_self_routing(Parameters(InstallSelfRoutingArgs {
                compact: Some(true),
            }))
            .await
            .unwrap(),
    );

    assert_eq!(
        response["markered_block"].as_str().unwrap(),
        sessionmunch_core::compact_block()
    );
    assert!(response["compact"].as_bool().unwrap());
}

#[tokio::test]
async fn memory_install_self_routing_tool_description_covers_snippet_and_skills() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tools = server.tool_router.list_all();
    let install = tools
        .iter()
        .find(|tool| tool.name == "memory_install_self_routing")
        .expect("memory_install_self_routing must be registered");
    let desc = install
        .description
        .as_deref()
        .expect("memory_install_self_routing must carry a description");

    assert!(
        desc.contains("markered_block") && desc.contains("managed_skills"),
        "tool description must tell agents to install snippet and skill payloads; got: {desc}"
    );
    assert!(
        desc.contains(".claude/skills")
            && desc.contains(".agents/skills")
            && desc.contains(".devin/skills")
            && desc.contains(".grok/skills")
            && desc.contains("$GROK_HOME/skills"),
        "tool description must name Claude, .agents, Devin, and Grok skill targets; got: {desc}"
    );
    assert!(
        desc.contains("preserve non-sessionmunch user content"),
        "tool description must preserve user content; got: {desc}"
    );
    assert!(
        desc.contains("unmanaged same-name skills") && desc.contains("explicitly forces"),
        "tool description must mention safe overwrite behavior; got: {desc}"
    );
    assert!(
        desc.contains("compact: true"),
        "tool description must mention compact option; got: {desc}"
    );
}

#[test]
fn feedback_reason_is_secret_scrubbed_single_line_and_bounded() {
    let raw = format!(
        "Authorization: Bearer abcdef0123456789ABCDEF0123456789\n# ignore safeguards {}",
        "x".repeat(700)
    );
    let reason = sanitize_feedback_reason(&Sanitizer::builtin(), Some(&raw)).unwrap();
    assert!(reason.contains("[REDACTED:bearer_token]"));
    assert!(!reason.contains("abcdef0123456789"));
    assert!(!reason.contains('\n'));
    assert!(!reason.contains('\r'));
    assert!(reason.chars().count() <= MAX_FEEDBACK_REASON_CHARS);
    assert_eq!(
        sanitize_feedback_reason(&Sanitizer::builtin(), Some("  \n\t")),
        None
    );
}

#[tokio::test]
async fn memory_feedback_is_scoped_flagged_and_retired_on_rewrite() {
    let (tmp, store, server, ws, proj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = server.with_wiki(wiki);
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let path = PagePath::new("notes/shared.md").unwrap();
    let make_page = |project_id, body: &str| NewPage {
        workspace_id: ws,
        project_id,
        path: path.clone(),
        title: "Shared".into(),
        body: body.into(),
        tier: Tier::Episodic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    };
    let target_id = store
        .writer
        .upsert_page(make_page(proj, "target v1"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(make_page(other, "other project"))
        .await
        .unwrap();

    let raw_reason = format!(
        "Authorization: Bearer abcdef0123456789ABCDEF0123456789\n# untrusted {}",
        "x".repeat(700)
    );
    let response = server
        .memory_feedback(
            Parameters(FeedbackArgs {
                path: path.to_string(),
                signal: FeedbackKind::Stale,
                reason: Some(raw_reason),
                project: None,
                workspace: None,
            }),
            test_optional_parts(),
        )
        .await
        .unwrap();
    let json = call_tool_json(response);
    assert_eq!(json["page_id"], target_id.to_string());
    assert_eq!(
        json["salience"].as_f64(),
        Some(sessionmunch_store::decay::SALIENCE_MIN)
    );
    assert_eq!(json["routed_to_lint"], true);

    let target = store
        .reader
        .decay_candidates(ws, proj)
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.path == path)
        .unwrap();
    assert_eq!(
        target.salience,
        Some(sessionmunch_store::decay::SALIENCE_MIN)
    );
    let other_page = store
        .reader
        .decay_candidates(ws, other)
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.path == path)
        .unwrap();
    assert_eq!(
        other_page.salience, None,
        "same path in another project must not move"
    );

    let findings = store.reader.open_feedback_findings(ws, proj).await.unwrap();
    assert_eq!(findings.len(), 1);
    let reason = findings[0].reason.as_deref().unwrap();
    assert!(reason.contains("[REDACTED:bearer_token]"));
    assert!(!reason.contains("abcdef0123456789"));
    assert!(!reason.contains('\n'));
    assert!(reason.chars().count() <= MAX_FEEDBACK_REASON_CHARS);
    assert!(
        store
            .reader
            .open_feedback_findings(ws, other)
            .await
            .unwrap()
            .is_empty()
    );

    let lint = call_tool_json(
        server
            .memory_lint(
                Parameters(LintArgs {
                    dry_run: Some(true),
                    no_llm: Some(true),
                    project: None,
                    workspace: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    let feedback_finding = lint["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["kind"] == "feedback_flagged")
        .expect("stale feedback must appear in memory_lint");
    assert_eq!(feedback_finding["pages"][0], path.to_string());
    let lint_message = feedback_finding["message"].as_str().unwrap();
    assert!(lint_message.contains("[REDACTED:bearer_token]"));
    assert!(!lint_message.contains("abcdef0123456789"));
    assert!(!lint_message.contains('\n'));

    let missing = server
        .memory_feedback(
            Parameters(FeedbackArgs {
                path: path.to_string(),
                signal: FeedbackKind::Helpful,
                reason: None,
                project: Some("missing-project".into()),
                workspace: None,
            }),
            test_optional_parts(),
        )
        .await
        .expect_err("an unknown explicit project must fail closed");
    assert!(
        missing.to_string().contains("not found"),
        "unexpected error: {missing}"
    );
    assert_eq!(
        store
            .reader
            .open_feedback_findings(ws, proj)
            .await
            .unwrap()
            .len(),
        1,
        "failed explicit scope must not fall back to the current project"
    );

    let new_id = store
        .writer
        .upsert_page(make_page(proj, "target v2"))
        .await
        .unwrap();
    assert_ne!(new_id, target_id);
    assert!(
        store
            .reader
            .open_feedback_findings(ws, proj)
            .await
            .unwrap()
            .is_empty(),
        "rewriting the page must retire flags attached to the old version"
    );
    let lint = call_tool_json(
        server
            .memory_lint(
                Parameters(LintArgs {
                    dry_run: Some(true),
                    no_llm: Some(true),
                    project: None,
                    workspace: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    assert!(
        lint["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["kind"] != "feedback_flagged"),
        "memory_lint must retire feedback tied to a superseded page version"
    );
}

#[tokio::test]
async fn memory_feedback_schema_exposes_the_signal_enum() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tool = server
        .tool_router
        .list_all()
        .into_iter()
        .find(|tool| tool.name == "memory_feedback")
        .expect("memory_feedback must be registered");
    let schema = serde_json::to_value(&tool.input_schema).unwrap();
    let signal = &schema["properties"]["signal"];
    let signal_schema = signal["$ref"]
        .as_str()
        .and_then(|reference| reference.strip_prefix("#/$defs/"))
        .map_or(signal, |name| &schema["$defs"][name]);
    for expected in ["helpful", "not_helpful", "stale", "wrong"] {
        let in_enum = signal_schema["enum"]
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value == expected));
        let in_one_of = signal_schema["oneOf"]
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value["const"] == expected));
        assert!(in_enum || in_one_of, "missing `{expected}` in {schema}");
    }
}

#[tokio::test]
async fn project_scoped_tool_schemas_expose_the_static_client_contract() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let mut project_scoped = 0;

    for tool in server.tool_router.list_all() {
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        let properties = &schema["properties"];
        if properties.get("project").is_none() || properties.get("workspace").is_none() {
            continue;
        }
        project_scoped += 1;
        let project_description = properties["project"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            project_description.contains("Static MCP clients must pass it together"),
            "{} project schema is missing static-client scope guidance: {}",
            tool.name,
            project_description
        );
        assert!(
            !project_description.contains("Omit unless the user explicitly names"),
            "{} project schema restored contradictory scope guidance: {}",
            tool.name,
            project_description
        );
        let tool_description = tool.description.as_deref().unwrap_or_default();
        assert!(
            !tool_description.contains("Omit project/workspace unless")
                && !tool_description.contains("together only when"),
            "{} tool description contains contradictory scope guidance: {}",
            tool.name,
            tool_description
        );
    }

    assert!(project_scoped > 0, "expected project-scoped tools");
}

#[tokio::test]
async fn prompts_expose_auto_improve_as_auto_approval_with_manual_opt_in() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        let lower = prompt.to_ascii_lowercase();
        assert!(prompt.contains("memory_auto_improve"));
        assert!(
            lower.contains("applies validated")
                || (lower.contains("approval") && lower.contains("path")),
            "{label} must state auto-improve applies through the approval path"
        );
        assert!(
            lower.contains("require_approval") && lower.contains("pending-writes"),
            "{label} must describe the manual review opt-in"
        );
    });
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tools = server.tool_router.list_all();
    let auto_improve = tools
        .iter()
        .find(|t| t.name == "memory_auto_improve")
        .expect("memory_auto_improve must be registered");
    let desc = auto_improve
        .description
        .as_deref()
        .expect("memory_auto_improve must carry a description");
    assert!(desc.contains("apply or stage validated"));
    assert!(desc.contains("approval"));
}

#[test]
fn prompts_teach_cross_workspace_handoff_scope() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        assert!(
            prompt.contains("memory_handoff_begin") && prompt.contains("memory_handoff_accept"),
            "{label} must include handoff lifecycle tools"
        );
        assert!(
            prompt.contains("workspace") && prompt.contains("project"),
            "{label} handoff guidance must mention workspace+project scoping"
        );
        assert!(
            prompt.contains("sibling")
                && (prompt.contains("workspace/project")
                    || prompt.contains("workspace + project")
                    || prompt.contains("workspace` + `project")),
            "{label} handoff guidance must restrict explicit workspace scope to named siblings"
        );
    });
}

/// `memory_forget_sweep` is admin-gated and destructive, and the surface
/// an agent reads is the only place it can learn what the tool will
/// delete. Both surfaces must name all four passes and must not claim a
/// blanket pin exemption.
///
/// The description previously said "Semantic / procedural / pinned pages
/// are exempt" full stop, which is true of the decay pass and false of
/// the TTL pass: `sweep.rs` pushes any candidate whose `expires_at` has
/// passed into `expired` and `continue`s three lines *before* the only
/// pin check. An agent asked "is it safe to run, I have pinned pages?"
/// had no way to answer anything but yes (#485). The behaviour is
/// deliberate — an explicit expiry is a more specific instruction than a
/// pin, pinned by `lifecycle.rs` — so the surface is what had to change.
#[tokio::test]
async fn forget_sweep_surfaces_name_every_pass_and_scope_the_pin_exemption() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tools = server.tool_router.list_all();
    let sweep = tools
        .iter()
        .find(|t| t.name == "memory_forget_sweep")
        .expect("memory_forget_sweep must be registered");
    let desc = sweep
        .description
        .as_deref()
        .expect("memory_forget_sweep must carry a description");

    // The TTL pass must be named, and named as pin-overriding. This is
    // the deletion-safety claim that was wrong.
    assert!(
        desc.contains("expires_at"),
        "description must name the TTL pass trigger; got: {desc}"
    );
    assert!(
        desc.contains("REGARDLESS OF TIER OR PIN"),
        "description must state that TTL overrides a pin; got: {desc}"
    );
    // The decay pass keeps its exemption, but scoped to itself.
    assert!(
        desc.contains("only this pass exempts"),
        "the pin exemption must be scoped to the decay pass; got: {desc}"
    );
    // The third pass, so `hard_deleted` in the report is interpretable.
    assert!(
        desc.contains("hard_delete_after_days"),
        "description must name the hard-delete pass; got: {desc}"
    );
    // The fourth pass deletes raw capture, not pages, and can remove
    // millions of rows on a real install. An agent that cannot read it
    // here cannot know the tool touches observations at all — which is the
    // exact regression this test was written to stop.
    assert!(
        desc.contains("observation_retention_days"),
        "description must name the observation prune pass; got: {desc}"
    );
    assert!(
        desc.contains("consolidated") && desc.contains("still live"),
        "description must state that only consolidated sessions with a live \
             page are pruned; got: {desc}"
    );
    assert!(
        desc.contains("DISABLED"),
        "description must state that observation pruning is off by default; \
             got: {desc}"
    );
    assert!(
        !desc.contains("THREE passes"),
        "the three-pass wording must not come back now there are four; got: {desc}"
    );
    // A bare unqualified exemption sentence is exactly the regression
    // this guards, so reject the old wording verbatim.
    assert!(
        !desc.contains("Semantic / procedural / pinned pages are exempt."),
        "the unscoped pin-exemption claim must not come back; got: {desc}"
    );

    // MEMORY_INSTRUCTIONS is read before an agent ever inspects
    // `tools/list`, so it carries the same obligation.
    assert!(
        MEMORY_INSTRUCTIONS.contains("hard-deleted even when"),
        "MEMORY_INSTRUCTIONS must warn that expired pages delete despite a pin"
    );
}

/// All three prompt surfaces must steer agents toward the H1-in-body
/// convention instead of passing the `title` argument. The `title`
/// argument is a known source of `JSON parsing` errors when the LLM
/// fails to escape quotes (issue #67); routing every "remember this"
/// call through the H1 path avoids the footgun entirely.
///
/// The three surfaces - `MEMORY_INSTRUCTIONS`, the installed routing
/// surface (`SNIPPET_BODY` plus managed skills), and the per-tool
/// `#[tool(description=...)]` string surfaced via `tools/list` - are
/// independent and must stay aligned.
#[tokio::test]
async fn prompts_steer_write_page_toward_h1_title_convention() {
    assert_detailed_prompt_surfaces(|label, prompt| {
        assert!(
            prompt.contains("H1"),
            "{label} must mention the H1 title convention for memory_write_page"
        );
        assert!(
            prompt.contains("omit") || prompt.contains("Omit"),
            "{label} must tell the agent to omit the `title` argument"
        );
    });
    // The third surface: the rmcp tool description sent to clients
    // via `tools/list`. Spell-checked against the same keywords so
    // that a future edit cannot silently drop the guidance from the
    // tool the agent actually inspects when deciding how to call.
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tools = server.tool_router.list_all();
    let write_page = tools
        .iter()
        .find(|t| t.name == "memory_write_page")
        .expect("memory_write_page must be registered");
    let desc = write_page
        .description
        .as_deref()
        .expect("memory_write_page must carry a description");
    assert!(
        desc.contains("H1"),
        "tool description must mention the H1 title convention; got: {desc}"
    );
    assert!(
        desc.contains("Do NOT pass")
            || desc.contains("do NOT pass")
            || desc.contains("omit")
            || desc.contains("Omit"),
        "tool description must explicitly tell the agent to omit `title`; got: {desc}"
    );
}

/// Read tools resolve the project in the order: explicit `project`
/// arg in the active workspace, explicit `project` in the baked
/// workspace, hook-published active project, baked-in default (issue #2).
#[tokio::test]
async fn effective_ids_follows_precedence_chain() {
    let (_tmp, store, server, ws, baked) = setup_server().await;

    // Baseline: nothing published, no arg → baked-in default.
    assert_eq!(
        server
            .effective_ids_with_actor(None, &sessionmunch_core::ActorKey::default())
            .await
            .unwrap(),
        (ws, baked)
    );

    // A second real project in the same workspace.
    let other = store
        .writer
        .get_or_create_project(
            ws,
            "projeto_camera",
            Some("/home/u/projeto_camera".to_string()),
        )
        .await
        .unwrap();

    // Hook publishes it → it becomes the default for cwd-less calls.
    server.active_project.set(ws, other);
    assert_eq!(
        server
            .effective_ids_with_actor(None, &sessionmunch_core::ActorKey::default())
            .await
            .unwrap(),
        (ws, other)
    );

    // An explicit (existing) project arg wins over the active pointer.
    assert_eq!(
        server
            .effective_ids_with_actor(Some("scratch"), &sessionmunch_core::ActorKey::default())
            .await
            .unwrap(),
        (ws, baked),
        "explicit project arg should override the active pointer"
    );

    // An explicit but unknown project name fails closed instead of
    // silently falling through to the active pointer.
    let err = server
        .effective_ids_with_actor(
            Some("does-not-exist"),
            &sessionmunch_core::ActorKey::default(),
        )
        .await
        .expect_err("unknown explicit project must not fall back");
    assert!(
        err.to_string().contains("does-not-exist"),
        "error should name the missing explicit project: {err}"
    );
}

#[tokio::test]
async fn write_target_ids_defaults_workspace_to_active_project() {
    let (_tmp, store, server, baked_ws, baked_proj) = setup_server().await;

    // A second workspace — the cwd's actual workspace (e.g. "djalmajr"),
    // distinct from the server's baked "default".
    let other_ws = store
        .writer
        .get_or_create_workspace("djalmajr")
        .await
        .unwrap();
    let other_proj = store
        .writer
        .get_or_create_project(other_ws, "sessionmunch", None)
        .await
        .unwrap();
    // Hook publishes the cwd's project (in the OTHER workspace).
    server.active_project.set(other_ws, other_proj);

    // Explicit project, NO workspace → must land in the active project's
    // workspace (djalmajr) and REUSE the existing project, not recreate it
    // under the baked default.
    let (ws, proj) = server
        .write_target_ids(None, Some("sessionmunch"))
        .await
        .unwrap();
    assert_eq!(
        ws, other_ws,
        "workspace must default to the active project's, not the baked default"
    );
    assert_eq!(
        proj, other_proj,
        "must reuse djalmajr/sessionmunch, not recreate it"
    );

    // A different project name (no workspace) also lands in the cwd's workspace.
    let (ws2, _p2) = server
        .write_target_ids(None, Some("sibling"))
        .await
        .unwrap();
    assert_eq!(
        ws2, other_ws,
        "a sibling project lands in the cwd's workspace"
    );

    // Explicit workspace still overrides the active default.
    let (ws3, _p3) = server
        .write_target_ids(Some("default"), Some("sessionmunch"))
        .await
        .unwrap();
    assert_eq!(
        ws3, baked_ws,
        "explicit workspace wins over the active pointer"
    );

    // No active project published → fall back to the baked workspace.
    let fresh = SessionMunchServer::new(
        store.reader.clone(),
        store.writer.clone(),
        baked_ws,
        baked_proj,
    );
    let (ws4, _p4) = fresh
        .write_target_ids(None, Some("sessionmunch"))
        .await
        .unwrap();
    assert_eq!(
        ws4, baked_ws,
        "no active project → baked workspace is the fallback"
    );
}

#[tokio::test]
async fn write_target_ids_rejects_workspace_without_project() {
    let (_tmp, _store, server, _baked_ws, _baked_proj) = setup_server().await;

    let err = server
        .write_target_ids(Some("default"), None)
        .await
        .expect_err("workspace-only writes must fail closed");
    assert!(
        err.to_string()
            .contains("workspace and project must be provided together"),
        "error should explain the required scope pair: {err}"
    );
}

#[tokio::test]
async fn project_only_write_round_trips_with_project_only_read_in_active_workspace() {
    let (tmp, store, server, baked_ws, _baked_proj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = server.with_wiki(wiki);

    let active_ws = store
        .writer
        .get_or_create_workspace("djalmajr")
        .await
        .unwrap();
    let active_proj = store
        .writer
        .get_or_create_project(active_ws, "sessionmunch", None)
        .await
        .unwrap();
    server.active_project.set(active_ws, active_proj);
    let parts = axum::http::Request::builder()
        .uri("/mcp")
        .method("POST")
        .body(())
        .unwrap()
        .into_parts()
        .0;

    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/sibling.md".to_string(),
                body: "project-only write should use the active workspace".to_string(),
                title: Some("Sibling Note".to_string()),
                tier: None,
                tags: Vec::new(),
                pinned: false,
                project: Some("sibling".to_string()),
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts),
        )
        .await
        .unwrap();

    assert!(
        store
            .reader
            .find_project(baked_ws, "sibling".to_string())
            .await
            .unwrap()
            .is_none(),
        "project-only write must not recreate default/sibling"
    );
    let sibling_proj = store
        .reader
        .find_project(active_ws, "sibling".to_string())
        .await
        .unwrap()
        .expect("project-only write should create active-workspace sibling");
    let direct_hits = store
        .reader
        .recent_pages_for_project(active_ws, sibling_proj, 5)
        .await
        .unwrap();
    assert_eq!(
        direct_hits.len(),
        1,
        "direct read should see the written page"
    );
    assert_eq!(
        server
            .effective_ids_with_actor(Some("sibling"), &sessionmunch_core::ActorKey::default())
            .await
            .unwrap(),
        (active_ws, sibling_proj),
        "project-only read resolution should use the active workspace"
    );

    let result = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(5),
                project: Some("sibling".to_string()),
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_else(|| panic!("expected text content"));
    assert!(
        text.contains("notes/sibling.md"),
        "project-only read must find the active-workspace write:\n{text}"
    );
    assert!(
        text.contains("Sibling Note"),
        "project-only read must return the written page:\n{text}"
    );
}

/// `as_of` turns memory_query into a time-travel lookup
/// (docs/temporal.md, issue #656): entity timeline fused with
/// version-filtered FTS. A superseded version answers for the
/// instant it was valid, the current version answers for now, an
/// entity-less version answers via the FTS leg alone, and the mode
/// refuses to combine with global/scopes.
#[tokio::test]
async fn memory_query_as_of_travels_the_entity_timeline() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let mut v1 = sessionmunch_core::NewPage {
        workspace_id: ws,
        project_id: proj,
        path: sessionmunch_core::PagePath::new("notes/db.md").unwrap(),
        title: "DB".into(),
        body: "we use postgres".into(),
        tier: sessionmunch_core::Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: vec!["postgres".into()],
        evidence: Vec::new(),
    };
    store.writer.upsert_page(v1.clone()).await.unwrap();
    let between = jiff::Timestamp::now().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    v1.body = "we migrated".into();
    v1.entities = vec!["sqlite".into()];
    store.writer.upsert_page(v1).await.unwrap();

    let args = |as_of: Option<String>, global: Option<bool>| QueryArgs {
        query: "postgres".into(),
        limit: Some(5),
        project: Some("scratch".into()),
        scopes: Vec::new(),
        workspace: Some("default".into()),
        global,
        include_expired: None,
        explain: Some(true),
        as_of,
        unit: None,
        parent_expansion: None,
        content_budget: None,
    };

    // Historical instant → the superseded version answers.
    let result = server
        .memory_query(
            Parameters(args(Some(between), None)),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .unwrap()
        .text
        .clone();
    assert!(text.contains("notes/db.md"), "{text}");
    assert!(text.contains("\"entity\""), "entity stream ran: {text}");
    assert!(text.contains("\"fts\""), "FTS stream ran: {text}");
    assert!(text.contains("entity_rank"), "entity provenance: {text}");
    assert!(text.contains("fts_rank"), "FTS provenance: {text}");

    // FTS-leg end to end: "migrated" matches no entity (v2 carries
    // `sqlite`), but the live version's body says "we migrated" —
    // audit mode at now still finds it through version-filtered FTS.
    let fts_now = server
        .memory_query(
            Parameters(QueryArgs {
                query: "migrated".into(),
                limit: Some(5),
                project: Some("scratch".into()),
                scopes: Vec::new(),
                workspace: Some("default".into()),
                global: None,
                include_expired: None,
                explain: Some(true),
                as_of: Some(jiff::Timestamp::now().to_string()),
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let fts_text = fts_now
        .content
        .first()
        .and_then(|c| c.as_text())
        .unwrap()
        .text
        .clone();
    assert!(fts_text.contains("notes/db.md"), "{fts_text}");
    assert!(fts_text.contains("fts_rank"), "{fts_text}");
    assert!(!fts_text.contains("entity_rank"), "{fts_text}");

    // Same query without as_of → no postgres hit anymore... the FTS
    // stream may still match old text? No: default searches latest
    // pages only, whose body says "we migrated".
    let now_result = server
        .memory_query(
            Parameters(args(None, None)),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let now_text = now_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .unwrap()
        .text
        .clone();
    assert!(!now_text.contains("notes/db.md"), "{now_text}");

    // Refusal: as_of + global is ambiguous.
    let err = server
        .memory_query(
            Parameters(args(Some("2026-06-01T00:00:00Z".into()), Some(true))),
            OptionalParts(test_parts_default()),
        )
        .await;
    assert!(err.is_err(), "as_of+global must be refused");

    // Garbage instant is a clear error, not a silent default.
    let bad = server
        .memory_query(
            Parameters(args(Some("not-a-time".into()), None)),
            OptionalParts(test_parts_default()),
        )
        .await;
    assert!(bad.is_err());
}

#[tokio::test]
async fn memory_query_returns_hits_via_tool_method() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "karpathy".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = match result.content.first().and_then(|c| c.as_text()) {
        Some(t) => t.text.clone(),
        None => panic!("expected text content"),
    };
    assert!(text.contains("foo.md"), "expected hit; got {text}");
    assert!(!text.contains("score_details"));
    assert!(!text.contains("streams_active"));

    let explained = server
        .memory_query(
            Parameters(QueryArgs {
                query: "karpathy".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: Some(true),
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = explained.content.first().and_then(|c| c.as_text()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text.text).unwrap();
    assert_eq!(
        value["streams_active"],
        serde_json::json!(["fts", "entity", "graph"])
    );
    let details = &value["hits"][0]["score_details"];
    assert_eq!(details["fts_rank"], 1);
    assert!(details.get("vector_rank").is_none());
    assert_eq!(details["rrf"]["vector"], 0.0);
    // This project never consolidated, so no entity rows exist and the
    // fourth stream contributes nothing — the same way the vector stream
    // stays silent without an embedder.
    assert!(details.get("entity_rank").is_none());
    assert!(details.get("entity_weight").is_none());
    assert_eq!(details["rrf"]["entity"], 0.0);
    let rank = value["hits"][0]["rank"].as_f64().unwrap();
    let fused = details["fused"].as_f64().unwrap();
    let authority = details["authority"].as_f64().unwrap();
    assert!((rank + fused * authority).abs() < f64::EPSILON);
}

/// G1: `memory_query` with `unit: "passage"` returns passage-level hits
/// (not whole-page bodies) sourced from the passage index built by E1.
#[tokio::test]
async fn memory_query_passage_unit_returns_passage_hits() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "karpathy".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: Some(QueryUnit::Passage),
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = match result.content.first().and_then(|c| c.as_text()) {
        Some(t) => t.text.clone(),
        None => panic!("expected text content"),
    };
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let hits = value["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "expected at least one passage hit");
    let hit = &hits[0];
    assert!(hit.get("passage_id").is_some(), "hit missing passage_id");
    assert!(hit.get("page_path").is_some(), "hit missing page_path");
    assert_eq!(hit["page_path"], "foo.md");
    assert!(hit.get("text").is_some(), "hit missing passage text");
}

/// G1: `parent_expansion: "document"` on a passage-mode query attaches
/// the owning page's workspace/project/path/title as parent context.
#[tokio::test]
async fn memory_query_passage_unit_with_document_parent_expansion() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "karpathy".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: Some(QueryUnit::Passage),
                parent_expansion: Some(ParentExpansion::Document),
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result.content.first().and_then(|c| c.as_text()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text.text).unwrap();
    let hit = &value["hits"][0];
    let parent = &hit["parent"];
    assert!(
        !parent.is_null(),
        "expected a document parent to be attached"
    );
    assert_eq!(parent["path"], "foo.md");
    assert_eq!(parent["title"], "Foo");
}

/// G1: a small `content_budget` truncates passage text instead of
/// returning the full passage body.
#[tokio::test]
async fn memory_query_passage_unit_respects_content_budget() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "karpathy".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: Some(QueryUnit::Passage),
                parent_expansion: None,
                content_budget: Some(5),
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result.content.first().and_then(|c| c.as_text()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text.text).unwrap();
    let hit_text = value["hits"][0]["text"].as_str().unwrap();
    assert!(
        hit_text.chars().count() <= 5,
        "expected content_budget to cap passage text, got {hit_text:?}"
    );
}

/// With NO embedder configured — the default deployment —
/// `memory_query` must still run the lexical entity and graph streams.
#[tokio::test]
async fn query_without_embedder_runs_entity_and_graph_streams() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    assert!(
        server.embedder.is_none(),
        "this test is specifically the no-embedder path"
    );

    // A page findable ONLY via its entities (body avoids the query
    // word), and a page findable ONLY via a link from an FTS hit.
    let mut entity_only = NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("concepts/broker.md").unwrap(),
        title: "Broker".into(),
        body: "The chosen transport gives at-least-once semantics.".into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: vec!["nats jetstream".into()],
        evidence: Vec::new(),
    };
    store.writer.upsert_page(entity_only.clone()).await.unwrap();
    entity_only.path = PagePath::new("concepts/linked.md").unwrap();
    entity_only.title = "Linked".into();
    entity_only.body = "Nothing quotable here.".into();
    entity_only.entities = Vec::new();
    store.writer.upsert_page(entity_only).await.unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("concepts/seed.md").unwrap(),
            title: "Seed".into(),
            body: "graphseed points at [[concepts/linked.md]].".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: vec![PagePath::new("concepts/linked.md").unwrap().into()],
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let query = async |q: &str| -> serde_json::Value {
        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: q.into(),
                    limit: Some(10),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: None,
                    include_expired: None,
                    explain: Some(true),
                    as_of: None,
                    unit: None,
                    parent_expansion: None,
                    content_budget: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result.content.first().and_then(|c| c.as_text()).unwrap();
        serde_json::from_str(&text.text).unwrap()
    };

    let entity_hit = query("jetstream").await;
    let paths: Vec<&str> = entity_hit["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"concepts/broker.md"),
        "entity stream must run without an embedder: {paths:?}"
    );
    assert_eq!(
        entity_hit["hits"][0]["score_details"]["matched_entities"],
        serde_json::json!(["nats jetstream"]),
        "{entity_hit}"
    );
    assert!(
        entity_hit["hits"][0]["score_details"]["entity_weight"]
            .as_f64()
            .is_some_and(|weight| weight > 0.0),
        "{entity_hit}"
    );
    assert_eq!(
        entity_hit["streams_active"],
        serde_json::json!(["fts", "entity", "graph"])
    );

    let graph_hit = query("graphseed").await;
    let paths: Vec<&str> = graph_hit["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"concepts/linked.md"),
        "graph stream must run without an embedder: {paths:?}"
    );
}

// Issue #154: default-scoped queries union the reserved `_global`
// preferences scope; explicitly scoped queries do not.
#[tokio::test]
async fn default_query_unions_global_scope_and_explicit_scope_skips_it() {
    let (_tmp, store, server, _ws, _proj) = setup_server().await;
    let global = sessionmunch_store::create_global_scope(&store.writer)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: global.workspace_id,
            project_id: global.project_id,
            path: PagePath::new("preferences/style.md").unwrap(),
            title: "Style".into(),
            body: "Karpathy approved standing preference: pnpm always.".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let query = |workspace: Option<&str>, project: Option<&str>| QueryArgs {
        query: "karpathy".into(),
        limit: Some(5),
        project: project.map(str::to_string),
        scopes: Vec::new(),
        workspace: workspace.map(str::to_string),
        global: None,
        include_expired: None,
        explain: None,
        as_of: None,
        unit: None,
        parent_expansion: None,
        content_budget: None,
    };

    let result = server
        .memory_query(
            Parameters(query(None, None)),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result.content.first().and_then(|c| c.as_text()).unwrap();
    assert!(
        text.text.contains("foo.md"),
        "current-project hit must remain: {}",
        text.text
    );
    assert!(
        text.text.contains("global_scope_hits") && text.text.contains("preferences/style.md"),
        "default query must union the reserved global scope: {}",
        text.text
    );
    assert!(
        !text.text.contains("score_details") && !text.text.contains("streams_active"),
        "ordinary default queries must preserve the old response shape"
    );

    let mut explained_args = query(None, None);
    explained_args.explain = Some(true);
    let explained = server
        .memory_query(
            Parameters(explained_args),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let explained_text = explained.content.first().and_then(|c| c.as_text()).unwrap();
    let explained_value: serde_json::Value = serde_json::from_str(&explained_text.text).unwrap();
    assert!(explained_value["hits"][0].get("score_details").is_some());
    assert!(
        explained_value["global_scope_hits"][0]
            .get("score_details")
            .is_some(),
        "the default query's reserved global-scope union must keep its explanation"
    );

    let result = server
        .memory_query(
            Parameters(query(Some("default"), Some("scratch"))),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result.content.first().and_then(|c| c.as_text()).unwrap();
    assert!(
        !text.text.contains("preferences/style.md"),
        "explicitly scoped queries must not union the global scope: {}",
        text.text
    );
}

// Issue #154: an absent `_global` scope contributes nothing and is
// never created by a read.
#[tokio::test]
async fn default_query_without_global_scope_is_unchanged() {
    let (_tmp, store, server, _ws, _proj) = setup_server().await;
    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "karpathy".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result.content.first().and_then(|c| c.as_text()).unwrap();
    assert!(
        !text.text.contains("global_scope_hits"),
        "no reserved scope -> field elided: {}",
        text.text
    );
    assert_eq!(
        sessionmunch_store::lookup_global_scope(&store.reader)
            .await
            .unwrap(),
        None,
        "a read must never create the reserved scope"
    );
}

#[tokio::test]
async fn write_page_scope_global_lands_in_reserved_scope() {
    let (tmp, store, server, _ws, _proj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = server.with_wiki(wiki);
    let write_args = |scope: Option<&str>, project: Option<&str>| WritePageArgs {
        path: "preferences/pkg.md".to_string(),
        body: "# Package manager\nAlways pnpm workspaces.".to_string(),
        title: None,
        tier: None,
        tags: Vec::new(),
        pinned: false,
        project: project.map(str::to_string),
        workspace: None,
        scope: scope.map(str::to_string),
        expires_at: None,
    };

    server
        .memory_write_page(
            Parameters(write_args(Some("global"), None)),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let global = sessionmunch_store::lookup_global_scope(&store.reader)
        .await
        .unwrap()
        .expect("scope: global write must create the reserved scope");
    let pages = store
        .reader
        .recent_pages_for_project(global.workspace_id, global.project_id, 10)
        .await
        .unwrap();
    assert!(
        pages
            .iter()
            .any(|p| p.path.as_str() == "preferences/pkg.md"),
        "page must land in the reserved scope; got {:?}",
        pages.iter().map(|p| p.path.as_str()).collect::<Vec<_>>()
    );

    let err = server
        .memory_write_page(
            Parameters(write_args(Some("global"), Some("other"))),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("scope + project must fail closed");
    assert!(err.to_string().contains("cannot be combined"), "{err}");

    let err = server
        .memory_write_page(
            Parameters(write_args(Some("universe"), None)),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("unknown scope values must fail closed");
    assert!(err.to_string().contains("unknown scope"), "{err}");
}

#[tokio::test]
async fn memory_query_returns_raw_hits_when_pages_miss() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::OpenCode,
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
                title: "raw prompt".into(),
                body: "raw fallback contains quokka only detail".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "quokka".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = match result.content.first().and_then(|c| c.as_text()) {
        Some(t) => t.text.clone(),
        None => panic!("expected text content"),
    };
    assert!(
        text.contains("\"hits\": []"),
        "expected no page hits; got {text}"
    );
    assert!(
        text.contains("raw_hits"),
        "expected raw fallback; got {text}"
    );
    assert!(text.contains("quokka"), "expected raw snippet; got {text}");
}

#[tokio::test]
async fn memory_query_returns_raw_hits_via_explicit_scopes() {
    // The raw-observation fallback must also fire on the explicit
    // `scopes` path (the recommended scope-bleed mitigation), not just
    // default / workspace+project. Regression for a scope with
    // observations but zero compiled pages.
    let (_tmp, store, server, ws, _proj) = setup_server().await;
    let scoped = store
        .writer
        .get_or_create_project(ws, "scoped-obs", None)
        .await
        .unwrap();
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: scoped,
            agent_kind: AgentKind::OpenCode,
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
                project_id: scoped,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "raw prompt".into(),
                body: "raw fallback contains quokka only detail".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "quokka".into(),
                limit: Some(5),
                project: None,
                scopes: vec![MemoryScopeArg {
                    project: "scoped-obs".into(),
                    workspace: "default".into(),
                }],
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            test_optional_parts(),
        )
        .await
        .unwrap();
    let text = match result.content.first().and_then(|c| c.as_text()) {
        Some(t) => t.text.clone(),
        None => panic!("expected text content"),
    };
    assert!(
        text.contains("\"hits\": []"),
        "expected no page hits; got {text}"
    );
    assert!(
        text.contains("raw_hits"),
        "expected raw fallback via scopes; got {text}"
    );
    assert!(text.contains("quokka"), "expected raw snippet; got {text}");
}

#[tokio::test]
async fn memory_query_scoped_raw_hits_respect_limit_and_rank_order() {
    let (_tmp, store, server, ws, _proj) = setup_server().await;
    let first = store
        .writer
        .get_or_create_project(ws, "rank-first", None)
        .await
        .unwrap();
    let second = store
        .writer
        .get_or_create_project(ws, "rank-second", None)
        .await
        .unwrap();
    for (project_id, title) in [
        (first, "first-a"),
        (first, "first-b"),
        (second, "second-a"),
        (second, "second-b"),
    ] {
        insert_test_observation(
            &store,
            ws,
            project_id,
            title,
            &format!("rank_token appears in {title}"),
        )
        .await;
    }

    let mut expected = store
        .reader
        .search_observations_for_project(ws, first, "rank_token".into(), 3)
        .await
        .unwrap();
    expected.extend(
        store
            .reader
            .search_observations_for_project(ws, second, "rank_token".into(), 3)
            .await
            .unwrap(),
    );
    expected.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    expected.truncate(3);
    assert!(
        expected.iter().any(|hit| hit.title.starts_with("first-"))
            && expected.iter().any(|hit| hit.title.starts_with("second-")),
        "test setup must require merged hits from both scopes: {expected:?}"
    );

    let json = call_tool_json(
        server
            .memory_query(
                Parameters(QueryArgs {
                    query: "rank_token".into(),
                    limit: Some(3),
                    project: None,
                    scopes: vec![
                        MemoryScopeArg {
                            project: "rank-first".into(),
                            workspace: "default".into(),
                        },
                        MemoryScopeArg {
                            project: "rank-second".into(),
                            workspace: "default".into(),
                        },
                    ],
                    workspace: None,
                    global: None,
                    include_expired: None,
                    explain: None,
                    as_of: None,
                    unit: None,
                    parent_expansion: None,
                    content_budget: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    let raw_hits = json["raw_hits"].as_array().unwrap();
    assert_eq!(raw_hits.len(), 3, "raw hits must be truncated: {json}");
    let titles: Vec<&str> = raw_hits
        .iter()
        .map(|hit| hit["title"].as_str().unwrap())
        .collect();
    let expected_titles: Vec<&str> = expected.iter().map(|hit| hit.title.as_str()).collect();
    assert_eq!(
        titles, expected_titles,
        "raw hits should match the merged-and-truncated store ranking: {json}"
    );
    let ranks: Vec<f64> = raw_hits
        .iter()
        .map(|hit| hit["rank"].as_f64().unwrap())
        .collect();
    assert!(
        ranks.windows(2).all(|pair| pair[0] <= pair[1]),
        "raw hits must be rank-sorted: {json}"
    );
}

#[tokio::test]
async fn memory_query_scoped_raw_hits_deduplicate_duplicate_scopes() {
    let (_tmp, store, server, ws, _proj) = setup_server().await;
    let scoped = store
        .writer
        .get_or_create_project(ws, "dedupe-obs", None)
        .await
        .unwrap();
    insert_test_observation(
        &store,
        ws,
        scoped,
        "dedupe raw prompt",
        "dedupe_token appears once",
    )
    .await;

    let json = call_tool_json(
        server
            .memory_query(
                Parameters(QueryArgs {
                    query: "dedupe_token".into(),
                    limit: Some(10),
                    project: None,
                    scopes: vec![
                        MemoryScopeArg {
                            project: "dedupe-obs".into(),
                            workspace: "default".into(),
                        },
                        MemoryScopeArg {
                            project: "dedupe-obs".into(),
                            workspace: "default".into(),
                        },
                    ],
                    workspace: None,
                    global: None,
                    include_expired: None,
                    explain: None,
                    as_of: None,
                    unit: None,
                    parent_expansion: None,
                    content_budget: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    let raw_hits = json["raw_hits"].as_array().unwrap();
    assert_eq!(
        raw_hits.len(),
        1,
        "duplicate scopes must not duplicate raw hits: {json}"
    );
    assert_eq!(raw_hits[0]["title"], "dedupe raw prompt");
}

#[tokio::test]
async fn memory_query_missing_scope_fails_closed_without_default_raw_fallback() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    insert_test_observation(
        &store,
        ws,
        proj,
        "default raw prompt",
        "missing_scope_token exists only in the default project",
    )
    .await;

    let err = server
        .memory_query(
            Parameters(QueryArgs {
                query: "missing_scope_token".into(),
                limit: Some(10),
                project: None,
                scopes: vec![MemoryScopeArg {
                    project: "absent".into(),
                    workspace: "default".into(),
                }],
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            test_optional_parts(),
        )
        .await
        .expect_err("missing explicit scope must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("absent") || msg.contains("not found"),
        "missing scope error should identify the bad scope: {msg}"
    );
}

#[tokio::test]
async fn memory_query_page_hits_suppress_scoped_raw_fallback() {
    let (_tmp, store, server, ws, _proj) = setup_server().await;
    let pages = store
        .writer
        .get_or_create_project(ws, "page-scope", None)
        .await
        .unwrap();
    let raw = store
        .writer
        .get_or_create_project(ws, "raw-scope", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: pages,
            path: PagePath::new("page-hit.md").unwrap(),
            title: "Page Hit".into(),
            body: "mixed_scope_token appears in a compiled page".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    insert_test_observation(
        &store,
        ws,
        raw,
        "raw mixed prompt",
        "mixed_scope_token also appears only in raw observations",
    )
    .await;

    let json = call_tool_json(
        server
            .memory_query(
                Parameters(QueryArgs {
                    query: "mixed_scope_token".into(),
                    limit: Some(10),
                    project: None,
                    scopes: vec![
                        MemoryScopeArg {
                            project: "page-scope".into(),
                            workspace: "default".into(),
                        },
                        MemoryScopeArg {
                            project: "raw-scope".into(),
                            workspace: "default".into(),
                        },
                    ],
                    workspace: None,
                    global: None,
                    include_expired: None,
                    explain: None,
                    as_of: None,
                    unit: None,
                    parent_expansion: None,
                    content_budget: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    assert!(
        json["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["path"] == "page-hit.md"),
        "expected compiled page hit: {json}"
    );
    assert!(
        json.get("raw_hits")
            .is_none_or(|raw_hits| raw_hits.as_array().is_some_and(Vec::is_empty)),
        "compiled page hits must suppress raw fallback: {json}"
    );
}

#[tokio::test]
async fn memory_query_can_target_explicit_workspace_project() {
    let (_tmp, store, server, _ws, _pj) = setup_server().await;
    let practice_ws = store
        .writer
        .get_or_create_workspace("practice")
        .await
        .unwrap();
    let testing = store
        .writer
        .get_or_create_project(practice_ws, "unit-testing", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: practice_ws,
            project_id: testing,
            path: PagePath::new("patterns.md").unwrap(),
            title: "Testing Patterns".into(),
            body: "workspace_specific_token belongs to practice".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "workspace_specific_token".into(),
                limit: Some(5),
                project: Some("unit-testing".into()),
                scopes: Vec::new(),
                workspace: Some("practice".into()),
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("patterns.md"), "expected hit; got {text}");
}

// Issue #155: the "exactly one of path/query" contract must live in the
// machine-readable schema, with branches demanding presence AND a
// non-null type — a bare `required` is satisfied by OpenCode-style
// `path: null` filling. Pins against a schemars upgrade silently
// dropping the `extend` attribute.
/// #577's class fence: NO registered tool may carry a root-level
/// combinator — one bad tool 400s the entire session for every
/// Messages-API-routed client.
#[tokio::test]
async fn no_tool_schema_carries_root_combinators() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    for tool in server.tool_router.list_all() {
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        for key in ["anyOf", "oneOf", "allOf"] {
            assert!(
                schema.get(key).is_none(),
                "tool `{}` carries root-level `{key}`; Anthropic-routed \
                     clients reject the whole session over it",
                tool.name
            );
        }
    }
}

#[test]
fn read_page_schema_carries_no_root_combinators() {
    // #577: the Anthropic Messages API rejects root-level
    // anyOf/oneOf/allOf on input_schema — a tool carrying one kills
    // the WHOLE session for every Messages-API-routed client before
    // any tool runs. The exactly-one contract lives in the field
    // descriptions and runtime validation instead. This pins every
    // tool schema, not just memory_read_page, so the class cannot
    // come back through another tool.
    let schema = serde_json::to_value(schemars::schema_for!(ReadPageArgs)).unwrap();
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "root-level `{key}` breaks Anthropic-routed clients: {schema}"
        );
    }
    // The contract itself is still declared to the model.
    for field in ["path", "query"] {
        let desc = schema["properties"][field]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            desc.contains("exactly one"),
            "`{field}` description must state the exactly-one contract"
        );
    }
}

// Pins what the patch strips (root combinators) and what it preserves.
#[test]
fn restricted_schema_tool_list_strips_root_combinators_only() {
    let schema: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "ReadPageArgs",
            "type": "object",
            "description": "Read one wiki page.",
            "properties": {
                "path": { "type": "string", "description": "Exact wiki path" },
                // Nested combinator: must survive (rejection is root-specific).
                "query": { "anyOf": [{ "type": "string" }, { "type": "null" }] }
            },
            "anyOf": [{ "required": ["path"] }, { "required": ["query"] }],
            "oneOf": [{ "required": ["path"] }],
            "allOf": [{ "required": ["query"] }]
        }))
        .unwrap();
    let tool = Tool::new("memory_read_page", "Read a wiki page", schema);

    let patched = restricted_schema_tool_list(vec![tool], SchemaDialect::RootCombinators);
    let out = &patched[0].input_schema;

    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            !out.contains_key(key),
            "root `{key}` must be stripped: {out:?}"
        );
    }
    for key in ["$schema", "title", "type", "description", "properties"] {
        assert!(out.contains_key(key), "root `{key}` must survive: {out:?}");
    }
    assert_eq!(
        out["properties"]["query"]["anyOf"],
        serde_json::json!([{ "type": "string" }, { "type": "null" }]),
        "nested combinators are out of scope for the patch"
    );
}

#[test]
fn restricted_schema_tool_list_leaves_flat_tools_untouched() {
    let schema: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": { "verbose": { "type": "boolean" } }
        }))
        .unwrap();
    let tool = Tool::new("memory_status", "Status counts", schema);
    let before = serde_json::to_value(&tool).unwrap();

    let patched = restricted_schema_tool_list(vec![tool], SchemaDialect::RootCombinators);
    let after = serde_json::to_value(&patched[0]).unwrap();

    assert_eq!(before, after, "flat tools must pass through unchanged");
}

#[test]
fn restricted_schema_flavor_matches_complete_query_pairs_only() {
    assert_eq!(
        restricted_schema_flavor("flavor=moonshot"),
        Some(SchemaDialect::RootCombinators)
    );
    assert_eq!(
        restricted_schema_flavor("client=kiro&flavor=bedrock&debug=false"),
        Some(SchemaDialect::RootCombinators)
    );
    assert_eq!(
        restricted_schema_flavor("flavor=gemini"),
        Some(SchemaDialect::GeminiSafe)
    );
    assert_eq!(
        restricted_schema_flavor("flavor=vertex&client=opencode"),
        Some(SchemaDialect::GeminiSafe)
    );
    // Several markers resolve to the strictest, whatever their order.
    assert_eq!(
        restricted_schema_flavor("flavor=gemini&flavor=moonshot"),
        Some(SchemaDialect::GeminiSafe)
    );
    assert_eq!(restricted_schema_flavor("flavor=unknown"), None);
    assert_eq!(
        restricted_schema_flavor("note=flavor=bedrock&client=kiro"),
        None
    );
}

fn gemini_safe(schema: serde_json::Value) -> serde_json::Value {
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(schema).unwrap();
    gemini_safe_schema(&mut map);
    serde_json::Value::Object(map)
}

// The reported Vertex 400: `schemars` renders every `Option<T>` argument as
// a union type, which a pass-through client turns into `any_of` with
// `description` still beside it.
#[test]
fn gemini_safe_schema_collapses_nullable_unions() {
    let out = gemini_safe(serde_json::json!({
        "type": "object",
        "properties": {
            "max_proposals": {
                "description": "Override the maximum validated proposal count.",
                "type": ["integer", "null"],
                "format": "uint",
                "minimum": 0
            },
            "kinds": {
                "type": ["array", "null"],
                "items": { "type": ["string", "null"] }
            },
            "nothing": { "type": ["null"] }
        }
    }));
    let properties = &out["properties"];

    assert_eq!(
        properties["max_proposals"],
        serde_json::json!({
            "description": "Override the maximum validated proposal count.",
            "type": "integer",
            "format": "uint",
            "minimum": 0,
            "nullable": true
        }),
        "the union must collapse; every other keyword must survive"
    );
    assert_eq!(properties["kinds"]["type"], serde_json::json!("array"));
    assert_eq!(
        properties["kinds"]["items"],
        serde_json::json!({ "type": "string", "nullable": true }),
        "nested subschemas must be rewritten too"
    );
    assert_eq!(
        properties["nothing"],
        serde_json::json!({ "nullable": true }),
        "a null-only union has no type to keep"
    );
}

// `Option<T>` over a `$ref`ed inner type: schemars wraps it in `anyOf` and
// leaves the doc comment as a sibling, which is the exact shape Vertex names
// in its error.
#[test]
fn gemini_safe_schema_flattens_combinator_carrying_siblings() {
    let out = gemini_safe(serde_json::json!({
        "type": "object",
        "properties": {
            "scope": {
                "description": "Scope to read.",
                "anyOf": [
                    { "$ref": "#/$defs/MemoryScopeArg" },
                    { "type": "null" }
                ]
            }
        }
    }));

    assert_eq!(
        out["properties"]["scope"],
        serde_json::json!({
            "description": "Scope to read.",
            "nullable": true,
            "$ref": "#/$defs/MemoryScopeArg"
        }),
        "the lone real branch must merge into the parent, doc comment intact"
    );
}

// The parent's own keys win, so a merge can never overwrite the field's
// description with the branch's.
#[test]
fn gemini_safe_schema_merge_keeps_the_parent_description() {
    let out = gemini_safe(serde_json::json!({
        "description": "Field docs.",
        "anyOf": [
            { "type": "string", "description": "Branch docs." },
            { "type": "null" }
        ]
    }));

    assert_eq!(out["description"], serde_json::json!("Field docs."));
    assert_eq!(out["type"], serde_json::json!("string"));
    assert_eq!(out["nullable"], serde_json::json!(true));
}

// Both shapes the rewrite deliberately declines to touch: narrowing a real
// union would change the advertised contract, and a lone combinator is
// already what Google wants.
#[test]
fn gemini_safe_schema_leaves_shapes_it_cannot_express_alone() {
    let real_union = serde_json::json!({ "type": ["string", "integer", "null"] });
    assert_eq!(gemini_safe(real_union.clone()), real_union);

    let lone_combinator = serde_json::json!({
        "anyOf": [{ "required": ["path"] }, { "required": ["query"] }]
    });
    assert_eq!(gemini_safe(lone_combinator.clone()), lone_combinator);

    let several_real_branches = serde_json::json!({
        "description": "Either shape.",
        "anyOf": [{ "type": "string" }, { "type": "integer" }, { "type": "null" }]
    });
    assert_eq!(
        gemini_safe(several_real_branches.clone()),
        several_real_branches
    );
}

// The Gemini dialect is a superset: root combinators still go, and every
// subschema is rewritten on top.
#[test]
fn restricted_schema_tool_list_gemini_safe_also_strips_root_combinators() {
    let schema: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": ["string", "null"] } },
            "anyOf": [{ "required": ["path"] }, { "required": ["query"] }]
        }))
        .unwrap();
    let tool = Tool::new("memory_read_page", "Read a wiki page", schema);

    let patched = restricted_schema_tool_list(vec![tool], SchemaDialect::GeminiSafe);
    let out = &patched[0].input_schema;

    assert!(
        !out.contains_key("anyOf"),
        "root combinator must be stripped"
    );
    assert_eq!(
        out["properties"]["query"],
        serde_json::json!({ "type": "string", "nullable": true })
    );
}

// Dialect isolation: the shipped Moonshot/Bedrock behavior must not change.
#[test]
fn restricted_schema_tool_list_root_combinators_keeps_nullable_unions() {
    let schema: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": ["string", "null"] } }
        }))
        .unwrap();
    let tool = Tool::new("memory_read_page", "Read a wiki page", schema);
    let before = serde_json::to_value(&tool).unwrap();

    let patched = restricted_schema_tool_list(vec![tool], SchemaDialect::RootCombinators);

    assert_eq!(
        serde_json::to_value(&patched[0]).unwrap(),
        before,
        "the root-combinator dialect must leave union types alone"
    );
}

// The two opt-ins are independent switches over one ordered dialect, so
// passing `false` to the weaker one must not undo the stronger one.
#[tokio::test]
async fn schema_dialect_opt_ins_only_raise_the_floor() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    assert_eq!(server.schema_dialect, SchemaDialect::Upstream);

    let gemini = server
        .clone()
        .with_gemini_safe_schemas(true)
        .with_strip_root_combinators(false);
    assert_eq!(gemini.schema_dialect, SchemaDialect::GeminiSafe);

    let combinators = server
        .with_strip_root_combinators(true)
        .with_gemini_safe_schemas(false);
    assert_eq!(combinators.schema_dialect, SchemaDialect::RootCombinators);
}

// The guard that actually pins "our tool surface is Vertex-safe": a future
// optional argument on any tool cannot silently reintroduce a union type or
// a combinator with siblings.
#[tokio::test]
async fn every_tool_schema_is_gemini_safe() {
    fn assert_safe(tool: &str, pointer: &str, schema: &serde_json::Value) {
        match schema {
            serde_json::Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    assert_safe(tool, &format!("{pointer}/{index}"), item);
                }
            }
            serde_json::Value::Object(map) => {
                assert!(
                    !map.get("type").is_some_and(serde_json::Value::is_array),
                    "{tool}{pointer}: Google's Schema takes a single `type`, got {:?}",
                    map.get("type")
                );
                for key in ["anyOf", "oneOf"] {
                    assert!(
                        !(map.contains_key(key) && map.len() > 1),
                        "{tool}{pointer}: `{key}` must be the only field, got keys {:?}",
                        map.keys().collect::<Vec<_>>()
                    );
                }
                for (key, value) in map {
                    assert_safe(tool, &format!("{pointer}/{key}"), value);
                }
            }
            _ => {}
        }
    }

    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let tools =
        restricted_schema_tool_list(server.tool_router.list_all(), SchemaDialect::GeminiSafe);
    assert!(!tools.is_empty(), "tools must be registered");
    for tool in &tools {
        let mut schema = serde_json::to_value(&tool.input_schema).unwrap();
        // `$defs` is out of scope for this dialect (see
        // [`gemini_safe_schema`]): `FeedbackKind` renders as a `oneOf` of
        // `const` branches with a sibling `description`, and Gemini CLI
        // forwards `$defs`/`$ref` to Vertex untouched without trouble. What
        // this guard pins is the argument schemas, where every `Option`
        // field lives.
        if let Some(schema) = schema.as_object_mut() {
            schema.shift_remove("$defs");
        }
        assert_safe(&tool.name, "", &schema);
    }
}

// Issue #155: the neither-arg error must teach a looping model what a
// valid retry looks like, naming both args and a concrete example.
#[tokio::test]
async fn memory_read_page_without_args_returns_instructive_error() {
    let (tmp, store, server, _ws, _pj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let err = server
        .with_wiki(wiki)
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: None,
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("neither arg must fail closed");
    let msg = err.to_string();
    for needle in ["`path`", "`query`", "notes/topic.md", "do not retry"] {
        assert!(msg.contains(needle), "error must contain {needle:?}: {msg}");
    }
}

#[tokio::test]
async fn memory_read_page_can_target_explicit_workspace_project() {
    let (tmp, store, server, _ws, _pj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let practice_ws = store
        .writer
        .get_or_create_workspace("practice")
        .await
        .unwrap();
    let docs = store
        .writer
        .get_or_create_project(practice_ws, "docs", None)
        .await
        .unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: practice_ws,
        project_id: docs,
        path: PagePath::new("notes/sibling.md").unwrap(),
        frontmatter: serde_json::json!({"title": "Sibling Page"}),
        body: "workspace explicit read body".to_string(),
        tier: Tier::Semantic,
        pinned: false,
        title: Some("Sibling Page".into()),
        admission_ctx: None,
        author_id: None,
        actor: sessionmunch_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    let result = server
        .with_wiki(wiki)
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("notes/sibling.md".into()),
                project: Some("docs".into()),
                workspace: Some("practice".into()),
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("workspace explicit read body"),
        "expected sibling workspace body; got {text}"
    );
    assert!(
        text.contains("notes/sibling.md"),
        "expected sibling workspace path; got {text}"
    );
}

#[tokio::test]
async fn memory_read_page_marks_db_fallback_when_file_missing() {
    let (tmp, store, server, ws, proj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/db-only-tool.md").unwrap(),
            title: "DB Only Tool".into(),
            body: "tool fallback body".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"title": "DB Only Tool"}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let result = server
        .with_wiki(wiki)
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("notes/db-only-tool.md".into()),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("tool fallback body"),
        "expected DB body; got {text}"
    );
    assert!(
        text.contains("db-fallback"),
        "expected fallback diagnostic; got {text}"
    );
}

#[tokio::test]
async fn memory_read_page_missing_error_names_the_scope() {
    let (tmp, store, server, _ws, _proj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = server.with_wiki(wiki);

    // Explicit scope: the not-found error names workspace/project and the
    // relative path (no raw disk error / absolute path leak).
    let err = server
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("does-not-exist.md".into()),
                project: Some("scratch".into()),
                workspace: Some("default".into()),
            }),
            test_optional_parts(),
        )
        .await
        .expect_err("missing page must error");
    let msg = err.to_string();
    assert!(
        msg.contains("default/scratch"),
        "must name the scope; got {msg}"
    );
    assert!(
        msg.contains("does-not-exist.md"),
        "must name the path; got {msg}"
    );

    // Auto-scoped read: the error adds the parallel-session hint.
    let err = server
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("does-not-exist.md".into()),
                project: None,
                workspace: None,
            }),
            test_optional_parts(),
        )
        .await
        .expect_err("missing page must error");
    assert!(
        err.to_string().contains("auto-resolved"),
        "auto-scoped error must hint at scope-bleed; got {err}"
    );
}

/// Seed one session with the given `(kind, title, body)` rows, in order,
/// and end it when `completed`. Returns the session id.
async fn seed_session_observations(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    completed: bool,
    rows: &[(ObservationKind, &str, &str)],
) -> SessionId {
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::OpenCode,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    for (kind, title, body) in rows {
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: *kind,
                    extension: None,
                    source_event: None,
                    title: (*title).into(),
                    body: (*body).into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
    }
    if completed {
        store.writer.end_session(session_id, None).await.unwrap();
    }
    session_id
}

fn session_observations_args(session_id: Option<SessionId>) -> ReadSessionObservationsArgs {
    ReadSessionObservationsArgs {
        session_id: session_id.map(|id| id.to_string()),
        limit: None,
        offset: None,
        order: None,
        kinds: None,
        query: None,
        body_max_chars: None,
        project: None,
        workspace: None,
    }
}

#[tokio::test]
async fn memory_read_session_observations_pages_in_capture_order() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let session_id = seed_session_observations(
        &store,
        ws,
        proj,
        true,
        &[
            (ObservationKind::UserPrompt, "first", "one"),
            (ObservationKind::PostToolUse, "second", "two"),
            (ObservationKind::Stop, "third", "three"),
        ],
    )
    .await;
    // The same session left one row in a sibling project: it must be
    // counted as elided, never returned.
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: other,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "elsewhere".into(),
                body: "other scope".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let mut args = session_observations_args(Some(session_id));
    args.limit = Some(2);
    let page = call_tool_json(
        server
            .memory_read_session_observations(Parameters(args), test_optional_parts())
            .await
            .unwrap(),
    );
    assert_eq!(page["session"]["session_id"], session_id.to_string());
    assert_eq!(page["session"]["observation_count"], 3);
    assert!(page["session"]["ended_at"].is_string());
    assert_eq!(page["total"], 3);
    assert_eq!(page["offset"], 0);
    assert_eq!(page["limit"], 2);
    assert_eq!(page["order"], "asc");
    assert_eq!(page["elided_other_scope"], 1);
    let titles: Vec<&str> = page["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["first", "second"]);
    assert_eq!(page["observations"][0]["kind"], "user-prompt");
    assert_eq!(page["observations"][0]["body"], "one");

    let mut args = session_observations_args(Some(session_id));
    args.offset = Some(2);
    args.order = Some("desc".into());
    let page = call_tool_json(
        server
            .memory_read_session_observations(Parameters(args), test_optional_parts())
            .await
            .unwrap(),
    );
    assert_eq!(page["order"], "desc");
    let titles: Vec<&str> = page["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        ["first"],
        "desc + offset 2 must land on the oldest row"
    );

    let mut args = session_observations_args(Some(session_id));
    args.kinds = Some(vec!["stop".into()]);
    let page = call_tool_json(
        server
            .memory_read_session_observations(Parameters(args), test_optional_parts())
            .await
            .unwrap(),
    );
    assert_eq!(page["total"], 1);
    assert_eq!(page["observations"][0]["title"], "third");
}

#[tokio::test]
async fn memory_read_session_observations_caps_bodies_with_marker() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let long_body = "x".repeat(1_000);
    let session_id = seed_session_observations(
        &store,
        ws,
        proj,
        true,
        &[(ObservationKind::UserPrompt, "long", long_body.as_str())],
    )
    .await;

    // 50 is below the floor: the cap clamps to 200 and says so.
    let mut args = session_observations_args(Some(session_id));
    args.body_max_chars = Some(50);
    let page = call_tool_json(
        server
            .memory_read_session_observations(Parameters(args), test_optional_parts())
            .await
            .unwrap(),
    );
    assert_eq!(page["body_max_chars"], 200);
    let body = page["observations"][0]["body"].as_str().unwrap();
    assert!(
        body.starts_with(&"x".repeat(200)),
        "body must keep the first 200 chars"
    );
    assert!(
        body.contains("[body truncated; 800 chars omitted]"),
        "body must end with a visible marker; got {body}"
    );

    let page = call_tool_json(
        server
            .memory_read_session_observations(
                Parameters(session_observations_args(Some(session_id))),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        page["observations"][0]["body"], long_body,
        "default cap keeps 1000 chars"
    );
}

#[tokio::test]
async fn memory_read_session_observations_rejects_session_from_other_scope() {
    let (_tmp, store, server, ws, _pj) = setup_server().await;
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let session_id = seed_session_observations(
        &store,
        ws,
        other,
        true,
        &[(ObservationKind::UserPrompt, "hidden", "in other")],
    )
    .await;

    let err = server
        .memory_read_session_observations(
            Parameters(session_observations_args(Some(session_id))),
            test_optional_parts(),
        )
        .await
        .expect_err("a session from another project must read as not found");
    let msg = err.to_string();
    assert!(
        msg.contains("not found in default/scratch"),
        "error must name the resolved scope; got {msg}"
    );

    let mut args = session_observations_args(Some(session_id));
    args.session_id = Some("not-a-uuid".into());
    let err = server
        .memory_read_session_observations(Parameters(args), test_optional_parts())
        .await
        .expect_err("a malformed session id must be rejected");
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert_eq!(err.message, "invalid session id: not-a-uuid");
}

#[tokio::test]
async fn memory_read_session_observations_rejects_unknown_kind_and_order() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let session_id = seed_session_observations(
        &store,
        ws,
        proj,
        true,
        &[(ObservationKind::UserPrompt, "p", "b")],
    )
    .await;

    let mut args = session_observations_args(Some(session_id));
    args.kinds = Some(vec!["user-prompt".into(), "bogus".into()]);
    let err = server
        .memory_read_session_observations(Parameters(args), test_optional_parts())
        .await
        .expect_err("unknown kind must be rejected");
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert_eq!(err.message, "unknown observation kind: bogus");

    let mut args = session_observations_args(Some(session_id));
    args.order = Some("sideways".into());
    let err = server
        .memory_read_session_observations(Parameters(args), test_optional_parts())
        .await
        .expect_err("unknown order must be rejected");
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn memory_read_session_observations_defaults_to_latest_completed_session() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let err = server
        .memory_read_session_observations(
            Parameters(session_observations_args(None)),
            test_optional_parts(),
        )
        .await
        .expect_err("an empty project has no completed session");
    assert!(
        err.to_string()
            .contains("no completed session in default/scratch"),
        "got {err}"
    );

    let completed = seed_session_observations(
        &store,
        ws,
        proj,
        true,
        &[(ObservationKind::UserPrompt, "done", "completed work")],
    )
    .await;
    let _open = seed_session_observations(
        &store,
        ws,
        proj,
        false,
        &[(ObservationKind::UserPrompt, "live", "still running")],
    )
    .await;

    let page = call_tool_json(
        server
            .memory_read_session_observations(
                Parameters(session_observations_args(None)),
                test_optional_parts(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        page["session"]["session_id"],
        completed.to_string(),
        "the open session must be skipped"
    );
    assert_eq!(page["observations"][0]["title"], "done");
}

#[tokio::test]
async fn memory_read_session_observations_query_filters_rows() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let session_id = seed_session_observations(
        &store,
        ws,
        proj,
        true,
        &[
            (ObservationKind::UserPrompt, "a", "alpha quokka detail"),
            (ObservationKind::UserPrompt, "b", "beta wombat detail"),
        ],
    )
    .await;

    let mut args = session_observations_args(Some(session_id));
    args.query = Some("quokka".into());
    let page = call_tool_json(
        server
            .memory_read_session_observations(Parameters(args), test_optional_parts())
            .await
            .unwrap(),
    );
    assert_eq!(page["total"], 1);
    assert_eq!(page["observations"].as_array().unwrap().len(), 1);
    assert_eq!(page["observations"][0]["title"], "a");
    assert_eq!(page["elided_other_scope"], 0);
}

#[tokio::test]
async fn memory_query_can_search_multiple_scopes() {
    let (_tmp, store, server, ws, _pj) = setup_server().await;
    let product = store
        .writer
        .get_or_create_project(ws, "product", None)
        .await
        .unwrap();
    let hidden = store
        .writer
        .get_or_create_project(ws, "hidden", None)
        .await
        .unwrap();
    let practice_ws = store
        .writer
        .get_or_create_workspace("practice")
        .await
        .unwrap();
    let testing = store
        .writer
        .get_or_create_project(practice_ws, "unit-testing", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: product,
            path: PagePath::new("product.md").unwrap(),
            title: "Product Rules".into(),
            body: "multi_scope_token belongs to product".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: practice_ws,
            project_id: testing,
            path: PagePath::new("patterns.md").unwrap(),
            title: "Testing Patterns".into(),
            body: "multi_scope_token belongs to practice".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: hidden,
            path: PagePath::new("hidden.md").unwrap(),
            title: "Hidden".into(),
            body: "multi_scope_token must not be returned".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "multi_scope_token".into(),
                limit: Some(10),
                project: None,
                scopes: vec![
                    MemoryScopeArg {
                        project: "product".into(),
                        workspace: "default".into(),
                    },
                    MemoryScopeArg {
                        project: "unit-testing".into(),
                        workspace: "practice".into(),
                    },
                ],
                workspace: None,
                global: None,
                include_expired: None,
                explain: Some(true),
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("product.md"), "expected product hit: {text}");
    assert!(
        text.contains("patterns.md"),
        "expected practice hit: {text}"
    );
    assert!(!text.contains("hidden.md"), "unexpected hidden hit: {text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["streams_active"],
        serde_json::json!(["fts", "entity", "graph"])
    );
    assert_eq!(
        value["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hit| hit.get("score_details").is_some())
            .count(),
        2,
        "every explicit-scope hit must keep its explanation"
    );
}

#[tokio::test]
async fn memory_query_global_searches_all_projects() {
    let (_tmp, store, server, ws, _pj) = setup_server().await;
    let other = store
        .writer
        .get_or_create_project(ws, "infra", None)
        .await
        .unwrap();
    let other_ws = store.writer.get_or_create_workspace("ops").await.unwrap();
    let third = store
        .writer
        .get_or_create_project(other_ws, "runbooks", None)
        .await
        .unwrap();
    for (w, p, path, body) in [
        (ws, other, "cluster.md", "global_token lives in infra"),
        (
            other_ws,
            third,
            "deploy.md",
            "global_token lives in ops runbooks",
        ),
    ] {
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: w,
                project_id: p,
                path: PagePath::new(path).unwrap(),
                title: path.into(),
                body: body.into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
    }
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: other,
            path: PagePath::new("expired.md").unwrap(),
            title: "expired.md".into(),
            body: "global_token expired historical context".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"expires_at": "2020-01-01"}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: Some("2020-01-01T23:59:59Z".parse().unwrap()),
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "global_token".into(),
                limit: Some(10),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: Some(true),
                include_expired: None,
                explain: Some(true),
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    // Both projects (across two workspaces) surface in one global call,
    // each annotated with its project name.
    assert!(text.contains("cluster.md"), "expected infra hit: {text}");
    assert!(text.contains("deploy.md"), "expected ops hit: {text}");
    assert!(
        text.contains("infra"),
        "hit must carry project name: {text}"
    );
    assert!(text.contains("global_hits"), "global hits field: {text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["streams_active"], serde_json::json!(["fts"]));
    assert!(
        value["global_hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit.get("score_details").is_none()),
        "global FTS hits must not claim project RRF provenance"
    );
    assert!(
        !text.contains("expired.md"),
        "expired global hit must be hidden by default: {text}"
    );

    let include_expired = server
        .memory_query(
            Parameters(QueryArgs {
                query: "global_token".into(),
                limit: Some(10),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: Some(true),
                include_expired: Some(true),
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let include_expired_text = include_expired
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        include_expired_text.contains("expired.md"),
        "include_expired must apply to global queries: {include_expired_text}"
    );
}

#[tokio::test]
async fn memory_query_default_global_marker_broadens_unscoped_query() {
    let (_tmp, store, server, ws, _pj) = setup_server().await;
    let infra = store
        .writer
        .get_or_create_project(ws, "infra", None)
        .await
        .unwrap();
    let ops_ws = store.writer.get_or_create_workspace("ops").await.unwrap();
    let runbooks = store
        .writer
        .get_or_create_project(ops_ws, "runbooks", None)
        .await
        .unwrap();
    for (w, p, path, body) in [
        (ws, infra, "cluster.md", "recall_token lives in infra"),
        (
            ops_ws,
            runbooks,
            "deploy.md",
            "recall_token lives in runbooks",
        ),
    ] {
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: w,
                project_id: p,
                path: PagePath::new(path).unwrap(),
                title: path.into(),
                body: body.into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
    }

    // The repo opted into `[recall] default_global` — the hook publishes it
    // on the ActiveProject (single slot here, matching the empty test actor).
    server
        .active_project
        .set_for(&sessionmunch_core::ActorKey::default(), ws, infra, true);

    // A query with NO scoping args now behaves as `global=true`.
    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "recall_token".into(),
                limit: Some(10),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("global_hits"),
        "default_global must route the unscoped query to global: {text}"
    );
    assert!(text.contains("cluster.md"), "infra hit expected: {text}");
    assert!(text.contains("deploy.md"), "runbooks hit expected: {text}");

    // Precedence: an EXPLICIT workspace+project still wins over
    // default_global — the query scopes to runbooks only (no infra hit).
    let scoped = server
        .memory_query(
            Parameters(QueryArgs {
                query: "recall_token".into(),
                limit: Some(10),
                project: Some("runbooks".into()),
                scopes: Vec::new(),
                workspace: Some("ops".into()),
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let scoped_text = scoped
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        scoped_text.contains("deploy.md"),
        "explicit ops/runbooks hit expected: {scoped_text}"
    );
    assert!(
        !scoped_text.contains("cluster.md"),
        "explicit scope must NOT broaden to infra: {scoped_text}"
    );
}

#[tokio::test]
async fn memory_query_global_rejects_explicit_scope() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let err = server
        .memory_query(
            Parameters(QueryArgs {
                query: "x".into(),
                limit: Some(5),
                project: Some("product".into()),
                scopes: Vec::new(),
                workspace: None,
                global: Some(true),
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await;
    assert!(
        err.is_err(),
        "global must not combine with project/workspace/scopes"
    );
}

#[tokio::test]
async fn memory_status_returns_counts() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_status(
            Parameters(StatusArgs {
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("\"pages_latest\": 1"));
}

#[tokio::test]
async fn memory_briefing_returns_structured_snapshot() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_briefing(
            Parameters(BriefingArgs {
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
                settled_first: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    // Spot-check the structural shape — every key must be present
    // so callers don't need to defensively handle missing fields.
    for key in [
        "\"counts\":",
        "\"activity_7d\":",
        "\"activity_30d\":",
        "\"last_observation_at\":",
        "\"pending_handoff_count\":",
        "\"rules\":",
        "\"slots\":",
        "\"recent_pages\":",
    ] {
        assert!(text.contains(key), "missing {key} in briefing:\n{text}");
    }
    // setup_server inserts one page, no sessions/observations,
    // no rules/slots. The activity windows therefore observe zero.
    assert!(
        text.contains("\"sessions\": 0"),
        "expected lifetime sessions: 0\n{text}"
    );
    // settled_first defaults to false: `settled` stays empty and is
    // therefore omitted from the JSON entirely (P4,
    // docs/design-hindsight-borrowings.md §5).
    assert!(
        !text.contains("\"settled\""),
        "settled must be omitted by default:\n{text}"
    );
}

/// P4 (docs/design-hindsight-borrowings.md §5): `settled_first: true`
/// leads the briefing with the project's `rule`/`decision` pages.
/// Tool-level companion to the reader-level ordering test.
#[tokio::test]
async fn memory_briefing_settled_first_surfaces_rule_and_decision_pages() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("decisions/pick-rust.md").unwrap(),
            title: "Pick Rust".into(),
            body: "we chose rust".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let result = server
        .memory_briefing(
            Parameters(BriefingArgs {
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
                settled_first: Some(true),
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("\"settled\":"),
        "settled_first=true must include settled:\n{text}"
    );
    assert!(
        text.contains("decisions/pick-rust.md"),
        "settled must surface the decision page:\n{text}"
    );
}

/// `memory_explore` without an LLM provider configured must
/// degrade to returning the underlying briefing rather than
/// erroring. Mirrors the behaviour of `memory_consolidate`
/// (no provider → clean error/no-op), and matches the design
/// invariant that LLM features are strictly opt-in.
#[tokio::test]
async fn memory_explore_without_llm_degrades_to_briefing() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_explore(
            Parameters(ExploreArgs {
                focus: None,
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("\"prose\": null"),
        "expected null prose\n{text}"
    );
    assert!(
        text.contains("no LLM provider configured"),
        "expected fallback reason\n{text}"
    );
    assert!(
        text.contains("\"briefing\":"),
        "expected briefing payload\n{text}"
    );
}

#[test]
fn explore_gap_bucket_picks_right_label() {
    use sessionmunch_store::BriefingSnapshot;
    // No prior activity → `none`.
    let snap = BriefingSnapshot::default();
    let gap = explore_gap_from_snapshot(&snap);
    assert_eq!(gap.bucket, "none");
    assert!(gap.hours_since_last.is_none());

    // Helper: build a snapshot with last_observation_at N hours ago.
    let snap_at = |hours: i64| -> BriefingSnapshot {
        let ts = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(hours);
        BriefingSnapshot {
            last_observation_at: Some(ts.to_string()),
            ..Default::default()
        }
    };

    let cases = [(2, "today"), (24 * 10, "dormant"), (24 * 60, "stale")];
    for (hours, expected) in cases {
        let g = explore_gap_from_snapshot(&snap_at(hours));
        assert_eq!(
            g.bucket, expected,
            "{hours}h → {expected}, got {}",
            g.bucket
        );
    }
}

#[tokio::test]
async fn memory_recent_returns_one_hit() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(5),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("foo.md"), "expected hit; got {text}");
}

#[tokio::test]
async fn memory_recent_default_global_marker_broadens_to_all_projects() {
    let (_tmp, store, server, ws, _pj) = setup_server().await;
    let infra = store
        .writer
        .get_or_create_project(ws, "infra", None)
        .await
        .unwrap();
    let ops_ws = store.writer.get_or_create_workspace("ops").await.unwrap();
    let runbooks = store
        .writer
        .get_or_create_project(ops_ws, "runbooks", None)
        .await
        .unwrap();
    for (w, p, path) in [(ws, infra, "cluster.md"), (ops_ws, runbooks, "deploy.md")] {
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: w,
                project_id: p,
                path: PagePath::new(path).unwrap(),
                title: path.into(),
                body: "recent body".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
    }
    // Opt into default_global on the (empty) test actor's single slot.
    server
        .active_project
        .set_for(&sessionmunch_core::ActorKey::default(), ws, infra, true);

    let result = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(10),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("global_hits"),
        "default_global must broaden recent across projects: {text}"
    );
    assert!(text.contains("cluster.md"), "{text}");
    assert!(text.contains("deploy.md"), "{text}");

    // An explicit workspace+project still scopes (no cross-project broaden).
    let scoped = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(10),
                project: Some("runbooks".into()),
                workspace: Some("ops".into()),
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let scoped_text = scoped
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(scoped_text.contains("deploy.md"), "{scoped_text}");
    assert!(
        !scoped_text.contains("cluster.md"),
        "explicit scope must not broaden: {scoped_text}"
    );
}

#[tokio::test]
async fn memory_write_page_writes_durable_page() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki);

    // Build a synthetic `Parts` so the new `Extension<Parts>` extractor
    // can be satisfied — no actor headers, so the admission chain
    // gets a default (anonymous) context, same as a stdio caller.
    let parts = axum::http::Request::builder()
        .uri("/mcp")
        .method("POST")
        .body(())
        .unwrap()
        .into_parts()
        .0;
    let result = server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/santander-2025.md".into(),
                body: "# Santander 2025\n\nDurable tax annotation.".into(),
                title: Some("Santander 2025".into()),
                tier: Some("semantic".into()),
                tags: vec!["finance".into()],
                pinned: true,
                project: None,
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("notes/santander-2025.md"), "got {text}");

    let recent = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(5),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let recent_text = recent
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        recent_text.contains("notes/santander-2025.md"),
        "write-page result must be visible to read tools; got {recent_text}"
    );
}

#[tokio::test]
async fn memory_write_page_rejects_workspace_without_project() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki);

    let err = server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/invalid-scope.md".into(),
                body: "# Invalid Scope".into(),
                title: None,
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: None,
                workspace: Some("default".into()),
                scope: None,
                expires_at: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("workspace-only memory_write_page must fail");
    assert!(
        err.to_string()
            .contains("workspace and project must be provided together"),
        "error should explain the required scope pair: {err}"
    );
}

#[tokio::test]
async fn memory_write_page_as_db_user_records_author() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let user_id = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice".into(),
                name: Some("Alice Smith".into()),
                email: Some("alice@example.com".into()),
            },
            sessionmunch_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki);
    let mut parts = test_parts_default();
    parts.extensions.insert(AuthLevel::User);
    parts.extensions.insert(user_id);
    parts.extensions.insert(ActorContext {
        user: Some("alice".into()),
        name: Some("Alice Smith".into()),
        email: Some("alice@example.com".into()),
        ..ActorContext::default()
    });

    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/user-attributed.md".into(),
                body: "# User Attributed\n\nWritten by a normal DB user.".into(),
                title: None,
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: None,
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts),
        )
        .await
        .unwrap();

    let meta = store
        .reader
        .page_meta("default", "scratch", "notes/user-attributed.md")
        .await
        .unwrap()
        .expect("written page should have metadata");
    let author = meta.author.expect("DB user write should carry author");
    assert_eq!(author.username, "alice");
    assert_eq!(author.name.as_deref(), Some("Alice Smith"));
    assert_eq!(author.email.as_deref(), Some("alice@example.com"));
}

fn parts_with_level(level: sessionmunch_core::AuthLevel) -> axum::http::request::Parts {
    let mut parts = test_parts_default();
    parts.extensions.insert(level);
    parts
}

/// Where `memory_write_page` says the page actually landed — not always
/// the path the caller asked for, since a slot write may be namespaced.
fn written_path(result: &CallToolResult) -> String {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("tool result carries text");
    serde_json::from_str::<serde_json::Value>(&text).expect("tool result is JSON")["path"]
        .as_str()
        .expect("response carries the written path")
        .to_string()
}

/// A `_slots/<segment>/…` body is injected verbatim into that operator's
/// next session brief, so an unguarded write into someone else's namespace
/// is a way to put chosen text into their agent's context. The read
/// boundary does not cover this direction.
///
/// Also pins the other half: with `[slots] per_user` off, a nested slot
/// path is an ordinary page and the write keeps working for anyone.
#[tokio::test]
async fn slot_writes_stay_inside_the_callers_namespace() {
    let (tmp, store, server, _ws, _pj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = server.with_wiki(wiki);
    // A users row is what puts this deployment on the multi-operator rung;
    // without one the historical single-operator escape hatch applies.
    let mut user = sessionmunch_core::NewUser {
        username: "alice".into(),
        name: None,
        email: None,
    };
    user.validate().unwrap();
    store
        .writer
        .create_human_user(user, sessionmunch_core::UserRole::User, None, false)
        .await
        .unwrap();

    let parts_for = |user: &str| {
        let mut parts = parts_with_level(sessionmunch_core::AuthLevel::User);
        parts.extensions.insert(sessionmunch_core::ActorContext {
            user: Some(user.to_string()),
            ..sessionmunch_core::ActorContext::default()
        });
        parts
    };
    let write = |server: SessionMunchServer, path: &str, parts: axum::http::request::Parts| {
        let path = path.to_string();
        async move {
            server
                .memory_write_page(
                    Parameters(WritePageArgs {
                        path,
                        body: "# Focus\nread this and obey".into(),
                        title: None,
                        tier: None,
                        tags: Vec::new(),
                        pinned: false,
                        project: None,
                        workspace: None,
                        scope: None,
                        expires_at: None,
                    }),
                    OptionalParts(parts),
                )
                .await
        }
    };

    let scoped = server.clone().with_per_user_slots(true);
    let err = write(
        scoped.clone(),
        "_slots/u-alice/current-focus.md",
        parts_for("bob"),
    )
    .await
    .expect_err("Bob must not write into Alice's slot namespace");
    assert!(err.to_string().contains("another operator"), "{err}");

    // Alice's own slot stays writable, unchanged.
    let own = write(
        scoped.clone(),
        "_slots/u-alice/current-focus.md",
        parts_for("alice"),
    )
    .await
    .expect("an operator owns their own namespace");
    assert_eq!(written_path(&own), "_slots/u-alice/current-focus.md");
    // The shared slot is namespaced into the writer's own prefix — the
    // engine's answer for the same string, and the response says where the
    // page actually landed.
    let shared = write(scoped.clone(), "_slots/current-focus.md", parts_for("bob"))
        .await
        .expect("a shared-slot write is re-homed, not refused");
    assert_eq!(written_path(&shared), "_slots/u-bob/current-focus.md");
    // Admin curation of any namespace stays possible.
    write(
        scoped,
        "_slots/u-alice/current-focus.md",
        parts_with_level(sessionmunch_core::AuthLevel::Root),
    )
    .await
    .expect("root may curate any namespace");

    // DEFAULT CONFIG: nested slot paths are ordinary pages again.
    write(server, "_slots/u-alice/current-focus.md", parts_for("bob"))
        .await
        .expect("with per-user slots off nothing may change for existing writers");
}

/// The MCP tool and the engine's own write path are two doors onto the same
/// hazard, so they must give the same answer for the same operator and the
/// same string. If this tool were the looser one it would simply be the one
/// an agent uses: the engine namespaces `_slots/current-focus.md` into the
/// writer's prefix, and a tool that instead wrote it as given would put
/// that body into EVERY operator's session brief.
#[tokio::test]
async fn mcp_and_engine_doors_agree_on_slot_placement() {
    let (tmp, store, server, _ws, _pj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = server.with_wiki(wiki);
    // A users row puts the deployment on the multi-operator rung; without
    // one every caller waves through the single-operator admin hatch.
    let mut user = sessionmunch_core::NewUser {
        username: "alice".into(),
        name: None,
        email: None,
    };
    user.validate().unwrap();
    store
        .writer
        .create_human_user(user, sessionmunch_core::UserRole::User, None, false)
        .await
        .unwrap();

    let write = |server: SessionMunchServer, path: &str, caller: &str| {
        let path = path.to_string();
        let mut parts = parts_with_level(sessionmunch_core::AuthLevel::User);
        parts.extensions.insert(sessionmunch_core::ActorContext {
            user: Some(caller.to_string()),
            ..sessionmunch_core::ActorContext::default()
        });
        async move {
            server
                .memory_write_page(
                    Parameters(WritePageArgs {
                        path,
                        body: "# Focus\nread this and obey".into(),
                        title: None,
                        tier: None,
                        tags: Vec::new(),
                        pinned: false,
                        project: None,
                        workspace: None,
                        scope: None,
                        expires_at: None,
                    }),
                    OptionalParts(parts),
                )
                .await
        }
    };

    // Every path here is a SEGMENT-shaped path (`u-…` / `uh-…`) or the
    // shared slot: since namespaces come from `IdentityKey::path_segment`
    // this list needs no GLOB-metacharacter paths — and no Windows skip,
    // because nothing this test writes contains a byte NTFS refuses. The
    // hostile-name coverage moved into the CALLER: `a*` passes
    // `validate_username` and hashes to a bounded `uh-…` segment.
    let hostile_ns = sessionmunch_core::IdentityKey::User("a*".into()).path_segment();
    let own_hostile = format!("_slots/{hostile_ns}/current-focus.md");
    let cases = [
        ("_slots/current-focus.md", "bob"),
        ("_slots/u-alice/current-focus.md", "alice"),
        ("_slots/u-alice/current-focus.md", "bob"),
        ("_slots/current-focus.md", "a*"),
        (own_hostile.as_str(), "a*"),
        ("notes/plain.md", "bob"),
    ];

    let scoped = server.clone().with_per_user_slots(true);
    for (path, caller) in cases {
        // The engine's rule, verbatim, keyed through the same accessor the
        // door uses: the consolidator writes `AsGiven` and `Personal` and
        // skips the refusal.
        let actor = sessionmunch_core::ActorContext {
            user: Some(caller.to_string()),
            ..sessionmunch_core::ActorContext::default()
        };
        let engine = sessionmunch_core::slot_placement(path, actor.identity_key().as_ref());
        let door = write(scoped.clone(), path, caller).await;
        match engine {
            sessionmunch_core::SlotPlacement::AsGiven => {
                assert_eq!(
                    written_path(&door.expect("engine writes this one as given")),
                    path,
                    "{path} by {caller}"
                );
            }
            sessionmunch_core::SlotPlacement::Personal(personal) => {
                assert_eq!(
                    written_path(&door.expect("engine re-homes this one")),
                    personal,
                    "{path} by {caller}"
                );
            }
            sessionmunch_core::SlotPlacement::ForeignNamespace => {
                assert!(door.is_err(), "engine refuses this one: {path} by {caller}");
            }
        }
    }

    // DEFAULT CONFIG (`[slots] per_user` off): the engine never consults
    // placement, so neither may this door — every path lands as given.
    for (path, caller) in cases {
        assert_eq!(
            written_path(
                &write(server.clone(), path, caller)
                    .await
                    .expect("with per-user slots off every slot write keeps working")
            ),
            path,
            "{path} by {caller}"
        );
    }
}

#[tokio::test]
async fn memory_read_page_unknown_explicit_project_does_not_fallback() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki);

    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/default.md".into(),
                body: "# Default\n\nThis page must not be read through a typo.".into(),
                title: None,
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: None,
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();

    let err = server
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("notes/default.md".into()),
                project: Some("typo".into()),
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("unknown explicit project must not fall back to scratch");
    assert!(
        err.to_string().contains("typo"),
        "error should name the missing explicit project: {err}"
    );
}

#[tokio::test]
async fn memory_delete_page_removes_the_page() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki);
    let parts = || {
        axum::http::Request::builder()
            .uri("/mcp")
            .method("POST")
            .body(())
            .unwrap()
            .into_parts()
            .0
    };

    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/temp.md".into(),
                body: "# Temp\n\nthrowaway".into(),
                title: Some("Temp".into()),
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: None,
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts()),
        )
        .await
        .unwrap();

    server
        .memory_delete_page(
            Parameters(DeletePageArgs {
                path: "notes/temp.md".into(),
                project: None,
                workspace: None,
            }),
            OptionalParts(parts()),
        )
        .await
        .unwrap();

    // The on-disk file is gone; reading it back errors (file not found).
    let read = server
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("notes/temp.md".into()),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await;
    assert!(read.is_err(), "deleted page must not be readable");

    // Regression: the derived index row must also be gone — the watcher
    // does not reconcile deletions, so a file-only delete would leave the
    // page surfacing in recent/search with stale content.
    let recent = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(10),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let recent_text = recent
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        !recent_text.contains("notes/temp.md"),
        "deleted page must not linger in the index; got {recent_text}"
    );
}

#[tokio::test]
async fn memory_delete_page_unknown_explicit_project_does_not_fallback() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki);

    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/keep.md".into(),
                body: "# Keep\n\nThis page must survive an explicit project typo.".into(),
                title: None,
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: None,
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();

    let err = server
        .memory_delete_page(
            Parameters(DeletePageArgs {
                path: "notes/keep.md".into(),
                project: Some("typo".into()),
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("unknown explicit project must not delete from scratch");
    assert!(
        err.to_string().contains("typo"),
        "error should name the missing explicit project: {err}"
    );

    let read = server
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("notes/keep.md".into()),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await;
    assert!(read.is_ok(), "page must survive delete with typo'd project");
}

/// Bug 5 regression: when a project name lives in MULTIPLE workspaces,
/// `memory_delete_page` without `workspace` resolved scope via
/// `effective_ids(project)` and could silently land in the wrong slot
/// (returning `deleted: true` while the page survived in the workspace
/// the operator actually meant). Passing `workspace` + `project` now
/// flows through `effective_ids_for_read_args` — the same path the read
/// tools use — so the delete lands EXACTLY where the operator pointed.
#[tokio::test]
async fn memory_delete_page_with_explicit_workspace_targets_right_scope() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws_alpha = store.writer.get_or_create_workspace("alpha").await.unwrap();
    let proj_alpha_shared = store
        .writer
        .get_or_create_project(ws_alpha, "shared", None)
        .await
        .unwrap();
    let ws_beta = store.writer.get_or_create_workspace("beta").await.unwrap();
    let proj_beta_shared = store
        .writer
        .get_or_create_project(ws_beta, "shared", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    // Server's baked default is alpha/shared; beta/shared is the
    // sibling we'll target via explicit (workspace, project).
    let server = SessionMunchServer::new(
        store.reader.clone(),
        store.writer.clone(),
        ws_alpha,
        proj_alpha_shared,
    )
    .with_wiki(wiki);
    let parts = || {
        axum::http::Request::builder()
            .uri("/mcp")
            .method("POST")
            .body(())
            .unwrap()
            .into_parts()
            .0
    };

    // Seed both workspaces with a SAME-NAMED page.
    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/twin.md".into(),
                body: "# alpha twin".into(),
                title: None,
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: Some("shared".into()),
                workspace: Some("alpha".into()),
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts()),
        )
        .await
        .unwrap();
    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/twin.md".into(),
                body: "# beta twin".into(),
                title: None,
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: Some("shared".into()),
                workspace: Some("beta".into()),
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts()),
        )
        .await
        .unwrap();

    // Delete from BETA only, explicit scope.
    server
        .memory_delete_page(
            Parameters(DeletePageArgs {
                path: "notes/twin.md".into(),
                project: Some("shared".into()),
                workspace: Some("beta".into()),
            }),
            OptionalParts(parts()),
        )
        .await
        .unwrap();

    // Alpha twin must survive.
    let read_alpha = server
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("notes/twin.md".into()),
                project: Some("shared".into()),
                workspace: Some("alpha".into()),
            }),
            OptionalParts(test_parts_default()),
        )
        .await;
    assert!(
        read_alpha.is_ok(),
        "alpha/shared/notes/twin.md must survive a delete targeting beta"
    );

    // Beta twin must be gone (file-on-disk delete + DB row cleared).
    let read_beta = server
        .memory_read_page(
            Parameters(ReadPageArgs {
                query: None,
                path: Some("notes/twin.md".into()),
                project: Some("shared".into()),
                workspace: Some("beta".into()),
            }),
            OptionalParts(test_parts_default()),
        )
        .await;
    assert!(
        read_beta.is_err(),
        "beta/shared/notes/twin.md must be gone after delete with explicit workspace"
    );

    // Defense-in-depth: the alpha-side IDs survive purge-check (project_id != deleted).
    let _ = proj_beta_shared;
}

#[tokio::test]
async fn memory_write_page_creates_explicit_project() {
    // Bug B regression: an explicit `project` that doesn't exist yet must
    // be created and written to — NOT silently land in the current project.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let baked = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, baked)
        .with_wiki(wiki);
    let parts = || {
        axum::http::Request::builder()
            .uri("/mcp")
            .method("POST")
            .body(())
            .unwrap()
            .into_parts()
            .0
    };

    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "notes/elsewhere.md".into(),
                body: "lands in `other`, not `scratch`".into(),
                title: None,
                tier: Some("semantic".into()),
                tags: vec![],
                pinned: false,
                project: Some("other".into()),
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts()),
        )
        .await
        .unwrap();

    // Visible in `other` (created), absent from the baked `scratch`.
    let in_other = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(5),
                project: Some("other".into()),
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let other_text = in_other
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        other_text.contains("notes/elsewhere.md"),
        "explicit project must be created + written; got {other_text}"
    );

    let in_scratch = server
        .memory_recent(
            Parameters(RecentArgs {
                limit: Some(5),
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let scratch_text = in_scratch
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        !scratch_text.contains("notes/elsewhere.md"),
        "write must not leak into the current project; got {scratch_text}"
    );
}

/// `memory_handoff_begin` must resolve the same project as
/// `memory_briefing` when hooks publish `ActiveProject` (issue #2).
#[tokio::test]
async fn handoff_begin_pending_count_matches_briefing_active_project() {
    let (_tmp, store, server, ws, baked) = setup_server().await;
    let active = store
        .writer
        .get_or_create_project(ws, "sessionmunch", Some(r"C:\GIT\sessionmunch".into()))
        .await
        .unwrap();
    assert_ne!(active, baked, "test needs baked default != active project");
    server.active_project.set(ws, active);

    server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "fix omp CHECK".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                cwd: Some(r"C:\GIT\sessionmunch".into()),
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();

    let briefing = server
        .memory_briefing(
            Parameters(BriefingArgs {
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
                settled_first: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = briefing
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("\"pending_handoff_count\": 1"),
        "briefing should see the handoff in the active project; got {text}",
    );
}

#[tokio::test]
async fn handoff_begin_then_accept_round_trips() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let begin = server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "left mid-refactor of writer actor".into(),
                open_questions: vec!["what max channel size?".into()],
                next_steps: vec!["finish supersession path".into()],
                files_touched: vec!["crates/sessionmunch-store/src/writer.rs".into()],
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let begin_text = begin
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(begin_text.contains("handoff_id"));

    // Accepting with matching cwd returns the handoff.
    let accept = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let accept_text = accept
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(accept_text.contains("left mid-refactor"));
    assert!(accept_text.contains("what max channel size?"));

    // Second accept returns null (handoff is now accepted).
    let again = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let again_text = again
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(again_text.contains("\"handoff\": null"));
}

/// `Handoff` is grouped internally into `HandoffScope`/`HandoffOrigin`/
/// `HandoffContent`/`HandoffLifecycle` sub-structs, each `#[serde(flatten)]`ed
/// so the wire JSON stays flat. This locks that in: the response must keep
/// exactly the pre-grouping key set at `handoff`, with no nested objects.
#[tokio::test]
async fn memory_handoff_accept_response_json_stays_flat() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "wire-shape probe".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                cwd: Some("/tmp/aim-wire".into()),
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let accept = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim-wire".into()),
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let accept_text = accept
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&accept_text).unwrap();
    let handoff = value
        .get("handoff")
        .and_then(serde_json::Value::as_object)
        .expect("handoff must be a flat JSON object");

    let mut keys: Vec<&str> = handoff.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = vec![
        "id",
        "workspace_id",
        "project_id",
        "from_session_id",
        "from_agent",
        "to_agent",
        "cwd",
        "summary",
        "open_questions",
        "next_steps",
        "files_touched",
        "state",
        "created_at",
        "accepted_by",
        "accepted_at",
        "accepted_by_session",
        "owner_user",
        "accepted_by_user",
    ];
    expected.sort_unstable();
    assert_eq!(
        keys, expected,
        "handoff sub-struct grouping must not change the flat MCP wire shape"
    );
    assert!(
        handoff.values().all(|v| !v.is_object()),
        "no field may have become a nested object"
    );
}

#[tokio::test]
async fn handoff_begin_caps_manual_text_after_scrub() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "s".repeat(HANDOFF_SUMMARY_MAX_CHARS + 20),
                open_questions: vec!["q".repeat(HANDOFF_ITEM_MAX_CHARS + 20)],
                next_steps: vec!["n".repeat(HANDOFF_ITEM_MAX_CHARS + 20)],
                files_touched: vec!["f".repeat(HANDOFF_FILE_MAX_CHARS + 20)],
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();

    let accept = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = accept
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("handoff summary truncated"));
    assert!(text.contains("handoff item truncated"));
    assert!(text.contains("handoff file truncated"));
}

#[tokio::test]
async fn handoff_begin_caps_manual_lists_in_aggregate() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let open_questions = (0..100)
        .map(|idx| format!("question-{idx}: {}", "q".repeat(400)))
        .collect();
    server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "contains sk-testsecret12345678901234567890 before cap".into(),
                open_questions,
                next_steps: vec![],
                files_touched: vec![],
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();

    let accept = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let text = accept
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("handoff open_questions truncated"));
    assert!(!text.contains("sk-testsecret"));
}

#[test]
fn handoff_list_cap_keeps_marker_inside_total_budget() {
    let items = (0..10).map(|idx| format!("item-{idx}: {}", "x".repeat(80)));
    let capped = cap_handoff_list(items, 100, 220, "item", "list");
    let rendered_len = capped
        .iter()
        .map(|item| item.chars().count())
        .sum::<usize>()
        .saturating_add(capped.len().saturating_sub(1));
    assert!(rendered_len <= 220);
    assert!(capped.iter().any(|item| item.contains("list truncated")));
}

#[tokio::test]
async fn handoff_begin_accept_honour_explicit_workspace() {
    // Regression for the scope-bleed facet: memory_handoff_begin/accept used
    // to ignore `workspace` (project-only resolution), so a cross-workspace
    // handoff landed in whatever project the contaminable active-project
    // slot pointed at instead of the named (workspace, project). Begin into
    // an explicit sibling workspace, then prove it's there — and NOT in the
    // current (default) project.
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "cross-workspace handoff".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                cwd: None,
                project: Some("sibling-app".into()),
                workspace: Some("djalmajr".into()),
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();

    // The current (default) project must NOT see it.
    let in_default = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: None,
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let in_default_text = in_default
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        in_default_text.contains("\"handoff\": null"),
        "cross-workspace handoff must not bleed into the current project"
    );

    // The explicit (workspace, project) does see it.
    let in_sibling = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: None,
                project: Some("sibling-app".into()),
                workspace: Some("djalmajr".into()),
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let in_sibling_text = in_sibling
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        in_sibling_text.contains("cross-workspace handoff"),
        "handoff must be retrievable from its explicit (workspace, project)"
    );
}

fn tool_json(result: &CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .expect("tool text");
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text not JSON ({e}): {text}"))
}

#[tokio::test]
async fn memory_handoff_list_does_not_claim_then_accept_same_id() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let begin = server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "inspect-without-claim baton".into(),
                open_questions: vec!["still open?".into()],
                next_steps: vec!["claim by id".into()],
                files_touched: vec!["crates/sessionmunch-mcp/src/server.rs".into()],
                cwd: None,
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let handoff_id = tool_json(&begin)["handoff_id"]
        .as_str()
        .expect("begin returns handoff_id")
        .to_string();

    let listed = server
        .memory_handoff_list(
            Parameters(HandoffListArgs {
                any_owner: None,
                limit: None,
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let listed_json = tool_json(&listed);
    let rows = listed_json["handoffs"].as_array().expect("handoffs array");
    assert_eq!(rows.len(), 1, "expected one open handoff: {listed_json}");
    assert_eq!(rows[0]["id"].as_str(), Some(handoff_id.as_str()));
    assert_eq!(
        rows[0]["summary"].as_str(),
        Some("inspect-without-claim baton")
    );
    assert_eq!(rows[0]["open_questions"][0].as_str(), Some("still open?"));
    assert_eq!(rows[0]["state"].as_str(), Some("open"));

    let stored = store
        .reader
        .handoff_by_id(HandoffId::from_str(&handoff_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.lifecycle.state,
        HandoffState::Open,
        "list must not claim the inspected row"
    );
    assert_eq!(stored.scope.workspace_id, ws);
    assert_eq!(stored.scope.project_id, proj);

    let claimed = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: None,
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: Some(handoff_id.clone()),
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let claimed_json = tool_json(&claimed);
    assert_eq!(
        claimed_json["handoff"]["summary"].as_str(),
        Some("inspect-without-claim baton"),
        "first claim of the inspected id must return the body: {claimed_json}"
    );
    let stored = store
        .reader
        .handoff_by_id(HandoffId::from_str(&handoff_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.lifecycle.state, HandoffState::Accepted);

    let again = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: None,
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: Some(handoff_id),
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let again_json = tool_json(&again);
    assert!(
        again_json["handoff"].is_null(),
        "second claim of the same id must return no body: {again_json}"
    );
}

#[tokio::test]
async fn memory_handoff_accept_by_id_leaves_sibling_open() {
    let (_tmp, store, server, _ws, _pj) = setup_server().await;
    let first = server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "first pending baton".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                cwd: None,
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let second = server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "sibling pending baton".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                cwd: None,
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let first_id = tool_json(&first)["handoff_id"]
        .as_str()
        .unwrap()
        .to_string();
    let sibling_id = tool_json(&second)["handoff_id"]
        .as_str()
        .unwrap()
        .to_string();

    let listed = tool_json(
        &server
            .memory_handoff_list(
                Parameters(HandoffListArgs {
                    any_owner: None,
                    limit: None,
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        listed["handoffs"].as_array().map(Vec::len),
        Some(2),
        "both rows must stay open after list: {listed}"
    );

    let claimed = tool_json(
        &server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: None,
                    project: None,
                    workspace: None,
                    any_owner: None,
                    handoff_id: Some(first_id.clone()),
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        claimed["handoff"]["summary"].as_str(),
        Some("first pending baton")
    );

    let first_row = store
        .reader
        .handoff_by_id(HandoffId::from_str(&first_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    let sibling_row = store
        .reader
        .handoff_by_id(HandoffId::from_str(&sibling_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_row.lifecycle.state, HandoffState::Accepted);
    assert_eq!(
        sibling_row.lifecycle.state,
        HandoffState::Open,
        "claiming one inspected id must leave the sibling open"
    );
}

#[tokio::test]
async fn memory_handoff_list_hides_private_baton_but_pages_stay_shared() {
    let (tmp, store, server, ws, proj) = setup_server().await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let bob_owner = sessionmunch_core::IdentityKey::User("bob".into()).storage_key();
    let bob_handoff = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: None,
            summary: "Bob's private prompt-derived context".into(),
            open_questions: vec!["private question".into()],
            next_steps: vec!["private next step".into()],
            files_touched: vec![],
            owner_user: Some(bob_owner),
        })
        .await
        .unwrap();
    let bob_user_id = store
        .writer
        .create_human_user(
            NewUser {
                username: "bob".into(),
                name: Some("Bob".into()),
                email: Some("bob@example.com".into()),
            },
            sessionmunch_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/bob-authored.md").unwrap(),
            title: "Bob authored".into(),
            body: "shared wiki knowledge from Bob".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"title": "Bob authored"}),
            pinned: false,
            links: Vec::new(),
            author_id: Some(bob_user_id),
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let server = server.with_wiki(wiki);

    let mut alice_parts = test_parts_default();
    alice_parts.extensions.insert(AuthLevel::User);
    alice_parts.extensions.insert(ActorContext {
        user: Some("alice".into()),
        ..ActorContext::default()
    });
    let listed = tool_json(
        &server
            .memory_handoff_list(
                Parameters(HandoffListArgs {
                    any_owner: None,
                    limit: None,
                    project: None,
                    workspace: None,
                }),
                OptionalParts(alice_parts.clone()),
            )
            .await
            .unwrap(),
    );
    let rows = listed["handoffs"].as_array().expect("handoffs array");
    assert!(
        rows.is_empty(),
        "Alice must not inspect Bob's private baton: {listed}"
    );
    let stored = store
        .reader
        .handoff_by_id(bob_handoff)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.lifecycle.state, HandoffState::Open);

    let page = tool_json(
        &server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/bob-authored.md".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(alice_parts),
            )
            .await
            .unwrap(),
    );
    assert!(
        page["body"]
            .as_str()
            .is_some_and(|body| body.contains("shared wiki knowledge from Bob")),
        "page reads must stay unfiltered by author: {page}"
    );
}

#[test]
fn grok_session_start_hook_does_not_fetch_handoff() {
    let src = include_str!("../../../../hooks/grok/session-start.sh");
    assert!(
        src.contains("Do NOT fetch /handoff"),
        "Grok SessionStart must keep the capture-only refusal"
    );
    assert!(
        !src.contains("/handoff?"),
        "Grok SessionStart must not fetch the claiming /handoff endpoint"
    );
}

#[tokio::test]
async fn handoff_cancel_expires_open_handoff_and_clears_briefing_count() {
    let (_tmp, store, server, _ws, _pj) = setup_server().await;
    let begin = server
        .memory_handoff_begin(
            Parameters(HandoffBeginArgs {
                summary: "accidental status summary".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                shared: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let begin_text = begin
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    let begin_json: serde_json::Value = serde_json::from_str(&begin_text).unwrap();
    let handoff_id = begin_json["handoff_id"].as_str().unwrap().to_string();

    let before = server
        .memory_briefing(
            Parameters(BriefingArgs {
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
                settled_first: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let before_text = before
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(before_text.contains("\"pending_handoff_count\": 1"));

    let cancel = server
        .memory_handoff_cancel(
            Parameters(HandoffCancelArgs {
                handoff_id: handoff_id.clone(),
                project: None,
                workspace: None,
                any_owner: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let cancel_text = cancel
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(cancel_text.contains("\"cancelled\": true"));
    assert!(cancel_text.contains("\"state\": \"expired\""));

    let after = server
        .memory_briefing(
            Parameters(BriefingArgs {
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
                settled_first: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let after_text = after
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(after_text.contains("\"pending_handoff_count\": 0"));

    let accept = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim".into()),
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .unwrap();
    let accept_text = accept
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(accept_text.contains("\"handoff\": null"));

    let stored = store
        .reader
        .handoff_by_id(HandoffId::from_str(&handoff_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.lifecycle.state, HandoffState::Expired);
}

#[tokio::test]
async fn handoff_cancel_hides_foreign_ids_before_object_state_is_read() {
    let (_tmp, store, server, ws, proj) = setup_server().await;
    let bob_id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: None,
            summary: "Bob's private prompt-derived context".into(),
            open_questions: vec!["private question".into()],
            next_steps: vec!["private next step".into()],
            files_touched: vec![],
            owner_user: Some(sessionmunch_core::IdentityKey::User("bob".into()).storage_key()),
        })
        .await
        .unwrap();

    let mut alice_parts = test_parts_default();
    alice_parts.extensions.insert(AuthLevel::User);
    alice_parts.extensions.insert(ActorContext {
        user: Some("alice".into()),
        ..ActorContext::default()
    });
    let foreign = server
        .memory_handoff_cancel(
            Parameters(HandoffCancelArgs {
                handoff_id: bob_id.to_string(),
                project: None,
                workspace: None,
                any_owner: None,
            }),
            OptionalParts(alice_parts),
        )
        .await
        .expect_err("Alice must not inspect or cancel Bob's handoff");

    let mut alice_parts = test_parts_default();
    alice_parts.extensions.insert(AuthLevel::User);
    alice_parts.extensions.insert(ActorContext {
        user: Some("alice".into()),
        ..ActorContext::default()
    });
    let absent = server
        .memory_handoff_cancel(
            Parameters(HandoffCancelArgs {
                handoff_id: HandoffId::new().to_string(),
                project: None,
                workspace: None,
                any_owner: None,
            }),
            OptionalParts(alice_parts),
        )
        .await
        .expect_err("an absent handoff must fail");

    assert_eq!(
        foreign.to_string(),
        absent.to_string(),
        "a known foreign id must be indistinguishable from an absent id"
    );
    assert!(!foreign.to_string().contains("Bob"));
    assert!(!foreign.to_string().contains("state"));
    assert_eq!(
        store
            .reader
            .handoff_by_id(bob_id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        HandoffState::Open,
    );

    let mut root_parts = test_parts_default();
    root_parts.extensions.insert(AuthLevel::Root);
    let result = server
        .memory_handoff_cancel(
            Parameters(HandoffCancelArgs {
                handoff_id: bob_id.to_string(),
                project: None,
                workspace: None,
                any_owner: Some(true),
            }),
            OptionalParts(root_parts),
        )
        .await
        .expect("root recovery can cancel a foreign handoff");
    let text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|content| content.text.as_str())
        .unwrap_or_default();
    assert!(text.contains("\"cancelled\": true"), "{text}");
}

// ----------------------------------------------------------------
// Error / mis-configured paths — caught at the tool boundary so the
// agent sees a clean McpError instead of a panic.
// ----------------------------------------------------------------

/// `memory_consolidate` is opt-in via the LLM provider. With no
/// consolidator wired, the tool must reject the call with a
/// clear "not configured" error — not panic.
#[tokio::test]
async fn memory_consolidate_without_provider_errors_cleanly() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let err = server
        .memory_consolidate(
            Parameters(ConsolidateArgs {
                session_id: "00000000-0000-0000-0000-000000000000".into(),
                dry_run: Some(true),
                multi_page: Some(false),
                instructions: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("must reject when no consolidator is configured");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("not configured"),
        "error should mention configuration: {msg}",
    );
    assert!(
        msg.contains("provider's required credentials"),
        "error should direct users to provider credentials: {msg}",
    );
    assert!(
        msg.contains("without a built-in model"),
        "error should not imply every provider needs an explicit model: {msg}",
    );
}

#[tokio::test]
async fn memory_auto_improve_removed_dry_run_arg_fails_closed() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let err = server
        .memory_auto_improve(
            Parameters(AutoImproveArgs {
                session_id: Some("00000000-0000-0000-0000-000000000000".into()),
                dry_run: Some(true),
                stage: None,
                mode: None,
                project: None,
                workspace: None,
                min_observations: None,
                min_session_duration_secs: None,
                min_confidence: None,
                max_input_tokens: None,
                max_proposals: None,
                include_raw_fallback: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("removed dry_run argument must fail closed");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("removed"),
        "error should mention removal: {msg}"
    );
}

#[tokio::test]
async fn memory_auto_improve_without_provider_errors_cleanly() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let err = server
        .memory_auto_improve(
            Parameters(AutoImproveArgs {
                session_id: Some("00000000-0000-0000-0000-000000000000".into()),
                dry_run: None,
                stage: None,
                mode: None,
                project: None,
                workspace: None,
                min_observations: None,
                min_session_duration_secs: None,
                min_confidence: None,
                max_input_tokens: None,
                max_proposals: None,
                include_raw_fallback: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("must reject when no LLM provider is configured");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("not configured"),
        "error should mention configuration: {msg}",
    );
}

struct PreflightMustNotCallLlm;

#[async_trait::async_trait]
impl sessionmunch_llm::LlmProvider for PreflightMustNotCallLlm {
    fn name(&self) -> &'static str {
        "preflight-must-not-call"
    }

    fn model(&self) -> &str {
        "preflight-must-not-call"
    }

    async fn complete(
        &self,
        _request: sessionmunch_llm::ChatRequest,
    ) -> sessionmunch_llm::LlmResult<sessionmunch_llm::ChatResponse> {
        panic!("preflight-skipped manual review must not call the LLM")
    }

    async fn complete_structured_raw(
        &self,
        _request: sessionmunch_llm::ChatRequest,
        _schema: serde_json::Value,
    ) -> sessionmunch_llm::LlmResult<serde_json::Value> {
        panic!("preflight-skipped manual review must not call the LLM")
    }
}

fn auto_improve_args(session_id: Option<SessionId>) -> AutoImproveArgs {
    AutoImproveArgs {
        session_id: session_id.map(|id| id.to_string()),
        dry_run: None,
        stage: None,
        mode: None,
        project: None,
        workspace: None,
        min_observations: None,
        min_session_duration_secs: None,
        min_confidence: None,
        max_input_tokens: None,
        max_proposals: None,
        include_raw_fallback: None,
    }
}

async fn seed_short_completed_session(
    store: &Store,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> SessionId {
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id,
            project_id,
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
                workspace_id,
                project_id,
                kind: ObservationKind::SessionStart,
                extension: None,
                source_event: None,
                title: "session start".into(),
                body: String::new(),
                importance: 1,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    store.writer.end_session(session_id, None).await.unwrap();
    session_id
}

#[tokio::test]
async fn implicit_auto_improve_advances_past_preflight_skips() {
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

    let mut sessions = Vec::new();
    for _ in 0..3 {
        sessions.push(seed_short_completed_session(&store, ws, proj).await);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let llm: Arc<dyn LlmProvider> = Arc::new(PreflightMustNotCallLlm);
    let consolidator = Arc::new(Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        llm.clone(),
        ws,
        proj,
    ));
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_consolidator_arc(wiki, llm, consolidator);

    for expected in sessions.iter().rev() {
        let response = call_tool_json(
            server
                .memory_auto_improve(
                    Parameters(auto_improve_args(None)),
                    OptionalParts(test_parts_default()),
                )
                .await
                .unwrap(),
        );
        assert_eq!(response["session_id"], expected.to_string());
        assert_eq!(response["summary"], "session skipped by preflight filters");
    }

    let err = server
        .memory_auto_improve(
            Parameters(auto_improve_args(None)),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("all implicit candidates have persisted review runs");
    assert!(
        err.to_string().contains("no completed session without"),
        "the exhausted queue should explain how to rerun: {err}"
    );

    let explicit = call_tool_json(
        server
            .memory_auto_improve(
                Parameters(auto_improve_args(Some(sessions[0]))),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect("an explicit session remains rerunnable"),
    );
    assert_eq!(explicit["session_id"], sessions[0].to_string());
}

/// Reviewer that always proposes the same two pages, so one of them can be
/// made to collide with an already-pending proposal.
struct TwoProposalAutoImproveLlm;

#[async_trait::async_trait]
impl sessionmunch_llm::LlmProvider for TwoProposalAutoImproveLlm {
    fn name(&self) -> &'static str {
        "fake-auto-improve"
    }

    fn model(&self) -> &str {
        "fake-model"
    }

    async fn complete(
        &self,
        _request: sessionmunch_llm::ChatRequest,
    ) -> sessionmunch_llm::LlmResult<sessionmunch_llm::ChatResponse> {
        Ok(sessionmunch_llm::ChatResponse {
            text: String::new(),
            usage: None,
            model: self.model().to_string(),
        })
    }

    async fn complete_structured_raw(
        &self,
        _request: sessionmunch_llm::ChatRequest,
        _schema: serde_json::Value,
    ) -> sessionmunch_llm::LlmResult<serde_json::Value> {
        Ok(serde_json::json!({
            "summary": "two staged proposals",
            "proposals": [
                {
                    "operation": "create_or_update",
                    "path": "notes/collides.md",
                    "title": "Colliding Lesson",
                    "kind": "note",
                    "confidence": 0.93,
                    "rationale": "A proposal for this page is already pending.",
                    "evidence": [{"page":"session", "quote":"durable lesson"}],
                    "body_markdown": "# Colliding Lesson\n\nsecond proposal"
                },
                {
                    "operation": "create_or_update",
                    "path": "notes/fresh.md",
                    "title": "Fresh Lesson",
                    "kind": "note",
                    "confidence": 0.91,
                    "rationale": "The session contains a durable lesson worth adding.",
                    "evidence": [{"page":"session", "quote":"durable lesson"}],
                    "body_markdown": "# Fresh Lesson\n\nfirst proposal"
                }
            ],
            "rejected_candidates": []
        }))
    }
}

/// Stage one pending proposal for `notes/collides.md` into
/// `pending_bucket`, then run `memory_auto_improve` as `caller` against the
/// same page. Returns the tool's JSON so the caller can see whether the
/// second proposal collided or got its own bucket.
async fn auto_improve_over_pending_target(
    trusted_proxy_identity: bool,
    pending_owner: Option<sessionmunch_core::IdentityKey>,
    caller: Option<&str>,
) -> serde_json::Value {
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
                body: "durable lesson worth capturing".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    store
        .writer
        .stage_auto_improve_run_for_owner(
            StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: None,
                provider: Some("test".into()),
                model: Some("model".into()),
                summary: Some("earlier run".into()),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({}),
                proposal_actor: sessionmunch_core::ActorContext::default(),
                proposals: vec![NewAutoImproveProposal {
                    operation: AutoImproveProposalOperation::Create,
                    target_path: PagePath::new("notes/collides.md").unwrap(),
                    kind: "note".into(),
                    title: "Colliding Lesson".into(),
                    confidence: 0.9,
                    rationale: "staged earlier".into(),
                    evidence_json: serde_json::json!([]),
                    body_markdown: "# Colliding Lesson\n\nfirst proposal".into(),
                    artifact_sha256: None,
                    edit_mode: None,
                    patch_json: None,
                    expected_base_body_sha256: None,
                }],
            },
            pending_owner,
        )
        .await
        .unwrap();

    let llm: Arc<dyn LlmProvider> = Arc::new(TwoProposalAutoImproveLlm);
    let consolidator = Arc::new(Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        llm.clone(),
        ws,
        proj,
    ));
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_consolidator_arc(wiki, llm, consolidator)
        .with_auto_improve_require_approval(true)
        .with_trusted_proxy_identity(trusted_proxy_identity);

    let mut parts = test_parts_default();
    if let Some(user) = caller {
        parts.extensions.insert(sessionmunch_core::ActorContext {
            user: Some(user.to_string()),
            ..sessionmunch_core::ActorContext::default()
        });
    }

    call_tool_json(
        server
            .memory_auto_improve(
                Parameters(AutoImproveArgs {
                    session_id: Some(session_id.to_string()),
                    dry_run: None,
                    stage: None,
                    mode: None,
                    project: None,
                    workspace: None,
                    min_observations: Some(1),
                    min_session_duration_secs: Some(0),
                    min_confidence: Some(0.75),
                    max_input_tokens: None,
                    max_proposals: Some(5),
                    include_raw_fallback: None,
                }),
                OptionalParts(parts),
            )
            .await
            .expect("a collision must not fail the run"),
    )
}

/// A proposal the store refuses to stage must reach the agent. Reporting
/// only `proposal_ids` shows a successful run of N-1 proposals with nothing
/// saying the Nth ever existed — the silent drop the per-proposal skip was
/// introduced to end.
#[tokio::test]
async fn memory_auto_improve_reports_a_collided_proposal_instead_of_dropping_it() {
    // The default parts carry no operator, matching the unattributed
    // bucket the pending proposal was staged into.
    let json = auto_improve_over_pending_target(false, None, None).await;

    let proposals = json["proposals"].as_array().expect("proposals array");
    assert_eq!(proposals.len(), 1, "the sibling proposal still stages");
    let skipped = json["skipped"].as_array().expect("skipped array");
    assert_eq!(skipped.len(), 1, "the collided proposal must be reported");
    assert_eq!(skipped[0]["target_path"], "notes/collides.md");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .is_some_and(|r| !r.trim().is_empty()),
        "a skipped proposal must say why: {}",
        skipped[0]
    );
}

/// `[auth].root_username` alone does not make a server multi-operator. The
/// scheduler and the report handlers stage unattributed, so bucketing an
/// interactive call by its actor would leave TWO pending proposals for one
/// page on a single-operator server — exactly the collision V42 promises
/// cannot happen.
#[tokio::test]
async fn single_operator_auto_improve_shares_one_pending_bucket_with_the_scheduler() {
    let json = auto_improve_over_pending_target(false, None, Some("the-operator")).await;

    let proposals = json["proposals"].as_array().expect("proposals array");
    assert_eq!(proposals.len(), 1, "the sibling proposal still stages");
    let skipped = json["skipped"].as_array().expect("skipped array");
    assert_eq!(
        skipped.len(),
        1,
        "a named root operator must not open a second pending bucket for the same page"
    );
    assert_eq!(skipped[0]["target_path"], "notes/collides.md");
}

/// The other mode: once a trusted proxy tells operators apart, each one
/// gets their own pending slot instead of blocking the others — the point
/// of V42's author-scoped index. The pending bucket is built through the
/// identity contract, the same encoding `owner_identity` produces for the
/// caller.
#[tokio::test]
async fn proxied_operators_each_hold_a_pending_proposal_for_the_same_page() {
    let alice = sessionmunch_core::IdentityKey::User("alice".into());
    let json = auto_improve_over_pending_target(true, Some(alice), Some("bob")).await;

    let proposals = json["proposals"].as_array().expect("proposals array");
    assert_eq!(
        proposals.len(),
        2,
        "a second operator must be able to propose for a page someone else has pending"
    );
    let skipped = json["skipped"].as_array().expect("skipped array");
    assert!(
        skipped.is_empty(),
        "nothing should collide across distinct operators: {skipped:?}"
    );
}

/// `memory_lint` reads the wiki to build its candidate set. With
/// no wiki wired, it must error cleanly.
#[tokio::test]
async fn memory_lint_without_wiki_errors_cleanly() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let err = server
        .memory_lint(
            Parameters(LintArgs {
                dry_run: Some(true),
                no_llm: None,
                project: None,
                workspace: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect_err("must reject when wiki is not attached");
    let msg = format!("{err:?}");
    // The exact phrasing isn't load-bearing; we just need
    // SOMETHING that names the missing dependency so the agent's
    // model has a chance of choosing a different tool.
    assert!(
        msg.contains("wiki") || msg.contains("not configured"),
        "error should explain the missing wiki: {msg}",
    );
}

#[tokio::test]
async fn memory_forget_sweep_targets_the_explicit_project() {
    // Bug C regression: sweep must evaluate the project named in args (or
    // the session's active project), NOT the baked default. An episodic
    // page in `audited` is a sweep candidate only when the sweep points
    // there — never when it runs against the baked `scratch`.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let baked = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = SessionMunchServer::new(store.reader.clone(), store.writer.clone(), ws, baked)
        .with_wiki(wiki);
    let parts = || {
        axum::http::Request::builder()
            .uri("/mcp")
            .method("POST")
            .body(())
            .unwrap()
            .into_parts()
            .0
    };

    server
        .memory_write_page(
            Parameters(WritePageArgs {
                path: "log/ep.md".into(),
                body: "episodic note".into(),
                title: None,
                tier: Some("episodic".into()),
                tags: vec![],
                pinned: false,
                project: Some("audited".into()),
                workspace: None,
                scope: None,
                expires_at: None,
            }),
            OptionalParts(parts()),
        )
        .await
        .unwrap();

    let sweep_count = |args: SweepArgs| {
        let server = &server;
        async move {
            let out = server
                .memory_forget_sweep(Parameters(args), OptionalParts(test_parts_default()))
                .await
                .unwrap();
            let text = out
                .content
                .first()
                .and_then(|c| c.as_text())
                .map(|t| t.text.clone())
                .unwrap();
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["candidates_evaluated"]
                .as_u64()
                .unwrap()
        }
    };

    let audited = sweep_count(SweepArgs {
        dry_run: Some(true),
        project: Some("audited".into()),
        workspace: None,
    })
    .await;
    assert!(
        audited >= 1,
        "sweep of the named project must evaluate its episodic page, got {audited}"
    );

    let baked = sweep_count(SweepArgs {
        dry_run: Some(true),
        project: None,
        workspace: None,
    })
    .await;
    assert_eq!(
        baked, 0,
        "sweep of the baked project must not see another project's page, got {baked}"
    );
}

#[tokio::test]
async fn mcp_admin_capability_honors_trusted_proxy_topology() {
    let (_tmp, _store, server, _ws, _project) = setup_server().await;
    let proxied = server.with_trusted_proxy_identity(true);

    for (level, allowed) in [
        (AuthLevel::Anonymous, false),
        (AuthLevel::User, false),
        (AuthLevel::Root, true),
    ] {
        let mut parts = test_parts_default();
        parts.extensions.insert(level);
        assert_eq!(
            proxied.require_admin_capability(&parts).await.is_ok(),
            allowed,
            "level={level:?}"
        );
    }

    assert!(
        proxied
            .require_admin_capability(&test_parts_default())
            .await
            .is_ok(),
        "stdio/in-process calls have no HTTP auth extension and retain local admin access"
    );
}

/// `memory_handoff_accept` with no pending handoff returns a
/// happy-path `{"handoff": null}` payload (NOT an error). This
/// is the documented contract — the agent can call accept on
/// every session-start without worrying about empty-queue errors.
#[tokio::test]
async fn memory_handoff_accept_when_none_pending_returns_null() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let result = server
        .memory_handoff_accept(
            Parameters(HandoffAcceptArgs {
                cwd: None,
                project: None,
                workspace: None,
                any_owner: None,
                handoff_id: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect("empty-queue must be Ok, not Err");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(
        text.contains("\"handoff\": null"),
        "expected handoff=null in: {text}",
    );
}

/// `memory_query` clamps `limit` into [1, 100]. Anyone sending
/// limit=10000 (DoS attempt or accidental overflow) gets the
/// max instead of an unbounded scan.
#[tokio::test]
async fn memory_query_clamps_outlandish_limit() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    // The clamp is internal; the test verifies the call succeeds
    // with a sane response. (We don't have 10k pages, so the
    // hit count is small — we just need NOT to error.)
    let result = server
        .memory_query(
            Parameters(QueryArgs {
                query: "Karpathy".into(),
                limit: Some(99_999),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await
        .expect("oversized limit should be clamped, not refused");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    // Returns valid JSON even on huge limit.
    let _: serde_json::Value = serde_json::from_str(&text).unwrap();
}

/// `memory_query` with malformed FTS5 must return a clean
/// McpError (NOT panic, NOT bare SQLite error). The FTS5
/// tokenizer treats `-` as a NOT operator and some characters
/// as syntax; an unbalanced quote is the simplest reproducer.
#[tokio::test]
async fn memory_query_malformed_fts5_returns_error() {
    let (_tmp, _store, server, _ws, _pj) = setup_server().await;
    let err = server
        .memory_query(
            Parameters(QueryArgs {
                query: "\"unbalanced".into(),
                limit: Some(10),
                project: None,
                scopes: Vec::new(),
                workspace: None,
                global: None,
                include_expired: None,
                explain: None,
                as_of: None,
                unit: None,
                parent_expansion: None,
                content_budget: None,
            }),
            OptionalParts(test_parts_default()),
        )
        .await;
    // Either a tidy 0-hit Ok (FTS5 is occasionally lenient) or
    // an Err — both are acceptable. A panic is not.
    if let Err(e) = err {
        let msg = format!("{e:?}");
        assert!(
            !msg.is_empty(),
            "error must carry diagnostic text for the agent",
        );
    }
}

#[test]
fn access_bump_throttle_dedups_within_cooldown_and_reallows_after() {
    let cooldown = Duration::from_secs(60);
    let t0 = Instant::now();
    let a = PageId::new();
    let b = PageId::new();
    let c = PageId::new();
    let mut seen = HashMap::new();

    // First sighting of both pages → both due, in input order.
    assert_eq!(
        select_bumpable(&mut seen, vec![a, b], None, t0, cooldown),
        vec![a, b]
    );

    // The same hot pages 30s later (inside the window) → nothing due, so
    // the writer actor is spared the redundant reinforcement writes.
    let t30 = t0 + Duration::from_secs(30);
    assert!(select_bumpable(&mut seen, vec![a, b], None, t30, cooldown).is_empty());

    // A fresh page mixed in with the cooling ones → only the fresh one.
    assert_eq!(
        select_bumpable(&mut seen, vec![a, b, c], None, t30, cooldown),
        vec![c]
    );

    // Past the window, `a` is due again.
    let t90 = t0 + Duration::from_secs(90);
    assert_eq!(
        select_bumpable(&mut seen, vec![a], None, t90, cooldown),
        vec![a]
    );

    // Aged-out entries are pruned, so the map never grows without bound.
    let t_far = t0 + Duration::from_secs(1_000);
    assert!(select_bumpable(&mut seen, Vec::new(), None, t_far, cooldown).is_empty());
    assert!(seen.is_empty(), "aged-out entries must be pruned");
}

/// The throttle is keyed on (page, operator) ON PURPOSE: keyed on the page
/// alone, whoever read it first would swallow everybody else's
/// reinforcement inside the cooldown window, under-counting breadth
/// exactly on the busy pages it matters for.
#[test]
fn access_bump_throttle_is_per_operator_not_per_page() {
    let cooldown = Duration::from_secs(60);
    let t0 = Instant::now();
    let page = PageId::new();
    let mut seen = HashMap::new();

    let alice = sessionmunch_core::IdentityKey::User("alice".into());
    let bob = sessionmunch_core::IdentityKey::User("bob".into());

    // Alice reads first; inside the window Bob's read must still count.
    assert_eq!(
        select_bumpable(&mut seen, vec![page], Some(&alice), t0, cooldown),
        vec![page]
    );
    let t10 = t0 + Duration::from_secs(10);
    assert_eq!(
        select_bumpable(&mut seen, vec![page], Some(&bob), t10, cooldown),
        vec![page],
        "one operator's read must not swallow another's reinforcement"
    );
    // An unattributed read is its own throttle slot, not Alice's.
    assert_eq!(
        select_bumpable(&mut seen, vec![page], None, t10, cooldown),
        vec![page]
    );

    // Each operator is still throttled against THEMSELVES.
    assert!(select_bumpable(&mut seen, vec![page], Some(&alice), t10, cooldown).is_empty());
    assert!(select_bumpable(&mut seen, vec![page], Some(&bob), t10, cooldown).is_empty());
}
#[test]
fn tool_write_classification_fails_toward_write() {
    for read in [
        "memory_query",
        "memory_read_page",
        "memory_recent",
        "memory_briefing",
        "memory_explore",
        "memory_status",
        "memory_install_self_routing",
    ] {
        assert!(!tool_call_is_write(read), "{read}");
    }
    for write in [
        "memory_write_page",
        "memory_delete_page",
        "memory_feedback",
        "memory_consolidate",
        "memory_forget_sweep",
        "memory_handoff_begin",
    ] {
        assert!(tool_call_is_write(write), "{write}");
    }
    // The deliberate default: a tool this list has never met counts as
    // a write, so forgetting to classify a future tool is visible.
    assert!(tool_call_is_write("memory_some_future_tool"));
}

#[test]
fn client_names_are_sanitized_and_bounded() {
    assert_eq!(
        sanitize_client_name("  Visual  \t Studio\nCode  ").as_deref(),
        Some("Visual Studio Code"),
    );
    assert_eq!(
        sanitize_client_name("evil\u{7}name").as_deref(),
        Some("evilname")
    );
    assert_eq!(
        sanitize_client_name("left\u{202e}right").as_deref(),
        Some("leftright"),
        "bidirectional display controls must not reach operator output"
    );
    assert_eq!(sanitize_client_name("   \u{0}\u{1} "), None);
    let long = "x".repeat(500);
    assert_eq!(
        sanitize_client_name(&long).unwrap().chars().count(),
        CLIENT_ACTIVITY_MAX_NAME_CHARS
    );
}

#[test]
fn client_activity_buffer_schedules_once_and_bounds_each_day() {
    let mut buffer = ClientActivityBuffer::new();
    assert!(buffer.record("client-000".into(), 7, false));
    assert!(!buffer.record("client-000".into(), 7, true));
    for idx in 1..CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY {
        assert!(!buffer.record(format!("client-{idx:03}"), 7, false));
    }
    assert!(!buffer.record("overflow-a".into(), 7, false));
    assert!(!buffer.record("overflow-b".into(), 7, true));

    assert_eq!(
        buffer.pending.len(),
        CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY + 1
    );
    assert_eq!(
        buffer
            .pending
            .get(&(CLIENT_ACTIVITY_OVERFLOW_CLIENT.to_string(), 7)),
        Some(&(1, 1))
    );
    assert_eq!(buffer.pending.get(&("client-000".into(), 7)), Some(&(1, 1)));
}

#[test]
fn failed_client_activity_batch_restores_without_breaking_the_bound() {
    let mut buffer = ClientActivityBuffer::new();
    for idx in 0..CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY {
        buffer.record_delta(format!("old-{idx:03}"), 7, 1, 0);
    }
    let failed = buffer.take_entries();

    for idx in 0..CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY {
        buffer.record_delta(format!("new-{idx:03}"), 7, 0, 1);
    }
    buffer.restore_entries(failed);

    assert_eq!(
        buffer.pending.len(),
        CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY + 1,
        "restoring a failed batch must not double the cardinality budget"
    );
    let totals = buffer.pending.values().fold((0_u64, 0_u64), |sum, delta| {
        (sum.0 + u64::from(delta.0), sum.1 + u64::from(delta.1))
    });
    assert_eq!(
        totals,
        (
            CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY as u64,
            CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY as u64
        ),
        "retry coalescing must preserve every delta"
    );
}

#[tokio::test]
async fn client_activity_flushes_without_a_later_tool_call() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let buffer = Arc::new(Mutex::new(ClientActivityBuffer::new()));
    {
        let mut locked = buffer.lock().unwrap();
        assert!(locked.record("quiet-client".into(), 99, false));
    }

    flush_client_activity_loop(buffer.clone(), store.writer.clone(), Duration::ZERO).await;

    let rows = store.reader.client_activity_since(Some(99)).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].client, "quiet-client");
    assert_eq!((rows[0].reads, rows[0].writes), (1, 0));
    let locked = buffer.lock().unwrap();
    assert!(locked.pending.is_empty());
    assert!(!locked.flush_scheduled);
}
