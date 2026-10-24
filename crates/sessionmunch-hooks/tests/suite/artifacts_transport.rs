//! P1.4: end-to-end artifact transport and provenance over real HTTP.
//!
//! Exercises the authoritative `POST /artifacts` router on a loopback server
//! backed by a real [`ArtifactStore`], driven by the satellite-side
//! [`ArtifactClient`] — no mock sink. Mirrors the shape of
//! `sleeper_spool_outage.rs` (the /session-events P1.1/P1.2 transport test):
//!
//! - `uploads_verified_artifact_and_links_provenance_end_to_end`: a
//!   compressible payload ships over the wire; the Hub verifies the receipt,
//!   stores the compressed content-addressed blob, and writes the provenance
//!   link. Re-shipping identical bytes is idempotent (same digest, still one
//!   valid blob).
//! - `lying_receipt_hash_is_rejected_and_nothing_is_stored`: a caller that
//!   claims a receipt that does not match its payload is refused (409) and no
//!   bytes or registry rows are persisted.
//! - `corrupt_existing_blob_is_repaired_on_reship_not_silently_accepted`: a
//!   blob tampered on the Hub's disk fails verification and is repaired by a
//!   reship of the correct bytes — never silently accepted as-is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use sessionmunch_core::SessionId;
use sessionmunch_hooks::{ArtifactState, artifacts_router};
use sessionmunch_workstream::{
    ArtifactClient, ArtifactClientError, ArtifactProvenance, ArtifactStore, ArtifactUpload,
};

/// Subdirectory of the temp root that acts as the Hub's artifact store.
const ARTIFACT_DIR: &str = "artifacts";

/// A live Hub: a real artifact store plus the real P1.4 router on loopback.
struct ArtifactHub {
    /// The canonical store root (so the test can inspect/tamper blobs).
    root: PathBuf,
    /// The store the Hub serves from (for direct in-process assertions).
    store: Arc<ArtifactStore>,
    /// `http://127.0.0.1:<port>`.
    base_url: String,
    _server: tokio::task::JoinHandle<()>,
}

impl ArtifactHub {
    async fn start(dir: &Path) -> Self {
        let root = dir.join(ARTIFACT_DIR).to_path_buf();
        let store = Arc::new(ArtifactStore::open(&root).expect("open artifact store"));
        let state = ArtifactState {
            store: store.clone(),
        };
        let app = artifacts_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            root: root.to_path_buf(),
            store,
            base_url,
            _server: server,
        }
    }

    fn blob_path(&self, digest: &str) -> PathBuf {
        self.root.join(format!("{digest}.gz"))
    }
}

impl Drop for ArtifactHub {
    fn drop(&mut self) {
        self._server.abort();
    }
}

fn provenance(session_id: SessionId) -> ArtifactProvenance {
    ArtifactProvenance {
        session_id,
        node_id: "satellite-1".into(),
        harness: "hermes".into(),
        event_id: Some("evt-100".into()),
        event_source: Some("managed".into()),
        created_at_ms: 1_700_000_000_500,
    }
}

fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(content))
}

/// A compressible payload (repeated run) so we can assert the on-disk blob is
/// actually gzip-compressed.
fn compressible_content(bytes: usize) -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog. "
        .iter()
        .cycle()
        .take(bytes)
        .copied()
        .collect()
}

#[tokio::test]
async fn uploads_verified_artifact_and_links_provenance_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = ArtifactHub::start(dir.path()).await;
    let client = ArtifactClient::new(&hub.base_url, None);

    let content = compressible_content(16 * 1024);
    let digest = sha256_hex(&content);
    let session_id = SessionId::new();
    let upload = ArtifactUpload {
        content,
        digest: digest.clone(),
        provenance: provenance(session_id),
    };

    let receipt = client.upload(&upload).await.expect("upload accepted");
    assert_eq!(
        receipt.sha256, digest,
        "receipt echoes the verified content address"
    );
    assert_eq!(
        receipt.session_id, session_id,
        "receipt echoes provenance session"
    );

    // The compressed blob landed on the Hub and returns the exact bytes.
    assert!(hub.blob_path(&digest).exists(), "blob stored on the Hub");
    let on_disk = std::fs::read(hub.blob_path(&digest)).expect("read blob");
    assert!(
        on_disk.len() < upload.content.len(),
        "blob is gzip-compressed"
    );
    assert_eq!(
        hub.store
            .retrieve(&digest)
            .expect("retrieve")
            .expect("present"),
        upload.content,
        "retrieve returns byte-for-byte the shipped content"
    );

    // Provenance is linked in the registry, addressing the stored digest.
    let registry = hub.root.join("registry.sqlite");
    assert!(registry.exists(), "registry written by the link");
    let conn = Connection::open(&registry).expect("open registry");
    let (recorded_digest, recorded_session, recorded_node): (String, String, String) = conn
        .query_row(
            "SELECT sha256, session_id, node_id FROM artifact_provenance",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0).unwrap(),
                    row.get::<_, String>(1).unwrap(),
                    row.get::<_, String>(2).unwrap(),
                ))
            },
        )
        .expect("read the recorded provenance row");
    assert_eq!(
        recorded_digest, digest,
        "registry key is the content address"
    );
    assert_eq!(
        recorded_session,
        session_id.to_string(),
        "session attribution persisted"
    );
    assert_eq!(recorded_node, "satellite-1", "node attribution persisted");

    // Idempotent re-ship: same digest, store stays valid with one blob.
    let again = client.upload(&upload).await.expect("reship accepted");
    assert_eq!(again.sha256, digest);
    assert!(
        hub.store.verify(&digest).expect("verify"),
        "store verifies after reship"
    );
}

#[tokio::test]
async fn lying_receipt_hash_is_rejected_and_nothing_is_stored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = ArtifactHub::start(dir.path()).await;
    let client = ArtifactClient::new(&hub.base_url, None);

    let content = b"payload that must not be stored under a lying receipt";
    let wrong = format!("{:064x}", 0usize);
    let upload = ArtifactUpload {
        content: content.to_vec(),
        digest: wrong.clone(),
        provenance: provenance(SessionId::new()),
    };

    let err = client.upload(&upload).await.unwrap_err();
    assert!(
        matches!(err, ArtifactClientError::HashMismatch { .. }),
        "a lying receipt must surface as HashMismatch, got {err:?}"
    );

    // Nothing persisted: no blob and no provenance registry.
    assert!(
        !hub.blob_path(&wrong).exists(),
        "no blob for the rejected digest"
    );
    assert!(
        !hub.root.join("registry.sqlite").exists(),
        "no provenance row for a rejected artifact"
    );
}

#[tokio::test]
async fn corrupt_existing_blob_is_repaired_on_reship_not_silently_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = ArtifactHub::start(dir.path()).await;
    let client = ArtifactClient::new(&hub.base_url, None);

    let content = b"corrupt then reship over the wire".repeat(4);
    let digest = sha256_hex(&content);
    let upload = ArtifactUpload {
        content,
        digest: digest.clone(),
        provenance: provenance(SessionId::new()),
    };
    client.upload(&upload).await.expect("initial upload");

    // Tamper with the stored blob on the Hub's disk.
    let blob = hub.blob_path(&digest);
    let mut bad = std::fs::read(&blob).expect("read stored blob");
    let mid = bad.len() / 2;
    bad[mid] ^= 0xff;
    std::fs::write(&blob, &bad).expect("tamper with blob");

    // A read of the corrupt blob must fail explicitly — never silently
    // return wrong bytes (NO-SHIP finding #3: identity check, not a probe).
    assert!(
        hub.store.verify(&digest).is_err(),
        "a corrupt existing blob must fail verification"
    );

    // A reship of the same content repairs it: the store rewrites the correct
    // bytes (never silently reports success while corruption persists) and
    // the Hub returns a normal receipt.
    let receipt = client.upload(&upload).await.expect("reship repairs");
    assert_eq!(receipt.sha256, digest);
    assert!(
        hub.store.verify(&digest).expect("verify"),
        "repaired blob verifies"
    );
    assert_eq!(
        hub.store
            .retrieve(&digest)
            .expect("retrieve")
            .expect("present"),
        upload.content,
        "repaired blob returns the exact repaired bytes"
    );
}
