//! Workspace rename and project/session moves.

use super::*;

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
