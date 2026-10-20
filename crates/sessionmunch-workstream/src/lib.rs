//! Read-only native harness adapters used by `sessionmunch run`.
//!
//! Includes the content-addressed, compressed artifact store
//! ([`artifact`]) used for large sanitized evidence and its provenance
//! linkage.

mod artifact;
mod artifact_client;
mod drain;
mod harness;
mod hermes_history;
mod history_import;
mod repository;
mod snapshot;
mod spool;
mod transcript;

pub use artifact::{
    ARTIFACTS_PATH, ArtifactError, ArtifactLoss, ArtifactProvenance, ArtifactReceipt,
    ArtifactStore, ArtifactUpload,
};
pub use artifact_client::{ARTIFACT_UPLOAD_BYTE_CAP, ArtifactClient, ArtifactClientError};

pub use crate::drain::{
    DeliveryReceipt, DrainReport, EventSink, HttpEventSink, SESSION_EVENTS_PATH, SinkError,
    SpoolDrainer,
};
/// Crash-safe local spooling and idempotent drain for remote session events.
pub use crate::spool::{Spool, SpoolBatch, SpoolConfig, SpoolError, SpoolMetrics, SpooledRecord};
pub use harness::{
    LaunchMode, LaunchPlan, ManagedHarness, allows_native_session_adoption, apply_yolo,
    build_launch_plan, has_native_session_selector, kiro_explicit_session_id,
    kiro_selects_non_default_engine, kiro_selects_v2_engine, kiro_selects_v3_engine,
};
pub use hermes_history::{
    HermesDiscovery, HermesSession, discover_hermes_history, export_hermes_session,
};
pub use history_import::{
    DiscoveredSession, HarnessDiscovery, IMPORTABLE_HARNESSES, INLINE_EVENT_BYTES,
    OversizedPayload, StoreStatus, discover_harness_history, export_discovered, to_session_events,
};
pub use repository::{RepositoryIdentity, inspect_repository};
pub use snapshot::{
    HarnessSnapshot, SnapshotOutcome, SnapshotReport, SnapshotSource, snapshot_all_harnesses,
    snapshot_store,
};
pub use transcript::{
    ExportedTranscript, NativeSessionCandidate, discover_native_session, export_transcript,
    kiro_harness_from_source_cursor, kiro_v3_resume_uses_default_store, list_native_sessions,
    native_session_exists, wait_for_transcript_flush,
};

/// HTTP client for hub delivery. Bounded so a dropped connection surfaces as
/// a retryable transport error instead of hanging the drain forever (seen in
/// the first fleet backfill: two importers sat idle with the hub at load 0).
pub(crate) fn bounded_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
#[path = "../tests/suite/mod.rs"]
mod integration;
