//! Workspace and project lookups and listings.

use super::*;

impl ReaderPool {
    /// Look up a workspace name by id.
    ///
    /// Returns `None` when no matching workspace exists. Used by the
    /// admission webhook chain to populate `AdmissionContext.workspace`
    /// so external webhooks can address the page by human name without
    /// re-implementing UUID→name lookup against the engine's store.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn workspace_name_by_id(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Option<String>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT name FROM workspaces WHERE id = ?1",
                    params![workspace_id.as_bytes()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(row_opt)
        })
        .await
    }

    /// Look up a project name by id within a workspace.
    ///
    /// Returns `None` when no matching project exists.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn project_name_by_id(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Option<String>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT name FROM projects WHERE id = ?1 AND workspace_id = ?2",
                    params![project_id.as_bytes(), workspace_id.as_bytes()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(row_opt)
        })
        .await
    }

    /// Return one row per (workspace, project) with page-count and
    /// last-updated aggregates. Used by the web UI project-list view.
    ///
    /// Only `is_latest = 1` pages are counted.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_projects_with_stats(&self) -> StoreResult<Vec<ProjectSummary>> {
        self.list_projects_with_stats_filtered(None).await
    }

    /// Return one row per project within one workspace.
    ///
    /// Only `is_latest = 1` pages are counted.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_projects_with_stats_for_workspace(
        &self,
        workspace: String,
    ) -> StoreResult<Vec<ProjectSummary>> {
        self.list_projects_with_stats_filtered(Some(workspace))
            .await
    }

    pub(super) async fn list_projects_with_stats_filtered(
        &self,
        workspace: Option<String>,
    ) -> StoreResult<Vec<ProjectSummary>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT w.name AS workspace_name, \
                        p.name AS project_name, \
                        COUNT(pg.id) AS page_count, \
                        MAX(pg.updated_at) AS last_updated_us \
                 FROM workspaces w \
                 JOIN projects p ON p.workspace_id = w.id \
                 LEFT JOIN pages pg ON pg.project_id = p.id AND pg.is_latest = 1 \
                 WHERE (?1 IS NULL OR w.name = ?1) \
                 GROUP BY w.id, p.id \
                 ORDER BY last_updated_us DESC NULLS LAST",
            )?;
            let rows = stmt.query_map(params![workspace], |row| {
                let workspace_name: String = row.get(0)?;
                let project_name: String = row.get(1)?;
                let page_count: i64 = row.get(2)?;
                let last_updated_us: Option<i64> = row.get(3)?;
                Ok((workspace_name, project_name, page_count, last_updated_us))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (workspace_name, project_name, page_count, last_updated_us) = r?;
                let last_updated = last_updated_us
                    .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                    .map(|ts| ts.to_string());
                out.push(ProjectSummary {
                    workspace_name,
                    project_name,
                    page_count: u64::try_from(page_count).unwrap_or(0),
                    last_updated,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Return one `(workspace, project)` scope by id, with the names and
    /// `repo_path` its `_meta.md` manifest is written from. `None` when the
    /// pair has no row.
    ///
    /// The single-scope counterpart of [`list_all_scopes`]: the wiki
    /// materializes a manifest for one scope the first time it writes into
    /// it, on a path where enumerating every scope in the store would be an
    /// N+1 over the whole tree.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn scope_row_by_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Option<ScopeRow>> {
        // (ws_name, proj_name, repo_path) — the ids are already known.
        type RawScope = (String, String, Option<String>);
        let raw: Option<RawScope> = self
            .with_conn(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT w.name, p.name, p.repo_path \
                         FROM projects p JOIN workspaces w ON w.id = p.workspace_id \
                         WHERE p.id = ?1 AND p.workspace_id = ?2",
                        params![project_id.as_bytes(), workspace_id.as_bytes()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                Ok(row)
            })
            .await?;
        Ok(
            raw.map(|(workspace_name, project_name, repo_path)| ScopeRow {
                workspace_id,
                workspace_name,
                project_id,
                project_name,
                repo_path,
            }),
        )
    }

    /// Return every `(workspace, project)` scope with its ids, names and
    /// `repo_path` — the data needed to write each scope's self-describing
    /// `_meta.md` manifest. Unlike [`list_projects_with_stats`], this carries
    /// the surrogate ids (not just names), so a rebuild can key the wiki tree.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_all_scopes(&self) -> StoreResult<Vec<ScopeRow>> {
        // (ws_id, ws_name, proj_id, proj_name, repo_path) as raw SQL columns.
        type RawScope = (Vec<u8>, String, Vec<u8>, String, Option<String>);
        let raw: Vec<RawScope> = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT w.id, w.name, p.id, p.name, p.repo_path \
                     FROM projects p JOIN workspaces w ON w.id = p.workspace_id",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?;
                let out: rusqlite::Result<Vec<_>> = rows.collect();
                Ok(out?)
            })
            .await?;
        raw.into_iter()
            .map(|(wi, wn, pi, pn, rp)| {
                Ok(ScopeRow {
                    workspace_id: WorkspaceId::from_slice(&wi)?,
                    workspace_name: wn,
                    project_id: ProjectId::from_slice(&pi)?,
                    project_name: pn,
                    repo_path: rp,
                })
            })
            .collect()
    }

    /// Return every workspace with its id and name so manifest backfill can
    /// describe even empty workspaces that have no project rows yet.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_all_workspace_scopes(&self) -> StoreResult<Vec<WorkspaceScopeRow>> {
        let raw: Vec<(Vec<u8>, String)> = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT id, name FROM workspaces")?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                let out: rusqlite::Result<Vec<_>> = rows.collect();
                Ok(out?)
            })
            .await?;
        raw.into_iter()
            .map(|(wi, wn)| {
                Ok(WorkspaceScopeRow {
                    workspace_id: WorkspaceId::from_slice(&wi)?,
                    workspace_name: wn,
                })
            })
            .collect()
    }

    /// Return one row per workspace with project/page-count and
    /// last-updated aggregates. Used by custom frontends that need a
    /// workspace chooser before narrowing into projects.
    ///
    /// Only `is_latest = 1` pages are counted.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_workspaces_with_stats(&self) -> StoreResult<Vec<WorkspaceSummary>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT w.name AS workspace_name, \
                        COUNT(DISTINCT p.id) AS project_count, \
                        COUNT(pg.id) AS page_count, \
                        MAX(pg.updated_at) AS last_updated_us \
                 FROM workspaces w \
                 LEFT JOIN projects p ON p.workspace_id = w.id \
                 LEFT JOIN pages pg ON pg.project_id = p.id AND pg.is_latest = 1 \
                 GROUP BY w.id \
                 ORDER BY w.name ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                let workspace_name: String = row.get(0)?;
                let project_count: i64 = row.get(1)?;
                let page_count: i64 = row.get(2)?;
                let last_updated_us: Option<i64> = row.get(3)?;
                Ok((workspace_name, project_count, page_count, last_updated_us))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (workspace_name, project_count, page_count, last_updated_us) = r?;
                let last_updated = last_updated_us
                    .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                    .map(|ts| ts.to_string());
                out.push(WorkspaceSummary {
                    workspace_name,
                    project_count: u64::try_from(project_count).unwrap_or(0),
                    page_count: u64::try_from(page_count).unwrap_or(0),
                    last_updated,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Look up a workspace id by name without creating it.
    ///
    /// Returns `None` when no workspace with the given name exists.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_workspace(&self, name: String) -> StoreResult<Option<WorkspaceId>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT id FROM workspaces WHERE name = ?1",
                    params![name],
                    |row| {
                        let bytes: Vec<u8> = row.get(0)?;
                        Ok(bytes)
                    },
                )
                .optional()?;
            row_opt
                .map(|bytes| WorkspaceId::from_slice(&bytes).map_err(StoreError::from))
                .transpose()
        })
        .await
    }

    /// Look up a project id by `(workspace_id, name)` without creating it.
    ///
    /// Returns `None` when no project with the given name exists in the workspace.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_project(
        &self,
        workspace_id: WorkspaceId,
        name: String,
    ) -> StoreResult<Option<ProjectId>> {
        self.with_conn(move |conn| {
            let row_opt = conn
                .query_row(
                    "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
                    params![workspace_id.as_bytes(), name],
                    |row| {
                        let bytes: Vec<u8> = row.get(0)?;
                        Ok(bytes)
                    },
                )
                .optional()?;
            row_opt
                .map(|bytes| ProjectId::from_slice(&bytes).map_err(StoreError::from))
                .transpose()
        })
        .await
    }

    /// Find the existing project whose `repo_path` is the longest
    /// prefix of `cwd`, if any. Used by the hook router before
    /// auto-creating a new project from `basename(cwd)` so an event
    /// whose cwd is inside an existing project's tree resolves to
    /// that parent instead of materialising a fragment project for
    /// the subdirectory name.
    ///
    /// `ORDER BY length(repo_path) DESC` picks the most-specific
    /// match: if the operator declared a sub-project via
    /// `.sessionmunch.toml` (which writes its own row with a longer
    /// `repo_path`), the sub-project wins over its outer parent.
    ///
    /// **Boundary safety.** Every layer of this query has explicit
    /// guards so a path-shaped value can't accidentally widen the
    /// match set:
    ///
    /// - `workspace_id = ?1` — never matches a project in another
    ///   workspace.
    /// - `repo_path IS NOT NULL` plus Rust-side normalization rejects stored
    ///   values that would match too broadly (`NULL`, `''`, `/`) or fail the
    ///   `<repo_path>/%` boundary (trailing slash/backslash).
    /// - Stored `repo_path` wildcards (`%`, `_`) are compared as literal path
    ///   bytes in Rust, not as SQL `LIKE` wildcards.
    /// - A normalized stored `repo_path` equal to the operator's home
    ///   directory (`home`) is never matched.
    ///   Such a row would be a prefix catch-all for every project
    ///   beneath `$HOME`; the caller passes the server's own `$HOME`
    ///   (filesystem root `/` is already excluded by the
    ///   `length(repo_path) > 1` guard). A `None` `home` is a no-op.
    /// - Input canonicalisation — trailing slashes stripped; cwds
    ///   containing dot-segments (`/foo/../bar`, `/./x`) are rejected
    ///   outright so a traversal-style path can't match a parent it
    ///   doesn't logically belong to.
    ///
    /// Returns `None` (caller falls through to create-by-basename)
    /// for every defensive case so a bad input degrades the same way
    /// as "no match" rather than picking the wrong project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn find_project_by_cwd_prefix(
        &self,
        workspace_id: WorkspaceId,
        cwd: String,
        home: Option<&str>,
    ) -> StoreResult<Option<(ProjectId, String)>> {
        let home = home.map(|h| normalize_cwd(h).into_owned());
        let cwd_norm = normalize_cwd(&cwd).into_owned();
        if !is_safe_cwd_for_prefix_match(&cwd_norm) {
            return Ok(None);
        }
        self.with_conn(move |conn| {
            // Compare in Rust instead of SQL LIKE so stored `%`/`_` bytes stay
            // literal and legacy Windows rows with backslashes keep matching a
            // slash-normalized hook cwd. New writes normalize `repo_path`, but
            // this compatibility read path protects existing databases.
            let mut stmt = conn.prepare(
                "SELECT id, name, repo_path FROM projects \
                 WHERE workspace_id = ?1 AND repo_path IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![workspace_id.as_bytes()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut matches = Vec::new();
            for row in rows {
                let (bytes, name, repo_path) = row?;
                if has_trailing_path_separator(&repo_path) {
                    continue;
                }
                let repo_norm = normalize_cwd(&repo_path).into_owned();
                if repo_norm.len() <= 1
                    || !is_safe_cwd_for_prefix_match(&repo_norm)
                    || home.as_deref() == Some(repo_norm.as_str())
                {
                    continue;
                }
                if cwd_within(&repo_norm, &cwd_norm) {
                    matches.push((bytes, name, repo_norm.len()));
                }
            }
            matches.sort_by_key(|entry| std::cmp::Reverse(entry.2));
            matches
                .into_iter()
                .next()
                .map(|(bytes, name, _)| {
                    ProjectId::from_slice(&bytes)
                        .map(|id| (id, name))
                        .map_err(StoreError::from)
                })
                .transpose()
        })
        .await
    }
}
