//! Sessions, their observations and the preserved session-event ledger.

use super::*;

impl ReaderPool {
    /// Return all observations for the given session, ordered by
    /// `created_at` ascending.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn observations_for_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<Observation>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, session_id, workspace_id, project_id, kind, extension, source_event, \
                        title, body, importance, created_at \
                 FROM observations \
                 WHERE session_id = ?1 \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map(params![session_id.as_bytes()], row_to_observation)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r??);
            }
            Ok(out)
        })
        .await
    }

    /// Read the promoted session-owned event ledger (newest first).
    ///
    /// Returns up to `limit` events for `session_id`. This is the authoritative
    /// history going forward; [`Self::observations_for_session`] remains for
    /// compatibility with the old capture path.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_events(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> StoreResult<Vec<SessionEvent>> {
        self.with_conn(move |conn| {
            crate::session_events::list_session_events(conn, &session_id, limit)
        })
        .await
    }

    /// The session's transcript events (imported or managed, never hook
    /// mirrors), newest first, at most `limit` capped at
    /// [`crate::MAX_TRANSCRIPT_EVENTS`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_transcript_events(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> StoreResult<Vec<SessionEvent>> {
        self.with_conn(move |conn| {
            crate::session_events::list_transcript_events(conn, &session_id, limit)
        })
        .await
    }

    /// Search the session event ledger with optional scope, machine,
    /// harness, and time filters.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_session_events_filtered(
        &self,
        search: SessionEventSearch,
    ) -> StoreResult<Vec<SessionEventHit>> {
        self.with_conn(move |conn| crate::session_events::search_session_events(conn, &search))
            .await
    }

    /// Search session events globally across all sessions using full-text search.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_session_events(
        &self,
        query: String,
        limit: usize,
    ) -> StoreResult<Vec<SessionEventHit>> {
        self.search_session_events_filtered(SessionEventSearch {
            query,
            limit,
            ..SessionEventSearch::default()
        })
        .await
    }

    /// Search session events scoped to a workspace and project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_session_events_scoped(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        limit: usize,
    ) -> StoreResult<Vec<SessionEventHit>> {
        self.search_session_events_filtered(SessionEventSearch {
            query,
            limit,
            scope: Some((workspace_id, project_id)),
            ..SessionEventSearch::default()
        })
        .await
    }

    /// Page through one session's raw observations as seen from one scope.
    ///
    /// Only the rows that landed in `(workspace_id, project_id)` are returned:
    /// a session that crossed repositories mid-flight has observations in more
    /// than one scope, and a caller resolved to one of them must not read the
    /// other. `elided_other_scope` reports how many rows were left out so the
    /// caller can tell a short session from a partially visible one. `total`
    /// counts the in-scope rows matching the page's kind and query filters, so
    /// paginating needs no second round-trip; a zero `limit` returns no rows
    /// but still reports both counts.
    ///
    /// The query goes through the same FTS5 normalization as
    /// [`Self::search_observations_for_project`] and is dropped when nothing
    /// searchable remains, so a punctuation-only query lists the session
    /// instead of matching nothing.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_observations_scoped(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        page: ObservationPage,
    ) -> StoreResult<ObservationPageResult> {
        let fts_query = page
            .query
            .as_deref()
            .map(normalize_fts_query)
            .filter(|q| !q.is_empty());
        let kinds: Vec<&'static str> = page
            .kinds
            .iter()
            .flatten()
            .map(ObservationKind::as_str)
            .collect();
        let direction = match page.order {
            ObservationOrder::Asc => "ASC",
            ObservationOrder::Desc => "DESC",
        };
        let (limit, offset) = (page.limit, page.offset);
        self.with_conn(move |conn| {
            // `?` is positional-by-order: session, scope, then the optional
            // MATCH term and kind list, then LIMIT/OFFSET on the page query.
            let mut sql_params: Vec<Value> = Vec::with_capacity(kinds.len() + 6);
            sql_params.push(Value::Blob(session_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            let mut source = String::from("FROM observations");
            let mut predicate = String::from(
                " WHERE observations.session_id = ? \
                   AND observations.workspace_id = ? \
                   AND observations.project_id = ?",
            );
            if let Some(q) = fts_query {
                source.push_str(
                    " JOIN observations_fts ON observations_fts.rowid = observations.rowid",
                );
                predicate.push_str(" AND observations_fts MATCH ?");
                sql_params.push(Value::Text(q));
            }
            if !kinds.is_empty() {
                predicate.push_str(" AND observations.kind IN (");
                for (idx, kind) in kinds.iter().enumerate() {
                    if idx > 0 {
                        predicate.push_str(", ");
                    }
                    predicate.push('?');
                    sql_params.push(Value::Text((*kind).to_string()));
                }
                predicate.push(')');
            }

            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) {source}{predicate}"),
                params_from_iter(sql_params.iter()),
                |row| row.get(0),
            )?;
            let elided: i64 = conn.query_row(
                "SELECT COUNT(*) FROM observations \
                 WHERE session_id = ?1 AND NOT (workspace_id = ?2 AND project_id = ?3)",
                params![
                    session_id.as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes()
                ],
                |row| row.get(0),
            )?;

            let mut records = Vec::new();
            if limit > 0 {
                sql_params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
                sql_params.push(Value::Integer(i64::try_from(offset).unwrap_or(i64::MAX)));
                let sql = format!(
                    "SELECT observations.id, observations.session_id, observations.kind, \
                            observations.title, observations.body, observations.importance, \
                            observations.created_at, observations.extension, \
                            observations.source_event \
                     {source}{predicate} \
                     ORDER BY observations.created_at {direction}, observations.id {direction} \
                     LIMIT ? OFFSET ?"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(
                    params_from_iter(sql_params.iter()),
                    row_to_observation_record,
                )?;
                for row in rows {
                    records.push(row??);
                }
            }
            Ok(ObservationPageResult {
                records,
                total: u64::try_from(total).unwrap_or(0),
                elided_other_scope: u64::try_from(elided).unwrap_or(0),
            })
        })
        .await
    }

    /// Return the `(workspace_id, project_id)` with the most recent recorded
    /// activity at or after `since_us` (wall clock, microseconds).
    ///
    /// Observations are the complete activity log — every lifecycle hook event
    /// lands as one, stamped with the scope it was routed to — so the newest
    /// row is the project the operator was last working in. Used at startup to
    /// seed the in-memory active-project fallback, which a restart otherwise
    /// drops (#678). The caller supplies the cutoff so a long-idle server does
    /// not resurrect a scope the live pointer would have expired anyway.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn most_recently_active_scope(
        &self,
        since_us: i64,
    ) -> StoreResult<Option<(WorkspaceId, ProjectId)>> {
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT workspace_id, project_id FROM observations \
                     WHERE created_at >= ?1 \
                     ORDER BY created_at DESC LIMIT 1",
                    params![since_us],
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .optional()?;
            match row {
                Some((ws, proj)) => Ok(Some((
                    WorkspaceId::from_slice(&ws)?,
                    ProjectId::from_slice(&proj)?,
                ))),
                None => Ok(None),
            }
        })
        .await
    }

    /// Return the latest completed session for a project.
    ///
    /// Used by read-only review tools that need a natural default when the user
    /// asks what the project just learned without naming a session id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_completed_session_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Option<SessionId>> {
        self.with_conn(move |conn| {
            let row_opt: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT id FROM sessions \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND ended_at IS NOT NULL \
                     ORDER BY ended_at DESC, started_at DESC LIMIT 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            row_opt
                .map(|bytes| SessionId::from_slice(&bytes).map_err(StoreError::from))
                .transpose()
        })
        .await
    }

    /// Return the latest completed session that has no persisted
    /// auto-improvement run in the same project.
    ///
    /// This is the implicit queue used by manual auto-improvement. Explicitly
    /// named sessions remain rerunnable, while an omitted session id advances
    /// past both full reviews and preflight-skipped runs.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_unreviewed_completed_session_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Option<SessionId>> {
        self.with_conn(move |conn| {
            let row_opt: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT s.id FROM sessions s \
                     WHERE s.workspace_id = ?1 \
                       AND s.project_id = ?2 \
                       AND s.ended_at IS NOT NULL \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM auto_improve_runs r \
                           WHERE r.workspace_id = s.workspace_id \
                             AND r.project_id = s.project_id \
                             AND r.session_id = s.id \
                       ) \
                     ORDER BY s.ended_at DESC, s.started_at DESC LIMIT 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            row_opt
                .map(|bytes| SessionId::from_slice(&bytes).map_err(StoreError::from))
                .transpose()
        })
        .await
    }

    /// Sum per-client MCP tool-call counters, optionally bounded to
    /// buckets at or after `since_day` (UTC days since the epoch;
    /// `None` = whole history). Ordered by total volume descending
    /// with a client-name tiebreak so equal totals do not reorder
    /// between calls.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn client_activity_since(
        &self,
        since_day: Option<i64>,
    ) -> StoreResult<Vec<ClientActivity>> {
        self.with_conn(move |conn| {
            let since_clause = if since_day.is_some() {
                " WHERE day >= :since"
            } else {
                ""
            };
            let sql = format!(
                "SELECT client, SUM(reads), SUM(writes) FROM client_activity\
                 {since_clause} \
                 GROUP BY client \
                 ORDER BY SUM(reads) + SUM(writes) DESC, client ASC"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let map = |row: &rusqlite::Row<'_>| {
                let client: String = row.get(0)?;
                let reads: i64 = row.get(1)?;
                let writes: i64 = row.get(2)?;
                Ok((client, reads, writes))
            };
            let rows = match &since_day {
                Some(since) => stmt.query_map(&[(":since", since as &dyn rusqlite::ToSql)], map)?,
                None => stmt.query_map([], map)?,
            };
            let mut out = Vec::new();
            for row in rows {
                let (client, reads, writes) = row?;
                out.push(ClientActivity {
                    client,
                    reads: u64::try_from(reads).unwrap_or(0),
                    writes: u64::try_from(writes).unwrap_or(0),
                });
            }
            Ok(out)
        })
        .await
    }

    /// Count sessions per agent CLI in one scope, newest window first.
    ///
    /// Counts every session the window covers, open or ended — the question
    /// is which tools produced this project's memory, not which are running
    /// right now (`open_sessions_for_scope_agent` answers that).
    ///
    /// `since_us` is an inclusive lower bound on `started_at`; `None` counts
    /// the project's whole history. The scan rides
    /// `idx_sessions_recent (workspace_id, project_id, started_at DESC)`, so
    /// it stays bounded by scope rather than table-wide.
    ///
    /// Ordering is count-descending with an agent-name tiebreak, so equal
    /// counts do not reorder between calls.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_counts_by_agent(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        owner_filter: OwnerFilter,
        since_us: Option<i64>,
    ) -> StoreResult<Vec<AgentSessionCount>> {
        self.with_conn(move |conn| {
            // Same reasoning as `open_sessions_for_scope_agent`: without the
            // owner predicate a shared server reports a teammate's activity as
            // the caller's own.
            let owner_clause = match &owner_filter {
                OwnerFilter::Any => "",
                OwnerFilter::User(_) => " AND (actor_user IS NULL OR actor_user = :actor)",
                OwnerFilter::Unattributed => " AND actor_user IS NULL",
            };
            let since_clause = if since_us.is_some() {
                " AND started_at >= :since"
            } else {
                ""
            };
            let sql = format!(
                "SELECT agent_kind, COUNT(*) AS n FROM sessions \
                 WHERE workspace_id = :ws AND project_id = :proj\
                 {since_clause}{owner_clause} \
                 GROUP BY agent_kind \
                 ORDER BY n DESC, agent_kind ASC"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let ws_bytes = workspace_id.as_bytes();
            let proj_bytes = project_id.as_bytes();
            let mut named: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
                (":ws", &ws_bytes as &dyn rusqlite::ToSql),
                (":proj", &proj_bytes as &dyn rusqlite::ToSql),
            ];
            if let Some(since) = &since_us {
                named.push((":since", since));
            }
            if let OwnerFilter::User(user) = &owner_filter {
                named.push((":actor", user));
            }
            let rows = stmt.query_map(named.as_slice(), |row| {
                let agent: String = row.get(0)?;
                let n: i64 = row.get(1)?;
                Ok((agent, n))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (agent, n) = row?;
                out.push(AgentSessionCount {
                    agent,
                    sessions: u64::try_from(n).unwrap_or(0),
                });
            }
            Ok(out)
        })
        .await
    }

    /// Return open sessions matching one scoped project and agent.
    ///
    /// Results are newest-first so callers can default to finalizing only the
    /// latest open session while offering an explicit all-sessions mode.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn open_sessions_for_scope_agent(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        agent_kind: AgentKind,
        owner_filter: OwnerFilter,
        limit: Option<usize>,
    ) -> StoreResult<Vec<OpenSession>> {
        self.open_sessions_for_scope_agent_filtered(
            workspace_id,
            project_id,
            agent_kind,
            owner_filter,
            limit,
            None,
        )
        .await
    }

    /// Return one exact open session for a scoped project and agent.
    ///
    /// The id narrows the scope, agent, open-state, and owner predicates; it
    /// never replaces them. A known id therefore cannot expose or finalize a
    /// colleague's session unless the caller explicitly uses
    /// [`OwnerFilter::Any`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn open_session_for_scope_agent_by_id(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        agent_kind: AgentKind,
        owner_filter: OwnerFilter,
        session_id: SessionId,
    ) -> StoreResult<Option<OpenSession>> {
        let mut sessions = self
            .open_sessions_for_scope_agent_filtered(
                workspace_id,
                project_id,
                agent_kind,
                owner_filter,
                Some(1),
                Some(session_id),
            )
            .await?;
        Ok(sessions.pop())
    }

    pub(super) async fn open_sessions_for_scope_agent_filtered(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        agent_kind: AgentKind,
        owner_filter: OwnerFilter,
        limit: Option<usize>,
        exact_session_id: Option<SessionId>,
    ) -> StoreResult<Vec<OpenSession>> {
        let agent = agent_kind.as_str().to_string();
        self.with_conn(move |conn| {
            let limit_clause = limit.map_or(String::new(), |n| format!(" LIMIT {}", n.max(1)));
            // Without the owner predicate this returns whichever session in the
            // scope started last — frequently a teammate's live one — and the
            // callers act on it destructively.
            let owner_clause = match &owner_filter {
                OwnerFilter::Any => "",
                OwnerFilter::User(_) => " AND (actor_user IS NULL OR actor_user = ?4)",
                OwnerFilter::Unattributed => " AND actor_user IS NULL",
            };
            let session_id_placeholder = if matches!(owner_filter, OwnerFilter::User(_)) {
                "?5"
            } else {
                "?4"
            };
            let session_id_clause = if exact_session_id.is_some() {
                format!(" AND id = {session_id_placeholder}")
            } else {
                String::new()
            };
            let sql = format!(
                "SELECT id, cwd FROM sessions \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                   AND agent_kind = ?3 AND ended_at IS NULL{owner_clause}{session_id_clause} \
                 ORDER BY started_at DESC, id DESC{limit_clause}"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let row_map = |row: &rusqlite::Row<'_>| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let cwd: Option<String> = row.get(1)?;
                Ok((id_bytes, cwd))
            };
            let rows = match (&owner_filter, exact_session_id) {
                (OwnerFilter::User(user), Some(sid)) => stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        agent,
                        user,
                        sid.as_bytes()
                    ],
                    row_map,
                )?,
                (OwnerFilter::User(user), None) => stmt.query_map(
                    params![workspace_id.as_bytes(), project_id.as_bytes(), agent, user],
                    row_map,
                )?,
                (_, Some(sid)) => stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        agent,
                        sid.as_bytes()
                    ],
                    row_map,
                )?,
                (_, None) => stmt.query_map(
                    params![workspace_id.as_bytes(), project_id.as_bytes(), agent],
                    row_map,
                )?,
            };
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, cwd) = row?;
                out.push(OpenSession {
                    session_id: SessionId::from_slice(&id_bytes)?,
                    cwd,
                });
            }
            Ok(out)
        })
        .await
    }

    /// List the sessions that touched one scope, newest first.
    ///
    /// A session is listed when its `sessions` row is anchored in the scope OR
    /// at least one of its observations landed there. The second half matters
    /// for a session that changed repositories mid-flight: its row stays
    /// frozen on the starting scope (`begin_session` is `ON CONFLICT DO
    /// NOTHING`), so listing by row alone would hide it from the very scope
    /// that holds its work. `observation_count` counts only the rows in this
    /// scope. `include_open == false` keeps sessions with `ended_at` set.
    ///
    /// The owner predicate follows the rest of the session surface: shared
    /// (`actor_user IS NULL`) rows are visible to everyone, owned rows only to
    /// their operator unless the caller passes [`OwnerFilter::Any`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn sessions_for_scope(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        owner_filter: OwnerFilter,
        include_open: bool,
        limit: usize,
        offset: usize,
    ) -> StoreResult<Vec<SessionSummary>> {
        self.sessions_for_scope_filtered(
            workspace_id,
            project_id,
            owner_filter,
            include_open,
            limit,
            offset,
            None,
        )
        .await
    }

    /// Return one session as seen from one scope, or `None` when the session
    /// has neither its row nor any observation there, or when the owner
    /// filter rejects it. The id narrows the same predicates
    /// [`Self::sessions_for_scope`] applies (open sessions included); it
    /// never replaces them, so a known id cannot read across scopes or
    /// operators.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_summary_scoped(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        owner_filter: OwnerFilter,
    ) -> StoreResult<Option<SessionSummary>> {
        let mut sessions = self
            .sessions_for_scope_filtered(
                workspace_id,
                project_id,
                owner_filter,
                true,
                1,
                0,
                Some(session_id),
            )
            .await?;
        Ok(sessions.pop())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn sessions_for_scope_filtered(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        owner_filter: OwnerFilter,
        include_open: bool,
        limit: usize,
        offset: usize,
        exact_session_id: Option<SessionId>,
    ) -> StoreResult<Vec<SessionSummary>> {
        self.with_conn(move |conn| {
            // The point lookup lets the primary key narrow first so the scope
            // test is one indexed probe; the listing materializes the scope's
            // session ids once instead of probing every session in the table.
            let membership = if exact_session_id.is_some() {
                " AND s.id = :sid \
                  AND ((s.workspace_id = :ws AND s.project_id = :proj) \
                       OR EXISTS (SELECT 1 FROM observations o \
                                  WHERE o.session_id = s.id \
                                    AND o.workspace_id = :ws AND o.project_id = :proj))"
            } else {
                " AND s.id IN (SELECT id FROM sessions \
                               WHERE workspace_id = :ws AND project_id = :proj \
                               UNION \
                               SELECT session_id FROM observations \
                               WHERE workspace_id = :ws AND project_id = :proj)"
            };
            let owner_clause = match &owner_filter {
                OwnerFilter::Any => "",
                OwnerFilter::User(_) => " AND (s.actor_user IS NULL OR s.actor_user = :actor)",
                OwnerFilter::Unattributed => " AND s.actor_user IS NULL",
            };
            let ended_clause = if include_open {
                ""
            } else {
                " AND s.ended_at IS NOT NULL"
            };
            let sql = format!(
                "SELECT s.id, s.cwd, s.agent_kind, s.started_at, s.ended_at, s.actor_user, \
                        (SELECT COUNT(*) FROM observations o \
                         WHERE o.session_id = s.id \
                           AND o.workspace_id = :ws AND o.project_id = :proj) AS n \
                 FROM sessions s \
                 WHERE 1 = 1{membership}{owner_clause}{ended_clause} \
                 ORDER BY s.started_at DESC, s.id DESC \
                 LIMIT :limit OFFSET :offset"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let ws_bytes = workspace_id.as_bytes();
            let proj_bytes = project_id.as_bytes();
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let offset = i64::try_from(offset).unwrap_or(i64::MAX);
            let sid_bytes = exact_session_id.map(|sid| *sid.as_bytes());
            let mut named: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
                (":ws", &ws_bytes as &dyn rusqlite::ToSql),
                (":proj", &proj_bytes as &dyn rusqlite::ToSql),
                (":limit", &limit),
                (":offset", &offset),
            ];
            if let Some(sid) = &sid_bytes {
                named.push((":sid", sid));
            }
            if let OwnerFilter::User(user) = &owner_filter {
                named.push((":actor", user));
            }
            let rows = stmt.query_map(named.as_slice(), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let cwd: Option<String> = row.get(1)?;
                let agent_kind: String = row.get(2)?;
                let started_us: i64 = row.get(3)?;
                let ended_us: Option<i64> = row.get(4)?;
                let actor_user: Option<String> = row.get(5)?;
                let n: i64 = row.get(6)?;
                Ok((
                    id_bytes, cwd, agent_kind, started_us, ended_us, actor_user, n,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, cwd, agent_kind, started_us, ended_us, actor_user, n) = row?;
                let started_at = jiff::Timestamp::from_microsecond(started_us)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default();
                let ended_at = ended_us
                    .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                    .map(|ts| ts.to_string());
                out.push(SessionSummary {
                    session_id: SessionId::from_slice(&id_bytes)?,
                    cwd,
                    agent_kind,
                    started_at,
                    ended_at,
                    observation_count: u64::try_from(n).unwrap_or(0),
                    actor_user,
                });
            }
            Ok(out)
        })
        .await
    }

    /// The `(workspace_id, project_id, cwd)` a session was created under,
    /// or `None` when no such session exists. The hook router uses this for
    /// session-sticky attribution: mid-session events inherit the session's
    /// scope instead of re-deriving a project from the event's cwd, so a
    /// `cd subdir/` inside a non-git project (whose parent has no
    /// `repo_path` for the prefix match to key on) can no longer scatter
    /// observations into basename-fragment projects. The session's own cwd
    /// is returned so the router can bound stickiness to the session's
    /// directory subtree.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_session_scope(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<(WorkspaceId, ProjectId, Option<String>)>> {
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT workspace_id, project_id, cwd FROM sessions WHERE id = ?1",
                    params![session_id.as_bytes()],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?;
            match row {
                Some((ws, proj, cwd)) => Ok(Some((
                    WorkspaceId::from_slice(&ws)?,
                    ProjectId::from_slice(&proj)?,
                    cwd,
                ))),
                None => Ok(None),
            }
        })
        .await
    }

    /// Ids of every session that touches `(workspace_id, project_id)`: a
    /// `sessions` row in the scope OR at least one observation stamped into
    /// it. The second leg catches the phantom projects that mid-session
    /// routing before the sticky mode filled with observations of sessions
    /// rooted elsewhere (no `sessions` row of their own), so a batch
    /// `move-session` can empty such a bucket. Ordered by first touch (the
    /// row's `started_at` or the earliest observation in the scope), then id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_ids_touching_scope(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<SessionId>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM ( \
                     SELECT id, started_at AS first_seen FROM sessions \
                     WHERE workspace_id = ?1 AND project_id = ?2 \
                     UNION ALL \
                     SELECT session_id AS id, MIN(created_at) AS first_seen FROM observations \
                     WHERE workspace_id = ?1 AND project_id = ?2 \
                     GROUP BY session_id \
                 ) \
                 GROUP BY id \
                 ORDER BY MIN(first_seen), id",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(SessionId::from_slice(&r?)?);
            }
            Ok(out)
        })
        .await
    }

    /// Rows keyed to a session across every scope, plus its
    /// `sessions/<id>.md` page rows in one scope: what a `move_session` into
    /// a scope that holds nothing of the session yet would touch. Used for
    /// the dry run of a move whose destination does not exist yet.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_dependent_rows(
        &self,
        session_id: SessionId,
        page_scope: (WorkspaceId, ProjectId),
    ) -> StoreResult<SessionDependentRows> {
        self.with_conn(move |conn| {
            let sid = session_id.as_bytes();
            let count = |sql: &str| -> StoreResult<u64> {
                Ok(conn.query_row(sql, [sid.as_slice()], |r| r.get::<_, i64>(0))? as u64)
            };
            let (page_versions, page_latest): (i64, i64) = conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(is_latest), 0) FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
                params![
                    page_scope.0.as_bytes(),
                    page_scope.1.as_bytes(),
                    format!("sessions/{session_id}.md"),
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(SessionDependentRows {
                observations: count("SELECT COUNT(*) FROM observations WHERE session_id = ?1")?,
                handoffs: count("SELECT COUNT(*) FROM handoffs WHERE from_session_id = ?1")?,
                consolidation_jobs: count(
                    "SELECT COUNT(*) FROM session_consolidation_jobs WHERE session_id = ?1",
                )?,
                auto_improve_runs: count(
                    "SELECT COUNT(*) FROM auto_improve_runs WHERE session_id = ?1",
                )?,
                auto_improve_claims: count(
                    "SELECT COUNT(*) FROM auto_improve_scheduler_claims WHERE session_id = ?1",
                )?,
                page_versions: page_versions as u64,
                page_latest: page_latest as u64,
            })
        })
        .await
    }

    /// How a `SessionEnd` should treat its target session (issue #152).
    ///
    /// The old boolean ("is the session open?") conflated two very different
    /// ended states: a *duplicate/stale* end (the observation generation did
    /// not advance — drop it, the reason the guard exists) and a *re-end* of a
    /// resumed session (the agent reused the id and kept working after the
    /// first end — the end path must run again or the resumed work never
    /// reaches the compiled session page). A persisted count is deliberately
    /// used instead of comparing wall clocks, which does not converge after
    /// clock skew (issue #261).
    pub async fn session_end_disposition(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        agent_kind: AgentKind,
    ) -> StoreResult<SessionEndDisposition> {
        let agent = agent_kind.as_str().to_string();
        self.with_conn(move |conn| {
            let end_state: Option<(Option<i64>, u64)> = conn
                .query_row(
                    "SELECT ended_at, ended_observation_count FROM sessions \
                     WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
                       AND agent_kind = ?4",
                    params![
                        session_id.as_bytes(),
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        agent,
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((ended_at, ended_observation_count)) = end_state else {
                // Missing, cross-scope, or cross-agent: never end it.
                return Ok(SessionEndDisposition::DropInvalid);
            };
            if ended_at.is_none() {
                return Ok(SessionEndDisposition::Open);
            }
            let current_observation_count: u64 = conn.query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )?;
            if current_observation_count > ended_observation_count {
                Ok(SessionEndDisposition::ReEndWithNewWork)
            } else {
                Ok(SessionEndDisposition::AlreadyEnded)
            }
        })
        .await
    }

    /// Return completed sessions eligible for scheduled auto-improvement.
    ///
    /// The scheduler uses a persisted first-run watermark so an upgrade with an
    /// LLM provider configured does not chew through historical backlog by
    /// default. The scheduler inserts a per-session claim before the LLM call,
    /// so failed scheduled reviews do not retry forever or starve newer
    /// sessions. Any existing auto-improvement run row for a session also counts
    /// as a scheduler review for candidate selection; manual reruns remain
    /// available through CLI/admin/MCP.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn auto_improve_candidate_sessions(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        min_session_age_secs: u64,
        limit: usize,
    ) -> StoreResult<Vec<AutoImproveCandidateSession>> {
        let cutoff = Timestamp::now().as_microsecond().saturating_sub(
            (min_session_age_secs.min(i64::MAX as u64 / 1_000_000) as i64) * 1_000_000,
        );
        self.with_conn(move |conn| {
            let watermark: Option<i64> = conn
                .query_row(
                    "SELECT watermark_ended_at FROM auto_improve_scheduler_state \
                     WHERE workspace_id = ?1 AND project_id = ?2",
                    params![workspace_id.as_bytes(), project_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(watermark) = watermark else {
                return Ok(Vec::new());
            };

            let mut stmt = conn.prepare_cached(
                "SELECT s.id, s.ended_at FROM sessions s \
                 WHERE s.workspace_id = ?1 \
                   AND s.project_id = ?2 \
                   AND s.ended_at IS NOT NULL \
                   AND s.ended_at > ?3 \
                   AND s.ended_at <= ?4 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM auto_improve_scheduler_claims c \
                       WHERE c.workspace_id = s.workspace_id \
                         AND c.project_id = s.project_id \
                         AND c.session_id = s.id \
                   ) \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM auto_improve_runs r \
                       WHERE r.workspace_id = s.workspace_id \
                         AND r.project_id = s.project_id \
                         AND r.session_id = s.id \
                   ) \
                 ORDER BY s.ended_at ASC, s.started_at ASC \
                 LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    watermark,
                    cutoff,
                    limit.min(i64::MAX as usize) as i64,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    Ok((id_bytes, row.get::<_, i64>(1)?))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, ended_at) = row?;
                out.push(AutoImproveCandidateSession {
                    session_id: SessionId::from_slice(&id_bytes)?,
                    ended_at,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Look up the `(workspace_id, project_id)` a session belongs to.
    /// Returns `None` when no such session row exists.
    ///
    /// Used by the consolidator + lint pass to write pages into the
    /// SESSION'S project, not the server's startup defaults — every
    /// session row carries the project_id the hook router resolved
    /// from its per-cwd basename heuristic, which is the correct
    /// target for any wiki page derived from that session.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_project_ids(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<(WorkspaceId, ProjectId)>> {
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare("SELECT workspace_id, project_id FROM sessions WHERE id = ?1")?;
            let mut rows = stmt.query(params![session_id.as_bytes()])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let ws_bytes: Vec<u8> = row.get(0)?;
            let proj_bytes: Vec<u8> = row.get(1)?;
            let ws = WorkspaceId::from_slice(&ws_bytes)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, 0))?;
            let proj = ProjectId::from_slice(&proj_bytes)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, 0))?;
            Ok(Some((ws, proj)))
        })
        .await
    }

    /// Look up the immutable harness identity stored for a session.
    ///
    /// Machine-generated session pages use this as their origin metadata.
    /// Reading it from the session row, rather than from the request that
    /// happens to trigger consolidation, keeps spool drains and later
    /// superseding writes attributed to the harness that created the session.
    /// Returns `None` when no such session row exists.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_agent_kind(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<AgentKind>> {
        self.with_conn(move |conn| {
            let stored: Option<String> = conn
                .query_row(
                    "SELECT agent_kind FROM sessions WHERE id = ?1",
                    params![session_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(stored.map(|value| AgentKind::from_wire(&value)))
        })
        .await
    }

    /// The operator a session belongs to (an
    /// [`sessionmunch_core::IdentityKey::storage_key`] string), as recorded at
    /// session start.
    ///
    /// The identity carrying a SessionEnd is not necessarily the identity that
    /// owned the session: a spool drain, an operator finalizing a stuck
    /// session, or a shared static hook token can all deliver it. Attribution
    /// for the session's page and the baton it leaves must follow the session,
    /// not the request.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_actor_user(&self, session_id: SessionId) -> StoreResult<Option<String>> {
        self.with_conn(move |conn| {
            let owner: Option<Option<String>> = conn
                .query_row(
                    "SELECT actor_user FROM sessions WHERE id = ?1",
                    params![session_id.as_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(owner.flatten())
        })
        .await
    }

    /// Resolve the `(workspace, project)` where a session's observations
    /// actually landed, independent of the (possibly stale) `sessions` row.
    ///
    /// The `sessions` row is frozen at the first session-start (`begin_session`
    /// uses `ON CONFLICT(id) DO NOTHING`), so a "hybrid" session that installed
    /// its scope marker mid-flight stays anchored to the pre-marker scope. The
    /// observation log, in contrast, carries the correct per-cwd scope for each
    /// captured event. Returns the scope holding the most observations for the
    /// session (ties broken by the most recent observation), or `None` when the
    /// session has no observations.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_scope_from_observations(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<(WorkspaceId, ProjectId)>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT workspace_id, project_id \
                 FROM observations \
                 WHERE session_id = ?1 \
                 GROUP BY workspace_id, project_id \
                 ORDER BY COUNT(*) DESC, MAX(created_at) DESC \
                 LIMIT 1",
            )?;
            let mut rows = stmt.query(params![session_id.as_bytes()])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let ws_bytes: Vec<u8> = row.get(0)?;
            let proj_bytes: Vec<u8> = row.get(1)?;
            let ws = WorkspaceId::from_slice(&ws_bytes).map_err(|e| {
                StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(format!(
                    "bad observation workspace_id: {e}"
                )))
            })?;
            let proj = ProjectId::from_slice(&proj_bytes).map_err(|e| {
                StoreError::Memory(sessionmunch_core::MemoryError::MalformedRecord(format!(
                    "bad observation project_id: {e}"
                )))
            })?;
            Ok(Some((ws, proj)))
        })
        .await
    }
}

/// Frontmatter keys a generated session page may carry: the rule-based
/// summary's, the session card's, and those every wiki write stamps on disk
/// (`type`, `description`, `sources`, `generated`, `pinned`,
/// `last_modified_by`). A `sessions/<id>.md` page with any other key (an LLM
/// consolidation's `consolidated`, tags, relations, or anything a person
/// added) is not replaced by a card.
pub const SESSION_CARD_REPLACEABLE_KEYS: &[&str] = &[
    "title",
    "session_id",
    "agent",
    "tier",
    "summary",
    "generator",
    "card",
    "type",
    "description",
    "sources",
    "generated",
    "pinned",
    "last_modified_by",
];

impl ReaderPool {
    /// Sessions whose deterministic card is due, at most `limit`, oldest id
    /// first: the session has transcript (non-hook) ledger rows, and its latest
    /// `sessions/<id>.md` page is missing, or is a generated page (only
    /// [`SESSION_CARD_REPLACEABLE_KEYS`] in its frontmatter) whose
    /// `card.builder_version` is older than `builder_version` or whose
    /// `card.last_sequence` is behind its transcript rows. `only` restricts the check
    /// to one session.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn session_cards_due(
        &self,
        builder_version: u32,
        only: Option<SessionId>,
        limit: usize,
    ) -> StoreResult<Vec<SessionId>> {
        self.with_conn(move |conn| {
            let keys = SESSION_CARD_REPLACEABLE_KEYS
                .iter()
                .map(|k| format!("'{k}'"))
                .collect::<Vec<_>>()
                .join(",");
            // The page path is `sessions/<hyphenated lowercase uuid>.md`,
            // rebuilt from the session's 16-byte id.
            let sql = format!(
                "SELECT s.id FROM sessions s \
                 WHERE EXISTS (SELECT 1 FROM session_events e \
                               WHERE e.session_id = s.id AND e.source != 'hook') \
                   AND (?3 IS NULL OR s.id = ?3) \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM pages pg \
                     WHERE pg.workspace_id = s.workspace_id AND pg.project_id = s.project_id \
                       AND pg.is_latest = 1 \
                       AND pg.path = 'sessions/' || lower( \
                             substr(hex(s.id), 1, 8) || '-' || substr(hex(s.id), 9, 4) || '-' || \
                             substr(hex(s.id), 13, 4) || '-' || substr(hex(s.id), 17, 4) || '-' || \
                             substr(hex(s.id), 21, 12)) || '.md' \
                       AND ( \
                         (json_extract(pg.frontmatter_json, '$.card.builder_version') >= ?1 \
                          AND json_extract(pg.frontmatter_json, '$.card.last_sequence') >= \
                              (SELECT MAX(e2.sequence) FROM session_events e2 \
                               WHERE e2.session_id = s.id AND e2.source != 'hook')) \
                         OR EXISTS (SELECT 1 FROM json_each(pg.frontmatter_json) j \
                                    WHERE j.key NOT IN ({keys})))) \
                 ORDER BY s.id LIMIT ?2"
            );
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let only = only.map(|id| id.as_bytes().to_vec());
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![builder_version, limit, only], |row| {
                row.get::<_, Vec<u8>>(0)
            })?;
            let mut due = Vec::new();
            for row in rows {
                due.push(SessionId::from_slice(&row?)?);
            }
            Ok(due)
        })
        .await
    }
}
