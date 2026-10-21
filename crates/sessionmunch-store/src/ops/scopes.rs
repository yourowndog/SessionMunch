//! Workspace and project creation, healing and hollow-project sweeps.

use super::*;

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
pub(super) fn project_name_in_other_workspaces(
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

pub(super) fn warn_project_name_in_other_workspaces(name: &str, also_in: &[String]) {
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
pub(super) fn should_heal_repo_path(repo_path: &str, home: Option<&str>) -> bool {
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

pub(super) fn normalize_repo_path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

pub(super) fn scheduler_state_table_exists(conn: &Connection) -> StoreResult<bool> {
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
