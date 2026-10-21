//! Handoff lookups and listings.

use super::*;

impl ReaderPool {
    /// Title and descriptor for a bounded set of already-fused page ids
    /// (#486).
    ///
    /// The vector stream returns `(PageId, PagePath, f32)` and nothing else,
    /// so a page it reaches first is inserted into the fusion map with empty
    /// strings and — because every fusion site uses `or_insert_with` — stays
    /// empty even when entity or graph later find it with a real descriptor
    /// already computed.
    ///
    /// Deliberately a post-fusion lookup rather than a wider embedding query.
    /// `top_embedding_hits_for_project` has no SQL `LIMIT`: it scores every
    /// embedded page in the project in Rust and takes top-k afterwards, so a
    /// descriptor column there would compute one for the whole corpus on
    /// every search. Here the input is only the handful of ids that survived
    /// fusion and still lack a title.
    /// Open handoffs for a project, oldest first (#513).
    ///
    /// `memory_handoff_cancel` takes an exact id and nothing exposed one, so a
    /// backlog could be observed in `status` counts but never addressed. Oldest
    /// first because the operator's question is "what is stale", and the
    /// automatic expiry deliberately spares manual and sibling-directory
    /// handoffs — which is how a months-old entry survives to be listed here.
    pub async fn open_handoffs_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        limit: usize,
    ) -> StoreResult<Vec<OpenHandoff>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, from_agent, to_agent, cwd, created_at \
                 FROM handoffs \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND state = 'open' \
                 ORDER BY created_at ASC, id ASC \
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    u64::try_from(limit).unwrap_or(u64::MAX),
                ],
                |row| {
                    let id: Vec<u8> = row.get(0)?;
                    Ok((
                        id,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (id, from_agent, to_agent, cwd, created_at) = row?;
                let Ok(id) = HandoffId::from_slice(&id) else {
                    continue;
                };
                out.push(OpenHandoff {
                    id: id.to_string(),
                    from_agent,
                    to_agent,
                    cwd,
                    created_at_ms: created_at / 1_000,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Return the open handoff the next session should pick up.
    ///
    /// A manual handoff (`memory_handoff_begin`, which always sets
    /// `from_session_id = None`) is project-wide and always a candidate,
    /// whatever cwd it carries. An auto SessionEnd handoff
    /// (`from_session_id = Some`) is a candidate only when its cwd is an
    /// ancestor-or-equal of `cwd_filter` (path-boundary, the prior art's
    /// check rather than exact equality: a handoff left in `/repo` reaches a
    /// session in `/repo/api`, but never `/repo-other`). Among candidates a
    /// manual handoff wins over an auto one, then the most recent, with cwd
    /// specificity only breaking timestamp ties — so an explicit "where we left off" baton
    /// deterministically beats the heuristic SessionEnd handoff, regardless of
    /// whether the model passed a cwd. With `cwd_filter == None` every open
    /// handoff is a candidate (project-wide read, e.g. the web overview). The
    /// path-boundary is computed in Rust rather than SQL `LIKE` so `%`/`_` in a
    /// path can never act as a wildcard.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_open_handoff(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cwd_filter: Option<String>,
        owner_filter: OwnerFilter,
    ) -> StoreResult<Option<Handoff>> {
        self.with_conn(move |conn| {
            // Ownership belongs in the query, not just in
            // `is_handoff_candidate`: prompt-derived fields from another
            // operator must never be loaded or deserialized for this caller.
            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 3);
            let sql = format!(
                "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                        cwd, summary, open_questions, next_steps, files_touched, state, \
                        created_at, accepted_by, accepted_at, accepted_by_session, \
                        owner_user, accepted_by_user \
                 FROM handoffs \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND state = 'open'{owner_clause} \
                 ORDER BY created_at DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> =
                vec![workspace_id.as_bytes(), project_id.as_bytes()];
            if let Some(owner) = owner_param.as_ref() {
                binds.push(owner);
            }
            let rows = stmt.query_map(binds.as_slice(), row_to_handoff)?;
            let mut selected: Option<Handoff> = None;
            for r in rows {
                let handoff = r??;
                if is_handoff_candidate(&handoff, cwd_filter.as_deref(), &owner_filter)
                    && selected
                        .as_ref()
                        .is_none_or(|current| prefer_handoff(&handoff, current).is_gt())
                {
                    selected = Some(handoff);
                }
            }
            Ok(selected)
        })
        .await
    }

    /// List a project's handoffs, newest first.
    ///
    /// The system had no handoff listing at all: every reader fetched "the
    /// single open one" and consumed it. That made a mis-delivered or
    /// prematurely consumed baton unrecoverable and invisible — there was no
    /// way to ask what happened to it. `state = None` returns every state so an
    /// operator can find an already-accepted handoff and read it back.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_handoffs(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        state: Option<HandoffState>,
        owner_filter: OwnerFilter,
        limit: usize,
    ) -> StoreResult<Vec<Handoff>> {
        let state = state.map(|s| s.as_str().to_string());
        self.with_conn(move |conn| {
            // Push the owner predicate and the limit into SQL rather than
            // filtering a fully materialised result set in Rust: without them
            // this reads (and sorts) every handoff the project has ever
            // accumulated, and the caller's limit provides no protection
            // because it is applied after the rows are already loaded.
            let (sql, owner_param) = handoff_listing_sql(state.is_some(), &owner_filter, limit);
            let mut stmt = conn.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> =
                vec![workspace_id.as_bytes(), project_id.as_bytes()];
            if let Some(state) = state.as_ref() {
                binds.push(state);
            }
            if let Some(owner) = owner_param.as_ref() {
                binds.push(owner);
            }
            let rows = stmt.query_map(binds.as_slice(), row_to_handoff)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row??);
            }
            Ok(out)
        })
        .await
    }

    /// Look up a handoff by id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn handoff_by_id(&self, handoff_id: HandoffId) -> StoreResult<Option<Handoff>> {
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                            cwd, summary, open_questions, next_steps, files_touched, state, \
                            created_at, accepted_by, accepted_at, accepted_by_session, \
                            owner_user, accepted_by_user \
                     FROM handoffs WHERE id = ?1",
                    params![handoff_id.as_bytes()],
                    row_to_handoff,
                )
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Look up a handoff by id within an exact scope and ownership boundary.
    ///
    /// This is the object-authorization form used by exact-id operations. The
    /// scope and owner predicates run before prompt-derived columns are loaded,
    /// so callers cannot distinguish a foreign id from an absent one or inspect
    /// another operator's malformed/private row as a side effect of checking it.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn handoff_by_id_in_scope(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        handoff_id: HandoffId,
        owner_filter: OwnerFilter,
    ) -> StoreResult<Option<Handoff>> {
        self.with_conn(move |conn| {
            let (owner_clause, owner_param) = handoff_owner_sql(&owner_filter, 4);
            let sql = format!(
                "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                        cwd, summary, open_questions, next_steps, files_touched, state, \
                        created_at, accepted_by, accepted_at, accepted_by_session, \
                        owner_user, accepted_by_user \
                 FROM handoffs \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND id = ?3{owner_clause}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = vec![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                handoff_id.as_bytes(),
            ];
            if let Some(owner) = owner_param.as_ref() {
                binds.push(owner);
            }
            let row = stmt
                .query_row(binds.as_slice(), row_to_handoff)
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Return the latest open handoff for the workspace, aggregating
    /// across all of its projects (no project filter).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_open_handoff_for_workspace(
        &self,
        workspace_id: WorkspaceId,
        owner_filter: OwnerFilter,
    ) -> StoreResult<Option<Handoff>> {
        self.with_conn(move |conn| {
            // The owner predicate is pushed into SQL (rather than filtered after
            // the fact like the project-scoped lookup) because this query keeps
            // its `LIMIT 1`: filtering afterwards would return nothing whenever
            // the newest open handoff in the workspace happened to belong to
            // somebody else.
            let owner_clause = match &owner_filter {
                OwnerFilter::Any => "",
                OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?2)",
                OwnerFilter::Unattributed => " AND owner_user IS NULL",
            };
            let sql = format!(
                "SELECT id, workspace_id, project_id, from_session_id, from_agent, to_agent, \
                        cwd, summary, open_questions, next_steps, files_touched, state, \
                        created_at, accepted_by, accepted_at, accepted_by_session, \
                        owner_user, accepted_by_user \
                 FROM handoffs \
                 WHERE workspace_id = ?1 AND state = 'open'{owner_clause} \
                 ORDER BY created_at DESC LIMIT 1"
            );
            let row_opt = match &owner_filter {
                OwnerFilter::User(user) => conn
                    .query_row(&sql, params![workspace_id.as_bytes(), user], row_to_handoff)
                    .optional()?,
                _ => conn
                    .query_row(&sql, params![workspace_id.as_bytes()], row_to_handoff)
                    .optional()?,
            };
            row_opt.transpose()
        })
        .await
    }
}
