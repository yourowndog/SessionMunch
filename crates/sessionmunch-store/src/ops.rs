//! Mutating SQL operations executed on the writer thread.
//!
//! Each operation is one transaction. Calling them from anywhere other than
//! the writer thread would violate the single-writer invariant (see
//! [`crate::writer`]).

use std::collections::BTreeSet;

use sessionmunch_core::{
    AgentKind, EntityId, HandoffAcceptance, HandoffId, IdentityKey, LinkTarget, NewHandoff,
    NewObservation, NewPage, NewSession, ObservationId, ObservationKind, OwnerFilter, PageEvidence,
    PageId, PagePath, ProjectId, SessionId, WorkspaceId,
};

/// Summary returned by [`reorg_sessions`] and exposed via
/// [`crate::writer::WriterHandle::reorg_sessions`].
#[derive(Debug, Default, Clone)]
pub struct ReorgSummary {
    /// Sessions whose `project_id` was changed.
    pub sessions_moved: usize,
    /// Observations updated to match their session's new project.
    pub observations_updated: usize,
    /// `is_latest=1` pages marked `is_latest=0` (mash-up graveyard).
    pub pages_graveyarded: usize,
}

/// Result of atomically claiming a keyed hook observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestObservationOutcome {
    /// This delivery claimed the key and inserted the observation.
    Inserted(ObservationId),
    /// The observation exists, but downstream hook effects did not finish.
    ResumePending,
    /// The observation and downstream hook effects already completed.
    AlreadyComplete,
}

/// Unforgeable writer-issued session capability for hook follow-up mutations.
#[derive(Clone, Debug)]
pub struct AdmittedSession {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    agent_kind: AgentKind,
    owner: Option<String>,
}
impl AdmittedSession {
    /// Persisted session identifier authorized by this guard.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }
    /// Persisted workspace identifier authorized by this guard.
    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }
    /// Persisted project identifier authorized by this guard.
    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }
    /// Persisted agent kind authorized by this guard.
    pub fn agent_kind(&self) -> AgentKind {
        self.agent_kind
    }
    /// Persisted owner storage key, or `None` for a shared session.
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
}

/// Result of atomic hook admission.
#[derive(Clone, Debug)]
pub enum HookSessionAdmission {
    /// An ordinary observation was admitted for this persisted session.
    Observation {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
        /// Result of claiming and inserting the observation.
        ingest: IngestObservationOutcome,
    },
    /// A terminal event was admitted for a session that was still open.
    EndOpen {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
        /// Result of claiming and inserting the observation.
        ingest: IngestObservationOutcome,
    },
    /// A terminal event was admitted after new observations followed an end.
    ReEnd {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
        /// Result of claiming and inserting the observation.
        ingest: IngestObservationOutcome,
    },
    /// A terminal event was already fully represented by this persisted session.
    AlreadyEnded {
        /// Writer-issued guard for the persisted session.
        session: AdmittedSession,
    },
    /// A terminal event named no persisted session and created nothing.
    InvalidMissingEnd,
    /// A terminal event named a persisted session in a different scope, so it
    /// is not that session's end. Mirrors the pre-guard
    /// `SessionEndDisposition::DropInvalid` arm.
    InvalidScopedEnd,
}
/// Result of conditionally ending a session whose persisted observations are
/// all lifecycle boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleOnlyEndOutcome {
    /// The session was still boundary-only inside the writer transaction.
    Ended {
        /// Startup handoff returned to the open pool, when one was claimed.
        reopened_handoff: Option<HandoffId>,
    },
    /// Substantive work arrived before the atomic end check.
    Substantive,
}

/// Summary returned by [`purge_project`] and exposed via
/// [`crate::writer::WriterHandle::purge_project`].
#[derive(Debug, Default, Clone)]
pub struct PurgeSummary {
    /// Human-readable `workspace/project` label. Set by the caller (writer
    /// only knows IDs); filled in by [`purge_project`] from its parameters.
    pub label: String,
    /// Distinct page paths that were present before the delete (all versions,
    /// not just `is_latest=1`). The admin handler uses this list to remove
    /// the corresponding files from the wiki directory.
    pub page_paths: Vec<String>,
    /// Number of `pages` rows deleted (all versions, not just latest).
    pub pages_deleted: u64,
    /// Number of `sessions` rows deleted.
    pub sessions_deleted: u64,
    /// Number of `observations` rows deleted.
    pub observations_deleted: u64,
    /// Number of `handoffs` rows deleted.
    pub handoffs_deleted: u64,
    /// Number of `page_embeddings` rows deleted (cascades through pages).
    pub embeddings_deleted: u64,
    /// Number of `workstreams` rows deleted. These cascade from `projects`,
    /// so a project that looks empty by page/session/observation count can
    /// still take a managed workstream — and its portable event ledger — down
    /// with it. Counted so the caller can say so out loud.
    pub workstreams_deleted: u64,
    /// Number of `managed_runs` rows deleted (cascades through workstreams).
    pub managed_runs_deleted: u64,
    /// Ids of the deleted workstreams. The admin layer uses these typed,
    /// pre-delete identifiers to remove `raw/workstreams/<id>/` after the SQL
    /// transaction commits and to report any filesystem partial failure.
    pub workstream_ids: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete.
    pub compacted: bool,
}
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::error::{StoreError, StoreResult};

/// One embedding upsert requested by a backfill or embed command.
#[derive(Debug)]
pub struct EmbeddingWrite {
    /// Page receiving the embedding.
    pub page_id: PageId,
    /// Packed little-endian `f32` vector bytes.
    pub vector_bytes: Vec<u8>,
    /// Embedding provider name.
    pub provider: String,
    /// Embedding model name.
    pub model: String,
    /// Vector dimension.
    pub dim: u32,
}

/// One passage embedding upsert requested by a backfill.
#[derive(Debug)]
pub struct PassageEmbeddingWrite {
    /// Passage receiving the embedding.
    pub passage_id: uuid::Uuid,
    /// Packed little-endian `f32` vector bytes.
    pub vector_bytes: Vec<u8>,
    /// Embedding provider name.
    pub provider: String,
    /// Embedding model name.
    pub model: String,
    /// Vector dimension.
    pub dim: u32,
}

mod pages;
pub use self::pages::*;
mod scopes;
pub use self::scopes::*;
mod sessions;
pub use self::sessions::*;
mod embeddings;
pub use self::embeddings::*;
mod activity;
pub use self::activity::*;
mod decay;
pub use self::decay::*;
mod handoffs;
pub use self::handoffs::*;
mod maintenance;
pub use self::maintenance::*;
mod purge;
pub use self::purge::*;
mod bootstrap;
pub use self::bootstrap::*;
mod relocate;
pub use self::relocate::*;

fn observation_kind_as_str(kind: ObservationKind) -> &'static str {
    kind.as_str()
}

fn audit(
    tx: &rusqlite::Transaction<'_>,
    op: &str,
    workspace_id: Option<&[u8; 16]>,
    project_id: Option<&[u8; 16]>,
    page_id: Option<&[u8; 16]>,
    author_id: Option<&[u8; 16]>,
    at: i64,
) -> StoreResult<()> {
    audit_with_detail(
        tx,
        op,
        workspace_id,
        project_id,
        page_id,
        author_id,
        at,
        "{}",
    )
}

/// `audit`, but with a caller-supplied JSON `detail` payload. Exists so a
/// destructive batch can record what it did (e.g. the observation prune's
/// deleted-row count) instead of leaving a gap only inferable from timestamps.
#[allow(clippy::too_many_arguments)]
fn audit_with_detail(
    tx: &rusqlite::Transaction<'_>,
    op: &str,
    workspace_id: Option<&[u8; 16]>,
    project_id: Option<&[u8; 16]>,
    page_id: Option<&[u8; 16]>,
    author_id: Option<&[u8; 16]>,
    at: i64,
    detail: &str,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO audit_log (at, op, workspace_id, project_id, page_id, author_id, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            at,
            op,
            workspace_id.map(|b| &b[..]),
            project_id.map(|b| &b[..]),
            page_id.map(|b| &b[..]),
            author_id.map(|b| &b[..]),
            detail,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
