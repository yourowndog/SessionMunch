//! Focused unit tests for the load-bearing mutating SQL paths.
//!
//! `Store::open` exercises these incidentally through
//! integration tests, but specific edges — supersession on body
//! change, no-op on identical body, handoff state transitions,
//! end_session summary linkage, embedding PK-replacement —
//! deserve direct coverage so a regression surfaces with a
//! one-line diff instead of a cascading e2e failure.
use super::*;
use rusqlite::Connection;
use sessionmunch_core::{
    FeedbackKind, LinkTarget, NewHandoff, NewPage, NewSession, PageEvidence, PageEvidenceKind,
    PagePath, ProjectId, Tier, UserId, WorkspaceId,
};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

static WARNING_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

impl Write for CapturedLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CapturedLogWriter(Arc::clone(&self.0))
    }
}

/// A subscriber that exists only to be counted (#558).
///
/// `tracing` resolves a callsite's interest **once**, on first execution,
/// and caches the answer process-globally. While only one `Dispatch` is
/// alive — which `WARNING_CAPTURE_LOCK` guarantees, since it serialises the
/// captures — `tracing-core` takes a fast path that resolves that answer
/// from the *registering thread's own* default subscriber
/// (`callsite.rs`: `if has_just_one { … dispatcher::get_default(f) }`).
/// A thread without one resolves to `Interest::never()`, and every later
/// capture of that callsite goes blind.
///
/// Keeping a second `Dispatch` alive for the life of the test binary
/// defeats that: with more than one registered, resolution consults every
/// live dispatcher instead of only the current thread's, so the capture's
/// own subscriber counts no matter which thread registers first.
///
/// **Being counted is the whole contribution.** It is never installed as
/// anyone's default and never receives an event. Verified by control: see
/// `a_subscriberless_thread_must_not_disable_a_callsite_the_capture_needs`,
/// which fails when this pin is removed and passes with it, whatever this
/// subscriber answers.
///
/// The two ways it could stop being invisible are not symmetric, and the
/// difference is easy to get backwards:
///
/// - **Interest is not a maximum.** `Interest::and` keeps the value when
///   two dispatchers agree and collapses to `sometimes()` when they differ.
///   This pin answers `never` (its `enabled` is false, and the default
///   `register_callsite` follows that), so pairing it with a capture that
///   answers `always` yields `sometimes` — a per-event `enabled()` call,
///   answered by whichever subscriber is actually in scope. That is why an
///   inert pin cannot silence a callsite someone else wants.
/// - **The level hint *is* a maximum**, and a `None` hint is read as
///   `TRACE`. Hence the explicit `Some(OFF)` below.
struct RegistryPin;

impl tracing::Subscriber for RegistryPin {
    /// Explicit, and load-bearing — see
    /// `the_registry_pin_stays_invisible_to_the_global_max_level`.
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::OFF)
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Holds the second dispatcher alive. Constructing a `Dispatch` is what
/// registers it, so simply keeping this initialised is the mechanism.
static REGISTRY_PIN: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();

fn capture_warnings(run: impl FnOnce()) -> String {
    REGISTRY_PIN.get_or_init(|| tracing::Dispatch::new(RegistryPin));
    let _guard = WARNING_CAPTURE_LOCK.lock().unwrap();
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(logs.clone())
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
}

/// Guards the `max_level_hint` override on [`RegistryPin`] against being
/// removed as apparent dead code.
///
/// The pin has to be *registered* to do its job, and every registered
/// dispatcher feeds `tracing`'s global max level — which `rebuild_interest`
/// computes as the maximum across live dispatchers, reading a `None` hint
/// as `TRACE`. Dropping the override therefore raises this binary's global
/// level from `OFF` to `TRACE` for good after the first capture. Measured
/// both ways: with `Some(OFF)` the level follows exactly the `OFF -> WARN`
/// path it follows with no pin at all; without the override it goes to
/// `TRACE` and stays there.
///
/// Nothing observable breaks when that happens, which is precisely why it
/// wants a guard rather than a comment — every `trace!` and `debug!`
/// callsite in the crate would quietly start being evaluated instead of
/// short-circuiting.
#[test]
fn the_registry_pin_stays_invisible_to_the_global_max_level() {
    use tracing::Subscriber as _;

    assert_eq!(
        RegistryPin.max_level_hint(),
        Some(tracing::level_filters::LevelFilter::OFF),
        "a None hint is read as TRACE, which would raise the whole binary's \
             global max level as a side effect of pinning the dispatcher"
    );
}

/// A `warn!` callsite reachable from nowhere but the control test below.
///
/// The defect being reproduced is in `tracing`'s **one-shot** callsite
/// registration, so the callsite has to still be unregistered when the
/// control runs. Any second caller would register it first and make the
/// control vacuous.
fn control_only_warn() {
    tracing::warn!("control-callsite-canary");
}

/// #558: a thread with no subscriber must not be able to disable a
/// callsite that a concurrent capture depends on.
///
/// The flake is `Interest::never()` cached **globally** for a callsite,
/// resolved from whichever thread happens to register it first:
///
/// - `tracing`'s event macro gates on the global max level *before*
///   consulting the callsite's interest. With no other subscriber in this
///   binary that level starts at `OFF`, so a subscriberless thread normally
///   short-circuits and never registers anything.
/// - While a capture holds a live `Dispatch`, the level is `WARN`, and now
///   a subscriberless thread *does* reach the registration.
/// - With a single live dispatcher, registration resolves interest from the
///   registering thread's own default — `NoSubscriber` — writing
///   `Interest::never()` into a process-global cache.
///
/// The capturing thread then skips its `warn!` entirely and the capture
/// comes back **empty**, which is the reported symptom. Under `--workspace`
/// the foreign thread is real: the homonym `warn!` in this module is also
/// driven by the `sessionmunch-writer` thread and by other libtest threads,
/// none of which hold a subscriber.
///
/// Ordering, not luck, reproduces it: register from the foreign thread
/// first, then ask the capture to observe the same callsite.
#[test]
fn a_subscriberless_thread_must_not_disable_a_callsite_the_capture_needs() {
    let logs = capture_warnings(|| {
        std::thread::spawn(control_only_warn)
            .join()
            .expect("control thread panicked");
        control_only_warn();
    });

    assert!(
        logs.contains("control-callsite-canary"),
        "a subscriberless thread registered this callsite as Interest::never() \
             and the capture went blind: {logs:?}"
    );
}

fn handoff_acceptance(
    handoff_id: HandoffId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> HandoffAcceptance {
    HandoffAcceptance {
        handoff_id,
        workspace_id,
        project_id,
        accepting_agent: AgentKind::Codex,
        accepting_session: None,
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    }
}

/// Open a fresh DB with migrations applied + a default workspace
/// and "scratch" project pre-created. Tuple-return keeps the
/// tempdir alive for the duration of the test.
// Issue #156 regression, class-wide: every AgentKind must survive
// begin_session — i.e. the persisted sessions.agent_kind CHECK
// constraint must enumerate every variant. Zero shipped with the enum
// variant but without the CHECK migration (V26) and only a live test
// caught the constraint failure; this pins the whole class so the next
// agent addition fails here, before any deploy.
#[test]
fn begin_session_accepts_every_agent_kind() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    for kind in sessionmunch_core::AgentKind::ALL {
        let session = NewSession {
            id: sessionmunch_core::SessionId::new(),
            workspace_id: ws,
            project_id: proj,
            agent_kind: kind,
            cwd: None,
            actor_user: None,
        };
        begin_session(&mut conn, &session).unwrap_or_else(|e| {
            panic!(
                "sessions.agent_kind CHECK rejects '{}' — add it to the \
                     constraint with a new migration: {e}",
                kind.as_str()
            )
        });
    }
}

#[test]
fn get_or_create_project_flags_homonym_across_workspaces() {
    let (_tmp, mut conn, _ws, _proj) = fresh_db();
    let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
    let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
    let a_shared = get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();

    // "shared" already lives in `alpha`, so from `beta`'s side it is a
    // cross-workspace homonym; the owning workspace and unique names
    // flag nothing.
    {
        let tx = conn.transaction().unwrap();
        assert_eq!(
            project_name_in_other_workspaces(&tx, &ws_b, "shared").unwrap(),
            vec!["alpha".to_string()]
        );
        assert!(
            project_name_in_other_workspaces(&tx, &ws_a, "shared")
                .unwrap()
                .is_empty(),
            "the owning workspace must be excluded"
        );
        assert!(
            project_name_in_other_workspaces(&tx, &ws_b, "unique-name")
                .unwrap()
                .is_empty()
        );
    }

    // Creation is NOT blocked — the homonym is id-namespaced.
    let b_shared = get_or_create_project(&mut conn, &ws_b, "shared", None).unwrap();
    assert_ne!(
        a_shared, b_shared,
        "homonymous projects must get distinct ids"
    );
}

#[test]
fn get_or_create_project_warns_when_creating_homonym_across_workspaces() {
    let (_tmp, mut conn, _ws, _proj) = fresh_db();
    let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
    let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
    get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();

    let logs = capture_warnings(|| {
        get_or_create_project(&mut conn, &ws_b, "shared", None).unwrap();
    });
    assert!(
        logs.contains("already exists in other workspace")
            && logs.contains("shared")
            && logs.contains("alpha"),
        "homonym creation warning should name the project and other workspace: {logs}"
    );

    let logs = capture_warnings(|| {
        get_or_create_project(&mut conn, &ws_b, "shared", None).unwrap();
    });
    assert!(
        !logs.contains("already exists in other workspace"),
        "idempotent lookup must not warn: {logs}"
    );
}

#[test]
fn ensure_project_with_id_warns_when_creating_homonym_across_workspaces() {
    let (_tmp, mut conn, _ws, _proj) = fresh_db();
    let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
    let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
    get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();
    let id = ProjectId::new();

    let logs = capture_warnings(|| {
        ensure_project_with_id(&mut conn, id, ws_b, "shared", None).unwrap();
    });
    assert!(
        logs.contains("already exists in other workspace")
            && logs.contains("shared")
            && logs.contains("alpha"),
        "manifest project creation warning should name the project and other workspace: {logs}"
    );

    let logs = capture_warnings(|| {
        ensure_project_with_id(&mut conn, id, ws_b, "shared", None).unwrap();
    });
    assert!(
        !logs.contains("already exists in other workspace"),
        "idempotent manifest import must not warn: {logs}"
    );
}

#[test]
fn ensure_project_with_id_does_not_warn_when_validation_fails() {
    let (_tmp, mut conn, _ws, _proj) = fresh_db();
    let ws_a = get_or_create_workspace(&mut conn, "alpha").unwrap();
    let ws_b = get_or_create_workspace(&mut conn, "beta").unwrap();
    get_or_create_project(&mut conn, &ws_a, "shared", None).unwrap();
    let id = ProjectId::new();
    ensure_project_with_id(&mut conn, id, ws_b, "other", None).unwrap();

    let logs = capture_warnings(|| {
        let err = ensure_project_with_id(&mut conn, id, ws_b, "shared", None)
            .expect_err("same id with different name must fail validation");
        assert!(
            matches!(err, StoreError::Duplicate(_)),
            "unexpected error: {err}"
        );
    });
    assert!(
        !logs.contains("already exists in other workspace"),
        "failed validation must not emit a creation warning: {logs}"
    );
}

#[test]
fn delete_workspace_refuses_non_empty_then_cascades_with_force() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
    let prepared = open_managed_run(&mut conn, &ws, &proj);

    // Non-empty (holds the "scratch" project + a page) → refused w/o force.
    let err = delete_workspace(&mut conn, &ws, false, Compaction::Skip).unwrap_err();
    assert!(
        matches!(err, StoreError::WorkspaceNotEmpty(n) if n >= 1),
        "expected WorkspaceNotEmpty, got {err:?}"
    );
    let ws_still: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM workspaces WHERE id = ?1",
            rusqlite::params![ws.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ws_still, 1, "refused delete must not touch the row");

    // Force cascades: project + page gone, workspace row gone.
    let summary = delete_workspace(&mut conn, &ws, true, Compaction::Skip).unwrap();
    assert!(
        summary.projects_deleted >= 1 && summary.pages_deleted >= 1,
        "{summary:?}"
    );
    assert_eq!(summary.workstreams_deleted, 1);
    assert_eq!(summary.managed_runs_deleted, 1);
    assert_eq!(
        summary.workstream_ids,
        vec![prepared.workstream_id.to_string()]
    );
    let proj_left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE workspace_id = ?1",
            rusqlite::params![ws.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(proj_left, 0, "projects must cascade on workspace delete");

    // Deleting again → NotFound.
    assert!(matches!(
        delete_workspace(&mut conn, &ws, true, Compaction::Skip).unwrap_err(),
        StoreError::NotFound(_)
    ));
}

#[test]
fn delete_workspace_empty_succeeds_without_force() {
    let (_tmp, mut conn, _ws, _proj) = fresh_db();
    let empty = get_or_create_workspace(&mut conn, "orphan-ws").unwrap();
    let summary = delete_workspace(&mut conn, &empty, false, Compaction::Skip).unwrap();
    assert_eq!(summary.projects_deleted, 0);
    assert_eq!(summary.pages_deleted, 0);
    assert_eq!(summary.workstreams_deleted, 0);
    assert_eq!(summary.managed_runs_deleted, 0);
    assert!(summary.workstream_ids.is_empty());
}

#[test]
fn rename_workspace_updates_name_and_rejects_collision() {
    let (_tmp, mut conn, ws, _proj) = fresh_db();
    get_or_create_workspace(&mut conn, "taken").unwrap();

    rename_workspace(&mut conn, &ws, "renamed").unwrap();
    let name: String = conn
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            rusqlite::params![ws.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "renamed", "name column updated in place");

    assert!(matches!(
        rename_workspace(&mut conn, &ws, "taken").unwrap_err(),
        StoreError::WorkspaceNameTaken(_)
    ));
    assert!(matches!(
        rename_workspace(&mut conn, &ws, "   ").unwrap_err(),
        StoreError::InvalidWorkspaceName(_)
    ));
}

fn fresh_db() -> (
    TempDir,
    Connection,
    sessionmunch_core::WorkspaceId,
    sessionmunch_core::ProjectId,
) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let mut conn = Connection::open(&db_path).unwrap();
    // A fixture needs no durability. SQLite's defaults (rollback journal,
    // synchronous=FULL) fsync every transaction, and nextest runs ~120 of
    // these in parallel, so the suite was disk-bound: 0.3s per test alone,
    // 2s+ under load. Production sets WAL + NORMAL in `Store::open`; these
    // tests exercise SQL, not the journal.
    conn.pragma_update(None, "journal_mode", "MEMORY").unwrap();
    conn.pragma_update(None, "synchronous", "OFF").unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    crate::migrations::run(&mut conn).unwrap();
    let ws = get_or_create_workspace(&mut conn, "default").unwrap();
    let proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
    (tmp, conn, ws, proj)
}

fn hook_session(
    id: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
    owner: Option<&str>,
) -> NewSession {
    NewSession {
        id,
        workspace_id: ws,
        project_id: proj,
        agent_kind: AgentKind::Codex,
        cwd: None,
        actor_user: owner.map(str::to_owned),
    }
}

fn hook_observation(session: &NewSession) -> NewObservation {
    NewObservation {
        session_id: session.id,
        workspace_id: session.workspace_id,
        project_id: session.project_id,
        kind: ObservationKind::UserPrompt,
        extension: None,
        source_event: None,
        title: "hook".into(),
        body: "observation".into(),
        importance: 5,
    }
}

/// Seed a session with one observation and a derived page, return the ids.
fn seed_session(
    conn: &mut Connection,
    ws: WorkspaceId,
    proj: ProjectId,
    marker: &str,
) -> (SessionId, PageId) {
    let sid = SessionId::new();
    let session = hook_session(sid, ws, proj, None);
    begin_session(conn, &session).unwrap();
    let mut obs = hook_observation(&session);
    obs.body = format!("obs-{marker}");
    insert_observation(conn, &obs).unwrap();
    let page_id = upsert_page(
        conn,
        &page(
            ws,
            proj,
            &format!("sessions/{marker}.md"),
            &format!("page-{marker}"),
        ),
    )
    .unwrap();
    end_session(conn, &sid, Some(&page_id)).unwrap();
    (sid, page_id)
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Insert one managed workstream event carrying a distinctive marker.
/// `workstream_events` cascades out of `workstreams`, which cascades out
/// of `projects` — so a project purge removes these rows without ever
/// running a `DELETE` statement against the table itself.
fn seed_workstream_event(
    conn: &Connection,
    workstream_id: &sessionmunch_core::WorkstreamId,
    marker: &str,
) {
    conn.execute(
        "INSERT INTO workstream_events (workstream_id, sequence, event_id, agent_kind, \
             native_session_id, kind, role, content, created_at) \
             VALUES (?1, 1, ?2, 'claude-code', 'native-1', 'message', 'user', ?3, ?4)",
        rusqlite::params![
            workstream_id.as_bytes(),
            format!("evt-{marker}"),
            format!("evt-body-{marker}"),
            Timestamp::now().as_microsecond(),
        ],
    )
    .unwrap();
}

/// Round-trip: recorded chunks come back ordered by index, and `clear`
/// empties them. A different fingerprint's rows are untouched (#621).
#[test]
fn bootstrap_chunk_progress_round_trips_and_clears() {
    let (_tmp, conn, _ws, _proj) = fresh_db();

    record_bootstrap_chunk(&conn, "fp-a", 1, r#"{"pages":[]}"#, "second chunk").unwrap();
    record_bootstrap_chunk(&conn, "fp-a", 0, r#"{"pages":[]}"#, "first chunk").unwrap();
    record_bootstrap_chunk(&conn, "fp-b", 0, r#"{"pages":[]}"#, "other run").unwrap();

    let loaded = load_bootstrap_progress(&conn, "fp-a").unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].chunk_index, 0);
    assert_eq!(loaded[0].rationale, "first chunk");
    assert_eq!(loaded[1].chunk_index, 1);
    assert_eq!(loaded[1].rationale, "second chunk");

    // Re-recording the same (fingerprint, chunk_index) replaces the row
    // rather than duplicating it — `INSERT OR REPLACE` idempotency.
    record_bootstrap_chunk(&conn, "fp-a", 0, r#"{"pages":[]}"#, "first chunk v2").unwrap();
    let loaded = load_bootstrap_progress(&conn, "fp-a").unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].rationale, "first chunk v2");

    clear_bootstrap_progress(&conn, "fp-a").unwrap();
    assert!(load_bootstrap_progress(&conn, "fp-a").unwrap().is_empty());
    assert_eq!(
        load_bootstrap_progress(&conn, "fp-b").unwrap().len(),
        1,
        "clearing one fingerprint must not touch another"
    );
}

/// The default for a project purge is the same logical delete `#387`
/// documented for a session purge. Pinned so the CLI help, the admin route
/// docs and `docs/lifecycle-ops.md` cannot drift away from the behaviour:
/// if this starts failing, byte-level removal became the default and all
/// three need updating together.
#[test]
fn purge_project_tombstones_the_scope_against_resurrection() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    seed_session(&mut conn, ws, proj, "canaryproj");

    assert!(
        !scope_is_purged(&conn, &ws, &proj).unwrap(),
        "a live project is not tombstoned"
    );

    purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        false,
        Compaction::Skip,
    )
    .unwrap();

    assert!(
        scope_is_purged(&conn, &ws, &proj).unwrap(),
        "purge must tombstone the scope so reindex cannot resurrect it"
    );
    // A different, un-purged project in the same workspace is unaffected.
    assert!(
        !scope_is_purged(&conn, &ws, &sessionmunch_core::ProjectId::new()).unwrap(),
        "the tombstone is scoped to the purged project id only"
    );
}

#[test]
fn delete_workspace_tombstones_the_whole_workspace() {
    let (_tmp, mut conn, ws, proj) = fresh_db();

    delete_workspace(&mut conn, &ws, true, Compaction::Skip).unwrap();

    // The whole-workspace tombstone covers every project id in that ws,
    // including ones whose rows are already gone via cascade.
    assert!(scope_is_purged(&conn, &ws, &proj).unwrap());
    assert!(scope_is_purged(&conn, &ws, &sessionmunch_core::ProjectId::new()).unwrap());
    // A different workspace is not affected by the sentinel.
    assert!(!scope_is_purged(&conn, &sessionmunch_core::WorkspaceId::new(), &proj).unwrap());
}

#[test]
fn purge_project_is_a_logical_delete_by_default() {
    let (tmp, mut conn, ws, proj) = fresh_db();
    let db_path = tmp.path().join("test.sqlite");
    seed_session(&mut conn, ws, proj, "canaryproj");

    let summary = purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        false,
        Compaction::Skip,
    )
    .unwrap();
    assert!(!summary.compacted, "the default does not VACUUM");

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    let bytes = std::fs::read(&db_path).unwrap();
    assert!(
        bytes.windows(14).any(|w| w == b"obs-canaryproj"),
        "documented boundary: purge-project is a logical delete. If this now \
             fails, byte-level removal became the default and the #540 note, the \
             CLI help and docs/lifecycle-ops.md all need updating to match."
    );
}

/// The opt-in half. Paired with `purge_project_is_a_logical_delete_by_default`
/// so the two together pin the difference between what the command promises
/// and what it offers.
#[test]
fn purge_project_with_reclaim_removes_the_bytes_from_the_file() {
    let (tmp, mut conn, ws, proj) = fresh_db();
    let db_path = tmp.path().join("test.sqlite");
    seed_session(&mut conn, ws, proj, "canaryproj");
    // A second project in the same workspace must survive untouched.
    let other = get_or_create_project(&mut conn, &ws, "keeper", None).unwrap();
    seed_session(&mut conn, ws, other, "survivor");

    let summary = purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        false,
        Compaction::Reclaim,
    )
    .unwrap();
    assert!(summary.compacted, "the summary reports that VACUUM ran");

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    let bytes = std::fs::read(&db_path).unwrap();
    assert!(
        !bytes.windows(14).any(|w| w == b"obs-canaryproj"),
        "Reclaim must remove the purged project's text from the database file"
    );
    assert!(
        bytes.windows(12).any(|w| w == b"obs-survivor"),
        "the sibling project's text is untouched by the compaction"
    );
}

/// The reason `reclaim_freed_pages` rebuilds all three FTS indexes rather
/// than the two a session purge needs.
///
/// `workstream_events` is an external-content FTS5 table whose rows leave
/// via the `projects` → `workstreams` → `workstream_events` cascade. A
/// cascade does not fire the `AFTER DELETE` trigger that would remove the
/// row from the index (`recursive_triggers` is off), so both the index
/// entry and its tokens survive the delete. Rebuilding only `pages_fts`
/// and `observations_fts` — the set a session purge needs — would leave a
/// managed agent's transcript text in the file after an operator asked for
/// it to be reclaimed.
#[test]
fn reclaim_clears_workstream_event_text_that_left_via_cascade() {
    let (tmp, mut conn, ws, proj) = fresh_db();
    let db_path = tmp.path().join("test.sqlite");
    let prepared = open_managed_run(&mut conn, &ws, &proj);
    seed_workstream_event(&conn, &prepared.workstream_id, "canaryevt");

    // Present before the purge, both in the file and in the index.
    let indexed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM workstream_events_fts \
                 WHERE workstream_events_fts MATCH '\"canaryevt\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 1, "the event is searchable before the purge");

    purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        true,
        Compaction::Reclaim,
    )
    .unwrap();

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    let bytes = std::fs::read(&db_path).unwrap();
    // Assert on the *token*, not the whole hyphenated body. The body only
    // ever existed contiguously in the content table, which `VACUUM` clears
    // on its own; what survives an ordinary delete is the tokenized form
    // inside the FTS5 segments, and only a `rebuild` removes that. Asserting
    // on the full string would pass with or without the rebuild and prove
    // nothing.
    assert!(
        !bytes.windows(9).any(|w| w == b"canaryevt"),
        "Reclaim must clear workstream event tokens; if this fails, the \
             workstream_events_fts rebuild was dropped from reclaim_freed_pages \
             and a managed transcript's text survives a compacted purge"
    );
}

/// `delete_workspace` takes the same path and makes the same promise.
#[test]
fn delete_workspace_reclaim_removes_the_bytes_and_reports_it() {
    let (tmp, mut conn, ws, proj) = fresh_db();
    let db_path = tmp.path().join("test.sqlite");
    seed_session(&mut conn, ws, proj, "canaryws");

    let skipped = {
        let other = get_or_create_workspace(&mut conn, "doomed").unwrap();
        delete_workspace(&mut conn, &other, false, Compaction::Skip).unwrap()
    };
    assert!(!skipped.compacted, "the default does not VACUUM");

    let summary = delete_workspace(&mut conn, &ws, true, Compaction::Reclaim).unwrap();
    assert!(summary.compacted, "the summary reports that VACUUM ran");

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    let bytes = std::fs::read(&db_path).unwrap();
    assert!(
        !bytes.windows(12).any(|w| w == b"obs-canaryws"),
        "Reclaim must remove the deleted workspace's text from the file"
    );
}

/// The premise of #549: after a large delete the space is *not* returned
/// to the filesystem — SQLite keeps the pages on its freelist for reuse.
/// `compact` is what gives them back, without deleting anything itself.
///
/// Deliberately paired assertions rather than one: the first pins that
/// there is a backlog to reclaim (otherwise the second could pass on an
/// empty database and prove nothing), the second that compaction reclaims
/// it.
#[test]
fn compact_returns_free_pages_that_a_delete_left_behind() {
    let (_tmp, mut conn, ws, proj) = fresh_db();

    // Enough rows that the freelist is unambiguous rather than noise.
    for i in 0..400 {
        upsert_page(
            &mut conn,
            &page(ws, proj, &format!("notes/{i}.md"), &"padding ".repeat(256)),
        )
        .unwrap();
    }
    conn.execute("DELETE FROM pages", []).unwrap();

    let before = database_bytes(&conn).unwrap();
    let freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap();
    assert!(
        freelist > 0,
        "the delete must leave a freelist, or this test proves nothing"
    );

    let summary = compact(&mut conn).unwrap();

    assert_eq!(summary.bytes_before, before);
    assert!(
        summary.bytes_after < summary.bytes_before,
        "compaction must shrink the file: {} -> {}",
        summary.bytes_before,
        summary.bytes_after
    );
    assert_eq!(
        summary.bytes_reclaimed(),
        summary.bytes_before - summary.bytes_after
    );
    let after_freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        after_freelist, 0,
        "the freelist is gone, not merely smaller"
    );
}

/// Compaction deletes nothing. Worth a test of its own: the operation is
/// reachable from an admin route and shares its implementation with the
/// destructive commands' `--compact`, so "it only reclaims" is a property
/// someone could break without noticing.
#[test]
fn compact_preserves_every_row_and_keeps_the_index_searchable() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    seed_session(&mut conn, ws, proj, "survivor");
    let pages_before = count(&conn, "SELECT COUNT(*) FROM pages");
    let obs_before = count(&conn, "SELECT COUNT(*) FROM observations");

    compact(&mut conn).unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM pages"), pages_before);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM observations"),
        obs_before
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM observations_fts \
                 WHERE observations_fts MATCH '\"obs-survivor\"'"
        ),
        1,
        "the rebuilt index still finds the surviving session"
    );
}

/// A `VACUUM` can leave the file marginally larger when defragmentation
/// costs more than the pages it released. That is zero reclaimed, not a
/// wrapped-around u64.
#[test]
fn bytes_reclaimed_saturates_instead_of_underflowing() {
    let grew = CompactSummary {
        bytes_before: 1_000,
        bytes_after: 1_200,
    };
    assert_eq!(grew.bytes_reclaimed(), 0);
}

/// #387, the reporter's reproduction: after purging one session by UUID,
/// its rows are gone from every table that held them.
#[test]
fn purge_session_removes_the_session_and_everything_derived_from_it() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let (sid, _page) = seed_session(&mut conn, ws, proj, "target");

    let summary = purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM sessions"),
        0,
        "sessions=0"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM observations"),
        0,
        "observations=0"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM pages"), 0, "pages=0");
    assert_eq!(summary.observations_deleted, 1);
    assert_eq!(summary.pages_deleted, 1);
    assert_eq!(
        summary.removed_paths,
        vec![PagePath::new("sessions/target.md").unwrap()]
    );
}

/// The blast radius must stop at the session. A sibling session in the
/// same project keeps every row it owns.
#[test]
fn purge_session_leaves_a_sibling_session_in_the_same_project_intact() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let (target, _) = seed_session(&mut conn, ws, proj, "target");
    let (keep, keep_page) = seed_session(&mut conn, ws, proj, "keep");

    purge_session(&mut conn, ws, proj, target, None, Compaction::Skip).unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM sessions"), 1);
    let survivor: Vec<u8> = conn
        .query_row("SELECT id FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(survivor, keep.as_bytes().to_vec(), "the sibling survived");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM observations"), 1);
    let body: String = conn
        .query_row("SELECT body FROM observations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "obs-keep");
    let page_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE id = ?1",
            [keep_page.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(page_rows, 1, "the sibling's page survived");
}

/// A session id is not authority over another scope. Naming the wrong
/// project must fail closed and delete nothing.
#[test]
fn purge_session_refuses_a_session_from_another_project_and_deletes_nothing() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
    let (sid, _) = seed_session(&mut conn, ws, proj, "target");

    let err = purge_session(&mut conn, ws, other, sid, None, Compaction::Skip)
        .expect_err("a session outside the named project must not be purgeable");
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");

    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM sessions"),
        1,
        "nothing deleted"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM observations"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM pages"), 1);
}

/// Same containment across workspaces.
#[test]
fn purge_session_refuses_a_session_from_another_workspace() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let ws2 = get_or_create_workspace(&mut conn, "second").unwrap();
    let proj2 = get_or_create_project(&mut conn, &ws2, "scratch", None).unwrap();
    let (sid, _) = seed_session(&mut conn, ws, proj, "target");

    let err = purge_session(&mut conn, ws2, proj2, sid, None, Compaction::Skip)
        .expect_err("cross-workspace purge must be refused");
    assert!(matches!(err, StoreError::NotFound(_)));
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM sessions"), 1);
}

/// Two projects can hold the same `sessions/<uuid>.md` path. Deleting by
/// path would take the other one; deleting by id must not.
#[test]
fn purge_session_does_not_delete_an_identically_pathed_page_in_another_project() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
    let (sid, _) = seed_session(&mut conn, ws, proj, "shared");
    let twin = upsert_page(
        &mut conn,
        &page(ws, other, "sessions/shared.md", "page-elsewhere"),
    )
    .unwrap();

    purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

    let survived: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE id = ?1",
            [twin.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        survived, 1,
        "a same-path page in another project must survive a purge by id"
    );
}

/// Insert one open handoff. `manual` writes `from_session_id = NULL`,
/// which is what makes a handoff exempt from the automatic sweep.
fn seed_handoff(
    conn: &Connection,
    ws: WorkspaceId,
    proj: ProjectId,
    cwd: Option<&str>,
    manual: bool,
    owner: Option<&str>,
    created_at: i64,
) -> Vec<u8> {
    let id = uuid::Uuid::now_v7().as_bytes().to_vec();
    let from_session: Option<Vec<u8>> = if manual {
        None
    } else {
        Some(SessionId::new().as_bytes().to_vec())
    };
    // An auto handoff needs a real session row for the FK.
    if let Some(sid) = &from_session {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, started_at, \
                 actor_user) VALUES (?1, ?2, ?3, 'codex', 1, ?4)",
            rusqlite::params![&sid[..], ws.as_bytes(), proj.as_bytes(), owner],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO handoffs (id, workspace_id, project_id, from_session_id, from_agent, \
             summary, state, created_at, cwd, owner_user) \
             VALUES (?1, ?2, ?3, ?4, 'codex', 'baton', 'open', ?5, ?6, ?7)",
        rusqlite::params![
            &id[..],
            ws.as_bytes(),
            proj.as_bytes(),
            from_session,
            created_at,
            cwd,
            owner
        ],
    )
    .unwrap();
    id
}

fn open_handoffs(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM handoffs WHERE state = 'open'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// #513: the leftover backlog is made of exactly the handoffs the
/// automatic sweep exempts — manual ones and ones from a non-matching
/// cwd. An operator-driven bulk expiry has to clear those, or it clears
/// nothing at all.
#[test]
fn expire_open_handoffs_clears_the_handoffs_the_auto_sweep_exempts() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    seed_handoff(&conn, ws, proj, Some("/work/a"), true, None, 100);
    seed_handoff(&conn, ws, proj, Some("/elsewhere"), false, None, 200);
    seed_handoff(&conn, ws, proj, Some("/work/a"), false, None, 300);
    assert_eq!(open_handoffs(&conn), 3, "precondition");

    let expired =
        expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();

    assert_eq!(expired, 3, "every open handoff in scope is cleared");
    assert_eq!(open_handoffs(&conn), 0);
}

/// It is a state change, not a delete: the summary and provenance survive
/// so an operator can still see what was cleared.
#[test]
fn expire_open_handoffs_preserves_the_rows_it_expires() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    seed_handoff(&conn, ws, proj, Some("/work/a"), true, None, 100);

    expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();

    let (state, summary): (String, String) = conn
        .query_row("SELECT state, summary FROM handoffs", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "expired");
    assert_eq!(summary, "baton", "content is not destroyed");
}

/// `--older-than` lets an operator clear a backlog without touching a
/// handoff created moments ago.
#[test]
fn expire_open_handoffs_respects_the_age_cutoff() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    seed_handoff(&conn, ws, proj, None, true, None, 100);
    seed_handoff(&conn, ws, proj, None, true, None, 5_000);

    let expired =
        expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, Some(1_000), None).unwrap();

    assert_eq!(expired, 1, "only the older one");
    assert_eq!(open_handoffs(&conn), 1);
}

/// Scope containment: another project's backlog is untouched.
#[test]
fn expire_open_handoffs_does_not_cross_project_boundaries() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
    seed_handoff(&conn, ws, proj, None, true, None, 100);
    seed_handoff(&conn, ws, other, None, true, None, 100);

    let expired =
        expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();

    assert_eq!(expired, 1);
    assert_eq!(
        open_handoffs(&conn),
        1,
        "the other project's handoff survives"
    );
}

/// Owner scoping: expiring as one user must not clear another's baton.
#[test]
fn expire_open_handoffs_leaves_another_owners_baton_alone() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    seed_handoff(&conn, ws, proj, None, true, Some("user:alice"), 100);
    seed_handoff(&conn, ws, proj, None, true, Some("user:bob"), 100);
    seed_handoff(&conn, ws, proj, None, true, None, 100);

    let expired = expire_open_handoffs(
        &mut conn,
        &ws,
        &proj,
        &OwnerFilter::User("user:alice".into()),
        None,
        None,
    )
    .unwrap();

    assert_eq!(expired, 2, "alice's own plus the shared one");
    let remaining: String = conn
        .query_row(
            "SELECT owner_user FROM handoffs WHERE state = 'open'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, "user:bob");
}

/// Already-accepted handoffs are history, not backlog.
#[test]
fn expire_open_handoffs_does_not_touch_accepted_ones() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    conn.execute(
        "INSERT INTO handoffs (id, workspace_id, project_id, from_agent, summary, state, \
             created_at) VALUES (?1, ?2, ?3, 'codex', 'done', 'accepted', 1)",
        rusqlite::params![
            &uuid::Uuid::now_v7().as_bytes()[..],
            ws.as_bytes(),
            proj.as_bytes()
        ],
    )
    .unwrap();

    let expired =
        expire_open_handoffs(&mut conn, &ws, &proj, &OwnerFilter::Any, None, None).unwrap();
    assert_eq!(expired, 0);
    let state: String = conn
        .query_row("SELECT state FROM handoffs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "accepted", "an accepted handoff is not backlog");
}

fn embed_failure_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM page_embed_failures", [], |r| r.get(0))
        .unwrap()
}

/// #528: a failed embed left nothing durable — the inline path warned and
/// returned success, and the backfill's warnings died with the container.
#[test]
fn a_failed_embed_is_recorded_with_its_reason_and_detail() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

    record_embed_failure(
        &conn,
        &page_id,
        EmbedOutcome::Failed,
        Some("400 empty Part"),
    )
    .unwrap();

    let (outcome, detail): (String, Option<String>) = conn
        .query_row(
            "SELECT outcome, detail FROM page_embed_failures WHERE page_id = ?1",
            [page_id.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "failed");
    assert_eq!(detail.as_deref(), Some("400 empty Part"));
}

/// A success must write nothing here — the `page_embeddings` row already
/// records it, and the reporter's first constraint was no extra write on
/// the hot path.
#[test]
fn a_successful_embed_writes_no_failure_row() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

    store_embedding(&mut conn, &page_id, &[0_u8; 8], "p", "m", 2).unwrap();

    assert_eq!(embed_failure_rows(&conn), 0);
}

/// The second constraint: a global re-embed must not erase the history.
#[test]
fn a_later_successful_embed_preserves_the_failure_history() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
    record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, Some("boom")).unwrap();

    store_embedding(&mut conn, &page_id, &[0_u8; 8], "p", "m", 2).unwrap();

    assert_eq!(
        embed_failure_rows(&conn),
        1,
        "the record of what went wrong survives the repair"
    );
}

/// Re-attempting overwrites rather than accumulating: one row per page.
#[test]
fn repeated_failures_keep_only_the_latest_attempt() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

    record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, Some("first")).unwrap();
    record_embed_failure(&conn, &page_id, EmbedOutcome::SkippedEmpty, None).unwrap();

    assert_eq!(embed_failure_rows(&conn), 1);
    let outcome: String = conn
        .query_row("SELECT outcome FROM page_embed_failures", [], |r| r.get(0))
        .unwrap();
    assert_eq!(outcome, "skipped_empty", "latest attempt wins");
}

/// A provider error can carry a whole response body; the ledger says what
/// happened, it does not archive it.
#[test]
fn failure_detail_is_truncated() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
    let huge = "x".repeat(5_000);

    record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, Some(&huge)).unwrap();

    let detail: String = conn
        .query_row("SELECT detail FROM page_embed_failures", [], |r| r.get(0))
        .unwrap();
    assert!(detail.chars().count() <= EMBED_FAILURE_DETAIL_MAX + 1);
    assert!(detail.ends_with('…'), "truncation is visible");
}

/// The ledger cannot outgrow the corpus: rows go with their page.
#[test]
fn a_failure_row_is_removed_with_its_page() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let page_id = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
    record_embed_failure(&conn, &page_id, EmbedOutcome::Failed, None).unwrap();

    delete_page(
        &mut conn,
        ws,
        proj,
        &PagePath::new("notes/a.md").unwrap(),
        None,
    )
    .unwrap();

    assert_eq!(embed_failure_rows(&conn), 0);
}

/// A failure row is the *last unsuccessful attempt*, not proof the page is
/// still broken. `status` must separate the two, or a page that failed
/// once and recovered reads as an outstanding problem forever.
#[test]
fn status_separates_unresolved_embed_failures_from_recovered_ones() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let broken = upsert_page(&mut conn, &page(ws, proj, "notes/broken.md", "b")).unwrap();
    let recovered = upsert_page(&mut conn, &page(ws, proj, "notes/ok.md", "b")).unwrap();

    record_embed_failure(&conn, &broken, EmbedOutcome::Failed, Some("boom")).unwrap();
    record_embed_failure(&conn, &recovered, EmbedOutcome::SkippedEmpty, None).unwrap();
    store_embedding(&mut conn, &recovered, &[0_u8; 8], "p", "m", 2).unwrap();

    let unresolved: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_embed_failures f \
                 JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                 LEFT JOIN page_embeddings pe ON pe.page_id = f.page_id \
                 WHERE pe.page_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let recovered_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_embed_failures f \
                 JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                 JOIN page_embeddings pe ON pe.page_id = f.page_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unresolved, 1, "only the page still lacking an embedding");
    assert_eq!(recovered_count, 1, "the repaired page is history");
}

/// Accepting a handoff does not make its text yours. Purging the
/// accepting session must not destroy the authoring session's record.
#[test]
fn purge_session_deletes_authored_handoffs_but_not_accepted_ones() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let (author, _) = seed_session(&mut conn, ws, proj, "author");
    let (accepter, _) = seed_session(&mut conn, ws, proj, "accepter");
    conn.execute(
        "INSERT INTO handoffs (id, workspace_id, project_id, from_session_id, from_agent, \
             summary, state, created_at, accepted_by_session) \
             VALUES (?1, ?2, ?3, ?4, 'codex', 'authored-by-author', 'accepted', 1, ?5)",
        rusqlite::params![
            &uuid::Uuid::now_v7().as_bytes()[..],
            ws.as_bytes(),
            proj.as_bytes(),
            author.as_bytes(),
            accepter.as_bytes()
        ],
    )
    .unwrap();

    let summary = purge_session(&mut conn, ws, proj, accepter, None, Compaction::Skip).unwrap();
    assert_eq!(summary.handoffs_deleted, 0, "accepting is not authorship");
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM handoffs"),
        1,
        "the authoring session's handoff survives"
    );

    let summary = purge_session(&mut conn, ws, proj, author, None, Compaction::Skip).unwrap();
    assert_eq!(
        summary.handoffs_deleted, 1,
        "the author's handoff is removed"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM handoffs"), 0);
}

/// Purging a session that is already gone is NotFound, not a partial
/// delete of whatever happens to match.
#[test]
fn purge_session_is_not_found_when_run_twice() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let (sid, _) = seed_session(&mut conn, ws, proj, "target");
    let (keep, _) = seed_session(&mut conn, ws, proj, "keep");

    purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();
    let err =
        purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).expect_err("already gone");
    assert!(matches!(err, StoreError::NotFound(_)));
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM sessions"),
        1,
        "the second call left the surviving session alone"
    );
    let _ = keep;
}

/// Pins the advertised boundary. A purged session is unreachable through
/// the API and through search, but its bytes are still in the database
/// file until a `rebuild` + `VACUUM` — exactly as they are for any other
/// SQLite delete. #387 promises the former and explicitly not the latter,
/// and this test fails if someone changes that without revisiting the
/// claim we make to operators.
#[test]
fn purge_session_is_unsearchable_but_does_not_claim_byte_level_removal() {
    let (tmp, mut conn, ws, proj) = fresh_db();
    let db_path = tmp.path().join("test.sqlite");
    let (sid, _) = seed_session(&mut conn, ws, proj, "target");
    let (_keep, _) = seed_session(&mut conn, ws, proj, "keep");

    purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

    let gone: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH '\"obs-target\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(gone, 0, "the purged session is not searchable");
    let kept: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH '\"obs-keep\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "the sibling session is still searchable");

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    let bytes = std::fs::read(&db_path).unwrap();
    let still_on_disk = bytes.windows(10).any(|w| w == b"obs-target");
    assert!(
        still_on_disk,
        "documented boundary: purge is a logical delete. If this now fails, \
             byte-level removal was added and the #387 scope note, the CLI help \
             and the operator-facing docs all need updating to match."
    );
}

/// `reclaim_freed_pages` promises to rebuild *every* FTS5 index, which is
/// what makes it safe for all three scopes to share one definition of
/// "reclaimed". #548 established that a project or workspace delete
/// cascades `workstream_events` out without firing the trigger that would
/// drop its rows from `workstream_events_fts`, so an index left out of the
/// batch keeps live entries for rows that no longer exist.
///
/// The promise is checked against the schema rather than trusted. A new
/// FTS index added later fails here, which routes whoever adds it into the
/// decision — the alternative is that `--compact` silently skips it and
/// text stays searchable after an operator asked for it to be reclaimed,
/// with the response still reporting `compacted: true`.
#[test]
fn reclaim_rebuilds_every_fts_index_in_the_schema() {
    let (_tmp, conn, _ws, _proj) = fresh_db();
    let mut in_schema: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND sql LIKE '%USING fts5%'",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    in_schema.sort();

    let mut declared: Vec<String> = ALL_FTS_INDEXES.iter().map(|s| (*s).to_owned()).collect();
    declared.sort();

    assert_eq!(
        in_schema, declared,
        "ALL_FTS_INDEXES is out of date. `reclaim_freed_pages` rebuilds only \
             what is listed there, so an FTS index missing from it survives a \
             --compact with entries for deleted rows still in it. Add the new \
             index, or exclude it deliberately here and say why."
    );
}

/// The opt-in half: `Compaction::Reclaim` must actually remove the bytes
/// the default leaves behind. Paired with
/// `purge_session_is_unsearchable_but_does_not_claim_byte_level_removal`,
/// which asserts the opposite for the default, so the two together pin
/// the difference between what we promise and what we offer.
#[test]
fn purge_session_with_reclaim_removes_the_bytes_from_the_file() {
    let (tmp, mut conn, ws, proj) = fresh_db();
    let db_path = tmp.path().join("test.sqlite");
    let (sid, _) = seed_session(&mut conn, ws, proj, "target");
    let (_keep, _) = seed_session(&mut conn, ws, proj, "keep");

    let summary = purge_session(&mut conn, ws, proj, sid, None, Compaction::Reclaim).unwrap();
    assert!(summary.compacted, "the summary reports that VACUUM ran");

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    let bytes = std::fs::read(&db_path).unwrap();
    assert!(
        !bytes.windows(10).any(|w| w == b"obs-target"),
        "Reclaim must remove the purged text from the database file"
    );
    assert!(
        bytes.windows(8).any(|w| w == b"obs-keep"),
        "the surviving session's text is untouched by the compaction"
    );
}

/// A purge must be terminal. The events that produced a session can sit
/// undelivered in a client hook spool for days (#493 measured spools with
/// thousands), so without a tombstone the next drain recreates the session
/// row and the observations land again — silently undoing a deletion an
/// application already reported to its user.
#[test]
fn a_purged_session_is_not_recreated_by_late_arriving_events() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let (sid, _) = seed_session(&mut conn, ws, proj, "target");
    purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

    // Exactly what a spool drain delivers after the purge.
    let session = hook_session(sid, ws, proj, None);
    let err =
        begin_session(&mut conn, &session).expect_err("a purged session must not be recreated");
    assert!(matches!(err, StoreError::SessionPurged(_)), "got {err:?}");

    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM sessions"),
        0,
        "still gone"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM observations"), 0);
}

/// The tombstone is scoped: purging a session id in one project must not
/// block ingest for the same id in another, or one purge could deny
/// capture across the whole store.
#[test]
fn a_tombstone_does_not_block_the_same_session_id_in_another_project() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
    let (sid, _) = seed_session(&mut conn, ws, proj, "target");
    purge_session(&mut conn, ws, proj, sid, None, Compaction::Skip).unwrap();

    let elsewhere = hook_session(sid, ws, other, None);
    begin_session(&mut conn, &elsewhere)
        .expect("a purge in one project must not suppress ingest in another");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM sessions"), 1);
}

fn session_end_observation(session: &NewSession) -> NewObservation {
    let mut observation = hook_observation(session);
    observation.kind = ObservationKind::SessionEnd;
    observation
}

fn seed_ingest_key(conn: &Connection, project_id: ProjectId, key: &str, completed: bool) {
    conn.execute(
        "INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            project_id.as_bytes(),
            key,
            Timestamp::now().as_microsecond(),
            completed.then_some(Timestamp::now().as_microsecond()),
        ],
    )
    .unwrap();
}

fn admitted_session(admission: HookSessionAdmission) -> AdmittedSession {
    match admission {
        HookSessionAdmission::Observation { session, .. }
        | HookSessionAdmission::EndOpen { session, .. }
        | HookSessionAdmission::ReEnd { session, .. }
        | HookSessionAdmission::AlreadyEnded { session } => session,
        HookSessionAdmission::InvalidMissingEnd | HookSessionAdmission::InvalidScopedEnd => {
            panic!("expected admitted session")
        }
    }
}

fn page(
    ws: sessionmunch_core::WorkspaceId,
    proj: sessionmunch_core::ProjectId,
    path: &str,
    body: &str,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: "test".into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

/// A page written before entity extraction — tags in frontmatter but
/// no entity links — is backfilled from those tags, with the link's
/// window opening at the page version's creation. A page written with
/// entities is left alone, and a re-run is a no-op.
#[test]
fn backfill_entity_index_populates_from_tags_idempotently() {
    let (_tmp, mut conn, ws, proj) = fresh_db();

    // Pre-fix page: tags present, but the write set no entities, so it
    // has no entity links — exactly what a mature store looks like.
    let mut stale = page(ws, proj, "concepts/writer.md", "the writer actor");
    stale.frontmatter_json = serde_json::json!({"tags": ["SQLite", "Concurrency"]});
    stale.entities = Vec::new();
    let stale_id = upsert_page(&mut conn, &stale).unwrap();
    let stale_created: i64 = conn
        .query_row(
            "SELECT created_at FROM pages WHERE id = ?1",
            params![stale_id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();

    // Already-indexed page: written with entities through the normal
    // path. The backfill must not touch it.
    let mut indexed = page(ws, proj, "concepts/search.md", "fts and vectors");
    indexed.entities = vec!["fts5".into()];
    upsert_page(&mut conn, &indexed).unwrap();
    let links_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM entity_page_links", [], |r| r.get(0))
        .unwrap();

    let summary = backfill_entity_index(&mut conn).unwrap();
    assert_eq!(summary.pages_backfilled, 1, "only the tag-only page");
    assert_eq!(summary.links_added, 2, "sqlite + concurrency");

    // Entities exist, normalized, and the links carry the page
    // version's own creation instant as the window open (docs/temporal).
    let names: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT e.name FROM entities e \
                     JOIN entity_page_links l ON l.entity_id = e.id \
                     WHERE l.page_id = ?1 ORDER BY e.name",
            )
            .unwrap();
        let rows = stmt
            .query_map(params![stale_id.as_bytes()], |r| r.get(0))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(names, vec!["concurrency".to_string(), "sqlite".to_string()]);
    let valid_from: i64 = conn
        .query_row(
            "SELECT DISTINCT valid_from FROM entity_page_links WHERE page_id = ?1",
            params![stale_id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        valid_from, stale_created,
        "window opens at the version's created_at"
    );

    // Idempotent: a second pass finds no candidates and inserts nothing.
    let again = backfill_entity_index(&mut conn).unwrap();
    assert_eq!(again, EntityBackfillSummary::default(), "re-run is a no-op");
    let links_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM entity_page_links", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        links_after,
        links_before + 2,
        "the pre-existing entity page kept its single link; only the stale page gained two"
    );
}

#[test]
fn page_feedback_is_scoped_rebuildable_and_audited() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
    let path = PagePath::new("notes/shared.md").unwrap();
    let target_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "target")).unwrap();
    upsert_page(&mut conn, &page(ws, other, path.as_str(), "other")).unwrap();

    let author = UserId::new();
    let now = Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO users (id, username, created_at) \
             VALUES (?1, 'feedback-author', ?2)",
        params![author.as_bytes(), now],
    )
    .unwrap();
    let params = crate::decay::DecayParams::default();

    let first = record_page_feedback(
        &mut conn,
        ws,
        proj,
        &path,
        FeedbackKind::Helpful,
        Some("useful"),
        Some(author),
        &params,
    )
    .unwrap()
    .unwrap();
    assert_eq!(first.0, target_id);
    assert!((first.1 - 1.25).abs() < f64::EPSILON);

    let second = record_page_feedback(
        &mut conn,
        ws,
        proj,
        &path,
        FeedbackKind::NotHelpful,
        None,
        Some(author),
        &params,
    )
    .unwrap()
    .unwrap();
    assert_eq!(second.0, target_id);
    assert!((second.1 - 1.0).abs() < f64::EPSILON);

    let target_salience: Option<f64> = conn
        .query_row(
            "SELECT salience FROM pages WHERE id = ?1",
            params![target_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(target_salience, Some(1.0));
    let other_salience: Option<f64> = conn
        .query_row(
            "SELECT salience FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![ws.as_bytes(), other.as_bytes(), path.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        other_salience, None,
        "same path in another scope must not move"
    );

    let events: Vec<(String, Option<String>, f64, Vec<u8>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kind, reason, salience_after, author_id FROM page_feedback \
                     WHERE page_id = ?1 ORDER BY rowid ASC",
            )
            .unwrap();
        stmt.query_map(params![target_id.as_bytes()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "helpful");
    assert_eq!(events[0].1.as_deref(), Some("useful"));
    assert_eq!(events[0].2, 1.25);
    assert_eq!(events[1].0, "not_helpful");
    assert_eq!(events[1].2, 1.0);
    assert!(events.iter().all(|event| event.3 == author.as_bytes()[..]));

    let audit_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log \
                 WHERE op = 'page_feedback' AND page_id = ?1 AND author_id = ?2",
            params![target_id.as_bytes(), author.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_rows, 2);
}

#[test]
fn page_feedback_noop_and_failure_leave_no_partial_state() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let params = crate::decay::DecayParams::default();
    let missing = PagePath::new("notes/missing.md").unwrap();
    assert!(
        record_page_feedback(
            &mut conn,
            ws,
            proj,
            &missing,
            FeedbackKind::Helpful,
            None,
            None,
            &params,
        )
        .unwrap()
        .is_none()
    );

    let path = PagePath::new("notes/rollback.md").unwrap();
    let page_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "body")).unwrap();
    let err = record_page_feedback(
        &mut conn,
        ws,
        proj,
        &path,
        FeedbackKind::Wrong,
        Some("must roll back"),
        Some(UserId::new()),
        &params,
    )
    .expect_err("unknown author must violate the feedback FK");
    assert!(
        matches!(err, StoreError::Sqlite(_)),
        "unexpected error: {err}"
    );

    let (feedback_rows, audit_rows, salience): (i64, i64, Option<f64>) = conn
        .query_row(
            "SELECT \
                    (SELECT COUNT(*) FROM page_feedback), \
                    (SELECT COUNT(*) FROM audit_log WHERE op = 'page_feedback'), \
                    (SELECT salience FROM pages WHERE id = ?1)",
            params![page_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(feedback_rows, 0);
    assert_eq!(audit_rows, 0);
    assert_eq!(salience, None);
}

#[test]
fn rewriting_a_flagged_page_retires_the_flag_but_keeps_its_event() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let path = PagePath::new("notes/versioned.md").unwrap();
    let old_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "old")).unwrap();
    let params = crate::decay::DecayParams::default();
    record_page_feedback(
        &mut conn,
        ws,
        proj,
        &path,
        FeedbackKind::Stale,
        Some("outdated"),
        None,
        &params,
    )
    .unwrap()
    .unwrap();

    let new_id = upsert_page(&mut conn, &page(ws, proj, path.as_str(), "new")).unwrap();
    assert_ne!(old_id, new_id);
    let new_salience: Option<f64> = conn
        .query_row(
            "SELECT salience FROM pages WHERE id = ?1",
            params![new_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(new_salience, None);

    let (events, open_flags): (i64, i64) = conn
        .query_row(
            "SELECT \
                    (SELECT COUNT(*) FROM page_feedback WHERE page_id = ?1), \
                    (SELECT COUNT(*) FROM page_feedback f \
                     JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                     WHERE f.page_id = ?1)",
            params![old_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(events, 1, "supersession keeps the append-only event");
    assert_eq!(open_flags, 0, "superseded feedback must not remain open");
}

/// Trickier path: upserting a page with a CHANGED body must
/// produce a NEW row and mark the previous row `is_latest = 0`.
/// This is the M7 supersession chain — the entire wiki versioning
/// guarantee rides on it.
/// V16: every page write lands an `audit_log` row whose
/// `author_id` mirrors the NewPage's. Anonymous writes leave it
/// NULL (the entire audit-log-by-author query pattern relies on
/// the partial index covering only the non-NULL minority).
#[test]
fn audit_log_records_author_for_attributed_create_page() {
    use sessionmunch_core::UserId;

    let (_tmp, mut conn, ws, proj) = fresh_db();

    // Seed a synthetic user row so the FK on author_id resolves.
    let user_id = UserId::new();
    let now = jiff::Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO users (id, username, name, email, created_at) \
             VALUES (?1, 'alice', NULL, NULL, ?2)",
        params![user_id.as_bytes(), now],
    )
    .unwrap();

    let mut np = page(ws, proj, "notes/by-alice.md", "alice body");
    np.author_id = Some(user_id);
    let page_id = upsert_page(&mut conn, &np).unwrap();

    let author_bytes: Vec<u8> = conn
        .query_row(
            "SELECT author_id FROM audit_log \
                 WHERE op = 'create_page' AND page_id = ?1",
            params![page_id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    let recorded = UserId::from_slice(&author_bytes).unwrap();
    assert_eq!(
        recorded, user_id,
        "create_page audit row must carry the writer's user_id"
    );
}

/// Backward-compat gate (and the headline of the "no behaviour
/// change for legacy installs" promise): anonymous writes leave
/// audit_log.author_id NULL — the partial index stays empty for
/// pre-multi-user history.
#[test]
fn audit_log_records_null_author_for_anonymous_create_page() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let np = page(ws, proj, "notes/anon.md", "anon body");
    assert!(np.author_id.is_none());
    let page_id = upsert_page(&mut conn, &np).unwrap();

    let author: Option<Vec<u8>> = conn
        .query_row(
            "SELECT author_id FROM audit_log \
                 WHERE op = 'create_page' AND page_id = ?1",
            params![page_id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        author.is_none(),
        "anonymous writes must record audit_log.author_id = NULL"
    );
}

/// Supersession rows carry the SUPERSEDING author, not the
/// original. Two consecutive attributed writes (alice then bob)
/// yield a create_page row tagged alice and a supersede_page row
/// tagged bob — point-in-time truth, not "latest author".
#[test]
fn audit_log_supersede_records_new_authors_identity() {
    use sessionmunch_core::UserId;

    let (_tmp, mut conn, ws, proj) = fresh_db();
    let now = jiff::Timestamp::now().as_microsecond();
    let alice = UserId::new();
    let bob = UserId::new();
    conn.execute(
        "INSERT INTO users (id, username, name, email, created_at) \
             VALUES (?1, 'alice', NULL, NULL, ?2), \
                    (?3, 'bob',   NULL, NULL, ?2)",
        params![alice.as_bytes(), now, bob.as_bytes()],
    )
    .unwrap();

    let mut np1 = page(ws, proj, "notes/shared.md", "v1");
    np1.author_id = Some(alice);
    upsert_page(&mut conn, &np1).unwrap();

    let mut np2 = page(ws, proj, "notes/shared.md", "v2 — different body");
    np2.author_id = Some(bob);
    let v2_id = upsert_page(&mut conn, &np2).unwrap();

    let bob_bytes: Vec<u8> = conn
        .query_row(
            "SELECT author_id FROM audit_log \
                 WHERE op = 'supersede_page' AND page_id = ?1",
            params![v2_id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        UserId::from_slice(&bob_bytes).unwrap(),
        bob,
        "supersede_page audit row must carry the SUPERSEDING author"
    );
}

#[test]
fn upsert_page_supersedes_on_body_change() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let id1 = upsert_page(&mut conn, &page(ws, proj, "notes/foo.md", "v1 body")).unwrap();
    let id2 = upsert_page(&mut conn, &page(ws, proj, "notes/foo.md", "v2 body")).unwrap();

    assert_ne!(id1, id2, "supersession must produce a new row id");

    let latest_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE path = ?1 AND is_latest = 1",
            params!["notes/foo.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(latest_count, 1, "exactly one latest version expected");

    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE path = ?1",
            params!["notes/foo.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 2, "old version must remain on disk for history");

    // The newest row should point at the older as its predecessor
    // (supersedes column), so chains are reconstructible.
    let supersedes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT supersedes FROM pages WHERE id = ?1",
            params![&id2.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert!(supersedes.is_some(), "new row must link to its predecessor");
}

/// Idempotency: re-upserting the same body should NOT create a
/// second row. The watcher's reconciliation calls upsert_page on
/// every file on every tick — without this, a quiet repo would
/// accumulate spurious history every 30 seconds.
#[test]
fn upsert_page_is_noop_when_body_unchanged() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let p = page(ws, proj, "notes/foo.md", "same body");
    let id1 = upsert_page(&mut conn, &p).unwrap();
    let id2 = upsert_page(&mut conn, &p).unwrap();

    assert_eq!(id1, id2, "identical body should not supersede");
    conn.execute(
        "UPDATE pages SET updated_at = 123 WHERE id = ?1",
        params![id1.as_bytes()],
    )
    .unwrap();
    let id3 = upsert_page(&mut conn, &p).unwrap();
    assert_eq!(id1, id3, "identical body should keep the same page id");
    let updated_at: i64 = conn
        .query_row(
            "SELECT updated_at FROM pages WHERE id = ?1",
            params![id1.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        updated_at, 123,
        "unchanged content should not dirty the row"
    );
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE path = ?1",
            params!["notes/foo.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 1, "no duplicate row for unchanged content");
}

fn evidence_count(conn: &Connection, page_id: &PageId) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM page_evidence WHERE page_id = ?1",
        params![page_id.as_bytes()],
        |r| r.get(0),
    )
    .unwrap()
}

/// P2 (docs/design-hindsight-borrowings.md §3): a page write's cited
/// evidence accrues in-transaction with the upsert. Reconsolidating the
/// same (unchanged) content from a different session still hits the
/// content short-circuit — same page id, no new version — but the new
/// session's citation lands, and the SAME session citing it again is a
/// no-op (`INSERT OR IGNORE` on the `(page_id, source_kind, source_id)`
/// PK), never inflating the count.
#[test]
fn reconsolidating_the_same_page_accrues_evidence_per_distinct_session() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut p = page(ws, proj, "notes/foo.md", "same body");
    p.evidence = vec![PageEvidence {
        kind: PageEvidenceKind::Session,
        source_id: "session-a".into(),
    }];
    let id = upsert_page(&mut conn, &p).unwrap();
    assert_eq!(evidence_count(&conn, &id), 1);

    p.evidence = vec![PageEvidence {
        kind: PageEvidenceKind::Session,
        source_id: "session-b".into(),
    }];
    let id2 = upsert_page(&mut conn, &p).unwrap();
    assert_eq!(id2, id, "unchanged content must not create a new version");
    assert_eq!(evidence_count(&conn, &id), 2);

    let id3 = upsert_page(&mut conn, &p).unwrap();
    assert_eq!(id3, id);
    assert_eq!(
        evidence_count(&conn, &id),
        2,
        "re-citing the same session is a no-op"
    );
}

/// A page written with no evidence stays at count 0 ("unknown"), and a
/// version created by a real content change (not the idempotent
/// short-circuit) starts its own, separate evidence trail.
#[test]
fn upsert_page_with_no_evidence_stays_at_zero_and_new_versions_start_fresh() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let id1 = upsert_page(&mut conn, &page(ws, proj, "notes/foo.md", "v1 body")).unwrap();
    assert_eq!(evidence_count(&conn, &id1), 0);

    let mut p2 = page(ws, proj, "notes/foo.md", "v2 body");
    p2.evidence = vec![PageEvidence {
        kind: PageEvidenceKind::Session,
        source_id: "session-a".into(),
    }];
    let id2 = upsert_page(&mut conn, &p2).unwrap();
    assert_ne!(id2, id1, "changed body supersedes to a new version");
    assert_eq!(
        evidence_count(&conn, &id1),
        0,
        "the old version is untouched"
    );
    assert_eq!(evidence_count(&conn, &id2), 1);
}

/// Purging a page must take its evidence with it (`ON DELETE CASCADE`,
/// V63) — evidence never outlives the page version it supports.
#[test]
fn deleting_a_page_cascades_its_evidence_rows() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut p = page(ws, proj, "notes/foo.md", "body");
    p.evidence = vec![PageEvidence {
        kind: PageEvidenceKind::Session,
        source_id: "session-a".into(),
    }];
    let id = upsert_page(&mut conn, &p).unwrap();
    assert_eq!(evidence_count(&conn, &id), 1);

    delete_page(
        &mut conn,
        ws,
        proj,
        &PagePath::new("notes/foo.md").unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(
        evidence_count(&conn, &id),
        0,
        "ON DELETE CASCADE must drop evidence with the page"
    );
}

/// OKF conformance happens at this choke point for every writer
/// (docs/okf.md): a page stored with bare frontmatter comes back
/// with the required `type`, a `generated {by, at}` stanza, and
/// provenance derived from the session stamp. Breaking the
/// conform call fails this test.
#[test]
fn stored_pages_are_okf_conformant() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut p = page(ws, proj, "gotchas/build.md", "watch out");
    p.frontmatter_json = serde_json::json!({
        "title": "Build gotcha",
        "session_id": "0192aaaa-0000-7000-8000-000000000000",
        "agent": "claude-code",
    });
    let id = upsert_page(&mut conn, &p).unwrap();
    let fm: String = conn
        .query_row(
            "SELECT frontmatter_json FROM pages WHERE id = ?1",
            params![id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    let fm: serde_json::Value = serde_json::from_str(&fm).unwrap();
    assert_eq!(fm["type"], "Gotcha");
    assert!(
        fm["generated"]["by"]
            .as_str()
            .unwrap()
            .starts_with("process:sessionmunch/")
    );
    assert!(
        fm["generated"]["at"].as_str().unwrap().ends_with('Z'),
        "generated.at stamped on the new version"
    );
    assert_eq!(
        fm["sources"][0]["resource"],
        "sessionmunch://session/0192aaaa-0000-7000-8000-000000000000"
    );
    // producer keys survive as extensions
    assert_eq!(fm["agent"], "claude-code");
}

/// The critical interaction: conformance adds `generated.at`, which
/// differs per write — an unchanged page must STILL be a noop, or
/// every rewrite would supersede itself over a timestamp.
#[test]
fn conformance_does_not_break_idempotent_rewrites() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut p = page(ws, proj, "notes/idem.md", "stable body");
    p.frontmatter_json = serde_json::json!({"title": "Idem"});
    let id1 = upsert_page(&mut conn, &p).unwrap();
    let id2 = upsert_page(&mut conn, &p).unwrap();
    assert_eq!(
        id1, id2,
        "conformed rewrite of identical content superseded"
    );
    let versions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE path = ?1",
            params!["notes/idem.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(versions, 1);
}

/// A producer that already writes a `type` keeps it verbatim — the
/// choke point repairs, it never overrules.
#[test]
fn an_explicit_producer_type_survives_the_choke_point() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut p = page(ws, proj, "notes/custom.md", "body");
    p.frontmatter_json = serde_json::json!({"type": "Custom Kind"});
    let id = upsert_page(&mut conn, &p).unwrap();
    let fm: String = conn
        .query_row(
            "SELECT frontmatter_json FROM pages WHERE id = ?1",
            params![id.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    let fm: serde_json::Value = serde_json::from_str(&fm).unwrap();
    assert_eq!(fm["type"], "Custom Kind");
}

#[test]
fn upsert_page_supersedes_on_frontmatter_change() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut p1 = page(ws, proj, "_slots/project_context.md", "same body");
    p1.frontmatter_json = serde_json::json!({
        "title": "Project context",
        "slot_kind": "state",
    });
    let id1 = upsert_page(&mut conn, &p1).unwrap();

    let mut p2 = p1.clone();
    p2.frontmatter_json = serde_json::json!({
        "title": "Project context",
        "slot_kind": "invariant",
    });
    let id2 = upsert_page(&mut conn, &p2).unwrap();

    assert_ne!(id1, id2, "frontmatter-only changes must supersede");
    let latest_frontmatter: String = conn
        .query_row(
            "SELECT frontmatter_json FROM pages WHERE id = ?1 AND is_latest = 1",
            params![id2.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        latest_frontmatter.contains("invariant"),
        "latest row should store the updated slot_kind"
    );
}

#[test]
fn upsert_page_persists_and_resolves_links() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut source = page(ws, proj, "concepts/source.md", "see target");
    source.links = vec![PagePath::new("decisions/target.md").unwrap().into()];
    let source_id = upsert_page(&mut conn, &source).unwrap();

    let unresolved: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM links \
                 WHERE from_page_id = ?1 AND to_path = ?2 AND to_page_id IS NULL",
            params![source_id.as_bytes(), "decisions/target.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unresolved, 1, "forward link should persist unresolved");

    let target_id = upsert_page(
        &mut conn,
        &page(ws, proj, "decisions/target.md", "target body"),
    )
    .unwrap();

    let resolved: Option<Vec<u8>> = conn
        .query_row(
            "SELECT to_page_id FROM links WHERE from_page_id = ?1 AND to_path = ?2",
            params![source_id.as_bytes(), "decisions/target.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(resolved.as_deref(), Some(&target_id.as_bytes()[..]));
}

/// A `[[infra:runbooks/02.md]]` link from one project resolves to a page
/// in a sibling project once that page exists — the cross-project edge.
#[test]
fn upsert_page_resolves_cross_project_link() {
    let (_tmp, mut conn, ws, scratch) = fresh_db();
    let infra = get_or_create_project(&mut conn, &ws, "infra", None).unwrap();

    let mut source = page(ws, scratch, "concepts/dep.md", "depends on infra runbook");
    source.links = vec![LinkTarget {
        workspace: None,
        project: Some("infra".into()),
        path: PagePath::new("runbooks/02.md").unwrap(),
        relation: None,
    }];
    let source_id = upsert_page(&mut conn, &source).unwrap();

    // Persisted with the scope, unresolved until the target project's page exists.
    let (to_project, resolved): (Option<String>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT to_project, to_page_id FROM links \
                 WHERE from_page_id = ?1 AND to_path = ?2",
            params![source_id.as_bytes(), "runbooks/02.md"],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(to_project.as_deref(), Some("infra"));
    assert!(
        resolved.is_none(),
        "cross-project link is unresolved before the target exists"
    );

    // Create the target in `infra` → the forward link repoints across projects.
    let target_id =
        upsert_page(&mut conn, &page(ws, infra, "runbooks/02.md", "the runbook")).unwrap();
    let resolved: Option<Vec<u8>> = conn
        .query_row(
            "SELECT to_page_id FROM links WHERE from_page_id = ?1 AND to_path = ?2",
            params![source_id.as_bytes(), "runbooks/02.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some(&target_id.as_bytes()[..]),
        "link must resolve across projects once the target lands"
    );
}

/// Store-boundary defense in depth: an unbounded handoff field or list
/// (a caller that skipped its own cap) is bounded before it reaches the
/// DB, so pathological content cannot grow the store without limit.
#[test]
fn insert_handoff_bounds_oversized_fields_at_the_store() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let huge = "x".repeat(HANDOFF_FIELD_MAX_BYTES * 3);
    let many: Vec<String> = (0..HANDOFF_LIST_MAX_ITEMS * 2)
        .map(|i| format!("step {i}"))
        .collect();
    let new = NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: None,
        summary: huge.clone(),
        open_questions: vec![huge.clone()],
        next_steps: many,
        files_touched: vec![],
        owner_user: None,
    };
    let id = insert_handoff(&mut conn, &new).unwrap();

    let (summary, open_q, next_s): (String, String, String) = conn
        .query_row(
            "SELECT summary, open_questions, next_steps FROM handoffs WHERE id = ?1",
            params![&id.as_bytes()[..]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(
        summary.len() <= HANDOFF_FIELD_MAX_BYTES,
        "summary must be bounded: {} bytes",
        summary.len()
    );
    let open: Vec<String> = serde_json::from_str(&open_q).unwrap();
    assert!(
        open[0].len() <= HANDOFF_FIELD_MAX_BYTES,
        "list item bounded"
    );
    let steps: Vec<String> = serde_json::from_str(&next_s).unwrap();
    assert!(
        steps.len() <= HANDOFF_LIST_MAX_ITEMS,
        "list length bounded: {}",
        steps.len()
    );
}

/// Handoff state machine: insert → Open; accept_handoff → Accepted
/// with accepted_by stamped. Calling accept again must be safe
/// (idempotent at the DB level) because hooks fire-and-forget.
#[test]
fn accept_handoff_transitions_open_to_accepted() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let new = NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: None,
        summary: "test summary".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let id = insert_handoff(&mut conn, &new).unwrap();

    // Pre-state: Open, accepted_by NULL.
    let (state, accepted_by): (String, Option<String>) = conn
        .query_row(
            "SELECT state, accepted_by FROM handoffs WHERE id = ?1",
            params![&id.as_bytes()[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "open");
    assert!(accepted_by.is_none());

    accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj)).unwrap();
    let (state, accepted_by): (String, Option<String>) = conn
        .query_row(
            "SELECT state, accepted_by FROM handoffs WHERE id = ?1",
            params![&id.as_bytes()[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "accepted");
    assert_eq!(accepted_by.as_deref(), Some("codex"));

    // Idempotency: accepting an already-accepted handoff must
    // either succeed silently or fail clearly, never corrupt
    // the row. (Current impl is a no-op UPDATE with a state
    // guard.)
    let second = accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj));
    assert!(second.is_ok(), "double-accept must not error");
}

#[test]
fn lifecycle_only_receiver_releases_its_handoff_and_cannot_reclaim_after_end() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let receiver = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: receiver,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: Some("/repo".into()),
            actor_user: None,
        },
    )
    .unwrap();
    let new_handoff = || NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: Some("/repo".into()),
        summary: "real pending work".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let handoff_id = insert_handoff(&mut conn, &new_handoff()).unwrap();
    let mut claim = handoff_acceptance(handoff_id, ws, proj);
    claim.accepting_session = Some(receiver);
    assert!(accept_handoff(&mut conn, &claim).unwrap());
    let second_handoff = insert_handoff(&mut conn, &new_handoff()).unwrap();
    let mut duplicate_start = handoff_acceptance(second_handoff, ws, proj);
    duplicate_start.accepting_session = Some(receiver);
    assert!(
        !accept_handoff(&mut conn, &duplicate_start).unwrap(),
        "a repeated SessionStart must not consume a second handoff"
    );

    assert_eq!(
        end_lifecycle_only_session(&mut conn, &receiver).unwrap(),
        LifecycleOnlyEndOutcome::Ended {
            reopened_handoff: Some(handoff_id)
        }
    );
    let (state, accepted_by, accepted_at, accepted_session): (
        String,
        Option<String>,
        Option<i64>,
        Option<Vec<u8>>,
    ) = conn
        .query_row(
            "SELECT state, accepted_by, accepted_at, accepted_by_session \
                 FROM handoffs WHERE id = ?1",
            params![handoff_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(state, "open");
    assert!(accepted_by.is_none());
    assert!(accepted_at.is_none());
    assert!(accepted_session.is_none());
    let (ended_at, summary_page): (Option<i64>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT ended_at, summary_page_id FROM sessions WHERE id = ?1",
            params![receiver.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(ended_at.is_some());
    assert!(summary_page.is_none());
    assert_eq!(audit_row_for(&conn, "release_lifecycle_only_handoff").0, 1);
    let second_state: String = conn
        .query_row(
            "SELECT state FROM handoffs WHERE id = ?1",
            params![second_handoff.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(second_state, "open");

    let next_handoff = insert_handoff(&mut conn, &new_handoff()).unwrap();
    let mut stale_claim = handoff_acceptance(next_handoff, ws, proj);
    stale_claim.accepting_session = Some(receiver);
    let error = accept_handoff(&mut conn, &stale_claim)
        .expect_err("an out-of-order startup fetch must not bind to an ended session");
    assert!(matches!(error, StoreError::InvalidState(_)));
    let state: String = conn
        .query_row(
            "SELECT state FROM handoffs WHERE id = ?1",
            params![next_handoff.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "open");
}

#[test]
fn lifecycle_only_end_compare_and_set_yields_to_substantive_observation() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let receiver = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: receiver,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: Some("/repo".into()),
            actor_user: None,
        },
    )
    .unwrap();
    let handoff_id = insert_handoff(
        &mut conn,
        &NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "continue".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        },
    )
    .unwrap();
    let mut claim = handoff_acceptance(handoff_id, ws, proj);
    claim.accepting_session = Some(receiver);
    assert!(accept_handoff(&mut conn, &claim).unwrap());
    insert_observation(
        &mut conn,
        &NewObservation {
            session_id: receiver,
            workspace_id: ws,
            project_id: proj,
            kind: ObservationKind::PreToolUse,
            extension: None,
            source_event: None,
            title: "Read".into(),
            body: "README.md".into(),
            importance: 5,
        },
    )
    .unwrap();

    assert_eq!(
        end_lifecycle_only_session(&mut conn, &receiver).unwrap(),
        LifecycleOnlyEndOutcome::Substantive
    );
    let ended_at: Option<i64> = conn
        .query_row(
            "SELECT ended_at FROM sessions WHERE id = ?1",
            params![receiver.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ended_at.is_none());
    let state: String = conn
        .query_row(
            "SELECT state FROM handoffs WHERE id = ?1",
            params![handoff_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "accepted");
}

/// Pins the two-sided invariant with `is_ephemeral_session` in
/// sessionmunch-hooks/src/router.rs: a session with only `session-start`,
/// `stop`, `session-end` (the #662 OpenCode internal-session shape) has
/// no positive-work observation and must end as `Ended`, not
/// `Substantive`.
#[test]
fn end_lifecycle_only_session_treats_stop_only_session_as_ended() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let receiver = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: receiver,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::OpenCode,
            cwd: Some("/repo".into()),
            actor_user: None,
        },
    )
    .unwrap();
    insert_observation(
        &mut conn,
        &NewObservation {
            session_id: receiver,
            workspace_id: ws,
            project_id: proj,
            kind: ObservationKind::Stop,
            extension: None,
            source_event: None,
            title: "stop".into(),
            body: String::new(),
            importance: 5,
        },
    )
    .unwrap();

    assert_eq!(
        end_lifecycle_only_session(&mut conn, &receiver).unwrap(),
        LifecycleOnlyEndOutcome::Ended {
            reopened_handoff: None
        },
        "a Stop-only session has no user-prompt/pre-tool-use/post-tool-use \
             observation, so it must be classified ended, not substantive"
    );
}

/// Companion to the Stop-only case above: a `user-prompt` observation
/// alone (no lifecycle bookends yet inserted beyond SessionStart) makes
/// the session substantive.
#[test]
fn end_lifecycle_only_session_treats_user_prompt_as_substantive() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let receiver = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: receiver,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::OpenCode,
            cwd: Some("/repo".into()),
            actor_user: None,
        },
    )
    .unwrap();
    insert_observation(
        &mut conn,
        &NewObservation {
            session_id: receiver,
            workspace_id: ws,
            project_id: proj,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "hello".into(),
            body: "do the thing".into(),
            importance: 5,
        },
    )
    .unwrap();

    assert_eq!(
        end_lifecycle_only_session(&mut conn, &receiver).unwrap(),
        LifecycleOnlyEndOutcome::Substantive
    );
}

#[test]
fn failed_auto_handoff_insert_rolls_back_same_cwd_expiration() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let first_session = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: first_session,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some("/repo".into()),
            actor_user: None,
        },
    )
    .unwrap();
    let handoff = |session_id| NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: Some(session_id),
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: Some("/repo".into()),
        summary: "continue".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let first = insert_handoff(&mut conn, &handoff(first_session)).unwrap();

    let error = insert_handoff(&mut conn, &handoff(SessionId::new()))
        .expect_err("a missing source session must violate the foreign key");
    assert!(matches!(error, StoreError::Sqlite(_)));
    let state: String = conn
        .query_row(
            "SELECT state FROM handoffs WHERE id = ?1",
            params![first.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        state, "open",
        "the failed transaction must restore the prior handoff"
    );
    assert_eq!(
        audit_row_for(&conn, "expire_superseded_handoffs").0,
        0,
        "the rolled-back expiration must leave no audit row"
    );
}

/// The handoff lifecycle (insert / accept / cancel) writes audit rows,
/// scoped to the handoff's workspace/project, with a NULL author (handoffs
/// are agent/session-keyed, not owned by a DB user).
#[test]
fn audit_log_records_handoff_lifecycle_with_null_author() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let new = || NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: None,
        summary: "s".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let id = insert_handoff(&mut conn, &new()).unwrap();
    accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj)).unwrap();
    let id2 = insert_handoff(&mut conn, &new()).unwrap();
    assert!(cancel_handoff(&mut conn, &id2, &ws, &proj, &OwnerFilter::Any).unwrap());

    for op in ["insert_handoff", "accept_handoff", "cancel_handoff"] {
        let (count, author) = audit_row_for(&conn, op);
        assert!(count >= 1, "{op} must be audited");
        assert!(author.is_none(), "{op} audit author must be NULL");
    }
    assert_eq!(
        audit_row_for(&conn, "insert_handoff").0,
        2,
        "two inserts → two rows"
    );
    // Idempotent misses stay out of the trail: a double-accept and a
    // cancel of an already-accepted handoff change no row, audit nothing.
    accept_handoff(&mut conn, &handoff_acceptance(id, ws, proj)).unwrap();
    assert!(!cancel_handoff(&mut conn, &id, &ws, &proj, &OwnerFilter::Any).unwrap());
    assert_eq!(
        audit_row_for(&conn, "accept_handoff").0,
        1,
        "a no-op double-accept must not write a second audit row"
    );
    assert_eq!(
        audit_row_for(&conn, "cancel_handoff").0,
        1,
        "cancelling a non-open handoff must not write an audit row"
    );
    let ws_bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT workspace_id FROM audit_log WHERE op = 'accept_handoff'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ws_bytes.as_deref(),
        Some(&ws.as_bytes()[..]),
        "handoff audit is scoped to its workspace"
    );
}

/// The stored cwd is normalized (trailing path separator stripped) at insert time
/// so trailing-slash drift between agent payloads cannot break the next
/// session's path-boundary match. Covers both manual and auto handoffs,
/// since both go through `insert_handoff`.
#[test]
fn insert_handoff_strips_trailing_separator_from_cwd() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let new = NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: Some(std::path::PathBuf::from("/home/u/repo/")),
        summary: "trailing slash".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let id = insert_handoff(&mut conn, &new).unwrap();
    let cwd: Option<String> = conn
        .query_row(
            "SELECT cwd FROM handoffs WHERE id = ?1",
            params![&id.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cwd.as_deref(), Some("/home/u/repo"));

    let windows = NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: Some(std::path::PathBuf::from(r"C:\repo\")),
        summary: "trailing backslash".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let id = insert_handoff(&mut conn, &windows).unwrap();
    let cwd: Option<String> = conn
        .query_row(
            "SELECT cwd FROM handoffs WHERE id = ?1",
            params![&id.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cwd.as_deref(), Some(r"C:\repo"));
}

#[test]
fn cancel_handoff_transitions_open_to_expired() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let new = NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: None,
        summary: "accidental handoff".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let id = insert_handoff(&mut conn, &new).unwrap();

    assert!(cancel_handoff(&mut conn, &id, &ws, &proj, &OwnerFilter::Any).unwrap());
    let state: String = conn
        .query_row(
            "SELECT state FROM handoffs WHERE id = ?1",
            params![&id.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "expired");

    assert!(
        !cancel_handoff(&mut conn, &id, &ws, &proj, &OwnerFilter::Any).unwrap(),
        "double-cancel should be a no-op"
    );
}

/// Supported hook agents persist concrete agent_kind values. V01's CHECK
/// omitted agents added after launch; regression for hook-router WARNs.
#[test]
fn begin_session_accepts_all_supported_agent_kinds() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    for agent_kind in AgentKind::ALL {
        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind,
                cwd: Some(std::path::PathBuf::from(r"C:\GIT\sessionmunch")),
                actor_user: None,
            },
        )
        .unwrap();

        let stored: String = conn
            .query_row(
                "SELECT agent_kind FROM sessions WHERE id = ?1",
                params![&sid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, agent_kind.as_str());
    }
}

/// end_session links the synthesised summary page so callers can
/// jump straight from session row to summary.
#[test]
fn end_session_links_summary_page_when_provided() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let sid = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        },
    )
    .unwrap();
    let page_id = upsert_page(
        &mut conn,
        &page(ws, proj, "sessions/abc.md", "summary body"),
    )
    .unwrap();
    end_session(&mut conn, &sid, Some(&page_id)).unwrap();

    let summary: Option<Vec<u8>> = conn
        .query_row(
            "SELECT summary_page_id FROM sessions WHERE id = ?1",
            params![&sid.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        summary.is_some(),
        "summary_page_id must persist when supplied"
    );
    let bytes = summary.unwrap();
    assert_eq!(bytes.len(), 16);
    assert_eq!(&bytes[..], &page_id.as_bytes()[..]);
}

/// end_session without a summary leaves the column NULL — the
/// session ended but no page was synthesised (e.g. zero
/// observations recorded). This must not be confused with the
/// summary-linked case.
#[test]
fn end_session_without_summary_page_id_leaves_null() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let sid = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        },
    )
    .unwrap();
    end_session(&mut conn, &sid, None).unwrap();
    let summary: Option<Vec<u8>> = conn
        .query_row(
            "SELECT summary_page_id FROM sessions WHERE id = ?1",
            params![&sid.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert!(summary.is_none());
}

/// Embeddings are keyed by page_id (PK). Re-storing for the same
/// page must REPLACE, not duplicate — otherwise `sessionmunch embed
/// --reembed` would multiply rows on each run.
#[test]
fn store_embedding_replaces_existing_row() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let pid = upsert_page(&mut conn, &page(ws, proj, "notes/x.md", "body")).unwrap();
    store_embedding(
        &mut conn,
        &pid,
        &vec![0u8; 1536 * 4],
        "test",
        "model-a",
        1536,
    )
    .unwrap();
    store_embedding(
        &mut conn,
        &pid,
        &vec![1u8; 1536 * 4],
        "test",
        "model-b",
        1536,
    )
    .unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_embeddings WHERE page_id = ?1",
            params![&pid.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "embedding row must be replaced, not duplicated");

    let model: String = conn
        .query_row(
            "SELECT model FROM page_embeddings WHERE page_id = ?1",
            params![&pid.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(model, "model-b", "latest model metadata wins");
}

#[test]
fn store_embeddings_batches_rows_in_one_call() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let p1 = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body a")).unwrap();
    let p2 = upsert_page(&mut conn, &page(ws, proj, "notes/b.md", "body b")).unwrap();

    store_embeddings(
        &mut conn,
        &[
            EmbeddingWrite {
                page_id: p1,
                vector_bytes: vec![0u8; 4],
                provider: "test".into(),
                model: "model".into(),
                dim: 1,
            },
            EmbeddingWrite {
                page_id: p2,
                vector_bytes: vec![1u8; 4],
                provider: "test".into(),
                model: "model".into(),
                dim: 1,
            },
        ],
    )
    .unwrap();

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM page_embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn delete_stale_page_embeddings_removes_mismatched_rows() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
    let p1 = upsert_page(&mut conn, &page(ws, proj, "a.md", "body a")).unwrap();
    let p2 = upsert_page(&mut conn, &page(ws, proj, "b.md", "body b")).unwrap();
    let p3 = upsert_page(&mut conn, &page(ws, other, "c.md", "body c")).unwrap();
    let old = upsert_page(&mut conn, &page(ws, proj, "old.md", "old body")).unwrap();
    let _new = upsert_page(&mut conn, &page(ws, proj, "old.md", "new body")).unwrap();
    store_embedding(
        &mut conn,
        &p1,
        &[0u8; 4],
        "google",
        "models/gemini-embedding-001",
        768,
    )
    .unwrap();
    store_embedding(
        &mut conn,
        &p3,
        &[2u8; 4],
        "google",
        "models/gemini-embedding-001",
        768,
    )
    .unwrap();
    store_embedding(
        &mut conn,
        &p2,
        &[1u8; 4],
        "openai",
        "openai/text-embedding-3-small",
        1536,
    )
    .unwrap();
    store_embedding(
        &mut conn,
        &old,
        &[3u8; 4],
        "openai",
        "openai/text-embedding-3-small",
        1536,
    )
    .unwrap();
    let n = super::delete_stale_page_embeddings(
        &mut conn,
        &ws,
        Some(&proj),
        "openai",
        "openai/text-embedding-3-small",
        1536,
    )
    .unwrap();
    assert_eq!(n, 2);
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM page_embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 2);
    let model: String = conn
        .query_row(
            "SELECT model FROM page_embeddings WHERE page_id = ?1",
            params![&p2.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(model, "openai/text-embedding-3-small");
    let other_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_embeddings WHERE page_id = ?1",
            params![&p3.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        other_rows, 1,
        "explicit project purge must not touch siblings"
    );
}

#[test]
fn path_search_text_indexes_slug_and_words() {
    // Both forms: hyphenated slug kept whole, plus split into words.
    assert_eq!(
        path_search_text("notes/foo-bar.md"),
        "notes foo-bar md notes foo bar md"
    );
    assert_eq!(path_search_text("a/b_c.md"), "a b_c md a b c md");
}

/// A page is findable by its PATH slug even when the slug appears in
/// neither the title nor the body — the V17 `path_search` FTS column.
#[test]
fn fts_matches_page_by_path_slug_not_in_body() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    // Title + body deliberately avoid the slug words.
    let mut p = page(
        ws,
        proj,
        "notes/followup-bulk-rename-runbook-titles.md",
        "totally unrelated prose about elephants",
    );
    p.title = "Elephants".into();
    upsert_page(&mut conn, &p).unwrap();

    // The slug, as a quoted single token (how prepare_fts5_query renders a
    // hyphenated term), matches via the path_search column.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages_fts \
                 WHERE pages_fts MATCH ?1",
            params!["\"followup-bulk-rename-runbook-titles\""],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "slug in path must be searchable");

    // A distinct path segment is independently searchable too.
    let seg: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'runbook'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seg, 1, "path segment token must match");

    // Body words still match (regression: body stays indexed at col 1).
    let body_hit: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'elephants'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body_hit, 1, "body must remain searchable");
}

#[test]
fn pages_fts_path_migration_preserves_accent_folding() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let mut p = page(ws, proj, "notes/descricao.md", "descrição do projeto");
    p.title = "Descrição".into();
    upsert_page(&mut conn, &p).unwrap();

    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'descricao'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "page FTS should remain accent-insensitive");
}

#[test]
fn pages_fts_update_trigger_ignores_access_counter_updates() {
    let (_tmp, conn, _ws, _proj) = fresh_db();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'pages_fts_au'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sql.contains("AFTER UPDATE OF title, body, path_search ON pages"),
        "pages_fts_au must not fire on access_count/last_accessed_at updates: {sql}"
    );
}

/// True move: re-stamping a project's workspace_id keeps the same
/// project_id and carries pages, sessions and observations along —
/// the whole point of the lossless move. The summary counts must
/// match what actually moved.
#[test]
fn move_project_workspace_restamps_all_domain_rows() {
    use sessionmunch_core::ObservationKind;

    let (_tmp, mut conn, src_ws, proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "djalmajr").unwrap();

    // Seed a page, a session and an observation under the source ws.
    let page_id = upsert_page(&mut conn, &page(src_ws, proj, "notes/a.md", "body")).unwrap();
    let sid = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: sid,
            workspace_id: src_ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        },
    )
    .unwrap();
    insert_observation(
        &mut conn,
        &NewObservation {
            session_id: sid,
            workspace_id: src_ws,
            project_id: proj,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "t".into(),
            body: "b".into(),
            importance: 5,
        },
    )
    .unwrap();
    end_session(&mut conn, &sid, None).unwrap();
    crate::session_consolidation::enqueue(&mut conn, src_ws, proj, sid).unwrap();
    conn.execute(
        "INSERT INTO auto_improve_rejections \
             (id, workspace_id, project_id, reason, normalized_fingerprint, summary, created_at) \
             VALUES (?1, ?2, ?3, 'rejected', 'fingerprint', 'summary', 1)",
        params![
            uuid::Uuid::new_v4().as_bytes(),
            src_ws.as_bytes(),
            proj.as_bytes(),
        ],
    )
    .unwrap();
    let managed = crate::workstream::prepare_run(
        &mut conn,
        &crate::PrepareWorkstreamRun {
            workspace_id: src_ws,
            project_id: proj,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: vec![AgentKind::Codex],
            selection: crate::WorkstreamSelection::Current,
            lease_owner: "test".into(),
        },
    )
    .unwrap();

    let summary = move_project_workspace(&mut conn, &proj, &src_ws, &dst_ws).unwrap();
    assert_eq!(summary.pages_moved, 1);
    assert_eq!(summary.sessions_moved, 1);
    assert_eq!(summary.observations_moved, 1);
    assert_eq!(summary.auto_improve_rejections_moved, 1);
    assert_eq!(summary.session_consolidation_jobs_moved, 1);
    assert_eq!(summary.workstreams_moved, 1);

    // The project_id is unchanged; every row now points at dst_ws.
    // `projects` keys the project by `id`; child tables by `project_id`.
    let count_in = |table: &str, ws: &sessionmunch_core::WorkspaceId| -> i64 {
        let id_col = if table == "projects" {
            "id"
        } else {
            "project_id"
        };
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {id_col} = ?1 AND workspace_id = ?2"),
            params![&proj.as_bytes()[..], ws.as_bytes()],
            |r| r.get(0),
        )
        .unwrap()
    };
    for table in [
        "projects",
        "pages",
        "sessions",
        "observations",
        "auto_improve_rejections",
        "session_consolidation_jobs",
        "workstreams",
    ] {
        assert_eq!(count_in(table, &dst_ws), 1, "{table} must move to dst ws");
        assert_eq!(count_in(table, &src_ws), 0, "{table} must leave src ws");
    }
    let managed_run_still_attached: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM managed_runs mr \
                 JOIN workstreams w ON w.id = mr.workstream_id \
                 WHERE mr.id = ?1 AND w.workspace_id = ?2 AND w.project_id = ?3",
            params![
                managed.run_id.as_bytes(),
                dst_ws.as_bytes(),
                proj.as_bytes(),
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(managed_run_still_attached, 1);
    // The page keeps its id (embeddings/links follow via page_id).
    let still_there: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE id = ?1 AND workspace_id = ?2",
            params![&page_id.as_bytes()[..], dst_ws.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(still_there, 1);
}

/// A same-named project already in the destination workspace makes the
/// projects UPDATE collide with UNIQUE(workspace_id, name); the whole
/// transaction must roll back, leaving the source intact. The admin
/// layer detects this merge case up front and routes it to copy+purge.
#[test]
fn move_project_workspace_rolls_back_on_name_collision() {
    let (_tmp, mut conn, src_ws, proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "djalmajr").unwrap();
    // Destination already holds a project named "scratch".
    get_or_create_project(&mut conn, &dst_ws, "scratch", None).unwrap();
    upsert_page(&mut conn, &page(src_ws, proj, "notes/a.md", "body")).unwrap();

    let err = move_project_workspace(&mut conn, &proj, &src_ws, &dst_ws);
    assert!(err.is_err(), "name collision must fail the move");

    // Source rows untouched after rollback.
    let src_pages: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE project_id = ?1 AND workspace_id = ?2",
            params![&proj.as_bytes()[..], src_ws.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(src_pages, 1, "rollback must preserve source pages");
}

/// Seed one session in `(ws, proj)` with everything `move_session`
/// re-stamps: two observations, one handoff it produced, one completed
/// consolidation job, one auto-improve run, one scheduler claim, and two
/// versions of its `sessions/<id>.md` page. Returns the session id.
fn seed_movable_session(conn: &mut Connection, ws: WorkspaceId, proj: ProjectId) -> SessionId {
    use sessionmunch_core::ObservationKind;

    let sid = SessionId::new();
    begin_session(
        conn,
        &NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some("/repo/src".into()),
            actor_user: None,
        },
    )
    .unwrap();
    for n in 0..2 {
        insert_observation(
            conn,
            &NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: format!("movable-obs-{n}"),
                body: "zebra token body".into(),
                importance: 5,
            },
        )
        .unwrap();
    }
    end_session(conn, &sid, None).unwrap();
    insert_handoff(
        conn,
        &NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: Some(sid),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "handoff from the movable session".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        },
    )
    .unwrap();
    assert!(crate::session_consolidation::enqueue(conn, ws, proj, sid).unwrap());
    conn.execute(
        "UPDATE session_consolidation_jobs SET state = 'completed', completed_at = 1 \
             WHERE session_id = ?1",
        params![sid.as_bytes()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_improve_runs \
             (id, workspace_id, project_id, session_id, proposal_actor_json, created_at) \
             VALUES (?1, ?2, ?3, ?4, '{}', 1)",
        params![
            uuid::Uuid::new_v4().as_bytes(),
            ws.as_bytes(),
            proj.as_bytes(),
            sid.as_bytes()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_improve_scheduler_claims \
             (workspace_id, project_id, session_id, claimed_at) VALUES (?1, ?2, ?3, 1)",
        params![ws.as_bytes(), proj.as_bytes(), sid.as_bytes()],
    )
    .unwrap();
    let path = format!("sessions/{sid}.md");
    upsert_page(conn, &page(ws, proj, &path, "first version")).unwrap();
    upsert_page(conn, &page(ws, proj, &path, "second version")).unwrap();
    sid
}

/// Rows of `table` keyed to `sid` (via `session_col`) that sit in the
/// given scope.
fn session_rows_in_scope(
    conn: &Connection,
    table: &str,
    session_col: &str,
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
) -> i64 {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM {table} \
                 WHERE {session_col} = ?1 AND workspace_id = ?2 AND project_id = ?3"
        ),
        params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        |r| r.get(0),
    )
    .unwrap()
}

/// `(all versions, latest versions)` of the session page in a scope.
fn session_page_versions(
    conn: &Connection,
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
) -> (i64, i64) {
    let path = format!("sessions/{sid}.md");
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(is_latest), 0) FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
        params![ws.as_bytes(), proj.as_bytes(), path],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// Observations of `sid` reachable through FTS in a scope, i.e. what a
/// scoped search would return after the move.
fn fts_hits_in_scope(conn: &Connection, sid: SessionId, ws: WorkspaceId, proj: ProjectId) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM observations_fts \
             JOIN observations ON observations.rowid = observations_fts.rowid \
             WHERE observations_fts MATCH 'zebra' \
               AND observations.session_id = ?1 \
               AND observations.workspace_id = ?2 AND observations.project_id = ?3",
        params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        |r| r.get(0),
    )
    .unwrap()
}

/// The `(table, session column)` pairs `move_session` re-stamps besides
/// `sessions` and `pages`.
const MOVE_SESSION_CHILD_TABLES: [(&str, &str); 5] = [
    ("observations", "session_id"),
    ("handoffs", "from_session_id"),
    ("session_consolidation_jobs", "session_id"),
    ("auto_improve_runs", "session_id"),
    ("auto_improve_scheduler_claims", "session_id"),
];

fn assert_session_bundle_in_scope(
    conn: &Connection,
    sid: SessionId,
    here: (WorkspaceId, ProjectId),
    gone: (WorkspaceId, ProjectId),
) {
    assert_eq!(
        session_rows_in_scope(conn, "sessions", "id", sid, here.0, here.1),
        1
    );
    assert_eq!(
        session_rows_in_scope(conn, "sessions", "id", sid, gone.0, gone.1),
        0
    );
    for (table, col) in MOVE_SESSION_CHILD_TABLES {
        let expected = if table == "observations" { 2 } else { 1 };
        assert_eq!(
            session_rows_in_scope(conn, table, col, sid, here.0, here.1),
            expected,
            "{table} rows must sit in the expected scope"
        );
        assert_eq!(
            session_rows_in_scope(conn, table, col, sid, gone.0, gone.1),
            0,
            "{table} rows must not sit in the other scope"
        );
    }
}

#[test]
fn move_session_dry_run_reports_counts_and_changes_nothing() {
    let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
    let sid = seed_movable_session(&mut conn, src_ws, src_proj);

    let summary = move_session(
        &mut conn,
        sid,
        dst_ws,
        dst_proj,
        PagesMode::Move,
        None,
        false,
    )
    .unwrap();
    assert!(summary.session_moved);
    assert_eq!(summary.observations, 2);
    assert_eq!(summary.handoffs, 1);
    assert_eq!(summary.consolidation_jobs, 1);
    assert_eq!(summary.auto_improve_runs, 1);
    assert_eq!(summary.auto_improve_claims, 1);
    assert_eq!(summary.page_versions_moved, 2);
    assert_eq!(summary.pages_regenerated, 0);
    assert_eq!(
        summary.page_path.as_deref(),
        Some(format!("sessions/{sid}.md").as_str())
    );
    assert_eq!(summary.from_workspace, src_ws);
    assert_eq!(summary.from_project, src_proj);
    assert_eq!(summary.to_workspace, dst_ws);
    assert_eq!(summary.to_project, dst_proj);
    assert_eq!(summary.cwd.as_deref(), Some("/repo/src"));

    // Rolled back: everything still lives in the source scope.
    assert_session_bundle_in_scope(&conn, sid, (src_ws, src_proj), (dst_ws, dst_proj));
    assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (2, 1));
    assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (0, 0));
    assert_eq!(fts_hits_in_scope(&conn, sid, src_ws, src_proj), 2);
    let audited: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE op = 'move_session'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 0, "dry run must not leave an audit row");
}

#[test]
fn move_session_restamps_dependent_rows_and_page_versions() {
    let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
    let sid = seed_movable_session(&mut conn, src_ws, src_proj);
    point_summary_at_latest(&mut conn, sid, src_ws, src_proj);
    let pointer = summary_page_id(&conn, sid);

    let summary = move_session(
        &mut conn,
        sid,
        dst_ws,
        dst_proj,
        PagesMode::Move,
        None,
        true,
    )
    .unwrap();
    assert!(summary.session_moved);
    assert_eq!(summary.observations, 2);
    assert_eq!(summary.page_versions_moved, 2);
    assert_eq!(summary.pages_regenerated, 0);
    assert_eq!(summary.page_versions_in_target, 2);
    // The page kept its id, so the pointer is still right.
    assert_eq!(summary_page_id(&conn, sid), pointer);

    assert_session_bundle_in_scope(&conn, sid, (dst_ws, dst_proj), (src_ws, src_proj));
    // Both versions moved and the supersession chain kept exactly one latest.
    assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (2, 1));
    assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (0, 0));
    // The FTS index followed the observations into the new scope.
    assert_eq!(fts_hits_in_scope(&conn, sid, dst_ws, dst_proj), 2);
    assert_eq!(fts_hits_in_scope(&conn, sid, src_ws, src_proj), 0);
    // cwd is historical truth and stays put.
    let cwd: Option<String> = conn
        .query_row(
            "SELECT cwd FROM sessions WHERE id = ?1",
            params![sid.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cwd.as_deref(), Some("/repo/src"));
    let audited: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log \
                 WHERE op = 'move_session' AND workspace_id = ?1 AND project_id = ?2",
            params![dst_ws.as_bytes(), dst_proj.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 1);
}

#[test]
fn move_session_rejects_page_path_taken_in_target_and_rolls_back() {
    let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
    let sid = seed_movable_session(&mut conn, src_ws, src_proj);
    let path = format!("sessions/{sid}.md");
    upsert_page(&mut conn, &page(dst_ws, dst_proj, &path, "already here")).unwrap();

    let err = move_session(
        &mut conn,
        sid,
        dst_ws,
        dst_proj,
        PagesMode::Move,
        None,
        true,
    )
    .expect_err("latest page at the same path in the target must block the move");
    assert!(
        matches!(&err, StoreError::PagePathTaken { path: p } if *p == path),
        "unexpected error: {err:?}"
    );
    // Nothing moved: the session rows were re-stamped inside the same
    // transaction and must have rolled back with the page check.
    assert_session_bundle_in_scope(&conn, sid, (src_ws, src_proj), (dst_ws, dst_proj));
    assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (2, 1));
    assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (1, 1));
}

#[test]
fn move_session_regenerate_retires_source_page_and_moves_rows() {
    let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
    let sid = seed_movable_session(&mut conn, src_ws, src_proj);
    // A latest page at the path in the target is fine under Regenerate.
    let path = format!("sessions/{sid}.md");
    upsert_page(&mut conn, &page(dst_ws, dst_proj, &path, "already here")).unwrap();
    // The session points at its (source) latest page, as consolidation
    // leaves it.
    point_summary_at_latest(&mut conn, sid, src_ws, src_proj);

    let summary = move_session(
        &mut conn,
        sid,
        dst_ws,
        dst_proj,
        PagesMode::Regenerate,
        None,
        true,
    )
    .unwrap();
    assert!(summary.session_moved);
    assert_eq!(summary.page_versions_moved, 0);
    assert_eq!(summary.pages_regenerated, 1);
    assert_eq!(summary.page_path.as_deref(), Some(path.as_str()));

    assert_session_bundle_in_scope(&conn, sid, (dst_ws, dst_proj), (src_ws, src_proj));
    // Source versions stay where they are, none of them latest anymore,
    // and the session no longer points at the retired page.
    assert_eq!(session_page_versions(&conn, sid, src_ws, src_proj), (2, 0));
    assert_eq!(session_page_versions(&conn, sid, dst_ws, dst_proj), (1, 1));
    assert_eq!(summary_page_id(&conn, sid), None);
}

/// Point `sessions.summary_page_id` at the session's latest page in a
/// scope, the way session consolidation does.
fn point_summary_at_latest(
    conn: &mut Connection,
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
) {
    let path = format!("sessions/{sid}.md");
    conn.execute(
        "UPDATE sessions SET summary_page_id = ( \
                 SELECT id FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1) \
             WHERE id = ?4",
        params![ws.as_bytes(), proj.as_bytes(), path, sid.as_bytes()],
    )
    .unwrap();
    assert!(summary_page_id(conn, sid).is_some());
}

fn summary_page_id(conn: &Connection, sid: SessionId) -> Option<Vec<u8>> {
    conn.query_row(
        "SELECT summary_page_id FROM sessions WHERE id = ?1",
        params![sid.as_bytes()],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn move_session_without_page_reports_no_page_path() {
    let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let dst_proj = get_or_create_project(&mut conn, &dst_ws, "target", None).unwrap();
    let sid = seed_movable_session(&mut conn, src_ws, src_proj);
    conn.execute(
        "DELETE FROM pages WHERE workspace_id = ?1 AND project_id = ?2",
        params![src_ws.as_bytes(), src_proj.as_bytes()],
    )
    .unwrap();

    let summary = move_session(
        &mut conn,
        sid,
        dst_ws,
        dst_proj,
        PagesMode::Move,
        None,
        true,
    )
    .unwrap();
    assert!(summary.session_moved);
    assert_eq!(summary.page_versions_moved, 0);
    assert_eq!(summary.pages_regenerated, 0);
    assert_eq!(summary.page_path, None);
    assert_eq!(summary.page_versions_in_target, 0);
}

#[test]
fn move_session_unknown_session_is_not_found() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let err = move_session(
        &mut conn,
        SessionId::new(),
        ws,
        proj,
        PagesMode::Move,
        None,
        true,
    )
    .expect_err("unknown session must not succeed");
    assert!(
        matches!(err, StoreError::NotFound(_)),
        "unexpected error: {err:?}"
    );
}

#[test]
fn move_session_unknown_target_project_is_not_found() {
    let (_tmp, mut conn, src_ws, src_proj) = fresh_db();
    let dst_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let sid = seed_movable_session(&mut conn, src_ws, src_proj);

    // A project id that exists, but not in the requested workspace: the
    // pairing triggers only guard INSERT, so the op must refuse it itself.
    let err = move_session(
        &mut conn,
        sid,
        dst_ws,
        src_proj,
        PagesMode::Move,
        None,
        true,
    )
    .expect_err("target project outside the target workspace must be refused");
    assert!(
        matches!(err, StoreError::NotFound(_)),
        "unexpected error: {err:?}"
    );
    assert_session_bundle_in_scope(&conn, sid, (src_ws, src_proj), (dst_ws, src_proj));
}

/// Same scope, nothing stray: a re-home with every count at zero, not
/// an error (the audit row is still written).
#[test]
fn move_session_same_scope_with_nothing_stray_reports_zero_counts() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let sid = seed_movable_session(&mut conn, ws, proj);

    let summary = move_session(&mut conn, sid, ws, proj, PagesMode::Move, None, true).unwrap();
    assert!(!summary.session_moved);
    assert_eq!(summary.observations, 0);
    assert_eq!(summary.handoffs, 0);
    assert_eq!(summary.consolidation_jobs, 0);
    assert_eq!(summary.auto_improve_runs, 0);
    assert_eq!(summary.auto_improve_claims, 0);
    assert_eq!(summary.page_versions_moved, 0);
    assert_eq!(summary.pages_regenerated, 0);
    assert_eq!(summary.page_path, None);
    assert_eq!(summary.from_workspace, ws);
    assert_eq!(summary.to_project, proj);
    assert_eq!(summary.cwd.as_deref(), Some("/repo/src"));
    assert_session_bundle_in_scope(&conn, sid, (ws, proj), (WorkspaceId::new(), proj));
    assert_eq!(session_page_versions(&conn, sid, ws, proj), (2, 1));
    let audited: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE op = 'move_session'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 1);
}

/// Insert `n` observations of `sid` stamped into `(ws, proj)` directly,
/// the way mid-session routing before the sticky mode scattered events
/// into phantom projects.
fn scatter_observations(
    conn: &mut Connection,
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
    n: usize,
) {
    use sessionmunch_core::ObservationKind;
    for i in 0..n {
        insert_observation(
            conn,
            &NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::PostToolUse,
                extension: None,
                source_event: None,
                title: format!("stray-obs-{i}"),
                body: "zebra token body".into(),
                importance: 5,
            },
        )
        .unwrap();
    }
}

/// A move gathers dependent rows by session id alone, so it drains every
/// scope they landed in — including projects the caller never named. That
/// is deliberate, but it is not cleanly reversible, so the dry run has to
/// say which scopes it is about to empty before anything is committed.
#[test]
fn move_session_dry_run_names_every_scope_it_will_drain() {
    let (_tmp, mut conn, ws, target) = fresh_db();
    let unnamed_a = get_or_create_project(&mut conn, &ws, "scratchpad", None).unwrap();
    let unnamed_b = get_or_create_project(&mut conn, &ws, "tmp", None).unwrap();
    let sid = seed_movable_session(&mut conn, ws, target);
    scatter_observations(&mut conn, sid, ws, unnamed_a, 3);
    scatter_observations(&mut conn, sid, ws, unnamed_b, 1);

    // Dry run: nothing is written, but the report must already name both
    // scopes it would empty.
    let summary = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, false).unwrap();

    let named: Vec<(String, u64)> = summary
        .source_scopes
        .iter()
        .map(|s| (s.project_name.clone(), s.observations))
        .collect();
    assert_eq!(
        named,
        vec![("scratchpad".to_string(), 3), ("tmp".to_string(), 1)],
        "both drained scopes must be named, largest first"
    );
    assert!(
        summary.source_scopes.iter().all(|s| s.workspace_id == ws),
        "each entry carries its resolved scope ids"
    );
    // The dry run really was a dry run.
    assert_eq!(
        session_rows_in_scope(&conn, "observations", "session_id", sid, ws, unnamed_a),
        3
    );

    // The destination itself is never listed as a scope to drain.
    let confirmed = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true).unwrap();
    assert_eq!(confirmed.observations, 4);
    let after = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, false).unwrap();
    assert!(
        after.source_scopes.is_empty(),
        "with everything in the destination there is nothing left to drain"
    );
}

/// The phantom-bucket case: the session row already sits in the target,
/// some of its observations landed in another scope. Moving the session
/// to its own scope re-homes those rows and reports the real counts.
#[test]
fn move_session_rehomes_stray_rows_when_session_row_already_in_target() {
    let (_tmp, mut conn, ws, target) = fresh_db();
    let stray = get_or_create_project(&mut conn, &ws, "tmp", None).unwrap();
    // Session (row, 2 observations, handoff, job, run, claim, page) in T.
    let sid = seed_movable_session(&mut conn, ws, target);
    // 3 observations of the same session stamped into S.
    scatter_observations(&mut conn, sid, ws, stray, 3);
    assert_eq!(
        session_rows_in_scope(&conn, "observations", "session_id", sid, ws, stray),
        3
    );

    // Dry run: same counts, nothing changes.
    let summary = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, false).unwrap();
    assert!(!summary.session_moved);
    assert_eq!(summary.observations, 3);
    assert_eq!(
        session_rows_in_scope(&conn, "observations", "session_id", sid, ws, stray),
        3
    );

    let summary = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true).unwrap();
    assert!(!summary.session_moved, "the row was already in the target");
    assert_eq!(summary.observations, 3, "only the stray rows are counted");
    assert_eq!(summary.handoffs, 0);
    assert_eq!(summary.consolidation_jobs, 0);
    assert_eq!(summary.auto_improve_runs, 0);
    assert_eq!(summary.auto_improve_claims, 0);
    assert_eq!(summary.page_versions_moved, 0);
    assert_eq!(summary.pages_regenerated, 0);
    assert_eq!(
        summary.page_path, None,
        "the page already sits in the target"
    );
    assert_eq!(summary.page_versions_in_target, 2);
    assert_eq!(summary.from_project, target);
    assert_eq!(summary.to_project, target);

    assert_eq!(
        session_rows_in_scope(&conn, "observations", "session_id", sid, ws, target),
        5
    );
    assert_eq!(
        session_rows_in_scope(&conn, "observations", "session_id", sid, ws, stray),
        0
    );
    assert_eq!(fts_hits_in_scope(&conn, sid, ws, target), 5);
    assert_eq!(fts_hits_in_scope(&conn, sid, ws, stray), 0);
    assert_eq!(
        session_rows_in_scope(&conn, "sessions", "id", sid, ws, target),
        1
    );
    assert_eq!(session_page_versions(&conn, sid, ws, target), (2, 1));
}

/// A re-home also gathers the session page when its rows lie outside the
/// target: moved under `Move` (collision rule against the target),
/// retired under `Regenerate`.
#[test]
fn move_session_rehome_handles_page_rows_outside_target() {
    let (_tmp, mut conn, ws, target) = fresh_db();
    let stray = get_or_create_project(&mut conn, &ws, "tmp", None).unwrap();
    let sid = seed_movable_session(&mut conn, ws, target);
    let path = format!("sessions/{sid}.md");
    // Park the page in the stray scope: nothing at the path in T.
    conn.execute(
        "UPDATE pages SET workspace_id = ?1, project_id = ?2 WHERE path = ?3",
        params![ws.as_bytes(), stray.as_bytes(), path],
    )
    .unwrap();
    assert_eq!(session_page_versions(&conn, sid, ws, target), (0, 0));
    assert_eq!(session_page_versions(&conn, sid, ws, stray), (2, 1));

    let summary = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true).unwrap();
    assert!(!summary.session_moved);
    assert_eq!(summary.observations, 0);
    assert_eq!(summary.page_versions_moved, 2);
    assert_eq!(summary.page_path.as_deref(), Some(path.as_str()));
    assert_eq!(session_page_versions(&conn, sid, ws, target), (2, 1));
    assert_eq!(session_page_versions(&conn, sid, ws, stray), (0, 0));

    // Stray latest AND a latest in the target: Move collides, Regenerate
    // retires the stray one and leaves the target's page alone.
    upsert_page(&mut conn, &page(ws, stray, &path, "stray again")).unwrap();
    let err = move_session(&mut conn, sid, ws, target, PagesMode::Move, None, true)
        .expect_err("a latest page at the path in the target must block the page move");
    assert!(
        matches!(&err, StoreError::PagePathTaken { path: p } if *p == path),
        "unexpected error: {err:?}"
    );
    assert_eq!(session_page_versions(&conn, sid, ws, stray), (1, 1));
    let summary = move_session(
        &mut conn,
        sid,
        ws,
        target,
        PagesMode::Regenerate,
        None,
        true,
    )
    .unwrap();
    assert_eq!(summary.pages_regenerated, 1);
    assert_eq!(summary.page_versions_moved, 0);
    assert_eq!(session_page_versions(&conn, sid, ws, stray), (1, 0));
    assert_eq!(session_page_versions(&conn, sid, ws, target), (2, 1));
}

#[test]
fn ensure_project_workspace_rejects_stale_pair_before_disk_write() {
    let (_tmp, conn, ws, proj) = fresh_db();
    let other_ws = WorkspaceId::new();

    ensure_project_workspace(&conn, &ws, &proj).unwrap();
    assert!(
        matches!(
            ensure_project_workspace(&conn, &other_ws, &proj),
            Err(StoreError::NotFound(_))
        ),
        "a stale workspace/project pair must fail before wiki writes touch disk"
    );
}

#[test]
fn ensure_project_workspace_separates_a_moved_project_from_a_missing_one() {
    let (_tmp, conn, ws, proj) = fresh_db();
    let other_ws = WorkspaceId::new();
    let ghost = ProjectId::new();

    // The project exists, but the caller carries a stale workspace: the
    // message has to name where it actually lives, or the reader cannot
    // tell a stale cache from a corrupt one.
    let moved = ensure_project_workspace(&conn, &other_ws, &proj).unwrap_err();
    let moved = moved.to_string();
    assert!(
        moved.contains(&ws.to_string()) && moved.contains(&other_ws.to_string()),
        "a moved project must name both the real and the supplied workspace, got: {moved}"
    );

    // No row at all. Saying this "does not belong to workspace X" sends
    // the reader hunting a scoping bug that is not there — the id is
    // simply dangling.
    let missing = ensure_project_workspace(&conn, &ws, &ghost)
        .unwrap_err()
        .to_string();
    assert!(
        missing.contains("does not exist"),
        "a dangling project id must be reported as missing, got: {missing}"
    );
    assert!(
        !missing.contains("does not belong to workspace"),
        "a dangling project id must not be described as a workspace mismatch, got: {missing}"
    );
}

#[test]
fn ensure_workspace_with_id_rejects_id_name_mismatch() {
    let (_tmp, mut conn, _ws, _proj) = fresh_db();
    let id = WorkspaceId::new();

    ensure_workspace_with_id(&mut conn, id, "from-manifest").unwrap();
    let err = ensure_workspace_with_id(&mut conn, id, "other-name").unwrap_err();

    assert!(
        matches!(err, StoreError::Duplicate(_)),
        "same workspace id with different name must fail loudly; got {err:?}"
    );
}

#[test]
fn ensure_project_with_id_rejects_existing_id_mismatch() {
    let (_tmp, mut conn, ws, _proj) = fresh_db();
    let id = ProjectId::new();

    ensure_project_with_id(&mut conn, id, ws, "from-manifest", Some("/repo/a")).unwrap();
    let err = ensure_project_with_id(&mut conn, id, ws, "renamed", Some("/repo/a")).unwrap_err();

    assert!(
        matches!(err, StoreError::Duplicate(_)),
        "same project id with different manifest data must fail loudly; got {err:?}"
    );
}

/// V19 data-repair migration: observations whose `project_id`
/// disagrees with their session's `project_id` are re-attributed
/// to the session's project. Handoffs that carry a session id are
/// repaired the same way. Project rows that become truly empty
/// after repair are deleted. The migration is idempotent: re-run
/// on a repaired DB updates / deletes nothing.
#[test]
fn v19_repairs_orphan_observation_attribution_and_purges_empty_projects() {
    use sessionmunch_core::{NewObservation, ObservationKind, SessionId};

    // Apply migrations through V18 (not V19) so we can seed the
    // orphaned-attribution state V19 is designed to repair. If we
    // ran the full chain via `fresh_db`, V19 would already be in
    // the refinery history and re-invoking `migrations::run` below
    // would be a no-op.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    crate::migrations::run_to(&mut conn, 18).unwrap();

    // Seed the bug shape with V18-and-earlier semantics: parent
    // project `manga-plus` and fragment project `reader` co-exist
    // in the same workspace; a session lives under `manga-plus`
    // and an observation was misattributed to `reader`.
    let ws = get_or_create_workspace(&mut conn, "default").unwrap();
    let parent = get_or_create_project(
        &mut conn,
        &ws,
        "manga-plus",
        Some("/mnt/data/Projects/manga-plus"),
    )
    .unwrap();
    let fragment = get_or_create_project(
        &mut conn,
        &ws,
        "reader",
        Some("/mnt/data/Projects/manga-plus/reader"),
    )
    .unwrap();

    let sid = SessionId::new();
    // Era-appropriate raw insert: this fixture stops at an older
    // schema on purpose (see `seed_historical_session`).
    seed_historical_session(
        &conn,
        &sid,
        &ws,
        &parent,
        sessionmunch_core::AgentKind::ClaudeCode,
        Some("/mnt/data/Projects/manga-plus"),
    );

    // Three misattributed observations on the fragment.
    for i in 0..3 {
        insert_observation_unmirrored(
            &mut conn,
            &NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: fragment,
                kind: ObservationKind::PreToolUse,
                extension: None,
                source_event: None,
                title: format!("call {i}"),
                body: "body".into(),
                importance: 5,
            },
        )
        .unwrap();
    }

    // Run the repair migration (V19). Target V19 explicitly rather than
    // the open-ended `run`: this test seeds rows first (leaving cached
    // statements on `sessions`), so letting a later table-rebuild
    // migration (V20+) run here would trip SQLITE_LOCKED on its
    // `DROP TABLE sessions`. Production runs migrations before any query,
    // so the rebuild is unaffected there.
    crate::migrations::run_to(&mut conn, 19).unwrap();

    // All observations now point at the parent.
    let cnt_parent: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
            params![&parent.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt_parent, 3, "observations re-attributed to parent");

    // The fragment row is gone — it's truly empty post-repair.
    let frag_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE id = ?1",
            params![&fragment.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(frag_rows, 0, "fragment project row deleted");

    // Parent survives; it owns its rows.
    let parent_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE id = ?1",
            params![&parent.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(parent_rows, 1);
}

// Hollow-project sweep: deletes only rows with zero data of any kind
// and past the age cutoff; reserved names survive even when hollow.
#[test]
fn sweep_hollow_projects_deletes_only_old_dataless_rows() {
    use sessionmunch_core::{AgentKind, NewSession, SessionId};
    let (_tmp, mut conn, ws, _scratch) = fresh_db();

    // Hollow + old: delete. (Backdate created_at 8 days.)
    let hollow = get_or_create_project(&mut conn, &ws, "zt", None).unwrap();
    // Hollow + fresh: keep (inside the grace window).
    let fresh = get_or_create_project(&mut conn, &ws, "new-probe", None).unwrap();
    // Old but has a session: keep (not hollow).
    let with_data = get_or_create_project(&mut conn, &ws, "one-off", None).unwrap();
    // Old but has a managed workstream: keep (not hollow).
    let with_workstream = get_or_create_project(&mut conn, &ws, "managed-only", None).unwrap();
    // Reserved + hollow + old: keep.
    let global = get_or_create_project(
        &mut conn,
        &ws,
        sessionmunch_core::GLOBAL_SCOPE_PROJECT,
        None,
    )
    .unwrap();

    let eight_days_us: i64 = 8 * 24 * 60 * 60 * 1_000_000;
    for id in [&hollow, &with_data, &with_workstream, &global] {
        conn.execute(
            "UPDATE projects SET created_at = created_at - ?1 WHERE id = ?2",
            params![eight_days_us, &id.as_bytes()[..]],
        )
        .unwrap();
    }
    begin_session(
        &mut conn,
        &NewSession {
            id: SessionId::new(),
            workspace_id: ws,
            project_id: with_data,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        },
    )
    .unwrap();
    let managed = open_managed_run(&mut conn, &ws, &with_workstream);

    let deleted = sweep_hollow_projects(&mut conn, 7).unwrap();
    assert_eq!(deleted, vec!["zt".to_string()]);

    let exists = |id: &sessionmunch_core::ProjectId| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM projects WHERE id = ?1",
            params![&id.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(exists(&hollow), 0, "old hollow row deleted");
    assert_eq!(exists(&fresh), 1, "fresh hollow row kept (grace window)");
    assert_eq!(exists(&with_data), 1, "data-bearing row kept");
    assert_eq!(
        exists(&with_workstream),
        1,
        "managed-workstream-bearing row kept"
    );
    assert_eq!(exists(&global), 1, "reserved _global kept even when hollow");
    let workstream_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM workstreams WHERE id = ?1",
            params![&managed.workstream_id.as_bytes()[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(workstream_rows, 1, "managed workstream kept");
    let run_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM managed_runs WHERE id = ?1",
            params![&managed.run_id.as_bytes()[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_rows, 1, "managed run kept");

    // Idempotent: a second pass deletes nothing.
    assert!(sweep_hollow_projects(&mut conn, 7).unwrap().is_empty());
}

/// Insert a `sessions` row using only the columns that have existed since
/// V01, for fixtures that deliberately stop at an older schema version.
/// The production `begin_session` writes whatever columns the CURRENT
/// schema has — including V40's `actor_user`, which these fixtures'
/// `sessions` tables do not have yet.
fn seed_historical_session(
    conn: &Connection,
    id: &sessionmunch_core::SessionId,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    agent_kind: sessionmunch_core::AgentKind,
    cwd: Option<&str>,
) {
    conn.execute(
        "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            agent_kind.as_str(),
            cwd,
            Timestamp::now().as_microsecond(),
        ],
    )
    .unwrap();
}

/// V27 re-runs the V19 repair for the fragments that accumulated
/// after V19: non-git parents have no repo_path, so the v0.12.2
/// prefix guard couldn't anchor subdirectory cwds and per-event
/// basename derivation kept minting fragment projects. Idempotent:
/// a second pass repairs nothing.
#[test]
fn v27_reattributes_nongit_fragments_and_preserves_reserved_projects() {
    use sessionmunch_core::{NewObservation, ObservationKind, SessionId};

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    crate::migrations::run_to(&mut conn, 26).unwrap();

    // Non-git parent (repo_path NULL — the production shape) plus a
    // basename fragment holding misattributed observations, and the
    // reserved projects that must survive even when empty.
    let ws = get_or_create_workspace(&mut conn, "default").unwrap();
    let parent = get_or_create_project(&mut conn, &ws, "tiktok_analysis", None).unwrap();
    let fragment = get_or_create_project(&mut conn, &ws, "sources", None).unwrap();
    let scratch = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
    let global = get_or_create_project(&mut conn, &ws, "_global", None).unwrap();

    let sid = SessionId::new();
    // Seeded with era-appropriate SQL rather than `begin_session`: this
    // fixture stops at an older schema on purpose, and the production
    // helper writes whatever columns the CURRENT schema has.
    seed_historical_session(
        &conn,
        &sid,
        &ws,
        &parent,
        sessionmunch_core::AgentKind::ClaudeCode,
        Some("/home/user/tiktok_analysis"),
    );
    for i in 0..4 {
        insert_observation_unmirrored(
            &mut conn,
            &NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: fragment,
                kind: ObservationKind::PostToolUse,
                extension: None,
                source_event: None,
                title: format!("call {i}"),
                body: "body".into(),
                importance: 5,
            },
        )
        .unwrap();
    }

    crate::migrations::run_to(&mut conn, 27).unwrap();

    let count = |sql: &str, id: &sessionmunch_core::ProjectId| -> i64 {
        conn.query_row(sql, params![&id.as_bytes()[..]], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
            &parent
        ),
        4,
        "observations re-attributed to the non-git parent"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM projects WHERE id = ?1", &fragment),
        0,
        "emptied fragment row deleted"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM projects WHERE id = ?1", &scratch),
        1,
        "scratch is reserved and survives empty"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM projects WHERE id = ?1", &global),
        1,
        "_global is reserved and survives empty"
    );

    // Idempotency: replay V27's statements directly (refinery won't
    // re-run an applied version); zero rows change.
    let sql = include_str!("../../migrations/V27__repair_nongit_fragment_attribution.sql");
    conn.execute_batch(sql).unwrap();
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
            &parent
        ),
        4
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM projects WHERE id = ?1", &scratch),
        1
    );
}

#[test]
fn v20_adds_grok_and_preserves_sessions_invariants_on_upgraded_db() {
    use sessionmunch_core::{NewObservation, ObservationKind, SessionId};

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let ws;
    let proj;
    let existing_sid = SessionId::new();

    {
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 19).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        ws = get_or_create_workspace(&mut conn, "default").unwrap();
        proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        // Era-appropriate raw insert: this fixture stops at an older
        // schema on purpose (see `seed_historical_session`).
        seed_historical_session(
            &conn,
            &existing_sid,
            &ws,
            &proj,
            sessionmunch_core::AgentKind::ClaudeCode,
            None,
        );
        insert_observation_unmirrored(
            &mut conn,
            &NewObservation {
                session_id: existing_sid,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "before v20".into(),
                body: "existing observation survives table rebuild".into(),
                importance: 5,
            },
        )
        .unwrap();
    }

    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    crate::migrations::run_to(&mut conn, 20).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    // Era-appropriate raw insert (see `seed_historical_session`):
    // this fixture asserts the migration's widened CHECK accepts the
    // newly added agent kind on an upgraded database.
    seed_historical_session(
        &conn,
        &SessionId::new(),
        &ws,
        &proj,
        sessionmunch_core::AgentKind::Grok,
        None,
    );

    let obs_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
            params![existing_sid.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(obs_count, 1, "V20 must preserve existing observations");

    let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at')",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(index_count, 3, "V20 must recreate sessions indexes");

    let trigger_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        trigger_count, 1,
        "V20 must recreate the V18 pairing trigger"
    );

    let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let other_proj = get_or_create_project(&mut conn, &other_ws, "other-project", None).unwrap();
    // Raw insert on purpose: the fixture sits on an older schema, and
    // what is under test is the pairing TRIGGER rejecting a session
    // whose workspace does not own its project.
    let err = conn
        .execute(
            "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, cwd, started_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![
                SessionId::new().as_bytes(),
                ws.as_bytes(),
                other_proj.as_bytes(),
                sessionmunch_core::AgentKind::Grok.as_str(),
                Timestamp::now().as_microsecond(),
            ],
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("sessions.workspace_id does not match"),
        "pairing trigger must reject split-brain sessions after V20: {err}"
    );

    let fk_violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_violations, 0, "V20 must leave foreign keys clean");
}

#[test]
fn v25_adds_pi_and_preserves_sessions_invariants_on_upgraded_db() {
    use sessionmunch_core::SessionId;

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let ws;
    let proj;
    let existing_sid = SessionId::new();

    {
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 24).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        ws = get_or_create_workspace(&mut conn, "default").unwrap();
        proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        // Era-appropriate raw insert: this fixture stops at an older
        // schema on purpose (see `seed_historical_session`).
        seed_historical_session(
            &conn,
            &existing_sid,
            &ws,
            &proj,
            sessionmunch_core::AgentKind::ClaudeCode,
            None,
        );
    }

    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    crate::migrations::run_to(&mut conn, 25).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    // Era-appropriate raw insert (see `seed_historical_session`):
    // this fixture asserts the migration's widened CHECK accepts the
    // newly added agent kind on an upgraded database.
    seed_historical_session(
        &conn,
        &SessionId::new(),
        &ws,
        &proj,
        sessionmunch_core::AgentKind::Pi,
        None,
    );

    let session_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(session_count, 2, "V25 must preserve existing sessions");

    let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at', 'idx_sessions_scope_ended')",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(index_count, 4, "V25 must recreate sessions indexes");

    let trigger_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        trigger_count, 1,
        "V25 must recreate the V18 pairing trigger"
    );

    let scheduler_trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'auto_improve_scheduler_claims_session_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        scheduler_trigger_count, 1,
        "V25 must recreate the V22 scheduler/session pairing trigger"
    );

    let fk_violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_violations, 0, "V25 must leave foreign keys clean");
}

#[test]
fn v28_adds_devin_and_preserves_sessions_invariants_on_upgraded_db() {
    use sessionmunch_core::SessionId;

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let ws;
    let proj;
    let existing_sid = SessionId::new();

    {
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 25).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        ws = get_or_create_workspace(&mut conn, "default").unwrap();
        proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        // Era-appropriate raw insert: this fixture stops at an older
        // schema on purpose (see `seed_historical_session`).
        seed_historical_session(
            &conn,
            &existing_sid,
            &ws,
            &proj,
            sessionmunch_core::AgentKind::ClaudeCode,
            None,
        );
    }

    // Run through V26 (Zero) and V27 (unrelated data repair) too, not
    // just straight to V28: this proves the whole upgrade chain composes
    // on a pre-V26 database, not just V28 applied in isolation.
    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    crate::migrations::run_to(&mut conn, 28).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    // Era-appropriate raw insert (see `seed_historical_session`):
    // this fixture asserts the migration's widened CHECK accepts the
    // newly added agent kind on an upgraded database.
    seed_historical_session(
        &conn,
        &SessionId::new(),
        &ws,
        &proj,
        sessionmunch_core::AgentKind::Devin,
        None,
    );

    let session_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(session_count, 2, "V28 must preserve existing sessions");

    let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at', 'idx_sessions_scope_ended')",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(index_count, 4, "V28 must recreate sessions indexes");

    let trigger_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        trigger_count, 1,
        "V28 must recreate the V18 pairing trigger"
    );

    let scheduler_trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'auto_improve_scheduler_claims_session_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        scheduler_trigger_count, 1,
        "V28 must recreate the V22 scheduler/session pairing trigger"
    );

    let fk_violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_violations, 0, "V28 must leave foreign keys clean");

    let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();
    let other_proj = get_or_create_project(&mut conn, &other_ws, "other-project", None).unwrap();
    // Raw insert on purpose: the fixture sits on an older schema, and
    // what is under test is the pairing TRIGGER rejecting a session
    // whose workspace does not own its project.
    let err = conn
        .execute(
            "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, cwd, started_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![
                SessionId::new().as_bytes(),
                ws.as_bytes(),
                other_proj.as_bytes(),
                sessionmunch_core::AgentKind::Devin.as_str(),
                Timestamp::now().as_microsecond(),
            ],
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("sessions.workspace_id does not match"),
        "pairing trigger must reject split-brain sessions after V28: {err}"
    );

    let fk_violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        fk_violations, 0,
        "V28 must leave foreign keys clean after pairing test"
    );
}

/// V19 is idempotent: re-running on a repaired DB is a no-op.
/// Also asserts the initial run on a clean DB (no orphans, no
/// empty fragments) is a no-op.
#[test]
fn v19_is_idempotent() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    // fresh_db already ran the full chain (including V19). Seed a
    // few valid rows to ensure they survive a re-run.
    upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
    let before: (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM projects), \
                        (SELECT COUNT(*) FROM observations), \
                        (SELECT COUNT(*) FROM pages)",
            params![],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    crate::migrations::run(&mut conn).unwrap();
    let after: (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM projects), \
                        (SELECT COUNT(*) FROM observations), \
                        (SELECT COUNT(*) FROM pages)",
            params![],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        before, after,
        "V19 must be a no-op on already-repaired data"
    );
}

/// `scratch` keeps its standalone handoffs even when it would
/// otherwise look empty. CLAUDE.md invariant #15a names it as the
/// defensive default for hook events that arrive without a usable
/// cwd; the V19 DELETE explicitly carves it out.
#[test]
fn v19_preserves_scratch_with_standalone_handoffs() {
    use sessionmunch_core::{AgentKind, NewHandoff};

    let (_tmp, mut conn, ws, _proj) = fresh_db();
    // Add a standalone handoff to scratch (no from_session_id).
    let scratch = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
    insert_handoff(
        &mut conn,
        &NewHandoff {
            workspace_id: ws,
            project_id: scratch,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "standalone".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        },
    )
    .unwrap();

    crate::migrations::run(&mut conn).unwrap();

    let scratch_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE name = 'scratch'",
            params![],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        scratch_rows, 1,
        "scratch must survive even if it looks empty"
    );
    let scratch_handoffs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM handoffs WHERE project_id = ?1",
            params![&scratch.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(scratch_handoffs, 1);
}

/// Seed a minimal `pages` row against a pre-V36 schema. Migration-era
/// tests run old schemas, where current `upsert_page` — which writes
/// the V36 `expires_at` column — cannot be used to seed fixtures.
pub(crate) fn insert_page_pre_v36(conn: &Connection, page: &NewPage) -> PageId {
    let id = PageId::new();
    let now = Timestamp::now().as_microsecond();
    let body_sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(page.body.as_bytes());
        hasher.finalize().into()
    };
    conn.execute(
        "INSERT INTO pages \
             (id, workspace_id, project_id, path, path_search, title, tier, body, \
              body_sha256, frontmatter_json, is_latest, pinned, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?12)",
        params![
            id.as_bytes(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
            page.path.as_str(),
            path_search_text(page.path.as_str()),
            page.title,
            page.tier.as_str(),
            page.body,
            body_sha256.as_slice(),
            serde_json::to_string(&page.frontmatter_json).unwrap(),
            i64::from(page.pinned),
            now,
        ],
    )
    .unwrap();
    id
}

#[test]
fn v18_migration_refuses_existing_split_brain_rows() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let mut conn = Connection::open(&db_path).unwrap();
    crate::migrations::run_to(&mut conn, 17).unwrap();

    let src_ws = get_or_create_workspace(&mut conn, "src").unwrap();
    let stale_ws = get_or_create_workspace(&mut conn, "stale").unwrap();
    let proj = get_or_create_project(&mut conn, &src_ws, "scratch", None).unwrap();
    let mut bad_page = page(src_ws, proj, "notes/split.md", "body");
    bad_page.workspace_id = stale_ws;
    insert_page_pre_v36(&conn, &bad_page);

    let err = crate::migrations::run_to(&mut conn, 18).unwrap_err();
    assert!(
        err.to_string().contains("CHECK constraint failed"),
        "V18 must abort instead of preserving split-brain rows: {err}"
    );
}

/// V18 integrity triggers: an INSERT whose `workspace_id` disagrees with
/// the project's actual workspace ABORTs (the split-brain a stale hook
/// cache would otherwise create), while the consistent pair inserts fine.
#[test]
fn insert_with_mismatched_workspace_is_rejected() {
    use sessionmunch_core::ObservationKind;

    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();

    // A page under the WRONG workspace (project lives in `ws`) is refused.
    let mut bad_page = page(ws, proj, "notes/a.md", "body");
    bad_page.workspace_id = other_ws;
    assert!(
        upsert_page(&mut conn, &bad_page).is_err(),
        "page insert with mismatched workspace must abort"
    );

    // The consistent pair inserts fine.
    upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

    // The session insert is guarded too: a mismatched pair aborts.
    let bad_sid = SessionId::new();
    assert!(
        begin_session(
            &mut conn,
            &NewSession {
                id: bad_sid,
                workspace_id: other_ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            },
        )
        .is_err(),
        "session insert with mismatched workspace must abort"
    );

    let sid = SessionId::new();
    begin_session(
        &mut conn,
        &NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        },
    )
    .unwrap();

    // The split-brain case the maintainer flagged: a hook writes an
    // observation with a stale workspace id for a moved project.
    let mismatched_obs = NewObservation {
        session_id: sid,
        workspace_id: other_ws,
        project_id: proj,
        kind: ObservationKind::UserPrompt,
        extension: None,
        source_event: None,
        title: "t".into(),
        body: "b".into(),
        importance: 5,
    };
    assert!(
        insert_observation(&mut conn, &mismatched_obs).is_err(),
        "observation insert with mismatched workspace must abort"
    );

    // Same observation under the correct workspace is accepted.
    let good_obs = NewObservation {
        workspace_id: ws,
        ..mismatched_obs
    };
    insert_observation(&mut conn, &good_obs).unwrap();

    // The handoff insert is the fourth INSERT trigger; audit flagged
    // that the original V18 test omitted it, so the only coverage
    // for the handoffs trigger was the temp-table CHECK on migration.
    // Assert the BEFORE INSERT trigger fires on a stale pair, and
    // a corrected pair lands cleanly.
    let mismatched_handoff = NewHandoff {
        workspace_id: other_ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: None,
        summary: "stale".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    assert!(
        insert_handoff(&mut conn, &mismatched_handoff).is_err(),
        "handoff insert with mismatched workspace must abort"
    );
    let good_handoff = NewHandoff {
        workspace_id: ws,
        ..mismatched_handoff
    };
    insert_handoff(&mut conn, &good_handoff).unwrap();
}

/// Regression for the rename-vs-purge race that live exploration
/// caught: a `rename_project` for a row that was deleted between
/// the admin handler's `lookup_ws_proj_no_create` and the
/// `UPDATE projects` used to silently return `Ok(())` — the admin
/// endpoint then responded `200 OK` for an operation that touched
/// zero rows, contradicting the concurrent purge's (also `200 OK`)
/// destruction of the same project. After the fix, the writer
/// returns `StoreError::NotFound`, which the admin handler maps to
/// `404 Not Found`. Pins both the writer-side semantic and a
/// concrete recipe for the failure shape so a future refactor
/// can't quietly downgrade the error back to a silent Ok.
#[test]
fn rename_project_after_purge_returns_not_found() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    // Simulate the post-purge state: project row gone.
    // `purge_project` drives the cascading deletes we want here.
    let _ = purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        false,
        Compaction::Skip,
    )
    .expect("purge of fresh project should succeed");
    // Now try to rename the project that no longer exists. The
    // pre-fix code returned `Ok(())` because `UPDATE` affected
    // zero rows. The fix returns `NotFound` so admin handlers
    // can respond 404 honestly.
    let err = rename_project(&mut conn, &ws, &proj, "renamed", None)
        .expect_err("rename of purged project must error");
    match err {
        StoreError::NotFound(_) => {}
        other => panic!("expected StoreError::NotFound, got {other:?}"),
    }
}

/// Belt-and-suspenders for the common path: rename of an existing
/// project still succeeds. Without this, a future "always return
/// NotFound" regression would also pass the test above by accident.
#[test]
fn rename_project_of_live_project_succeeds() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    rename_project(&mut conn, &ws, &proj, "renamed-live", None)
        .expect("rename of live project must succeed");
}

fn seed_user(conn: &Connection, username: &str) -> sessionmunch_core::UserId {
    crate::users::insert_human_user(
        conn,
        &sessionmunch_core::NewUser {
            username: username.to_string(),
            name: None,
            email: None,
        },
        sessionmunch_core::UserRole::User,
        None,
        false,
    )
    .expect("seed user")
}

fn audit_row_for(conn: &Connection, op: &str) -> (i64, Option<Vec<u8>>) {
    conn.query_row(
        "SELECT COUNT(*), MAX(author_id) FROM audit_log WHERE op = ?1",
        [op],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// Attribution guard: a `purge_project` leaves exactly one `audit_log`
/// row tagged with the op + the authenticated operator — the
/// point-in-time answer to "who wiped this project?" (V16's motivating
/// case). Without the audit call the count would be 0.
#[test]
fn audit_log_records_purge_project_with_author() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let author = seed_user(&conn, "alice");

    purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        Some(author),
        false,
        Compaction::Skip,
    )
    .expect("purge should succeed");

    let (count, op_author) = audit_row_for(&conn, "purge_project");
    assert_eq!(count, 1, "exactly one purge_project audit row");
    assert_eq!(
        op_author.as_deref(),
        Some(&author.as_bytes()[..]),
        "audit row must carry the purging operator"
    );
}

/// Open one managed run on `proj` so the purge guard has something to
/// refuse. Mirrors what `sessionmunch run` does on launch.
fn open_managed_run(
    conn: &mut Connection,
    ws: &sessionmunch_core::WorkspaceId,
    proj: &sessionmunch_core::ProjectId,
) -> crate::workstream::PreparedWorkstreamRun {
    crate::workstream::prepare_run(
        conn,
        &crate::workstream::PrepareWorkstreamRun {
            workspace_id: *ws,
            project_id: *proj,
            repo_fingerprint: "repo-fp".to_string(),
            worktree_fingerprint: "worktree-fp".to_string(),
            cwd: "/tmp/checkout".to_string(),
            agent: sessionmunch_core::AgentKind::ClaudeCode,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: crate::workstream::WorkstreamSelection::Current,
            lease_owner: "test".to_string(),
        },
    )
    .expect("opening a managed run should succeed")
}

/// `workstreams` cascades out of `projects` and `managed_runs` cascades
/// out of `workstreams`, so purging a project silently tore the lease row
/// out from under a live agent: its heartbeat then failed with
/// `409 managed run lease is not active` every 30s and its transcript
/// never reached the ledger. The purge must refuse instead.
#[test]
fn purge_project_refuses_while_a_managed_run_is_active() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    open_managed_run(&mut conn, &ws, &proj);

    let err = purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        false,
        Compaction::Skip,
    )
    .expect_err("an active managed run must block the purge");

    match err {
        StoreError::ManagedRunActive { count, workstreams } => {
            assert_eq!(count, 1, "one active run");
            assert!(
                workstreams.contains("default"),
                "the error names the workstream so the operator can find the session: {workstreams}"
            );
        }
        other => panic!("expected ManagedRunActive, got {other:?}"),
    }

    let survived: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE id = ?1",
            rusqlite::params![&proj.as_bytes()[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(survived, 1, "a refused purge must not delete anything");
}

/// `force` is the deliberate override, and the summary must account for
/// what the cascade took with it — the counters the pre-fix report showed
/// (`0 pages, 0 sessions, …`) made a scope carrying a live workstream look
/// safe to delete.
#[test]
fn forced_purge_reports_the_workstreams_it_cascaded() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let prepared = open_managed_run(&mut conn, &ws, &proj);

    let summary = purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        true,
        Compaction::Skip,
    )
    .expect("force purges regardless of the live lease");

    assert_eq!(summary.workstreams_deleted, 1);
    assert_eq!(summary.managed_runs_deleted, 1);
    assert_eq!(
        summary.workstream_ids,
        vec![prepared.workstream_id.to_string()],
        "the raw/workstreams/<id>/ dir is reported for post-commit cleanup"
    );
}

/// A crashed wrapper leaves `state = 'active'` behind forever — the only
/// sweep that flips it to `'expired'` lives in `prepare_run`. Guarding on
/// the state alone would let one dead session block every future purge of
/// the project, so the guard must read the lease expiry too.
#[test]
fn purge_project_ignores_a_managed_run_whose_lease_expired() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    open_managed_run(&mut conn, &ws, &proj);
    // Simulate the crash: the row still says 'active', but nothing has
    // heartbeated it, so its lease lapsed.
    conn.execute(
        "UPDATE managed_runs SET lease_expires_at = ?1 WHERE state = 'active'",
        rusqlite::params![Timestamp::now().as_microsecond() - 1],
    )
    .unwrap();

    let summary = purge_project(
        &mut conn,
        &ws,
        &proj,
        "default/scratch",
        None,
        false,
        Compaction::Skip,
    )
    .expect("a lapsed lease is not a running agent");

    assert_eq!(summary.workstreams_deleted, 1);
    assert_eq!(summary.managed_runs_deleted, 1);
}

/// A `rename_project` writes an attributed audit row, committed
/// atomically with the rename.
#[test]
fn audit_log_records_rename_project_with_author() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let author = seed_user(&conn, "bob");

    rename_project(&mut conn, &ws, &proj, "renamed", Some(author)).expect("rename should succeed");

    let (count, op_author) = audit_row_for(&conn, "rename_project");
    assert_eq!(count, 1, "exactly one rename_project audit row");
    assert_eq!(op_author.as_deref(), Some(&author.as_bytes()[..]));
}

/// A failed rename (name collision) writes NO audit row — the tx that
/// wraps the UPDATE + audit rolls back as a unit.
#[test]
fn audit_log_omits_rename_project_on_collision() {
    let (_tmp, mut conn, ws, _proj) = fresh_db();
    let author = seed_user(&conn, "carol");
    // Second project whose name we'll collide with.
    let other = get_or_create_project(&mut conn, &ws, "taken", None).unwrap();

    let err = rename_project(&mut conn, &ws, &other, "scratch", Some(author))
        .expect_err("rename onto an existing name must fail");
    assert!(matches!(err, StoreError::ProjectNameTaken(_)));

    let (count, _) = audit_row_for(&conn, "rename_project");
    assert_eq!(count, 0, "a rolled-back rename must leave no audit row");
}

/// Deleting an existing page writes ONE attributed `delete_page` audit row
/// pointing at the deleted page id — the "who deleted the gotcha page?"
/// case V16 names.
#[test]
fn audit_log_records_delete_page_with_author_and_page_id() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let author = seed_user(&conn, "dave");
    let pid = upsert_page(&mut conn, &page(ws, proj, "notes/doomed.md", "body")).unwrap();

    delete_page(
        &mut conn,
        ws,
        proj,
        &PagePath::new("notes/doomed.md").unwrap(),
        Some(author),
    )
    .unwrap();

    let (count, op_author) = audit_row_for(&conn, "delete_page");
    assert_eq!(count, 1, "exactly one delete_page audit row");
    assert_eq!(op_author.as_deref(), Some(&author.as_bytes()[..]));
    let page_id: Option<Vec<u8>> = conn
        .query_row(
            "SELECT page_id FROM audit_log WHERE op = 'delete_page'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        page_id.as_deref(),
        Some(&pid.as_bytes()[..]),
        "audit row must point at the deleted page id"
    );
}

/// A delete that matches no row (idempotent no-op) writes NO audit row.
#[test]
fn audit_log_omits_delete_page_noop() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    delete_page(
        &mut conn,
        ws,
        proj,
        &PagePath::new("notes/ghost.md").unwrap(),
        None,
    )
    .unwrap();
    let (count, _) = audit_row_for(&conn, "delete_page");
    assert_eq!(count, 0, "a no-op delete must leave no audit row");
}

/// Run the reader's exact FTS5 `MATCH` against the real, populated
/// `pages_fts` index — the path the web search / MCP query take.
/// Returns the matched paths (and surfaces any FTS5 syntax error as
/// an `Err`, the way the bug originally manifested).
fn fts_match_paths(conn: &Connection, raw: &str) -> rusqlite::Result<Vec<String>> {
    let fts_query = crate::fts_query::prepare_fts5_query(raw);
    let mut stmt = conn.prepare(
        "SELECT pages.path \
             FROM pages_fts \
             JOIN pages ON pages.rowid = pages_fts.rowid \
             WHERE pages_fts MATCH ?1 AND pages.is_latest = 1 \
             ORDER BY pages_fts.rank",
    )?;
    let rows = stmt.query_map(params![fts_query], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// End-to-end regression for the dotted-filename search bug (PR #81).
/// Searching `current.md` used to reach FTS5 **bare** and SQLite
/// errored with `fts5: syntax error near "."`, so the web UI showed
/// "No results" and the MCP surfaced the raw error. The string-level
/// `fts_query` unit tests only proved the *output* was quoted — they
/// never exercised real FTS5. This drives the actual indexed
/// `pages_fts` (via `upsert_page` → `path_search` triggers) to prove
/// the prepared query (a) does not error and (b) matches the page at
/// `reference/architecture-current.md`. This is the scenario that
/// would have caught the bug *before* it shipped.
#[test]
fn dotted_filename_search_matches_indexed_path() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    upsert_page(
        &mut conn,
        &page(ws, proj, "reference/architecture-current.md", "body text"),
    )
    .unwrap();

    // The prepared query must not error AND must find the page.
    let hits = fts_match_paths(&conn, "current.md")
        .expect("dotted-filename search must not raise an FTS5 syntax error");
    assert!(
        hits.iter()
            .any(|p| p == "reference/architecture-current.md"),
        "search for `current.md` should match the indexed path; got {hits:?}"
    );

    // Guard the sanitizer is load-bearing: the same token reaching
    // FTS5 bare (the pre-fix behaviour) is a hard syntax error.
    let bare = conn
        .prepare("SELECT rowid FROM pages_fts WHERE pages_fts MATCH ?1")
        .unwrap()
        .query_map(params!["current.md"], |r| r.get::<_, i64>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<i64>>>);
    assert!(
        bare.is_err(),
        "raw `current.md` should error in FTS5 — if this passes, the \
             quoting sanitizer is no longer load-bearing and the test above \
             proves nothing"
    );
}

/// Regression for the live-found hyphen bug: searching `ui-refresh`
/// returned nothing in prod even though
/// `follow-ups/ui-refresh-scroll-restoration.md` exists. The first fix
/// quoted it as `"ui-refresh"`, which **does not error but also does not
/// match** the indexed `ui refresh` — only `"ui refresh"` (sub-token
/// phrase) does. The string-level test can't see this; this drives real
/// FTS5 against the real `path_search` index. It would have caught the
/// bug the dotted-only fix left behind.
#[test]
fn hyphenated_filename_search_matches_indexed_path() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    upsert_page(
        &mut conn,
        &page(
            ws,
            proj,
            "follow-ups/ui-refresh-scroll-restoration.md",
            "body text",
        ),
    )
    .unwrap();

    let hits = fts_match_paths(&conn, "ui-refresh")
        .expect("hyphenated search must not raise an FTS5 syntax error");
    assert!(
        hits.iter()
            .any(|p| p == "follow-ups/ui-refresh-scroll-restoration.md"),
        "search for `ui-refresh` should match the indexed path; got {hits:?}"
    );

    // Pin the exact FTS5 quirk the fix works around: the keeps-the-hyphen
    // phrase matches nothing, the spaces phrase matches. If this ever
    // flips, the sub-token quoting is no longer load-bearing.
    let count = |q: &str| -> i64 {
        conn.query_row(
            "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH ?1",
            params![q],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        count("\"ui-refresh\""),
        0,
        "kept-hyphen phrase must not match"
    );
    assert_eq!(count("\"ui refresh\""), 1, "sub-token phrase must match");
}

/// Issue #103 legacy heal: the two broad sentinels ($HOME and `/`) are
/// always NULLed. Here the inputs are non-existent fake paths, so the
/// nested row is preserved by the multi-user/unmounted safety rule (a
/// `repo_path` absent on this host is left alone), not merely because it
/// is non-sentinel -- the filesystem broadening is covered separately by
/// `heal_catch_all_repo_paths_uses_filesystem_to_judge_catch_alls`.
#[test]
fn heal_catch_all_repo_paths_nulls_home_and_root_only() {
    let (_tmp, mut conn, ws, _proj) = fresh_db();

    let read_repo_path = |conn: &Connection, id: &ProjectId| -> Option<String> {
        conn.query_row(
            "SELECT repo_path FROM projects WHERE id = ?1",
            params![id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };

    let home = get_or_create_project(&mut conn, &ws, "home", Some("/home/tester")).unwrap();
    let root = get_or_create_project(&mut conn, &ws, "root", Some("/")).unwrap();
    let nested =
        get_or_create_project(&mut conn, &ws, "app", Some("/home/tester/projects/app")).unwrap();
    let none = get_or_create_project(&mut conn, &ws, "none", None).unwrap();

    let healed = heal_catch_all_repo_paths(&mut conn, Some("/home/tester")).unwrap();
    assert_eq!(healed, 2, "only the $HOME and `/` rows should be healed");

    assert_eq!(
        read_repo_path(&conn, &home),
        None,
        "$HOME row must be NULLed"
    );
    assert_eq!(read_repo_path(&conn, &root), None, "`/` row must be NULLed");
    assert_eq!(
        read_repo_path(&conn, &nested),
        Some("/home/tester/projects/app".to_string()),
        "nested fake path does not exist on disk, so the safety rule preserves it"
    );
    assert_eq!(read_repo_path(&conn, &none), None, "NULL row stays NULL");

    // Idempotent: a second pass over a healed DB changes nothing.
    assert_eq!(
        heal_catch_all_repo_paths(&mut conn, Some("/home/tester")).unwrap(),
        0,
        "re-running the heal must be a no-op"
    );
}

/// Filesystem broadening of the #103 heal: a real cwd captured as a
/// catch-all (a non-git ancestor like `~/projects`) is NULLed, a real git
/// work-tree root is preserved, and a path absent on this host is left
/// alone (multi-user/unmounted safety).
#[test]
fn heal_catch_all_repo_paths_uses_filesystem_to_judge_catch_alls() {
    let (_tmp, mut conn, ws, _proj) = fresh_db();
    let fixtures = TempDir::new().unwrap();

    let read_repo_path = |conn: &Connection, id: &ProjectId| -> Option<String> {
        conn.query_row(
            "SELECT repo_path FROM projects WHERE id = ?1",
            params![id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };

    // Real non-git ancestor (the legacy `~/projects`-style catch-all).
    let non_git = fixtures.path().join("projects");
    std::fs::create_dir_all(&non_git).unwrap();
    let non_git_path = non_git.to_str().unwrap().to_string();
    let non_git_proj =
        get_or_create_project(&mut conn, &ws, "non_git", Some(&non_git_path)).unwrap();

    // Real git work-tree root (legitimate prefix key).
    let git_root = fixtures.path().join("repo");
    std::fs::create_dir_all(git_root.join(".git")).unwrap();
    let git_path = git_root.to_str().unwrap().to_string();
    let git_proj = get_or_create_project(&mut conn, &ws, "git_root", Some(&git_path)).unwrap();

    // Path absent on this host (do NOT create it).
    let gone = fixtures.path().join("gone");
    let gone_path = gone.to_str().unwrap().to_string();
    let gone_proj = get_or_create_project(&mut conn, &ws, "gone", Some(&gone_path)).unwrap();

    let healed = heal_catch_all_repo_paths(&mut conn, None).unwrap();
    assert_eq!(
        healed, 1,
        "only the real non-git catch-all should be healed"
    );

    assert_eq!(
        read_repo_path(&conn, &non_git_proj),
        None,
        "real non-git ancestor must be healed"
    );
    assert_eq!(
        read_repo_path(&conn, &git_proj),
        Some(normalize_repo_path_key(&git_path)),
        "real git work-tree root must be preserved"
    );
    assert_eq!(
        read_repo_path(&conn, &gone_proj),
        Some(normalize_repo_path_key(&gone_path)),
        "path absent on this host must be preserved (multi-user/unmounted safety)"
    );
}

#[test]
fn hook_admission_records_the_first_machine_and_keeps_it() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, None);
    let event = hook_observation(&session);
    let machine_of = |conn: &Connection| -> Option<String> {
        conn.query_row(
            "SELECT machine_id FROM sessions WHERE id = ?1",
            params![session.id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    admit_hook_session_event(&mut conn, &session, &event, &OwnerFilter::Any, None, None).unwrap();
    assert_eq!(machine_of(&conn), None, "an older client sends no machine");
    admit_hook_session_event(
        &mut conn,
        &session,
        &event,
        &OwnerFilter::Any,
        None,
        Some("sleeper"),
    )
    .unwrap();
    assert_eq!(machine_of(&conn).as_deref(), Some("sleeper"));
    admit_hook_session_event(
        &mut conn,
        &session,
        &event,
        &OwnerFilter::Any,
        None,
        Some("titan"),
    )
    .unwrap();
    assert_eq!(machine_of(&conn).as_deref(), Some("sleeper"));
}

#[test]
fn hook_admission_rejects_cross_owner_fresh_key_without_claiming_it() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let alice = "user:alice";
    let bob = "user:bob";
    let session = hook_session(SessionId::new(), ws, proj, Some(alice));
    begin_session(&mut conn, &session).unwrap();
    let bob_event = hook_observation(&session);

    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &session,
            &bob_event,
            &OwnerFilter::User(bob.into()),
            Some("fresh-key"),
            None,
        ),
        Err(StoreError::SessionCollision)
    ));
    let observations: u64 = conn
        .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
        .unwrap();
    let keys: u64 = conn
        .query_row("SELECT COUNT(*) FROM ingest_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!((observations, keys), (0, 0));

    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &session,
            &bob_event,
            &OwnerFilter::User(alice.into()),
            Some("fresh-key"),
            None,
        )
        .unwrap(),
        HookSessionAdmission::Observation {
            ingest: IngestObservationOutcome::Inserted(_),
            ..
        }
    ));
}

// A mid-session `cd` into another project resolves the event to a
// different (workspace, project) than the session row. Under the default
// `follow-cwd` routing that is legitimate — the observation belongs in the
// project it names — so scope difference must NOT be treated as a UUID
// collision. Guarding it as one silently dropped every cross-project
// mid-session event (#394 / #396 composition).
#[test]
fn cross_project_mid_session_observation_is_admitted_not_dropped() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    begin_session(&mut conn, &session).unwrap();

    // Same session and operator, event resolved into a sibling project.
    let visited = get_or_create_project(&mut conn, &ws, "visited", None).unwrap();
    let mut moved = session.clone();
    moved.project_id = visited;
    let observation = hook_observation(&moved);

    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &moved,
            &observation,
            &OwnerFilter::User("user:alice".into()),
            None,
            None,
        )
        .unwrap(),
        HookSessionAdmission::Observation {
            ingest: IngestObservationOutcome::Inserted(_),
            ..
        }
    ));

    // It lands in the project it named, and the session row is untouched.
    let landed: Vec<u8> = conn
        .query_row("SELECT project_id FROM observations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(landed.as_slice(), visited.as_bytes());
    let session_project: Vec<u8> = conn
        .query_row(
            "SELECT project_id FROM sessions WHERE id = ?1",
            params![session.id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session_project.as_slice(), proj.as_bytes());
}

// Identity is still identity: a different operator or a different agent
// reusing the UUID stays terminal, in the same scope-moved shape as above.
#[test]
fn cross_project_events_still_reject_foreign_owner_and_agent() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    begin_session(&mut conn, &session).unwrap();
    let visited = get_or_create_project(&mut conn, &ws, "visited", None).unwrap();

    let mut moved = session.clone();
    moved.project_id = visited;
    let observation = hook_observation(&moved);
    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &moved,
            &observation,
            &OwnerFilter::User("user:bob".into()),
            None,
            None,
        ),
        Err(StoreError::SessionCollision)
    ));

    let mut other_agent = moved.clone();
    other_agent.agent_kind = AgentKind::ClaudeCode;
    let observation = hook_observation(&other_agent);
    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &other_agent,
            &observation,
            &OwnerFilter::User("user:alice".into()),
            None,
            None,
        ),
        Err(StoreError::SessionCollision)
    ));

    let observations: u64 = conn
        .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(observations, 0, "neither rejection may leave a row");
}

// A terminal event naming a different scope is not this session's end, so
// it is dropped rather than ending someone else's session — preserving the
// pre-guard `SessionEndDisposition::DropInvalid` arm.
#[test]
fn cross_project_session_end_is_dropped_without_ending_the_session() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    begin_session(&mut conn, &session).unwrap();
    let visited = get_or_create_project(&mut conn, &ws, "visited", None).unwrap();

    let mut moved = session.clone();
    moved.project_id = visited;
    let end = session_end_observation(&moved);
    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &moved,
            &end,
            &OwnerFilter::User("user:alice".into()),
            None,
            None,
        )
        .unwrap(),
        HookSessionAdmission::InvalidScopedEnd
    ));

    let ended: Option<i64> = conn
        .query_row(
            "SELECT ended_at FROM sessions WHERE id = ?1",
            params![session.id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ended, None, "a foreign-scope end must not end the session");
    let observations: u64 = conn
        .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(observations, 0);
}

#[test]
fn concurrent_alice_and_bob_events_only_admit_alice_observation() {
    let (tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    begin_session(&mut conn, &session).unwrap();
    drop(conn);
    let path = tmp.path().join("test.sqlite");
    let gate = Arc::new(std::sync::Barrier::new(2));
    let run = |owner: &'static str, key: &'static str, gate: Arc<std::sync::Barrier>| {
        let path = path.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            let mut conn = Connection::open(path).unwrap();
            conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
            gate.wait();
            admit_hook_session_event(
                &mut conn,
                &session,
                &hook_observation(&session),
                &OwnerFilter::User(owner.into()),
                Some(key),
                None,
            )
        })
    };
    let alice = run("user:alice", "alice-key", Arc::clone(&gate));
    let bob = run("user:bob", "bob-key", gate);
    assert!(matches!(
        alice.join().unwrap(),
        Ok(HookSessionAdmission::Observation { .. })
    ));
    assert!(matches!(
        bob.join().unwrap(),
        Err(StoreError::SessionCollision)
    ));

    let conn = Connection::open(path).unwrap();
    let observations: u64 = conn
        .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(observations, 1);
}

#[test]
fn hook_admission_rejects_tuple_mismatches_before_mutation() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    let mut observation = hook_observation(&session);
    observation.project_id = other;

    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &session,
            &observation,
            &OwnerFilter::User("user:alice".into()),
            Some("tuple-key"),
            None,
        ),
        Err(StoreError::InvalidState(_))
    ));
    let counts: (u64, u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sessions), (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    assert_eq!(counts, (0, 0, 0));

    let alice_session = hook_session(session.id, ws, proj, Some("user:alice"));
    begin_session(&mut conn, &alice_session).unwrap();
    let mut agent_mismatch = alice_session.clone();
    agent_mismatch.agent_kind = AgentKind::ClaudeCode;
    let observation = hook_observation(&alice_session);
    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &agent_mismatch,
            &observation,
            &OwnerFilter::User("user:alice".into()),
            Some("agent-key"),
            None,
        ),
        Err(StoreError::SessionCollision)
    ));
    let counts: (u64, u64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(counts, (0, 0));
}

#[test]
fn hook_admission_rejects_unadmitted_new_owner_and_accepts_shared_session() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let owned = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &owned,
            &hook_observation(&owned),
            &OwnerFilter::User("user:bob".into()),
            Some("denied-owner"),
            None,
        ),
        Err(StoreError::SessionCollision)
    ));
    let counts: (u64, u64, u64) = conn
            .query_row("SELECT (SELECT COUNT(*) FROM sessions), (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap();
    assert_eq!(counts, (0, 0, 0));

    let shared = hook_session(SessionId::new(), ws, proj, None);
    let admission = admit_hook_session_event(
        &mut conn,
        &shared,
        &hook_observation(&shared),
        &OwnerFilter::User("user:alice".into()),
        None,
        None,
    )
    .unwrap();
    assert!(admitted_session(admission).owner().is_none());
    let owner: Option<String> = conn
        .query_row(
            "SELECT actor_user FROM sessions WHERE id = ?1",
            params![shared.id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(owner.is_none());

    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &shared,
            &hook_observation(&shared),
            &OwnerFilter::User("user:bob".into()),
            Some("shared-bob"),
            None,
        )
        .unwrap(),
        HookSessionAdmission::Observation { .. }
    ));
    let owner: Option<String> = conn
        .query_row(
            "SELECT actor_user FROM sessions WHERE id = ?1",
            params![shared.id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(owner.is_none());
}

#[test]
fn open_session_end_admissions_preserve_all_ingest_outcomes() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    for (key, completed, expected_complete) in [
        ("open-inserted", None, false),
        ("open-pending", Some(false), false),
        ("open-complete", Some(true), true),
    ] {
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        begin_session(&mut conn, &session).unwrap();
        if let Some(completed) = completed {
            seed_ingest_key(&conn, proj, key, completed);
        }
        let admission = admit_hook_session_event(
            &mut conn,
            &session,
            &session_end_observation(&session),
            &OwnerFilter::User("user:alice".into()),
            Some(key),
            None,
        )
        .unwrap();
        match admission {
            HookSessionAdmission::EndOpen { ingest, .. } if key == "open-inserted" => {
                assert!(matches!(ingest, IngestObservationOutcome::Inserted(_)));
            }
            HookSessionAdmission::EndOpen { ingest, .. } if key == "open-pending" => {
                assert_eq!(ingest, IngestObservationOutcome::ResumePending);
            }
            HookSessionAdmission::EndOpen { ingest, .. } if expected_complete => {
                assert_eq!(ingest, IngestObservationOutcome::AlreadyComplete);
            }
            other => panic!("unexpected open SessionEnd admission: {other:?}"),
        }
    }
}

#[test]
fn reend_session_end_admissions_preserve_all_ingest_outcomes() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    for (key, completed, expected_complete) in [
        ("reend-inserted", None, false),
        ("reend-pending", Some(false), false),
        ("reend-complete", Some(true), true),
    ] {
        let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
        begin_session(&mut conn, &session).unwrap();
        end_session(&mut conn, &session.id, None).unwrap();
        insert_observation(&mut conn, &hook_observation(&session)).unwrap();
        if let Some(completed) = completed {
            seed_ingest_key(&conn, proj, key, completed);
        }
        let admission = admit_hook_session_event(
            &mut conn,
            &session,
            &session_end_observation(&session),
            &OwnerFilter::User("user:alice".into()),
            Some(key),
            None,
        )
        .unwrap();
        match admission {
            HookSessionAdmission::ReEnd { ingest, .. } if key == "reend-inserted" => {
                assert!(matches!(ingest, IngestObservationOutcome::Inserted(_)));
            }
            HookSessionAdmission::ReEnd { ingest, .. } if key == "reend-pending" => {
                assert_eq!(ingest, IngestObservationOutcome::ResumePending);
            }
            HookSessionAdmission::ReEnd { ingest, .. } if expected_complete => {
                assert_eq!(ingest, IngestObservationOutcome::AlreadyComplete);
            }
            other => panic!("unexpected re-end SessionEnd admission: {other:?}"),
        }
    }
}

#[test]
fn missing_session_end_creates_no_session_key_or_observation() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &session,
            &session_end_observation(&session),
            &OwnerFilter::User("user:alice".into()),
            Some("missing-end"),
            None,
        )
        .unwrap(),
        HookSessionAdmission::InvalidMissingEnd
    ));
    let counts: (u64, u64, u64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sessions), (SELECT COUNT(*) FROM observations), (SELECT COUNT(*) FROM ingest_keys)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    assert_eq!(counts, (0, 0, 0));
}

#[test]
fn ordinary_observation_already_complete_remains_observation_admission() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    begin_session(&mut conn, &session).unwrap();
    seed_ingest_key(&conn, proj, "ordinary-complete", true);
    assert!(matches!(
        admit_hook_session_event(
            &mut conn,
            &session,
            &hook_observation(&session),
            &OwnerFilter::User("user:alice".into()),
            Some("ordinary-complete"),
            None,
        )
        .unwrap(),
        HookSessionAdmission::Observation {
            ingest: IngestObservationOutcome::AlreadyComplete,
            ..
        }
    ));
}

#[test]
fn admitted_guard_revalidates_and_lifecycle_preserves_handoff_release_audit() {
    let (_tmp, mut conn, ws, proj) = fresh_db();
    let session = hook_session(SessionId::new(), ws, proj, Some("user:alice"));
    let guard = admitted_session(
        admit_hook_session_event(
            &mut conn,
            &session,
            &hook_observation(&session),
            &OwnerFilter::User("user:alice".into()),
            None,
            None,
        )
        .unwrap(),
    );
    conn.execute(
        "DELETE FROM sessions WHERE id = ?1",
        params![session.id.as_bytes()],
    )
    .unwrap();
    assert!(matches!(
        end_admitted_session(&mut conn, &guard, None),
        Err(StoreError::SessionCollision)
    ));
    begin_session(&mut conn, &session).unwrap();
    conn.execute(
        "UPDATE sessions SET actor_user = 'user:bob' WHERE id = ?1",
        params![session.id.as_bytes()],
    )
    .unwrap();
    assert!(matches!(
        end_admitted_lifecycle_only_session(&mut conn, &guard),
        Err(StoreError::SessionCollision)
    ));
    let handoff = NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: Some(session.id),
        from_agent: AgentKind::Codex,
        to_agent: None,
        cwd: None,
        summary: "x".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: Some("user:alice".into()),
    };
    assert!(matches!(
        end_admitted_session_with_handoff(&mut conn, &guard, None, &handoff),
        Err(StoreError::SessionCollision)
    ));

    let receiver = hook_session(SessionId::new(), ws, proj, None);
    let mut lifecycle_observation = hook_observation(&receiver);
    lifecycle_observation.kind = ObservationKind::SessionStart;
    let receiver_guard = admitted_session(
        admit_hook_session_event(
            &mut conn,
            &receiver,
            &lifecycle_observation,
            &OwnerFilter::User("user:alice".into()),
            None,
            None,
        )
        .unwrap(),
    );
    let handoff = NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: None,
        summary: "x".into(),
        open_questions: vec![],
        next_steps: vec![],
        files_touched: vec![],
        owner_user: None,
    };
    let handoff_id = insert_handoff(&mut conn, &handoff).unwrap();
    let mut acceptance = handoff_acceptance(handoff_id, ws, proj);
    acceptance.accepting_session = Some(receiver.id);
    assert!(accept_handoff(&mut conn, &acceptance).unwrap());
    assert_eq!(
        end_admitted_lifecycle_only_session(&mut conn, &receiver_guard).unwrap(),
        LifecycleOnlyEndOutcome::Ended {
            reopened_handoff: Some(handoff_id)
        }
    );
    assert_eq!(audit_row_for(&conn, "release_lifecycle_only_handoff").0, 1);
}
