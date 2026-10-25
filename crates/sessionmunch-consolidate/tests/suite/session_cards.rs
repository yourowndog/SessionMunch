//! The session-card maintenance tick: imported transcripts get their
//! `sessions/<id>.md` card off the ingest path, in bounded batches.

use sessionmunch_consolidate::run_session_card_tick;
use sessionmunch_core::{
    ActorContext, AgentKind, NewSession, NewSessionEvent, PagePath, ProjectId, SessionId,
    SessionSource, Tier, WorkspaceId, WorkstreamEventKind,
};
use sessionmunch_store::{Store, StoredPageBody};
use sessionmunch_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

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

fn event(
    id: &str,
    kind: WorkstreamEventKind,
    role: &str,
    content: &str,
    source: SessionSource,
) -> NewSessionEvent {
    NewSessionEvent {
        event_id: id.into(),
        kind,
        role: Some(role.into()),
        content: content.into(),
        occurred_at: None,
        source,
        metadata: serde_json::json!({}),
    }
}

fn transcript(tag: &str) -> Vec<NewSessionEvent> {
    vec![
        event(
            &format!("{tag}-u"),
            WorkstreamEventKind::Message,
            "user",
            "Fix the upload bug",
            SessionSource::Import,
        ),
        event(
            &format!("{tag}-a"),
            WorkstreamEventKind::Message,
            "assistant",
            "Fixed: uploads retry now.",
            SessionSource::Import,
        ),
    ]
}

impl Fixture {
    async fn session(&self, events: Vec<NewSessionEvent>) -> SessionId {
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
            .append_session_events(id, events)
            .await
            .unwrap();
        id
    }

    async fn page(&self, id: SessionId) -> Option<StoredPageBody> {
        self.store
            .reader
            .page_body_by_ids(self.ws, self.proj, &format!("sessions/{id}.md"))
            .await
            .unwrap()
    }
}

fn frontmatter(page: &StoredPageBody) -> serde_json::Value {
    serde_json::from_str(&page.frontmatter_json).unwrap()
}

#[tokio::test]
async fn parallel_lanes_write_every_due_card_once() {
    let f = fixture().await;
    let mut ids = Vec::new();
    for n in 0..9 {
        ids.push(f.session(transcript(&format!("p{n}"))).await);
    }
    let tick = run_session_card_tick(&f.store.reader, &f.wiki, None, 20, 4)
        .await
        .unwrap();
    assert_eq!((tick.due, tick.written, tick.failed), (9, 9, 0));
    for id in &ids {
        let page = f.page(*id).await.expect("every session got its card");
        assert_eq!(frontmatter(&page)["generator"], "session-card");
    }
    let settled = run_session_card_tick(&f.store.reader, &f.wiki, None, 20, 4)
        .await
        .unwrap();
    assert_eq!(settled.due, 0);
}

#[tokio::test]
async fn ticks_write_due_cards_in_bounded_batches_and_converge() {
    let f = fixture().await;
    let mut ids = Vec::new();
    for n in 0..7 {
        ids.push(f.session(transcript(&format!("s{n}"))).await);
    }
    let first = run_session_card_tick(&f.store.reader, &f.wiki, None, 3, 1)
        .await
        .unwrap();
    assert_eq!((first.due, first.written, first.failed), (3, 3, 0));
    assert!(
        first.next_after.is_some(),
        "a full batch continues next tick"
    );
    let second = run_session_card_tick(&f.store.reader, &f.wiki, first.next_after, 3, 1)
        .await
        .unwrap();
    assert_eq!(second.written, 3);
    let third = run_session_card_tick(&f.store.reader, &f.wiki, second.next_after, 3, 1)
        .await
        .unwrap();
    assert_eq!(third.written, 1);
    assert_eq!(third.next_after, None, "the pass reached the end and wraps");
    for id in &ids {
        let page = f.page(*id).await.expect("every session got its card");
        assert_eq!(frontmatter(&page)["generator"], "session-card");
    }
    let settled = run_session_card_tick(&f.store.reader, &f.wiki, None, 3, 1)
        .await
        .unwrap();
    assert_eq!(
        settled.due, 0,
        "converged: nothing is due once every card is current"
    );
}

#[tokio::test]
async fn a_card_behind_the_ledger_is_refreshed_with_stable_refs() {
    let f = fixture().await;
    let id = f.session(transcript("a")).await;
    run_session_card_tick(&f.store.reader, &f.wiki, None, 10, 1)
        .await
        .unwrap();
    assert_eq!(
        frontmatter(&f.page(id).await.unwrap())["card"]["last_sequence"],
        2
    );
    f.store
        .writer
        .append_session_events(
            id,
            vec![event(
                "a-x",
                WorkstreamEventKind::Message,
                "assistant",
                "Also added a test.",
                SessionSource::Import,
            )],
        )
        .await
        .unwrap();
    let tick = run_session_card_tick(&f.store.reader, &f.wiki, None, 10, 1)
        .await
        .unwrap();
    assert_eq!(tick.written, 1);
    let page = f.page(id).await.unwrap();
    assert_eq!(frontmatter(&page)["card"]["last_sequence"], 3);
    assert!(page.body.contains("(verbatim, e3)"));
    assert!(
        page.body.contains("- Fix the upload bug (e1)"),
        "earlier refs do not move"
    );
}

#[tokio::test]
async fn the_tick_never_replaces_a_consolidated_or_annotated_page() {
    let f = fixture().await;
    let id = f.session(transcript("c")).await;
    f.wiki
        .write_page(WritePageRequest {
            workspace_id: f.ws,
            project_id: f.proj,
            path: PagePath::new(format!("sessions/{id}.md")).unwrap(),
            frontmatter: serde_json::json!({"title": "Upload fix", "tier": "episodic", "consolidated": true}),
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
    let tick = run_session_card_tick(&f.store.reader, &f.wiki, None, 10, 1)
        .await
        .unwrap();
    assert_eq!(tick.due, 0);
    assert_eq!(
        f.page(id).await.unwrap().body.trim(),
        "A consolidated narrative."
    );
}

#[tokio::test]
async fn hook_mirrors_alone_never_make_a_card_due() {
    let f = fixture().await;
    let hook_only = f
        .session(vec![event(
            "h-u",
            WorkstreamEventKind::Message,
            "user",
            "hello",
            SessionSource::Hook,
        )])
        .await;
    let tick = run_session_card_tick(&f.store.reader, &f.wiki, None, 10, 1)
        .await
        .unwrap();
    assert_eq!(tick.due, 0);
    assert!(f.page(hook_only).await.is_none());
}
