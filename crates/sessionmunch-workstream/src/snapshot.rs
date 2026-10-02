//! Read-only, content-addressed snapshotting of perishable native harness
//! transcript stores.
//!
//! The North Star (docs/SESSIONMUNCH-NORTH-STAR.md, §5.9) calls for an
//! emergency preservation step: before parsing/import design is complete, copy
//! the native harness stores into hashed, read-only artifacts so a future
//! importer can re-read the original bytes. This is a preservation step, not a
//! claim that the content has been interpreted — it deliberately reuses the
//! existing transcript discovery/readers instead of building a parallel history
//! product.
//!
//! Design:
//! - Sources are opened read-only and never modified. Nothing in this module
//!   opens a harness store for writing.
//! - North Star invariant 7 (sanitize-before-persistence) is enforced at the
//!   artifact boundary: the canonical [`sessionmunch_core::Sanitizer`] is applied
//!   to a source's bytes before they are hashed, written, deduplicated, or
//!   indexed. Text transcripts are scrubbed as UTF-8; SQLite-backed stores
//!   (OpenCode/Crush) are scrubbed per text cell via a transactionally
//!   consistent online-backup copy, so the preserved database stays valid and
//!   reopenable.
//! - SQLite-backed stores are snapshotted transactionally: the online backup
//!   API copies the live database (including committed WAL-resident data) so a
//!   raw `opencode.db`/`crush.db` file copy never silently drops rows that live
//!   only in the WAL.
//! - Each discovered transcript file is stored as a blob named by the SHA-256 of
//!   its *sanitized* bytes under `<artifact_root>/<harness>/<sha256>`. The store
//!   is therefore content-addressed: identical sanitized bytes deduplicate to
//!   one blob.
//! - Re-running against an unchanged store writes no new blobs (every source is
//!   reported `already-known`), and the per-harness `index.json` is rewritten
//!   deterministically, so repeated snapshots are byte-for-byte identical.
//! - Every source is reported with an explicit outcome (`snapshotted`,
//!   `already-known`, `unreadable`) and every absent/unsupported store is
//!   reported explicitly. Nothing is dropped silently.
//! - Durable provenance — machine identity, harness, absolute source/store
//!   identity, capture time, and best-effort repository HEAD/branch — is
//!   appended to a per-run ledger so snapshots stay attributable and replayable
//!   across machines and runs without disturbing deterministic content identity.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use rusqlite::{Connection, DatabaseName, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sessionmunch_core::Sanitizer;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::ManagedHarness;
use crate::repository::inspect_repository;
use crate::transcript::{collect_session_files, crush_db, opencode_db, session_roots};

const SNAPSHOT_VERSION: &str = "harness-store-v1";
const INDEX_FILE: &str = "index.json";
const PROVENANCE_LEDGER: &str = "provenance.jsonl";

/// Why a single source file was or was not preserved.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum SnapshotOutcome {
    /// A new content-addressed blob was written for this source.
    Snapshotted,
    /// A blob for this exact content already existed; nothing was written.
    AlreadyKnown,
    /// The source could not be opened or read. Reported explicitly — never
    /// silent.
    Unreadable,
}

/// One source file and its snapshot verdict.
#[derive(Debug)]
pub struct SnapshotSource {
    /// Absolute source path as reported.
    pub source: String,
    /// Path relative to the harness store root (stable across machines).
    pub rel: String,
    /// SHA-256 hex of the sanitized source content (empty when unreadable).
    pub sha256: String,
    /// Byte length of the sanitized source (0 when unreadable).
    pub size: u64,
    /// How this source was handled.
    pub outcome: SnapshotOutcome,
    /// Human-readable reason for `unreadable` sources.
    pub detail: Option<String>,
}

/// Snapshot verdict for one harness's transcript store.
#[derive(Debug)]
pub struct HarnessSnapshot {
    /// Harness whose store this snapshot covers.
    pub harness: ManagedHarness,
    /// Store roots that exist and were scanned.
    pub store_roots: Vec<String>,
    /// Expected store paths that were absent (potential history loss).
    pub missing_roots: Vec<String>,
    /// Per-source outcomes.
    pub sources: Vec<SnapshotSource>,
    /// Non-`None` when this harness's store cannot be snapshotted at all.
    pub unsupported: Option<String>,
}

impl HarnessSnapshot {
    /// Number of sources preserved with a freshly-written blob on this run.
    #[must_use]
    pub fn snapshotted(&self) -> usize {
        self.sources
            .iter()
            .filter(|s| s.outcome == SnapshotOutcome::Snapshotted)
            .count()
    }

    /// Number of sources whose content was already preserved.
    #[must_use]
    pub fn already_known(&self) -> usize {
        self.sources
            .iter()
            .filter(|s| s.outcome == SnapshotOutcome::AlreadyKnown)
            .count()
    }

    /// Number of sources that could not be read (explicit, never silent).
    #[must_use]
    pub fn unreadable(&self) -> usize {
        self.sources
            .iter()
            .filter(|s| s.outcome == SnapshotOutcome::Unreadable)
            .count()
    }

    /// Whether this harness was fully preserved ("clean" run).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.unsupported.is_none() && self.missing_roots.is_empty() && self.unreadable() == 0
    }
}

/// Aggregate snapshot verdict across every known harness.
#[derive(Debug)]
pub struct SnapshotReport {
    /// One verdict per known harness.
    pub harnesses: Vec<HarnessSnapshot>,
}

impl SnapshotReport {
    /// Sources with freshly-written blobs across all harnesses.
    #[must_use]
    pub fn snapshotted_total(&self) -> usize {
        let mut total = 0;
        for snap in &self.harnesses {
            total += snap.snapshotted();
        }
        total
    }

    /// Sources already preserved across all harnesses.
    #[must_use]
    pub fn already_known_total(&self) -> usize {
        let mut total = 0;
        for snap in &self.harnesses {
            total += snap.already_known();
        }
        total
    }

    /// Sources that could not be read across all harnesses.
    #[must_use]
    pub fn unreadable_total(&self) -> usize {
        let mut total = 0;
        for snap in &self.harnesses {
            total += snap.unreadable();
        }
        total
    }
}

/// Snapshot one harness's transcript store into content-addressed blobs under
/// `artifact_root`. Source files are opened read-only and never modified.
///
/// `machine_id` is a stable machine identifier recorded into durable
/// provenance. `sanitizer` is the canonical privacy strip applied before any
/// source bytes are hashed, written, deduplicated, or indexed.
pub async fn snapshot_store(
    harness: ManagedHarness,
    artifact_root: &Path,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    machine_id: &str,
    sanitizer: &Sanitizer,
) -> Result<HarnessSnapshot> {
    let discovered = discover_sources(harness, home, cwd, session_dir)?;
    let mut sources = Vec::with_capacity(discovered.files.len());
    for (source, root) in discovered.files {
        sources.push(snapshot_source(
            harness,
            artifact_root,
            &source,
            &root,
            sanitizer,
        )?);
    }
    let present_roots = discovered
        .present_roots
        .iter()
        .map(|root| root.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let snap = HarnessSnapshot {
        harness,
        store_roots: present_roots,
        missing_roots: discovered.missing_store,
        sources,
        unsupported: discovered.unsupported,
    };
    write_index(&snap, artifact_root)?;
    append_provenance(&snap, artifact_root, cwd, machine_id)?;
    Ok(snap)
}

/// Snapshot every known harness at once into `artifact_root`.
pub async fn snapshot_all_harnesses(
    artifact_root: &Path,
    home: &Path,
    cwd: &Path,
    machine_id: &str,
    sanitizer: &Sanitizer,
) -> Result<SnapshotReport> {
    let mut harnesses = Vec::with_capacity(13);
    for harness in [
        ManagedHarness::Claude,
        ManagedHarness::Codex,
        ManagedHarness::OpenCode,
        ManagedHarness::OpenCode2,
        ManagedHarness::Pi,
        ManagedHarness::Crush,
        ManagedHarness::Omp,
        ManagedHarness::Kimi,
        ManagedHarness::CommandCode,
        ManagedHarness::Kiro,
        ManagedHarness::KiroV3,
        ManagedHarness::Grok,
        ManagedHarness::Antigravity,
    ] {
        harnesses.push(
            snapshot_store(
                harness,
                artifact_root,
                home,
                cwd,
                None,
                machine_id,
                sanitizer,
            )
            .await?,
        );
    }
    Ok(SnapshotReport { harnesses })
}

/// The files that make up one harness's transcript store, plus which expected
/// roots are absent and whether the store is unsupported at all.
struct Discovered {
    /// `(source file, store root to relativize against)`.
    files: Vec<(PathBuf, PathBuf)>,
    present_roots: Vec<PathBuf>,
    missing_store: Vec<String>,
    unsupported: Option<String>,
}

fn discover_sources(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Result<Discovered> {
    match harness {
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
            introduce_single_file_store(&opencode_db(home, session_dir))
        }
        ManagedHarness::Crush => introduce_single_file_store(&crush_db(cwd, session_dir)),
        ManagedHarness::Antigravity => Ok(Discovered {
            files: Vec::new(),
            present_roots: Vec::new(),
            missing_store: Vec::new(),
            unsupported: Some(
                "per-conversation SQLite stores are id-addressed; snapshotting them requires a known conversation id".into()
            ),
        }),
        _ => {
            let roots = session_roots(harness, home, session_dir);
            let primary = roots
                .first()
                .map(|root| root.to_path_buf())
                .unwrap_or_else(|| home.to_path_buf());
            let mut present_roots = Vec::new();
            let mut missing_store = Vec::new();
            for root in &roots {
                if root.is_dir() {
                    present_roots.push(root.to_path_buf());
                } else {
                    missing_store.push(root.to_string_lossy().to_string());
                }
            }
            let files = collect_session_files(harness, home, session_dir)?
                .iter()
                .map(|path| (path.to_path_buf(), primary.clone()))
                .collect::<Vec<_>>();
            Ok(Discovered {
                files,
                present_roots,
                missing_store,
                unsupported: None,
            })
        }
    }
}

/// A store that lives in one SQLite file: present → one source, absent →
/// one explicit missing store.
fn introduce_single_file_store(db: &Path) -> Result<Discovered> {
    if db.is_file() {
        let root = db
            .parent()
            .map(|path| path.to_path_buf())
            .unwrap_or_else(|| db.to_path_buf());
        Ok(Discovered {
            files: vec![(db.to_path_buf(), root.clone())],
            present_roots: vec![root],
            missing_store: Vec::new(),
            unsupported: None,
        })
    } else {
        Ok(Discovered {
            files: Vec::new(),
            present_roots: Vec::new(),
            missing_store: vec![db.to_string_lossy().to_string()],
            unsupported: None,
        })
    }
}

/// Whether this harness's transcript store is the OpenCode/Crush SQLite
/// database, which must be snapshotted transactionally (online backup) rather
/// than as a raw file copy.
fn is_sqlite_backed(harness: ManagedHarness) -> bool {
    matches!(
        harness,
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 | ManagedHarness::Crush
    )
}

/// Produce one source's snapshot verdict. Routes OpenCode/Crush SQLite stores
/// through the transactional online-backup path; all other harnesses stream
/// their text bytes directly. Both paths sanitize before hashing/persistence.
fn snapshot_source(
    harness: ManagedHarness,
    artifact_root: &Path,
    source: &Path,
    root: &Path,
    sanitizer: &Sanitizer,
) -> Result<SnapshotSource> {
    if is_sqlite_backed(harness) {
        snapshot_sqlite_store(harness, artifact_root, source, root, sanitizer)
    } else {
        snapshot_text_store(harness, artifact_root, source, root, sanitizer)
    }
}

/// Stream one text source file read-only into a content-addressed blob of its
/// sanitized bytes, deduplicating on identical sanitized content. The source
/// is never opened for writing.
fn snapshot_text_store(
    harness: ManagedHarness,
    artifact_root: &Path,
    source: &Path,
    root: &Path,
    sanitizer: &Sanitizer,
) -> Result<SnapshotSource> {
    let source_text = source.to_string_lossy().to_string();
    let rel = relative_to(source, root);
    let dest_dir = artifact_root.join(harness.as_str());
    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating snapshot dir {}", dest_dir.display()))?;

    let mut reader = match File::open(source) {
        Ok(file) => file,
        Err(_) => {
            return Ok(SnapshotSource {
                source: source_text,
                rel,
                sha256: String::new(),
                size: 0,
                outcome: SnapshotOutcome::Unreadable,
                detail: Some("open failed; source left untouched".into()),
            });
        }
    };

    let sanitized = match read_and_scrub(&mut reader, sanitizer) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(SnapshotSource {
                source: source_text,
                rel,
                sha256: String::new(),
                size: 0,
                outcome: SnapshotOutcome::Unreadable,
                detail: Some(format!("read failed: {error}")),
            });
        }
    };
    let sha = format!("{:x}", Sha256::digest(&sanitized));
    let size = sanitized.len() as u64;

    let temp = dest_dir.join(format!(".blob-{}.tmp", Uuid::new_v4()));
    fs::write(&temp, &sanitized)
        .with_context(|| format!("writing temp blob {}", temp.display()))?;
    let published = publish_blob(&dest_dir, &sha, &temp)?;

    Ok(SnapshotSource {
        source: source_text,
        rel,
        sha256: sha,
        size,
        outcome: if published {
            SnapshotOutcome::Snapshotted
        } else {
            SnapshotOutcome::AlreadyKnown
        },
        detail: None,
    })
}

/// Snapshot a SQLite-backed store (OpenCode/Crush) transactionally: the online
/// backup API produces a consistent, WAL-complete copy of the live database,
/// then its text content is scrubbed in place with the canonical sanitizer so
/// the preserved file carries no secrets and stays reopenable.
fn snapshot_sqlite_store(
    harness: ManagedHarness,
    artifact_root: &Path,
    source: &Path,
    root: &Path,
    sanitizer: &Sanitizer,
) -> Result<SnapshotSource> {
    let source_text = source.to_string_lossy().to_string();
    let rel = relative_to(source, root);
    let dest_dir = artifact_root.join(harness.as_str());
    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating snapshot dir {}", dest_dir.display()))?;

    let consistent = dest_dir.join(format!(".consistent-{}.db", Uuid::new_v4()));
    let (sha, size, published) = match (|| -> Result<(String, u64, bool)> {
        // Open the source read-only (never for writing) and run the online
        // backup API so committed WAL-resident rows are included.
        let connection = Connection::open_with_flags(
            source,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening SQLite store {} read-only", source.display()))?;
        connection
            .backup(DatabaseName::Main, &consistent, None)
            .with_context(|| format!("online-backup of {}", source.display()))?;

        // Scrub every text cell in the consistent copy, then hash, dedupe and
        // publish.
        sanitize_sqlite_text(&consistent, sanitizer)?;
        let sanitized = fs::read(&consistent)
            .with_context(|| format!("reading consistent snapshot {}", consistent.display()))?;
        let sha = format!("{:x}", Sha256::digest(&sanitized));
        let size = sanitized.len() as u64;
        let published = publish_blob(&dest_dir, &sha, &consistent)?;
        Ok((sha, size, published))
    })() {
        Ok(value) => value,
        Err(error) => {
            fs::remove_file(&consistent).ok();
            return Ok(SnapshotSource {
                source: source_text,
                rel,
                sha256: String::new(),
                size: 0,
                outcome: SnapshotOutcome::Unreadable,
                detail: Some(format!("SQLite snapshot failed: {error}")),
            });
        }
    };
    Ok(SnapshotSource {
        source: source_text,
        rel,
        sha256: sha,
        size,
        outcome: if published {
            SnapshotOutcome::Snapshotted
        } else {
            SnapshotOutcome::AlreadyKnown
        },
        detail: None,
    })
}

/// Sanitize a SQLite database's text content in place using the canonical
/// sanitizer. Text-affinity columns across every user table are scrubbed;
/// INTEGER/BLOB/REAL and primary-key columns are left untouched so the
/// database remains structurally valid and reopenable.
fn sanitize_sqlite_text(path: &Path, sanitizer: &Sanitizer) -> Result<()> {
    let connection = Connection::open(path)
        .with_context(|| format!("opening SQLite snapshot {} for scrubbing", path.display()))?;
    // Force writes straight into the main database file (not a rollback/WAL
    // journal) so the scrubbed copy is what we read back and publish.
    connection.pragma_update(None, "journal_mode", "DELETE")?;

    let table_names: Vec<String> = {
        let mut stmt = connection.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    for table in &table_names {
        let qt = quote_ident(table);
        // Column (id, name, type).
        let columns: Vec<(String, String)> = {
            let mut stmt = connection.prepare(&format!("PRAGMA table_info({qt})"))?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (column, declared_type) in columns {
            if !text_affinity(&declared_type) {
                continue;
            }
            let qc = quote_ident(&column);
            // Read candidate text cells, scrub, and write back with UPDATE.
            let select = format!("SELECT rowid, {qc} FROM {qt} WHERE typeof({qc}) = 'text'");
            let mut stmt = connection.prepare(&select)?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            if rows.is_empty() {
                continue;
            }
            let update = format!("UPDATE {qt} SET {qc} = ?1 WHERE rowid = ?2");
            let mut stmt = connection.prepare(&update)?;
            for (rowid, value) in rows {
                let scrubbed = sanitizer.scrub(&value);
                if scrubbed != value {
                    stmt.execute(params![scrubbed, rowid])?;
                }
            }
        }
    }
    Ok(())
}

/// Quote a SQLite identifier safely by doubling embedded double-quotes.
fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// SQLite affinity → is this a text column worth scrubbing?
fn text_affinity(declared: &str) -> bool {
    let upper = declared.trim().to_ascii_uppercase();
    upper.is_empty() || upper.contains("TEXT") || upper.contains("CHAR") || upper.contains("CLOB")
}

/// Read a whole source file, scrubbing its text through the canonical
/// sanitizer so secrets are removed before any hashing, writing,
/// deduplication, or indexing happens.
fn read_and_scrub(reader: &mut File, sanitizer: &Sanitizer) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    // Lossy UTF-8 keeps the artifact textual (JSONL) while guaranteeing the
    // scrubber never sees invalid UTF-8 bytes.
    let text = String::from_utf8_lossy(&bytes);
    Ok(sanitizer.scrub(&text).into_bytes())
}

/// Publish `temp` as `<dir>/<sha>`, deduplicating on identical sanitized
/// content. A pre-existing intact blob makes this a no-write run; a corrupt
/// named blob is repaired. Returns whether a new blob was written.
fn publish_blob(dir: &Path, sha: &str, temp: &Path) -> Result<bool> {
    let blob = dir.join(sha);
    let published = match fs::metadata(&blob).ok() {
        Some(_) => {
            // A blob already exists for this content. Verify it rather than
            // trusting the name: a corrupt blob is rewritten, an intact one
            // makes this a deduplicated, no-write run.
            let intact = hash_file(&blob)
                .with_context(|| format!("verifying existing blob {}", blob.display()))?
                == sha;
            if intact {
                fs::remove_file(temp)
                    .with_context(|| format!("removing temp {}", temp.display()))?;
                false
            } else {
                fs::rename(temp, &blob)
                    .with_context(|| format!("repairing blob {}", blob.display()))?;
                true
            }
        }
        None => {
            fs::rename(temp, &blob)
                .with_context(|| format!("publishing blob {}", blob.display()))?;
            true
        }
    };

    // Post-write integrity: the published blob must hash to its name.
    if published
        && hash_file(&blob)
            .with_context(|| format!("verifying published blob {}", blob.display()))?
            != sha
    {
        return Err(anyhow!(
            "snapshot integrity check failed for {}; re-run to repair",
            blob.display()
        ));
    }
    Ok(published)
}

/// SHA-256 hex of a file (used to verify stored blobs).
fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Path of a source relative to its store root.
fn relative_to(path: &Path, root: &Path) -> String {
    let text = path.to_string_lossy();
    let base = root.to_string_lossy();
    if let Some(rest) = text.strip_prefix(base.as_ref()) {
        rest.trim_start_matches('/').to_string()
    } else {
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or(text)
            .to_string()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotIndexEntry {
    rel: String,
    /// Absolute source identity (durable provenance, machine-local path).
    source: String,
    sha256: String,
    size: u64,
    blob: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotIndex {
    snapshot_version: String,
    harness: String,
    store_roots: Vec<String>,
    sources: Vec<SnapshotIndexEntry>,
}

/// Write (atomically, overwriting) the deterministic per-harness index that
/// maps preserved sources to their content-addressed blobs. Skipped entirely
/// for a fully unsupported harness (no store to index).
fn write_index(snap: &HarnessSnapshot, artifact_root: &Path) -> Result<()> {
    if snap.unsupported.is_some() && snap.sources.is_empty() {
        return Ok(());
    }
    let dir = artifact_root.join(snap.harness.as_str());
    let mut entries = snap
        .sources
        .iter()
        .filter(|s| s.outcome != SnapshotOutcome::Unreadable && !s.sha256.is_empty())
        .map(|s| SnapshotIndexEntry {
            rel: s.rel.clone(),
            source: s.source.clone(),
            sha256: s.sha256.clone(),
            size: s.size,
            blob: s.sha256.clone(),
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.rel.clone());
    let index = SnapshotIndex {
        snapshot_version: SNAPSHOT_VERSION.into(),
        harness: snap.harness.as_str().to_string(),
        store_roots: snap.store_roots.clone(),
        sources: entries,
    };
    let serialized = serde_json::to_string_pretty(&index)?;
    atomic_write(&dir.join(INDEX_FILE), serialized.as_bytes())?;
    Ok(())
}

/// Durable provenance for one snapshot run/harness: machine identity, harness,
/// absolute source/store identity, capture time, and best-effort repo
/// HEAD/branch. Appended to `<artifact_root>/provenance.jsonl` (newline
/// delimited) so every run is attributable without perturbing the
/// byte-deterministic `index.json`.
#[derive(Debug, Serialize)]
struct ProvenanceRecord {
    snapshot_version: String,
    machine_id: String,
    capture_time: String,
    harness: String,
    store_roots: Vec<String>,
    missing_roots: Vec<String>,
    unsupported: Option<String>,
    repo_head: Option<String>,
    repo_branch: Option<String>,
    sources: Vec<SnapshotIndexEntry>,
}

fn append_provenance(
    snap: &HarnessSnapshot,
    artifact_root: &Path,
    cwd: &Path,
    machine_id: &str,
) -> Result<()> {
    let (repo_head, repo_branch) = match inspect_repository(cwd) {
        Ok(identity) => (identity.checkpoint.head, identity.checkpoint.branch),
        Err(_) => (None, None),
    };
    let sources = snap
        .sources
        .iter()
        .filter(|s| s.outcome != SnapshotOutcome::Unreadable)
        .map(|s| SnapshotIndexEntry {
            rel: s.rel.clone(),
            source: s.source.clone(),
            sha256: s.sha256.clone(),
            size: s.size,
            blob: s.sha256.clone(),
        })
        .collect::<Vec<_>>();
    let record = ProvenanceRecord {
        snapshot_version: SNAPSHOT_VERSION.into(),
        machine_id: machine_id.to_string(),
        capture_time: jiff::Timestamp::now().to_string(),
        harness: snap.harness.as_str().to_string(),
        store_roots: snap.store_roots.clone(),
        missing_roots: snap.missing_roots.clone(),
        unsupported: snap.unsupported.clone(),
        repo_head,
        repo_branch,
        sources,
    };
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    let ledger = artifact_root.join(PROVENANCE_LEDGER);
    if let Some(parent) = ledger.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ledger)
        .with_context(|| format!("opening provenance ledger {}", ledger.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending provenance for {}", snap.harness.as_str()))?;
    Ok(())
}

/// Write bytes atomically (tmp + rename), like every other durable write in
/// the codebase.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(anyhow!("no parent directory for {}", path.display()));
    };
    fs::create_dir_all(parent)?;
    let leaf = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or(INDEX_FILE.into());
    let tmp = parent.join(format!(".{leaf}.tmp"));
    fs::write(&tmp, bytes)
        .with_context(|| format!("writing {} bytes to {}", bytes.len(), tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} over {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const TEST_MACHINE: &str = "test-machine";

    fn sanitizer() -> Sanitizer {
        Sanitizer::builtin()
    }

    /// Write a fake Claude JSONL transcript store under `store`.
    fn write_claude_store(store: &Path, session_id: &str, body: &str) -> PathBuf {
        fs::create_dir_all(store).unwrap();
        let path = store.join(format!("{session_id}.jsonl"));
        fs::write(&path, format!("{body}\n").as_bytes()).unwrap();
        path.to_path_buf()
    }

    async fn claude_snapshot(
        artifact_root: &Path,
        home: &Path,
        cwd: &Path,
        store: &Path,
    ) -> HarnessSnapshot {
        snapshot_store(
            ManagedHarness::Claude,
            artifact_root,
            home,
            cwd,
            Some(store),
            TEST_MACHINE,
            &sanitizer(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn snapshots_are_content_addressed_and_leave_sources_untouched() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let store = td.path().join("claude-store");
        write_claude_store(
            &store,
            "sess-1",
            "{\"sessionId\":\"sess-1\",\"cwd\":\"/repo\"}",
        );
        let artifact = td.path().join("snapshots");

        let before = fs::read(store.join("sess-1.jsonl")).unwrap();
        let modified_before = fs::metadata(store.join("sess-1.jsonl"))
            .unwrap()
            .modified()
            .unwrap();

        let snap = claude_snapshot(&artifact, &home, &cwd, &store).await;

        assert_eq!(snap.snapshotted(), 1, "one new blob");
        assert_eq!(snap.already_known(), 0);
        assert_eq!(snap.unreadable(), 0);
        assert!(snap.is_clean());

        let sha = snap.sources[0].sha256.clone();
        let blob = artifact.join("claude").join(&sha);
        assert!(blob.is_file(), "blob exists at {}", blob.display());
        // Content-addressed over the SANITIZED bytes (no secrets here, so the
        // sanitized bytes equal the source bytes).
        assert_eq!(sha, format!("{:x}", Sha256::digest(before.clone())));

        // Source bytes and mtime unchanged — the store was only ever read.
        let after = fs::read(store.join("sess-1.jsonl")).unwrap();
        assert_eq!(after, before, "source content must be untouched");
        assert_eq!(
            fs::metadata(store.join("sess-1.jsonl"))
                .unwrap()
                .modified()
                .unwrap(),
            modified_before,
            "source mtime must be untouched"
        );
        assert!(snap.sources[0].detail.is_none());
    }

    #[tokio::test]
    async fn repeated_snapshot_on_unchanged_store_is_idempotent_and_deterministic() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let store = td.path().join("claude-store");
        write_claude_store(
            &store,
            "sess-1",
            "{\"sessionId\":\"sess-1\",\"cwd\":\"/repo\"}",
        );
        let artifact = td.path().join("snapshots");

        let _first = claude_snapshot(&artifact, &home, &cwd, &store).await;
        let index_first = fs::read(artifact.join("claude").join("index.json")).unwrap();

        // Rerun an UNCHANGED source set: no new blobs, all already-known.
        let second = claude_snapshot(&artifact, &home, &cwd, &store).await;
        assert_eq!(second.snapshotted(), 0, "no new blobs on unchanged rerun");
        assert_eq!(second.already_known(), 1, "sole source dedupes");

        // Byte-identical index across the two runs.
        let index_second = fs::read(artifact.join("claude").join("index.json")).unwrap();
        assert_eq!(
            index_first, index_second,
            "index must be byte-identical across unchanged reruns"
        );

        // Blob count unchanged: exactly one blob for the one source.
        let dir = artifact.join("claude");
        let blob_count = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name != "index.json" && !name.starts_with('.')
            })
            .count();
        assert_eq!(blob_count, 1, "unchanged store rewrites no blobs");
    }

    #[tokio::test]
    async fn same_content_two_paths_dedupe_to_one_blob() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let store = td.path().join("claude-store");
        // Two distinct sessions with IDENTICAL content.
        write_claude_store(
            &store,
            "sess-1",
            "{\"sessionId\":\"sess-1\",\"cwd\":\"/repo\"}",
        );
        write_claude_store(
            &store,
            "sess-2",
            "{\"sessionId\":\"sess-1\",\"cwd\":\"/repo\"}",
        );
        let artifact = td.path().join("snapshots");

        let snap = claude_snapshot(&artifact, &home, &cwd, &store).await;
        // Same content on a second path dedupes: one fresh snapshot, one
        // already-known, one shared blob.
        assert_eq!(snap.snapshotted(), 1, "first path writes the blob");
        assert_eq!(snap.already_known(), 1, "second identical path dedupes");
        // Identical sanitized content → one content-addressed blob address.
        assert_eq!(
            snap.sources[0].sha256, snap.sources[1].sha256,
            "identical content must dedupe to one blob address"
        );
        let blob = artifact.join("claude").join(&snap.sources[0].sha256);
        assert!(blob.is_file());
        let dir = artifact.join("claude");
        let blob_count = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name != "index.json" && !name.starts_with('.')
            })
            .count();
        assert_eq!(blob_count, 1, "two identical sources share one blob");
    }

    #[tokio::test]
    async fn representative_secrets_never_appear_in_blobs_or_index() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let store = td.path().join("claude-store");
        let secret = "sk-ant-test1234567890abcdefghijklmn";
        let body = format!(
            "{{\"api_key\":\"{}\",\"cwd\":\"/repo\",\"note\":\"not a secret\"}}",
            secret
        );
        write_claude_store(&store, "sess-1", &body);
        let artifact = td.path().join("snapshots");

        let snap = claude_snapshot(&artifact, &home, &cwd, &store).await;
        assert_eq!(snap.snapshotted(), 1);

        // Scan every blob under the harness dir for the secret.
        let dir = artifact.join("claude");
        let mut leaked_blob = false;
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "index.json" {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap();
            if bytes.windows(secret.len()).any(|w| w == secret.as_bytes()) {
                leaked_blob = true;
                break;
            }
        }
        assert!(!leaked_blob, "secret must not appear in any stored blob");

        // Same scan over index.json.
        let index = fs::read(dir.join("index.json")).unwrap();
        assert!(
            !index.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "secret must not appear in index.json"
        );
    }

    #[tokio::test]
    async fn missing_store_is_reported_explicitly() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let missing = td.path().join("does-not-exist");
        let artifact = td.path().join("snapshots");

        let snap = claude_snapshot(&artifact, &home, &cwd, &missing).await;

        assert_eq!(snap.sources.len(), 0);
        assert!(!snap.is_clean());
        assert_eq!(snap.missing_roots.len(), 1, "absent store must be explicit");
    }

    #[tokio::test]
    async fn unsupported_harness_is_reported_not_silently_dropped() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let artifact = td.path().join("snapshots");

        let snap = snapshot_store(
            ManagedHarness::Antigravity,
            &artifact,
            &home,
            &cwd,
            None,
            TEST_MACHINE,
            &sanitizer(),
        )
        .await
        .unwrap();

        assert!(
            snap.unsupported.is_some(),
            "antigravity must be reported unsupported"
        );
        assert!(snap.sources.is_empty());
        assert!(!snap.is_clean());
    }

    #[tokio::test]
    async fn unreadable_source_is_reported_and_never_dropped() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let artifact = td.path().join("snapshots");

        // Opening a directory read-only succeeds on Linux but reading it fails
        // (EISDIR), which exercises the unreadable path without depending on
        // ambient permissions.
        let dir = td.path().join("a-directory");
        fs::create_dir_all(&dir).unwrap();

        let snap = snapshot_text_store(ManagedHarness::Claude, &artifact, &dir, &dir, &sanitizer())
            .unwrap();

        assert_eq!(
            snap.outcome,
            SnapshotOutcome::Unreadable,
            "must be reported unreadable"
        );
        assert!(snap.detail.is_some(), "reason must travel with the loss");
        assert_eq!(snap.sha256, "");
        // No blob may be written for an unreadable source (this low-level call
        // bypasses write_index and emits no index).
        let dir = artifact.join("claude");
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(
                    !name.starts_with(".blob-") && name != "index.json",
                    "stray artifact {name}"
                );
            }
        }
    }

    #[tokio::test]
    async fn snapshot_all_harnesses_reports_every_known_harness() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let artifact = td.path().join("snapshots");

        let report = snapshot_all_harnesses(&artifact, &home, &cwd, TEST_MACHINE, &sanitizer())
            .await
            .unwrap();
        // Every known harness is present, whether or not it had a store.
        assert_eq!(report.harnesses.len(), 13);
        // Every non-clean harness carries an explicit reason — nothing vanishes
        // without a trace in the report.
        assert!(
            report.harnesses.iter().all(|h| {
                h.is_clean()
                    || !h.missing_roots.is_empty()
                    || h.unsupported.is_some()
                    || h.unreadable() > 0
            }),
            "every non-clean harness must carry an explicit reason"
        );
    }

    /// Durable provenance: the run ledger records machine identity, harness,
    /// absolute source identity, and capture time without disturbing the
    /// byte-deterministic index.
    #[tokio::test]
    async fn provenance_ledger_records_machine_harness_and_capture_time() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let store = td.path().join("claude-store");
        write_claude_store(
            &store,
            "sess-1",
            "{\"sessionId\":\"sess-1\",\"cwd\":\"/repo\"}",
        );
        let artifact = td.path().join("snapshots");

        let snap = claude_snapshot(&artifact, &home, &cwd, &store).await;
        assert_eq!(snap.snapshotted(), 1);

        let ledger = artifact.join(PROVENANCE_LEDGER);
        assert!(ledger.is_file(), "provenance ledger must be written");
        let text = fs::read_to_string(&ledger).unwrap();
        let record: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(record["machine_id"], "test-machine");
        assert_eq!(record["harness"], "claude");
        assert!(record["capture_time"].is_string(), "capture time recorded");
        assert!(
            record["sources"][0]["source"]
                .as_str()
                .unwrap()
                .ends_with("sess-1.jsonl")
        );
    }

    /// A live-WAL regression: a committed row that exists only in the WAL
    /// (not yet checkpointed into the main database file) must survive into the
    /// preserved blob.
    #[tokio::test]
    async fn sqlite_store_preserves_committed_wal_data_transactionally() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let artifact = td.path().join("snapshots");

        // Build an OpenCode-style store where discovery expects opencode.db.
        let db_root = td.path().join("opencode-store");
        fs::create_dir_all(&db_root).unwrap();
        let db = db_root.join("opencode.db");

        // Create a WAL-mode database, commit a row, and keep the connection
        // open so the row is WAL-resident (not checkpointed into the main
        // opencode.db file).
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(
                "CREATE TABLE evidence (id INTEGER PRIMARY KEY, body TEXT);
                 INSERT INTO evidence (body) VALUES ('wal-resident-committed');",
            )
            .unwrap();

            let snap = snapshot_store(
                ManagedHarness::OpenCode,
                &artifact,
                &home,
                &cwd,
                Some(&db_root),
                TEST_MACHINE,
                &sanitizer(),
            )
            .await
            .unwrap();
            assert_eq!(snap.snapshotted(), 1, "one blob for the opencode store");
            assert_eq!(snap.unreadable(), 0);

            // The emitted blob, opened as a database, must contain the
            // committed WAL-resident row.
            let sha = snap.sources[0].sha256.clone();
            let blob = artifact.join("opencode").join(&sha);
            assert!(blob.is_file(), "blob exists at {}", blob.display());
            let check = Connection::open(&blob).unwrap();
            let count: i64 = check
                .query_row(
                    "SELECT COUNT(*) FROM evidence WHERE body = ?1",
                    params!["wal-resident-committed"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                count, 1,
                "committed WAL-resident row must be preserved in the blob"
            );
        }
    }
}
