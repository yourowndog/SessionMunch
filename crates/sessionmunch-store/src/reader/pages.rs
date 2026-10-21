//! Page metadata, bodies, links and cross-project edges.

use super::*;

impl ReaderPool {
    /// Look up a page's workspace and project names by page id.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_meta_by_id(&self, page_id: PageId) -> StoreResult<Option<PageMeta>> {
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let sql = format!(
                "SELECT w.name, p.name, w.id, p.id, pg.path, pg.title, \
                        {kind_expr}, \
                        pg.tier, pg.pinned, pg.created_at, pg.updated_at, \
                        sp.path AS supersedes_path, \
                        au.username, au.name, au.email, pg.expires_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 LEFT JOIN pages sp ON sp.id = pg.supersedes \
                 LEFT JOIN users au ON au.id = pg.author_id \
                 WHERE pg.id = ?1 AND pg.is_latest = 1"
            );
            let row_opt = conn
                .query_row(&sql, params![page_id.as_bytes()], page_meta_from_row)
                .optional()?;
            row_opt.transpose()
        })
        .await
    }

    /// Look up a page's workspace and project names by its path (across all
    /// workspaces and projects). Returns the first `is_latest = 1` match.
    ///
    /// Used by the web search handler to resolve workspace/project for a hit
    /// without a per-hit SQL join.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_meta_by_path(&self, path: &str) -> StoreResult<Option<PageMeta>> {
        let path = path.to_owned();
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let sql = format!(
                "SELECT w.name, p.name, w.id, p.id, pg.path, pg.title, \
                        {kind_expr}, \
                        pg.tier, pg.pinned, pg.created_at, pg.updated_at, \
                        sp.path AS supersedes_path, \
                        au.username, au.name, au.email, pg.expires_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 LEFT JOIN pages sp ON sp.id = pg.supersedes \
                 LEFT JOIN users au ON au.id = pg.author_id \
                 WHERE pg.path = ?1 AND pg.is_latest = 1 \
                 LIMIT 1"
            );
            let row_opt = conn
                .query_row(&sql, params![path], page_meta_from_row)
                .optional()?;
            row_opt.transpose()
        })
        .await
    }

    /// Resolve the outgoing links and incoming back-links for the latest
    /// version of a page identified by `(workspace_id, project_id, path)`.
    ///
    /// Both ends are constrained to `is_latest = 1`, so superseded versions
    /// never leak into the link panel. Returns empty lists when the page is
    /// missing or has no links.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_links(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: String,
    ) -> StoreResult<PageLinks> {
        self.with_conn(move |conn| {
            let id_opt: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT id FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(id_bytes) = id_opt else {
                return Ok(PageLinks::default());
            };

            // Outgoing: latest pages this page links to. Incoming: latest
            // pages that link here. Both reuse the path-inference `kind`
            // fallback so untagged pages still classify.
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let outgoing = format!(
                "SELECT DISTINCT pg.path, pg.title, {kind_expr}, \
                            ws.name, pr.name \
                     FROM links l \
                     JOIN pages pg ON pg.id = l.to_page_id \
                     JOIN projects pr ON pr.id = pg.project_id \
                     JOIN workspaces ws ON ws.id = pg.workspace_id \
                     WHERE l.from_page_id = ?1 AND pg.is_latest = 1 \
                     ORDER BY ws.name, pr.name, pg.path"
            );
            let incoming = format!(
                "SELECT DISTINCT pg.path, pg.title, {kind_expr}, \
                            ws.name, pr.name \
                     FROM links l \
                     JOIN pages pg ON pg.id = l.from_page_id \
                     JOIN projects pr ON pr.id = pg.project_id \
                     JOIN workspaces ws ON ws.id = pg.workspace_id \
                     WHERE l.to_page_id = ?1 AND pg.is_latest = 1 \
                     ORDER BY ws.name, pr.name, pg.path"
            );

            let collect = |sql: &str| -> StoreResult<Vec<RelatedPage>> {
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt.query_map(params![id_bytes], |row| {
                    Ok(RelatedPage {
                        path: row.get(0)?,
                        title: row.get(1)?,
                        kind: row.get(2)?,
                        workspace: row.get(3)?,
                        project: row.get(4)?,
                    })
                })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            };

            Ok(PageLinks {
                links: collect(&outgoing)?,
                backlinks: collect(&incoming)?,
            })
        })
        .await
    }

    /// List unresolved cross-project links authored by pages in this project
    /// — declared dependencies on another project's page that don't resolve.
    /// Each row says whether the named target project exists, so the lint can
    /// tell a typo'd project name from a missing / renamed target page.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn dangling_cross_project_links(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<DanglingCrossLink>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT fp.path, l.to_workspace, l.to_project, l.to_path, \
                        EXISTS ( \
                            SELECT 1 FROM projects pr \
                            JOIN workspaces ws ON ws.id = pr.workspace_id \
                            WHERE pr.name = l.to_project \
                              AND ws.name = COALESCE( \
                                  l.to_workspace, \
                                  (SELECT name FROM workspaces WHERE id = ?1) \
                              ) \
                        ) AS project_exists \
                 FROM links l \
                 JOIN pages fp ON fp.id = l.from_page_id \
                     AND fp.workspace_id = ?1 AND fp.project_id = ?2 AND fp.is_latest = 1 \
                 WHERE l.to_page_id IS NULL AND l.to_project IS NOT NULL \
                 ORDER BY fp.path, l.to_project, l.to_path",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let exists: i64 = row.get(4)?;
                    Ok(DanglingCrossLink {
                        from_path: row.get(0)?,
                        workspace: row.get(1)?,
                        project: row.get(2)?,
                        path: row.get(3)?,
                        project_exists: exists != 0,
                    })
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Typed `contradicts` edges declared by latest pages of one project
    /// (docs/okf.md relations vocabulary). Each row feeds one rule-based
    /// lint finding.
    ///
    /// # Errors
    /// Propagates SQL errors from the read pool.
    pub async fn contradiction_edges(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<ContradictionEdge>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT fp.path, l.to_path,                         l.to_page_id IS NOT NULL AS resolved                  FROM links l                  JOIN pages fp ON fp.id = l.from_page_id                      AND fp.workspace_id = ?1 AND fp.project_id = ?2 AND fp.is_latest = 1                  WHERE l.link_type = 'contradicts'                  ORDER BY fp.path, l.to_path",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let resolved: i64 = row.get(2)?;
                    Ok(ContradictionEdge {
                        from_path: row.get(0)?,
                        to_path: row.get(1)?,
                        resolved: resolved != 0,
                    })
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Resolved cross-project edges (links whose endpoints are in different
    /// projects). When `scope` is `Some((ws, proj))`, only edges that touch
    /// that project (as source or target) are returned; `None` returns the
    /// whole cross-project graph. Powers `/api/v1/graph`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn cross_project_edges(
        &self,
        scope: Option<(WorkspaceId, ProjectId)>,
    ) -> StoreResult<Vec<CrossProjectEdge>> {
        self.with_conn(move |conn| {
            let base = "SELECT fw.name, fpr.name, fp.path, tw.name, tpr.name, tp.path \
                 FROM links l \
                 JOIN pages fp ON fp.id = l.from_page_id AND fp.is_latest = 1 \
                 JOIN pages tp ON tp.id = l.to_page_id AND tp.is_latest = 1 \
                 JOIN projects fpr ON fpr.id = fp.project_id \
                 JOIN workspaces fw ON fw.id = fp.workspace_id \
                 JOIN projects tpr ON tpr.id = tp.project_id \
                 JOIN workspaces tw ON tw.id = tp.workspace_id \
                 WHERE fp.project_id != tp.project_id";
            let map_row = |row: &rusqlite::Row<'_>| {
                Ok(CrossProjectEdge {
                    from_workspace: row.get(0)?,
                    from_project: row.get(1)?,
                    from_path: row.get(2)?,
                    to_workspace: row.get(3)?,
                    to_project: row.get(4)?,
                    to_path: row.get(5)?,
                })
            };
            let mut out = Vec::new();
            if let Some((_ws, proj)) = scope {
                let sql =
                    format!("{base} AND (fp.project_id = ?1 OR tp.project_id = ?1) ORDER BY fw.name, fpr.name, fp.path");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![proj.as_bytes()], map_row)?;
                for r in rows {
                    out.push(r?);
                }
            } else {
                let sql = format!("{base} ORDER BY fw.name, fpr.name, fp.path");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], map_row)?;
                for r in rows {
                    out.push(r?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// All `is_latest = 1` pages under a given (workspace, project),
    /// ordered by path ascending. Used by the web UI tree view.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn list_pages(
        &self,
        workspace: &str,
        project: &str,
    ) -> StoreResult<Vec<PageSummary>> {
        let workspace = workspace.to_owned();
        let project = project.to_owned();
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let mut stmt = conn.prepare(&format!(
                "SELECT pg.path, pg.title, {kind_expr} AS kind, \
                        pg.tier, pg.updated_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 WHERE w.name = ?1 AND p.name = ?2 AND pg.is_latest = 1 \
                 ORDER BY pg.path ASC"
            ))?;
            let rows = stmt.query_map(params![workspace, project], |row| {
                let path: String = row.get(0)?;
                let title: String = row.get(1)?;
                let kind: String = row.get(2)?;
                let tier: String = row.get(3)?;
                let updated_us: i64 = row.get(4)?;
                Ok((path, title, kind, tier, updated_us))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (path, title, kind, tier, updated_us) = r?;
                let updated_at = jiff::Timestamp::from_microsecond(updated_us)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default();
                out.push(PageSummary {
                    path,
                    title,
                    kind,
                    tier,
                    updated_at,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Full page metadata for the page-view template (body comes from
    /// `Wiki::read_page`). Returns `None` when no `is_latest = 1` row
    /// matches the given path.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_meta(
        &self,
        workspace: &str,
        project: &str,
        page_path: &str,
    ) -> StoreResult<Option<PageMeta>> {
        let workspace = workspace.to_owned();
        let project = project.to_owned();
        let page_path = page_path.to_owned();
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pg.path", "pg.frontmatter_json");
            let sql = format!(
                "SELECT w.name, p.name, w.id, p.id, pg.path, pg.title, \
                        {kind_expr}, \
                        pg.tier, pg.pinned, pg.created_at, pg.updated_at, \
                        sp.path AS supersedes_path, \
                        au.username, au.name, au.email, pg.expires_at \
                 FROM pages pg \
                 JOIN projects p ON p.id = pg.project_id \
                 JOIN workspaces w ON w.id = pg.workspace_id \
                 LEFT JOIN pages sp ON sp.id = pg.supersedes \
                 LEFT JOIN users au ON au.id = pg.author_id \
                 WHERE w.name = ?1 AND p.name = ?2 AND pg.path = ?3 AND pg.is_latest = 1"
            );
            let row_opt = conn
                .query_row(
                    &sql,
                    params![workspace, project, page_path],
                    page_meta_from_row,
                )
                .optional()?;
            row_opt.transpose()
        })
        .await
    }

    /// Fetch the latest version's stored body/title/frontmatter for a page by
    /// `(workspace_id, project_id, path)`. Used as a DB-backed fallback for
    /// `memory_read_page` when the on-disk markdown read fails (index/disk
    /// skew — see `gotchas/read-page-by-query-misses`). Returns `None` when no
    /// `is_latest` row exists.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_body_by_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &str,
    ) -> StoreResult<Option<StoredPageBody>> {
        let path = path.to_owned();
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT title, body, frontmatter_json, tier, pinned FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| {
                        Ok(StoredPageBody {
                            title: row.get(0)?,
                            body: row.get(1)?,
                            frontmatter_json: row.get(2)?,
                            tier: row.get(3)?,
                            pinned: row.get::<_, i64>(4)? != 0,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    /// Return the latest page version id for one fully scoped path.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn latest_page_id_by_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: String,
    ) -> StoreResult<Option<PageId>> {
        self.with_conn(move |conn| {
            let id: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT id FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id.map(|bytes| PageId::from_slice(&bytes)).transpose()?)
        })
        .await
    }

    /// Return whether the latest version of one fully scoped page is expired.
    /// Returns `None` when the path has no latest indexed row.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn page_expired_by_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &str,
    ) -> StoreResult<Option<bool>> {
        let path = path.to_owned();
        self.with_conn(move |conn| {
            let expires_at: Option<Option<i64>> = conn
                .query_row(
                    "SELECT expires_at FROM pages \
                     WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 \
                       AND is_latest = 1",
                    params![workspace_id.as_bytes(), project_id.as_bytes(), path],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(expires_at.map(|expires_at| expires_at.is_some_and(|value| value <= now_us())))
        })
        .await
    }
}
