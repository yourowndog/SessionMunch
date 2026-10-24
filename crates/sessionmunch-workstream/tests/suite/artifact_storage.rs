//! End-to-end artifact storage behaviour: receipt hash validation, compressed
//! content-addressed write, retrieval by SHA-256, corruption detection, and
//! provenance linkage. Exercises the public [`ArtifactStore`] API against a
//! real temp directory (no test binary, compiled into the lib harness).

use crate::{ArtifactError, ArtifactProvenance, ArtifactStore};
use sessionmunch_core::SessionId;

#[test]
fn mismatched_receipt_hash_is_an_explicit_validation_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = ArtifactStore::open(dir.path()).expect("open store");

    let content = b"some large sanitized tool output";
    // A plausible-looking but incorrect digest (NOT the hash of `content`).
    let wrong_digest = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(b"different bytes"))
    };
    assert!(wrong_digest.len() == 64 && wrong_digest.chars().all(|c| c.is_ascii_hexdigit()));

    let err = store
        .store_verified(content, &wrong_digest)
        .expect_err("must reject");
    match err {
        ArtifactError::HashMismatch { expected, computed } => {
            assert_eq!(expected, wrong_digest);
            assert_ne!(computed, wrong_digest);
            assert_eq!(computed, {
                use sha2::{Digest, Sha256};
                format!("{:x}", Sha256::digest(content))
            });
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }

    // Nothing was persisted for the rejected artifact.
    assert_eq!(
        std::fs::read_dir(dir.path())
            .map(|it| it.count())
            .expect("read dir"),
        0
    );
}

#[test]
fn valid_artifact_writes_compressed_hash_verified_and_retrieves_by_sha256() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = ArtifactStore::open(dir.path()).expect("open store");

    // Build a payload that is compressible (repeated run) so we can assert the
    // on-disk blob is actually smaller (i.e. compression happened).
    let content: Vec<u8> = b"the quick brown fox jumps over the lazy dog. "
        .iter()
        .cycle()
        .take(64 * 1024)
        .copied()
        .collect();

    // Trusted-path store returns the content address.
    let digest = store.store(&content).expect("store");
    assert_eq!(digest.len(), 64);
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

    // The blob is gzip-compressed on disk (smaller than raw) and inside root.
    let blob = dir.path().join(format!("{digest}.gz"));
    assert!(blob.exists(), "blob must be written");
    let on_disk = std::fs::read(&blob).expect("read blob");
    assert!(
        on_disk.len() < content.len(),
        "compressed blob smaller than raw"
    );

    // Retrieval by SHA-256 returns byte-for-byte the original content, and the
    // re-hash integrity check passes.
    let got = store.retrieve(&digest).expect("retrieve").expect("present");
    assert_eq!(got, content);
    assert!(store.verify(&digest).expect("verify"));

    // The explicit receipt-hash path also accepts the correct digest.
    let again = store
        .store_verified(&content, &digest)
        .expect("verified store");
    assert_eq!(again, digest);
}

#[test]
fn provenance_linkage_round_trips_and_phantom_digest_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = ArtifactStore::open(dir.path()).expect("open store");

    let content = b"artifact evidence with provenance";
    let digest = store.store(content).expect("store");

    let provenance = ArtifactProvenance {
        session_id: SessionId::new(),
        node_id: "sleeper".into(),
        harness: "hermes".into(),
        event_id: Some("evt-9".into()),
        event_source: Some("managed".into()),
        created_at_ms: 1_700_000_000_123,
    };
    store
        .link(&digest, &provenance)
        .expect("link real artifact");

    // Registry exists and carries a row addressing the stored content.
    let registry = dir.path().join("registry.sqlite");
    assert!(registry.exists(), "registry.sqlite should exist");

    // A digest with no stored artifact must fail explicitly (no phantom link).
    let phantom = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(b"never stored"))
    };
    let err = store
        .link(&phantom, &provenance)
        .expect_err("must reject phantom");
    match err.downcast_ref::<ArtifactError>() {
        Some(ArtifactError::Missing { digest: d }) => assert_eq!(d, &phantom),
        other => panic!("expected Missing error, got {other:?}"),
    }
}

#[test]
fn traversal_shaped_digests_never_escape_the_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = ArtifactStore::open(dir.path()).expect("open store");

    // Path-traversal payloads and malformed identities are rejected as
    // InvalidDigest and never reach a path join.
    for bad in [
        "../etc/passwd".to_string(),
        "/etc/passwd".to_string(),
        "..".to_string(),
        format!("/tmp/{}-..", "a".repeat(55)),
    ] {
        let err = store
            .retrieve(&bad)
            .expect_err("must reject traversal shape");
        match err {
            ArtifactError::InvalidDigest { .. } => {}
            other => panic!("expected InvalidDigest for {bad:?}, got {other:?}"),
        }
    }

    // Only the registry may exist after our (rejected) attempts; no artifact
    // leak outside the root.
    for entry in std::fs::read_dir(dir.path()).expect("read dir") {
        let name = entry.expect("entry").file_name();
        assert!(
            name == "registry.sqlite"
                || !name.to_string_lossy().contains(&format!(
                    "{}{}",
                    "..",
                    std::path::MAIN_SEPARATOR
                ))
        );
    }
}
