//! Inputs to decay, pruning, feedback and other maintenance passes.

use super::*;

impl ReaderPool {
    /// Return decay-evaluation candidates for the M8 forget sweep.
    ///
    /// Walks `pages` rows with `is_latest = 1` and returns the columns
    /// the forget sweep needs to compute the retention formula. The
    /// sweep itself filters by tier (only `episodic`) + pinned flag,
    /// so this method does not pre-filter -- it just hands the data
    /// over.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn decay_candidates(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<DecayCandidate>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, path, tier, pinned, updated_at, access_count, last_accessed_at, \
                        frontmatter_json, expires_at, salience \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                row_to_decay_candidate,
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r??);
            }
            Ok(out)
        })
        .await
    }

    /// Return decay tombstones old enough for permanent cleanup.
    ///
    /// `superseded_at` is written only by the forget-sweep eviction path, so
    /// it is the tombstone discriminator regardless of whether a rewritten
    /// page head also has a `supersedes` ancestor. The caller still routes
    /// each result through the wiki layer before deleting its version chain.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn decay_tombstones_before(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cutoff_us: i64,
    ) -> StoreResult<Vec<DecayTombstone>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, path FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                   AND is_latest = 0 \
                   AND superseded_at IS NOT NULL \
                   AND superseded_at <= ?3 \
                 ORDER BY superseded_at, id",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes(), cutoff_us],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (id, path) = row?;
                out.push(DecayTombstone {
                    id: PageId::from_slice(&id)?,
                    path: PagePath::new(path)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Count the observations the prune pass would delete for this scope.
    ///
    /// Same predicate as
    /// [`prune_consolidated_observations`](crate::ops::prune_consolidated_observations),
    /// read-only, so a dry run can report a number without any chance of a
    /// write. Kept beside the delete rather than derived from it because the
    /// dry run must never open a write transaction at all.
    ///
    /// # Errors
    /// Propagates SQL errors.
    pub async fn prunable_observation_count(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cutoff_us: i64,
    ) -> StoreResult<usize> {
        self.with_conn(move |conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM observations o \
                 WHERE o.workspace_id = ?1 \
                   AND o.project_id = ?2 \
                   AND o.created_at < ?3 \
                   AND EXISTS ( \
                       SELECT 1 FROM sessions s \
                       JOIN pages p ON p.id = s.summary_page_id \
                       WHERE s.id = o.session_id \
                         AND p.superseded_at IS NULL \
                   )",
                params![workspace_id.as_bytes(), project_id.as_bytes(), cutoff_us],
                |row| row.get(0),
            )?;
            Ok(usize::try_from(n).unwrap_or(0))
        })
        .await
    }

    /// Return the number of DISTINCT operators that reinforced each
    /// `is_latest = 1` page of a project, for the sweep's breadth term.
    ///
    /// Scoped and grouped exactly like [`decay_candidates`](Self::decay_candidates)
    /// so the sweep pays for one query per run, never one per candidate. Pages
    /// with no per-operator rows (everything read anonymously, and everything
    /// that predates `page_access`) are simply absent from the map, which the
    /// caller reads as breadth 0 — scored identically to breadth 1 by
    /// [`retention_score_with_breadth`](crate::retention_score_with_breadth).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn access_breadth_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<std::collections::HashMap<PageId, u32>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT pa.page_id, COUNT(*) \
                 FROM page_access pa \
                 JOIN pages p ON p.id = pa.page_id \
                 WHERE p.workspace_id = ?1 AND p.project_id = ?2 AND p.is_latest = 1 \
                 GROUP BY pa.page_id",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let id: Vec<u8> = row.get(0)?;
                    let actors: i64 = row.get(1)?;
                    Ok((id, u32::try_from(actors).unwrap_or(u32::MAX)))
                },
            )?;
            let mut out = std::collections::HashMap::new();
            for r in rows {
                let (id, actors) = r?;
                out.insert(PageId::from_slice(&id)?, actors);
            }
            Ok(out)
        })
        .await
    }

    /// Number of `page_evidence` rows for each of `page_ids` (P2,
    /// docs/design-hindsight-borrowings.md §3): what produced or
    /// reaffirmed that page version. One batch query for the whole
    /// result page — never one per hit — so
    /// [`Self::hybrid_search_explained`] can attach
    /// `SearchExplain::evidence_count` without touching the hot,
    /// non-explained default path. A page id with zero evidence rows is
    /// simply absent from the map; callers read that as count 0
    /// ("unknown", not "unsupported").
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_evidence_counts(
        &self,
        page_ids: &[PageId],
    ) -> StoreResult<std::collections::HashMap<PageId, u32>> {
        if page_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let page_id_blobs: Vec<Value> = page_ids
            .iter()
            .map(|id| Value::Blob(id.as_bytes().to_vec()))
            .collect();
        self.with_conn(move |conn| {
            let placeholders = std::iter::repeat_n("?", page_id_blobs.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT page_id, COUNT(*) FROM page_evidence \
                 WHERE page_id IN ({placeholders}) GROUP BY page_id"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(page_id_blobs.iter()), |row| {
                let id: Vec<u8> = row.get(0)?;
                let n: i64 = row.get(1)?;
                Ok((id, u32::try_from(n).unwrap_or(u32::MAX)))
            })?;
            let mut out = std::collections::HashMap::new();
            for r in rows {
                let (id, n) = r?;
                out.insert(PageId::from_slice(&id)?, n);
            }
            Ok(out)
        })
        .await
    }

    /// Return `stale` / `wrong` feedback still attached to a *current*
    /// page version in one project, grouped by (page, kind) — the input
    /// for the lint pass's `feedback_flagged` findings. Rewriting a
    /// flagged page supersedes the version the feedback points at, so
    /// the finding drops out here without any explicit dismissal.
    /// Most-recent signal first.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn open_feedback_findings(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<FeedbackFinding>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT pg.path, f.kind, COUNT(*) AS signal_count, MAX(f.created_at) AS latest, \
                        (SELECT reason FROM page_feedback r \
                          WHERE r.page_id = f.page_id AND r.kind = f.kind \
                            AND r.reason IS NOT NULL \
                          ORDER BY r.created_at DESC, r.id DESC LIMIT 1) AS reason \
                 FROM page_feedback f \
                 JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                 WHERE f.workspace_id = ?1 AND f.project_id = ?2 \
                   AND f.kind IN ('stale', 'wrong') \
                 GROUP BY f.page_id, pg.path, f.kind \
                 ORDER BY latest DESC, pg.path ASC, f.kind ASC",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let path: String = row.get(0)?;
                    let kind: String = row.get(1)?;
                    let signal_count: i64 = row.get(2)?;
                    let latest_us: i64 = row.get(3)?;
                    let reason: Option<String> = row.get(4)?;
                    Ok((path, kind, signal_count, latest_us, reason))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (path, kind, signal_count, latest_us, reason) = row?;
                out.push(FeedbackFinding {
                    path,
                    kind,
                    signal_count: u32::try_from(signal_count.max(0)).unwrap_or(u32::MAX),
                    latest_at: jiff::Timestamp::from_microsecond(latest_us)
                        .map(|ts| ts.to_string())
                        .unwrap_or_default(),
                    reason,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Cross-session pass cadence probe: completed sessions newer than
    /// the pass's last run for this scope (docs/experience.md).
    ///
    /// # Errors
    /// Propagates SQL errors from the read pool.
    pub async fn experience_pass_due(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<(u64, i64)> {
        self.with_conn(move |conn| {
            crate::auto_improve::experience_pass_due(conn, workspace_id, project_id)
        })
        .await
    }

    /// Return the last successful global maintenance completion for `job`.
    pub async fn maintenance_job_last_success(
        &self,
        job: MaintenanceJob,
    ) -> StoreResult<Option<i64>> {
        self.with_conn(move |conn| crate::maintenance::last_success(conn, job))
            .await
    }
}
