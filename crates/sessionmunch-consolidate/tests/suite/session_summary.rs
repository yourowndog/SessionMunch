//! Summary jobs over a digest, driven by a scripted fake provider.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use serde_json::{Value, json};
use sessionmunch_consolidate::session_summary::{
    JobKey, JobScope, OutcomeStatus, PROMPT_VERSION, SummaryBudgets, SummaryFields, build_jobs,
    summarize, summary_schema,
};
use sessionmunch_core::{SessionEvent, WorkstreamEventKind};
use sessionmunch_llm::{ChatRequest, ChatResponse, LlmError, LlmProvider, LlmResult};
use sessionmunch_workstream::{NativeSignals, SessionDigest, extract_facts};

/// Answers scripted in order; records every prompt it was asked.
pub(crate) struct FakeProvider {
    answers: Mutex<VecDeque<Value>>,
    prompts: Mutex<Vec<String>>,
}

impl FakeProvider {
    pub(crate) fn new(answers: Vec<Value>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("prompts").clone()
    }
}

/// Always answers 429, like a hosted free tier over its limit.
pub(crate) struct RateLimitedProvider;

#[async_trait::async_trait]
impl LlmProvider for RateLimitedProvider {
    fn name(&self) -> &'static str {
        "rate-limited"
    }

    fn model(&self) -> &str {
        "rate-limited-model"
    }

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Err(LlmError::Provider {
            status: 429,
            body: "rate limited".into(),
        })
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: Value,
    ) -> LlmResult<Value> {
        Err(LlmError::Provider {
            status: 429,
            body: "rate limited".into(),
        })
    }
}

#[async_trait::async_trait]
impl LlmProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn model(&self) -> &str {
        "fake-model"
    }

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Err(LlmError::UnexpectedShape(
            "summary jobs use structured output".into(),
        ))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        _schema: Value,
    ) -> LlmResult<Value> {
        let prompt = request
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        self.prompts.lock().expect("prompts").push(prompt);
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .ok_or_else(|| LlmError::UnexpectedShape("no scripted answer left".into()))
    }
}

fn event(kind: WorkstreamEventKind, role: &str, content: &str, metadata: Value) -> SessionEvent {
    SessionEvent {
        // Numbered by `session`, as the ledger numbers rows.
        sequence: 0,
        event_id: format!("native:{content:.16}"),
        kind,
        role: Some(role.into()),
        content: content.into(),
        occurred_at: None,
        metadata,
    }
}

fn user(text: &str) -> SessionEvent {
    event(WorkstreamEventKind::Message, "user", text, json!({}))
}

fn assistant(text: &str) -> SessionEvent {
    event(WorkstreamEventKind::Message, "assistant", text, json!({}))
}

fn bash(command: &str, output: &str, is_error: bool) -> [SessionEvent; 2] {
    [
        event(
            WorkstreamEventKind::ToolCall,
            "assistant",
            &format!("Bash: {}", json!({"command": command})),
            json!({"tool": "Bash"}),
        ),
        event(
            WorkstreamEventKind::ToolResult,
            "tool",
            output,
            json!({"is_error": is_error}),
        ),
    ]
}

type Parts = (SessionDigest, Vec<SessionEvent>);

fn session(mut events: Vec<SessionEvent>) -> Parts {
    for (index, event) in events.iter_mut().enumerate() {
        event.sequence = i64::try_from(index + 1).unwrap();
    }
    let digest = extract_facts("claude", "s1", &events, &[], &NativeSignals::default());
    (digest, events)
}

fn answer(status: &str, outcome_cites: &[&str], next: &str, next_cites: &[&str]) -> Value {
    json!({
        "outcome": {"status": status, "text": format!("Outcome {status}."), "cites": outcome_cites},
        "decisions": [],
        "retractions": [],
        "next_action": {"text": next, "cites": next_cites},
    })
}

fn small() -> Parts {
    let mut events = vec![user("Fix the upload bug")];
    events.extend(bash("cargo t -p uploader", "Summary 3 passed", false));
    events.push(assistant("Fixed; tests pass."));
    session(events)
}

async fn run(
    provider: Option<&FakeProvider>,
    parts: &Parts,
    cache: &mut BTreeMap<String, SummaryFields>,
) -> Option<sessionmunch_consolidate::session_summary::SummaryReport> {
    let provider = provider.map(|p| p as &dyn LlmProvider);
    summarize(
        provider,
        &parts.0,
        &parts.1,
        SummaryBudgets::default(),
        cache,
    )
    .await
    .expect("summarize")
}

#[tokio::test]
async fn no_provider_means_no_summary() {
    let parts = small();
    assert!(run(None, &parts, &mut BTreeMap::new()).await.is_none());
}

#[tokio::test]
async fn a_small_session_is_one_job_and_a_valid_answer_is_kept() {
    let parts = small();
    let provider = FakeProvider::new(vec![json!({
        "outcome": {"status": "done", "text": "The bug was fixed and the tests passed.", "cites": ["e3", "e4"]},
        "decisions": [{"text": "Fix in the uploader, not the client.", "cites": ["e2"]}],
        "retractions": [],
        "next_action": {"text": "none", "cites": []},
    })]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert_eq!(report.calls, 1);
    assert_eq!(report.retries, 0);
    assert!(report.rejections.is_empty());
    let fields = report.fields.expect("fields");
    assert_eq!(fields.outcome.status, OutcomeStatus::Done);
    assert_eq!(fields.decisions.len(), 1);
    let prompt = &provider.prompts()[0];
    assert!(prompt.contains("check test passed [e2,e3]"));
    assert!(prompt.contains("[e3] RESULT exit=0: Summary 3 passed"));
}

#[tokio::test]
async fn rejected_cites_get_one_retry_with_the_reasons() {
    let parts = small();
    let provider = FakeProvider::new(vec![
        answer("done", &["e99"], "none", &[]),
        answer("done", &["e4"], "none", &[]),
    ]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert_eq!(report.calls, 2);
    assert_eq!(report.retries, 1);
    assert_eq!(report.rejections.len(), 1);
    assert!(report.rejections[0].reason.contains("e99"));
    assert!(
        provider.prompts()[1].contains(
            "A validator rejected these items:\n- outcome: cites not in this job's input"
        )
    );
    assert!(report.removed.is_empty());
    assert_eq!(report.fields.expect("fields").outcome.cites, vec!["e4"]);
}

#[tokio::test]
async fn items_still_rejected_after_the_retry_are_removed() {
    let parts = small();
    let bad = json!({
        "outcome": {"status": "done", "text": "Done.", "cites": ["e4"]},
        "decisions": [
            {"text": "Committed the fix to main.", "cites": ["e4"]},
            {"text": "Kept the client unchanged.", "cites": ["e1"]}
        ],
        "retractions": [],
        "next_action": {"text": "Release it.", "cites": []},
    });
    let provider = FakeProvider::new(vec![bad.clone(), bad]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert_eq!(
        report.removed,
        vec!["decisions[0]".to_string(), "next_action".to_string()]
    );
    let fields = report.fields.expect("fields");
    assert_eq!(fields.decisions.len(), 1);
    assert_eq!(fields.decisions[0].text, "Kept the client unchanged.");
    assert!(fields.next_action.text.is_empty());
    let reasons: Vec<&str> = report
        .rejections
        .iter()
        .map(|r| r.reason.as_str())
        .collect();
    assert!(reasons.contains(&"claims a commit or push but the facts show none"));
    assert!(reasons.contains(&"no cites"));
}

#[tokio::test]
async fn outcome_must_agree_with_how_the_session_ended() {
    let parts = session(vec![
        user("audit the hooks"),
        assistant("Starting."),
        user("[Request interrupted by user]"),
    ]);
    let provider = FakeProvider::new(vec![
        answer("done", &["e2"], "none", &[]),
        answer("abandoned", &["e2"], "Resume the hook audit.", &["e1"]),
    ]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert_eq!(
        report.rejections[0].reason,
        "status done but the facts say the session ended interrupted"
    );
    assert_eq!(
        report.fields.expect("fields").outcome.status,
        OutcomeStatus::Abandoned
    );
}

#[tokio::test]
async fn a_pass_claim_against_a_failed_check_is_rejected() {
    let mut events = vec![user("make the tests green")];
    events.extend(bash("pytest", "Exit code 1\n2 failed", true));
    events.push(assistant("Two tests still fail."));
    let parts = session(events);
    let lying = json!({
        "outcome": {"status": "done", "text": "All tests pass now.", "cites": ["e3"]},
        "decisions": [], "retractions": [],
        "next_action": {"text": "none", "cites": []},
    });
    let provider = FakeProvider::new(vec![lying.clone(), lying]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert!(
        report
            .rejections
            .iter()
            .any(|r| r.reason == "claims tests or checks passed but the facts show a failure")
    );
    let fields = report.fields.expect("fields");
    assert_eq!(
        fields.outcome.status,
        OutcomeStatus::Unknown,
        "removed outcome reads unknown"
    );
}

#[tokio::test]
async fn an_anticipated_denial_is_the_intended_result() {
    let mut events = vec![user(
        "Run exactly once: systemctl restart x.service || echo CLAUDE_DENIED > canary.txt",
    )];
    events.extend(bash(
        "systemctl restart x.service || echo CLAUDE_DENIED > canary.txt",
        "PreToolUse:Bash hook error: DENY: systemctl mutation is restricted",
        true,
    ));
    events.push(assistant("The guard blocked it; nothing ran."));
    let parts = session(events);
    let provider = FakeProvider::new(vec![
        answer("abandoned", &["e3"], "Relax the guard policy.", &["e3"]),
        answer("done", &["e3"], "none", &[]),
    ]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert!(
        provider.prompts()[0]
            .contains("EXPECTED-FAILURE signal: request(s) e1 anticipate a denial")
    );
    assert!(report.rejections[0].reason.contains("EXPECTED-FAILURE"));
    assert_eq!(
        report.fields.expect("fields").outcome.status,
        OutcomeStatus::Done
    );
}

fn long_session() -> Parts {
    // Assistant turns are clipped to 1,200 chars (~300 tokens), so 3 x 15 of
    // them put the session over the 12k-token one-job budget.
    let filler = "word ".repeat(400);
    let mut events = Vec::new();
    for n in 1..=3 {
        events.push(user(&format!("Request number {n}: build part {n}")));
        for _ in 0..15 {
            events.push(assistant(&filler));
            events.extend(bash("make", &"log line\n".repeat(50), false));
        }
    }
    events.push(assistant("All three parts built."));
    session(events)
}

#[tokio::test]
async fn long_sessions_map_per_request_then_reduce_and_keep_open_steps() {
    let parts = long_session();
    let jobs = build_jobs(&parts.0, &parts.1, SummaryBudgets::default());
    let scopes: Vec<JobScope> = jobs.iter().map(|j| j.key.scope).collect();
    assert_eq!(
        scopes,
        vec![
            JobScope::Segment(1),
            JobScope::Segment(2),
            JobScope::Segment(3)
        ]
    );
    let segment = |n: usize, next: &str| {
        let cite = format!("e{}", jobs[n].allowed.iter().next().copied().unwrap_or(1));
        let next_cites = if next == "none" {
            vec![]
        } else {
            vec![cite.clone()]
        };
        json!({
            "outcome": {"status": "partial", "text": "Part built.", "cites": [cite]},
            "decisions": [], "retractions": [],
            "next_action": {"text": next, "cites": next_cites},
        })
    };
    let provider = FakeProvider::new(vec![
        segment(0, "none"),
        segment(1, "Bump the version."),
        segment(2, "Wire part 2 into the release script."),
        answer("done", &["e1"], "none", &[]),
    ]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert_eq!(report.calls, 4);
    let reduce_prompt = provider.prompts()[3].clone();
    assert!(reduce_prompt.contains("FINAL SEGMENT SUMMARY (the latest state"));
    assert!(reduce_prompt.contains("Wire part 2 into the release script."));
    let fields = report.fields.expect("fields");
    assert!(
        report.next_action_from_segment,
        "the final segment's open step survives the reduce"
    );
    assert_eq!(
        fields.next_action.text,
        "Wire part 2 into the release script."
    );
}

#[tokio::test]
async fn a_step_a_later_segment_finished_is_not_restored() {
    let parts = long_session();
    let jobs = build_jobs(&parts.0, &parts.1, SummaryBudgets::default());
    let segment = |n: usize, next: &str| {
        let cite = format!("e{}", jobs[n].allowed.iter().next().copied().unwrap_or(1));
        let next_cites = if next == "none" {
            vec![]
        } else {
            vec![cite.clone()]
        };
        json!({
            "outcome": {"status": "partial", "text": "Part built.", "cites": [cite]},
            "decisions": [], "retractions": [],
            "next_action": {"text": next, "cites": next_cites},
        })
    };
    let provider = FakeProvider::new(vec![
        segment(0, "none"),
        segment(1, "Wire part 2 into the release script."),
        segment(2, "none"),
        answer("done", &["e1"], "none", &[]),
    ]);
    let report = run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    assert!(!report.next_action_from_segment);
    assert_eq!(report.fields.expect("fields").next_action.text, "none");
}

#[tokio::test]
async fn the_reduce_prompt_fits_the_budget_with_every_segment_naming_a_step() {
    let filler = "word ".repeat(400);
    let mut events = Vec::new();
    for n in 1..=30 {
        events.push(user(&format!("Request number {n}: build part {n}")));
        for _ in 0..3 {
            events.push(assistant(&filler));
        }
    }
    let parts = session(events);
    let budgets = SummaryBudgets::default();
    let jobs = build_jobs(&parts.0, &parts.1, budgets);
    let step = "Follow up on this part: ".to_string() + &"detail ".repeat(300);
    let mut answers: Vec<Value> = jobs
        .iter()
        .map(|job| {
            let cite = format!("e{}", job.allowed.iter().next().copied().unwrap_or(1));
            json!({
                "outcome": {"status": "partial", "text": "Part built.", "cites": [cite]},
                "decisions": [], "retractions": [],
                "next_action": {"text": step, "cites": [cite]},
            })
        })
        .collect();
    answers.push(answer("partial", &["e1"], "none", &[]));
    let provider = FakeProvider::new(answers);
    run(Some(&provider), &parts, &mut BTreeMap::new())
        .await
        .expect("report");
    let reduce_prompt = provider.prompts().last().cloned().expect("reduce prompt");
    // The default segment budget: 20k tokens.
    assert!(
        reduce_prompt.len() / 4 <= 20_000,
        "reduce prompt is {} tokens",
        reduce_prompt.len() / 4
    );
}

#[tokio::test]
async fn reruns_with_the_same_inputs_make_no_calls() {
    let parts = small();
    let mut cache = BTreeMap::new();
    let provider = FakeProvider::new(vec![answer("done", &["e4"], "none", &[])]);
    run(Some(&provider), &parts, &mut cache)
        .await
        .expect("report");
    let again = FakeProvider::new(Vec::new());
    let report = run(Some(&again), &parts, &mut cache).await.expect("report");
    assert_eq!(report.calls, 0);
    assert_eq!(
        report.fields.expect("fields").outcome.status,
        OutcomeStatus::Done
    );
}

#[test]
fn job_keys_are_stable_and_change_with_scope_input_and_versions() {
    let parts = small();
    let job = &build_jobs(&parts.0, &parts.1, SummaryBudgets::default())[0];
    let same = &build_jobs(&parts.0, &parts.1, SummaryBudgets::default())[0];
    assert_eq!(job.key.id(), same.key.id());
    assert_eq!(job.key.prompt_version, PROMPT_VERSION);
    let mut other = job.key.clone();
    other.scope = JobScope::Segment(1);
    assert_ne!(job.key.id(), other.id());
    let mut bumped: JobKey = job.key.clone();
    bumped.prompt_version += 1;
    assert_ne!(job.key.id(), bumped.id());
    let mut grown = parts.1.clone();
    grown.push(assistant("One more note."));
    let grown = session(grown);
    assert_ne!(
        job.key.id(),
        build_jobs(&grown.0, &grown.1, SummaryBudgets::default())[0]
            .key
            .id()
    );
}

#[test]
fn schema_requires_the_four_fields_and_caps_decisions() {
    let schema = summary_schema();
    assert_eq!(
        schema["required"],
        json!(["outcome", "decisions", "retractions", "next_action"])
    );
    assert_eq!(schema["properties"]["decisions"]["maxItems"], 3);
    assert_eq!(schema["additionalProperties"], false);
}

// ------------------------------------------------------- the summary tick

mod tick {
    use serde_json::json;
    use sessionmunch_consolidate::run_session_card_tick;
    use sessionmunch_consolidate::session_summary::{SummaryBudgets, run_session_summary_tick};
    use sessionmunch_core::{
        ActorContext, AgentKind, NewSession, NewSessionEvent, PagePath, ProjectId, SessionId,
        SessionSource, Tier, WorkspaceId, WorkstreamEventKind,
    };
    use sessionmunch_store::{Store, StoredPageBody};
    use sessionmunch_wiki::{Wiki, WritePageRequest};
    use tempfile::TempDir;

    use super::{FakeProvider, RateLimitedProvider};

    struct Fixture {
        _dir: TempDir,
        store: Store,
        wiki: Wiki,
        ws: WorkspaceId,
        proj: ProjectId,
    }

    async fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("acme").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let wiki = Wiki::new(dir.path(), store.writer.clone()).unwrap();
        Fixture {
            _dir: dir,
            store,
            wiki,
            ws,
            proj,
        }
    }

    fn ledger(id: &str, role: &str, content: &str) -> NewSessionEvent {
        NewSessionEvent {
            event_id: id.into(),
            kind: WorkstreamEventKind::Message,
            role: Some(role.into()),
            content: content.into(),
            occurred_at: None,
            source: SessionSource::Import,
            metadata: json!({}),
        }
    }

    impl Fixture {
        /// A session with a current card, as the card tick leaves it.
        async fn carded_session(&self, tag: &str) -> SessionId {
            let id = SessionId::new();
            self.store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: self.ws,
                    project_id: self.proj,
                    agent_kind: AgentKind::Codex,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            self.store
                .writer
                .append_session_events(
                    id,
                    vec![
                        ledger(&format!("{tag}-u"), "user", "Fix the upload bug"),
                        ledger(
                            &format!("{tag}-a"),
                            "assistant",
                            "Fixed: uploads retry now.",
                        ),
                    ],
                )
                .await
                .unwrap();
            run_session_card_tick(&self.store.reader, &self.wiki, None, 50, 1)
                .await
                .unwrap();
            id
        }

        async fn page(&self, id: SessionId) -> StoredPageBody {
            self.store
                .reader
                .page_body_by_ids(self.ws, self.proj, &format!("sessions/{id}.md"))
                .await
                .unwrap()
                .unwrap()
        }

        async fn tick(
            &self,
            provider: &FakeProvider,
            limit: usize,
        ) -> sessionmunch_consolidate::SessionSummaryTick {
            run_session_summary_tick(
                &self.store.reader,
                &self.store.writer,
                &self.wiki,
                &[sessionmunch_consolidate::SummaryLane { provider, slots: 1 }],
                SummaryBudgets::default(),
                None,
                limit,
            )
            .await
            .unwrap()
        }
    }

    fn good() -> serde_json::Value {
        json!({
            "outcome": {"status": "done", "text": "Uploads retry now.", "cites": ["e2"]},
            "decisions": [{"text": "Retry in the uploader, not the client.", "cites": ["e1"]}],
            "retractions": [],
            "next_action": {"text": "none", "cites": []},
        })
    }

    #[tokio::test]
    async fn answers_accepted_before_a_provider_error_are_kept() {
        let f = fixture().await;
        let id = SessionId::new();
        f.store
            .writer
            .begin_session(NewSession {
                id,
                workspace_id: f.ws,
                project_id: f.proj,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        let filler = "word ".repeat(400);
        let mut rows = Vec::new();
        for n in 1..=3 {
            rows.push(ledger(
                &format!("u{n}"),
                "user",
                &format!("Request {n}: build part {n}"),
            ));
            for k in 0..20 {
                rows.push(ledger(
                    &format!("a{n}-{k}"),
                    "assistant",
                    &format!("{k} {filler}"),
                ));
            }
        }
        f.store
            .writer
            .append_session_events(id, rows)
            .await
            .unwrap();
        run_session_card_tick(&f.store.reader, &f.wiki, None, 50, 1)
            .await
            .unwrap();
        // Each segment opens with its request: e1, e22, e43.
        let segment = |cite: &str| {
            json!({
                "outcome": {"status": "partial", "text": "Part built.", "cites": [cite]},
                "decisions": [], "retractions": [],
                "next_action": {"text": "none", "cites": []},
            })
        };
        // Three segment answers, then nothing left for the reduce: an error.
        let first = FakeProvider::new(vec![segment("e1"), segment("e22"), segment("e43")]);
        let tick = f.tick(&first, 5).await;
        assert_eq!((tick.failed, tick.calls), (1, 0), "{tick:?}");
        assert_eq!(
            first.prompts().len(),
            4,
            "three segments, then the failed reduce"
        );
        let reduce = json!({
            "outcome": {"status": "done", "text": "All parts built.", "cites": ["e1"]},
            "decisions": [], "retractions": [],
            "next_action": {"text": "none", "cites": []},
        });
        let second = FakeProvider::new(vec![reduce]);
        let tick = f.tick(&second, 5).await;
        assert_eq!(tick.written, 1, "{tick:?}");
        assert_eq!(second.prompts().len(), 1, "only the reduce is asked again");
    }

    #[tokio::test]
    async fn the_tick_adds_a_labelled_cited_section_and_keeps_the_card() {
        let f = fixture().await;
        let id = f.carded_session("a").await;
        let card = f.page(id).await.body;
        let provider = FakeProvider::new(vec![good()]);
        let tick = f.tick(&provider, 5).await;
        assert_eq!((tick.due, tick.written, tick.calls), (1, 1, 1));
        let page = f.page(id).await;
        assert!(
            page.body.starts_with(card.trim_end()),
            "the deterministic card is untouched"
        );
        assert!(page.body.contains("## AI summary"));
        assert!(page.body.contains("Written by `fake-model`"));
        assert!(
            page.body
                .contains("- **Outcome (done):** Uploads retry now. (e2)")
        );
        assert!(
            page.body
                .contains("  - Retry in the uploader, not the client. (e1)")
        );
        let fm: serde_json::Value = serde_json::from_str(&page.frontmatter_json).unwrap();
        assert_eq!(fm["ai_summary"]["model"], "fake-model");
        assert_eq!(fm["ai_summary"]["status"], "summarized");
        assert_eq!(
            f.tick(&provider, 5).await.due,
            0,
            "a summarized card is not due again"
        );
    }

    #[tokio::test]
    async fn a_rewritten_card_gets_its_summary_back_from_the_cache_with_no_calls() {
        let f = fixture().await;
        let id = f.carded_session("b").await;
        f.tick(&FakeProvider::new(vec![good()]), 5).await;
        // SessionEnd (or a newer builder) rewrites the card without the section.
        let (ws, proj) = (f.ws, f.proj);
        let page = sessionmunch_consolidate::session_card_page(
            &f.store.reader,
            ws,
            proj,
            id,
            AgentKind::Codex,
        )
        .await
        .unwrap()
        .unwrap();
        f.wiki
            .write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: page.path,
                frontmatter: page.frontmatter_json,
                body: page.body,
                tier: Tier::Episodic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::default(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
        assert!(!f.page(id).await.body.contains("## AI summary"));
        let silent = FakeProvider::new(Vec::new());
        let tick = f.tick(&silent, 5).await;
        assert_eq!(
            (tick.written, tick.calls),
            (1, 0),
            "cache hit: no provider calls"
        );
        assert!(
            f.page(id)
                .await
                .body
                .contains("- **Outcome (done):** Uploads retry now. (e2)")
        );
    }

    #[tokio::test]
    async fn items_with_bad_cites_are_removed_and_listed() {
        let f = fixture().await;
        let id = f.carded_session("c").await;
        let bad = json!({
            "outcome": {"status": "done", "text": "Uploads retry now.", "cites": ["e2"]},
            "decisions": [{"text": "Rewrote the client.", "cites": ["e999"]}],
            "retractions": [],
            "next_action": {"text": "none", "cites": []},
        });
        let provider = FakeProvider::new(vec![bad.clone(), bad]);
        let tick = f.tick(&provider, 5).await;
        assert_eq!(tick.calls, 2, "one retry with the validator's reasons");
        let body = f.page(id).await.body;
        assert!(!body.contains("Rewrote the client."));
        assert!(body.contains("- **Not summarized (removed by the validator):**"));
        assert!(body.contains("`decisions[0]`: cites not in this job's input"));
    }

    #[tokio::test]
    async fn parallel_lanes_summarize_every_due_session() {
        let f = fixture().await;
        for tag in ["d", "e", "g", "h"] {
            f.carded_session(tag).await;
        }
        let provider = FakeProvider::new(vec![good(), good(), good(), good()]);
        let tick = run_session_summary_tick(
            &f.store.reader,
            &f.store.writer,
            &f.wiki,
            &[sessionmunch_consolidate::SummaryLane {
                provider: &provider,
                slots: 3,
            }],
            SummaryBudgets::default(),
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!((tick.due, tick.written, tick.failed), (4, 4, 0));
        assert_eq!(tick.calls, 4);
    }

    #[tokio::test]
    async fn a_rate_limited_lane_hands_its_sessions_to_the_others() {
        let f = fixture().await;
        for tag in ["d", "e", "g"] {
            f.carded_session(tag).await;
        }
        let good_lane = FakeProvider::new(vec![good(), good(), good()]);
        let tick = run_session_summary_tick(
            &f.store.reader,
            &f.store.writer,
            &f.wiki,
            &[
                sessionmunch_consolidate::SummaryLane {
                    provider: &RateLimitedProvider,
                    slots: 2,
                },
                sessionmunch_consolidate::SummaryLane {
                    provider: &good_lane,
                    slots: 1,
                },
            ],
            SummaryBudgets::default(),
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            (tick.due, tick.written, tick.failed),
            (3, 3, 0),
            "a 429 is not a failure; another lane takes the session"
        );
    }

    #[tokio::test]
    async fn the_tick_is_bounded() {
        let f = fixture().await;
        for tag in ["d", "e", "g"] {
            f.carded_session(tag).await;
        }
        let provider = FakeProvider::new(vec![good(), good(), good()]);
        let first = f.tick(&provider, 2).await;
        assert_eq!((first.due, first.written), (2, 2));
        assert!(first.next_after.is_some());
        let rest = run_session_summary_tick(
            &f.store.reader,
            &f.store.writer,
            &f.wiki,
            &[sessionmunch_consolidate::SummaryLane {
                provider: &provider,
                slots: 1,
            }],
            SummaryBudgets::default(),
            first.next_after,
            2,
        )
        .await
        .unwrap();
        assert_eq!((rest.due, rest.written, rest.next_after), (1, 1, None));
    }

    #[tokio::test]
    async fn a_consolidated_page_never_gets_a_summary() {
        let f = fixture().await;
        let id = f.carded_session("h").await;
        f.wiki
            .write_page(WritePageRequest {
                workspace_id: f.ws,
                project_id: f.proj,
                path: PagePath::new(format!("sessions/{id}.md")).unwrap(),
                frontmatter: json!({"title": "Upload fix", "tier": "episodic", "consolidated": true}),
                body: "A consolidated narrative.".into(),
                tier: Tier::Episodic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::default(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
        let tick = f.tick(&FakeProvider::new(vec![good()]), 5).await;
        assert_eq!(tick.due, 0);
        assert_eq!(f.page(id).await.body.trim(), "A consolidated narrative.");
    }
}
