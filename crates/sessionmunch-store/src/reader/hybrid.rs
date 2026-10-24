//! Hybrid retrieval: entity and graph signals, page authority, and RRF fusion behind `memory_query`.

use super::*;

impl ReaderPool {
    /// Rank pages by how many of the query's tokens match their indexed
    /// entities, weighting each match by inverse entity frequency — a
    /// query token matching an entity that appears on 2 pages is a much
    /// stronger signal than one matching an entity on 200.
    ///
    /// Matching is lexical (exact name, name prefix, or a compound-word
    /// prefix) so this stays a plain SQL stream with no LLM call. It returns
    /// no candidates when the project has no entities indexed, as in the
    /// common zero-LLM case without hand-edited entity frontmatter.
    ///
    /// `expiry_cutoff_us`: see [`Self::search_pages_for_project`].
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub(crate) async fn entity_hits_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: &str,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<EntityHit>> {
        self.entity_hits_for_project_at(
            workspace_id,
            project_id,
            query,
            limit,
            expiry_cutoff_us,
            None,
        )
        .await
    }

    /// Entity hits with an optional ingestion-time instant
    /// (docs/temporal.md). `as_of_us: None` = current knowledge (latest
    /// versions, expiry honoured). `Some(T)` = the page versions whose
    /// entity-link windows contain `T` — expiry is deliberately ignored
    /// there: a page valid at `T` that has since expired was still what
    /// we knew at `T`.
    pub async fn entity_hits_for_project_at(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: &str,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
        as_of_us: Option<i64>,
    ) -> StoreResult<Vec<EntityHit>> {
        let tokens = entity_query_tokens(query);
        if tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let mut placeholders = String::with_capacity(tokens.len() * 96);
            let mut sql_params: Vec<Value> = Vec::with_capacity(tokens.len() * 5 + 7);
            for (idx, token) in tokens.iter().enumerate() {
                if idx > 0 {
                    placeholders.push_str(" OR ");
                }
                // Match exact names, name prefixes, and word prefixes after
                // any accepted entity separator. The latter lets `actor`
                // find `writer actor`, `writer-actor`, and `writer_actor`.
                //
                // `?` is positional-by-order, same as the graph query.
                placeholders.push_str(
                    "(e.name = ? \
                      OR e.name LIKE ? || '%' ESCAPE '\\' \
                      OR e.name LIKE '% ' || ? || '%' ESCAPE '\\' \
                      OR e.name LIKE '%-' || ? || '%' ESCAPE '\\' \
                      OR e.name LIKE '%\\_' || ? || '%' ESCAPE '\\')",
                );
                sql_params.push(Value::Text(token.clone()));
                let escaped = like_escape(token);
                for _ in 0..4 {
                    sql_params.push(Value::Text(escaped.clone()));
                }
            }
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            match as_of_us {
                Some(t) => {
                    // freq window + outer window: two params each.
                    sql_params.push(Value::Integer(t));
                    sql_params.push(Value::Integer(t));
                    sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
                    sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
                    sql_params.push(Value::Integer(t));
                    sql_params.push(Value::Integer(t));
                }
                None => {
                    sql_params.push(Value::Integer(cutoff));
                    sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
                    sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
                    sql_params.push(Value::Integer(cutoff));
                }
            }
            sql_params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));

            let mut sql = String::with_capacity(placeholders.len() + 1_200);
            write!(
                &mut sql,
                // `freq` is how many current pages in this project carry
                // the entity; 1/freq is the inverse-frequency weight.
                "WITH matched AS ( \
                   SELECT e.id AS entity_id, e.name AS name \
                   FROM entities e \
                   WHERE ({placeholders}) \
                     AND e.workspace_id = ? AND e.project_id = ? \
                 ), \
                 freq AS ( \
                   SELECT m.entity_id, m.name, COUNT(*) AS pages \
                   FROM matched m \
                   JOIN entity_page_links l ON l.entity_id = m.entity_id \
                   JOIN pages p ON p.id = l.page_id \
                   WHERE 1 = 1{freq_version_filter} \
                   GROUP BY m.entity_id, m.name \
                 ) \
                 SELECT pg.id, pg.path, pg.title, {descriptor} AS snippet, \
                        SUM(1.0 / f.pages) AS weight, \
                        COUNT(*) AS matches, \
                        json_group_array(f.name) AS names \
                 FROM freq f \
                 JOIN entity_page_links l ON l.entity_id = f.entity_id \
                 JOIN pages pg ON pg.id = l.page_id \
                 WHERE pg.workspace_id = ? AND pg.project_id = ?{version_filter} \
                 GROUP BY pg.id, pg.path, pg.title \
                 ORDER BY weight DESC, matches DESC, pg.path ASC \
                 LIMIT ?",
                descriptor = page_descriptor_expr("pg.body", "pg.frontmatter_json"),
                version_filter = if as_of_us.is_some() {
                    // Window containment (docs/temporal.md); no expiry.
                    " AND l.valid_from <= ? \
                      AND (l.superseded_at IS NULL OR l.superseded_at > ?)"
                        .to_string()
                } else {
                    format!(" AND pg.is_latest = 1{}", not_expired("pg", "?"))
                },
                freq_version_filter = if as_of_us.is_some() {
                    " AND l.valid_from <= ? \
                      AND (l.superseded_at IS NULL OR l.superseded_at > ?)"
                        .to_string()
                } else {
                    format!(" AND p.is_latest = 1{}", not_expired("p", "?"))
                },
            )
            .expect("writing SQL into String cannot fail");

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let title: String = row.get(2)?;
                let snippet = page_descriptor(&row.get::<_, String>(3)?, &title);
                let weight: f64 = row.get(4)?;
                let names_json: String = row.get(6)?;
                Ok((id_bytes, path, title, snippet, weight, names_json))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, weight, names_json) = row?;
                let mut matched: Vec<String> = serde_json::from_str(&names_json)?;
                matched.sort_unstable();
                matched.dedup();
                out.push(EntityHit {
                    hit: PageHit {
                        id: PageId::from_slice(&id_bytes)?,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank: 0.0,
                    },
                    weight,
                    matched,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Pages linked to or from the seed pages, scoped to latest pages in the
    /// same project, plus provenance: which seed produced each neighbour
    /// (`seed_ord` indexes into the caller's `seed_ids`) and in which
    /// direction the link points. Feeds the `memory_query` explain surface.
    pub(super) async fn graph_neighbors_for_project_explained(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        seed_ids: Vec<PageId>,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<GraphNeighbor>> {
        if seed_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();

            let mut values_clause = String::with_capacity(seed_ids.len() * 8);
            let mut sql_params = Vec::with_capacity(seed_ids.len() * 2 + 6);
            for (idx, seed_id) in seed_ids.iter().enumerate() {
                if idx > 0 {
                    values_clause.push_str(", ");
                }
                values_clause.push_str("(?, ?)");
                sql_params.push(Value::Blob(seed_id.as_bytes().to_vec()));
                sql_params.push(Value::Integer(idx as i64));
            }
            let now = cutoff;
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            sql_params.push(Value::Integer(now));
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));
            sql_params.push(Value::Integer(now));

            let out_descriptor = page_descriptor_expr("tp.body", "tp.frontmatter_json");
            let in_descriptor = page_descriptor_expr("fp.body", "fp.frontmatter_json");
            let out_not_expired = not_expired("tp", "?");
            let in_not_expired = not_expired("fp", "?");
            let mut sql = String::with_capacity(values_clause.len() + 1_500);
            write!(
                &mut sql,
                "WITH seeds(seed_id, seed_ord) AS (VALUES {values_clause}), \
                 neighbors AS ( \
                   SELECT tp.id AS id, tp.path AS path, tp.title AS title, \
                          {out_descriptor} AS snippet, \
                          seeds.seed_ord * 2 AS stream_ord, tp.updated_at AS updated_at, \
                          l.link_type AS link_type \
                   FROM seeds \
                   JOIN links l ON l.from_page_id = seeds.seed_id \
                   JOIN pages tp ON tp.id = l.to_page_id \
                   WHERE tp.workspace_id = ? AND tp.project_id = ? AND tp.is_latest = 1{out_not_expired} \
                   UNION ALL \
                   SELECT fp.id AS id, fp.path AS path, fp.title AS title, \
                          {in_descriptor} AS snippet, \
                          seeds.seed_ord * 2 + 1 AS stream_ord, fp.updated_at AS updated_at, \
                          l.link_type AS link_type \
                   FROM seeds \
                   JOIN links l ON l.to_page_id = seeds.seed_id \
                   JOIN pages fp ON fp.id = l.from_page_id \
                   WHERE fp.workspace_id = ? AND fp.project_id = ? AND fp.is_latest = 1{in_not_expired} \
                 ) \
                 SELECT id, path, title, snippet, stream_ord, link_type \
                 FROM neighbors \
                 WHERE NOT EXISTS (SELECT 1 FROM seeds s WHERE s.seed_id = neighbors.id) \
                 ORDER BY stream_ord ASC, updated_at DESC, path ASC"
            )
            .expect("writing SQL into String cannot fail");

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let title: String = row.get(2)?;
                let snippet = page_descriptor(&row.get::<_, String>(3)?, &title);
                let stream_ord: i64 = row.get(4)?;
                let link_type: String = row.get(5)?;
                Ok((id_bytes, path, title, snippet, stream_ord, link_type))
            })?;

            for row in rows {
                let (id_bytes, path, title, snippet, stream_ord, link_type) = row?;
                let id = PageId::from_slice(&id_bytes)?;
                if !seen.insert(id) {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let seed_ord = (stream_ord / 2).max(0) as usize;
                out.push(GraphNeighbor {
                    hit: PageHit {
                        id,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank: 0.0,
                    },
                    seed_ord,
                    incoming: stream_ord % 2 == 1,
                    edge: (link_type != "references").then_some(link_type),
                });
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    pub(super) async fn page_descriptors_for_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        page_ids: Vec<PageId>,
    ) -> StoreResult<std::collections::HashMap<PageId, (String, String)>> {
        if page_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.with_conn(move |conn| {
            let mut values_clause = String::with_capacity(page_ids.len() * 5);
            let mut sql_params = Vec::with_capacity(page_ids.len() + 2);
            for (idx, page_id) in page_ids.iter().enumerate() {
                if idx > 0 {
                    values_clause.push_str(", ");
                }
                values_clause.push_str("(?)");
                sql_params.push(Value::Blob(page_id.as_bytes().to_vec()));
            }
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));

            let sql = format!(
                "WITH requested(id) AS (VALUES {values_clause}) \
                 SELECT pages.id, pages.title, {descriptor} AS descriptor \
                 FROM requested \
                 JOIN pages ON pages.id = requested.id \
                 WHERE pages.workspace_id = ? \
                   AND pages.project_id = ? \
                   AND pages.is_latest = 1",
                descriptor = page_descriptor_expr("pages.body", "pages.frontmatter_json"),
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let title: String = row.get(1)?;
                let descriptor: String = row.get(2)?;
                Ok((id_bytes, title, descriptor))
            })?;

            let mut out = std::collections::HashMap::new();
            for row in rows {
                let (id_bytes, title, descriptor) = row?;
                if let Ok(id) = PageId::from_slice(&id_bytes) {
                    out.insert(id, (title, descriptor));
                }
            }
            Ok(out)
        })
        .await
    }

    pub(super) async fn page_authorities_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        page_ids: Vec<PageId>,
    ) -> StoreResult<std::collections::HashMap<PageId, PageAuthority>> {
        self.page_authorities_for_ids(workspace_id, project_id, page_ids, true)
            .await
    }

    /// Authority inputs for specific page versions, without the
    /// latest-only filter: audit mode ranks superseded versions too
    /// (issue #656).
    pub(super) async fn page_authorities_for_versions(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        page_ids: Vec<PageId>,
    ) -> StoreResult<std::collections::HashMap<PageId, PageAuthority>> {
        self.page_authorities_for_ids(workspace_id, project_id, page_ids, false)
            .await
    }

    pub(super) async fn page_authorities_for_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        page_ids: Vec<PageId>,
        latest_only: bool,
    ) -> StoreResult<std::collections::HashMap<PageId, PageAuthority>> {
        if page_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.with_conn(move |conn| {
            let mut values_clause = String::with_capacity(page_ids.len() * 5);
            let mut sql_params = Vec::with_capacity(page_ids.len() + 2);
            for (idx, page_id) in page_ids.iter().enumerate() {
                if idx > 0 {
                    values_clause.push_str(", ");
                }
                values_clause.push_str("(?)");
                sql_params.push(Value::Blob(page_id.as_bytes().to_vec()));
            }
            sql_params.push(Value::Blob(workspace_id.as_bytes().to_vec()));
            sql_params.push(Value::Blob(project_id.as_bytes().to_vec()));

            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let latest_filter = if latest_only {
                "AND pages.is_latest = 1"
            } else {
                ""
            };
            let sql = format!(
                "WITH requested(id) AS (VALUES {values_clause}) \
                 SELECT pages.id, pages.path, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM requested \
                 JOIN pages ON pages.id = requested.id \
                 WHERE pages.workspace_id = ? \
                   AND pages.project_id = ? \
                   {latest_filter}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let tier: String = row.get(2)?;
                let pinned = row.get::<_, i64>(3)? != 0;
                let frontmatter_json: String = row.get(4)?;
                let kind: String = row.get(5)?;
                Ok((id_bytes, path, tier, pinned, frontmatter_json, kind))
            })?;

            let mut authorities = std::collections::HashMap::new();
            for row in rows {
                let (id_bytes, path, tier, pinned, frontmatter_json, kind) = row?;
                authorities.insert(
                    PageId::from_slice(&id_bytes)?,
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json),
                );
            }
            Ok(authorities)
        })
        .await
    }

    /// Hybrid search: RRF-fuse FTS5 results with cosine-similarity
    /// over the stored embeddings of the matching `(provider, model,
    /// dim)`, entity matches, and link-neighbour expansion — four RRF
    /// streams. Applies the bounded page-authority adjustment after
    /// fusion and returns the top-`limit` pages by adjusted fused score.
    ///
    /// Each stream degrades independently to contributing nothing: no
    /// `query_vec` skips the vector stream, an empty `entities` table
    /// skips the entity stream, and graph expansion still runs from
    /// whatever seeds the other streams produced.
    ///
    /// `expiry_cutoff_us`: see [`Self::search_pages_for_project`]. The
    /// cutoff resolves once here so all streams agree on it.
    ///
    /// k=60 is the canonical RRF constant.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        query_vec: Option<Vec<f32>>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<PageHit>> {
        Ok(self
            .hybrid_search_inner(
                workspace_id,
                project_id,
                query,
                query_vec,
                provider,
                model,
                dim,
                limit,
                expiry_cutoff_us,
                false,
            )
            .await?
            .into_iter()
            .map(|(hit, _)| hit)
            .collect())
    }

    /// [`Self::hybrid_search`] plus a [`SearchExplain`] per hit — the
    /// per-stream ranks, raw scores, and RRF contributions the fusion
    /// otherwise discards. Callers that don't need the bounded provenance
    /// bookkeeping use `hybrid_search`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search_explained(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        query_vec: Option<Vec<f32>>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<(PageHit, SearchExplain)>> {
        let hits = self
            .hybrid_search_inner(
                workspace_id,
                project_id,
                query,
                query_vec,
                provider,
                model,
                dim,
                limit,
                expiry_cutoff_us,
                true,
            )
            .await?;
        // Evidence counts (P2, docs/design-hindsight-borrowings.md §3) are
        // explain-only: one batch query over the already-fused result page
        // ids, never a per-hit query on the hot default path.
        let page_ids: Vec<PageId> = hits.iter().map(|(hit, _)| hit.id).collect();
        let counts = self.page_evidence_counts(&page_ids).await?;
        Ok(hits
            .into_iter()
            .map(|(hit, explain)| {
                let mut explain = explain.unwrap_or_default();
                explain.evidence_count = Some(counts.get(&hit.id).copied().unwrap_or(0));
                (hit, explain)
            })
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn hybrid_search_inner(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        query_vec: Option<Vec<f32>>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
        explain: bool,
    ) -> StoreResult<Vec<(PageHit, Option<SearchExplain>)>> {
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        // The routing decision is read off the query before the FTS call
        // consumes it; with routing disabled this is a no-op `false`.
        let tuning = self.tuning;
        let session_recall = tuning.session_recall_routing && is_session_recall_query(&query);
        // Authority is applied after RRF, so every stream needs the same
        // bounded candidate window used by FTS-only search. A `limit * 2`
        // window was too narrow at small limits for a canonical page to enter
        // the fused pool and receive its bounded promotion.
        let candidate_limit = authority_candidate_limit(limit);
        // Tokenize the borrowed query before the FTS candidate call consumes
        // it. This avoids cloning a caller-controlled query on the hot path.
        let entity_hits = self
            .entity_hits_for_project(
                workspace_id,
                project_id,
                &query,
                candidate_limit,
                Some(cutoff),
            )
            .await?;
        let fts_candidates = self
            .search_page_candidates_for_project(
                workspace_id,
                project_id,
                query,
                candidate_limit,
                Some(cutoff),
            )
            .await?;
        let mut authorities: std::collections::HashMap<PageId, PageAuthority> = fts_candidates
            .iter()
            .map(|(hit, authority)| (hit.id, *authority))
            .collect();
        let fts_hits: Vec<PageHit> = fts_candidates
            .into_iter()
            .map(|(hit, _authority)| hit)
            .collect();
        let mut vec_hits: Vec<(PageId, PagePath, f32)> = Vec::new();
        let mut abstract_hits: Vec<(PageId, PagePath, f32)> = Vec::new();
        if let Some(qv) = query_vec {
            if tuning.abstract_vectors {
                abstract_hits = self
                    .top_embedding_hits_in_table(
                        EmbeddingTable::Abstract,
                        workspace_id,
                        project_id,
                        qv.clone(),
                        provider.clone(),
                        model.clone(),
                        dim,
                        candidate_limit,
                        cutoff,
                    )
                    .await?;
            }
            vec_hits = self
                .top_embedding_hits_for_project(
                    workspace_id,
                    project_id,
                    qv,
                    provider,
                    model,
                    dim,
                    candidate_limit,
                    cutoff,
                )
                .await?;
        }

        let mut seed_seen = std::collections::HashSet::new();
        let mut seed_ids = Vec::new();
        let mut seed_paths = explain.then(Vec::new);
        for hit in &fts_hits {
            if seed_seen.insert(hit.id) {
                seed_ids.push(hit.id);
                if let Some(paths) = &mut seed_paths {
                    paths.push(hit.path.as_str().to_string());
                }
            }
        }
        for (id, path, _) in vec_hits.iter().chain(abstract_hits.iter()) {
            if seed_seen.insert(*id) {
                seed_ids.push(*id);
                if let Some(paths) = &mut seed_paths {
                    paths.push(path.as_str().to_string());
                }
            }
        }
        for e in &entity_hits {
            if seed_seen.insert(e.hit.id) {
                seed_ids.push(e.hit.id);
                if let Some(paths) = &mut seed_paths {
                    paths.push(e.hit.path.as_str().to_string());
                }
            }
        }
        let graph_hits = self
            .graph_neighbors_for_project_explained(
                workspace_id,
                project_id,
                seed_ids,
                candidate_limit,
                Some(cutoff),
            )
            .await?;

        // RRF fuse: score(d) = Σ 1/(k + rank_i(d)) over rankers.
        let k = 60.0_f64;
        struct Fused {
            path: PagePath,
            title: String,
            snippet: String,
            score: f64,
            explain: Option<SearchExplain>,
        }
        let mut fused: std::collections::HashMap<PageId, Fused> = std::collections::HashMap::new();

        for (rank, h) in fts_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(h.id).or_insert_with(|| Fused {
                path: h.path.clone(),
                title: h.title.clone(),
                snippet: h.snippet.clone(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.fts_rank = Some(rank + 1);
                details.fts_score = Some(h.rank);
                details.rrf.fts = contrib;
                details.fused += contrib;
            }
        }
        for (rank, (id, path, cosine)) in vec_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(*id).or_insert_with(|| Fused {
                path: path.clone(),
                title: String::new(),
                snippet: String::new(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.vector_rank = Some(rank + 1);
                details.cosine = Some(*cosine);
                details.rrf.vector = contrib;
                details.fused += contrib;
            }
        }
        for (rank, (id, path, cosine)) in abstract_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(*id).or_insert_with(|| Fused {
                path: path.clone(),
                title: String::new(),
                snippet: String::new(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.abstract_rank = Some(rank + 1);
                details.abstract_cosine = Some(*cosine);
                details.rrf.abstract_vector = contrib;
                details.fused += contrib;
            }
        }
        for (rank, e) in entity_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(e.hit.id).or_insert_with(|| Fused {
                path: e.hit.path.clone(),
                title: e.hit.title.clone(),
                snippet: e.hit.snippet.clone(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.entity_rank = Some(rank + 1);
                details.entity_weight = Some(e.weight);
                details.matched_entities = e.matched.clone();
                details.rrf.entity = contrib;
                details.fused += contrib;
            }
        }
        for (rank, n) in graph_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(n.hit.id).or_insert_with(|| Fused {
                path: n.hit.path.clone(),
                title: n.hit.title.clone(),
                snippet: n.hit.snippet.clone(),
                score: 0.0,
                explain: explain.then(SearchExplain::default),
            });
            entry.score += contrib;
            if let Some(details) = &mut entry.explain {
                details.graph_rank = Some(rank + 1);
                details.graph_via = Some(GraphVia {
                    seed_path: seed_paths
                        .as_ref()
                        .and_then(|paths| paths.get(n.seed_ord))
                        .cloned()
                        .unwrap_or_default(),
                    direction: if n.incoming { "incoming" } else { "outgoing" },
                    edge: n.edge.clone(),
                });
                details.rrf.graph = contrib;
                details.fused += contrib;
            }
        }

        let mut out: Vec<(PageHit, Option<SearchExplain>)> = fused
            .into_iter()
            .map(|(id, entry)| {
                (
                    PageHit {
                        id,
                        path: entry.path,
                        title: entry.title,
                        snippet: entry.snippet,
                        rank: -entry.score, // lower = better (matches FTS5 convention)
                    },
                    entry.explain,
                )
            })
            .collect();
        // #486: fill in hits the vector stream reached first. Only those with
        // an empty title are looked up — a page fts, entity or graph already
        // described keeps the descriptor those streams computed, which is the
        // whole reason not to recompute it here.
        let undescribed: Vec<PageId> = out
            .iter()
            .filter_map(|(hit, _)| hit.title.is_empty().then_some(hit.id))
            .collect();
        if !undescribed.is_empty() {
            let descriptors = self
                .page_descriptors_for_ids(workspace_id, project_id, undescribed)
                .await?;
            for (hit, _) in &mut out {
                if let Some((title, descriptor)) = descriptors.get(&hit.id) {
                    hit.title = title.clone();
                    hit.snippet = descriptor.clone();
                }
            }
        }

        let missing_authorities: Vec<PageId> = out
            .iter()
            .filter_map(|(hit, _)| (!authorities.contains_key(&hit.id)).then_some(hit.id))
            .collect();
        authorities.extend(
            self.page_authorities_for_project(workspace_id, project_id, missing_authorities)
                .await?,
        );
        for (hit, explain) in &mut out {
            if let Some(authority) = authorities.get(&hit.id) {
                let factor = authority.factor_for(session_recall, tuning.session_recall_bonus);
                hit.rank = apply_rank_multiplier(hit.rank, factor);
                // `fused` stays the raw RRF sum; without the multiplier
                // beside it the explain could not account for the rank it
                // returns, which is the whole point of the surface.
                if let Some(details) = explain {
                    details.authority = Some(factor);
                    if session_recall {
                        details.intent = Some("session_recall");
                        details.intent_boost = Some(factor / authority.factor);
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            a.0.rank
                .partial_cmp(&b.0.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Deterministic tiebreak: equal fused scores (common when
                // two streams each contribute a rank-1 hit) previously fell
                // back to HashMap iteration order.
                .then_with(|| a.0.path.as_str().cmp(b.0.path.as_str()))
        });
        out.truncate(limit);
        Ok(out)
    }
}
