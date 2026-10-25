//! P1.6: outage survival, crash-recovery, and spool replay idempotency.
//!
//! End-to-end coverage across the P1.1 ingest endpoint and the P1.2 spool and
//! drain engine, over real HTTP against a real store — no mock sink:
//!
//! - `outage_queues_1000_then_drains_with_zero_loss`: 1,000 events spool while
//!   no Sleeper is reachable; once it comes back every event lands exactly
//!   once, in order, with intact content.
//! - `duplicate_replay_of_all_1000_is_a_noop`: resending all 1,000 events
//!   changes nothing (`UNIQUE(session_id, event_id)` holds the line).
//! - `crash_between_accept_and_ack_resumes_without_duplicates`: a delivery the
//!   server stored but the client never ACKed is redelivered once and skipped
//!   server-side; the final count is still exactly 1,000.
//! - `spool_survives_process_restart_mid_drain`: kill the server halfway (plus
//!   a planted torn write fragment), reopen everything, and resume from the
//!   durable high-water mark to exactly 1,000 with zero loss.
//!
//! Each test owns its spool directory, store directory, and loopback server,
//! so they run hermetically in any order.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use sessionmunch_core::{
    NewSessionEvent, Sanitizer, SessionId, SessionSource, WorkstreamEventKind,
};
use sessionmunch_hooks::{SessionEventState, session_events_router};
use sessionmunch_store::Store;
use sessionmunch_workstream::{
    DeliveryReceipt, EventSink, HttpEventSink, SinkError, Spool, SpoolBatch, SpoolConfig,
    SpoolDrainer,
};
use tempfile::TempDir;

/// Events per acceptance scenario.
const EVENT_COUNT: usize = 1_000;
/// Events per spooled batch (100 batches cover 1,000 events).
const EVENTS_PER_BATCH: usize = 10;
/// Rare token embedded in every event body for the FTS cross-check.
const PROBE_TOKEN: &str = "zephyrmunch";

/// A live Sleeper: real store plus the real P1.1 ingest router on loopback.
struct Sleeper {
    _spool_parent: TempDir,
    store: Store,
    base_url: String,
    _server: tokio::task::JoinHandle<()>,
}

impl Sleeper {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let state = SessionEventState {
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            sanitizer: Sanitizer::default(),
            wiki: None,
        };
        let app = session_events_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            _spool_parent: dir,
            store,
            base_url,
            _server: server,
        }
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        self._server.abort();
    }
}

fn sample_event(index: usize) -> NewSessionEvent {
    NewSessionEvent {
        event_id: format!("p16-{index:06}"),
        kind: WorkstreamEventKind::Message,
        role: Some("user".into()),
        content: format!("outage probe {index:06} {PROBE_TOKEN}"),
        occurred_at: Some("2026-10-05T00:00:00Z".into()),
        source: SessionSource::Hook,
        metadata: serde_json::json!({"model": "deepseek-v4"}),
    }
}

fn open_spool(dir: &std::path::Path) -> Spool {
    Spool::open(&SpoolConfig {
        dir: dir.to_path_buf(),
        max_spool_bytes: 0,
    })
    .unwrap()
}

/// Enqueue all 1,000 acceptance events; returns the session id and the exact
/// batches sent (kept so the replay test can resend them verbatim).
fn enqueue_1000(spool: &Spool) -> (SessionId, Vec<SpoolBatch>) {
    let session_id = SessionId::new();
    let mut batches = Vec::new();
    for batch_no in 0..EVENT_COUNT / EVENTS_PER_BATCH {
        let events: Vec<NewSessionEvent> = (0..EVENTS_PER_BATCH)
            .map(|i| sample_event(batch_no * EVENTS_PER_BATCH + i))
            .collect();
        let batch = SpoolBatch::new("qa-ws", "qa-proj", session_id.to_string(), "hermes", events);
        spool.enqueue(&batch).unwrap();
        batches.push(batch);
    }
    (session_id, batches)
}

/// Drain until the spool is empty; returns (batches, accepted-events).
async fn drain_all<S: EventSink>(drainer: &SpoolDrainer<S>, spool: &Spool) -> (usize, usize) {
    let mut batches = 0usize;
    let mut accepted = 0usize;
    for _ in 0..10_000 {
        if spool.is_empty().unwrap() {
            break;
        }
        let report = drainer.drain_once().await;
        assert!(
            !report.permanent_failure,
            "drain hit a permanent failure at seq {:?}",
            report.stopped_at_seq
        );
        batches += report.delivered_batches;
        accepted += report.accepted_events;
    }
    assert!(spool.is_empty().unwrap(), "spool did not drain to empty");
    (batches, accepted)
}

/// Verify the store holds exactly the 1,000 acceptance events: full id set,
/// contiguous per-session sequence, and byte-identical content.
async fn assert_exact_1000(store: &Store, session_id: SessionId) {
    let events = store
        .reader
        .session_events(session_id, EVENT_COUNT + 100)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        EVENT_COUNT,
        "store must hold exactly 1000 events"
    );
    let ids: HashSet<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids.len(), EVENT_COUNT, "event ids must be unique");
    for i in 0..EVENT_COUNT {
        let want = format!("p16-{i:06}");
        assert!(ids.contains(want.as_str()), "missing event {want}");
    }
    let mut sequences: Vec<i64> = events.iter().map(|e| e.sequence).collect();
    sequences.sort_unstable();
    for pair in sequences.windows(2) {
        assert_eq!(pair[1] - pair[0], 1, "sequences must be contiguous");
    }
    for event in &events {
        let index: usize = event.event_id[4..].parse().unwrap();
        assert_eq!(
            event.content,
            format!("outage probe {index:06} {PROBE_TOKEN}"),
            "content of {} must survive sanitized round-trip",
            event.event_id
        );
    }
}

/// Outage: 1,000 events queue with no Sleeper listening, then drain to zero
/// loss once it is back. The first drain attempt must fail offline without
/// losing or reordering anything.
#[tokio::test]
async fn outage_queues_1000_then_drains_with_zero_loss() {
    let spool_dir = TempDir::new().unwrap();
    let spool = open_spool(spool_dir.path());
    let (session_id, _) = enqueue_1000(&spool);
    assert_eq!(spool.metrics().pending_batches, 100);

    // No server yet: the drain attempt stops offline and keeps everything.
    let offline_sink = Arc::new(HttpEventSink::new("http://127.0.0.1:1", None));
    let offline_drainer = SpoolDrainer::new(spool.clone(), offline_sink);
    let report = offline_drainer.drain_once().await;
    assert_eq!(report.delivered_batches, 0);
    assert!(report.stopped_on_offline);
    assert_eq!(spool.metrics().pending_batches, 100);

    // Sleeper comes back; everything drains exactly once.
    let sleeper = Sleeper::start().await;
    let sink = Arc::new(HttpEventSink::new(&sleeper.base_url, None));
    let drainer = SpoolDrainer::new(spool.clone(), sink);
    let (batches, accepted) = drain_all(&drainer, &spool).await;
    assert_eq!(batches, 100);
    assert_eq!(accepted, EVENT_COUNT);
    assert_eq!(spool.metrics().highwater, 100);
    assert_exact_1000(&sleeper.store, session_id).await;

    // The drained events are lexically retrievable (P1.5 index rode along).
    let hits = sleeper
        .store
        .reader
        .search_session_events(PROBE_TOKEN.into(), 10)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.session_id == session_id),
        "drained events must be visible to lexical search"
    );
}

/// Replay: resending all 1,000 events verbatim is a store-level no-op.
#[tokio::test]
async fn duplicate_replay_of_all_1000_is_a_noop() {
    let spool_dir = TempDir::new().unwrap();
    let spool = open_spool(spool_dir.path());
    let (session_id, batches) = enqueue_1000(&spool);

    let sleeper = Sleeper::start().await;
    let sink = HttpEventSink::new(&sleeper.base_url, None);
    let drainer = SpoolDrainer::new(
        spool.clone(),
        Arc::new(HttpEventSink::new(&sleeper.base_url, None)),
    );
    let (_, accepted) = drain_all(&drainer, &spool).await;
    assert_eq!(accepted, EVENT_COUNT);
    assert_exact_1000(&sleeper.store, session_id).await;

    // Resend every batch exactly as the wire carried it: zero new rows.
    let mut replayed = 0usize;
    for batch in &batches {
        let receipt = sink.deliver(batch).await.unwrap();
        assert_eq!(receipt.accepted, 0, "replayed batch must be skipped");
        replayed += 1;
    }
    assert_eq!(replayed, 100);
    assert_exact_1000(&sleeper.store, session_id).await;

    // And the drain engine itself has nothing left to do.
    let report = drainer.drain_once().await;
    assert_eq!(report.delivered_batches, 0);
}

/// Steady-state restart: the node's spool was already fully drained before it
/// died, so on reopen the batch directory is empty. Every event captured after
/// the restart must still reach Sleeper. This is the common case for a healthy
/// long-lived node, and it is distinct from
/// `spool_survives_process_restart_mid_drain` (which reopens with 50 batches
/// still pending, so the sequence high-water is visible on disk).
#[tokio::test]
async fn restart_after_fully_drained_spool_still_delivers_new_events() {
    let spool_dir = TempDir::new().unwrap();

    // --- Process 1: capture and drain a first session, then go away. ---
    {
        let spool = open_spool(spool_dir.path());
        let (first_session, _) = enqueue_1000(&spool);
        let sleeper = Sleeper::start().await;
        let sink = Arc::new(HttpEventSink::new(&sleeper.base_url, None));
        let drainer = SpoolDrainer::new(spool.clone(), sink);
        let (_, accepted) = drain_all(&drainer, &spool).await;
        assert_eq!(accepted, EVENT_COUNT);
        assert_exact_1000(&sleeper.store, first_session).await;
        // The spool is now completely empty: no batch files, no pending work.
        assert!(spool.is_empty().unwrap());
        assert_eq!(spool.metrics().pending_batches, 0);
    }

    // --- Process 2: the restarted node captures a fresh session. ---
    let spool = open_spool(spool_dir.path());
    let session_id = SessionId::new();
    let events: Vec<NewSessionEvent> = (0..EVENTS_PER_BATCH)
        .map(|i| sample_event(EVENT_COUNT + i))
        .collect();
    spool
        .enqueue(&SpoolBatch::new(
            "qa-ws",
            "qa-proj",
            session_id.to_string(),
            "hermes",
            events,
        ))
        .unwrap();
    assert_eq!(
        spool.metrics().pending_batches,
        1,
        "the fresh batch must be counted as pending"
    );

    let sleeper = Sleeper::start().await;
    let sink = Arc::new(HttpEventSink::new(&sleeper.base_url, None));
    let drainer = SpoolDrainer::new(spool.clone(), sink);
    let (batches, accepted) = drain_all(&drainer, &spool).await;

    assert_eq!(batches, 1, "the post-restart batch must be delivered");
    assert_eq!(
        accepted, EVENTS_PER_BATCH,
        "and must be accepted by Sleeper"
    );

    let stored = sleeper
        .store
        .reader
        .session_events(session_id, 100)
        .await
        .unwrap();
    assert_eq!(
        stored.len(),
        EVENTS_PER_BATCH,
        "events captured after a clean restart must not be silently stranded"
    );
}

/// A sink that forwards to the real HTTP sink, then — past `die_after`
/// successful deliveries — reports the (already stored) batch as lost. This
/// models a client crash between the server's 202 and the local high-water
/// write: the batch is durable remotely but still pending locally.
struct CrashAfterAck {
    inner: HttpEventSink,
    die_after: usize,
    delivered: AtomicUsize,
}

#[async_trait]
impl EventSink for CrashAfterAck {
    async fn deliver(&self, batch: &SpoolBatch) -> Result<DeliveryReceipt, SinkError> {
        let receipt = self.inner.deliver(batch).await?;
        let n = self.delivered.fetch_add(1, Ordering::SeqCst);
        if n >= self.die_after {
            return Err(SinkError::Transport(
                "simulated crash: acceptance lost before local ack".into(),
            ));
        }
        Ok(receipt)
    }
}

/// Crash mid-drain: the batch stored-but-never-ACKed is redelivered once and
/// skipped server-side; the ledger still converges to exactly 1,000.
#[tokio::test]
async fn crash_between_accept_and_ack_resumes_without_duplicates() {
    let spool_dir = TempDir::new().unwrap();
    let spool = open_spool(spool_dir.path());
    let (session_id, _) = enqueue_1000(&spool);

    let sleeper = Sleeper::start().await;
    let crashing = Arc::new(CrashAfterAck {
        inner: HttpEventSink::new(&sleeper.base_url, None),
        die_after: 30,
        delivered: AtomicUsize::new(0),
    });
    let drainer = SpoolDrainer::new(spool.clone(), crashing);

    // Drain until the simulated crash: 30 batches acked, batch 30 stored
    // remotely but reported lost, so the pass stops with it still pending.
    let mut accepted = 0usize;
    for _ in 0..10_000 {
        let report = drainer.drain_once().await;
        accepted += report.accepted_events;
        if report.stopped_on_offline {
            break;
        }
    }
    assert_eq!(accepted, 300, "30 acked batches report acceptance");
    assert_eq!(spool.metrics().highwater, 30);
    assert_eq!(spool.metrics().pending_batches, 70);
    // Batch 30 reached the server (310 rows) but its receipt was lost, so it
    // is still pending locally: the redelivery the resume half performs.
    let stored = sleeper
        .store
        .reader
        .session_events(session_id, EVENT_COUNT + 100)
        .await
        .unwrap();
    assert_eq!(stored.len(), 310);

    // A fresh drainer over the same spool resumes from the high-water mark.
    // Batch 30 goes down the wire a second time and must be skipped.
    let honest = Arc::new(HttpEventSink::new(&sleeper.base_url, None));
    let resume = SpoolDrainer::new(spool.clone(), honest);
    let (batches, resumed_accepted) = drain_all(&resume, &spool).await;
    assert_eq!(batches, 70);
    assert_eq!(
        resumed_accepted, 690,
        "the redelivered batch contributes zero new rows"
    );
    assert_exact_1000(&sleeper.store, session_id).await;
}

/// Full process-death simulation: server and spool handles are dropped
/// mid-drain, a torn write fragment is planted, and everything reopens.
/// Recovery resumes from the durable high-water mark to exactly 1,000.
#[tokio::test]
async fn spool_survives_process_restart_mid_drain() {
    let spool_dir = TempDir::new().unwrap();
    let spool = open_spool(spool_dir.path());
    let (session_id, _) = enqueue_1000(&spool);
    let session_string = session_id.to_string();

    let data_dir = TempDir::new().unwrap();
    let data_path = data_dir.path().to_path_buf();
    let store = Store::open(&data_path).unwrap();
    let state = SessionEventState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        sanitizer: Sanitizer::default(),
        wiki: None,
    };
    let app = session_events_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Drain exactly half, then die: drop the drainer, the spool handle, the
    // server task, and the whole store (WAL recovery on reopen).
    let sink = Arc::new(HttpEventSink::new(&base_url, None));
    let drainer = SpoolDrainer::new(spool.clone(), sink).with_batches_per_pass(50);
    let report = drainer.drain_once().await;
    assert_eq!(report.delivered_batches, 50);
    drop(drainer);
    drop(spool);
    server.abort();
    drop(store);

    // Plant a torn write fragment: a real crash could leave one behind, and
    // reopen must discard it rather than choke or replay it.
    std::fs::write(
        spool_dir.path().join("batches").join("torn-fragment.tmp"),
        b"{\"workspace\": \"qa-ws\", \"trunca",
    )
    .unwrap();

    // Reopen everything the way a restarted node would.
    let spool = open_spool(spool_dir.path());
    assert_eq!(spool.metrics().highwater, 50);
    assert_eq!(spool.metrics().pending_batches, 50);
    let store = Store::open(&data_path).unwrap();
    let state = SessionEventState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        sanitizer: Sanitizer::default(),
        wiki: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let _server = tokio::spawn(async move {
        let _ = axum::serve(listener, session_events_router(state)).await;
    });

    let sink = Arc::new(HttpEventSink::new(&base_url, None));
    let resume = SpoolDrainer::new(spool.clone(), sink);
    let (batches, accepted) = drain_all(&resume, &spool).await;
    assert_eq!(batches, 50);
    assert_eq!(accepted, 500);
    let session_id = session_string.parse::<SessionId>().unwrap();
    assert_exact_1000(&store, session_id).await;
}

/// Regression (t_0474cfae): a poison batch at the spool head (duplicate
/// event_id within one batch, rejected 400 by `sanitize_batch`) must not
/// wedge the good batches behind it. The poison is quarantined with
/// dead-letter evidence and the tail drains in the same pass.
#[tokio::test]
async fn poison_head_is_quarantined_and_good_tail_drains() {
    let spool_dir = TempDir::new().unwrap();
    let spool = open_spool(spool_dir.path());
    let session_id = SessionId::new();
    let mk_batch = |events: Vec<NewSessionEvent>| {
        SpoolBatch::new("qa-ws", "qa-proj", session_id.to_string(), "hermes", events)
    };
    let poison = |id: &str| NewSessionEvent {
        event_id: id.to_string(),
        kind: WorkstreamEventKind::Message,
        role: Some("user".into()),
        content: format!("poison-{id}"),
        occurred_at: Some("2026-10-05T00:00:00Z".into()),
        source: SessionSource::Hook,
        metadata: serde_json::json!({"model": "deepseek-v4"}),
    };
    // seq0: poison (duplicate event_id in one batch). seq1/seq2: good.
    spool
        .enqueue(&mk_batch(vec![poison("dup-1"), poison("dup-1")]))
        .unwrap();
    spool.enqueue(&mk_batch(vec![sample_event(1)])).unwrap();
    spool.enqueue(&mk_batch(vec![sample_event(2)])).unwrap();
    assert_eq!(spool.metrics().pending_batches, 3);

    let sleeper = Sleeper::start().await;
    let sink = Arc::new(HttpEventSink::new(&sleeper.base_url, None));
    let drainer = SpoolDrainer::new(spool.clone(), sink);

    let report = drainer.drain_once().await;
    assert_eq!(report.quarantined_batches, 1, "poison head quarantined");
    assert_eq!(report.quarantined_seqs, vec![0]);
    assert_eq!(
        report.delivered_batches, 2,
        "good tail drains in the same pass"
    );
    assert!(!report.permanent_failure);
    assert!(!report.stopped_on_offline);
    assert!(spool.is_empty().unwrap(), "spool drains completely");
    assert_eq!(
        spool.quarantined_seqs(),
        vec![0],
        "dead-letter evidence kept"
    );

    // The two good events landed exactly once.
    let events = sleeper
        .store
        .reader
        .session_events(session_id, 100)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    let ids: HashSet<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert!(ids.contains("p16-000001"));
    assert!(ids.contains("p16-000002"));
}

/// The sink's status taxonomy (t_0474cfae review): only statuses that are
/// provably one batch's fault may quarantine. 400/409/413/422 map to
/// non-retryable rejection; 401/403 to the auth wall; everything else —
/// transient 429/5xx, wrong-endpoint 404/405/501 — is retryable so the
/// batches are retained, never silently discarded.
#[tokio::test]
async fn sink_status_taxonomy_never_quarantines_transient_or_endpoint_errors() {
    use axum::http::StatusCode;
    use axum::routing::post;

    async fn fixed_status_server(status: u16) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/session-events",
            post(move || async move {
                (
                    StatusCode::from_u16(status).unwrap(),
                    format!("probe-{status}"),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base_url, server)
    }

    let batch = SpoolBatch::new(
        "qa-ws",
        "qa-proj",
        SessionId::new().to_string(),
        "hermes",
        vec![sample_event(0)],
    );

    // Per-batch poison: quarantine-eligible.
    for status in [400u16, 409, 413, 422] {
        let (base_url, _server) = fixed_status_server(status).await;
        let err = HttpEventSink::new(&base_url, None)
            .deliver(&batch)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SinkError::Rejected { .. }),
            "status {status} must be a rejection"
        );
        assert!(!err.is_retryable(), "status {status} must not retry");
    }

    // Auth wall: retryable, surfaced distinctly, never quarantined.
    let (base_url, _server) = fixed_status_server(401).await;
    let err = HttpEventSink::new(&base_url, None)
        .deliver(&batch)
        .await
        .unwrap_err();
    assert!(
        matches!(err, SinkError::Unauthorized(_)),
        "401 must be the auth wall"
    );
    assert!(err.is_retryable());

    // Transient and wrong-endpoint: retryable, batches retained.
    for status in [404u16, 405, 408, 425, 429, 500, 502, 503] {
        let (base_url, _server) = fixed_status_server(status).await;
        let err = HttpEventSink::new(&base_url, None)
            .deliver(&batch)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SinkError::Transport(_)),
            "status {status} must be transport (retained, retried)"
        );
        assert!(err.is_retryable(), "status {status} must retry");
    }
}
