//! Promoted session-owned event ledger and provenance (North Star P1).
//!
//! The portable workstream-event concept is promoted to a session-owned
//! source ledger: hook, import, and managed events converge on one `sessions`
//! row via `begin_session_with_provenance` + `append_session_events`, insertion
//! is idempotent on the stable `event_id`, and provenance (machine, harness,
//! native session, git detail, source) is written best-effort.

use sessionmunch_core::{
    AgentKind, NewSession, NewSessionEvent, ProjectId, SessionId, SessionProvenance, SessionSource,
    WorkspaceId, WorkstreamEventKind,
};
use sessionmunch_store::Store;

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "shared-app".to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

fn new_session(ws: WorkspaceId, proj: ProjectId, id: SessionId) -> NewSession {
    NewSession {
        id,
        workspace_id: ws,
        project_id: proj,
        agent_kind: AgentKind::Hermes,
        cwd: Some("/repo".into()),
        actor_user: Some("user:alice".into()),
    }
}

fn event(id: &str, kind: WorkstreamEventKind, content: &str) -> NewSessionEvent {
    NewSessionEvent {
        event_id: id.to_string(),
        kind,
        role: None,
        content: content.to_string(),
        occurred_at: Some("2026-10-02T00:00:00Z".into()),
        source: SessionSource::Hook,
        metadata: serde_json::json!({
            "model": "deepseek-v4",
            "provider": "openrouter",
        }),
    }
}

#[tokio::test]
async fn provenance_and_events_converge_on_one_session_idempotently() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let session_id = SessionId::new();

    let provenance = SessionProvenance {
        native_session_id: Some("native-1".into()),
        machine_id: Some("sleeper".into()),
        cwd: Some("/repo".into()),
        git_root: Some("/repo".into()),
        git_branch: Some("main".into()),
        commit_at_start: Some("abc123".into()),
        source: SessionSource::Import,
        import_cursor: Some("cursor-1".into()),
    };
    store
        .writer
        .begin_session_with_provenance(new_session(ws, proj, session_id), provenance.clone())
        .await
        .unwrap();

    // First append inserts both events.
    let first = store
        .writer
        .append_session_events(
            session_id,
            vec![
                event("evt-1", WorkstreamEventKind::Message, "hello"),
                event("evt-2", WorkstreamEventKind::ToolCall, "ls: /repo"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(first, 2);

    // Re-appending identical event ids is a no-op (idempotent identity).
    let second = store
        .writer
        .append_session_events(
            session_id,
            vec![
                event("evt-1", WorkstreamEventKind::Message, "hello-rewritten"),
                event("evt-3", WorkstreamEventKind::ToolResult, "done"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(second, 1, "only the genuinely new event must insert");

    let events = store.reader.session_events(session_id, 100).await.unwrap();
    assert_eq!(events.len(), 3);
    // Newest first.
    assert_eq!(events[0].event_id, "evt-3");
    // Idempotency meant the original content won the duplicate.
    let evt1 = events.iter().find(|e| e.event_id == "evt-1").unwrap();
    assert_eq!(evt1.content, "hello");
    assert_eq!(evt1.sequence, 1);
    assert_eq!(evt1.metadata["model"], "deepseek-v4");
    // Sequences are dense and ordered.
    let seqs: Vec<i64> = events.iter().map(|e| e.sequence).collect();
    assert_eq!(
        seqs,
        vec![3, 2, 1],
        "session events must be strictly monotonically sequenced"
    );

    // Provenance is readable and refreshes best-effort without clobbering.
    let updated = SessionProvenance {
        source: SessionSource::Managed,
        import_cursor: Some("cursor-2".into()),
        ..Default::default()
    };
    let changed = store
        .writer
        .update_session_provenance(session_id, updated)
        .await
        .unwrap();
    assert!(changed);
    let row: (Option<String>, Option<String>, String, Option<String>) = {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.query_row(
            "SELECT machine_id, native_session_id, source, import_cursor \
             FROM sessions WHERE id = ?1",
            rusqlite::params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    };
    assert_eq!(
        row,
        (
            Some("sleeper".into()),
            Some("native-1".into()),
            "managed".into(),
            Some("cursor-2".into())
        )
    );
}

#[tokio::test]
async fn a_global_session_with_no_project_and_free_text_harness_works() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch".to_string(), None)
        .await
        .unwrap();
    let session_id = SessionId::new();

    // The promoted path accepts a free-text harness (AgentKind::Other maps to
    // the `other` string here) and provenance; the API requires a project, but
    // the schema permits a NULL one for global/host-wide import rows.
    store
        .writer
        .begin_session_with_provenance(
            NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            },
            SessionProvenance::default(),
        )
        .await
        .unwrap();

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let harness: String = conn
        .query_row(
            "SELECT agent_kind FROM sessions WHERE id = ?1",
            rusqlite::params![session_id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(harness, "other");
}

#[tokio::test]
async fn search_session_events_fts_global_and_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let session_id = SessionId::new();

    store
        .writer
        .begin_session_with_provenance(
            new_session(ws, proj, session_id),
            SessionProvenance {
                native_session_id: Some("native-123".into()),
                cwd: Some("/repo".into()),
                git_root: Some("/repo".into()),
                git_branch: Some("main".into()),
                commit_at_start: Some("abc".into()),
                source: SessionSource::Import,
                machine_id: Some("sleeper".into()),
                import_cursor: None,
            },
        )
        .await
        .unwrap();

    store
        .writer
        .append_session_events(
            session_id,
            vec![
                NewSessionEvent {
                    event_id: "evt-1".into(),
                    kind: WorkstreamEventKind::Message,
                    role: Some("user".into()),
                    content: "How do I configure postgres replica failover?\
Some extra context"
                        .into(),
                    occurred_at: Some("2026-10-02T10:00:00Z".into()),
                    source: SessionSource::Import,
                    metadata: serde_json::json!({"model": "claude-3-7-sonnet"}),
                },
                NewSessionEvent {
                    event_id: "evt-2".into(),
                    kind: WorkstreamEventKind::ToolResult,
                    role: None,
                    content: "Executed pg_ctl promote on replica node 2".into(),
                    occurred_at: Some("2026-10-02T10:01:00Z".into()),
                    source: SessionSource::Import,
                    metadata: serde_json::json!({"tool": "bash"}),
                },
            ],
        )
        .await
        .unwrap();

    // Global search
    let hits = store
        .reader
        .search_session_events("postgres failover".into(), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, session_id);
    assert_eq!(hits[0].event_id, "evt-1");
    assert!(hits[0].snippet.contains("<mark>postgres</mark>"));
    assert_eq!(hits[0].workspace_id, Some(ws));
    assert_eq!(hits[0].project_id, Some(proj));
    assert_eq!(hits[0].harness, "hermes");

    // Scoped search matching
    let scoped_hits = store
        .reader
        .search_session_events_scoped(ws, proj, "\"replica node\"".into(), 10)
        .await
        .unwrap();
    assert_eq!(scoped_hits.len(), 1);
    assert_eq!(scoped_hits[0].event_id, "evt-2");

    // Scoped search with non-matching project ID returns nothing
    let other_proj = ProjectId::new();
    let empty_hits = store
        .reader
        .search_session_events_scoped(ws, other_proj, "\"replica node\"".into(), 10)
        .await
        .unwrap();
    assert!(empty_hits.is_empty());
}

fn observation(
    session_id: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
    kind: sessionmunch_core::ObservationKind,
    title: &str,
    body: &str,
) -> sessionmunch_core::Sanitized<sessionmunch_core::NewObservation> {
    sessionmunch_core::Sanitized::new(
        sessionmunch_core::NewObservation {
            session_id,
            workspace_id: ws,
            project_id: proj,
            kind,
            extension: None,
            source_event: None,
            title: title.into(),
            body: body.into(),
            importance: 5,
        },
        &sessionmunch_core::Sanitizer::default(),
    )
}

#[tokio::test]
async fn hook_observations_mirror_into_the_searchable_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let session_id = SessionId::new();
    store
        .writer
        .begin_session_with_provenance(
            new_session(ws, proj, session_id),
            SessionProvenance {
                machine_id: Some("sleeper".into()),
                ..SessionProvenance::default()
            },
        )
        .await
        .unwrap();
    let prompt = store
        .writer
        .insert_observation(observation(
            session_id,
            ws,
            proj,
            sessionmunch_core::ObservationKind::UserPrompt,
            "prompt",
            "deploy the gryphonlatch service",
        ))
        .await
        .unwrap();
    store
        .writer
        .insert_observation(observation(
            session_id,
            ws,
            proj,
            sessionmunch_core::ObservationKind::PostToolUse,
            "tool non-file",
            "gryphonlatch: connection refused",
        ))
        .await
        .unwrap();

    let events = store.reader.session_events(session_id, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    let first = events.iter().find(|e| e.sequence == 1).unwrap();
    assert_eq!(first.event_id, format!("obs:{prompt}"));
    assert_eq!(first.kind, WorkstreamEventKind::Message);
    assert_eq!(first.role.as_deref(), Some("user"));
    assert_eq!(first.content, "prompt\ndeploy the gryphonlatch service");
    assert_eq!(first.metadata["observation_kind"], "user-prompt");
    let second = events.iter().find(|e| e.sequence == 2).unwrap();
    assert_eq!(second.kind, WorkstreamEventKind::ToolResult);

    let on_sleeper = store
        .reader
        .search_session_events_filtered(sessionmunch_core::SessionEventSearch {
            query: "gryphonlatch".into(),
            limit: 10,
            machine: Some("SLEEPER".into()),
            newest_first: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(on_sleeper.len(), 2, "machine filter is case-insensitive");
    assert_eq!(on_sleeper[0].sequence, 2, "newest first");
    assert_eq!(on_sleeper[0].project.as_deref(), Some("shared-app"));
    assert_eq!(on_sleeper[0].workspace.as_deref(), Some("acme"));
    assert_eq!(on_sleeper[0].cwd.as_deref(), Some("/repo"));
    assert!(on_sleeper[0].created_at > 0);

    let elsewhere = store
        .reader
        .search_session_events_filtered(sessionmunch_core::SessionEventSearch {
            query: "gryphonlatch".into(),
            limit: 10,
            machine: Some("titan".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(elsewhere.is_empty());

    let future = store
        .reader
        .search_session_events_filtered(sessionmunch_core::SessionEventSearch {
            query: "gryphonlatch".into(),
            limit: 10,
            since_micros: Some(i64::MAX - 1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(future.is_empty(), "since filter excludes older events");

    let wrong_harness = store
        .reader
        .search_session_events_filtered(sessionmunch_core::SessionEventSearch {
            query: "gryphonlatch".into(),
            limit: 10,
            harness: Some("codex".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(wrong_harness.is_empty());
}

#[tokio::test]
async fn event_search_applies_the_session_owner_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let alice = SessionId::new();
    store
        .writer
        .begin_session_with_provenance(new_session(ws, proj, alice), SessionProvenance::default())
        .await
        .unwrap();
    store
        .writer
        .append_session_events(
            alice,
            vec![event(
                "a1",
                WorkstreamEventKind::ToolCall,
                "rotate the moonpetal key",
            )],
        )
        .await
        .unwrap();
    let search = |owner| sessionmunch_core::SessionEventSearch {
        query: "moonpetal".into(),
        limit: 10,
        owner,
        ..Default::default()
    };
    let owner_of = |key: &str| Some(sessionmunch_core::OwnerFilter::User(key.into()));
    assert_eq!(
        store
            .reader
            .search_session_events_filtered(search(owner_of("user:alice")))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .reader
            .search_session_events_filtered(search(owner_of("user:bob")))
            .await
            .unwrap()
            .is_empty(),
        "another operator's session events stay private"
    );
    assert!(
        store
            .reader
            .search_session_events_filtered(search(Some(
                sessionmunch_core::OwnerFilter::Unattributed
            )))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .reader
            .search_session_events_filtered(search(None))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_prompt_titled_with_itself_is_stored_once() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let session_id = SessionId::new();
    store
        .writer
        .begin_session_with_provenance(
            new_session(ws, proj, session_id),
            SessionProvenance::default(),
        )
        .await
        .unwrap();
    store
        .writer
        .insert_observation(observation(
            session_id,
            ws,
            proj,
            sessionmunch_core::ObservationKind::UserPrompt,
            "fix the ledger",
            "fix the ledger please",
        ))
        .await
        .unwrap();
    let events = store.reader.session_events(session_id, 10).await.unwrap();
    assert_eq!(events[0].content, "fix the ledger please");
}

#[tokio::test]
async fn ledger_events_attach_to_an_existing_session_whatever_scope_the_importer_guessed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other_proj = store
        .writer
        .get_or_create_project(ws, "guessed-elsewhere".to_string(), None)
        .await
        .unwrap();
    let session_id = SessionId::new();
    store
        .writer
        .begin_session_with_provenance(
            new_session(ws, proj, session_id),
            SessionProvenance::default(),
        )
        .await
        .unwrap();

    // Same session, different guessed project, no owner named: attaches.
    let mut guessed = new_session(ws, other_proj, session_id);
    guessed.actor_user = None;
    store
        .writer
        .begin_session_with_provenance(guessed.clone(), SessionProvenance::default())
        .await
        .expect("scope is not identity");
    store
        .writer
        .append_session_events(
            session_id,
            vec![event(
                "late",
                WorkstreamEventKind::ToolCall,
                "imported later",
            )],
        )
        .await
        .unwrap();
    let hits = store
        .reader
        .search_session_events("imported".into(), 10)
        .await
        .unwrap();
    assert_eq!(
        hits[0].project.as_deref(),
        Some("shared-app"),
        "kept its hook scope"
    );

    // A different harness or a different named owner is a real collision.
    let mut other_agent = guessed.clone();
    other_agent.agent_kind = AgentKind::Codex;
    assert!(matches!(
        store
            .writer
            .begin_session_with_provenance(other_agent, SessionProvenance::default())
            .await,
        Err(sessionmunch_store::StoreError::SessionCollision)
    ));
    let mut other_owner = guessed;
    other_owner.actor_user = Some("user:bob".into());
    assert!(matches!(
        store
            .writer
            .begin_session_with_provenance(other_owner, SessionProvenance::default())
            .await,
        Err(sessionmunch_store::StoreError::SessionCollision)
    ));
}
