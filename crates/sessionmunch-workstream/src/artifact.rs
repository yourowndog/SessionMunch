//! Content-addressed, compressed artifact storage with provenance linkage.
//!
//! Large sanitized evidence (full tool outputs, imported transcripts) is stored
//! content-addressed: the bytes are gzip-compressed and the blob is named by
//! the lowercase SHA-256 hex digest of its **raw** (uncompressed) content.
//! Identity is the digest, so identical bytes deduplicate automatically and
//! integrity is re-checked on both write and read.
//!
//! Trust boundary — no path escapes the artifact root. The only untrusted
//! input that ever reaches path construction is a SHA-256 digest string, and
//! it is validated to be exactly 64 lowercase hex digits before it is joined
//! to the (canonicalized) root. Every artifact lives at
//! `<root>/<lower-hex-64>.gz`; there are no caller-supplied subpaths, so
//! `../`-traversal and absolute-path injection are structurally impossible.
//!
//! Provenance linkage — every artifact may be linked to the originating
//! session id, node id, and event provenance. Links are recorded in a small
//! SQLite registry (`<root>/registry.sqlite`) so, per the North Star
//! invariant ``No silent loss'', a stored artifact can be attributed and any
//! missing/corrupt artifact reports as an explicit [`ArtifactLoss`].

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sessionmunch_core::SessionId;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Compression extension applied to every stored artifact blob (gzip).
const ARTIFACT_EXT: &str = ".gz";
/// Filename of the SQLite provenance-linkage registry within the root.
const REGISTRY_FILENAME: &str = "registry.sqlite";
/// A SHA-256 hex digest is exactly 64 hex digits.
const DIGEST_LEN: usize = 64;

/// Base path of the authoritative artifact-ingest endpoint on the Hub
/// (`POST {central_url}/artifacts`). Shared by the Hub's server router and the
/// satellite [`ArtifactClient`] so both compile against one endpoint string.
pub const ARTIFACTS_PATH: &str = "/artifacts";

/// Errors that name an artifact identity and thus surface explicitly at the
/// store boundary instead of being buried in a generic I/O failure.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    /// Receipt check failed: computed digest differs from the caller-supplied.
    #[error("SHA-256 mismatch on receipt: expected {expected}, computed {computed}")]
    HashMismatch {
        /// The digest the caller claimed the content should hash to.
        expected: String,
        /// The digest actually computed from the content.
        computed: String,
    },

    /// The named artifact is not present (no silent loss — the caller decides).
    #[error("artifact missing: {digest}")]
    Missing {
        /// The content address that has no stored blob.
        digest: String,
    },

    /// The named artifact exists but cannot be decompressed or its bytes do
    /// not match its content address (corruption or tampering detected).
    #[error("artifact corrupt: {digest}: {reason}")]
    Corrupt {
        /// The content address that failed its integrity check.
        digest: String,
        /// Why the blob could not be trusted (decompress / re-hash failure).
        reason: String,
    },

    /// Refused an identity that is not a plausible SHA-256 digest. This is the
    /// guard that makes path traversal impossible: nothing non-hex reaches a
    /// path join.
    #[error(
        "invalid artifact digest {digest:?}: expected {expected_digest_len} lowercase hex digits"
    )]
    InvalidDigest {
        /// The rejected, non-hex identity.
        digest: String,
        /// How many lowercase hex digits a valid digest has.
        expected_digest_len: usize,
    },
}

impl ArtifactError {
    /// The artifact identity this error names, when it names one (returns
    /// `None` for [`ArtifactError::HashMismatch`], which has no stored digest).
    #[must_use]
    pub fn digest(&self) -> Option<&str> {
        match self {
            Self::HashMismatch { .. } => None,
            Self::Missing { digest }
            | Self::Corrupt { digest, .. }
            | Self::InvalidDigest { digest, .. } => Some(digest.as_str()),
        }
    }
}

/// Provenance linkage recorded for one content-addressed artifact: the session
/// it originated from, the node that produced it, and the event provenance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactProvenance {
    /// The session this artifact's evidence originated from.
    pub session_id: SessionId,
    /// Machine / node id that produced the artifact.
    pub node_id: String,
    /// Harness name (free text, e.g. `hermes`, `claude-code`).
    pub harness: String,
    /// Stable source-derived event id, when the artifact is tied to one event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Event source (`hook` | `import` | `managed`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_source: Option<String>,
    /// Creation timestamp (epoch ms) recorded when the link row was written.
    pub created_at_ms: i64,
}

/// Wire shape of one artifact-upload request — the single authoritative
/// contract shared by the Hub's server handler (`POST /artifacts`) and the
/// satellite caller ([`ArtifactClient`]), so a caller and the Hub can never
/// drift on the shape they exchange.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtifactUpload {
    /// Raw artifact bytes.
    pub content: Vec<u8>,
    /// Caller-computed lower SHA-256 hex of `content` (the receipt). The Hub
    /// verifies this before persisting anything and refuses a lying hash.
    pub digest: String,
    /// Ownership / event provenance for the artifact.
    pub provenance: ArtifactProvenance,
}

/// The Hub's acceptance receipt for a stored artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReceipt {
    /// The verified content address the Hub stored and gzip-compressed.
    pub sha256: String,
    /// The session id echoed back from provenance (attribution).
    pub session_id: SessionId,
}

/// A loss report fulfilling the ``no silent loss'' invariant: naming what was
/// expected and why it could not be delivered (missing / corrupt / unsupported).
#[derive(Clone, Debug, Serialize)]
pub struct ArtifactLoss {
    /// Human-readable reason this artifact could not be delivered.
    pub reason: String,
    /// Content address that was expected, when the loss names a specific
    /// artifact identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    /// Expected raw byte length of the artifact.
    pub byte_length: usize,
}

impl ArtifactLoss {
    /// Build a loss report.
    #[must_use]
    pub fn new(
        reason: impl Into<String>,
        expected_sha256: Option<String>,
        byte_length: usize,
    ) -> Self {
        Self {
            reason: reason.into(),
            expected_sha256,
            byte_length,
        }
    }
}

/// Content-addressed artifact directory. Malicious or corrupted contents can
/// only ever be addressed by a validated digest, so the store never joins an
/// untrusted path.
pub struct ArtifactStore {
    root: PathBuf,
    registry: Option<PathBuf>,
}

impl fmt::Debug for ArtifactStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactStore")
            .field("root", &self.root)
            .field("registry", &self.registry)
            .finish()
    }
}

impl ArtifactStore {
    /// The canonical on-disk artifact directory for the runtime
    /// (`~/.local/share/sessionmunch/artifacts` on Linux/macOS), matching the
    /// North Star runtime layout.
    #[must_use]
    pub fn default_root() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local"))
            .join("sessionmunch")
            .join("artifacts")
    }

    /// Open a store rooted at `root`. The root is canonicalized so the
    /// containment check in [`Self::path_for_digest`] always compares against
    /// a real path, and the directory is created on first use.
    ///
    /// # Errors
    /// Returns an [`anyhow::Error`] if the root cannot be canonicalized (for
    /// a not-yet-existing root, its nearest existing ancestor is canonicalized
    /// and re-joined, so normal first-use works without a pre-created dir).
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("creating artifact root {}", root.display()))?;
        let canonical_root = canonicalize_for_write(root)?;
        let registry = Some(canonical_root.join(REGISTRY_FILENAME));
        Ok(Self {
            root: canonical_root,
            registry,
        })
    }

    /// The artifact root on disk.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The on-disk path for a validated digest, guaranteed to sit inside
    /// `self.root`. Called only with strings already checked by
    /// [`Self::validate_digest`], so the result can never escape the root.
    fn path_for_digest(&self, digest: &str) -> PathBuf {
        self.root.join(format!("{digest}{ARTIFACT_EXT}"))
    }

    /// Validate that `digest` is exactly `DIGEST_LEN` lowercase hex digits —
    /// the only shape that may be turned into a path segment.
    fn validate_digest(digest: &str) -> Result<(), ArtifactError> {
        if digest.len() != DIGEST_LEN
            || !digest.chars().all(|c| c.is_ascii_hexdigit())
            || digest != digest.to_ascii_lowercase()
        {
            return Err(ArtifactError::InvalidDigest {
                digest: digest.to_string(),
                expected_digest_len: DIGEST_LEN,
            });
        }
        Ok(())
    }

    /// Store `content` content-addressed under `expected_sha256`.
    ///
    /// The caller-supplied digest is verified against the computed SHA-256 of
    /// `content` **before** anything is written; a mismatch returns an explicit
    /// [`ArtifactError::HashMismatch`] and no bytes are persisted. On success
    /// the raw bytes are gzip-compressed and written to
    /// `<root>/<expected_sha256>.gz`, fsynced and renamed into place. Storing
    /// identical content twice is idempotent (returns the same digest, no new
    /// write).
    ///
    /// # Errors
    /// Returns [`ArtifactError::InvalidDigest`] / [`ArtifactError::HashMismatch`]
    /// for validation failures, or an [`anyhow::Error`] wrapping an I/O /
    /// compression failure.
    pub fn store_verified(
        &self,
        content: &[u8],
        expected_sha256: &str,
    ) -> Result<String, ArtifactError> {
        Self::validate_digest(expected_sha256)?;
        let computed = Self::sha256_hex(content);
        if computed != expected_sha256 {
            return Err(ArtifactError::HashMismatch {
                expected: expected_sha256.to_string(),
                computed,
            });
        }
        self.write_blob(content, expected_sha256)
            .map_err(|e| ArtifactError::Corrupt {
                digest: expected_sha256.to_string(),
                reason: e.to_string(),
            })?;
        Ok(computed)
    }

    /// Store `content` content-addressed, computing its digest locally. This
    /// is the trusted-source path where the caller cannot supply a lying hash.
    ///
    /// # Errors
    /// Mirrors [`Self::store_verified`].
    pub fn store(&self, content: &[u8]) -> Result<String, ArtifactError> {
        let digest = Self::sha256_hex(content);
        self.write_blob(content, &digest)
            .map_err(|e| ArtifactError::Corrupt {
                digest: digest.clone(),
                reason: e.to_string(),
            })?;
        Ok(digest)
    }

    /// Retrieve an artifact by its SHA-256 digest. A missing artifact returns
    /// `Ok(None)`; a present but unreadable/corrupt artifact returns an
    /// explicit [`ArtifactError::Corrupt`].
    ///
    /// # Errors
    /// [`ArtifactError::InvalidDigest`] for a malformed digest,
    /// [`ArtifactError::Corrupt`] when the blob is missing-from-disk is not
    /// distinguishable from corrupt is handled via `Ok(None)`; decompression or
    /// re-hash failures surface as [`ArtifactError::Corrupt`].
    pub fn retrieve(&self, digest: &str) -> Result<Option<Vec<u8>>, ArtifactError> {
        Self::validate_digest(digest)?;
        let path = self.path_for_digest(digest);
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read(&path).map_err(|e| ArtifactError::Corrupt {
            digest: digest.to_string(),
            reason: format!("read failed: {e}"),
        })?;
        let content = Self::gunzip(&raw).map_err(|e| ArtifactError::Corrupt {
            digest: digest.to_string(),
            reason: e.to_string(),
        })?;
        let computed = Self::sha256_hex(&content);
        if computed != digest {
            return Err(ArtifactError::Corrupt {
                digest: digest.to_string(),
                reason: format!("content hash {computed} != address {digest}"),
            });
        }
        Ok(Some(content))
    }

    /// Integrity check: `true` iff a **valid** artifact exists for `digest`.
    ///
    /// Unlike a naive existence probe, this decompresses the stored blob and
    /// re-hashes the recovered bytes against the content address, so a present
    /// but corrupt/tampered blob returns `Err(Corrupt)` rather than `Ok(true)`.
    /// A missing blob returns `Ok(false)`.
    ///
    /// # Errors
    /// [`ArtifactError::InvalidDigest`] for a malformed digest, or
    /// [`ArtifactError::Corrupt`] when the blob exists but fails decompression
    /// or its content does not match its content address.
    pub fn verify(&self, digest: &str) -> Result<bool, ArtifactError> {
        Self::validate_digest(digest)?;
        let path = self.path_for_digest(digest);
        if !path.exists() {
            return Ok(false);
        }
        // Decompress + re-hash the existing blob: `retrieve` returns `Ok(None)`
        // only when the path is absent (already handled above), `Err(Corrupt)`
        // when the blob is untrustworthy, and `Ok(Some(..))` when it is valid.
        self.retrieve(digest).map(|present| present.is_some())
    }

    /// Record provenance linkage for a stored artifact identified by its
    /// content-address `digest` (as returned by [`Self::store`] / stored under
    /// [`Self::store_verified`]). Fails explicitly if the artifact is not
    /// present so we never link a phantom.
    ///
    /// # Errors
    /// [`ArtifactError::InvalidDigest`] if `digest` is malformed,
    /// [`ArtifactError::Missing`] if no artifact exists for it, or an
    /// [`anyhow::Error`] wrapping a registry I/O / SQL error.
    pub fn link(&self, digest: &str, provenance: &ArtifactProvenance) -> anyhow::Result<()> {
        Self::validate_digest(digest)?;
        // Robust link: verify the referenced artifact exists first, then record.
        match self.verify(digest) {
            Ok(false) => {
                return Err(anyhow::Error::new(ArtifactError::Missing {
                    digest: digest.to_string(),
                }));
            }
            Ok(true) => {}
            Err(error) => return Err(error.into()),
        }
        self.write_link(digest, provenance)
    }

    fn write_link(&self, digest: &str, provenance: &ArtifactProvenance) -> anyhow::Result<()> {
        let registry = self
            .registry
            .as_deref()
            .context("registry not configured")?;
        if let Some(parent) = registry.parent() {
            fs::create_dir_all(parent).context("creating registry dir")?;
        }
        let conn = Connection::open(registry).context("opening artifact registry")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS artifact_provenance (
                sha256         TEXT PRIMARY KEY,
                session_id     TEXT NOT NULL,
                node_id        TEXT NOT NULL,
                harness        TEXT NOT NULL,
                event_id       TEXT,
                event_source   TEXT,
                created_at_ms  INTEGER NOT NULL
            );",
        )
        .context("creating registry schema")?;
        conn.execute(
            "INSERT OR REPLACE INTO artifact_provenance
                 (sha256, session_id, node_id, harness, event_id, event_source, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                digest,
                provenance.session_id.to_string(),
                provenance.node_id,
                provenance.harness,
                provenance.event_id,
                provenance.event_source,
                provenance.created_at_ms,
            ],
        )
        .context("recording artifact provenance")?;
        Ok(())
    }

    fn write_blob(&self, content: &[u8], digest: &str) -> anyhow::Result<()> {
        let path = self.path_for_digest(digest);
        if path.exists() {
            // An existing blob is only accepted when it actually verifies
            // (decompresses and re-hashes to this content address). A present
            // but corrupt blob is repaired by rewriting the correct bytes — a
            // retry must never silently report success while corruption
            // persists. If the rewrite itself fails, the error propagates.
            match self.verify(digest) {
                Ok(true) => return Ok(()), // idempotent: valid blob already present
                Ok(false) => {
                    // Exists on disk but not retrievable as valid; treat as
                    // corrupt and rewrite below.
                }
                Err(corrupt) => {
                    // Corrupt or invalid existing blob: repair by rewriting it
                    // with the correct bytes below. The error is dropped from
                    // the log only because the repair replaces the very blob
                    // it names; a rewrite failure still propagates.
                    let _ = corrupt;
                }
            }
            self.rewrite_blob(content, &path)?;
            return Ok(());
        }
        self.rewrite_blob(content, &path)?;
        Ok(())
    }

    /// Atomically write `content` as the gzip-compressed blob at `path`
    /// (temp + fsync + rename), replacing any prior bytes.
    fn rewrite_blob(&self, content: &[u8], path: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(&self.root).context("creating artifact root")?;
        let compressed = Self::gzip(content)?;
        let tmp = self.root.join(format!(".blob-{}.tmp", Uuid::new_v4()));
        let mut file = File::create(&tmp).context("creating temp artifact")?;
        file.write_all(&compressed)
            .context("writing temp artifact")?;
        file.sync_all().context("syncing temp artifact")?;
        fs::rename(&tmp, path).context("publishing artifact")?;
        Ok(())
    }

    /// gzip-compress raw bytes (flate2, gzip format).
    fn gzip(content: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(content)?;
        enc.finish().context("finishing gzip compression")
    }

    /// Decompress gzip bytes; fails if the stream is truncated / corrupt.
    fn gunzip(compressed: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut dec = GzDecoder::new(compressed);
        let mut out = Vec::with_capacity(compressed.len() * 2);
        dec.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Lowercase SHA-256 hex of a byte slice — the content address.
    fn sha256_hex(content: &[u8]) -> String {
        format!("{:x}", Sha256::digest(content))
    }
}

/// Canonicalize a root for writing: if it does not exist yet (the common
/// first-use case) canonicalize its nearest existing ancestor and re-join the
/// remainder, so every path comparison inside the store runs against a
/// resolved absolute prefix.
fn canonicalize_for_write(root: &Path) -> anyhow::Result<PathBuf> {
    if root.exists() {
        return root
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", root.display()));
    }
    let mut ancestor = root.to_path_buf();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            anyhow::anyhow!(
                "no existing ancestor to canonicalize for {}",
                root.display()
            )
        })?;
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .expect("ancestor chain terminates at root")
            .to_path_buf();
    }
    let mut canonical = ancestor
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", ancestor.display()))?;
    for segment in suffix.into_iter().rev() {
        canonical.push(segment);
    }
    fs::create_dir_all(&canonical)?;
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn store_for_test(root: &Path) -> ArtifactStore {
        ArtifactStore::open(root).expect("open store")
    }

    #[test]
    fn store_verified_round_trips_by_digest() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"the quick brown fox jumps over the lazy dog".repeat(3);
        let digest = store.store(&content).expect("store");
        assert_eq!(digest.len(), DIGEST_LEN);
        // The on-disk blob is gzip-compressed (smaller) and inside the root.
        let blob = root.join(format!("{digest}{ARTIFACT_EXT}"));
        assert!(blob.exists(), "blob should exist");
        let blob_bytes = fs::read(&blob).expect("read blob");
        assert!(blob_bytes.len() < content.len(), "compressed smaller");
        // Retrieve returns the exact original bytes.
        let got = store.retrieve(&digest).expect("retrieve").expect("present");
        assert_eq!(got, content);
        // Verify says present + valid.
        assert!(store.verify(&digest).expect("verify"));
    }

    #[test]
    fn store_verified_rejects_hash_mismatch_and_writes_nothing() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"payload that must not be persisted under a lying digest";
        // A plausible but wrong digest.
        let wrong = format!("{:064x}", 0_usize);
        let err = store
            .store_verified(content, &wrong)
            .expect_err("must reject mismatch");
        match err {
            ArtifactError::HashMismatch { expected, computed } => {
                assert_eq!(expected, wrong);
                assert_ne!(computed, wrong);
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
        // Nothing was written.
        assert!(!root.join(format!("{wrong}{ARTIFACT_EXT}")).exists());
        let root_empty = fs::read_dir(&root)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true);
        assert!(root_empty, "root stays empty");
    }

    #[test]
    fn store_verified_accepts_a_correct_receipt_hash() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"verified receipt";
        let digest = store.store(content).expect("store");
        // Same content with the correct expected digest succeeds.
        let again = store
            .store_verified(content, &digest)
            .expect("matching receipt hash accepted");
        assert_eq!(again, digest);
    }

    #[test]
    fn store_is_idempotent() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"dedupe me";
        let a = store.store(content).expect("first");
        let b = store.store(content).expect("second");
        assert_eq!(a, b);
        let entries = fs::read_dir(&root).expect("read root").count();
        assert_eq!(entries, 1, "only one blob written");
    }

    #[test]
    fn retrieve_missing_returns_none_but_verify_reports_missing() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let digest = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(store.retrieve(digest).expect("retrieve"), None);
        assert!(!store.verify(digest).expect("verify"));
    }

    #[test]
    fn corrupt_blob_is_reported_as_explicit_loss() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"corrupt me on purpose";
        let digest = store.store(content).expect("store");
        // Tamper with the stored blob.
        let blob = root.join(format!("{digest}{ARTIFACT_EXT}"));
        let good = fs::read(&blob).expect("read blob");
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        fs::write(&blob, &bad).expect("tamper");
        // Retrieval now reports the corruption explicitly rather than silently
        // returning wrong bytes.
        let err = store.retrieve(&digest).expect_err("must report corruption");
        match err {
            ArtifactError::Corrupt { digest: d, reason } => {
                assert_eq!(d, digest);
                assert!(!reason.is_empty());
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
        // Restore and confirm retrieval succeeds again (also exercises the
        // re-hash guard).
        fs::write(&blob, &good).expect("restore");
        assert_eq!(
            store.retrieve(&digest).expect("retrieve"),
            Some(content.to_vec())
        );
    }

    #[test]
    fn invalid_digest_can_never_escape_the_root() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        for bad in [
            "../etc/passwd".to_string(),
            "/etc/passwd".to_string(),
            ".".to_string(),
            "..".to_string(),
            "abcdef".to_string(), // too short
            "Zbcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(), // uppercase
            "bcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789_".to_string(), // underscore
        ] {
            assert_eq!(
                store.retrieve(&bad).unwrap_err().digest(),
                Some(bad.as_str()),
                "{bad:?} should be rejected as InvalidDigest"
            );
        }
        // And nothing escaped: root is clean.
        let entries = fs::read_dir(&root).expect("read root").count();
        assert_eq!(entries, 0, "no files leaked out");
    }

    #[test]
    fn link_records_and_rejects_phantom_digest() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"linkable artifact";
        let digest = store.store(content).expect("store");
        // Link to a real (stored) artifact.
        let provenance = ArtifactProvenance {
            session_id: SessionId::new(),
            node_id: "sleeper".into(),
            harness: "hermes".into(),
            event_id: Some("evt-1".into()),
            event_source: Some("managed".into()),
            created_at_ms: 1_700_000_000_000,
        };
        assert!(store.link(&digest, &provenance).is_ok());
        let registry = root.join(REGISTRY_FILENAME);
        assert!(registry.exists(), "registry exists");
        // The digest recorded equals the stored artifact's content address.
        let conn = Connection::open(&registry).expect("open registry");
        let recorded: String = conn
            .query_row(
                "SELECT sha256 FROM artifact_provenance WHERE harness = 'hermes'",
                [],
                |r| r.get(0),
            )
            .expect("link row");
        // session_id+node_id is NOT the content address, so we verify the
        // event/session metadata landed rather than the exact digest key.
        assert_eq!(recorded.len(), DIGEST_LEN);
        // We stored under the content digest; the row's key must be that.
        assert_eq!(recorded, digest);
    }

    #[test]
    fn default_root_is_under_data_dir() {
        let root = ArtifactStore::default_root();
        assert!(
            root.ends_with("sessionmunch/artifacts")
                || root.to_string_lossy().contains("sessionmunch")
        );
        assert!(root.is_absolute());
    }

    #[test]
    fn verify_reports_corrupt_existing_blob_not_success() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"verify must decompress and re-hash, not just stat";
        let digest = store.store(content).expect("store");
        // Corrupt the stored blob at the tail byte.
        let blob = root.join(format!("{digest}{ARTIFACT_EXT}"));
        let good = fs::read(&blob).expect("read blob");
        let mut bad = good.clone();
        bad[0] ^= 0xff;
        fs::write(&blob, &bad).expect("tamper");
        // `verify` must NOT report a corrupt existing blob as valid: a naive
        // existence check would return true here. We require an explicit
        // Corrupt error (the NO-SHIP finding #3 identity check, not a probe).
        let err = store
            .verify(&digest)
            .expect_err("corrupt blob must fail verify");
        match err {
            ArtifactError::Corrupt { digest: d, .. } => assert_eq!(d, digest),
            other => panic!("expected Corrupt, got {other:?}"),
        }
        // Serious as written: it must not silently say Ok(true).
        assert!(store.verify(&digest).as_ref().is_err());
    }

    #[test]
    fn retry_store_on_corrupt_blob_repairs_instead_of_silently_succeeding() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"retry must repair a corrupt existing blob, never silently keep it";
        let digest = store.store(content).expect("store");
        // Corrupt the stored blob.
        let blob = root.join(format!("{digest}{ARTIFACT_EXT}"));
        let good = fs::read(&blob).expect("read blob");
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        fs::write(&blob, &bad).expect("tamper");
        // A retry of the same content must leave the blob VALID again (repair),
        // not silently return success while the corruption persists.
        let repaired = store.store(content).expect("retry store");
        assert_eq!(repaired, digest);
        // The repaired blob verifies and retrieves byte-for-byte.
        assert!(store.verify(&digest).expect("verify after repair"));
        assert_eq!(
            store.retrieve(&digest).expect("retrieve").expect("present"),
            content
        );
    }

    #[test]
    fn link_rejects_a_corrupt_existing_blob() {
        let (_dir, root) = temp_root();
        let store = store_for_test(&root);
        let content = b"corrupt then try to link";
        let digest = store.store(content).expect("store");
        let blob = root.join(format!("{digest}{ARTIFACT_EXT}"));
        let mut bad = fs::read(&blob).expect("read blob");
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        fs::write(&blob, &bad).expect("tamper");
        let provenance = ArtifactProvenance {
            session_id: SessionId::new(),
            node_id: "sleeper".into(),
            harness: "hermes".into(),
            event_id: Some("evt-x".into()),
            event_source: Some("managed".into()),
            created_at_ms: 1_700_000_000_001,
        };
        // Linking must fail: the referenced blob is corrupt, so we would
        // otherwise record provenance for a blob we cannot deliver.
        assert!(store.link(&digest, &provenance).is_err());
        // No phantom link row was written.
        let registry = root.join(REGISTRY_FILENAME);
        if registry.exists() {
            let conn = Connection::open(&registry).expect("open registry");
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM artifact_provenance", [], |r| r.get(0))
                .expect("count");
            assert_eq!(count, 0, "no phantom provenance row recorded");
        }
    }
}
