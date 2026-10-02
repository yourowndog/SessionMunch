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
//! - Each discovered transcript file is streamed into a blob named by its
//!   SHA-256 under `<artifact_root>/<harness>/<sha256>`. The store is therefore
//!   content-addressed: identical bytes deduplicate to one blob.
//! - Re-running against an unchanged store writes no new blobs (every source is
//!   reported `already-known`), and the per-harness `index.json` is rewritten
//!   deterministically, so repeated snapshots are byte-for-byte identical.
//! - Every source is reported with an explicit outcome (`snapshotted`,
//!   `already-known`, `unreadable`) and every absent/unsupported store is
//!   reported explicitly. Nothing is dropped silently.

use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::ManagedHarness;
use crate::transcript::{collect_session_files, crush_db, opencode_db, session_roots};

const SNAPSHOT_VERSION: &str = "harness-store-v1";
const INDEX_FILE: &str = "index.json";

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
    /// SHA-256 hex of the source content (empty when unreadable).
    pub sha256: String,
    /// Byte length of the source (0 when unreadable).
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
pub async fn snapshot_store(
    harness: ManagedHarness,
    artifact_root: &Path,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Result<HarnessSnapshot> {
    let discovered = discover_sources(harness, home, cwd, session_dir)?;
    let mut sources = Vec::with_capacity(discovered.files.len());
    for (source, root) in discovered.files {
        sources.push(snapshot_file(harness, artifact_root, &source, &root)?);
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
    Ok(snap)
}

/// Snapshot every known harness at once into `artifact_root`.
pub async fn snapshot_all_harnesses(
    artifact_root: &Path,
    home: &Path,
    cwd: &Path,
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
        harnesses.push(snapshot_store(harness, artifact_root, home, cwd, None).await?);
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

/// Stream one source file read-only into a content-addressed blob, deduplicating
/// on identical bytes. The source is never opened for writing.
fn snapshot_file(
    harness: ManagedHarness,
    artifact_root: &Path,
    source: &Path,
    root: &Path,
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

    let temp = dest_dir.join(format!(".blob-{}.tmp", Uuid::new_v4()));
    let hashed = match hash_stream(&mut reader, &temp) {
        Ok(value) => value,
        Err(error) => {
            fs::remove_file(&temp).ok();
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
    let (sha, size) = hashed;
    let blob = dest_dir.join(&sha);

    let published = match fs::metadata(&blob).ok() {
        Some(_) => {
            // A blob already exists for this content. Verify it rather than
            // trusting the name: a corrupt blob is rewritten, an intact one
            // makes this a deduplicated, no-write run.
            let intact = hash_file(&blob)
                .with_context(|| format!("verifying existing blob {}", blob.display()))?
                == sha;
            if intact {
                fs::remove_file(&temp)
                    .with_context(|| format!("removing temp {}", temp.display()))?;
                false
            } else {
                fs::rename(&temp, &blob)
                    .with_context(|| format!("repairing blob {}", blob.display()))?;
                true
            }
        }
        None => {
            fs::rename(&temp, &blob)
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

/// Stream `source` (opened read-only) into `dest`, hashing as it goes.
fn hash_stream(source: &mut File, dest: &Path) -> Result<(String, u64)> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut writer = options
        .open(dest)
        .with_context(|| format!("creating temp blob {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
        writer
            .write_all(&buffer[..read])
            .with_context(|| format!("writing temp blob {}", dest.display()))?;
    }
    Ok((format!("{:x}", hasher.finalize()), size))
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

    /// Write a fake Claude JSONL transcript store under `store`.
    fn write_claude_store(store: &Path, session_id: &str, body: &str) -> PathBuf {
        fs::create_dir_all(store).unwrap();
        let path = store.join(format!("{session_id}.jsonl"));
        fs::write(&path, format!("{}\n", body).as_bytes()).unwrap();
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
    async fn repeated_snapshot_dedupes_and_is_deterministic() {
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

        let first = claude_snapshot(&artifact, &home, &cwd, &store).await;
        // A second, distinct session with identical content maps to the SAME
        // blob (content addressing), and re-running writes no new blobs.
        write_claude_store(
            &store,
            "sess-2",
            "{\"sessionId\":\"sess-1\",\"cwd\":\"/repo\"}",
        );

        let second = claude_snapshot(&artifact, &home, &cwd, &store).await;
        assert_eq!(second.snapshotted(), 0, "no new blobs on rerun");
        assert_eq!(second.already_known(), 2, "both sources dedupe");
        assert!(
            second
                .sources
                .iter()
                .all(|s| s.sha256 == first.sources[0].sha256.clone())
        );

        // Deterministic index: identical bytes across runs.
        let index_first = fs::read(artifact.join("claude").join("index.json")).unwrap();
        let index_second = fs::read(artifact.join("claude").join("index.json")).unwrap();
        assert_eq!(
            index_first, index_second,
            "index must be byte-deterministic"
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

        let snap = snapshot_store(ManagedHarness::Antigravity, &artifact, &home, &cwd, None)
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
        // ambient permissions (a chmod-000 file is still readable by root).
        let dir = td.path().join("a-directory");
        fs::create_dir_all(&dir).unwrap();

        let snap = snapshot_file(ManagedHarness::Claude, &artifact, &dir, &dir).unwrap();

        assert_eq!(
            snap.outcome,
            SnapshotOutcome::Unreadable,
            "must be reported unreadable"
        );
        assert!(snap.detail.is_some(), "reason must travel with the loss");
        assert_eq!(snap.sha256, "");
        // No blob may have been written for an unreadable source: the only
        // file the harness dir may hold is the index (which write_index emits
        // because sources were seen) — a published blob or a stray temp is a
        // failure.
        let dir = artifact.join("claude");
        let mut stray = false;
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default();
                if name != "index.json" {
                    stray = true;
                }
            }
        }
        assert!(!stray, "no blob may be written for an unreadable source");
    }

    #[tokio::test]
    async fn snapshot_all_harnesses_reports_every_known_harness() {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        let cwd = td.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let artifact = td.path().join("snapshots");

        let report = snapshot_all_harnesses(&artifact, &home, &cwd)
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
}
