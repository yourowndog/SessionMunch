//! Session, project and workspace purges.

use super::*;

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
