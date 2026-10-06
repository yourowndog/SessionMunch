//! Read-only native harness adapters used by `sessionmunch run`.

mod drain;
mod harness;
mod repository;
mod snapshot;
mod spool;
mod transcript;

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
