//! Crash-safe local spool for session events destined for a remote Sleeper.
//!
//! Every node writes its session-event batches to a durable local spool
//! before any network transmission (North Star invariant). The spool is:
//!
//! - **Crash-safe.** Each batch is committed as its own file via an atomic
//!   write (temp + `sync_all` + rename). A crash anywhere in a write leaves
//!   only a `.tmp` fragment that [`Spool::open`] discards; every committed
//!   batch file is always whole. Re-open truncates away incomplete writes and
//!   resumes exactly where the previous process left off.
//! - **Append-only.** Batches are only added, never mutated in place. Order
//!   is preserved by a monotonic sequence encoded in the filename; the
//!   only mutation is deletion of batches the remote has acknowledged.
//! - **Bounded.** Enqueue refuses (with a [`SpoolError::Backpressure`] and a
//!   *metric*) once unacknowledged bytes would exceed
//!   [`SpoolConfig::max_spool_bytes`]. `0` means unbounded.
//! - **Idempotently drained.** A separate durable *high-water mark* records
//!   the highest batch sequence Sleeper acknowledged. Draining resumes after
//!   that mark, so re-delivering already-sent batches on reconnect is a
//!   no-op at the store (the server deduplicates on `event_id`).
//!
//! When no remote Sleeper URL is configured the spool still accepts and
//! durably persists every batch locally; the caller simply never starts a
//! drain worker, which is what keeps standalone local operation intact.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use sessionmunch_core::{NewSessionEvent, SessionProvenance};

/// One enqueued session-event batch, mirroring the wire shape of the
/// `POST /session-events` ingest request so the HTTP sink can send it
/// verbatim. Reused from the P1.1 schema contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpoolBatch {
    /// Workspace the session belongs to (created on the write path).
    pub workspace: String,
    /// Project the session belongs to (created on the write path).
    pub project: String,
    /// Caller-supplied, idempotent session identifier.
    pub session_id: String,
    /// Harness wire string (e.g. `hermes`). Unknown values map to `other`
    /// on the server side.
    pub harness: String,
    /// Working directory at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional operator storage key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_user: Option<String>,
    /// Optional provenance written best-effort alongside the base row.
    #[serde(default)]
    pub provenance: SessionProvenance,
    /// Events to append. Idempotent on each `event_id`.
    pub events: Vec<NewSessionEvent>,
}

impl SpoolBatch {
    /// Build a batch for one session with a caller-supplied event list.
    #[must_use]
    pub fn new(
        workspace: impl Into<String>,
        project: impl Into<String>,
        session_id: impl Into<String>,
        harness: impl Into<String>,
        events: Vec<NewSessionEvent>,
    ) -> Self {
        Self {
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
}

/// A spooled batch with the sequence the drain engine ACKs by.
#[derive(Clone, Debug)]
pub struct SpooledRecord {
    /// Monotonic lexical order within the spool.
    pub seq: u64,
    /// The batch payload.
    pub batch: SpoolBatch,
}

/// Configuration for opening a [`Spool`].
#[derive(Clone, Debug)]
pub struct SpoolConfig {
    /// Directory that will hold the batch files and the high-water mark.
    pub dir: PathBuf,
    /// Byte cap on unacknowledged spool contents. `0` disables the cap.
    pub max_spool_bytes: u64,
}

/// Live accounting exposed to metrics.
#[derive(Clone, Debug, Default)]
pub struct SpoolMetrics {
    /// Bytes currently spooled but not yet acknowledged by Sleeper.
    pub pending_bytes: u64,
    /// Number of batches currently spooled but not yet acknowledged.
    pub pending_batches: u64,
    /// High-water ACK mark: the exclusive lower bound (count) of batches
    /// durably acknowledged by Sleeper. The next batch to drain has this seq.
    pub highwater: u64,
    /// Configured byte cap (0 = unbounded).
    pub max_spool_bytes: u64,
    /// Number of `enqueue` calls rejected by the byte cap.
    pub backpressure_rejections: u64,
}

/// Filename width for the zero-padded batch sequence. 20 digits holds any
/// realistic `u64` batch count and keeps lexical sort equal to numeric sort.
const SEQ_WIDTH: usize = 20;
/// Batch files live under this per-spool subdirectory.
const BATCH_DIR: &str = "batches";
/// Rejected batches quarantined by the drain engine live here, preserved for
/// operator inspection instead of wedging the spool head-of-line.
const QUARANTINE_DIR: &str = "quarantine";
/// High-water mark file.
const HIGHWATER_FILE: &str = "highwater";
/// `u64` values that fit in [`SEQ_WIDTH`] digits.
const SEQ_MAX: u64 = u64::MAX / 10;

#[derive(Default)]
struct Inner {
    /// Next sequence to assign to an enqueued batch.
    next_seq: u64,
    /// Highest sequentially-acknowledged batch (high-water mark).
    highwater: u64,
    /// Sum of file sizes for batches with `seq > highwater`.
    pending_bytes: u64,
    /// Count of batches with `seq > highwater`.
    pending_batches: u64,
    /// Backpressure rejections since open.
    backpressure_rejections: u64,
}

/// Errors surfaced by the spool API.
#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    /// The enqueue would push unacknowledged bytes past `max_spool_bytes`.
    #[error("spool backpressure: {pending} bytes pending exceeds cap {max}")]
    Backpressure {
        /// Bytes that would be pending after this enqueue.
        pending: u64,
        /// Configured byte cap.
        max: u64,
    },
    /// An underlying filesystem operation failed.
    #[error("spool io error: {0}")]
    Io(#[from] std::io::Error),
    /// An anyhow-wrapped error (e.g. from a helper returning `anyhow::Result`).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
    /// A batch file could not be parsed as JSON.
    #[error("invalid spool record {path}: {message}")]
    InvalidRecord {
        /// Path of the malformed batch file.
        path: String,
        /// Parse error message.
        message: String,
    },
}

/// A crash-safe, bounded, append-only spool of session-event batches.
///
/// All methods take `&self` and serialize their state behind a mutex, so a
/// producer thread and a drain worker can share one [`Spool`] handle.
#[derive(Clone)]
pub struct Spool {
    dir: PathBuf,
    max_spool_bytes: u64,
    inner: std::sync::Arc<Mutex<Inner>>,
}

impl Spool {
    /// Open (or create) the spool at `config.dir`, recovering any incomplete
    /// writes from a previous crashed process.
    pub fn open(config: &SpoolConfig) -> Result<Self> {
        let batch_dir = config.dir.join(BATCH_DIR);
        fs::create_dir_all(&batch_dir)
            .with_context(|| format!("creating spool dir {}", batch_dir.display()))?;

        // Discard any `.tmp` fragment a previous process left mid-write.
        for entry in fs::read_dir(&batch_dir)
            .with_context(|| format!("reading spool dir {}", batch_dir.display()))?
        {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }

        let highwater = read_highwater(&config.dir.join(HIGHWATER_FILE))?;
        let (next_seq, pending_bytes, pending_batches) = scan_batches(&batch_dir, highwater)?;

        let inner = Inner {
            next_seq,
            highwater,
            pending_bytes,
            pending_batches,
            backpressure_rejections: 0,
        };
        Ok(Self {
            dir: config.dir.clone(),
            max_spool_bytes: config.max_spool_bytes,
            inner: std::sync::Arc::new(Mutex::new(inner)),
        })
    }

    /// Durably add a batch to the spool, returning its sequence number.
    ///
    /// The batch is fully committed (`sync_all` + rename) before this returns,
    /// so a crash immediately after a successful [`Self::enqueue`] cannot lose
    /// the batch. Returns [`SpoolError::Backpressure`] when the byte cap would
    /// be exceeded.
    pub fn enqueue(&self, batch: &SpoolBatch) -> Result<u64, SpoolError> {
        let payload = serde_json::to_vec(batch).map_err(|e| SpoolError::InvalidRecord {
            path: "<in-memory>".into(),
            message: e.to_string(),
        })?;
        let payload_len = payload.len() as u64;

        let mut inner = self.lock()?;
        if self.max_spool_bytes > 0
            && inner.pending_bytes.saturating_add(payload_len) > self.max_spool_bytes
        {
            inner.backpressure_rejections += 1;
            return Err(SpoolError::Backpressure {
                pending: inner.pending_bytes + payload_len,
                max: self.max_spool_bytes,
            });
        }

        let seq = inner.next_seq;
        if seq > SEQ_MAX {
            return Err(anyhow!("spool sequence exhausted at {seq}").into());
        }
        let path = self.batch_path(seq);
        write_atomic(&path, &payload).map_err(SpoolError::Io)?;

        inner.next_seq = seq + 1;
        inner.pending_bytes += payload_len;
        inner.pending_batches += 1;
        Ok(seq)
    }

    /// All batches with `seq >= highwater`, in ascending order (highwater is
    /// the exclusive lower bound held by the durable ACK mark; the next batch
    /// to drain). Empty when the sink is fully caught up.
    pub fn pending(&self) -> Result<Vec<SpooledRecord>, SpoolError> {
        let inner = self.lock()?;
        let highwater = inner.highwater;
        let batch_dir = self.dir.join(BATCH_DIR);
        // Reread the directory so newly-enqueued batches (this process or a
        // crashed predecessor that fsynced then re-opened) are included.
        let mut records = Vec::new();
        for entry in fs::read_dir(&batch_dir).map_err(SpoolError::Io)? {
            let entry = entry.map_err(SpoolError::Io)?;
            let Some(seq) = parse_batch_filename(&entry.path()) else {
                continue;
            };
            if seq < highwater {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(SpoolError::Io)?;
            let batch: SpoolBatch =
                serde_json::from_slice(&bytes).map_err(|e| SpoolError::InvalidRecord {
                    path: entry.path().display().to_string(),
                    message: e.to_string(),
                })?;
            records.push(SpooledRecord { seq, batch });
        }
        records.sort_by_key(|r| r.seq);
        Ok(records)
    }

    /// Advance the high-water mark past `through_seq` (i.e. through the batch
    /// sequence the remote acknowledged) and delete every batch below the new
    /// mark, reclaiming disk. Idempotent — a repeated or out-of-order ACK is a
    /// no-op (the mark only moves forward).
    pub fn ack(&self, through_seq: u64) -> Result<usize, SpoolError> {
        let mut inner = self.lock()?;
        let batch_dir = self.dir.join(BATCH_DIR);
        let new_highwater = through_seq.saturating_add(1).max(inner.highwater);
        // Read the batch file sizes up-front so the byte accounting is exact.
        let mut to_delete = Vec::new();
        for entry in fs::read_dir(&batch_dir).map_err(SpoolError::Io)? {
            let entry = entry.map_err(SpoolError::Io)?;
            let Some(seq) = parse_batch_filename(&entry.path()) else {
                continue;
            };
            if seq < new_highwater {
                let size = entry.metadata().map_err(SpoolError::Io)?.len();
                to_delete.push((seq, entry.path(), size));
            }
        }
        for (_seq, path, size) in &to_delete {
            if fs::remove_file(path).is_ok() {
                inner.pending_bytes = inner.pending_bytes.saturating_sub(*size);
                inner.pending_batches = inner.pending_batches.saturating_sub(1);
            }
        }
        inner.highwater = new_highwater;
        let deleted = to_delete.len();
        // Persist the advanced high-water mark (atomic temp+rename).
        write_highwater(&self.dir.join(HIGHWATER_FILE), inner.highwater)?;
        Ok(deleted)
    }

    /// Quarantine the batch at `seq`: move its file into the quarantine
    /// directory (dead-letter evidence for the operator) and advance the
    /// high-water mark past it so later good batches can still drain.
    ///
    /// The caller is the drain engine on a permanent per-batch rejection
    /// (e.g. the remote answered 400). Transport and auth failures must never
    /// quarantine: those are destination-wide, not per-batch poison, and
    /// quarantining on them would discard good batches over a bad token or a
    /// dead network. Idempotent for an already-quarantined or acked seq.
    pub fn quarantine(&self, seq: u64) -> Result<bool, SpoolError> {
        let mut inner = self.lock()?;
        if seq < inner.highwater {
            return Ok(false);
        }
        let src = self.batch_path(seq);
        let Ok(bytes) = fs::read(&src) else {
            // Already gone (acked concurrently): still advance the mark so a
            // repeated quarantine request converges like `ack`.
            let new_highwater = seq.saturating_add(1).max(inner.highwater);
            inner.highwater = new_highwater;
            write_highwater(&self.dir.join(HIGHWATER_FILE), inner.highwater)?;
            return Ok(false);
        };
        let size = bytes.len() as u64;
        let quarantine_dir = self.dir.join(QUARANTINE_DIR);
        fs::create_dir_all(&quarantine_dir).map_err(SpoolError::Io)?;
        let dst = quarantine_dir.join(format!("{seq:0width$}.json", width = SEQ_WIDTH));
        write_atomic(&dst, &bytes).map_err(SpoolError::Io)?;
        fs::remove_file(&src).map_err(SpoolError::Io)?;
        inner.pending_bytes = inner.pending_bytes.saturating_sub(size);
        inner.pending_batches = inner.pending_batches.saturating_sub(1);
        inner.highwater = seq.saturating_add(1).max(inner.highwater);
        write_highwater(&self.dir.join(HIGHWATER_FILE), inner.highwater)?;
        Ok(true)
    }

    /// Sequence numbers currently held in quarantine, in ascending order.
    pub fn quarantined_seqs(&self) -> Vec<u64> {
        let quarantine_dir = self.dir.join(QUARANTINE_DIR);
        let Ok(entries) = fs::read_dir(&quarantine_dir) else {
            return Vec::new();
        };
        let mut seqs = Vec::new();
        for entry in entries.flatten() {
            if let Some(seq) = parse_batch_filename(&entry.path()) {
                seqs.push(seq);
            }
        }
        seqs.sort_unstable();
        seqs
    }

    /// Snapshot of the spool's live accounting for metrics.
    pub fn metrics(&self) -> SpoolMetrics {
        let inner = self.lock().ok();
        let (pending_bytes, pending_batches, highwater, rejections) = match &inner {
            Some(guard) => (
                guard.pending_bytes,
                guard.pending_batches,
                guard.highwater,
                guard.backpressure_rejections,
            ),
            None => return SpoolMetrics::default(),
        };
        SpoolMetrics {
            pending_bytes,
            pending_batches,
            highwater,
            max_spool_bytes: self.max_spool_bytes,
            backpressure_rejections: rejections,
        }
    }

    /// The configured byte cap (0 = unbounded).
    #[must_use]
    pub fn max_spool_bytes(&self) -> u64 {
        self.max_spool_bytes
    }

    /// True when no batch needs draining.
    pub fn is_empty(&self) -> Result<bool, SpoolError> {
        Ok(self.pending()?.is_empty())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, SpoolError> {
        // A poisoned mutex means a previous holder panicked; the data is still
        // consistent (the panic happened after our file ops), so recover the
        // guard rather than failing the whole spool.
        Ok(self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    fn batch_path(&self, seq: u64) -> PathBuf {
        self.dir
            .join(BATCH_DIR)
            .join(format!("{seq:0width$}.json", width = SEQ_WIDTH))
    }
}

/// Write `bytes` atomically (temp + `sync_all` + rename + dir sync). The data
/// is on disk before the batch filename exists, which is what makes the spool
/// crash-safe: a crash either leaves only the temp fragment (discarded on
/// [recovery](Spool::open)) or the fully-committed batch file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("batch path has no parent"))?;
    fs::create_dir_all(parent)?;
    let leaf = path.file_name().and_then(|n| n.to_str()).unwrap_or("batch");
    let tmp = parent.join(format!(".{leaf}.tmp"));

    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?; // data reachable before the rename
    fs::rename(&tmp, path)?;
    sync_dir(parent)?;
    Ok(())
}

/// Best-effort fsync of a directory so the batch rename itself is durable.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Parse `NNN.json` batch filenames into a sequence number. Temp fragments
/// (`.NNN.json.tmp`) never parse — they start with a dot, so they are never
/// mistaken for committed batches.
fn parse_batch_filename(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') {
        return None;
    }
    let stem = name.strip_suffix(".json")?;
    stem.parse().ok()
}

fn read_highwater(path: &Path) -> Result<u64> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .trim()
            .parse()
            .with_context(|| format!("corrupt high-water mark {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_highwater(path: &Path, value: u64) -> Result<()> {
    write_atomic(path, value.to_string().as_bytes())?;
    Ok(())
}

fn scan_batches(batch_dir: &Path, highwater: u64) -> Result<(u64, u64, u64), SpoolError> {
    let mut max_seq: Option<u64> = None;
    let mut pending_bytes = 0u64;
    let mut pending_batches = 0u64;
    for entry in fs::read_dir(batch_dir).map_err(SpoolError::Io)? {
        let entry = entry.map_err(SpoolError::Io)?;
        let Some(seq) = parse_batch_filename(&entry.path()) else {
            continue;
        };
        max_seq = Some(max_seq.map_or(seq, |max| max.max(seq)));
        if seq >= highwater {
            let size = entry.metadata().map_err(SpoolError::Io)?.len();
            pending_bytes = pending_bytes.saturating_add(size);
            pending_batches = pending_batches.saturating_add(1);
        }
    }
    // Fresh spool (no committed batch) starts at the high-water mark, not 0:
    // after a fully-drained restart the batch directory is empty but the
    // mark persists, and reusing sequences below it would strand new batches
    // beneath the drain window forever. Otherwise the next sequence continues
    // above the highest committed batch (floored at the mark, defensively).
    let next_seq = max_seq.map_or(highwater, |max| max.saturating_add(1).max(highwater));
    Ok((next_seq, pending_bytes, pending_batches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_event(id: &str) -> NewSessionEvent {
        NewSessionEvent {
            event_id: id.to_string(),
            kind: sessionmunch_core::WorkstreamEventKind::Message,
            role: Some("user".into()),
            content: format!("content-{id}"),
            occurred_at: Some("2026-10-02T00:00:00Z".into()),
            source: sessionmunch_core::SessionSource::Hook,
            metadata: serde_json::json!({"model": "deepseek-v4"}),
        }
    }

    fn batch(id: &str, events: Vec<NewSessionEvent>) -> SpoolBatch {
        SpoolBatch::new("acme", "app", id, "hermes", events)
    }

    fn open_spool(dir: &Path, max: u64) -> Spool {
        Spool::open(&SpoolConfig {
            dir: dir.to_path_buf(),
            max_spool_bytes: max,
        })
        .unwrap()
    }

    #[test]
    fn enqueue_then_pending_replays_in_order() {
        let tmp = tempdir().unwrap();
        let spool = open_spool(tmp.path(), 0);
        spool
            .enqueue(&batch("s1", vec![sample_event("e1")]))
            .unwrap();
        spool
            .enqueue(&batch("s1", vec![sample_event("e2")]))
            .unwrap();
        spool
            .enqueue(&batch("s2", vec![sample_event("e3")]))
            .unwrap();

        let pending = spool.pending().unwrap();
        assert_eq!(pending.len(), 3);
        let seqs: Vec<u64> = pending.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [0, 1, 2]);
        assert_eq!(pending[0].batch.session_id, "s1");
        assert_eq!(pending[2].batch.session_id, "s2");
        assert!(!spool.is_empty().unwrap());

        let m = spool.metrics();
        assert_eq!(m.pending_batches, 3);
        assert!(m.pending_bytes > 0);
        assert_eq!(m.highwater, 0);
    }

    #[test]
    fn ack_advances_highwater_and_reclaims_disk() {
        let tmp = tempdir().unwrap();
        let spool = open_spool(tmp.path(), 0);
        for i in 0..5 {
            spool
                .enqueue(&batch(
                    &format!("s{i}"),
                    vec![sample_event(&format!("e{i}"))],
                ))
                .unwrap();
        }
        let before = spool.metrics().pending_bytes;
        assert!(before > 0);

        // ACK through seq 1 (i.e. past the first two sequences).
        let deleted = spool.ack(1).unwrap();
        assert_eq!(deleted, 2);
        let pending = spool.pending().unwrap();
        let seqs: Vec<u64> = pending.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [2, 3, 4]);
        let m = spool.metrics();
        assert_eq!(m.highwater, 2);
        assert!(m.pending_bytes < before);

        // ACKing again is idempotent.
        assert_eq!(spool.ack(1).unwrap(), 0);
        assert_eq!(spool.metrics().highwater, 2);

        // ACK everything: spool empties and bytes reach zero.
        let deleted = spool.ack(100).unwrap();
        assert_eq!(deleted, 3);
        assert!(spool.is_empty().unwrap());
        let m = spool.metrics();
        assert_eq!(m.pending_batches, 0);
        assert_eq!(m.pending_bytes, 0);
    }

    #[test]
    fn reopen_after_clean_close_replays_same_pending_batches() {
        let tmp = tempdir().unwrap();
        {
            let spool = open_spool(tmp.path(), 0);
            spool
                .enqueue(&batch("s1", vec![sample_event("e1"), sample_event("e2")]))
                .unwrap();
        }
        // Fresh handle on the same directory must see the persisted batch.
        let spool = open_spool(tmp.path(), 0);
        let pending = spool.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].batch.events.len(), 2);
        assert_eq!(pending[0].seq, 0);
        // next_seq continues after the recovered max.
        spool
            .enqueue(&batch("s2", vec![sample_event("e3")]))
            .unwrap();
        let seqs: Vec<u64> = spool.pending().unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [0, 1]);
    }

    /// Simulated crash: an enqueue is interrupted halfway through its atomic
    /// write, leaving a `.tmp` fragment and nothing else. This is the exact
    /// on-disk state a `kill -9` mid-write produces. Recovery must discard
    /// the fragment and keep every previously committed batch.
    #[test]
    fn sim_crash_mid_write_recovers_uncorrupted() {
        let tmp = tempdir().unwrap();
        let config = SpoolConfig {
            dir: tmp.path().to_path_buf(),
            max_spool_bytes: 0,
        };
        {
            let spool = Spool::open(&config).unwrap();
            // Committed before the crash.
            spool
                .enqueue(&batch("s1", vec![sample_event("e1")]))
                .unwrap();
            spool
                .enqueue(&batch("s1", vec![sample_event("e2")]))
                .unwrap();
        }
        // Interrupted second enqueue: a partial temp fragment is left in the
        // batch dir (exactly what atomic_write produces before rename).
        let batch_dir = tmp.path().join(BATCH_DIR);
        fs::write(
            batch_dir.join(".00000000000000000002.json.tmp"),
            b"{\"partial",
        )
        .unwrap();

        let spool = Spool::open(&config).unwrap();
        let pending = spool.pending().unwrap();
        // The fragment was discarded; the two committed batches survive.
        assert_eq!(pending.len(), 2, "commit must survive, torn tail discarded");
        assert_eq!(pending[0].batch.events[0].event_id, "e1");
        assert_eq!(pending[1].batch.events[0].event_id, "e2");
        // next_seq resumes above the highest committed seq (1), not the torn
        // fragment (2), so enqueueing reuses the freed slot ordering safely.
        let seq = spool
            .enqueue(&batch("s2", vec![sample_event("e3")]))
            .unwrap();
        assert_eq!(seq, 2);
        assert!(!batch_dir.join(".00000000000000000002.json.tmp").exists());
    }

    #[test]
    fn max_spool_bytes_triggers_backpressure_and_metric() {
        let tmp = tempdir().unwrap();
        // Enqueue one batch to size it, then pick a cap that fits exactly
        // that number of batches and rejects one more.
        let probe = batch("s1", vec![sample_event("e1")]);
        let one_len = serde_json::to_vec(&probe).unwrap().len() as u64;

        let spool = open_spool(tmp.path(), one_len * 3);
        // Three fit, the fourth bumps past 3x one_len.
        for i in 0..3 {
            spool
                .enqueue(&batch(
                    &format!("s{i}"),
                    vec![sample_event(&format!("e{i}"))],
                ))
                .unwrap();
        }
        let err = spool
            .enqueue(&batch("s9", vec![sample_event("e9")]))
            .unwrap_err();
        assert!(matches!(err, SpoolError::Backpressure { .. }));
        assert_eq!(spool.metrics().backpressure_rejections, 1);
        // The spool still holds exactly the three accepted batches.
        assert_eq!(spool.pending().unwrap().len(), 3);
        assert_eq!(spool.metrics().pending_batches, 3);
    }

    #[test]
    fn unbounded_spool_accepts_many_batches() {
        let tmp = tempdir().unwrap();
        let spool = open_spool(tmp.path(), 0);
        for i in 0..500 {
            spool
                .enqueue(&batch(
                    &format!("s{i}"),
                    vec![sample_event(&format!("e{i}"))],
                ))
                .unwrap();
        }
        assert_eq!(spool.pending().unwrap().len(), 500);
        assert_eq!(spool.metrics().pending_batches, 500);
    }

    #[test]
    fn unrelated_tmp_files_are_ignored_not_mistaken_for_batches() {
        let tmp = tempdir().unwrap();
        let config = SpoolConfig {
            dir: tmp.path().to_path_buf(),
            max_spool_bytes: 0,
        };
        let spool = Spool::open(&config).unwrap();
        spool
            .enqueue(&batch("s1", vec![sample_event("e1")]))
            .unwrap();
        // A temp fragment that lives in the spool root (not the batch dir)
        // must not be mistaken for a batch.
        fs::write(tmp.path().join(".highwater.tmp"), "7").unwrap();
        assert_eq!(spool.pending().unwrap().len(), 1);
        // Its contents must be a valid real batch, not padding.
        let p = spool.pending().unwrap();
        assert_eq!(p[0].batch.events[0].content, "content-e1");
    }

    #[test]
    fn quarantine_moves_poison_aside_and_advances_highwater() {
        let tmp = tempdir().unwrap();
        let spool = open_spool(tmp.path(), 0);
        for i in 0..3 {
            spool
                .enqueue(&batch(
                    &format!("s{i}"),
                    vec![sample_event(&format!("e{i}"))],
                ))
                .unwrap();
        }

        assert!(spool.quarantine(0).unwrap(), "first quarantine applies");
        assert!(
            !spool.quarantine(0).unwrap(),
            "repeat quarantine is a no-op"
        );
        assert_eq!(spool.quarantined_seqs(), vec![0]);
        // The dead-letter file survives for operator inspection.
        assert!(
            tmp.path()
                .join("quarantine")
                .join(format!("{:020}.json", 0))
                .exists()
        );
        // The tail is still pending behind the advanced mark.
        let seqs: Vec<u64> = spool.pending().unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!(spool.metrics().highwater, 1);
        assert_eq!(spool.metrics().pending_batches, 2);

        // Reopening keeps the quarantine and the mark (crash-safe).
        let reopened = open_spool(tmp.path(), 0);
        assert_eq!(reopened.quarantined_seqs(), vec![0]);
        let seqs: Vec<u64> = reopened.pending().unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [1, 2]);
    }

    #[test]
    fn reopen_after_full_drain_assigns_sequences_above_highwater() {
        let tmp = tempdir().unwrap();
        let config = SpoolConfig {
            dir: tmp.path().to_path_buf(),
            max_spool_bytes: 0,
        };
        {
            let spool = Spool::open(&config).unwrap();
            for i in 0..3 {
                spool
                    .enqueue(&batch(
                        &format!("s{i}"),
                        vec![sample_event(&format!("e{i}"))],
                    ))
                    .unwrap();
            }
            spool.ack(2).unwrap();
            assert!(spool.is_empty().unwrap());
            assert_eq!(spool.metrics().highwater, 3);
        }
        // Reopen with an empty batch dir but a persisted mark: the next batch
        // must land at the mark, never at a reused seq below the drain window.
        let spool = Spool::open(&config).unwrap();
        let seq = spool
            .enqueue(&batch("s9", vec![sample_event("e9")]))
            .unwrap();
        assert_eq!(seq, 3);
        let pending = spool.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, 3);
        assert_eq!(spool.metrics().pending_batches, 1);
    }
}
