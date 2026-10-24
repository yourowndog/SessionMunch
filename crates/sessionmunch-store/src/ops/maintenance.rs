//! Session reorg, project rename, wiki migrations and database compaction.

use super::*;

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
pub(super) const ALL_FTS_INDEXES: &[&str] = &[
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
