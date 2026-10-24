//! Decay deletes, page deletion and observation pruning.

use super::*;

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

pub(super) fn delete_page_inner(
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
