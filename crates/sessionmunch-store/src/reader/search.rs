//! Page, passage and observation search, plus recent-page listings.

use super::*;

impl ReaderPool {
    /// Run a full-text search against the FTS5 index, apply the bounded page
    /// authority adjustment, and return the top `is_latest = 1` matches.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_pages(&self, query: String, limit: usize) -> StoreResult<Vec<PageHit>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "SELECT pages.id, pages.path, pages.title, \
                        snippet(pages_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        pages_fts.rank, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM pages_fts \
                 JOIN pages ON pages.rowid = pages_fts.rowid \
                 WHERE pages_fts MATCH ?1 AND pages.is_latest = 1{not_expired} \
                 ORDER BY pages_fts.rank \
                 LIMIT ?2",
                not_expired = not_expired("pages", "?3"),
            );
            let mut stmt = conn.prepare(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![fts_query, authority_candidate_limit(limit) as i64, now_us()],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let path: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let snippet: String = row.get(3)?;
                    let rank: f64 = row.get(4)?;
                    let tier: String = row.get(5)?;
                    let pinned = row.get::<_, i64>(6)? != 0;
                    let frontmatter_json: String = row.get(7)?;
                    let kind: String = row.get(8)?;
                    Ok((
                        id_bytes,
                        path,
                        title,
                        snippet,
                        rank,
                        tier,
                        pinned,
                        frontmatter_json,
                        kind,
                    ))
                },
            )?;

            let mut candidates = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank, tier, pinned, frontmatter_json, kind) =
                    row?;
                let authority =
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json);
                candidates.push((
                    PageHit {
                        id: PageId::from_slice(&id_bytes)?,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank,
                    },
                    authority,
                ));
            }
            Ok(rerank_page_hits(candidates, limit))
        })
        .await
    }

    /// Run a global authority-adjusted full-text search and include
    /// workspace/project names in each row. This keeps the web search route
    /// to one SQLite query instead of one search query plus a metadata lookup
    /// per hit.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_pages_with_meta(
        &self,
        query: String,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<PageHitWithMeta>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "SELECT workspaces.name, projects.name, pages.path, pages.title, \
                        snippet(pages_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        pages_fts.rank, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM pages_fts \
                 JOIN pages ON pages.rowid = pages_fts.rowid \
                 JOIN projects ON projects.id = pages.project_id \
                 JOIN workspaces ON workspaces.id = pages.workspace_id \
                 WHERE pages_fts MATCH ?1 AND pages.is_latest = 1{not_expired} \
                 ORDER BY pages_fts.rank \
                 LIMIT ?2",
                not_expired = not_expired("pages", "?3"),
            );
            let mut stmt = conn.prepare(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![fts_query, authority_candidate_limit(limit) as i64, cutoff],
                |row| {
                    let workspace_name: String = row.get(0)?;
                    let project_name: String = row.get(1)?;
                    let path: String = row.get(2)?;
                    let title: String = row.get(3)?;
                    let snippet: String = row.get(4)?;
                    let rank: f64 = row.get(5)?;
                    let tier: String = row.get(6)?;
                    let pinned = row.get::<_, i64>(7)? != 0;
                    let frontmatter_json: String = row.get(8)?;
                    let kind: String = row.get(9)?;
                    Ok((
                        workspace_name,
                        project_name,
                        path,
                        title,
                        snippet,
                        rank,
                        tier,
                        pinned,
                        frontmatter_json,
                        kind,
                    ))
                },
            )?;

            let mut candidates = Vec::new();
            for row in rows {
                let (
                    workspace_name,
                    project_name,
                    path,
                    title,
                    snippet,
                    rank,
                    tier,
                    pinned,
                    frontmatter_json,
                    kind,
                ) = row?;
                let authority =
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json);
                candidates.push((
                    PageHitWithMeta {
                        workspace_name,
                        project_name,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank,
                    },
                    authority,
                ));
            }
            Ok(rerank_page_hits_with_meta(candidates, limit))
        })
        .await
    }

    /// Run an authority-adjusted full-text search scoped to one project.
    ///
    /// `expiry_cutoff_us`: `None` hides pages whose TTL has passed as of
    /// now (the default retrieval behaviour); `Some(cutoff)` compares
    /// against that instant instead — pass `i64::MIN` to include
    /// expired pages.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_pages_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<PageHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let candidates = self
            .search_page_candidates_for_project(
                workspace_id,
                project_id,
                query,
                authority_candidate_limit(limit),
                expiry_cutoff_us,
            )
            .await?;
        Ok(rerank_page_hits(candidates, limit))
    }

    /// Run a hybrid passage search over `page_passages_fts` (lexical)
    /// and `page_passage_embeddings` (dense), fusing both rank lists
    /// with Reciprocal Rank Fusion (k=60).
    ///
    /// `query` is the free-text lexical query passed through
    /// [`prepare_fts5_query`] before being used as a `MATCH` argument
    /// against `page_passages_fts`. When `query_vector` is `Some`, the
    /// dense stream is also searched: `page_passage_embeddings` rows
    /// are gated by workspace_id, project_id AND matching
    /// {provider, model, dim}, then ranked by cosine similarity with
    /// `query_vector` (normalized to unit length before comparison).
    /// When `query_vector` is `None`, only the lexical stream runs
    /// and every hit carries `lexical_rank` set and `dense_rank = None`.
    ///
    /// The two rank lists are fused with RRF (k=60) and ties broken
    /// deterministically by passage id so re-running the same query
    /// never reorders ties.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_passages_hybrid(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        query_vector: Option<Vec<f32>>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
    ) -> StoreResult<Vec<PassageHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let fts_query = prepare_fts5_query(&query);
        let lexical = if fts_query.is_empty() {
            Vec::new()
        } else {
            self.search_passages_lexical(workspace_id, project_id, fts_query.clone(), limit)
                .await?
        };
        let dense = if let Some(vec) = query_vector {
            Some(
                self.search_passages_dense(
                    workspace_id,
                    project_id,
                    vec,
                    provider,
                    model,
                    dim,
                    limit,
                )
                .await?,
            )
        } else {
            None
        };
        Ok(rrf_fuse(lexical, dense, limit))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn search_passages_lexical(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        fts_query: String,
        limit: usize,
    ) -> StoreResult<Vec<PassageHit>> {
        self.with_conn(move |conn| {
            let sql = "SELECT page_passages.id, page_passages.section_id, \
                        page_passages.page_id, page_passages.workspace_id, \
                        page_passages.project_id, pages.path, pages.title, \
                        page_passages.heading_path, page_passages.text, \
                        page_passages.start_byte, page_passages.end_byte, \
                        page_passages_fts.rank \
                 FROM page_passages_fts \
                 JOIN page_passages ON page_passages.rowid = page_passages_fts.rowid \
                 JOIN pages ON pages.id = page_passages.page_id \
                 WHERE page_passages_fts MATCH ?1 \
                   AND page_passages.workspace_id = ?2 \
                   AND page_passages.project_id = ?3 \
                 ORDER BY page_passages_fts.rank, page_passages.id \
                 LIMIT ?4"
                .to_string();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![
                    fts_query,
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    limit as i64
                ],
                |row| {
                    let passage_id_bytes: Vec<u8> = row.get(0)?;
                    let section_id_bytes: Vec<u8> = row.get(1)?;
                    let page_id_bytes: Vec<u8> = row.get(2)?;
                    let ws_bytes: Vec<u8> = row.get(3)?;
                    let proj_bytes: Vec<u8> = row.get(4)?;
                    let path: String = row.get(5)?;
                    let title: String = row.get(6)?;
                    let heading_path: String = row.get(7)?;
                    let text: String = row.get(8)?;
                    let start_byte: i64 = row.get(9)?;
                    let end_byte: i64 = row.get(10)?;
                    let rank: f64 = row.get(11)?;
                    Ok((
                        passage_id_bytes,
                        section_id_bytes,
                        page_id_bytes,
                        ws_bytes,
                        proj_bytes,
                        path,
                        title,
                        heading_path,
                        text,
                        start_byte,
                        end_byte,
                        rank,
                    ))
                },
            )?;
            let mut hits = Vec::new();
            for row in rows {
                let (pid, sid, page_id, ws, proj, path, title, heading_path, text, s, e, _rank) =
                    row?;
                hits.push(PassageHit {
                    passage_id: uuid_from_stored_bytes(&pid, "page_passages.id")?,
                    section_id: uuid_from_stored_bytes(&sid, "page_passages.section_id")?,
                    page_id: PageId::from_slice(&page_id)?,
                    workspace_id: WorkspaceId::from_slice(&ws)?,
                    project_id: ProjectId::from_slice(&proj)?,
                    page_path: PagePath::new(path)?,
                    page_title: title,
                    heading_path,
                    text,
                    start_byte: s,
                    end_byte: e,
                    rank: 0,
                    lexical_rank: None,
                    dense_rank: None,
                    rrf_score: None,
                });
            }
            // SQL rows already arrive in BM25 order (`page_passages_fts.rank`
            // ascending, best first) with a deterministic UUID tie-break —
            // do NOT re-sort by passage id here, which would overwrite the
            // BM25 ordering and corrupt the `lexical_rank` fed into RRF.
            for (i, hit) in hits.iter_mut().enumerate() {
                hit.lexical_rank = Some(i + 1);
            }
            Ok(hits)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn search_passages_dense(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query_vector: Vec<f32>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
    ) -> StoreResult<Vec<(PassageHit, f32)>> {
        let dim_check = query_vector.len();
        if dim_check != dim as usize {
            return Err(StoreError::Memory(
                sessionmunch_core::MemoryError::MalformedRecord(format!(
                    "query vector dim {} != embedding dim {}",
                    dim_check, dim
                )),
            ));
        }
        self.with_conn(move |conn| {
            let sql = "SELECT page_passages.id, page_passages.section_id, \
                        page_passages.page_id, page_passages.workspace_id, \
                        page_passages.project_id, pages.path, pages.title, \
                        page_passages.heading_path, page_passages.text, \
                        page_passages.start_byte, page_passages.end_byte, \
                        page_passage_embeddings.embedding \
                 FROM page_passage_embeddings \
                 JOIN page_passages ON page_passages.id = page_passage_embeddings.passage_id \
                 JOIN pages ON pages.id = page_passages.page_id \
                 WHERE page_passages.workspace_id = ?1 \
                   AND page_passages.project_id = ?2 \
                   AND page_passage_embeddings.provider = ?3 \
                   AND page_passage_embeddings.model = ?4 \
                   AND page_passage_embeddings.dim = ?5"
                .to_string();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider,
                    model,
                    dim,
                ],
                |row| {
                    let passage_id_bytes: Vec<u8> = row.get(0)?;
                    let section_id_bytes: Vec<u8> = row.get(1)?;
                    let page_id_bytes: Vec<u8> = row.get(2)?;
                    let ws_bytes: Vec<u8> = row.get(3)?;
                    let proj_bytes: Vec<u8> = row.get(4)?;
                    let path: String = row.get(5)?;
                    let title: String = row.get(6)?;
                    let heading_path: String = row.get(7)?;
                    let text: String = row.get(8)?;
                    let start_byte: i64 = row.get(9)?;
                    let end_byte: i64 = row.get(10)?;
                    let embedding_bytes: Vec<u8> = row.get(11)?;
                    Ok((
                        passage_id_bytes,
                        section_id_bytes,
                        page_id_bytes,
                        ws_bytes,
                        proj_bytes,
                        path,
                        title,
                        heading_path,
                        text,
                        start_byte,
                        end_byte,
                        embedding_bytes,
                    ))
                },
            )?;
            let mut scored = Vec::new();
            for row in rows {
                let (pid, sid, page_id, ws, proj, path, title, heading_path, text, s, e, emb) =
                    row?;
                let query_norm = l2_normalize(&query_vector);
                let emb_norm = l2_normalize(&bytes_to_f32_vec(&emb, dim)?);
                let cosine = dot_f32(&query_norm, &emb_norm);
                scored.push((
                    PassageHit {
                        passage_id: uuid_from_stored_bytes(&pid, "page_passages.id")?,
                        section_id: uuid_from_stored_bytes(&sid, "page_passages.section_id")?,
                        page_id: PageId::from_slice(&page_id)?,
                        workspace_id: WorkspaceId::from_slice(&ws)?,
                        project_id: ProjectId::from_slice(&proj)?,
                        page_path: PagePath::new(path)?,
                        page_title: title,
                        heading_path,
                        text,
                        start_byte: s,
                        end_byte: e,
                        rank: 0,
                        lexical_rank: None,
                        dense_rank: None,
                        rrf_score: None,
                    },
                    cosine,
                ));
            }
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
            scored.truncate(limit);
            Ok(scored)
        })
        .await
    }

    pub(super) async fn search_page_candidates_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        candidate_limit: usize,
        expiry_cutoff_us: Option<i64>,
    ) -> StoreResult<Vec<(PageHit, PageAuthority)>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() || candidate_limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = expiry_cutoff_us.unwrap_or_else(now_us);
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "SELECT pages.id, pages.path, pages.title, \
                        snippet(pages_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        pages_fts.rank, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM pages_fts \
                 JOIN pages ON pages.rowid = pages_fts.rowid \
                 WHERE pages_fts MATCH ?1 \
                   AND pages.workspace_id = ?2 \
                   AND pages.project_id = ?3 \
                   AND pages.is_latest = 1{not_expired} \
                 ORDER BY pages_fts.rank \
                 LIMIT ?4",
                not_expired = not_expired("pages", "?5"),
            );
            let mut stmt = conn.prepare(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![
                    fts_query,
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    candidate_limit as i64,
                    cutoff
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let path: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let snippet: String = row.get(3)?;
                    let rank: f64 = row.get(4)?;
                    let tier: String = row.get(5)?;
                    let pinned = row.get::<_, i64>(6)? != 0;
                    let frontmatter_json: String = row.get(7)?;
                    let kind: String = row.get(8)?;
                    Ok((
                        id_bytes,
                        path,
                        title,
                        snippet,
                        rank,
                        tier,
                        pinned,
                        frontmatter_json,
                        kind,
                    ))
                },
            )?;

            let mut candidates = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank, tier, pinned, frontmatter_json, kind) =
                    row?;
                let authority =
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json);
                candidates.push((
                    PageHit {
                        id: PageId::from_slice(&id_bytes)?,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank,
                    },
                    authority,
                ));
            }
            Ok(candidates)
        })
        .await
    }

    /// Authority-adjusted full-text candidates over the page versions
    /// whose ingestion windows contain `as_of_us` (issue #656).
    /// BM25 uses the current index's statistics, not a snapshot at T.
    /// Same candidate shape as
    /// [`Self::search_page_candidates_for_project`], but the corpus is
    /// versions alive at `T` instead of latest versions. TTL expiry is
    /// evaluated at `T`: a page already expired then was already hidden
    /// from search then.
    pub(super) async fn search_page_candidates_for_project_at(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        candidate_limit: usize,
        as_of_us: i64,
    ) -> StoreResult<Vec<(PageHit, PageAuthority)>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() || candidate_limit == 0 {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let kind_expr = page_kind_expr("pages.path", "pages.frontmatter_json");
            let sql = format!(
                "SELECT pages.id, pages.path, pages.title, \
                        snippet(pages_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        pages_fts.rank, pages.tier, pages.pinned, \
                        pages.frontmatter_json, {kind_expr} AS kind \
                 FROM pages_fts \
                 JOIN pages ON pages.rowid = pages_fts.rowid \
                 WHERE pages_fts MATCH ?1 \
                   AND pages.workspace_id = ?2 \
                   AND pages.project_id = ?3 \
                   AND pages.valid_from <= ?4 \
                   AND (pages.valid_to IS NULL OR pages.valid_to > ?5){not_expired} \
                 ORDER BY pages_fts.rank \
                 LIMIT ?7",
                not_expired = not_expired("pages", "?6"),
            );
            let mut stmt = conn.prepare(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![
                    fts_query,
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    as_of_us,
                    as_of_us,
                    as_of_us,
                    candidate_limit as i64
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let path: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let snippet: String = row.get(3)?;
                    let rank: f64 = row.get(4)?;
                    let tier: String = row.get(5)?;
                    let pinned = row.get::<_, i64>(6)? != 0;
                    let frontmatter_json: String = row.get(7)?;
                    let kind: String = row.get(8)?;
                    Ok((
                        id_bytes,
                        path,
                        title,
                        snippet,
                        rank,
                        tier,
                        pinned,
                        frontmatter_json,
                        kind,
                    ))
                },
            )?;

            let mut candidates = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank, tier, pinned, frontmatter_json, kind) =
                    row?;
                let authority =
                    PageAuthority::from_stored(&path, &kind, &tier, pinned, &frontmatter_json);
                candidates.push((
                    PageHit {
                        id: PageId::from_slice(&id_bytes)?,
                        path: PagePath::new(path)?,
                        title,
                        snippet,
                        rank,
                    },
                    authority,
                ));
            }
            Ok(candidates)
        })
        .await
    }

    /// Time-travel search backing `memory_query(as_of)`
    /// (docs/temporal.md, issue #656): the entity-window lookup
    /// unchanged, plus version-filtered FTS over the page ingestion
    /// windows alive at `as_of_us`, fused with the same RRF (k=60) the
    /// default path uses and the same bounded authority adjustment —
    /// current-index relevance over knowledge *valid at T*. Vector, graph, and
    /// the raw-observation fallback stay out of audit mode: embeddings
    /// and links are present-tense artifacts with no version scope, and
    /// audit reads must not perturb access stats (no bump, no rerank).
    ///
    /// `explain` mirrors [`Self::hybrid_search`]: `false` drops the
    /// per-hit [`SearchExplain`]; `streams_active` reporting stays with
    /// the MCP caller.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_pages_for_project_at(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        limit: usize,
        as_of_us: i64,
        explain: bool,
    ) -> StoreResult<Vec<(PageHit, Option<SearchExplain>)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let candidate_limit = authority_candidate_limit(limit);
        let entity_hits = self
            .entity_hits_for_project_at(
                workspace_id,
                project_id,
                &query,
                candidate_limit,
                None,
                Some(as_of_us),
            )
            .await?;
        let fts_candidates = self
            .search_page_candidates_for_project_at(
                workspace_id,
                project_id,
                query,
                candidate_limit,
                as_of_us,
            )
            .await?;
        let mut authorities: std::collections::HashMap<PageId, PageAuthority> = fts_candidates
            .iter()
            .map(|(hit, authority)| (hit.id, *authority))
            .collect();
        // Entity-only hits never passed through an FTS candidate row;
        // read their authority off the version row itself (no latest
        // filter — audit mode ranks superseded versions too).
        let missing: Vec<PageId> = entity_hits
            .iter()
            .map(|e| e.hit.id)
            .filter(|id| !authorities.contains_key(id))
            .collect();
        authorities.extend(
            self.page_authorities_for_versions(workspace_id, project_id, missing)
                .await?,
        );

        // RRF fuse: score(d) = Σ 1/(k + rank_i(d)) over the two streams.
        let k = 60.0_f64;
        struct FusedAt {
            path: PagePath,
            title: String,
            snippet: String,
            score: f64,
            explain: Option<SearchExplain>,
        }
        let mut fused: std::collections::HashMap<PageId, FusedAt> =
            std::collections::HashMap::new();
        for (rank, h) in fts_candidates.iter().map(|(hit, _)| hit).enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(h.id).or_insert_with(|| FusedAt {
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
        for (rank, e) in entity_hits.iter().enumerate() {
            let contrib = 1.0 / (k + (rank + 1) as f64);
            let entry = fused.entry(e.hit.id).or_insert_with(|| FusedAt {
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
        // No session-recall routing in audit mode; the plain authority
        // factor keeps maintained pages' bounded advantage at T.
        for (hit, details) in &mut out {
            if let Some(authority) = authorities.get(&hit.id) {
                hit.rank = authority.adjust_rank(hit.rank);
                if let Some(explain) = details {
                    explain.authority = Some(authority.factor);
                }
            }
        }
        out.sort_by(|a, b| {
            a.0.rank
                .partial_cmp(&b.0.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.path.as_str().cmp(b.0.path.as_str()))
        });
        out.truncate(limit);
        Ok(out)
    }

    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn search_observations_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: String,
        limit: usize,
    ) -> StoreResult<Vec<ObservationHit>> {
        let fts_query = normalize_fts_query(&query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT observations.id, observations.session_id, observations.kind, \
                        observations.title, \
                        snippet(observations_fts, 1, '<mark>', '</mark>', '…', 24) AS snip, \
                        observations_fts.rank, observations.created_at \
                 FROM observations_fts \
                 JOIN observations ON observations.rowid = observations_fts.rowid \
                 WHERE observations_fts MATCH ?1 \
                   AND observations.workspace_id = ?2 \
                   AND observations.project_id = ?3 \
                 ORDER BY observations_fts.rank \
                 LIMIT ?4",
            )?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![
                    fts_query,
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    limit as i64,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let session_bytes: Vec<u8> = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let title: String = row.get(3)?;
                    let snippet: String = row.get(4)?;
                    let rank: f64 = row.get(5)?;
                    let created_us: i64 = row.get(6)?;
                    Ok((
                        id_bytes,
                        session_bytes,
                        kind,
                        title,
                        snippet,
                        rank,
                        created_us,
                    ))
                },
            )?;

            let mut hits = Vec::new();
            for row in rows {
                let (id_bytes, session_bytes, kind, title, snippet, rank, created_us) = row?;
                let created_at = jiff::Timestamp::from_microsecond(created_us)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default();
                hits.push(ObservationHit {
                    id: ObservationId::from_slice(&id_bytes)?,
                    session_id: SessionId::from_slice(&session_bytes)?,
                    kind,
                    title,
                    snippet,
                    rank,
                    created_at,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// Return the N most-recently-updated `is_latest = 1` pages.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn recent_pages(&self, limit: usize) -> StoreResult<Vec<PageHit>> {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT id, path, title, \
                        {descriptor} AS snip, \
                        CAST(updated_at AS REAL) AS rank \
                 FROM pages \
                 WHERE is_latest = 1{not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?1",
                descriptor = page_descriptor_expr("body", "frontmatter_json"),
                not_expired = not_expired("pages", "?2"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(params![limit as i64, now_us()], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let path: String = row.get(1)?;
                let title: String = row.get(2)?;
                let snippet = page_descriptor(&row.get::<_, String>(3)?, &title);
                let rank: f64 = row.get(4)?;
                Ok((id_bytes, path, title, snippet, rank))
            })?;
            let mut hits = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank) = row?;
                hits.push(PageHit {
                    id: PageId::from_slice(&id_bytes)?,
                    path: PagePath::new(path)?,
                    title,
                    snippet,
                    rank,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// Return the N most-recently-updated pages scoped to one project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn recent_pages_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        limit: usize,
    ) -> StoreResult<Vec<PageHit>> {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT id, path, title, \
                        {descriptor} AS snip, \
                        CAST(updated_at AS REAL) AS rank \
                 FROM pages \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1{not_expired} \
                 ORDER BY updated_at DESC \
                 LIMIT ?3",
                descriptor = page_descriptor_expr("body", "frontmatter_json"),
                not_expired = not_expired("pages", "?4"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    limit as i64,
                    now_us()
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let path: String = row.get(1)?;
                    let title: String = row.get(2)?;
                    let snippet = page_descriptor(&row.get::<_, String>(3)?, &title);
                    let rank: f64 = row.get(4)?;
                    Ok((id_bytes, path, title, snippet, rank))
                },
            )?;
            let mut hits = Vec::new();
            for row in rows {
                let (id_bytes, path, title, snippet, rank) = row?;
                hits.push(PageHit {
                    id: PageId::from_slice(&id_bytes)?,
                    path: PagePath::new(path)?,
                    title,
                    snippet,
                    rank,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// The `limit` most-recently-updated `is_latest` pages across EVERY
    /// project, each annotated with its workspace + project name. The
    /// cross-project analog of [`Self::recent_pages_for_project`]; used when a
    /// read's scope broadens to global.
    ///
    /// Recency has no FTS relevance score, so the reused
    /// [`PageHitWithMeta::rank`] field carries `updated_at` (µs, cast to
    /// REAL) — larger still means "ranks first", callers must not read it
    /// as an FTS rank.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn recent_pages_global(&self, limit: usize) -> StoreResult<Vec<PageHitWithMeta>> {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT workspaces.name, projects.name, pages.path, pages.title, \
                        {descriptor} AS snip, \
                        CAST(pages.updated_at AS REAL) AS rank \
                 FROM pages \
                 JOIN projects ON projects.id = pages.project_id \
                 JOIN workspaces ON workspaces.id = pages.workspace_id \
                 WHERE pages.is_latest = 1{not_expired} \
                 ORDER BY pages.updated_at DESC \
                 LIMIT ?1",
                descriptor = page_descriptor_expr("pages.body", "pages.frontmatter_json"),
                not_expired = not_expired("pages", "?2"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            #[allow(clippy::cast_possible_wrap)]
            let rows = stmt.query_map(params![limit as i64, now_us()], |row| {
                let workspace_name: String = row.get(0)?;
                let project_name: String = row.get(1)?;
                let path: String = row.get(2)?;
                let title: String = row.get(3)?;
                let snippet = page_descriptor(&row.get::<_, String>(4)?, &title);
                let rank: f64 = row.get(5)?;
                Ok((workspace_name, project_name, path, title, snippet, rank))
            })?;
            let mut hits = Vec::new();
            for row in rows {
                let (workspace_name, project_name, path, title, snippet, rank) = row?;
                hits.push(PageHitWithMeta {
                    workspace_name,
                    project_name,
                    path: PagePath::new(path)?,
                    title,
                    snippet,
                    rank,
                });
            }
            Ok(hits)
        })
        .await
    }
}
