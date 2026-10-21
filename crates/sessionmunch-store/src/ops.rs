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

/// Upsert a page by path, superseding any existing latest version when the
/// content (sha256 of body) has changed.
///
/// Returns the id of the page row that should now be considered current.
pub fn upsert_page(conn: &mut Connection, page: &NewPage) -> StoreResult<PageId> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let (result_id, _passages) = upsert_page_in_tx(&tx, page, now)?;
    tx.commit()?;
    Ok(result_id)
}

/// Resolve a workspace by name, creating it if missing. Atomic.
pub fn get_or_create_workspace(
    conn: &mut Connection,
    name: &str,
) -> StoreResult<sessionmunch_core::WorkspaceId> {
    let tx = conn.transaction()?;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM workspaces WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    let id = if let Some(bytes) = existing {
        sessionmunch_core::WorkspaceId::from_slice(&bytes)?
    } else {
        let id = sessionmunch_core::WorkspaceId::new();
        tx.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id.as_bytes(), name, Timestamp::now().as_microsecond()],
        )?;
        id
    };
    tx.commit()?;
    Ok(id)
}

/// Names of other workspaces that already contain a project called `name`
/// (excluding `workspace_id`), ordered by workspace name. Homonymous
/// projects across workspaces are legal — rows are id-namespaced — but on
/// *creation* a non-empty result is almost always an accidental misroute,
/// so the caller can surface a warning. Runs inside the caller's `tx`.
fn project_name_in_other_workspaces(
    tx: &rusqlite::Transaction<'_>,
    workspace_id: &sessionmunch_core::WorkspaceId,
    name: &str,
) -> StoreResult<Vec<String>> {
    let mut stmt = tx.prepare(
        "SELECT w.name FROM projects p \
         JOIN workspaces w ON w.id = p.workspace_id \
         WHERE p.name = ?1 AND p.workspace_id != ?2 \
         ORDER BY w.name",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![name, workspace_id.as_bytes()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn warn_project_name_in_other_workspaces(name: &str, also_in: &[String]) {
    if !also_in.is_empty() {
        tracing::warn!(
            project = name,
            also_in = ?also_in,
            "creating a project whose name already exists in other workspace(s) — legal (id-namespaced) but often an accidental misroute"
        );
    }
}

/// Resolve a project by `(workspace_id, name)`, creating it if missing.
/// Atomic.
pub fn get_or_create_project(
    conn: &mut Connection,
    workspace_id: &sessionmunch_core::WorkspaceId,
    name: &str,
    repo_path: Option<&str>,
) -> StoreResult<sessionmunch_core::ProjectId> {
    let repo_path = repo_path.map(normalize_repo_path_key);
    let tx = conn.transaction()?;
    let mut also_in = Vec::new();
    let mut created = false;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id.as_bytes(), name],
            |row| row.get(0),
        )
        .optional()?;
    let id = if let Some(bytes) = existing {
        sessionmunch_core::ProjectId::from_slice(&bytes)?
    } else {
        also_in = project_name_in_other_workspaces(&tx, workspace_id, name)?;
        let id = sessionmunch_core::ProjectId::new();
        tx.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.as_bytes(),
                workspace_id.as_bytes(),
                name,
                repo_path.as_deref(),
                Timestamp::now().as_microsecond()
            ],
        )?;
        created = true;
        id
    };
    tx.commit()?;
    if scheduler_state_table_exists(conn)? {
        crate::auto_improve::ensure_scheduler_state(conn, *workspace_id, id)?;
    }
    if created {
        warn_project_name_in_other_workspaces(name, &also_in);
    }
    Ok(id)
}

/// Delete "hollow" project rows: zero pages (any version), zero sessions,
/// zero observations, zero handoffs, zero managed workstreams, zero
/// auto-improve runs/proposals/rejections, and older than `min_age_days`.
/// (The per-project scheduler-state row is bookkeeping created for every
/// project and does not count as data.) These are pure bookkeeping noise left
/// behind by probes, renames, and failed first events — nothing exists to lose,
/// which is what makes this safe to run on a schedule (the operator-driven
/// `purge-project` covers everything that actually holds data). Reserved
/// projects (`scratch`, the cwd-less fallback; `_global`, the preferences
/// scope) are exempt even when empty. Returns the deleted names for logging.
///
/// # Errors
/// Propagates SQLite failures.
pub fn sweep_hollow_projects(conn: &mut Connection, min_age_days: u32) -> StoreResult<Vec<String>> {
    let cutoff =
        Timestamp::now().as_microsecond() - i64::from(min_age_days) * 24 * 60 * 60 * 1_000_000;
    let tx = conn.transaction()?;
    let names: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT name FROM projects
             WHERE name NOT IN ('scratch', ?1)
               AND created_at < ?2
               AND NOT EXISTS (SELECT 1 FROM pages        WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM sessions     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM observations WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM handoffs     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM workstreams  WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_runs      WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_proposals WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_rejections WHERE project_id = projects.id)",
        )?;
        let rows = stmt.query_map(
            params![sessionmunch_core::GLOBAL_SCOPE_PROJECT, cutoff],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<Result<_, _>>()?
    };
    if !names.is_empty() {
        tx.execute(
            "DELETE FROM projects
             WHERE name NOT IN ('scratch', ?1)
               AND created_at < ?2
               AND NOT EXISTS (SELECT 1 FROM pages        WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM sessions     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM observations WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM handoffs     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM workstreams  WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_runs      WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_proposals WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_rejections WHERE project_id = projects.id)",
            params![sessionmunch_core::GLOBAL_SCOPE_PROJECT, cutoff],
        )?;
    }
    tx.commit()?;
    Ok(names)
}

/// NULL out `repo_path` values that act as longest-prefix-match catch-alls
/// (issue #103), so every project nested beneath them stops resolving to the
/// wrong row after upgrade. A one-shot startup heal; idempotent (a healed row
/// is NULL and drops out of the candidate set, so a second pass heals 0).
/// Returns the number of rows healed.
///
/// A row is healed when:
/// - `repo_path` is one of the two broad sentinels -- filesystem root (`/`)
///   or the operator's home directory (`home`, when provided). These are
///   healed even if they happen to be git work-tree roots (e.g. a dotfiles
///   repo checked out at `$HOME`): as prefix keys they swallow everything
///   beneath them.
/// - otherwise, `repo_path` EXISTS on this host but is NOT a git work-tree
///   root (e.g. a bare `~/projects` cwd the original corruption captured).
///
/// A `repo_path` that does NOT exist on this host is left untouched: under a
/// remote/multi-user daemon it may be a client path for another user, or a
/// temporarily unmounted drive, and destroying it would wipe a valid prefix
/// key. This safety rule is mandatory.
pub fn heal_catch_all_repo_paths(conn: &mut Connection, home: Option<&str>) -> StoreResult<u64> {
    let home = home.map(normalize_repo_path_key);
    let candidates: Vec<(Vec<u8>, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, repo_path FROM projects WHERE repo_path IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let to_null: Vec<Vec<u8>> = candidates
        .into_iter()
        .filter(|(_, repo_path)| should_heal_repo_path(repo_path, home.as_deref()))
        .map(|(id, _)| id)
        .collect();
    let tx = conn.transaction()?;
    for id in &to_null {
        tx.execute(
            "UPDATE projects SET repo_path = NULL WHERE id = ?1",
            params![id],
        )?;
    }
    tx.commit()?;
    Ok(u64::try_from(to_null.len()).unwrap_or(0))
}

/// Decide whether a non-NULL `repo_path` is a prefix-match catch-all that
/// should be NULLed. See [`heal_catch_all_repo_paths`] for the full rule.
fn should_heal_repo_path(repo_path: &str, home: Option<&str>) -> bool {
    let repo_path_key = normalize_repo_path_key(repo_path);
    if repo_path_key == "/" || home == Some(repo_path_key.as_str()) {
        return true; // broad sentinels, healed even if they look like git roots
    }
    let p = std::path::Path::new(repo_path);
    // Non-existent paths (and stat errors) are left alone: multi-user/unmounted
    // safety. An existing path is a catch-all only when its `.git` is
    // definitively absent (a normal repo has a `.git` dir, a worktree/submodule
    // a `.git` file); a `.git` stat error preserves the row, same as the
    // path-existence check above.
    matches!(p.try_exists(), Ok(true)) && matches!(p.join(".git").try_exists(), Ok(false))
}

fn normalize_repo_path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

fn scheduler_state_table_exists(conn: &Connection) -> StoreResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'auto_improve_scheduler_state'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Insert a workspace with an **explicit id**, idempotent. Unlike
/// [`get_or_create_workspace`] (which mints a fresh id), this preserves the id
/// the caller already holds — used by `reindex`, which recovers the id from the
/// wiki directory name so the rebuilt index keys pages by the same
/// `(workspace_id, project_id)` the on-disk tree is laid out under. Re-running
/// is a no-op (`ON CONFLICT(id)`). `created_at` is the rebuild time.
pub fn ensure_workspace_with_id(
    conn: &mut Connection,
    id: sessionmunch_core::WorkspaceId,
    name: &str,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(id) DO NOTHING",
        params![id.as_bytes(), name, Timestamp::now().as_microsecond()],
    )?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            params![id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        Some(existing) if existing == name => Ok(()),
        Some(existing) => Err(StoreError::Duplicate(format!(
            "workspace id {id} already exists as name '{existing}', not manifest name '{name}'"
        ))),
        None => Err(StoreError::NotFound(format!(
            "workspace id {id} was not inserted"
        ))),
    }?;
    Ok(())
}

/// Insert a project with an **explicit id** under `workspace_id`, idempotent.
/// The reindex counterpart of [`ensure_workspace_with_id`].
pub fn ensure_project_with_id(
    conn: &mut Connection,
    id: sessionmunch_core::ProjectId,
    workspace_id: sessionmunch_core::WorkspaceId,
    name: &str,
    repo_path: Option<&str>,
) -> StoreResult<()> {
    let repo_path = repo_path.map(normalize_repo_path_key);
    let tx = conn.transaction()?;
    let inserted = tx.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO NOTHING",
        params![
            id.as_bytes(),
            workspace_id.as_bytes(),
            name,
            repo_path.as_deref(),
            Timestamp::now().as_microsecond()
        ],
    )?;
    let also_in = if inserted > 0 {
        project_name_in_other_workspaces(&tx, &workspace_id, name)?
    } else {
        Vec::new()
    };
    type ProjectRow = (Vec<u8>, String, Option<String>);
    let existing: Option<ProjectRow> = tx
        .query_row(
            "SELECT workspace_id, name, repo_path FROM projects WHERE id = ?1",
            params![id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match existing {
        Some((existing_ws, existing_name, existing_repo_path))
            if existing_ws.as_slice() == workspace_id.as_bytes()
                && existing_name == name
                && existing_repo_path.as_deref() == repo_path.as_deref() =>
        {
            Ok(())
        }
        Some((existing_ws, existing_name, existing_repo_path)) => {
            Err(StoreError::Duplicate(format!(
                "project id {id} already exists with workspace_id bytes length {}, name='{existing_name}', repo_path={existing_repo_path:?}; manifest has workspace={workspace_id}, name='{name}', repo_path={repo_path:?}",
                existing_ws.len(),
            )))
        }
        None => Err(StoreError::NotFound(format!(
            "project id {id} was not inserted"
        ))),
    }?;
    tx.commit()?;
    if inserted > 0 {
        warn_project_name_in_other_workspaces(name, &also_in);
    }
    Ok(())
}

/// Assert that `project_id` currently belongs to `workspace_id`.
///
/// Wiki writes call this before touching the filesystem so a stale hook/cache
/// carrying the old workspace for a moved project fails before it can create an
/// orphan file. The pairing INSERT triggers are still the final SQL backstop.
///
/// The two ways this fails need different words, because they send a reader
/// looking in different places. A project that moved workspaces is a stale
/// pairing: the row exists, and the caller's copy of its scope is out of date.
/// A project id with no row at all is a dangling reference — nothing moved,
/// and there is no scoping question to answer. Reporting the second as "does
/// not belong to workspace X" costs whoever reads that line a hunt for a
/// workspace/project mismatch that does not exist. The disambiguating query
/// runs only on the failure path, so the common case still pays one lookup.
pub fn ensure_project_workspace(
    conn: &Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> StoreResult<()> {
    let found = conn
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1 AND workspace_id = ?2",
            params![project_id.as_bytes(), workspace_id.as_bytes()],
            |_| Ok(()),
        )
        .optional()?;
    if found.is_some() {
        return Ok(());
    }
    let owner: Option<Vec<u8>> = conn
        .query_row(
            "SELECT workspace_id FROM projects WHERE id = ?1",
            params![project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Err(StoreError::NotFound(match owner {
        Some(owner) => format!(
            "project {project_id} does not belong to workspace {workspace_id}; \
             it belongs to {}",
            WorkspaceId::from_slice(&owner)?
        ),
        None => format!("project {project_id} does not exist"),
    }))
}

/// Upsert a batch of pages inside one transaction. Either *all* pages
/// land (each becoming the new `is_latest=true` version) or none do.
///
/// This is the M7b atomic-fan-out path: the consolidator can hand a
/// list of {sessions, concepts, decisions} pages and trust that
/// either the whole batch supersedes or the wiki is unchanged.
pub fn upsert_pages_batch(conn: &mut Connection, pages: &[NewPage]) -> StoreResult<Vec<PageId>> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let mut out = Vec::with_capacity(pages.len());
    for page in pages {
        let (id, _passages) = upsert_page_in_tx(&tx, page, now)?;
        out.push(id);
    }
    tx.commit()?;
    Ok(out)
}

struct ExistingPageVersion {
    id: Vec<u8>,
    body_sha256: Vec<u8>,
    frontmatter_json: String,
    title: String,
    tier: String,
    pinned: i64,
}

/// Normalise a page path into FTS-friendly search text, indexing BOTH forms
/// so either a whole-slug or a single-word query hits:
/// - segments: `/` and `.` → space, KEEPING `-`/`_` (FTS token chars) so the
///   full hyphenated slug stays one token (`foo-bar` matches a `"foo-bar"`
///   query);
/// - words: also split `-`/`_` so each word is its own token (`bar` matches).
///
/// `notes/foo-bar.md` → `notes foo-bar md notes foo bar md`.
///
/// MUST stay byte-identical to the backfill expression in migration V17 so
/// the `rebuild` and live-write paths index the same text (matching bm25
/// term frequencies, not just the same match set).
pub(crate) fn path_search_text(path: &str) -> String {
    let segments = path.replace(['/', '.'], " ");
    let words = segments.replace(['-', '_'], " ");
    format!("{segments} {words}")
}

/// Count latest page rows whose frontmatter is not yet OKF-conformant
/// (missing `type` or `generated.at`). Read-only; the migration uses it
/// to decide whether the backup gate must engage at all.
pub fn okf_nonconformant_latest_pages(conn: &Connection) -> StoreResult<u64> {
    let mut stmt = conn.prepare("SELECT path, frontmatter_json FROM pages WHERE is_latest = 1")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut pending = 0u64;
    for (path, fm_str) in rows {
        let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
        let mut conformed = fm.clone();
        sessionmunch_core::okf::conform_frontmatter(&path, &mut conformed);
        if conformed != fm || sessionmunch_core::okf::generated_at(&fm).is_none() {
            pending += 1;
        }
    }
    Ok(pending)
}

/// One latest-page row rewritten in place by [`okf_migrate_latest_pages`].
#[derive(Debug, Clone)]
pub struct OkfMigratedPage {
    /// Owning workspace.
    pub workspace_id: sessionmunch_core::WorkspaceId,
    /// Owning project.
    pub project_id: sessionmunch_core::ProjectId,
    /// Wiki-relative page path.
    pub path: String,
    /// The `generated.at` stamped from the row's own `updated_at`.
    pub generated_at: String,
}

/// One-shot OKF migration of every latest page row (docs/okf.md): conform
/// the frontmatter IN PLACE — same id, same version row, `updated_at`
/// untouched, body untouched — stamping `generated.at` from the row's own
/// `updated_at` so nothing looks freshly written. Historical versions
/// (`is_latest = 0`) stay as they were. Idempotent: conformed rows are
/// left alone, so a re-run rewrites zero pages.
pub fn okf_migrate_latest_pages(conn: &mut Connection) -> StoreResult<Vec<OkfMigratedPage>> {
    type LatestPageRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, String, i64);
    let tx = conn.transaction()?;
    let mut migrated = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, workspace_id, project_id, path, frontmatter_json, updated_at              FROM pages WHERE is_latest = 1",
        )?;
        let rows: Vec<LatestPageRow> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (id, ws, proj, path, fm_str, updated_at) in rows {
            let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
            let mut conformed = fm.clone();
            sessionmunch_core::okf::conform_frontmatter(&path, &mut conformed);
            let at = jiff::Timestamp::from_microsecond(updated_at)
                .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
                .strftime("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            if sessionmunch_core::okf::generated_at(&conformed).is_none() {
                sessionmunch_core::okf::stamp_generated_at(&mut conformed, &at);
            }
            if conformed == fm {
                continue;
            }
            tx.execute(
                "UPDATE pages SET frontmatter_json = ?1 WHERE id = ?2",
                params![serde_json::to_string(&conformed)?, id],
            )?;
            migrated.push(OkfMigratedPage {
                workspace_id: sessionmunch_core::WorkspaceId::from_slice(&ws)?,
                project_id: sessionmunch_core::ProjectId::from_slice(&proj)?,
                path,
                generated_at: sessionmunch_core::okf::generated_at(&conformed)
                    .unwrap_or(&at)
                    .to_string(),
            });
        }
    }
    tx.commit()?;
    Ok(migrated)
}

/// Stamp the per-version `generated.at` (write time, UTC) onto conformed
/// frontmatter and serialize it. Runs only on the paths that create a new
/// version row — the idempotent short-circuit above never reaches it, so
/// an unchanged page keeps its original timestamp.
fn stamped_frontmatter(mut conformed: serde_json::Value, now_us: i64) -> StoreResult<String> {
    // The wiki layer stamps (or inherits) `generated.at` before the file
    // is emitted; keep that value so file and row stay byte-aligned. The
    // stamp here covers writers that reach the store without a wiki file.
    if sessionmunch_core::okf::generated_at(&conformed).is_none() {
        let ts = jiff::Timestamp::from_microsecond(now_us)
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        sessionmunch_core::okf::stamp_generated_at(&mut conformed, &ts);
    }
    Ok(serde_json::to_string(&conformed)?)
}

/// What a [`backfill_entity_index`] pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EntityBackfillSummary {
    /// Latest pages that gained at least one entity link.
    pub pages_backfilled: u64,
    /// Entity→page links inserted.
    pub links_added: u64,
}

/// One-shot, idempotent backfill of the entity index from existing
/// frontmatter (docs/temporal.md).
///
/// The entity retrieval stream and every `as_of` timeline read from
/// `entities` / `entity_page_links`, which are populated only when a page
/// is written with entities. Historically those came from an LLM
/// consolidator's `entities:` field, absent on the bulk of a real store —
/// bootstrapped pages predate it and stable pages are never
/// re-consolidated — so both features sat empty on mature deployments.
///
/// This derives each latest page's entities from its `tags`/`entities`
/// frontmatter (the exact rule the write path now uses,
/// [`sessionmunch_core::frontmatter_entity_names`]) and attaches the links
/// the page version never got, with `valid_from` = the version's
/// `created_at` — the same window semantics the V56 migration used, and
/// honest because those tags have lived in the frontmatter since that
/// version was written.
///
/// Only latest pages that currently have NO entity links are considered,
/// so a store whose pages were written through the fixed path is a cheap
/// no-op and a re-run inserts nothing (the link insert is also
/// `ON CONFLICT DO NOTHING`). Superseded versions are left untouched.
pub fn backfill_entity_index(conn: &mut Connection) -> StoreResult<EntityBackfillSummary> {
    type CandidateRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, i64);
    let tx = conn.transaction()?;
    let mut summary = EntityBackfillSummary::default();
    {
        let rows: Vec<CandidateRow> = {
            let mut stmt = tx.prepare(
                "SELECT p.id, p.workspace_id, p.project_id, p.frontmatter_json, p.created_at \
                 FROM pages p \
                 WHERE p.is_latest = 1 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM entity_page_links l WHERE l.page_id = p.id \
                   )",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        for (page_id, ws, proj, fm_str, created_at) in rows {
            let fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap_or_default();
            let names = sessionmunch_core::frontmatter_entity_names(&fm);
            if names.is_empty() {
                continue;
            }
            let mut page_gained_a_link = false;
            for name in names {
                let entity_id: Vec<u8> = tx.query_row(
                    "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (workspace_id, project_id, name) DO UPDATE SET name = excluded.name \
                     RETURNING id",
                    params![EntityId::new().as_bytes(), &ws, &proj, name, created_at],
                    |row| row.get(0),
                )?;
                let inserted = tx.execute(
                    "INSERT INTO entity_page_links (entity_id, page_id, valid_from) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT (entity_id, page_id) DO NOTHING",
                    params![entity_id, &page_id, created_at],
                )?;
                if inserted > 0 {
                    summary.links_added += 1;
                    page_gained_a_link = true;
                }
            }
            if page_gained_a_link {
                summary.pages_backfilled += 1;
            }
        }
    }
    tx.commit()?;
    Ok(summary)
}

/// Rebuild the section/passage index for all latest pages in a scope,
/// processing in bounded batches. Returns the count of pages processed.
pub fn backfill_sections(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    batch_size: usize,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let rows: Vec<(Vec<u8>,)> = tx
        .prepare(
            "SELECT p.id FROM pages p
             WHERE p.workspace_id = ?1 AND p.project_id = ?2 AND p.is_latest = 1",
        )?
        .query_map([workspace_id.as_bytes(), project_id.as_bytes()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?,))
        })?
        .collect::<Result<_, _>>()?;
    drop(tx);

    let batch_size = batch_size.max(1);
    let mut count: u64 = 0;
    for chunk in rows.chunks(batch_size) {
        let tx = conn.transaction()?;
        for (page_id_bytes,) in chunk {
            let page_id = PageId::from_slice(page_id_bytes)?;
            let body: String = tx.query_row(
                "SELECT body FROM pages WHERE id = ?1",
                params![page_id_bytes],
                |row| row.get(0),
            )?;
            crate::passage_index::replace_page_sections_and_passages(
                &tx,
                page_id,
                *workspace_id,
                *project_id,
                &body,
            )?;
            count += 1;
        }
        tx.commit()?;
    }
    Ok(count)
}

pub(crate) fn upsert_page_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    now: i64,
) -> StoreResult<(PageId, Vec<crate::passage_index::CreatedPassage>)> {
    let path_search = path_search_text(page.path.as_str());
    let body_sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(page.body.as_bytes());
        hasher.finalize().into()
    };
    // OKF conformance at the single write choke point (docs/okf.md):
    // fill the deterministic keys, compare idempotency modulo the
    // per-version `generated.at`, and stamp `at` only when content
    // actually changed (below).
    let mut conformed = page.frontmatter_json.clone();
    sessionmunch_core::okf::conform_frontmatter(page.path.as_str(), &mut conformed);
    let tier_str = page.tier.as_str();

    let existing: Option<ExistingPageVersion> = tx
        .query_row(
            "SELECT id, body_sha256, frontmatter_json, title, tier, pinned FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
            ],
            |row| {
                Ok(ExistingPageVersion {
                    id: row.get(0)?,
                    body_sha256: row.get(1)?,
                    frontmatter_json: row.get(2)?,
                    title: row.get(3)?,
                    tier: row.get(4)?,
                    pinned: row.get(5)?,
                })
            },
        )
        .optional()?;

    if let Some(existing) = existing {
        let existing_fm: serde_json::Value =
            serde_json::from_str(&existing.frontmatter_json).unwrap_or_default();
        if existing.body_sha256 == body_sha256
            && sessionmunch_core::okf::strip_generated_at(&existing_fm)
                == sessionmunch_core::okf::strip_generated_at(&conformed)
            && existing.title == page.title
            && existing.tier == tier_str
            && existing.pinned == i64::from(page.pinned)
        {
            let unchanged_id = PageId::from_slice(&existing.id).map_err(StoreError::from)?;
            // The content short-circuit skips a new version row, but a
            // reconsolidation from a different session still cites the
            // page it reaffirmed (P2, docs/design-hindsight-borrowings.md
            // §3) — record the evidence against the still-current id.
            insert_evidence_in_tx(tx, &unchanged_id, &page.evidence, now)?;
            return Ok((unchanged_id, Vec::new()));
        }
        let frontmatter_str = stamped_frontmatter(conformed, now)?;
        let new_id = PageId::new();
        // The page-grain ingestion window closes in the same statement,
        // same instant (issue #656, docs/design-page-ingestion-windows.md).
        tx.execute(
            "UPDATE pages SET is_latest = 0, valid_to = ?2 WHERE id = ?1",
            params![&existing.id, now],
        )?;
        // Close the superseded version's entity-link windows at the new
        // version's birth instant (docs/temporal.md).
        tx.execute(
            "UPDATE entity_page_links SET superseded_at = ?2 \
             WHERE page_id = ?1 AND superseded_at IS NULL",
            params![&existing.id, now],
        )?;
        tx.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, supersedes, pinned, author_id, \
              created_at, updated_at, expires_at, valid_from) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?14, ?14, ?15, ?14)",
            params![
                new_id.as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
                path_search,
                page.title,
                tier_str,
                page.body,
                body_sha256.as_slice(),
                frontmatter_str,
                &existing.id,
                i64::from(page.pinned),
                page.author_id.map(|id| id.as_bytes().to_vec()),
                now,
                page.expires_at.map(|ts| ts.as_microsecond()),
            ],
        )?;
        replace_links_in_tx(tx, &new_id, page)?;
        attach_entities_in_tx(tx, &new_id, page, now)?;
        refresh_incoming_links_for_path(tx, page, &new_id)?;
        insert_evidence_in_tx(tx, &new_id, &page.evidence, now)?;
        // Rebuild the section/passage index for this page version in the same
        // transaction (AGENTS.md invariant #3: indexes commit with the data).
        let passages = crate::passage_index::replace_page_sections_and_passages(
            tx,
            new_id,
            page.workspace_id,
            page.project_id,
            &page.body,
        )?;
        audit(
            tx,
            "supersede_page",
            Some(page.workspace_id.as_bytes()),
            Some(page.project_id.as_bytes()),
            Some(new_id.as_bytes()),
            page.author_id
                .as_ref()
                .map(sessionmunch_core::UserId::as_bytes),
            now,
        )?;
        return Ok((new_id, passages));
    }
    let frontmatter_str = stamped_frontmatter(conformed, now)?;
    let new_id = PageId::new();
    tx.execute(
        "INSERT INTO pages \
         (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
          frontmatter_json, is_latest, pinned, author_id, created_at, updated_at, expires_at, valid_from) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?13, ?14, ?13)",
        params![
            new_id.as_bytes(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
            page.path.as_str(),
            path_search,
            page.title,
            tier_str,
            page.body,
            body_sha256.as_slice(),
            frontmatter_str,
            i64::from(page.pinned),
            page.author_id.map(|id| id.as_bytes().to_vec()),
            now,
            page.expires_at.map(|ts| ts.as_microsecond()),
        ],
    )?;
    replace_links_in_tx(tx, &new_id, page)?;
    attach_entities_in_tx(tx, &new_id, page, now)?;
    refresh_incoming_links_for_path(tx, page, &new_id)?;
    insert_evidence_in_tx(tx, &new_id, &page.evidence, now)?;
    // Rebuild the section/passage index for this page version in the same
    // transaction (AGENTS.md invariant #3: indexes commit with the data).
    let passages = crate::passage_index::replace_page_sections_and_passages(
        tx,
        new_id,
        page.workspace_id,
        page.project_id,
        &page.body,
    )?;
    audit(
        tx,
        "create_page",
        Some(page.workspace_id.as_bytes()),
        Some(page.project_id.as_bytes()),
        Some(new_id.as_bytes()),
        page.author_id
            .as_ref()
            .map(sessionmunch_core::UserId::as_bytes),
        now,
    )?;
    Ok((new_id, passages))
}

/// Attach the normalized entity set to a new page version (V38). Entity
/// rows are shared within one project; links stay attached to immutable
/// page versions and latest-page filtering controls retrieval.
fn attach_entities_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page_id: &PageId,
    page: &NewPage,
    version_created_at: i64,
) -> StoreResult<()> {
    if page.entities.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    // Defence in depth: the wiki layer normalises on the way in, but the
    // store is the last gate before the UNIQUE(name) constraint.
    for name in sessionmunch_core::normalize_entities(&page.entities) {
        let entity_id: Vec<u8> = tx.query_row(
            "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (workspace_id, project_id, name) DO UPDATE SET name = excluded.name \
             RETURNING id",
            params![
                EntityId::new().as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                name,
                now,
            ],
            |row| row.get(0),
        )?;
        // Bi-temporal-lite (docs/temporal.md): the link's window opens
        // when its page version was created and stays open until the
        // version is superseded (closed in `upsert_page_in_tx`).
        tx.execute(
            "INSERT INTO entity_page_links (entity_id, page_id, valid_from) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT (entity_id, page_id) DO NOTHING",
            params![entity_id, page_id.as_bytes(), version_created_at],
        )?;
    }
    Ok(())
}

fn replace_links_in_tx(
    tx: &rusqlite::Transaction<'_>,
    from_page_id: &PageId,
    page: &NewPage,
) -> StoreResult<()> {
    tx.execute(
        "DELETE FROM links WHERE from_page_id = ?1",
        params![from_page_id.as_bytes()],
    )?;

    let mut seen = BTreeSet::new();
    for link in &page.links {
        // The relation joins the dedupe key (and the table's PK): a
        // typed edge and a plain reference to the same target are two
        // different rows, mirroring `link_type` in the schema.
        let key = (
            link.workspace.clone(),
            link.project.clone(),
            link.path.as_str().to_string(),
            link.relation,
        );
        if !seen.insert(key) {
            continue;
        }
        let to_page_id = latest_page_id_for_link(tx, page, link)?;
        let to_page_blob = to_page_id.as_ref().map(|id| &id.as_bytes()[..]);
        tx.execute(
            "INSERT INTO links \
                 (from_page_id, to_page_id, to_workspace, to_project, to_path, link_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                from_page_id.as_bytes(),
                to_page_blob,
                link.workspace,
                link.project,
                link.path.as_str(),
                link.relation
                    .map_or("references", sessionmunch_core::Relation::as_str),
            ],
        )?;
    }
    Ok(())
}

/// Record a page write's evidence sources (P2,
/// docs/design-hindsight-borrowings.md §3), in the same transaction as the
/// page upsert. `INSERT OR IGNORE` on the `(page_id, source_kind,
/// source_id)` PK makes re-citing the same source a no-op — a session that
/// reconsolidates the same path twice does not inflate the count. Empty
/// `evidence` (every pre-2.2 caller) inserts nothing.
fn insert_evidence_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page_id: &PageId,
    evidence: &[PageEvidence],
    now: i64,
) -> StoreResult<()> {
    for e in evidence {
        tx.execute(
            "INSERT OR IGNORE INTO page_evidence (page_id, source_kind, source_id, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![page_id.as_bytes(), e.kind.as_str(), e.source_id, now],
        )?;
    }
    Ok(())
}

/// Resolve a link target to the latest page id it points at, or `None` if the
/// target workspace / project / page does not exist yet (an unresolved forward
/// link). A bare link resolves within the source page's own project; a
/// `[[project:path]]` / `[[workspace/project:path]]` link resolves against the
/// named project (same workspace when only the project is given).
fn latest_page_id_for_link(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    link: &LinkTarget,
) -> StoreResult<Option<PageId>> {
    let (workspace_blob, project_blob): (Vec<u8>, Vec<u8>) = match &link.project {
        None => (
            page.workspace_id.as_bytes().to_vec(),
            page.project_id.as_bytes().to_vec(),
        ),
        Some(project_name) => {
            let workspace_blob: Vec<u8> = match &link.workspace {
                None => page.workspace_id.as_bytes().to_vec(),
                Some(workspace_name) => {
                    let found: Option<Vec<u8>> = tx
                        .query_row(
                            "SELECT id FROM workspaces WHERE name = ?1",
                            params![workspace_name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match found {
                        Some(id) => id,
                        None => return Ok(None),
                    }
                }
            };
            let project_blob: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
                    params![workspace_blob, project_name],
                    |row| row.get(0),
                )
                .optional()?;
            match project_blob {
                Some(id) => (workspace_blob, id),
                None => return Ok(None),
            }
        }
    };

    let bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![workspace_blob, project_blob, link.path.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| PageId::from_slice(&bytes).map_err(StoreError::from))
        .transpose()
}

fn refresh_incoming_links_for_path(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    latest_page_id: &PageId,
) -> StoreResult<()> {
    // (1) Bare (same-project) links: from_page lives in this page's project and
    // the target carries no scope. Repoints all matches (not only unresolved):
    // a new page version changes the latest id, so resolved links must follow.
    tx.execute(
        "UPDATE links \
         SET to_page_id = ?1 \
         WHERE to_project IS NULL AND to_path = ?2 \
           AND EXISTS ( \
               SELECT 1 FROM pages from_page \
               WHERE from_page.id = links.from_page_id \
                 AND from_page.workspace_id = ?3 \
                 AND from_page.project_id = ?4 \
           )",
        params![
            latest_page_id.as_bytes(),
            page.path.as_str(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
        ],
    )?;

    // (2) Cross-project links naming this page's project by name. `to_workspace`
    // may be explicit (cross-workspace) or NULL (same workspace as the source).
    let project_name: Option<String> = tx
        .query_row(
            "SELECT name FROM projects WHERE id = ?1",
            params![page.project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let workspace_name: Option<String> = tx
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            params![page.workspace_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if let (Some(project_name), Some(workspace_name)) = (project_name, workspace_name) {
        tx.execute(
            "UPDATE links \
             SET to_page_id = ?1 \
             WHERE to_project = ?2 AND to_path = ?3 \
               AND ( \
                   to_workspace = ?4 \
                   OR ( \
                       to_workspace IS NULL \
                       AND EXISTS ( \
                           SELECT 1 FROM pages from_page \
                           WHERE from_page.id = links.from_page_id \
                             AND from_page.workspace_id = ?5 \
                       ) \
                   ) \
               )",
            params![
                latest_page_id.as_bytes(),
                project_name,
                page.path.as_str(),
                workspace_name,
                page.workspace_id.as_bytes(),
            ],
        )?;
    }
    Ok(())
}

/// Begin (or re-affirm) a session row keyed on the caller-supplied id.
/// Idempotent: a second call with the same id leaves the row untouched.
pub fn begin_session(conn: &mut Connection, session: &NewSession) -> StoreResult<()> {
    begin_session_row(conn, session)
}

pub(crate) fn begin_session_in_transaction(
    tx: &Transaction<'_>,
    session: &NewSession,
) -> StoreResult<()> {
    begin_session_row(tx, session)?;
    let matches_receiver: bool = tx.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM sessions \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
               AND agent_kind = ?4 AND actor_user IS ?5 \
         )",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
            session.agent_kind.as_str(),
            session.actor_user.as_deref(),
        ],
        |row| row.get(0),
    )?;
    if !matches_receiver {
        return Err(StoreError::InvalidState(
            "startup receiver id belongs to a different scope, agent, or operator".into(),
        ));
    }
    Ok(())
}

/// Whether this session was purged and must not be recreated by late-arriving
/// events. Scoped: a purge in one project does not suppress ingest for a
/// session id in another.
fn session_is_purged(conn: &Connection, session: &NewSession) -> StoreResult<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM purged_sessions \
             WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
         )",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
        ],
        |row| row.get::<_, i64>(0),
    )? == 1)
}

fn begin_session_row(conn: &Connection, session: &NewSession) -> StoreResult<()> {
    validate_identity_storage_key(session.actor_user.as_deref(), "session owner")?;
    // A purge must be terminal. The events that produced a session can still
    // be sitting undelivered in a client hook spool; without this check the
    // next drain recreates the session row and the observations land again,
    // silently undoing a deletion an application already promised its user
    // (#387). Every ingest path funnels through here, so this is the one
    // place the guard has to live.
    if session_is_purged(conn, session)? {
        return Err(StoreError::SessionPurged(session.id.to_string()));
    }
    let now = Timestamp::now().as_microsecond();
    let agent = session.agent_kind.as_str();
    let cwd: Option<String> = session
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    conn.execute(
        "INSERT INTO sessions \
         (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO NOTHING",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
            agent,
            cwd,
            now,
            session.actor_user.as_deref(),
        ],
    )?;
    Ok(())
}

/// Stamp a session as ended, optionally linking the synthesised summary page,
/// and record the observation generation covered by this end.
pub fn end_session(
    conn: &mut Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
) -> StoreResult<()> {
    end_session_row(conn, session_id, summary_page_id)
}

/// Atomically end a lifecycle-only session and return its startup handoff to
/// the open pool.
///
/// Only a handoff claimed by this exact receiver session is reopened. The
/// compare-and-set protects a baton that has moved to any other state, while
/// clearing all acceptance metadata makes a later claim indistinguishable
/// from the original open row.
pub fn end_lifecycle_only_session(
    conn: &mut Connection,
    session_id: &SessionId,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    let tx = conn.transaction()?;
    let outcome = end_lifecycle_only_session_in_tx(&tx, session_id)?;
    tx.commit()?;
    Ok(outcome)
}

fn end_lifecycle_only_session_in_tx(
    tx: &Transaction<'_>,
    session_id: &SessionId,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    // Substantive is defined POSITIVELY: at least one observation that is
    // real work (a user prompt or a tool use). This MUST stay in sync with
    // `is_ephemeral_session` in sessionmunch-hooks/src/router.rs — both sides
    // must classify every observation set identically, or this atomic
    // re-check would silently revert the router's verdict. Everything else,
    // including `stop` and `notification` (some harnesses, e.g. OpenCode,
    // fire these for purely internal work), is ephemeral.
    let has_substantive_observation: bool = tx.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM observations \
             WHERE session_id = ?1 \
               AND kind IN ('user-prompt', 'pre-tool-use', 'post-tool-use') \
         )",
        params![session_id.as_bytes()],
        |row| row.get(0),
    )?;
    if has_substantive_observation {
        return Ok(LifecycleOnlyEndOutcome::Substantive);
    }
    end_session_row(tx, session_id, None)?;
    let reopened: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "UPDATE handoffs \
             SET state = 'open', accepted_by = NULL, accepted_at = NULL, \
                 accepted_by_session = NULL, accepted_by_user = NULL \
             WHERE id = ( \
                 SELECT id FROM handoffs \
                 WHERE state = 'accepted' AND accepted_by_session = ?1 \
                 ORDER BY accepted_at DESC, created_at DESC LIMIT 1 \
             ) AND state = 'accepted' AND accepted_by_session = ?1 \
             RETURNING id, workspace_id, project_id",
            params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let reopened = reopened
        .map(|(id, workspace_id, project_id)| -> StoreResult<HandoffId> {
            let handoff_id = HandoffId::from_slice(&id)?;
            let workspace_id = WorkspaceId::from_slice(&workspace_id)?;
            let project_id = ProjectId::from_slice(&project_id)?;
            audit(
                tx,
                "release_lifecycle_only_handoff",
                Some(workspace_id.as_bytes()),
                Some(project_id.as_bytes()),
                None,
                None,
                Timestamp::now().as_microsecond(),
            )?;
            Ok(handoff_id)
        })
        .transpose()?;
    Ok(LifecycleOnlyEndOutcome::Ended {
        reopened_handoff: reopened,
    })
}

fn end_session_row(
    conn: &Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let page_blob: Option<&[u8]> = summary_page_id.map(|p| &p.as_bytes()[..]);
    conn.execute(
        "UPDATE sessions \
         SET ended_at = ?1, summary_page_id = ?2, \
             ended_observation_count = (\
                 SELECT COUNT(*) FROM observations WHERE session_id = ?3\
             ) \
         WHERE id = ?3",
        params![now, page_blob, session_id.as_bytes()],
    )?;
    Ok(())
}

/// Append a single observation. Caller is expected to have already
/// inserted the parent session via [`begin_session`].
pub fn insert_observation(
    conn: &mut Connection,
    obs: &NewObservation,
) -> StoreResult<ObservationId> {
    let tx = conn.transaction()?;
    let id = insert_observation_row(&tx, obs)?;
    tx.commit()?;
    Ok(id)
}

/// Seed an observation on a schema older than V65, before the session-event
/// ledger exists. Upgrade tests use it so the V66 backfill has history to
/// carry forward.
#[cfg(test)]
pub(crate) fn insert_observation_unmirrored(
    conn: &mut Connection,
    obs: &NewObservation,
) -> StoreResult<ObservationId> {
    let id = ObservationId::new();
    conn.execute(
        "INSERT INTO observations (id, session_id, workspace_id, project_id, kind, title, body, \
          importance, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id.as_bytes(),
            obs.session_id.as_bytes(),
            obs.workspace_id.as_bytes(),
            obs.project_id.as_bytes(),
            observation_kind_as_str(obs.kind),
            obs.title,
            obs.body,
            i64::from(obs.importance.clamp(1, 10)),
            Timestamp::now().as_microsecond(),
        ],
    )?;
    Ok(id)
}

/// Ingest-keys older than this are swept opportunistically on every keyed
/// insert. Keys only need to outlive the client spool (7 days + retry
/// backoff); 30 days is a generous margin.
const INGEST_KEY_TTL_MICROS: i64 = 30 * 24 * 60 * 60 * 1_000_000;

/// Claim a project-scoped ingest key and append its observation atomically.
///
/// The key claim and the observation row commit in ONE transaction: either
/// both land or neither does, so a crash cannot claim a key without its
/// observation. A claimed-but-incomplete key tells the ingest path to resume
/// downstream wiki/handoff effects without inserting another observation. A
/// completed key means the whole event can be acknowledged and skipped.
pub fn insert_observation_keyed(
    conn: &mut Connection,
    obs: &NewObservation,
    ingest_key: &str,
) -> StoreResult<IngestObservationOutcome> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;

    // Sweep before looking up the current key so an expired key can be reused
    // even when no unrelated keyed event arrived in the meantime.
    tx.execute(
        "DELETE FROM ingest_keys WHERE seen_at < ?1",
        params![now - INGEST_KEY_TTL_MICROS],
    )?;
    let existing: Option<Option<i64>> = tx
        .query_row(
            "SELECT completed_at FROM ingest_keys \
             WHERE project_id = ?1 AND key = ?2",
            params![obs.project_id.as_bytes(), ingest_key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(completed_at) = existing {
        tx.commit()?;
        return Ok(if completed_at.is_some() {
            IngestObservationOutcome::AlreadyComplete
        } else {
            IngestObservationOutcome::ResumePending
        });
    }

    tx.execute(
        "INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) \
         VALUES (?1, ?2, ?3, NULL)",
        params![obs.project_id.as_bytes(), ingest_key, now],
    )?;
    let id = insert_observation_row(&tx, obs)?;
    tx.commit()?;
    Ok(IngestObservationOutcome::Inserted(id))
}

/// Find or create the hook session, validate its immutable tuple and owner,
/// optionally claim an ingest key, and append the observation in one writer
/// transaction. Validation always precedes key mutation.
pub fn admit_hook_session_event(
    conn: &mut Connection,
    session: &NewSession,
    obs: &NewObservation,
    owner_filter: &OwnerFilter,
    ingest_key: Option<&str>,
    machine_id: Option<&str>,
) -> StoreResult<HookSessionAdmission> {
    if obs.session_id != session.id
        || obs.workspace_id != session.workspace_id
        || obs.project_id != session.project_id
    {
        return Err(StoreError::InvalidState(
            "hook observation does not match its session tuple".into(),
        ));
    }
    let session_end = obs.kind == ObservationKind::SessionEnd;
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    type Row = (Vec<u8>, Vec<u8>, String, Option<String>, Option<i64>, u64);
    let existing: Option<Row> = tx.query_row(
        "SELECT workspace_id, project_id, agent_kind, actor_user, ended_at, ended_observation_count FROM sessions WHERE id = ?1",
        params![session.id.as_bytes()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    ).optional()?;
    let (owner, ended_at, ended_count) = match existing {
        Some((ws, project, agent, owner, ended_at, ended_count)) => {
            // Corrupt owners fail closed, including for Any recovery. Owner
            // and agent identify WHO the session belongs to, so a mismatch
            // there is a genuine UUID collision and is terminal.
            if owner
                .as_deref()
                .is_some_and(|value| IdentityKey::from_storage_key(value).is_none())
                || agent != session.agent_kind.as_str()
                || !owner_filter.admits(owner.as_deref())
            {
                return Err(StoreError::SessionCollision);
            }
            // Scope is NOT identity. Under the default `follow-cwd` routing a
            // mid-session `cd` into another project legitimately resolves this
            // event to a different (workspace, project) than the session row,
            // and the observation belongs in the project it names — that is
            // exactly the record-splitting `[routing] mid_session = "sticky"`
            // exists to opt out of. Treating the difference as a collision
            // silently DROPPED those events instead of recording them.
            //
            // A terminal event is the one exception: an end naming a different
            // scope is not this session's end, so it is dropped rather than
            // ending someone else's session (the pre-guard
            // `SessionEndDisposition::DropInvalid` arm).
            let scoped_to_session = ws.as_slice() == session.workspace_id.as_bytes()
                && project.as_slice() == session.project_id.as_bytes();
            if session_end && !scoped_to_session {
                tx.commit()?;
                return Ok(HookSessionAdmission::InvalidScopedEnd);
            }
            (owner, ended_at, ended_count)
        }
        None if session_end => {
            tx.commit()?;
            return Ok(HookSessionAdmission::InvalidMissingEnd);
        }
        None => {
            validate_identity_storage_key(session.actor_user.as_deref(), "session owner")?;
            if !owner_filter.admits(session.actor_user.as_deref()) {
                return Err(StoreError::SessionCollision);
            }
            tx.execute(
                "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![session.id.as_bytes(), session.workspace_id.as_bytes(), session.project_id.as_bytes(), session.agent_kind.as_str(), session.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()), now, session.actor_user.as_deref()],
            )?;
            (session.actor_user.clone(), None, 0)
        }
    };
    // Best-effort provenance: the first event that names its machine fixes
    // it. A later event never rewrites it, so a moved session keeps the
    // machine it started on.
    if let Some(machine_id) = machine_id {
        tx.execute(
            "UPDATE sessions SET machine_id = ?2 WHERE id = ?1 AND machine_id IS NULL",
            params![session.id.as_bytes(), machine_id],
        )?;
    }
    let guard = AdmittedSession {
        session_id: session.id,
        workspace_id: session.workspace_id,
        project_id: session.project_id,
        agent_kind: session.agent_kind,
        owner,
    };
    if session_end && ended_at.is_some() {
        let count: u64 = tx.query_row(
            "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
            params![session.id.as_bytes()],
            |r| r.get(0),
        )?;
        if count <= ended_count {
            tx.commit()?;
            return Ok(HookSessionAdmission::AlreadyEnded { session: guard });
        }
    }
    let ingest = if let Some(key) = ingest_key {
        tx.execute(
            "DELETE FROM ingest_keys WHERE seen_at < ?1",
            params![now - INGEST_KEY_TTL_MICROS],
        )?;
        let old: Option<Option<i64>> = tx
            .query_row(
                "SELECT completed_at FROM ingest_keys WHERE project_id = ?1 AND key = ?2",
                params![obs.project_id.as_bytes(), key],
                |r| r.get(0),
            )
            .optional()?;
        match old {
            Some(done) => {
                if done.is_some() {
                    IngestObservationOutcome::AlreadyComplete
                } else {
                    IngestObservationOutcome::ResumePending
                }
            }
            None => {
                tx.execute("INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) VALUES (?1, ?2, ?3, NULL)", params![obs.project_id.as_bytes(), key, now])?;
                IngestObservationOutcome::Inserted(insert_observation_row(&tx, obs)?)
            }
        }
    } else {
        IngestObservationOutcome::Inserted(insert_observation_row(&tx, obs)?)
    };
    tx.commit()?;
    if !session_end {
        Ok(HookSessionAdmission::Observation {
            session: guard,
            ingest,
        })
    } else if ended_at.is_some() {
        Ok(HookSessionAdmission::ReEnd {
            session: guard,
            ingest,
        })
    } else {
        Ok(HookSessionAdmission::EndOpen {
            session: guard,
            ingest,
        })
    }
}

fn validate_admitted_session(tx: &Transaction<'_>, admitted: &AdmittedSession) -> StoreResult<()> {
    let owner: Option<String> = tx.query_row(
        "SELECT actor_user FROM sessions WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND agent_kind = ?4",
        params![admitted.session_id.as_bytes(), admitted.workspace_id.as_bytes(), admitted.project_id.as_bytes(), admitted.agent_kind.as_str()],
        |r| r.get(0),
    ).optional()?.ok_or(StoreError::SessionCollision)?;
    if owner != admitted.owner
        || owner
            .as_deref()
            .is_some_and(|v| IdentityKey::from_storage_key(v).is_none())
    {
        return Err(StoreError::SessionCollision);
    }
    Ok(())
}

/// Guarded hook equivalent of [`end_session`].
pub fn end_admitted_session(
    conn: &mut Connection,
    admitted: &AdmittedSession,
    page: Option<&PageId>,
) -> StoreResult<()> {
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    end_session_row(&tx, &admitted.session_id, page)?;
    tx.commit()?;
    Ok(())
}

/// Guarded hook lifecycle-only end.
pub fn end_admitted_lifecycle_only_session(
    conn: &mut Connection,
    admitted: &AdmittedSession,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    let outcome = end_lifecycle_only_session_in_tx(&tx, &admitted.session_id)?;
    tx.commit()?;
    Ok(outcome)
}

/// Guarded hook end plus automatic handoff in one transaction.
pub fn end_admitted_session_with_handoff(
    conn: &mut Connection,
    admitted: &AdmittedSession,
    page: Option<&PageId>,
    handoff: &NewHandoff,
) -> StoreResult<HandoffId> {
    if handoff.from_session_id != Some(admitted.session_id)
        || handoff.workspace_id != admitted.workspace_id
        || handoff.project_id != admitted.project_id
        || handoff.owner_user != admitted.owner
    {
        return Err(StoreError::SessionCollision);
    }
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    end_session_row(&tx, &admitted.session_id, page)?;
    let id = insert_handoff_row(&tx, handoff)?;
    tx.commit()?;
    Ok(id)
}

/// Mark a keyed hook event complete after its downstream effects finish.
///
/// This transition is idempotent so two resumed processors can converge on the
/// same completed key without turning a successful delivery into an error.
pub fn complete_observation_ingest(
    conn: &mut Connection,
    project_id: &ProjectId,
    ingest_key: &str,
) -> StoreResult<()> {
    if !complete_observation_ingest_if_claimed(conn, project_id, ingest_key)? {
        return Err(StoreError::InvalidState(
            "cannot complete an ingest key that was not claimed".into(),
        ));
    }
    Ok(())
}

/// Mark a keyed hook event complete when its observation claim exists.
///
/// Recovery paths use the boolean result to distinguish a pending native-hook
/// replay from an unkeyed or unrelated duplicate SessionEnd.
pub fn complete_observation_ingest_if_claimed(
    conn: &mut Connection,
    project_id: &ProjectId,
    ingest_key: &str,
) -> StoreResult<bool> {
    let completed_at = Timestamp::now().as_microsecond();
    let matched = conn.execute(
        "UPDATE ingest_keys \
         SET completed_at = COALESCE(completed_at, ?1) \
         WHERE project_id = ?2 AND key = ?3",
        params![completed_at, project_id.as_bytes(), ingest_key],
    )?;
    Ok(matched != 0)
}

/// The observation INSERT itself, shared by the plain and keyed paths
/// (`&Connection` so it also runs inside a [`rusqlite::Transaction`]). Every
/// caller holds a transaction, so the session-ledger mirror commits with it.
fn insert_observation_row(conn: &Connection, obs: &NewObservation) -> StoreResult<ObservationId> {
    let id = ObservationId::new();
    let now = Timestamp::now().as_microsecond();
    let kind = observation_kind_as_str(obs.kind);
    let importance: i64 = i64::from(obs.importance.clamp(1, 10));
    let (extension, source_event) = match (&obs.extension, &obs.source_event) {
        (Some(extension), Some(source_event)) => {
            (Some(extension.as_str()), Some(source_event.as_str()))
        }
        _ => (None, None),
    };
    conn.execute(
        "INSERT INTO observations \
         (id, session_id, workspace_id, project_id, kind, extension, source_event, title, body, \
          importance, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.as_bytes(),
            obs.session_id.as_bytes(),
            obs.workspace_id.as_bytes(),
            obs.project_id.as_bytes(),
            kind,
            extension,
            source_event,
            obs.title,
            obs.body,
            importance,
            now,
        ],
    )?;
    crate::session_events::mirror_observation(conn, &id, obs, now)?;
    Ok(id)
}

/// Store / replace one page's embedding. Bytes are the host-endian
/// `f32` packing of the unit-normalised vector. Provider/model/dim
/// are denormalised onto the row so a single SELECT can detect
/// heterogeneity (refuse-on-mismatch path).
/// Why an embed attempt produced no embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedOutcome {
    /// The embedding provider returned an error.
    Failed,
    /// The page could not be read from the wiki.
    Unreadable,
    /// The page body was empty after trimming, so there was nothing to embed.
    /// Not an error, but permanent until the body changes — and previously
    /// indistinguishable from an idle pass.
    SkippedEmpty,
}

impl EmbedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Unreadable => "unreadable",
            Self::SkippedEmpty => "skipped_empty",
        }
    }
}

/// Longest `detail` we keep. A provider error can carry a whole response body;
/// the ledger exists to say what happened, not to archive it.
const EMBED_FAILURE_DETAIL_MAX: usize = 500;

/// Record that an embed attempt on `page_id` did not produce an embedding.
///
/// Failure-only by design: a success writes nothing, because the existence of
/// a `page_embeddings` row already records it. That keeps the common path free
/// of an extra write and means a global `embed --force` cannot erase the
/// history here — it never touches this table (#528).
///
/// Upserts on `page_id`, so the row is always the most recent unsuccessful
/// attempt. It is deliberately left in place after a later pass succeeds: join
/// against `page_embeddings` to tell "still unresolved" from "recovered".
///
/// # Errors
/// Propagates SQL errors.
pub fn record_embed_failure(
    conn: &Connection,
    page_id: &PageId,
    outcome: EmbedOutcome,
    detail: Option<&str>,
) -> StoreResult<()> {
    let detail = detail.map(|d| {
        let mut truncated: String = d.chars().take(EMBED_FAILURE_DETAIL_MAX).collect();
        if truncated.chars().count() < d.chars().count() {
            truncated.push('…');
        }
        truncated
    });
    conn.execute(
        "INSERT INTO page_embed_failures (page_id, at, outcome, detail) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(page_id) DO UPDATE SET \
             at = excluded.at, outcome = excluded.outcome, detail = excluded.detail",
        params![
            page_id.as_bytes(),
            Timestamp::now().as_microsecond(),
            outcome.as_str(),
            detail,
        ],
    )?;
    Ok(())
}

pub fn store_embedding(
    conn: &mut Connection,
    page_id: &PageId,
    vector_bytes: &[u8],
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(page_id) DO UPDATE SET \
             vector = excluded.vector, \
             provider = excluded.provider, \
             model = excluded.model, \
             dim = excluded.dim, \
             created_at = excluded.created_at",
        params![page_id.as_bytes(), vector_bytes, provider, model, dim, now,],
    )?;
    Ok(())
}

/// Store / replace a batch of page embeddings in one transaction.
pub fn store_embeddings(conn: &mut Connection, embeddings: &[EmbeddingWrite]) -> StoreResult<()> {
    store_embeddings_in_table(conn, "page_embeddings", embeddings)
}

/// Store / replace a batch of L0 abstract embeddings
/// (`page_abstract_embeddings`) in one transaction. Same row shape and
/// upsert rule as [`store_embeddings`]; only the table differs.
pub fn store_abstract_embeddings(
    conn: &mut Connection,
    embeddings: &[EmbeddingWrite],
) -> StoreResult<()> {
    store_embeddings_in_table(conn, "page_abstract_embeddings", embeddings)
}

/// Store / replace a batch of passage embeddings in one transaction.
pub fn store_passage_embeddings(
    conn: &mut Connection,
    embeddings: &[PassageEmbeddingWrite],
) -> StoreResult<()> {
    if embeddings.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let sql = "INSERT INTO page_passage_embeddings (passage_id, provider, model, dim, embedding) \
                   VALUES (?1, ?2, ?3, ?4, ?5) \
                   ON CONFLICT(passage_id) DO UPDATE SET \
                       provider = excluded.provider, \
                       model = excluded.model, \
                       dim = excluded.dim, \
                       embedding = excluded.embedding";
        let mut stmt = tx.prepare(sql)?;
        for embedding in embeddings {
            stmt.execute(params![
                embedding.passage_id.as_bytes(),
                embedding.provider.as_str(),
                embedding.model.as_str(),
                embedding.dim,
                embedding.vector_bytes.as_slice(),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn store_embeddings_in_table(
    conn: &mut Connection,
    table: &'static str,
    embeddings: &[EmbeddingWrite],
) -> StoreResult<()> {
    if embeddings.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    {
        let sql = format!(
            "INSERT INTO {table} (page_id, vector, provider, model, dim, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(page_id) DO UPDATE SET \
                 vector = excluded.vector, \
                 provider = excluded.provider, \
                 model = excluded.model, \
                 dim = excluded.dim, \
                 created_at = excluded.created_at"
        );
        let mut stmt = tx.prepare(&sql)?;
        for embedding in embeddings {
            stmt.execute(params![
                embedding.page_id.as_bytes(),
                embedding.vector_bytes.as_slice(),
                embedding.provider.as_str(),
                embedding.model.as_str(),
                embedding.dim,
                now,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Remove a page's L0 abstract embedding row, if any. Called when a page is
/// rewritten without its frontmatter `abstract:` so the abstract stream
/// never ranks on a line the page no longer carries.
pub fn delete_abstract_embedding(conn: &mut Connection, page_id: &PageId) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM page_abstract_embeddings WHERE page_id = ?1",
        params![page_id.as_bytes()],
    )?;
    Ok(())
}

/// Fold per-client MCP tool-call deltas into their `(client, day)`
/// buckets. UPSERT per bucket inside one transaction: the buffer layer
/// in the MCP server coalesces a minute of calls into a handful of
/// entries, so this stays a few tiny rows per flush regardless of
/// traffic.
pub(crate) fn bump_client_activity(
    conn: &mut Connection,
    entries: &[(String, i64, u32, u32)],
) -> StoreResult<()> {
    if entries.is_empty() {
        return Ok(());
    }
    for (client, _, _, _) in entries {
        if !is_normalized_client_activity_label(client) {
            return Err(StoreError::InvalidState(
                "client activity label is not normalized".into(),
            ));
        }
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO client_activity (client, day, reads, writes) \
             VALUES ( \
                 CASE \
                     WHEN ?1 = ?5 \
                       OR EXISTS ( \
                           SELECT 1 FROM client_activity \
                           WHERE client = ?1 AND day = ?2 \
                       ) \
                       OR ( \
                           SELECT COUNT(*) FROM client_activity \
                           WHERE day = ?2 AND client <> ?5 \
                       ) < ?6 \
                     THEN ?1 \
                     ELSE ?5 \
                 END, \
                 ?2, ?3, ?4 \
             ) \
             ON CONFLICT(client, day) DO UPDATE SET \
                 reads = CASE \
                     WHEN reads > 9223372036854775807 - excluded.reads \
                     THEN 9223372036854775807 \
                     ELSE reads + excluded.reads \
                 END, \
                 writes = CASE \
                     WHEN writes > 9223372036854775807 - excluded.writes \
                     THEN 9223372036854775807 \
                     ELSE writes + excluded.writes \
                 END",
        )?;
        for (client, day, reads, writes) in entries {
            if *reads == 0 && *writes == 0 {
                continue;
            }
            stmt.execute(params![
                client,
                day,
                reads,
                writes,
                crate::CLIENT_ACTIVITY_OVERFLOW_CLIENT,
                crate::CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn is_normalized_client_activity_label(client: &str) -> bool {
    if client.is_empty()
        || client != client.trim()
        || client.chars().count() > crate::CLIENT_ACTIVITY_MAX_NAME_CHARS
        || client.chars().any(|c| c.is_control() || is_bidi_control(c))
    {
        return false;
    }

    let mut previous_space = false;
    for c in client.chars() {
        if c.is_whitespace() {
            if c != ' ' || previous_space {
                return false;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
    }
    true
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Bump `access_count` + `last_accessed_at` for the pages whose ids
/// appear in `page_ids`. Idempotent for unknown ids (no-op).
/// Used by the read path to feed the M8 reinforcement term.
///
/// Bump shared access counters and record an optional typed operator as a
/// distinct reader. Both mutations commit in the same transaction.
pub fn bump_access_for_pages_for_actor(
    conn: &mut Connection,
    page_ids: &[PageId],
    actor: Option<&IdentityKey>,
) -> StoreResult<()> {
    if page_ids.is_empty() {
        return Ok(());
    }
    if actor.is_some_and(|identity| !identity.is_valid()) {
        return Err(StoreError::InvalidState(
            "page access actor is not a normalized identity".into(),
        ));
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE pages \
             SET access_count = access_count + 1, last_accessed_at = ?1 \
             WHERE id = ?2 AND is_latest = 1",
        )?;
        for id in page_ids {
            stmt.execute(params![now, id.as_bytes()])?;
        }
        // Per-operator reinforcement, recorded in the SAME transaction so the
        // scalar and the breakdown cannot drift. Purely additive: the scalar
        // above is what the retention formula still reads by default.
        if let Some(actor) = actor {
            let actor = actor.storage_key();
            // The `WHERE EXISTS` guard is what keeps the documented idempotence
            // for unknown ids: `page_access.page_id` REFERENCES `pages(id)` and
            // the writer connection runs with `foreign_keys` ON, so a bare
            // INSERT for a page deleted between the search and this (detached,
            // post-response) bump would raise a FK violation and roll back the
            // WHOLE batch — costing every other page in the result set its
            // reinforcement. `INSERT OR IGNORE` does not help: it suppresses
            // constraint conflicts, not FK violations. The predicate mirrors the
            // UPDATE above so the scalar and the breakdown cannot drift.
            let mut per_actor = tx.prepare(
                "INSERT INTO page_access (page_id, actor) \
                 SELECT ?1, ?2 \
                 WHERE EXISTS (SELECT 1 FROM pages WHERE id = ?1 AND is_latest = 1) \
                 ON CONFLICT(page_id, actor) DO NOTHING",
            )?;
            for id in page_ids {
                per_actor.execute(params![id.as_bytes(), actor])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Record one explicit feedback signal against the latest version of a
/// page and update its derived salience, in a single transaction.
///
/// `page_feedback` is the append-only source of truth; `pages.salience`
/// is the derived value the decay formula reads. Returns the page id and
/// the salience after the update, or `None` when the path has no latest
/// version in that scope (idempotent no-op for a deleted page).
#[allow(clippy::too_many_arguments)]
pub fn record_page_feedback(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    kind: sessionmunch_core::FeedbackKind,
    reason: Option<&str>,
    author_id: Option<sessionmunch_core::UserId>,
    params: &crate::decay::DecayParams,
) -> StoreResult<Option<(PageId, f64)>> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let existing: Option<(Vec<u8>, Option<f64>)> = tx
        .query_row(
            "SELECT id, salience FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((page_id_bytes, current_salience)) = existing else {
        return Ok(None);
    };
    let page_id = PageId::from_slice(&page_id_bytes)?;
    let next_salience = crate::decay::salience_after_feedback(params, current_salience, kind);

    tx.execute(
        "INSERT INTO page_feedback \
         (id, page_id, workspace_id, project_id, kind, reason, salience_after, author_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            sessionmunch_core::PageFeedbackId::new().as_bytes(),
            page_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            kind.as_str(),
            reason,
            next_salience,
            author_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    // Salience rides on the page *version*: a later supersession starts
    // from salience_default again, which is intentional — feedback is a
    // judgement on the content that was read, not on the path forever.
    tx.execute(
        "UPDATE pages SET salience = ?1 WHERE id = ?2",
        params![next_salience, page_id.as_bytes()],
    )?;
    audit(
        &tx,
        "page_feedback",
        Some(workspace_id.as_bytes()),
        Some(project_id.as_bytes()),
        Some(page_id.as_bytes()),
        author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
        now,
    )?;
    tx.commit()?;
    Ok(Some((page_id, next_salience)))
}

/// Mark the expected latest page as evicted by the forget sweep.
///
/// The full identity and latest-id check share one transaction so a stale
/// sweep candidate cannot evict a page that was rewritten after selection.
/// `superseded_at` is the decay-tombstone marker; unlike `supersedes`, it is
/// never populated by ordinary page versioning.
pub fn soft_delete_for_decay_if_latest(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    expected_latest_id: PageId,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let affected = tx.execute(
        "UPDATE pages \
         SET is_latest = 0, superseded_at = ?1, valid_to = ?1 \
         WHERE id = ?2 \
           AND workspace_id = ?3 \
           AND project_id = ?4 \
           AND path = ?5 \
           AND is_latest = 1",
        params![
            now,
            expected_latest_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            path.as_str(),
        ],
    )?;
    if affected != 0 {
        // Retirement is supersession for the entity timeline too: an
        // open window on a tombstoned page made `as_of` resurrect
        // retired knowledge forever (post-audit finding). The page-grain
        // window closes in the same statement above (issue #656).
        tx.execute(
            "UPDATE entity_page_links SET superseded_at = ?1 \
             WHERE page_id = ?2 AND superseded_at IS NULL",
            params![now, expected_latest_id.as_bytes()],
        )?;
        audit(
            &tx,
            "soft_delete_for_decay",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            Some(expected_latest_id.as_bytes()),
            // Decay is a scheduled/admin system operation rather than a
            // user-attributable page edit.
            None,
            now,
        )?;
    }
    tx.commit()?;
    Ok(affected != 0)
}

/// Delete every version of a page (by path) from the index. Used when the
/// wiki file is removed (`Wiki::delete_page`): the watcher does not handle
/// file deletions, so the derived rows must be dropped explicitly or the
/// page keeps surfacing in search/recent with stale content. FK cascades
/// drop outgoing links + embeddings; the `pages_fts_ad` trigger keeps FTS in
/// sync; incoming links are set to NULL (unresolved). Idempotent.
pub fn delete_page(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    author_id: Option<sessionmunch_core::UserId>,
) -> StoreResult<()> {
    delete_page_inner(conn, workspace_id, project_id, path, None, author_id).map(|_| ())
}

/// Delete every version of `path` only when `expected_latest_id` is still its
/// latest version. The comparison and deletion share one transaction so a
/// stale retention candidate cannot remove a page that was refreshed later.
pub fn delete_page_if_latest(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    expected_latest_id: PageId,
    author_id: Option<sessionmunch_core::UserId>,
) -> StoreResult<bool> {
    delete_page_inner(
        conn,
        workspace_id,
        project_id,
        path,
        Some(expected_latest_id),
        author_id,
    )
}

fn delete_page_inner(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    expected_latest_id: Option<PageId>,
    author_id: Option<sessionmunch_core::UserId>,
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    // Capture the latest page id BEFORE the delete so the audit row can point
    // at it. None when the page is absent (delete is an idempotent no-op).
    let page_id: Option<[u8; 16]> = tx
        .query_row(
            "SELECT id FROM pages WHERE workspace_id = ?1 AND project_id = ?2 \
             AND path = ?3 AND is_latest = 1 LIMIT 1",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .and_then(|v| <[u8; 16]>::try_from(v.as_slice()).ok());
    if expected_latest_id
        .as_ref()
        .is_some_and(|expected| page_id.as_ref() != Some(expected.as_bytes()))
    {
        return Ok(false);
    }
    let rows = tx.execute(
        "DELETE FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            path.as_str()
        ],
    )?;
    if rows > 0 {
        tx.execute(
            "DELETE FROM entities \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM entity_page_links l WHERE l.entity_id = entities.id \
               )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
    }
    // Only audit a real deletion — a no-op delete (0 rows) writes nothing, so
    // the trail isn't polluted with idempotent misses.
    if rows > 0 {
        audit(
            &tx,
            "delete_page",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            page_id.as_ref(),
            author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
            Timestamp::now().as_microsecond(),
        )?;
    }
    tx.commit()?;
    Ok(rows > 0)
}

/// Permanently delete one eligible decay tombstone and its ancestry chain.
///
/// The tombstone id, full path identity, cutoff, and current latest-page state
/// are all rechecked in the transaction. This lets the wiki layer remove the
/// authoritative file only when the path has no newer live page, while still
/// allowing an old chain to be cleaned after that path was deliberately
/// recreated. Ordinary supersession rows have `superseded_at IS NULL` and
/// cannot become roots of this deletion.
pub fn hard_delete_decayed_page_chain(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    tombstone_id: PageId,
    expected_latest_id: Option<PageId>,
    cutoff_us: i64,
) -> StoreResult<usize> {
    let tx = conn.transaction()?;
    let current_latest: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
            |row| row.get(0),
        )
        .optional()?;
    let expected_latest = expected_latest_id.map(|id| id.as_bytes().to_vec());
    if current_latest != expected_latest {
        return Ok(0);
    }
    let n = tx.execute(
        "WITH RECURSIVE decay_chain(id) AS ( \
             SELECT id FROM pages \
             WHERE id = ?1 \
               AND workspace_id = ?2 \
               AND project_id = ?3 \
               AND path = ?4 \
               AND is_latest = 0 \
               AND superseded_at IS NOT NULL \
               AND superseded_at <= ?5 \
             UNION \
             SELECT parent.id \
             FROM decay_chain c \
             JOIN pages child ON child.id = c.id \
             JOIN pages parent ON parent.id = child.supersedes \
             WHERE parent.workspace_id = ?2 \
               AND parent.project_id = ?3 \
               AND parent.path = ?4 \
         ) \
         DELETE FROM pages WHERE id IN (SELECT id FROM decay_chain)",
        params![
            tombstone_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            path.as_str(),
            cutoff_us,
        ],
    )?;
    if n > 0 {
        tx.execute(
            "DELETE FROM entities \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM entity_page_links l WHERE l.entity_id = entities.id \
               )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
        audit(
            &tx,
            "hard_delete_decayed",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            Some(tombstone_id.as_bytes()),
            None,
            Timestamp::now().as_microsecond(),
        )?;
    }
    tx.commit()?;
    Ok(n)
}

/// One batch of an observation prune, as reported by
/// [`prune_consolidated_observations`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ObservationPruneOutcome {
    /// Observation rows permanently deleted by this batch.
    pub deleted: usize,
    /// Distinct sessions that lost rows in this batch. Returned rather than
    /// counted so a caller looping over batches can deduplicate a session whose
    /// rows straddle a batch boundary.
    pub sessions_touched: Vec<SessionId>,
    /// Sessions whose `ended_observation_count` watermark was lowered back onto
    /// the surviving row count.
    pub sessions_resynced: usize,
}

/// Permanently delete one bounded batch of raw observations belonging to
/// sessions that were already distilled into a live summary page.
///
/// Raw capture is the store's growth driver, but it is also the only input
/// consolidation, auto-improvement and the `raw_hits` fallback have. Deleting
/// it is safe exactly when its durable distillation still exists, so the
/// predicate joins through `sessions.summary_page_id` — the marker
/// [`end_session_row`] writes — and requires the page it points at to still be
/// live. `superseded_at IS NOT NULL` means decay already evicted that page, so
/// its session's raw capture is the last copy and must stay; a page hard-deleted
/// by the same sweep is excluded for free, because the `ON DELETE SET NULL`
/// foreign key has already cleared the pointer. A session with no `sessions`
/// row, or one that never ended, matches nothing and can never be pruned.
///
/// `ORDER BY created_at` makes the batching deterministic: a run interrupted
/// halfway leaves a clean age boundary rather than holes, so the next run
/// resumes on a store whose invariant ("everything older than X is gone for
/// consolidated sessions") is still one comparison.
///
/// The watermark resync is not optional. `session_end_disposition` compares a
/// live `COUNT(*)` against the persisted `sessions.ended_observation_count`;
/// leaving the watermark above reality would make a resumed session's genuinely
/// new work read as `AlreadyEnded` and never be re-consolidated. The
/// `ended_observation_count > (…)` guard keeps the repair idempotent and
/// one-directional — it can only lower a watermark that is now above reality.
/// Bounding it to the ids the delete returned also repairs sessions whose row
/// lives in a different scope than the observations being pruned.
pub fn prune_consolidated_observations(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    cutoff_us: i64,
    batch: usize,
) -> StoreResult<ObservationPruneOutcome> {
    if batch == 0 {
        return Ok(ObservationPruneOutcome::default());
    }
    let tx = conn.transaction()?;
    let mut deleted = 0usize;
    let mut sessions: BTreeSet<Vec<u8>> = BTreeSet::new();
    {
        let mut stmt = tx.prepare(
            "DELETE FROM observations \
             WHERE id IN ( \
                 SELECT o.id FROM observations o \
                 WHERE o.workspace_id = ?1 \
                   AND o.project_id = ?2 \
                   AND o.created_at < ?3 \
                   AND EXISTS ( \
                       SELECT 1 FROM sessions s \
                       JOIN pages p ON p.id = s.summary_page_id \
                       WHERE s.id = o.session_id \
                         AND p.superseded_at IS NULL \
                   ) \
                 ORDER BY o.created_at \
                 LIMIT ?4 \
             ) \
             RETURNING session_id",
        )?;
        let rows = stmt.query_map(
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                cutoff_us,
                u64::try_from(batch).unwrap_or(u64::MAX),
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        for row in rows {
            sessions.insert(row?);
            deleted += 1;
        }
    }
    let mut sessions_touched = Vec::with_capacity(sessions.len());
    let mut sessions_resynced = 0usize;
    if deleted > 0 {
        for session_id in &sessions {
            sessions_touched.push(SessionId::from_slice(session_id)?);
            sessions_resynced += tx.execute(
                "UPDATE sessions \
                 SET ended_observation_count = ( \
                         SELECT COUNT(*) FROM observations o WHERE o.session_id = sessions.id \
                     ) \
                 WHERE id = ?1 \
                   AND ended_observation_count > ( \
                         SELECT COUNT(*) FROM observations o WHERE o.session_id = sessions.id \
                     )",
                params![session_id],
            )?;
        }
        audit_with_detail(
            &tx,
            "prune_observations",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            Timestamp::now().as_microsecond(),
            &format!("{{\"deleted\":{deleted}}}"),
        )?;
    }
    tx.commit()?;
    Ok(ObservationPruneOutcome {
        deleted,
        sessions_touched,
        sessions_resynced,
    })
}

/// Insert a new handoff in state=open.
pub fn insert_handoff(conn: &mut Connection, h: &NewHandoff) -> StoreResult<HandoffId> {
    let tx = conn.transaction()?;
    let id = insert_handoff_row(&tx, h)?;
    tx.commit()?;
    Ok(id)
}

/// Atomically stamp a session ended and insert its automatic handoff.
///
/// A failed handoff insert rolls the end stamp back, so a keyed retry can run
/// the complete SessionEnd path instead of observing a partially ended session.
pub fn end_session_with_handoff(
    conn: &mut Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
    handoff: &NewHandoff,
) -> StoreResult<HandoffId> {
    if handoff.from_session_id.as_ref() != Some(session_id) {
        return Err(StoreError::InvalidState(
            "automatic handoff source does not match the ended session".into(),
        ));
    }
    let tx = conn.transaction()?;
    let session_scope: Option<(Vec<u8>, Vec<u8>, Option<String>)> = tx
        .query_row(
            "SELECT workspace_id, project_id, actor_user FROM sessions WHERE id = ?1",
            params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((workspace_id, project_id, actor_user)) = session_scope else {
        return Err(StoreError::InvalidState(
            "cannot end a missing session with an automatic handoff".into(),
        ));
    };
    if workspace_id.as_slice() != handoff.workspace_id.as_bytes()
        || project_id.as_slice() != handoff.project_id.as_bytes()
    {
        return Err(StoreError::InvalidState(
            "automatic handoff scope does not match the ended session".into(),
        ));
    }
    if actor_user != handoff.owner_user {
        return Err(StoreError::InvalidState(
            "automatic handoff owner does not match the ended session".into(),
        ));
    }
    end_session_row(&tx, session_id, summary_page_id)?;
    let id = insert_handoff_row(&tx, handoff)?;
    tx.commit()?;
    Ok(id)
}

/// Store-boundary bound for a single handoff prose field. Generous
/// (16 KiB, matching the observation body) — a backstop against
/// unbounded content, not a functional cap; callers cap tighter.
const HANDOFF_FIELD_MAX_BYTES: usize = 16 * 1024;
/// Store-boundary bound on the number of items in a handoff list field.
const HANDOFF_LIST_MAX_ITEMS: usize = 256;

fn bound_handoff_field(value: &str) -> String {
    sessionmunch_core::truncate_utf8_bytes(value, HANDOFF_FIELD_MAX_BYTES)
}

fn bound_handoff_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .take(HANDOFF_LIST_MAX_ITEMS)
        .map(|s| bound_handoff_field(s))
        .collect()
}

fn insert_handoff_row(conn: &Transaction<'_>, h: &NewHandoff) -> StoreResult<HandoffId> {
    validate_identity_storage_key(h.owner_user.as_deref(), "handoff owner")?;
    let id = HandoffId::new();
    let now = Timestamp::now().as_microsecond();
    // Store-boundary bound (defense in depth): the MCP/hook callers already
    // scrub and cap handoff prose, but the store is the last gate before
    // durable persistence — mirror the observation body's bound so a caller
    // that ever forgets cannot write unbounded content to the DB. Generous
    // enough never to fire under the callers' tighter caps.
    let summary = bound_handoff_field(&h.summary);
    let open_q = serde_json::to_string(&bound_handoff_list(&h.open_questions))?;
    let next_s = serde_json::to_string(&bound_handoff_list(&h.next_steps))?;
    let files = serde_json::to_string(&bound_handoff_list(&h.files_touched))?;
    let from_session: Option<&[u8]> = h.from_session_id.as_ref().map(|s| &s.as_bytes()[..]);
    // Normalize the stored cwd: strip trailing path separators (keep a bare root
    // as "/"). The hook extractor preserves whatever the agent payload sent,
    // so this single write point guarantees a consistent stored form for both
    // manual and auto (SessionEnd) handoffs, keeping the next session's
    // path-boundary match robust to trailing slash/backslash drift.
    let cwd: Option<String> = h.cwd.as_ref().map(|p| {
        let s = p.to_string_lossy();
        let trimmed = s.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    });
    let from_agent = h.from_agent.as_str();
    let to_agent = h.to_agent.map(AgentKind::as_str);
    // A newer automatic handoff from the exact same cwd is the only one that
    // can ever win there, even before a SessionStart occurs. Bound abandoned
    // same-directory sessions without touching deliberate manual handoffs or
    // independent parent/sibling cwd scopes — and without crossing an operator
    // boundary: the same directory inside a shared container is the norm, so
    // owner equality is the only thing keeping one operator's SessionEnd from
    // retiring another's pending baton.
    if from_session.is_some() {
        let expired = conn.execute(
            "UPDATE handoffs SET state = 'expired' \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND state = 'open' AND from_session_id IS NOT NULL \
               AND (cwd = ?3 OR (cwd IS NULL AND ?3 IS NULL)) \
               AND owner_user IS ?4",
            params![
                h.workspace_id.as_bytes(),
                h.project_id.as_bytes(),
                cwd,
                h.owner_user.as_deref()
            ],
        )?;
        if expired > 0 {
            audit(
                conn,
                "expire_superseded_handoffs",
                Some(h.workspace_id.as_bytes()),
                Some(h.project_id.as_bytes()),
                None,
                None,
                now,
            )?;
        }
    }
    // Insert + audit atomically. Handoffs are keyed by agent/session, not a DB
    // user, so the audit author is NULL — the row records the lifecycle event
    // (op + workspace/project + time), not an operator identity.
    conn.execute(
        "INSERT INTO handoffs \
         (id, workspace_id, project_id, from_session_id, from_agent, to_agent, cwd, summary, \
          open_questions, next_steps, files_touched, state, created_at, owner_user) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'open', ?12, ?13)",
        params![
            id.as_bytes(),
            h.workspace_id.as_bytes(),
            h.project_id.as_bytes(),
            from_session,
            from_agent,
            to_agent,
            cwd,
            summary,
            open_q,
            next_s,
            files,
            now,
            // NULL owner = shared with the whole project: what a caller with no
            // actor writes, and what every pre-V39 row already looks like.
            h.owner_user.as_deref(),
        ],
    )?;
    audit(
        conn,
        "insert_handoff",
        Some(h.workspace_id.as_bytes()),
        Some(h.project_id.as_bytes()),
        None,
        None,
        now,
    )?;
    Ok(id)
}

/// Mark a handoff accepted by `accepting_agent` / `accepting_session`.
///
/// Returns whether this call is the one that actually claimed it: `false` means
/// the row was already accepted/expired, or its owner does not admit this
/// caller. Callers must not hand the body to the agent on `false`, or a lost
/// race delivers the same baton twice.
///
/// `receiving_cwd` is where the *claiming* session is starting, not where the
/// claimed handoff came from; it bounds the post-claim sweep of stale automatic
/// handoffs.
pub fn accept_handoff(conn: &mut Connection, acceptance: &HandoffAcceptance) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let claimed = accept_handoff_in_transaction(&tx, acceptance)?;
    tx.commit()?;
    Ok(claimed)
}

pub(crate) fn accept_handoff_in_transaction(
    tx: &Transaction<'_>,
    acceptance: &HandoffAcceptance,
) -> StoreResult<bool> {
    let HandoffAcceptance {
        handoff_id,
        workspace_id,
        project_id,
        accepting_agent,
        accepting_session,
        accepting_user,
        owner_filter,
        receiving_cwd,
    } = acceptance;
    let accepting_user = accepting_user.as_deref();
    validate_identity_storage_key(accepting_user, "accepting handoff operator")?;
    match (owner_filter, accepting_user) {
        (OwnerFilter::User(expected), Some(actual)) if expected != actual => {
            return Err(StoreError::InvalidState(
                "accepting handoff operator does not match its owner filter".into(),
            ));
        }
        (OwnerFilter::User(_), None) => {
            return Err(StoreError::InvalidState(
                "an owner-scoped handoff claim requires an accepting operator".into(),
            ));
        }
        (OwnerFilter::Unattributed, Some(_)) => {
            return Err(StoreError::InvalidState(
                "an unattributed handoff claim cannot record a named operator".into(),
            ));
        }
        _ => {}
    }
    let now = Timestamp::now().as_microsecond();
    let agent = accepting_agent.as_str();
    let session: Option<&[u8]> = accepting_session.as_ref().map(|s| &s.as_bytes()[..]);
    if let Some(accepting_session) = accepting_session {
        let receiver: Option<(bool, bool)> = tx
            .query_row(
                "SELECT workspace_id = ?2 AND project_id = ?3 AND agent_kind = ?4, \
                        ended_at IS NULL \
                 FROM sessions WHERE id = ?1",
                params![
                    accepting_session.as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    accepting_agent.as_str(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((matching_scope_and_agent, open)) = receiver else {
            return Err(StoreError::InvalidState(
                "handoff receiver session does not exist".into(),
            ));
        };
        if !matching_scope_and_agent {
            return Err(StoreError::InvalidState(
                "handoff receiver session does not match the accepting scope and agent".into(),
            ));
        }
        if !open {
            return Err(StoreError::InvalidState(
                "an ended session cannot accept a handoff".into(),
            ));
        }
        let already_claimed: bool = tx.query_row(
            "SELECT EXISTS( \
                 SELECT 1 FROM handoffs \
                 WHERE state = 'accepted' AND accepted_by_session = ?1 \
             )",
            params![accepting_session.as_bytes()],
            |row| row.get(0),
        )?;
        if already_claimed {
            // SessionStart can be retried. One receiver must never consume a
            // second baton, or an empty end could return only one and strand
            // the first accepted row.
            return Ok(false);
        }
    }
    let metadata = tx
        .query_row(
            "SELECT from_session_id IS NOT NULL, cwd, created_at, owner_user \
             FROM handoffs \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND state = 'open'",
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, bool>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((automatic, cwd, created_at, owner_user)) = metadata else {
        return Ok(false);
    };
    // The ownership check rides along in the UPDATE's WHERE rather than being a
    // separate read: the claim stays a single atomic compare-and-set (only one
    // racing session can flip 'open' -> 'accepted'), and a caller who is not
    // allowed to take this baton simply changes 0 rows.
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?8)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'accepted', accepted_by = ?1, accepted_at = ?2, \
         accepted_by_session = ?3, accepted_by_user = ?4 \
         WHERE id = ?5 AND workspace_id = ?6 AND project_id = ?7 \
           AND state = 'open'{owner_clause}"
    );
    let changed = match owner_filter {
        OwnerFilter::User(user) => tx.execute(
            &sql,
            params![
                agent,
                now,
                session,
                accepting_user,
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                user
            ],
        )?,
        _ => tx.execute(
            &sql,
            params![
                agent,
                now,
                session,
                accepting_user,
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )?,
    };
    // Only a real state transition ('open' -> 'accepted') is audited.
    if changed > 0 {
        audit(
            tx,
            "accept_handoff",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            now,
        )?;
        if automatic {
            // The sweep inherits the claimed handoff's owner: retiring stale
            // batons must never reach across an operator boundary, or one
            // person's session start silently destroys another's pending
            // handoff — a worse failure than the misdelivery ownership exists
            // to prevent, because nothing is delivered at all.
            let expired = expire_superseded_auto_handoffs(
                tx,
                handoff_id,
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                cwd.as_deref(),
                created_at,
                receiving_cwd.as_deref(),
                owner_user.as_deref(),
            )?;
            if expired > 0 {
                audit(
                    tx,
                    "expire_superseded_handoffs",
                    Some(workspace_id.as_bytes()),
                    Some(project_id.as_bytes()),
                    None,
                    None,
                    now,
                )?;
            }
        }
    }
    Ok(changed > 0)
}

fn validate_identity_storage_key(value: Option<&str>, label: &str) -> StoreResult<()> {
    if value.is_some_and(|value| IdentityKey::from_storage_key(value).is_none()) {
        return Err(StoreError::InvalidState(format!(
            "{label} is not a qualified identity storage key"
        )));
    }
    Ok(())
}

/// Expire older automatic handoffs that were eligible for the same receiving
/// cwd as the accepted handoff. Manual and sibling-directory handoffs are
/// deliberately excluded, and so is every handoff belonging to a different
/// operator: a NULL owner is visible to *everyone*, so expiring one on Alice's
/// behalf would take it away from Bob too. Equality on `owner_user` keeps the
/// unattributed single-operator case (every row NULL) behaving exactly as it
/// does without ownership.
#[allow(clippy::too_many_arguments)]
fn expire_superseded_auto_handoffs(
    tx: &Transaction<'_>,
    accepted_id: &HandoffId,
    workspace_id: &[u8],
    project_id: &[u8],
    accepted_cwd: Option<&str>,
    accepted_created_at: i64,
    receiving_cwd: Option<&str>,
    accepted_owner: Option<&str>,
) -> StoreResult<usize> {
    let receiving_cwd = receiving_cwd.or(accepted_cwd);
    let accepted_key = crate::reader::handoff_selection_key(
        false,
        accepted_created_at,
        accepted_cwd,
        *accepted_id,
    );
    let mut stmt = tx.prepare(
        "SELECT id, cwd, created_at FROM handoffs \
         WHERE workspace_id = ?1 AND project_id = ?2 \
           AND state = 'open' AND from_session_id IS NOT NULL \
           AND owner_user IS ?3",
    )?;
    let rows = stmt.query_map(params![workspace_id, project_id, accepted_owner], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let (id_bytes, cwd, created_at) = row?;
        let id = HandoffId::from_slice(&id_bytes)?;
        if crate::reader::auto_handoff_matches_cwd(cwd.as_deref(), receiving_cwd)
            && crate::reader::handoff_selection_key(false, created_at, cwd.as_deref(), id)
                < accepted_key
        {
            ids.push(id_bytes);
        }
    }
    drop(stmt);

    let mut expired = 0;
    for chunk in ids.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        expired += tx.execute(
            &format!(
                "UPDATE handoffs SET state = 'expired' \
                 WHERE state = 'open' AND id IN ({placeholders})"
            ),
            rusqlite::params_from_iter(chunk.iter().map(Vec::as_slice)),
        )?;
    }
    Ok(expired)
}

/// Expire every open handoff in one scope, optionally only those older than
/// `older_than_us`. Returns how many rows changed.
///
/// # Why this ignores the automatic expiry's exemptions
///
/// [`expire_superseded_auto_handoffs`] deliberately spares two kinds of open
/// handoff: manual ones (`from_session_id IS NULL`, written by a person on
/// purpose) and ones whose `cwd` does not match the accepting session's. Those
/// exemptions are correct for an automatic sweep triggered by an unrelated
/// accept.
///
/// They also mean the leftover backlog consists, by construction, of exactly
/// the handoffs the sweep will never touch. An operator-driven bulk expiry
/// that honoured the same exemptions would therefore expire nothing at all —
/// it would be a no-op for the only situation it exists to resolve. So this
/// applies to every open handoff in scope, which is what an operator asking to
/// clear a backlog is asking for.
///
/// Owner scoping still applies, and lives inside the `UPDATE` rather than a
/// preceding read, so expiring another user's baton is a zero-row no-op rather
/// than a read-then-write race — the same shape [`cancel_handoff`] uses.
///
/// This is a state change, not a delete: the summary, provenance and
/// timestamps survive. An expired handoff simply stops being consumable.
///
/// # Errors
/// Propagates SQL errors.
pub fn expire_open_handoffs(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    owner_filter: &OwnerFilter,
    older_than_us: Option<i64>,
    author_id: Option<sessionmunch_core::UserId>,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = :owner)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let age_clause = if older_than_us.is_some() {
        " AND created_at <= :cutoff"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'expired' \
         WHERE workspace_id = :ws AND project_id = :proj \
           AND state = 'open'{owner_clause}{age_clause}"
    );

    let ws_bytes: Vec<u8> = workspace_id.as_bytes().to_vec();
    let proj_bytes: Vec<u8> = project_id.as_bytes().to_vec();
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
        (":ws", &ws_bytes as &dyn rusqlite::ToSql),
        (":proj", &proj_bytes as &dyn rusqlite::ToSql),
    ];
    let owner_value;
    if let OwnerFilter::User(user) = owner_filter {
        owner_value = user.clone();
        params.push((":owner", &owner_value as &dyn rusqlite::ToSql));
    }
    let cutoff_value;
    if let Some(cutoff) = older_than_us {
        cutoff_value = cutoff;
        params.push((":cutoff", &cutoff_value as &dyn rusqlite::ToSql));
    }

    let changed = tx.execute(&sql, params.as_slice())? as u64;

    if changed > 0 {
        audit_with_detail(
            &tx,
            "expire_open_handoffs",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
            Timestamp::now().as_microsecond(),
            &format!("{{\"expired\":{changed}}}"),
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Mark an open handoff expired so it will no longer be consumed.
pub fn cancel_handoff(
    conn: &mut Connection,
    handoff_id: &HandoffId,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    owner_filter: &OwnerFilter,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    // Same reasoning as the accept path: the owner predicate lives in the
    // UPDATE so cancelling somebody else's baton is a 0-row no-op instead of a
    // read-then-write race.
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?4)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'expired' \
         WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
           AND state = 'open'{owner_clause}"
    );
    let changed = match owner_filter {
        OwnerFilter::User(user) => tx.execute(
            &sql,
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                user,
            ],
        )?,
        _ => tx.execute(
            &sql,
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )?,
    };
    if changed > 0 {
        audit(
            &tx,
            "cancel_handoff",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            now,
        )?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

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

/// Retro-fit sessions + observations to per-cwd projects and graveyard
/// any `is_latest=1` pages (which are mash-ups across the old single-project
/// bucket). Executes atomically in one transaction.
///
/// `plan` contains `(session_id, new_project_id)` pairs. Sessions not in
/// the plan are left untouched. Pages are graveyarded unconditionally so a
/// fresh consolidation can regenerate clean per-project pages.
pub fn reorg_sessions(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    plan: &[(SessionId, ProjectId)],
) -> StoreResult<ReorgSummary> {
    if plan.is_empty() {
        return Ok(ReorgSummary::default());
    }
    let tx = conn.transaction()?;
    let mut sessions_moved = 0usize;
    let mut observations_updated = 0usize;
    for (session_id, new_project_id) in plan {
        let rows = tx.execute(
            "UPDATE sessions
             SET project_id = ?1
             WHERE id = ?2 AND workspace_id = ?3 AND project_id != ?1",
            params![
                new_project_id.as_bytes(),
                session_id.as_bytes(),
                workspace_id.as_bytes()
            ],
        )?;
        sessions_moved += rows;
        // Update observations whose session_id matches, keeping project_id
        // in sync with the session row we just moved.
        let obs_rows = tx.execute(
            "UPDATE observations SET project_id = ?1 WHERE session_id = ?2 AND workspace_id = ?3",
            params![
                new_project_id.as_bytes(),
                session_id.as_bytes(),
                workspace_id.as_bytes()
            ],
        )?;
        observations_updated += obs_rows;
    }
    // Graveyard only this workspace's latest pages; sibling workspaces may
    // have already-consolidated pages that must remain current. Both
    // grains close at one shared instant (issue #656).
    let retire_at = Timestamp::now().as_microsecond();
    tx.execute(
        "UPDATE entity_page_links SET superseded_at = ?2 \
         WHERE superseded_at IS NULL AND page_id IN ( \
             SELECT id FROM pages WHERE workspace_id = ?1 AND is_latest = 1)",
        params![workspace_id.as_bytes(), retire_at],
    )?;
    let pages_graveyarded: usize = tx.execute(
        "UPDATE pages SET is_latest = 0, valid_to = ?2 \
         WHERE workspace_id = ?1 AND is_latest = 1",
        params![workspace_id.as_bytes(), retire_at],
    )?;
    tx.commit()?;
    Ok(ReorgSummary {
        sessions_moved,
        observations_updated,
        pages_graveyarded,
    })
}

/// Rename a project within its workspace.
///
/// Only the `name` column is updated — all pages, sessions, observations,
/// and handoffs remain associated with the same `project_id`. No files
/// move on disk (the wiki is flat: every page from every project lives
/// under `wiki/`; only the `project_id` foreign key distinguishes them).
///
/// # Errors
/// - [`StoreError::InvalidProjectName`] when `new_name` is empty,
///   contains a `/` character, or is all whitespace.
/// - [`StoreError::ProjectNameTaken`] when a project with `new_name`
///   already exists in the same workspace.
/// - [`StoreError::Sqlite`] on any other SQL failure.
pub fn rename_project(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    new_name: &str,
    author_id: Option<sessionmunch_core::UserId>,
) -> StoreResult<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidProjectName(
            "project name must not be empty or all whitespace".into(),
        ));
    }
    if trimmed.contains('/') {
        return Err(StoreError::InvalidProjectName(
            "project name must not contain '/' (it appears in URL paths)".into(),
        ));
    }

    // Wrap the UPDATE + audit row in one transaction so the trail can never
    // diverge from the rename it records (on any error the tx drops without
    // commit, rolling both back).
    let tx = conn.transaction()?;
    let rows = tx.execute(
        "UPDATE projects SET name = ?1 WHERE id = ?2 AND workspace_id = ?3",
        params![trimmed, project_id.as_bytes(), workspace_id.as_bytes()],
    );

    match rows {
        // Zero rows affected means the project row vanished between the
        // admin handler's `lookup_ws_proj_no_create` and this UPDATE —
        // the classic shape is a concurrent `purge-project` racing the
        // rename. Without this check, the rename would happily return
        // `Ok(())` and the admin handler would respond `200 OK` for an
        // operation that touched nothing, contradicting the purge's
        // (also `200 OK`) destruction of the same row.
        Ok(0) => Err(StoreError::NotFound(format!(
            "project id {project_id} no longer exists in workspace {workspace_id} \
             (race with concurrent purge or delete)",
        ))),
        Ok(_) => {
            audit(
                &tx,
                "rename_project",
                Some(workspace_id.as_bytes()),
                Some(project_id.as_bytes()),
                None,
                author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
                Timestamp::now().as_microsecond(),
            )?;
            tx.commit()?;
            Ok(())
        }
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(StoreError::ProjectNameTaken(trimmed.to_string()))
        }
        Err(e) => Err(StoreError::Sqlite(e)),
    }
}

/// Record a successfully-applied wiki-structure migration.
///
/// Uses `INSERT OR IGNORE` so re-running the same name is a no-op
/// (idempotent by design — the runner already skips known names, but
/// this guards against any concurrent writes).
pub fn insert_wiki_migration(
    conn: &mut Connection,
    name: &str,
    applied_at: i64,
) -> StoreResult<()> {
    conn.execute(
        "INSERT OR IGNORE INTO wiki_migrations (name, applied_at) VALUES (?1, ?2)",
        params![name, applied_at],
    )?;
    Ok(())
}

/// Whether a destructive scope operation should reclaim the freed bytes
/// afterwards. Shared by [`purge_session`], [`purge_project`] and
/// [`delete_workspace`] so the family makes one promise, not three.
///
/// A purge is a logical delete: the rows are gone and nothing can reach them
/// through the API or through search, but their bytes remain in free pages of
/// the database file until the file is rewritten — exactly as for any other
/// SQLite delete.
///
/// [`Compaction::Reclaim`] additionally rebuilds the affected FTS5 indexes
/// (the tokens survive an ordinary delete inside the index segments) and runs
/// `VACUUM`, after the deletion has committed.
///
/// # Cost and limits
///
/// `VACUUM` rewrites the entire database and needs free disk space of roughly
/// the database's own size. On a multi-gigabyte store this is minutes, not
/// milliseconds, so it is opt-in rather than the default.
///
/// It also does **not** make the content forensically unrecoverable. The wiki
/// git repository keeps page text in its objects and commit messages, and any
/// backup taken before the purge still contains it. Reclaiming bytes from the
/// live SQLite file is worth doing on its own terms; it is not a guarantee of
/// erasure everywhere, and must not be described as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    /// Leave the freed pages alone. The default, and what #387 promises.
    Skip,
    /// Rebuild the affected FTS indexes and `VACUUM` after committing.
    Reclaim,
}

/// Every FTS5 index in the schema, which is what [`reclaim_freed_pages`]
/// rebuilds.
///
/// Named here rather than spelled inside the SQL so the "every" is checkable:
/// `reclaim_rebuilds_every_fts_index_in_the_schema` compares this list against
/// `sqlite_master` and fails when a new index is added without a decision
/// being made about it. A list inside a string literal cannot be compared to
/// anything, and the failure mode of getting it wrong is silent — text stays
/// searchable after an operator asked for it to be reclaimed.
const ALL_FTS_INDEXES: &[&str] = &[
    "observations_fts",
    "page_passages_fts",
    "pages_fts",
    "session_events_fts",
    "workstream_events_fts",
];

/// Rebuild every FTS5 index, then `VACUUM`. Runs after the caller's
/// transaction has committed: `VACUUM` cannot run inside one, and a rebuild is
/// a whole-index operation we do not want holding the write lock for the
/// duration of a delete.
///
/// All three indexes are rebuilt regardless of which scope was deleted. They
/// are `content=`-backed external-content tables, so an ordinary delete leaves
/// the tokens behind inside the index segments and only a `rebuild` clears
/// them — and a project or workspace delete cascades `workstream_events` just
/// as it does `pages` and `observations`. Rebuilding only the indexes a given
/// caller "should" have touched is exactly the reasoning that leaves text
/// searchable after a purge, and the cost is dominated by the `VACUUM` that
/// follows in every case. A `rebuild` is idempotent, so the extra work is a
/// no-op for a scope that deleted nothing from that table.
pub(crate) fn reclaim_freed_pages(conn: &Connection) -> StoreResult<()> {
    let mut batch = String::new();
    for index in ALL_FTS_INDEXES {
        // Compile-time constants, never caller input; FTS5 has no
        // bind-parameter form for the rebuild statement.
        batch.push_str(&format!("INSERT INTO {index}({index}) VALUES('rebuild'); "));
    }
    batch.push_str("VACUUM;");
    conn.execute_batch(&batch)?;
    Ok(())
}

/// What one [`compact`] run reclaimed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompactSummary {
    /// Database size in bytes before the rebuild + `VACUUM`.
    pub bytes_before: u64,
    /// Database size in bytes afterwards.
    pub bytes_after: u64,
}

impl CompactSummary {
    /// Bytes returned to the filesystem. Saturating: a `VACUUM` can leave the
    /// file marginally larger when defragmentation costs more than the free
    /// pages it released, and that is a zero, not a negative.
    #[must_use]
    pub fn bytes_reclaimed(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Reclaim free pages on demand, deleting nothing.
///
/// Same operation the destructive commands offer as `--compact`, reachable
/// without first deleting something. Until this existed, an operator whose
/// database had accumulated free pages — a large forget-sweep, a retention
/// pass, months of superseded page versions — had no way to return the space
/// except by purging data they wanted to keep.
///
/// # Cost
///
/// `VACUUM` takes an exclusive lock and rewrites the whole file, so every
/// write blocks for the duration and it needs free disk space of roughly the
/// database's own size. Read [`crate::reader::StorageStatus::reclaimable_bytes`]
/// first and run this when it is worth the stall — not on a blind schedule.
///
/// # Errors
/// Propagates the SQL error from the rebuild or the `VACUUM`.
pub fn compact(conn: &mut Connection) -> StoreResult<CompactSummary> {
    let bytes_before = database_bytes(conn)?;
    reclaim_freed_pages(conn)?;
    let bytes_after = database_bytes(conn)?;
    Ok(CompactSummary {
        bytes_before,
        bytes_after,
    })
}

/// `page_count * page_size` — the database's own view of its size, which is
/// what `VACUUM` acts on. Deliberately not the filesystem size: a `-wal`
/// sidecar holds committed pages not yet checkpointed into the main file, so
/// `metadata().len()` would move for reasons compaction has nothing to do with.
pub(crate) fn database_bytes(conn: &Connection) -> StoreResult<u64> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok((page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64))
}

/// What [`purge_session`] removed. All counts are rows actually deleted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeSessionSummary {
    /// `observations` rows removed.
    pub observations_deleted: u64,
    /// `handoffs` rows removed — only those this session *authored*.
    pub handoffs_deleted: u64,
    /// `pages` rows removed, counting every version in the supersession chain.
    pub pages_deleted: u64,
    /// `auto_improve_runs` rows removed.
    pub auto_improve_runs_deleted: u64,
    /// On-disk wiki paths whose rows are gone, for the caller to unlink.
    pub removed_paths: Vec<PagePath>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran).
    pub compacted: bool,
}

/// Delete one session and everything addressable from it, within one scope.
///
/// # Scope containment
///
/// This is a destructive operation driven by a caller-supplied id, so the
/// scope is enforced structurally rather than trusted:
///
/// - the session must exist **and** belong to `(workspace_id, project_id)`;
///   a session id from another scope is [`StoreError::NotFound`] and nothing
///   is deleted;
/// - every statement is additionally filtered on `workspace_id` and
///   `project_id` wherever the table carries them, so even a mismatched id
///   cannot reach a row in another workspace or project;
/// - derived pages are deleted by **id**, from a set collected and
///   scope-checked first — never by path. Two projects may hold the same
///   `sessions/<uuid>.md` path, and a hand-written page can occupy the path a
///   session later claims; deleting by path would take those with it.
///
/// # Ordering
///
/// `auto_improve_runs.session_id`, `handoffs.from_session_id`,
/// `handoffs.accepted_by_session` and `pages.supersedes` are all
/// `ON DELETE SET NULL`. Cutting the session first nulls the pointers needed
/// to find what it produced, so every target is collected before anything is
/// deleted.
///
/// # What is deliberately *not* deleted
///
/// Handoffs this session **accepted** but did not author. Their summary text
/// belongs to the authoring session; accepting one does not transfer
/// ownership, and removing it would destroy another session's record. Those
/// rows keep their content and lose only the `accepted_by_session` pointer,
/// via the existing `ON DELETE SET NULL`.
pub fn purge_session(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    author_id: Option<sessionmunch_core::UserId>,
    compaction: Compaction,
) -> StoreResult<PurgeSessionSummary> {
    let wid = workspace_id.as_bytes();
    let pid = project_id.as_bytes();
    let sid = session_id.as_bytes();
    let tx = conn.transaction()?;

    // The session must live in this scope. A mismatch is NotFound, not a
    // silent no-op: the caller asked to delete something and must learn that
    // it did not happen.
    let in_scope: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sessions \
         WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
        |row| row.get(0),
    )?;
    if in_scope == 0 {
        return Err(StoreError::NotFound(format!(
            "session {session_id} not found in this workspace/project"
        )));
    }

    // ---- collect, before anything is cut ----

    // The derived page and every version of it. Walk from the recorded
    // summary page across the supersession chain, staying inside the scope.
    let page_ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "WITH RECURSIVE chain(id) AS ( \
                 SELECT summary_page_id FROM sessions \
                  WHERE id = ?1 AND summary_page_id IS NOT NULL \
                 UNION \
                 SELECT p.id FROM pages p JOIN chain c ON p.supersedes = c.id \
             ) \
             SELECT p.id FROM pages p JOIN chain c ON p.id = c.id \
              WHERE p.workspace_id = ?2 AND p.project_id = ?3",
        )?;
        stmt.query_map(rusqlite::params![&sid[..], &wid[..], &pid[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let removed_paths: Vec<PagePath> = if page_ids.is_empty() {
        Vec::new()
    } else {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT path FROM pages \
              WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        )?;
        let mut paths = Vec::new();
        for id in &page_ids {
            let found: Option<String> = stmt
                .query_row(rusqlite::params![&id[..], &wid[..], &pid[..]], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(path) = found {
                let path = PagePath::new(path)?;
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        paths
    };

    let run_ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM auto_improve_runs \
              WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        )?;
        stmt.query_map(rusqlite::params![&sid[..], &wid[..], &pid[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    // ---- delete, innermost first ----

    for run in &run_ids {
        tx.execute(
            "DELETE FROM auto_improve_rejections \
              WHERE source_run_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            rusqlite::params![&run[..], &wid[..], &pid[..]],
        )?;
    }
    let auto_improve_runs_deleted = tx.execute(
        "DELETE FROM auto_improve_runs \
          WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )? as u64;

    // Authored only. See the note above about accepted handoffs.
    let handoffs_deleted = tx.execute(
        "DELETE FROM handoffs \
          WHERE from_session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )? as u64;

    // Pages by id, scoped. Cascades `page_embeddings` and fires `pages_fts_ad`.
    let mut pages_deleted = 0u64;
    for id in &page_ids {
        pages_deleted += tx.execute(
            "DELETE FROM pages WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            rusqlite::params![&id[..], &wid[..], &pid[..]],
        )? as u64;
    }

    // Deleted explicitly rather than left to the cascade so the row count is
    // known and can be reported. Measured: this does *not* change what the
    // FTS index retains — with an external-content table the cascade already
    // makes the text unsearchable, and the tokens stay in the FTS5 segments
    // either way until a `rebuild`. See the scope note on this function.
    let observations_deleted = tx.execute(
        "DELETE FROM observations \
          WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )? as u64;

    // Tombstone before the row goes, in the same transaction: a purge that
    // committed the deletion but not the tombstone would be undone by the
    // next spool drain (#387).
    tx.execute(
        "INSERT INTO purged_sessions (session_id, workspace_id, project_id, purged_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(session_id) DO UPDATE SET purged_at = excluded.purged_at",
        rusqlite::params![
            &sid[..],
            &wid[..],
            &pid[..],
            Timestamp::now().as_microsecond()
        ],
    )?;

    // Finally the session itself; cascades scheduler claims and
    // consolidation jobs, which hold no content of their own.
    tx.execute(
        "DELETE FROM sessions WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        rusqlite::params![&sid[..], &wid[..], &pid[..]],
    )?;

    audit(
        &tx,
        "purge_session",
        Some(wid),
        Some(pid),
        None,
        author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
        Timestamp::now().as_microsecond(),
    )?;

    tx.commit()?;

    if compaction == Compaction::Reclaim {
        reclaim_freed_pages(conn)?;
    }

    Ok(PurgeSessionSummary {
        observations_deleted,
        handoffs_deleted,
        pages_deleted,
        auto_improve_runs_deleted,
        removed_paths,
        compacted: compaction == Compaction::Reclaim,
    })
}

/// Delete a project's rows inside one transaction.
///
/// This is a *logical* delete unless `compaction` is [`Compaction::Reclaim`].
/// The rows are gone and nothing reaches them through the API or search, but
/// their bytes remain in free pages of the database file until it is
/// rewritten — as with any SQLite delete. Neither mode is forensic erasure:
/// the wiki git history and any earlier backup still hold the content.
///
/// Execution order:
/// 1. Count rows in each dependent table (pages/all versions, sessions,
///    observations, handoffs, embeddings) before the delete so we can
///    report how many rows were removed.
/// 2. Collect all distinct page paths stored under the project — these are
///    the on-disk files the caller must clean up after this function returns.
/// 3. DELETE FROM projects WHERE id = ? — the ON DELETE CASCADE clauses in
///    V01 + V02 propagate the delete to pages, sessions, observations,
///    handoffs, and page_embeddings automatically.
/// 4. Commit and return the [`PurgeSummary`].
///
/// The `workspace_project_label` string is passed in by the caller (the
/// admin handler has the human-readable names; the writer only has IDs) and
/// forwarded verbatim into [`PurgeSummary::label`] for logging.
///
/// `force` overrides the live-managed-run guard (step 0): `workstreams`
/// cascades out of `projects`, so purging a scope whose lease is still live
/// would delete the lease row out from under a running agent.
///
/// # Errors
/// Returns [`StoreError::ManagedRunActive`] when a managed run's lease is
/// still live and `force` is false, or [`StoreError`] if any SQL statement
/// fails. The transaction is rolled back automatically on error.
/// Whether `(workspace_id, project_id)` — or the whole workspace — was purged
/// and tombstoned by [`purge_project`] / [`delete_workspace`] (#607).
///
/// Returns true when either an exact project tombstone exists or a
/// whole-workspace tombstone (the `zeroblob(16)` sentinel project id) covers
/// the workspace. `reindex` calls this before recreating a scope from on-disk
/// `_meta.md`, so a purge whose file removal crashed cannot be resurrected.
pub fn scope_is_purged(
    conn: &Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> StoreResult<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM purged_scopes \
             WHERE workspace_id = ?1 AND (project_id = ?2 OR project_id = zeroblob(16)) \
             LIMIT 1",
            rusqlite::params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Session ids tombstoned by `purge_session` in one scope.
///
/// The scope-level twin of [`scope_is_purged`], read the same way and for the
/// same reason: a purge whose page-file removal did not complete leaves the
/// markdown on disk, and the wiki reindex must not put it back (#701). Loaded
/// once per directory per reindex pass rather than once per page — a purged
/// scope is one row, but a project accumulates one purged session per purge.
///
/// # Errors
/// Propagates SQL errors.
pub fn purged_session_ids(
    conn: &Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> StoreResult<Vec<SessionId>> {
    let mut stmt = conn.prepare(
        "SELECT session_id FROM purged_sessions \
         WHERE workspace_id = ?1 AND project_id = ?2",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![workspace_id.as_bytes(), project_id.as_bytes()],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(SessionId::from_slice(&row?)?);
    }
    Ok(out)
}

/// One recorded bootstrap chunk, as loaded by [`load_bootstrap_progress`].
#[derive(Debug, Clone)]
pub struct BootstrapChunkRecord {
    /// Position of this chunk in the run's chunk plan (0-based).
    pub chunk_index: u32,
    /// The chunk's `BootstrapPage` batch, serialized as JSON.
    pub pages_json: String,
    /// The chunk's LLM-authored rationale.
    pub rationale: String,
}

/// Durably record one completed bootstrap chunk's output (#621).
///
/// `INSERT OR REPLACE` makes this idempotent: a non-resume run still records
/// every chunk (so a later `--resume` has something to reuse), and re-running
/// the same chunk index just overwrites its row.
///
/// # Errors
/// Returns [`StoreError`] if the SQL statement fails.
pub fn record_bootstrap_chunk(
    conn: &Connection,
    fingerprint: &str,
    chunk_index: u32,
    pages_json: &str,
    rationale: &str,
) -> StoreResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO bootstrap_chunk_progress \
         (fingerprint, chunk_index, pages_json, rationale, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            fingerprint,
            chunk_index,
            pages_json,
            rationale,
            Timestamp::now().as_microsecond(),
        ],
    )?;
    Ok(())
}

/// Load every recorded chunk for `fingerprint`, ordered by `chunk_index` — the
/// order `bootstrap --resume` needs to seed its `pages_by_path` accumulator.
///
/// # Errors
/// Returns [`StoreError`] if the SQL statement fails.
pub fn load_bootstrap_progress(
    conn: &Connection,
    fingerprint: &str,
) -> StoreResult<Vec<BootstrapChunkRecord>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_index, pages_json, rationale FROM bootstrap_chunk_progress \
         WHERE fingerprint = ?1 ORDER BY chunk_index",
    )?;
    let rows = stmt
        .query_map(params![fingerprint], |row| {
            Ok(BootstrapChunkRecord {
                chunk_index: row.get(0)?,
                pages_json: row.get(1)?,
                rationale: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Delete every recorded chunk for `fingerprint`. Called once a bootstrap run
/// completes successfully — a fresh run has nothing left to resume.
///
/// # Errors
/// Returns [`StoreError`] if the SQL statement fails.
pub fn clear_bootstrap_progress(conn: &Connection, fingerprint: &str) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM bootstrap_chunk_progress WHERE fingerprint = ?1",
        params![fingerprint],
    )?;
    Ok(())
}

pub fn purge_project(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    workspace_project_label: &str,
    author_id: Option<sessionmunch_core::UserId>,
    force: bool,
    compaction: Compaction,
) -> StoreResult<PurgeSummary> {
    let tx = conn.transaction()?;

    let count = |sql: &str, id: &[u8]| -> StoreResult<u64> {
        let n: Option<i64> = tx
            .query_row(sql, rusqlite::params![id], |row| row.get(0))
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };

    let pid = project_id.as_bytes();

    // Managed workstreams cascade out of `projects`, and `managed_runs`
    // cascades out of them. A live run's lease row would go with them, which
    // leaves the running agent heartbeating a run id that no longer exists —
    // `409 managed run lease is not active`, every 30s, with its transcript
    // unable to reach any ledger. Refuse unless the operator insists.
    //
    // `state = 'active'` alone is NOT liveness: a crashed wrapper leaves that
    // row untouched, and the only sweep that flips it to `'expired'` runs
    // inside `workstream::prepare_run`. Without the lease-expiry predicate a
    // single crashed agent would block every future purge of the project
    // until someone launched another managed run. The lease is short (90s,
    // extended by heartbeat), so `lease_expires_at > now` is the real signal.
    let now = Timestamp::now().as_microsecond();
    let active_runs: Vec<(String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT w.name, mr.agent_kind FROM managed_runs mr \
             JOIN workstreams w ON w.id = mr.workstream_id \
             WHERE w.project_id = ?1 AND mr.state = 'active' \
               AND mr.lease_expires_at > ?2",
        )?;
        stmt.query_map(rusqlite::params![&pid[..], now], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if !active_runs.is_empty() && !force {
        let mut names: Vec<String> = active_runs
            .iter()
            .map(|(workstream, agent)| format!("'{workstream}' ({agent})"))
            .collect();
        names.sort();
        names.dedup();
        return Err(StoreError::ManagedRunActive {
            count: active_runs.len() as u64,
            workstreams: names.join(", "),
        });
    }
    let pages_deleted = count("SELECT COUNT(*) FROM pages WHERE project_id = ?1", &pid[..])?;
    let sessions_deleted = count(
        "SELECT COUNT(*) FROM sessions WHERE project_id = ?1",
        &pid[..],
    )?;
    let observations_deleted = count(
        "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
        &pid[..],
    )?;
    let handoffs_deleted = count(
        "SELECT COUNT(*) FROM handoffs WHERE project_id = ?1",
        &pid[..],
    )?;
    // page_embeddings cascade through pages; count pages that have them.
    let embeddings_deleted = count(
        "SELECT COUNT(*) FROM page_embeddings \
         WHERE page_id IN (SELECT id FROM pages WHERE project_id = ?1)",
        &pid[..],
    )?;

    let workstreams_deleted = count(
        "SELECT COUNT(*) FROM workstreams WHERE project_id = ?1",
        &pid[..],
    )?;
    let managed_runs_deleted = count(
        "SELECT COUNT(*) FROM managed_runs WHERE workstream_id IN \
         (SELECT id FROM workstreams WHERE project_id = ?1)",
        &pid[..],
    )?;

    // Collect all distinct on-disk paths for the caller to clean up.
    // We use DISTINCT because multiple versions of the same logical page
    // share a path; the file only exists once. The statement must be
    // dropped before we call tx.commit() to release the borrow on `tx`.
    let page_paths: Vec<String> = {
        let mut path_stmt = tx.prepare("SELECT DISTINCT path FROM pages WHERE project_id = ?1")?;
        path_stmt
            .query_map(rusqlite::params![&pid[..]], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?
    };

    // Same idea for the workstream segment directories: the rows go with the
    // cascade but `raw/workstreams/<id>/` needs post-commit filesystem
    // cleanup. Decode through the typed id so the reported string matches the
    // directory name `write_segment` builds from `WorkstreamId::to_string`; a
    // malformed blob is a corrupt row, so surface it rather than silently
    // shortening the cleanup list.
    let workstream_ids: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM workstreams WHERE project_id = ?1")?;
        let rows = stmt
            .query_map(rusqlite::params![&pid[..]], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|raw| sessionmunch_core::WorkstreamId::from_slice(&raw).map(|id| id.to_string()))
            .collect::<Result<Vec<_>, _>>()?
    };

    // Cascade handles pages / sessions / observations / handoffs /
    // page_embeddings. The workspace row is intentionally left intact —
    // other projects may still live there.
    tx.execute(
        "DELETE FROM projects WHERE id = ?1 AND workspace_id = ?2",
        rusqlite::params![&pid[..], workspace_id.as_bytes()],
    )?;

    // Tombstone the scope so a `reindex` cannot resurrect it from an on-disk
    // directory the post-commit file removal failed to (or crashed before)
    // deleting. Written in the same transaction as the DELETE so the deletion
    // and its terminality commit atomically (#607). `INSERT OR REPLACE` keeps
    // a repeated purge idempotent and refreshes `purged_at`.
    tx.execute(
        "INSERT OR REPLACE INTO purged_scopes (workspace_id, project_id, purged_at) \
         VALUES (?1, ?2, ?3)",
        rusqlite::params![workspace_id.as_bytes(), &pid[..], now],
    )?;

    // Attributed audit trail for the destructive purge. `page_id` is None
    // (the whole project is gone); the operator identity comes from the
    // authenticated request (NULL when single-user / unauthenticated).
    audit(
        &tx,
        "purge_project",
        Some(workspace_id.as_bytes()),
        Some(project_id.as_bytes()),
        None,
        author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
        Timestamp::now().as_microsecond(),
    )?;

    tx.commit()?;

    if compaction == Compaction::Reclaim {
        reclaim_freed_pages(conn)?;
    }

    Ok(PurgeSummary {
        label: workspace_project_label.to_string(),
        page_paths,
        pages_deleted,
        sessions_deleted,
        observations_deleted,
        handoffs_deleted,
        embeddings_deleted,
        workstreams_deleted,
        managed_runs_deleted,
        workstream_ids,
        compacted: compaction == Compaction::Reclaim,
    })
}

/// Summary returned by [`delete_workspace`].
#[derive(Debug, Default, Clone)]
pub struct DeleteWorkspaceSummary {
    /// Projects removed (0 when the workspace was already empty).
    pub projects_deleted: u64,
    /// `pages` rows removed via cascade (all versions).
    pub pages_deleted: u64,
    /// Managed `workstreams` rows removed via cascade.
    pub workstreams_deleted: u64,
    /// `managed_runs` rows removed via cascade.
    pub managed_runs_deleted: u64,
    /// Pre-delete identifiers for post-commit raw-segment cleanup.
    pub workstream_ids: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete.
    pub compacted: bool,
}

/// Delete a workspace row. Refuses a workspace that still holds projects
/// unless `force` is set (the guard exists so a stray typo can't wipe a live
/// workspace). The `workspace_id` FKs are `ON DELETE CASCADE`, so a single
/// `DELETE FROM workspaces` also removes its projects / pages / sessions /
/// observations / handoffs / managed workstreams. The caller removes the
/// on-disk workspace and raw workstream directories afterwards.
///
/// # Errors
/// [`StoreError::WorkspaceNotEmpty`] when it still holds projects and `force`
/// is false; [`StoreError::NotFound`] when the workspace does not exist.
pub fn delete_workspace(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    force: bool,
    compaction: Compaction,
) -> StoreResult<DeleteWorkspaceSummary> {
    let tx = conn.transaction()?;
    let wid = workspace_id.as_bytes();
    let count = |sql: &str| -> StoreResult<u64> {
        let n: Option<i64> = tx
            .query_row(sql, rusqlite::params![&wid[..]], |row| row.get(0))
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };

    let projects_deleted = count("SELECT COUNT(*) FROM projects WHERE workspace_id = ?1")?;
    if projects_deleted > 0 && !force {
        return Err(StoreError::WorkspaceNotEmpty(projects_deleted));
    }
    let pages_deleted = count("SELECT COUNT(*) FROM pages WHERE workspace_id = ?1")?;
    let workstreams_deleted = count("SELECT COUNT(*) FROM workstreams WHERE workspace_id = ?1")?;
    let managed_runs_deleted = count(
        "SELECT COUNT(*) FROM managed_runs mr \
         JOIN workstreams w ON w.id = mr.workstream_id \
         WHERE w.workspace_id = ?1",
    )?;
    let workstream_ids: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM workstreams WHERE workspace_id = ?1")?;
        let rows = stmt
            .query_map(rusqlite::params![&wid[..]], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|raw| sessionmunch_core::WorkstreamId::from_slice(&raw).map(|id| id.to_string()))
            .collect::<Result<Vec<_>, _>>()?
    };

    let removed = tx.execute(
        "DELETE FROM workspaces WHERE id = ?1",
        rusqlite::params![&wid[..]],
    )?;
    if removed == 0 {
        return Err(StoreError::NotFound("workspace".into()));
    }

    // Whole-workspace tombstone (#607): a 16-byte all-zero project id marks the
    // entire workspace as purged so `reindex` cannot recreate it — or any of
    // its projects — from on-disk `_meta.md` that the post-commit directory
    // removal failed to delete. Same transaction as the DELETE for atomic
    // terminality.
    tx.execute(
        "INSERT OR REPLACE INTO purged_scopes (workspace_id, project_id, purged_at) \
         VALUES (?1, zeroblob(16), ?2)",
        rusqlite::params![&wid[..], Timestamp::now().as_microsecond()],
    )?;

    tx.commit()?;

    if compaction == Compaction::Reclaim {
        reclaim_freed_pages(conn)?;
    }

    Ok(DeleteWorkspaceSummary {
        projects_deleted,
        pages_deleted,
        workstreams_deleted,
        managed_runs_deleted,
        workstream_ids,
        compacted: compaction == Compaction::Reclaim,
    })
}

/// Rename a workspace: a `workspaces.name` UPDATE only — the on-disk dir is
/// keyed by UUID, so nothing moves. Mirrors [`rename_project`].
///
/// # Errors
/// [`StoreError::InvalidWorkspaceName`] on an empty / `/`-containing name;
/// [`StoreError::WorkspaceNameTaken`] on a `UNIQUE(name)` collision;
/// [`StoreError::NotFound`] when the workspace vanished (race with delete).
pub fn rename_workspace(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    new_name: &str,
) -> StoreResult<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidWorkspaceName(
            "workspace name must not be empty or all whitespace".into(),
        ));
    }
    if trimmed.contains('/') {
        return Err(StoreError::InvalidWorkspaceName(
            "workspace name must not contain '/'".into(),
        ));
    }
    let rows = conn.execute(
        "UPDATE workspaces SET name = ?1 WHERE id = ?2",
        params![trimmed, workspace_id.as_bytes()],
    );
    match rows {
        Ok(0) => Err(StoreError::NotFound(format!(
            "workspace id {workspace_id} no longer exists (race with delete)"
        ))),
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(StoreError::WorkspaceNameTaken(trimmed.to_string()))
        }
        Err(e) => Err(StoreError::Sqlite(e)),
    }
}

/// Summary returned by [`move_project_workspace`] and exposed via
/// [`crate::writer::WriterHandle::move_project_workspace`].
#[derive(Debug, Default, Clone)]
pub struct MoveSummary {
    /// `pages` rows re-stamped (all versions, not just latest).
    pub pages_moved: u64,
    /// `sessions` rows re-stamped.
    pub sessions_moved: u64,
    /// `observations` rows re-stamped.
    pub observations_moved: u64,
    /// `handoffs` rows re-stamped.
    pub handoffs_moved: u64,
    /// `audit_log` rows re-stamped.
    pub audit_log_moved: u64,
    /// `auto_improve_runs` rows re-stamped.
    pub auto_improve_runs_moved: u64,
    /// `auto_improve_proposals` rows re-stamped.
    pub auto_improve_proposals_moved: u64,
    /// `auto_improve_rejections` rows re-stamped.
    pub auto_improve_rejections_moved: u64,
    /// `auto_improve_scheduler_state` rows re-stamped.
    pub auto_improve_scheduler_state_moved: u64,
    /// `auto_improve_scheduler_claims` rows re-stamped.
    pub auto_improve_scheduler_claims_moved: u64,
    /// Durable SessionEnd consolidation jobs re-stamped.
    pub session_consolidation_jobs_moved: u64,
    /// Managed workstreams re-stamped. Native sessions, runs, and events stay
    /// attached through `workstream_id` and need no direct update.
    pub workstreams_moved: u64,
}

/// Re-stamp a project's `workspace_id` across every domain table in ONE
/// transaction, keeping the same `project_id`. This is a lossless "true move":
/// pages, sessions, observations, handoffs and supersession history all stay
/// attached to the project — unlike a copy+purge, which drops everything but
/// the latest pages.
///
/// `page_embeddings` and `links` are keyed by `page_id` (not `workspace_id`),
/// so they follow automatically with no re-stamp.
///
/// The destination workspace row MUST already exist (FK on
/// `projects.workspace_id`); the caller get-or-creates it first. A same-named
/// project already living in the destination workspace makes the `projects`
/// UPDATE violate `UNIQUE (workspace_id, name)` and the whole transaction
/// rolls back — the caller must detect that merge case and route it through
/// copy+purge instead.
pub fn move_project_workspace(
    conn: &mut Connection,
    project_id: &ProjectId,
    from_workspace: &WorkspaceId,
    to_workspace: &WorkspaceId,
) -> StoreResult<MoveSummary> {
    let tx = conn.transaction()?;

    let pid = project_id.as_bytes();
    let from = from_workspace.as_bytes();
    let to = to_workspace.as_bytes();

    // Re-stamp child tables first (they carry the denormalized workspace_id),
    // then the project row last. Order is irrelevant inside the transaction,
    // but doing projects last keeps the UNIQUE(workspace_id, name) failure —
    // the merge-collision signal — as the final, cheapest check.
    let pages_moved = tx.execute(
        "UPDATE pages SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    // page_sections/page_passages denormalize workspace_id/project_id for
    // scoped passage queries (plan: "passage tables carry and filter
    // workspace/project on every stream") — restamp them alongside pages or
    // their scope goes stale after a project move.
    tx.execute(
        "UPDATE page_sections SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )?;
    tx.execute(
        "UPDATE page_passages SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )?;
    let sessions_moved = tx.execute(
        "UPDATE sessions SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let observations_moved = tx.execute(
        "UPDATE observations SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let handoffs_moved = tx.execute(
        "UPDATE handoffs SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let audit_log_moved = tx.execute(
        "UPDATE audit_log SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_runs_moved = tx.execute(
        "UPDATE auto_improve_runs SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_proposals_moved = tx.execute(
        "UPDATE auto_improve_proposals SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_rejections_moved = tx.execute(
        "UPDATE auto_improve_rejections SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_scheduler_state_moved = tx.execute(
        "UPDATE auto_improve_scheduler_state SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_scheduler_claims_moved = tx.execute(
        "UPDATE auto_improve_scheduler_claims SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let session_consolidation_jobs_moved = tx.execute(
        "UPDATE session_consolidation_jobs SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let workstreams_moved = tx.execute(
        "UPDATE workstreams SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;

    let projects_updated = tx.execute(
        "UPDATE projects SET workspace_id = ?1 WHERE id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )?;
    if projects_updated != 1 {
        return Err(StoreError::NotFound(format!(
            "project {project_id} not found in source workspace {from_workspace}"
        )));
    }

    tx.commit()?;
    Ok(MoveSummary {
        pages_moved,
        sessions_moved,
        observations_moved,
        handoffs_moved,
        audit_log_moved,
        auto_improve_runs_moved,
        auto_improve_proposals_moved,
        auto_improve_rejections_moved,
        auto_improve_scheduler_state_moved,
        auto_improve_scheduler_claims_moved,
        session_consolidation_jobs_moved,
        workstreams_moved,
    })
}

/// How [`move_session`] treats the session's consolidated wiki page
/// (`sessions/<session_id>.md`) when the session changes scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PagesMode {
    /// Re-stamp every version of the page into the destination scope so the
    /// curated page and its supersession history follow the session. Fails
    /// with [`StoreError::PagePathTaken`] when the destination already holds
    /// a latest page at that path.
    #[default]
    Move,
    /// Leave the rows in the source scope but clear `is_latest` (and the
    /// session's `summary_page_id` when it pointed at them), so the next
    /// consolidation of the session writes a fresh page in the destination.
    Regenerate,
}

/// One scope a move drained observations out of, and how many.
///
/// A move matches dependent rows by session id alone, so it gathers rows from
/// EVERY scope they landed in — not just the one the caller named as the
/// source. That is deliberate (it is what repairs a session scattered by
/// pre-sticky mid-session routing), but it is not cleanly reversible: moving
/// back sends every row to one scope, and the original per-row attribution is
/// gone. So the caller is told which scopes it is about to drain, by name,
/// before it commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveSessionSourceScope {
    /// Workspace the rows are currently stamped with.
    pub workspace_id: WorkspaceId,
    /// Project the rows are currently stamped with.
    pub project_id: ProjectId,
    /// Workspace name, resolved for the operator-facing report.
    pub workspace_name: String,
    /// Project name, resolved for the operator-facing report.
    pub project_name: String,
    /// Observations of this session currently in that scope.
    pub observations: u64,
}

/// Summary returned by [`move_session`] and exposed via
/// [`crate::writer::WriterHandle::move_session`]. Populated identically for a
/// dry run (`commit = false`), whose transaction is rolled back after counting.
#[derive(Debug, Clone)]
pub struct MoveSessionSummary {
    /// Whether the `sessions` row changed scope. `false` when the row already
    /// sat in the target scope: the call is then a "re-home" that only
    /// re-stamps the session's rows still lying in other scopes, and every
    /// count says how many of those there were (all zero when none).
    pub session_moved: bool,
    /// `observations` rows re-stamped: every row of the session that was
    /// outside the target scope, including any that landed outside the
    /// session row's own scope. Rows already in the target are not counted.
    pub observations: u64,
    /// `handoffs` rows re-stamped (`from_session_id` = the session).
    pub handoffs: u64,
    /// `session_consolidation_jobs` rows re-stamped.
    pub consolidation_jobs: u64,
    /// `auto_improve_runs` rows re-stamped.
    pub auto_improve_runs: u64,
    /// `auto_improve_scheduler_claims` rows re-stamped.
    pub auto_improve_claims: u64,
    /// `pages` rows (all versions) re-stamped under [`PagesMode::Move`].
    pub page_versions_moved: u64,
    /// `pages` rows whose `is_latest` was cleared under
    /// [`PagesMode::Regenerate`].
    pub pages_regenerated: u64,
    /// The session page path when the source scope held at least one version
    /// of it, so the caller can move or retire the on-disk file too.
    pub page_path: Option<String>,
    /// `pages` versions of `sessions/<id>.md` sitting in the target scope
    /// once the call is done (moved there now or already there before), so
    /// a caller can tell "nothing to move" from "no page anywhere".
    pub page_versions_in_target: u64,
    /// Scope the session was read from.
    pub from_workspace: WorkspaceId,
    /// Scope the session was read from.
    pub from_project: ProjectId,
    /// Scope the session now belongs to.
    pub to_workspace: WorkspaceId,
    /// Scope the session now belongs to.
    pub to_project: ProjectId,
    /// The session's recorded working directory, left untouched by the move
    /// (historical truth; the caller may warn when it no longer matches the
    /// destination project).
    pub cwd: Option<String>,
    /// Scopes holding observations of this session that are NOT the target,
    /// with their counts, newest-largest first. More than one entry means the
    /// move will drain a project the caller did not name; an empty list means
    /// everything already sits in the target.
    pub source_scopes: Vec<MoveSessionSourceScope>,
}

/// Re-stamp one session and every row that hangs off it (`observations`,
/// `handoffs` it produced, its consolidation jobs, auto-improve runs and
/// scheduler claim) into `(target_workspace, target_project)` in ONE
/// transaction, plus its `sessions/<session_id>.md` page per `pages`.
///
/// Dependent rows are matched by session id alone, so a hybrid session whose
/// events were scattered over other scopes (mid-session routing before the
/// sticky mode) is gathered whole; only rows not already in the target are
/// touched and counted.
///
/// A target equal to the session row's own scope is a **re-home**: the row
/// stays put (`session_moved = false`) and the same sweep re-stamps every
/// dependent row still lying in any other scope; a session with nothing
/// outside the target reports zero counts (not an error). Its page rows are
/// then handled wherever they sit outside the target: under
/// [`PagesMode::Move`] they are re-stamped (refused with
/// [`StoreError::PagePathTaken`] when the target already holds a latest page
/// or more than one outside scope does), under [`PagesMode::Regenerate`]
/// their latest rows are retired. In a real move the page rows handled are
/// those of the source scope, as before.
///
/// `commit = false` performs the same work and rolls the transaction back
/// before returning, so the summary is an exact dry run. `author_id` is the
/// operator recorded on the `audit_log` row, as in [`rename_project`].
///
/// The `workspace_id`/`project_id` pairing triggers only guard INSERT, so the
/// target pair is validated against `projects` here before any UPDATE runs.
///
/// # Errors
/// - [`StoreError::NotFound`] when the session or the target project (in the
///   target workspace) does not exist.
/// - [`StoreError::PagePathTaken`] under [`PagesMode::Move`] when the target
///   scope already has a latest page at the session page path.
/// - [`StoreError::Sqlite`] on any other SQL failure.
pub fn move_session(
    conn: &mut Connection,
    session_id: SessionId,
    target_workspace: WorkspaceId,
    target_project: ProjectId,
    pages: PagesMode,
    author_id: Option<sessionmunch_core::UserId>,
    commit: bool,
) -> StoreResult<MoveSessionSummary> {
    let tx = conn.transaction()?;

    let sid = session_id.as_bytes();
    let row = tx
        .query_row(
            "SELECT workspace_id, project_id, cwd FROM sessions WHERE id = ?1",
            params![&sid[..]],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((from_ws, from_proj, cwd)) = row else {
        return Err(StoreError::NotFound(format!(
            "session {session_id} not found"
        )));
    };
    let from_workspace = WorkspaceId::from_slice(&from_ws)?;
    let from_project = ProjectId::from_slice(&from_proj)?;

    let mut summary = MoveSessionSummary {
        session_moved: false,
        observations: 0,
        handoffs: 0,
        consolidation_jobs: 0,
        auto_improve_runs: 0,
        auto_improve_claims: 0,
        page_versions_moved: 0,
        pages_regenerated: 0,
        page_path: None,
        page_versions_in_target: 0,
        from_workspace,
        from_project,
        to_workspace: target_workspace,
        to_project: target_project,
        cwd,
        source_scopes: Vec::new(),
    };
    let rehome = from_workspace == target_workspace && from_project == target_project;

    let to_ws = target_workspace.as_bytes();
    let to_proj = target_project.as_bytes();
    let target_exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1 AND workspace_id = ?2",
            params![&to_proj[..], &to_ws[..]],
            |row| row.get(0),
        )
        .optional()?;
    if target_exists.is_none() {
        return Err(StoreError::NotFound(format!(
            "project {target_project} not found in workspace {target_workspace}"
        )));
    }

    if !rehome {
        let sessions_updated = tx.execute(
            "UPDATE sessions SET workspace_id = ?1, project_id = ?2 WHERE id = ?3",
            params![&to_ws[..], &to_proj[..], &sid[..]],
        )?;
        summary.session_moved = sessions_updated == 1;
    }
    // Which scopes this move is about to drain, resolved to names BEFORE the
    // re-stamp moves the rows. A move gathers by session id alone, so it can
    // empty a project the caller never named; the operator sees that in the
    // dry run rather than discovering it afterwards, when the original
    // attribution is no longer recoverable.
    {
        let mut stmt = tx.prepare(
            "SELECT o.workspace_id, o.project_id, w.name, p.name, COUNT(*) \
             FROM observations o \
             JOIN workspaces w ON w.id = o.workspace_id \
             JOIN projects p ON p.id = o.project_id \
             WHERE o.session_id = ?1 AND NOT (o.workspace_id = ?2 AND o.project_id = ?3) \
             GROUP BY o.workspace_id, o.project_id \
             ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map(params![&sid[..], &to_ws[..], &to_proj[..]], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (ws_bytes, proj_bytes, workspace_name, project_name, count) = row?;
            summary.source_scopes.push(MoveSessionSourceScope {
                workspace_id: WorkspaceId::from_slice(&ws_bytes)?,
                project_id: ProjectId::from_slice(&proj_bytes)?,
                workspace_name,
                project_name,
                observations: u64::try_from(count).unwrap_or(0),
            });
        }
    }

    // Every dependent row of the session not already in the target, whatever
    // scope it lies in. Fires `observations_fts_au` per observation row
    // (delete + reinsert of the same text); the FTS index stays consistent
    // inside this transaction.
    let restamp = |table: &str, session_col: &str| -> StoreResult<u64> {
        Ok(tx.execute(
            &format!(
                "UPDATE {table} SET workspace_id = ?1, project_id = ?2 \
                 WHERE {session_col} = ?3 \
                   AND NOT (workspace_id = ?1 AND project_id = ?2)"
            ),
            params![&to_ws[..], &to_proj[..], &sid[..]],
        )? as u64)
    };
    summary.observations = restamp("observations", "session_id")?;
    summary.handoffs = restamp("handoffs", "from_session_id")?;
    summary.consolidation_jobs = restamp("session_consolidation_jobs", "session_id")?;
    summary.auto_improve_runs = restamp("auto_improve_runs", "session_id")?;
    summary.auto_improve_claims = restamp("auto_improve_scheduler_claims", "session_id")?;

    // Page rows to handle: the source scope's in a real move; in a re-home,
    // any scope other than the target (the source IS the target).
    let page_path = format!("sessions/{session_id}.md");
    let page_scope_sql = if rehome {
        "NOT (workspace_id = ?1 AND project_id = ?2)"
    } else {
        "workspace_id = ?1 AND project_id = ?2"
    };
    let page_scope_params = if rehome {
        (&to_ws[..], &to_proj[..])
    } else {
        (&from_ws[..], &from_proj[..])
    };
    let (candidate_versions, candidate_latest): (i64, i64) = tx.query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(is_latest), 0) FROM pages \
             WHERE {page_scope_sql} AND path = ?3"
        ),
        params![page_scope_params.0, page_scope_params.1, page_path.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if candidate_versions > 0 {
        match pages {
            PagesMode::Move => {
                let taken: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM pages \
                         WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                           AND is_latest = 1",
                        params![&to_ws[..], &to_proj[..], page_path.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                // Two latest rows cannot share the path in the target
                // (`idx_pages_latest_path`): a re-home that finds latest
                // rows in more than one outside scope is a collision too.
                if taken.is_some() || candidate_latest > 1 {
                    return Err(StoreError::PagePathTaken { path: page_path });
                }
                summary.page_versions_moved = tx.execute(
                    &format!(
                        "UPDATE pages SET workspace_id = ?4, project_id = ?5 \
                         WHERE {page_scope_sql} AND path = ?3"
                    ),
                    params![
                        page_scope_params.0,
                        page_scope_params.1,
                        page_path.as_str(),
                        &to_ws[..],
                        &to_proj[..],
                    ],
                )? as u64;
            }
            PagesMode::Regenerate => {
                // Close the retiring page's entity windows first (the
                // predicate needs is_latest = 1, flipped just below); the
                // page-grain window closes with the flip, same instant
                // (issue #656).
                let retire_at = Timestamp::now().as_microsecond();
                tx.execute(
                    &format!(
                        "UPDATE entity_page_links SET superseded_at = ?4 \
                         WHERE superseded_at IS NULL AND page_id IN ( \
                             SELECT id FROM pages \
                             WHERE {page_scope_sql} AND path = ?3 AND is_latest = 1)"
                    ),
                    params![
                        page_scope_params.0,
                        page_scope_params.1,
                        page_path.as_str(),
                        retire_at,
                    ],
                )?;
                summary.pages_regenerated = tx.execute(
                    &format!(
                        "UPDATE pages SET is_latest = 0, valid_to = ?4 \
                         WHERE {page_scope_sql} AND path = ?3 AND is_latest = 1"
                    ),
                    params![
                        page_scope_params.0,
                        page_scope_params.1,
                        page_path.as_str(),
                        retire_at
                    ],
                )? as u64;
                // The session's summary pointer targeted the page just
                // retired; the next consolidation sets it again.
                tx.execute(
                    &format!(
                        "UPDATE sessions SET summary_page_id = NULL \
                         WHERE id = ?4 AND summary_page_id IN ( \
                             SELECT id FROM pages WHERE {page_scope_sql} AND path = ?3)"
                    ),
                    params![
                        page_scope_params.0,
                        page_scope_params.1,
                        page_path.as_str(),
                        &sid[..],
                    ],
                )?;
            }
        }
        summary.page_path = Some(page_path.clone());
    }
    summary.page_versions_in_target = tx.query_row(
        "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
        params![&to_ws[..], &to_proj[..], page_path.as_str()],
        |row| row.get::<_, i64>(0),
    )? as u64;

    audit(
        &tx,
        "move_session",
        Some(to_ws),
        Some(to_proj),
        None,
        author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
        Timestamp::now().as_microsecond(),
    )?;

    if commit {
        tx.commit()?;
    } else {
        tx.rollback()?;
    }
    Ok(summary)
}

/// Remove embedding rows in a workspace/project scope whose `(provider, model, dim)`
/// does not match the configured triple, plus rows tied to superseded pages.
pub fn delete_stale_page_embeddings(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let (n, orphans) = if let Some(project_id) = project_id {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1\
             ) \
               AND NOT (provider = ?3 AND model = ?4 AND dim = CAST(?5 AS INTEGER))",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                provider,
                model,
                dim
            ],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
        (n, orphans)
    } else {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 1\
             ) \
               AND NOT (provider = ?2 AND model = ?3 AND dim = CAST(?4 AS INTEGER))",
            params![workspace_id.as_bytes(), provider, model, dim],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes()],
        )?;
        (n, orphans)
    };
    tx.commit()?;
    Ok(u64::try_from(n.saturating_add(orphans)).unwrap_or(0))
}

#[cfg(test)]
pub(crate) mod tests;
