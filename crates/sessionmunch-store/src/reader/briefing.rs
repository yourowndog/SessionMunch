//! Session-start briefings and memory-health reports.

use super::*;

impl ReaderPool {
    /// Snapshot the database to `dest_path` using SQLite's online backup
    /// API. The source DB stays writable for the duration of the copy.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn snapshot_to(&self, dest_path: PathBuf) -> StoreResult<()> {
        self.with_conn(move |conn| {
            conn.backup(rusqlite::DatabaseName::Main, &dest_path, None)
                .map_err(StoreError::from)
        })
        .await
    }

    /// Assemble a [`BriefingSnapshot`] — pure SQL aggregation across
    /// the `pages` / `sessions` / `observations` / `handoffs` tables.
    /// No LLM, no schema reads outside what's already there.
    ///
    /// `recent_pages_limit` caps the `recent_pages` array; pass a
    /// small number (5-20) — this is meant to be skimmed, not paged
    /// through.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_lines)]
    pub async fn briefing(
        &self,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
    ) -> StoreResult<BriefingSnapshot> {
        self.briefing_with_slot_visibility(
            recent_pages_limit,
            owner_filter,
            &sessionmunch_core::SlotVisibility::All,
        )
        .await
    }

    /// Assemble a global briefing while filtering operator-owned slot pages.
    ///
    /// This additive variant preserves [`Self::briefing`] for existing callers.
    /// The filter only controls agent-context injection; exact page reads remain
    /// project-wide.
    pub async fn briefing_with_slot_visibility(
        &self,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
        slot_visibility: &sessionmunch_core::SlotVisibility,
    ) -> StoreResult<BriefingSnapshot> {
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        let slot_visibility = slot_visibility.clone();
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(&slot_visibility, 3);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            let now_us = jiff::Timestamp::now().as_microsecond();
            let day_us: i64 = 86_400 * 1_000_000;
            let cutoff_7d = now_us - 7 * day_us;
            let cutoff_30d = now_us - 30 * day_us;

            let counts = StatusCounts {
                pages_latest: count(conn, "SELECT COUNT(*) FROM pages WHERE is_latest = 1")?,
                pages_all: count(conn, "SELECT COUNT(*) FROM pages")?,
                sessions: count(conn, "SELECT COUNT(*) FROM sessions")?,
                observations: count(conn, "SELECT COUNT(*) FROM observations")?,
            };

            let activity_7d = window_activity(conn, 7, cutoff_7d)?;
            let activity_30d = window_activity(conn, 30, cutoff_30d)?;

            let last_observation_at: Option<i64> = conn
                .query_row("SELECT MAX(created_at) FROM observations", [], |row| {
                    row.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten();
            let last_observation_at = last_observation_at
                .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                .map(|ts| ts.to_string());

            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 1);
            let mut owner_binds: Vec<&dyn rusqlite::ToSql> = Vec::new();
            if let Some(owner) = owner_param.as_ref() {
                owner_binds.push(owner);
            }
            let pending_handoff_count: u64 = count_bound(
                conn,
                &format!("SELECT COUNT(*) FROM handoffs WHERE state = 'open'{owner_clause}"),
                &owner_binds,
            )?;

            // Rules: any `is_latest = 1` page under `_rules/`.
            // Routed there automatically by the consolidator when
            // `kind = "rule"` — see consolidator.rs::slugify_for_rule.
            let mut rules_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE is_latest = 1 AND path GLOB '_rules/*'{not_expired} \
                  ORDER BY updated_at DESC",
                not_expired = not_expired("pages", "?1"),
            ))?;
            let rules: Vec<BriefingPage> = rules_stmt
                .query_map(params![now_us], briefing_page_from_row)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut slots_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE is_latest = 1 AND path GLOB '_slots/*'{not_expired} \
                  ORDER BY path ASC",
                not_expired = not_expired("pages", "?1"),
            ))?;
            let slots: Vec<BriefingPage> = slots_stmt
                .query_map(params![now_us], briefing_page_from_row)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE is_latest = 1 AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?1",
                not_expired = not_expired("pages", "?2"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt
                    .query_map(params![recent_limit, now_us, glob], briefing_page_from_row)?,
                None => {
                    recent_stmt.query_map(params![recent_limit, now_us], briefing_page_from_row)?
                }
            };
            let recent_pages: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut snapshot = BriefingSnapshot {
                counts,
                activity_7d,
                activity_30d,
                last_observation_at,
                pending_handoff_count,
                rules,
                slots,
                recent_pages,
                cross_project_dependents: 0,
                cross_project_dependencies: 0,
                settled: Vec::new(),
            };
            filter_briefing_slots(&mut snapshot, &slot_visibility);
            Ok(snapshot)
        })
        .await
    }

    /// Assemble a project-scoped [`BriefingSnapshot`].
    ///
    /// `settled_first` opts into populating [`BriefingSnapshot::settled`]
    /// with the project's highest-standing `rule`/`decision` pages (P4,
    /// docs/design-hindsight-borrowings.md §5); `false` leaves the
    /// snapshot byte-for-byte as before.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_lines)]
    pub async fn briefing_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
        settled_first: bool,
    ) -> StoreResult<BriefingSnapshot> {
        self.briefing_for_project_with_slot_visibility(
            workspace_id,
            project_id,
            recent_pages_limit,
            owner_filter,
            &sessionmunch_core::SlotVisibility::All,
            settled_first,
        )
        .await
    }

    /// Assemble a project briefing while filtering operator-owned slot
    /// pages. See [`ReaderPool::briefing_for_project`] for `settled_first`.
    pub async fn briefing_for_project_with_slot_visibility(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
        slot_visibility: &sessionmunch_core::SlotVisibility,
        settled_first: bool,
    ) -> StoreResult<BriefingSnapshot> {
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        let slot_visibility = slot_visibility.clone();
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(&slot_visibility, 5);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            let now_us = jiff::Timestamp::now().as_microsecond();
            let day_us: i64 = 86_400 * 1_000_000;
            let cutoff_7d = now_us - 7 * day_us;
            let cutoff_30d = now_us - 30 * day_us;

            let counts = StatusCounts {
                pages_latest: count_project(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
                    workspace_id,
                    project_id,
                )?,
                pages_all: count_project(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2",
                    workspace_id,
                    project_id,
                )?,
                sessions: count_project(
                    conn,
                    "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND project_id = ?2",
                    workspace_id,
                    project_id,
                )?,
                observations: count_project(
                    conn,
                    "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND project_id = ?2",
                    workspace_id,
                    project_id,
                )?,
            };

            let activity_7d = window_activity_project(conn, 7, cutoff_7d, workspace_id, project_id)?;
            let activity_30d = window_activity_project(conn, 30, cutoff_30d, workspace_id, project_id)?;

            let last_observation_at: Option<i64> = conn
                .query_row(
                    "SELECT MAX(created_at) FROM observations WHERE workspace_id = ?1 AND project_id = ?2",
                    params![workspace_id.as_bytes(), project_id.as_bytes()],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten();
            let last_observation_at = last_observation_at
                .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                .map(|ts| ts.to_string());

            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 3);
            let mut owner_binds: Vec<&dyn rusqlite::ToSql> =
                vec![workspace_id.as_bytes(), project_id.as_bytes()];
            if let Some(owner) = owner_param.as_ref() {
                owner_binds.push(owner);
            }
            let pending_handoff_count = count_bound(
                conn,
                &format!(
                    "SELECT COUNT(*) FROM handoffs \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND state = 'open'{owner_clause}"
                ),
                &owner_binds,
            )?;

            let mut rules_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 AND path GLOB '_rules/*'{not_expired} \
                  ORDER BY updated_at DESC",
                not_expired = not_expired("pages", "?3"),
            ))?;
            let rules: Vec<BriefingPage> = rules_stmt
                .query_map(params![workspace_id.as_bytes(), project_id.as_bytes(), now_us], briefing_page_from_row)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut slots_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 AND path GLOB '_slots/*'{not_expired} \
                  ORDER BY path ASC",
                not_expired = not_expired("pages", "?3"),
            ))?;
            let slots: Vec<BriefingPage> = slots_stmt
                .query_map(params![workspace_id.as_bytes(), project_id.as_bytes(), now_us], briefing_page_from_row)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 \
                   AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?3",
                not_expired = not_expired("pages", "?4"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us,
                        glob
                    ],
                    briefing_page_from_row,
                )?,
                None => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us
                    ],
                    briefing_page_from_row,
                )?,
            };
            let recent_pages: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let (cross_project_dependents, cross_project_dependencies) =
                cross_project_degree(conn, workspace_id, project_id)?;

            let settled: Vec<SettledPage> = if settled_first {
                let settled_kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
                let mut settled_stmt = conn.prepare_cached(&format!(
                    "SELECT pages.path, pages.title, {settled_kind_expr} AS kind, \
                            COUNT(page_evidence.source_id) AS evidence_count \
                     FROM pages \
                     LEFT JOIN page_evidence ON page_evidence.page_id = pages.id \
                     WHERE pages.workspace_id = ?1 AND pages.project_id = ?2 \
                       AND pages.is_latest = 1 \
                       AND ({settled_kind_expr}) IN ('rule', 'decision'){not_expired} \
                     GROUP BY pages.id \
                     ORDER BY evidence_count DESC, pages.updated_at DESC \
                     LIMIT 8",
                    not_expired = not_expired("pages", "?3"),
                ))?;
                settled_stmt
                    .query_map(
                        params![workspace_id.as_bytes(), project_id.as_bytes(), now_us],
                        settled_page_from_row,
                    )?
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                Vec::new()
            };

            let mut snapshot = BriefingSnapshot {
                counts,
                activity_7d,
                activity_30d,
                last_observation_at,
                pending_handoff_count,
                rules,
                slots,
                recent_pages,
                cross_project_dependents,
                cross_project_dependencies,
                settled,
            };
            filter_briefing_slots(&mut snapshot, &slot_visibility);
            Ok(snapshot)
        })
        .await
    }

    /// Fetch the pages that make up a session-start project brief: the
    /// "core" pages WITH bodies (pinned, plus everything under `_rules/`
    /// and `_slots/` — the operator-curated, highest-signal memory), and
    /// the most-recently-updated page titles WITHOUT bodies (pointers the
    /// agent can follow up on via `memory_query`).
    ///
    /// Core pages are ordered pinned-first then by path, so a char-budget
    /// cut in the renderer drops the least-curated content last. Both
    /// limits are clamped defensively; the renderer applies the actual
    /// byte budget.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_brief_pages(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        core_pages_limit: usize,
        recent_pages_limit: usize,
    ) -> StoreResult<(Vec<BriefPageBody>, Vec<BriefingPage>)> {
        self.session_brief_pages_with_slot_visibility(
            workspace_id,
            project_id,
            core_pages_limit,
            recent_pages_limit,
            sessionmunch_core::SlotVisibility::All,
        )
        .await
    }

    /// Fetch session-start pages while excluding other operators' slot pages.
    pub async fn session_brief_pages_with_slot_visibility(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        core_pages_limit: usize,
        recent_pages_limit: usize,
        slot_visibility: sessionmunch_core::SlotVisibility,
    ) -> StoreResult<(Vec<BriefPageBody>, Vec<BriefingPage>)> {
        let core_limit = core_pages_limit.clamp(1, 100) as i64;
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        let (slot_sql, slot_glob) = slot_visibility_sql(&slot_visibility, 5);
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(&slot_visibility, 5);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            let core_sql = format!(
                "SELECT path, title, body, pinned, updated_at \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1{not_expired} \
                   AND (path GLOB '_rules/*' \
                        OR (pinned = 1 AND path NOT GLOB '_slots/*') \
                        OR ({slot_sql})) \
                 ORDER BY pinned DESC, path ASC \
                 LIMIT ?3",
                not_expired = not_expired("pages", "?4"),
            );
            let mut core_stmt = conn.prepare_cached(&core_sql)?;
            let core_row = |row: &rusqlite::Row<'_>| {
                let updated_us: i64 = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                    updated_us,
                ))
            };
            let core_rows = match slot_glob.as_deref() {
                Some(glob) => core_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        core_limit,
                        now_us(),
                        glob
                    ],
                    core_row,
                )?,
                None => core_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        core_limit,
                        now_us()
                    ],
                    core_row,
                )?,
            };
            let core: Vec<BriefPageBody> = core_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|(path, title, body, pinned, updated_us)| {
                    jiff::Timestamp::from_microsecond(updated_us)
                        .map(|ts| BriefPageBody {
                            path,
                            title,
                            body,
                            pinned,
                            updated_at: ts.to_string(),
                        })
                        .map_err(|e| {
                            StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(
                                format!("bad updated_at: {e}"),
                            ))
                        })
                })
                .collect::<StoreResult<Vec<_>>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 \
                   AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?3",
                not_expired = not_expired("pages", "?4"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us(),
                        glob
                    ],
                    briefing_page_from_row,
                )?,
                None => recent_stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        recent_limit,
                        now_us()
                    ],
                    briefing_page_from_row,
                )?,
            };
            let recent: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            Ok((core, recent))
        })
        .await
    }

    /// Build a [`BriefingSnapshot`] aggregated across all projects in a
    /// workspace.
    ///
    /// Mirrors [`ReaderPool::briefing_for_project`] but scopes every query
    /// to the workspace only (no `project_id` filter).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn briefing_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
    ) -> StoreResult<BriefingSnapshot> {
        self.briefing_for_workspace_with_slot_visibility(
            workspace_id,
            recent_pages_limit,
            owner_filter,
            &sessionmunch_core::SlotVisibility::All,
        )
        .await
    }

    /// Assemble a workspace briefing while filtering operator-owned slots.
    pub async fn briefing_for_workspace_with_slot_visibility(
        &self,
        workspace_id: WorkspaceId,
        recent_pages_limit: usize,
        owner_filter: OwnerFilter,
        slot_visibility: &sessionmunch_core::SlotVisibility,
    ) -> StoreResult<BriefingSnapshot> {
        let recent_limit = recent_pages_limit.clamp(1, 100) as i64;
        let slot_visibility = slot_visibility.clone();
        let (recent_slot_sql, recent_glob) = slot_exclusion_sql(&slot_visibility, 4);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("path", "frontmatter_json");
            let now_us = jiff::Timestamp::now().as_microsecond();
            let day_us: i64 = 86_400 * 1_000_000;
            let cutoff_7d = now_us - 7 * day_us;
            let cutoff_30d = now_us - 30 * day_us;

            let counts = StatusCounts {
                pages_latest: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND is_latest = 1",
                    workspace_id,
                )?,
                pages_all: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1",
                    workspace_id,
                )?,
                sessions: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
                    workspace_id,
                )?,
                observations: count_workspace(
                    conn,
                    "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1",
                    workspace_id,
                )?,
            };

            let activity_7d = window_activity_workspace(conn, 7, cutoff_7d, workspace_id)?;
            let activity_30d = window_activity_workspace(conn, 30, cutoff_30d, workspace_id)?;

            let last_observation_at: Option<i64> = conn
                .query_row(
                    "SELECT MAX(created_at) FROM observations WHERE workspace_id = ?1",
                    params![workspace_id.as_bytes()],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten();
            let last_observation_at = last_observation_at
                .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                .map(|ts| ts.to_string());

            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 2);
            let mut owner_binds: Vec<&dyn rusqlite::ToSql> = vec![workspace_id.as_bytes()];
            if let Some(owner) = owner_param.as_ref() {
                owner_binds.push(owner);
            }
            let pending_handoff_count = count_bound(
                conn,
                &format!(
                    "SELECT COUNT(*) FROM handoffs \
                     WHERE workspace_id = ?1 AND state = 'open'{owner_clause}"
                ),
                &owner_binds,
            )?;

            let mut rules_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND is_latest = 1 AND path GLOB '_rules/*'{not_expired} \
                  ORDER BY updated_at DESC",
                not_expired = not_expired("pages", "?2"),
            ))?;
            let rules: Vec<BriefingPage> = rules_stmt
                .query_map(
                    params![workspace_id.as_bytes(), now_us],
                    briefing_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut slots_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                  WHERE workspace_id = ?1 AND is_latest = 1 AND path GLOB '_slots/*'{not_expired} \
                  ORDER BY path ASC",
                not_expired = not_expired("pages", "?2"),
            ))?;
            let slots: Vec<BriefingPage> = slots_stmt
                .query_map(
                    params![workspace_id.as_bytes(), now_us],
                    briefing_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut recent_stmt = conn.prepare_cached(&format!(
                "SELECT path, title, {kind_expr} AS kind, \
                        updated_at \
                 FROM pages \
                 WHERE workspace_id = ?1 AND is_latest = 1 AND ({recent_slot_sql}){not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?2",
                not_expired = not_expired("pages", "?3"),
            ))?;
            let recent_rows = match recent_glob.as_deref() {
                Some(glob) => recent_stmt.query_map(
                    params![workspace_id.as_bytes(), recent_limit, now_us, glob],
                    briefing_page_from_row,
                )?,
                None => recent_stmt.query_map(
                    params![workspace_id.as_bytes(), recent_limit, now_us],
                    briefing_page_from_row,
                )?,
            };
            let recent_pages: Vec<BriefingPage> = recent_rows
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;

            let mut snapshot = BriefingSnapshot {
                counts,
                activity_7d,
                activity_30d,
                last_observation_at,
                pending_handoff_count,
                rules,
                slots,
                recent_pages,
                cross_project_dependents: 0,
                cross_project_dependencies: 0,
                settled: Vec::new(),
            };
            filter_briefing_slots(&mut snapshot, &slot_visibility);
            Ok(snapshot)
        })
        .await
    }

    /// Compute basic memory-health counters for a workspace.
    ///
    /// Returns `(stale, duplicates, orphans)`:
    /// - `stale`: latest episodic pages not updated within `STALE_DAYS` (30).
    /// - `duplicates`: extra latest pages that share a title.
    /// - `orphans`: latest pages with no inbound or outbound links.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn memory_health_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<(u64, u64, u64)> {
        self.memory_health_scoped(workspace_id, None).await
    }

    /// Per-project variant of [`ReaderPool::memory_health_for_workspace`]:
    /// the same stale / duplicate / orphan counters, confined to one project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn memory_health_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<(u64, u64, u64)> {
        self.memory_health_scoped(workspace_id, Some(project_id))
            .await
    }

    pub(super) async fn memory_health_scoped(
        &self,
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
    ) -> StoreResult<(u64, u64, u64)> {
        self.with_conn(move |conn| {
            let now_us = jiff::Timestamp::now().as_microsecond();
            let cutoff_30d = now_us - 30 * 86_400 * 1_000_000;
            let proj = project_id.map(|p| Value::Blob(p.as_bytes().to_vec()));
            let ws = || Value::Blob(workspace_id.as_bytes().to_vec());
            // Optional project filter, anonymous-`?` style. The orphan query
            // aliases pages as `p`, so it needs its own qualified clause.
            let clause = if proj.is_some() {
                " AND project_id = ?"
            } else {
                ""
            };
            let clause_p = if proj.is_some() {
                " AND p.project_id = ?"
            } else {
                ""
            };

            let stale: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM pages \
                         WHERE workspace_id = ? AND is_latest = 1 \
                           AND tier = 'episodic' AND updated_at < ?{clause}"
                    ),
                    params_from_iter(
                        [ws(), Value::Integer(cutoff_30d)]
                            .into_iter()
                            .chain(proj.clone()),
                    ),
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);

            let duplicates: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COALESCE(SUM(c - 1), 0) FROM ( \
                            SELECT COUNT(*) c FROM pages \
                            WHERE workspace_id = ? AND is_latest = 1{clause} \
                            GROUP BY title HAVING c > 1 \
                         )"
                    ),
                    params_from_iter(std::iter::once(ws()).chain(proj.clone())),
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);

            let orphans: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM pages p \
                         WHERE p.workspace_id = ? AND p.is_latest = 1{clause_p} \
                           AND NOT EXISTS (SELECT 1 FROM links l WHERE l.from_page_id = p.id) \
                           AND NOT EXISTS (SELECT 1 FROM links l WHERE l.to_page_id = p.id)"
                    ),
                    params_from_iter(std::iter::once(ws()).chain(proj.clone())),
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);

            Ok((
                u64::try_from(stale).unwrap_or(0),
                u64::try_from(duplicates).unwrap_or(0),
                u64::try_from(orphans).unwrap_or(0),
            ))
        })
        .await
    }

    /// Drill-down lists behind [`ReaderPool::memory_health_for_workspace`]'s
    /// counters: the actual stale / duplicate / orphan pages, each capped at
    /// `limit`. Definitions mirror the counters exactly so the lists explain
    /// the headline numbers.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn health_detail_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        limit: usize,
    ) -> StoreResult<HealthDetail> {
        self.health_detail_scoped(workspace_id, None, limit).await
    }

    /// Per-project variant of [`ReaderPool::health_detail_for_workspace`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn health_detail_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        limit: usize,
    ) -> StoreResult<HealthDetail> {
        self.health_detail_scoped(workspace_id, Some(project_id), limit)
            .await
    }

    pub(super) async fn health_detail_scoped(
        &self,
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
        limit: usize,
    ) -> StoreResult<HealthDetail> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn(move |conn| {
            let now_us = jiff::Timestamp::now().as_microsecond();
            let cutoff_30d = now_us - 30 * 86_400 * 1_000_000;
            let proj = project_id.map(|p| Value::Blob(p.as_bytes().to_vec()));
            let ws = || Value::Blob(workspace_id.as_bytes().to_vec());
            let clause = if proj.is_some() {
                " AND pg.project_id = ?"
            } else {
                ""
            };
            let inner_clause = if proj.is_some() {
                " AND project_id = ?"
            } else {
                ""
            };

            // Shared SELECT prefix: identity + path-inferred `kind`.
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let select = format!(
                "SELECT w.name, p.name, pg.path, pg.title, {kind_expr} \
                   FROM pages pg \
                   JOIN projects p ON p.id = pg.project_id \
                   JOIN workspaces w ON w.id = pg.workspace_id "
            );

            // Params follow placeholder order: ws, cutoff, [proj], limit.
            let stale_sql = format!(
                "{select} WHERE pg.workspace_id = ? AND pg.is_latest = 1 \
                   AND pg.tier = 'episodic' AND pg.updated_at < ?{clause} \
                 ORDER BY pg.updated_at ASC LIMIT ?"
            );
            let mut stale_stmt = conn.prepare(&stale_sql)?;
            let stale = stale_stmt
                .query_map(
                    params_from_iter(
                        [ws(), Value::Integer(cutoff_30d)]
                            .into_iter()
                            .chain(proj.clone())
                            .chain(std::iter::once(Value::Integer(limit))),
                    ),
                    health_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;

            // Params: ws, [proj], ws(inner), [proj(inner)], limit.
            let dup_sql = format!(
                "{select} WHERE pg.workspace_id = ? AND pg.is_latest = 1{clause} \
                   AND pg.title IN ( \
                       SELECT title FROM pages \
                       WHERE workspace_id = ? AND is_latest = 1{inner_clause} \
                       GROUP BY title HAVING COUNT(*) > 1 \
                   ) \
                 ORDER BY pg.title, p.name LIMIT ?"
            );
            let mut dup_stmt = conn.prepare(&dup_sql)?;
            let duplicates = dup_stmt
                .query_map(
                    params_from_iter(
                        std::iter::once(ws())
                            .chain(proj.clone())
                            .chain(std::iter::once(ws()))
                            .chain(proj.clone())
                            .chain(std::iter::once(Value::Integer(limit))),
                    ),
                    health_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;

            // Params: ws, [proj], limit.
            let orphan_sql = format!(
                "{select} WHERE pg.workspace_id = ? AND pg.is_latest = 1{clause} \
                   AND NOT EXISTS (SELECT 1 FROM links l WHERE l.from_page_id = pg.id) \
                   AND NOT EXISTS (SELECT 1 FROM links l WHERE l.to_page_id = pg.id) \
                 ORDER BY pg.updated_at DESC LIMIT ?"
            );
            let mut orphan_stmt = conn.prepare(&orphan_sql)?;
            let orphans = orphan_stmt
                .query_map(
                    params_from_iter(
                        std::iter::once(ws())
                            .chain(proj.clone())
                            .chain(std::iter::once(Value::Integer(limit))),
                    ),
                    health_page_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(HealthDetail {
                stale,
                duplicates,
                orphans,
            })
        })
        .await
    }
}
