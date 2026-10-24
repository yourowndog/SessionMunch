//! Status, storage, derived-index and audit reports.

use super::*;

impl ReaderPool {
    /// Return counts that must be zero before `sessionmunch reindex` runs.
    ///
    /// `reindex` is a rebuild-from-files operation, not an in-place repair of a
    /// dirty DB. If rows already exist, stale derived rows or DB-only episodic
    /// state could survive and contradict the "rebuilt from wiki" contract.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn reindex_target_status(&self) -> StoreResult<ReindexTargetStatus> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT \
                    (SELECT COUNT(*) FROM workspaces), \
                    (SELECT COUNT(*) FROM projects), \
                    (SELECT COUNT(*) FROM pages), \
                    (SELECT COUNT(*) FROM links), \
                    (SELECT COUNT(*) FROM page_embeddings), \
                    (SELECT COUNT(*) FROM sessions), \
                    (SELECT COUNT(*) FROM observations), \
                    (SELECT COUNT(*) FROM handoffs), \
                    (SELECT COUNT(*) FROM users), \
                    (SELECT COUNT(*) FROM audit_log)",
                [],
                |row| {
                    Ok(ReindexTargetStatus {
                        workspaces: row.get(0)?,
                        projects: row.get(1)?,
                        pages: row.get(2)?,
                        links: row.get(3)?,
                        page_embeddings: row.get(4)?,
                        sessions: row.get(5)?,
                        observations: row.get(6)?,
                        handoffs: row.get(7)?,
                        users: row.get(8)?,
                        audit_log: row.get(9)?,
                    })
                },
            )?)
        })
        .await
    }

    /// Structural cross-project contamination audit (cheap, SQL-only, no LLM).
    ///
    /// One HIGH-precision heuristic:
    /// - **CHECK A** (`session_wrong_bucket`): a session whose `cwd`
    ///   longest-prefix-resolves to a *different* project than the one it landed
    ///   in — the direct signature of the auto-scope bleed bug. Resolved with the
    ///   same prefix and cwd-safety rules as [`Self::find_project_by_cwd_prefix`],
    ///   so the audit never claims a session a live resolve would not.
    ///
    /// Note: an earlier "observation drifted from its session's project" check
    /// was removed. With per-event cwd resolution, an observation's `project_id`
    /// is set from the cwd *of that event* — so an agent that legitimately
    /// `cd`s across repos in one session produces observations whose project
    /// differs from the session's home project. That is correct attribution,
    /// not contamination, so the check flagged normal cross-repo work as a false
    /// positive (and observations carry no cwd of their own to disambiguate).
    /// CHECK A — anchored on the session's own cwd — remains the precise signal.
    ///
    /// Detects only contamination with a STRUCTURAL trace; purely semantic
    /// mislandings (topic-level, no cwd/session anomaly) are not detectable.
    /// `scope` restricts findings to the given landed `(workspace, project)`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn audit_contamination(
        &self,
        scope: Option<(WorkspaceId, ProjectId)>,
        home: Option<&str>,
    ) -> StoreResult<ContaminationReport> {
        let scoped = scope.is_some();
        let home = home.map(|h| normalize_cwd(h).into_owned());
        let scope_params: Vec<Value> = match &scope {
            Some((ws, proj)) => vec![
                Value::Blob(ws.as_bytes().to_vec()),
                Value::Blob(proj.as_bytes().to_vec()),
            ],
            None => Vec::new(),
        };

        // One connection: the candidate session list + the valid repo_path
        // prefixes used to resolve CHECK A. Loading prefixes once avoids a
        // reader query per historical session while preserving the same path
        // boundary and safety rules as the runtime resolver.
        type Candidate = (String, WorkspaceId, ProjectId, String, String, String);
        type Prefix = (WorkspaceId, ProjectId, String, String);
        let (mut findings, candidates, prefixes): (
            Vec<ContaminationFinding>,
            Vec<Candidate>,
            Vec<Prefix>,
        ) = self
            .with_conn(move |conn| {
                // CHECK A findings are built after this closure (they need the
                // repo_path prefixes resolved outside the DB connection); this
                // closure only gathers the raw inputs.
                let findings: Vec<ContaminationFinding> = Vec::new();

                // Candidate sessions (cwd present) — resolved outside the conn.
                let mut s_sql = String::from(
                    "SELECT lower(hex(s.id)), s.workspace_id, s.project_id, wl.name, pl.name, s.cwd \
                     FROM sessions s \
                     JOIN workspaces wl ON wl.id = s.workspace_id \
                     JOIN projects pl ON pl.id = s.project_id \
                     WHERE s.cwd IS NOT NULL",
                );
                if scoped {
                    s_sql.push_str(" AND s.workspace_id = ? AND s.project_id = ?");
                }
                let mut candidates: Vec<Candidate> = Vec::new();
                {
                    let mut stmt = conn.prepare(&s_sql)?;
                    let rows = stmt.query_map(params_from_iter(scope_params.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    })?;
                    for r in rows {
                        let (id, ws_b, proj_b, lws, lproj, cwd) = r?;
                        candidates.push((
                            id,
                            WorkspaceId::from_slice(&ws_b)?,
                            ProjectId::from_slice(&proj_b)?,
                            lws,
                            lproj,
                            cwd,
                        ));
                    }
                }

                let mut p_sql = String::from(
                    "SELECT workspace_id, id, name, repo_path \
                     FROM projects \
                     WHERE repo_path IS NOT NULL \
                       AND length(repo_path) > 1 \
                       AND repo_path NOT LIKE '%/'",
                );
                if scoped {
                    p_sql.push_str(" AND workspace_id = ?");
                }
                p_sql.push_str(" ORDER BY length(repo_path) DESC");
                let mut prefixes: Vec<Prefix> = Vec::new();
                {
                    let mut stmt = conn.prepare(&p_sql)?;
                    if scoped {
                        let rows = stmt.query_map(params_from_iter(scope_params.iter().take(1)), |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, Vec<u8>>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        })?;
                        for r in rows {
                            let (ws_b, proj_b, name, repo_path) = r?;
                            if has_trailing_path_separator(&repo_path) {
                                continue;
                            }
                            let repo_path = normalize_cwd(&repo_path).into_owned();
                            if repo_path.len() <= 1
                                || !is_safe_cwd_for_prefix_match(&repo_path)
                                || home.as_deref() == Some(repo_path.as_str())
                            {
                                continue;
                            }
                            prefixes.push((
                                WorkspaceId::from_slice(&ws_b)?,
                                ProjectId::from_slice(&proj_b)?,
                                name,
                                repo_path,
                            ));
                        }
                    } else {
                        let rows = stmt.query_map([], |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, Vec<u8>>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        })?;
                        for r in rows {
                            let (ws_b, proj_b, name, repo_path) = r?;
                            if has_trailing_path_separator(&repo_path) {
                                continue;
                            }
                            let repo_path = normalize_cwd(&repo_path).into_owned();
                            if repo_path.len() <= 1
                                || !is_safe_cwd_for_prefix_match(&repo_path)
                                || home.as_deref() == Some(repo_path.as_str())
                            {
                                continue;
                            }
                            prefixes.push((
                                WorkspaceId::from_slice(&ws_b)?,
                                ProjectId::from_slice(&proj_b)?,
                                name,
                                repo_path,
                            ));
                        }
                    }
                }

                Ok((findings, candidates, prefixes))
            })
            .await?;

        // CHECK A: resolve each session's cwd against preloaded valid prefixes
        // and flag a bucket mismatch.
        for (id, ws, landed_proj, landed_ws_name, landed_proj_name, cwd) in candidates {
            let cwd_norm = normalize_cwd(&cwd).into_owned();
            if !is_safe_cwd_for_prefix_match(&cwd_norm) {
                continue;
            }
            let resolved = prefixes.iter().find(|(prefix_ws, _, _, repo_path)| {
                *prefix_ws == ws && cwd_within(repo_path, &cwd_norm)
            });
            if let Some((_, resolved_proj, resolved_name, _)) = resolved
                && *resolved_proj != landed_proj
            {
                findings.push(ContaminationFinding {
                    check: "session_wrong_bucket",
                    confidence: "high",
                    entity_kind: "session",
                    entity_id: id,
                    landed_workspace: landed_ws_name,
                    landed_project: landed_proj_name,
                    expected_project: Some(resolved_name.clone()),
                    cwd: Some(cwd),
                });
            }
        }

        let summary = ContaminationSummary {
            sessions_misbucketed: findings
                .iter()
                .filter(|f| f.check == "session_wrong_bucket")
                .count(),
        };
        Ok(ContaminationReport { summary, findings })
    }

    /// List `audit_log` rows newest-first, resolving names through LEFT JOINs.
    ///
    /// Workspace/project filters match on **name** (the same convention as
    /// [`Self::list_pages`]). `before_id` is a keyset cursor (`id < ?` under
    /// `ORDER BY id DESC`): new events get higher ids, so they land on later
    /// first pages instead of shifting this window and duplicating or skipping
    /// rows already seen. `limit` is clamped to `1..=200`.
    ///
    /// `detail` is the schema column; the only writer currently stores the
    /// literal `{}`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_audit_events(&self, filter: AuditLogFilter) -> StoreResult<Vec<AuditEvent>> {
        let limit = filter.limit.clamp(1, 200);
        let workspace = filter.workspace.filter(|s| !s.is_empty());
        let project = filter.project.filter(|s| !s.is_empty());
        let op = filter.op.filter(|s| !s.is_empty());
        let before_id = filter.before_id;
        self.with_conn(move |conn| {
            let mut sql = String::from(
                "SELECT a.id, a.at, a.op, w.name, p.name, pg.path, u.username, a.detail \
                 FROM audit_log a \
                 LEFT JOIN workspaces w ON w.id = a.workspace_id \
                 LEFT JOIN projects p ON p.id = a.project_id \
                 LEFT JOIN pages pg ON pg.id = a.page_id \
                 LEFT JOIN users u ON u.id = a.author_id",
            );
            let mut binds: Vec<Value> = Vec::new();
            let mut clauses: Vec<&str> = Vec::new();
            if workspace.is_some() {
                clauses.push("w.name = ?");
            }
            if project.is_some() {
                clauses.push("p.name = ?");
            }
            if op.is_some() {
                clauses.push("a.op = ?");
            }
            if before_id.is_some() {
                clauses.push("a.id < ?");
            }
            if !clauses.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&clauses.join(" AND "));
            }
            sql.push_str(" ORDER BY a.id DESC LIMIT ?");
            if let Some(ws) = &workspace {
                binds.push(Value::Text(ws.clone()));
            }
            if let Some(proj) = &project {
                binds.push(Value::Text(proj.clone()));
            }
            if let Some(op) = &op {
                binds.push(Value::Text(op.clone()));
            }
            if let Some(before) = before_id {
                binds.push(Value::Integer(before));
            }
            binds.push(Value::Integer(i64::try_from(limit).unwrap_or(200)));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(binds.iter()), |row| {
                Ok(AuditEvent {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    op: row.get(2)?,
                    workspace: row.get(3)?,
                    project: row.get(4)?,
                    page_path: row.get(5)?,
                    author_username: row.get(6)?,
                    detail: row.get(7)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Return aggregate counts for the `status` view.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn status_counts(&self) -> StoreResult<StatusCounts> {
        self.with_conn(|conn| {
            let pages_latest: u64 = count(conn, "SELECT COUNT(*) FROM pages WHERE is_latest = 1")?;
            let pages_all: u64 = count(conn, "SELECT COUNT(*) FROM pages")?;
            let sessions: u64 = count(conn, "SELECT COUNT(*) FROM sessions")?;
            let observations: u64 = count(conn, "SELECT COUNT(*) FROM observations")?;
            Ok(StatusCounts {
                pages_latest,
                pages_all,
                sessions,
                observations,
            })
        })
        .await
    }

    /// Physical storage figures for the database file.
    ///
    /// Three `PRAGMA` reads, no table scan, so it is cheap enough to sit in
    /// `status` next to the row counts.
    ///
    /// # Errors
    /// Propagates the SQL error from the pragma reads.
    pub async fn storage_status(&self) -> StoreResult<StorageStatus> {
        // The database's own dir may not exist as a distinct mount point
        // (it usually doesn't), but `available_space` walks up to whatever
        // filesystem holds it either way. A read failure (unsupported
        // filesystem, transient error) is a signal we don't have, not a
        // reason to fail the whole status report.
        let data_dir_free_bytes = self
            .inner
            .db_path
            .parent()
            .and_then(|dir| fs2::available_space(dir).ok());
        self.with_conn(move |conn| {
            let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let freelist_count: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
            let page_size = u64::try_from(page_size.max(0)).unwrap_or(0);
            let page_count = u64::try_from(page_count.max(0)).unwrap_or(0);
            let freelist_count = u64::try_from(freelist_count.max(0)).unwrap_or(0);
            Ok(StorageStatus {
                page_size,
                page_count,
                freelist_count,
                database_bytes: page_count.saturating_mul(page_size),
                reclaimable_bytes: freelist_count.saturating_mul(page_size),
                data_dir_free_bytes,
            })
        })
        .await
    }

    /// Return health counters for derived indexes and link/embedding state.
    ///
    /// These checks are intentionally read-only and derived-index-safe: they
    /// report drift but do not repair it. Rebuild/backfill paths stay behind
    /// explicit admin operations.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn derived_index_status(&self) -> StoreResult<DerivedIndexStatus> {
        self.with_conn(|conn| {
            let mut triples_stmt = conn.prepare(
                "SELECT provider, model, dim, COUNT(*) \
                 FROM page_embeddings \
                 GROUP BY provider, model, dim \
                 ORDER BY COUNT(*) DESC, provider, model, dim",
            )?;
            let embedding_triples = triples_stmt
                .query_map([], |row| {
                    let provider: String = row.get(0)?;
                    let model: String = row.get(1)?;
                    let dim: i64 = row.get(2)?;
                    let count: i64 = row.get(3)?;
                    Ok(EmbeddingTripleCount {
                        provider,
                        model,
                        dim: u32::try_from(dim.max(0)).unwrap_or(0),
                        count: u64::try_from(count.max(0)).unwrap_or(0),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(DerivedIndexStatus {
                pages_rows: count(conn, "SELECT COUNT(*) FROM pages")?,
                // `pages_fts` is an external-content FTS5 table, so
                // `COUNT(*)` on it is answered from `pages` and can never
                // diverge. The `_docsize` shadow table holds one row per
                // indexed document, which is what makes this pair a drift
                // check rather than a tautology. Same for `observations_fts`.
                pages_fts_rows: count(conn, "SELECT COUNT(*) FROM pages_fts_docsize")?,
                observations_rows: count(conn, "SELECT COUNT(*) FROM observations")?,
                observations_fts_rows: count(
                    conn,
                    "SELECT COUNT(*) FROM observations_fts_docsize",
                )?,
                // Split by whether an embedding exists *now*: a failure row is
                // the last unsuccessful attempt, not proof the page is still
                // broken. Without this join a page that failed once and
                // recovered would read as an outstanding problem forever.
                embed_failures_unresolved: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM page_embed_failures f \
                     JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                     LEFT JOIN page_embeddings pe ON pe.page_id = f.page_id \
                     WHERE pe.page_id IS NULL",
                )?,
                embed_failures_recovered: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM page_embed_failures f \
                     JOIN pages pg ON pg.id = f.page_id AND pg.is_latest = 1 \
                     JOIN page_embeddings pe ON pe.page_id = f.page_id",
                )?,
                // Aligned with the backfill's own skip rule: an
                // empty-body page can never be embedded, so counting it
                // as \"missing\" overstated the actionable number forever
                // (observed live: a stable 427 that no backfill could
                // ever clear). Unembeddable pages are reported apart.
                latest_pages_missing_embeddings: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM pages pg \
                     LEFT JOIN page_embeddings pe ON pe.page_id = pg.id \
                     WHERE pg.is_latest = 1 AND pe.page_id IS NULL \
                       AND TRIM(pg.body) != ''",
                )?,
                latest_pages_unembeddable: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM pages pg \
                     LEFT JOIN page_embeddings pe ON pe.page_id = pg.id \
                     WHERE pg.is_latest = 1 AND pe.page_id IS NULL \
                       AND TRIM(pg.body) = ''",
                )?,
                embedding_rows: count(conn, "SELECT COUNT(*) FROM page_embeddings")?,
                embedding_triples,
                typed_links_from_latest_pages: {
                    let mut stmt = conn.prepare(
                        "SELECT l.link_type, COUNT(*) FROM links l \
                         JOIN pages fp ON fp.id = l.from_page_id AND fp.is_latest = 1 \
                         WHERE l.link_type != 'references' \
                         GROUP BY l.link_type ORDER BY l.link_type",
                    )?;
                    let rows: Vec<(String, i64)> = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<Result<_, _>>()?;
                    rows.into_iter()
                        .map(|(k, v)| (k, u64::try_from(v).unwrap_or(0)))
                        .collect()
                },
                links_from_latest_pages: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM links l \
                     JOIN pages fp ON fp.id = l.from_page_id \
                     WHERE fp.is_latest = 1",
                )?,
                unresolved_links_from_latest_pages: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM links l \
                     JOIN pages fp ON fp.id = l.from_page_id \
                     WHERE fp.is_latest = 1 AND l.to_page_id IS NULL",
                )?,
                stale_links_from_latest_pages: count(
                    conn,
                    "SELECT COUNT(*) \
                     FROM links l \
                     JOIN pages fp ON fp.id = l.from_page_id \
                     LEFT JOIN pages tp ON tp.id = l.to_page_id \
                     WHERE fp.is_latest = 1 \
                       AND l.to_page_id IS NOT NULL \
                       AND COALESCE(tp.is_latest, 0) != 1",
                )?,
            })
        })
        .await
    }

    /// Return aggregate counts scoped to one project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn status_counts_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<StatusCounts> {
        self.with_conn(move |conn| {
            let pages_latest = count_project(
                conn,
                "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
                workspace_id,
                project_id,
            )?;
            let pages_all = count_project(
                conn,
                "SELECT COUNT(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2",
                workspace_id,
                project_id,
            )?;
            let sessions = count_project(
                conn,
                "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1 AND project_id = ?2",
                workspace_id,
                project_id,
            )?;
            let observations = count_project(
                conn,
                "SELECT COUNT(*) FROM observations WHERE workspace_id = ?1 AND project_id = ?2",
                workspace_id,
                project_id,
            )?;
            Ok(StatusCounts {
                pages_latest,
                pages_all,
                sessions,
                observations,
            })
        })
        .await
    }

    /// Return all migration names recorded in the `wiki_migrations` table.
    ///
    /// Used by the wiki migration runner to determine which migrations have
    /// already been applied to this data directory.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn wiki_migration_names(&self) -> StoreResult<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT name FROM wiki_migrations ORDER BY name")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names)
        })
        .await
    }

    // ── user lookups ────────────────────────────────────────────────
}
