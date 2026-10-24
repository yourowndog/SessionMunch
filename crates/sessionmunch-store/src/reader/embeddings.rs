//! Stored embeddings and vector-similarity candidates.

use super::*;

impl ReaderPool {
    /// Load every `is_latest=1` page's embedding for the project, but
    /// only when the stored `(provider, model, dim)` matches the
    /// caller's expectation. Mismatched rows are skipped (the
    /// refuse-on-mismatch check is `embedding_meta_for_mismatch`).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn load_embeddings(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<StoredEmbedding>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT page_embeddings.page_id, page_embeddings.vector, pages.path \
                 FROM page_embeddings \
                 JOIN pages ON pages.id = page_embeddings.page_id \
                 WHERE pages.workspace_id = ?1 \
                   AND pages.project_id = ?2 \
                   AND pages.is_latest = 1 \
                   AND page_embeddings.provider = ?3 \
                   AND page_embeddings.model = ?4 \
                   AND page_embeddings.dim = ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider,
                    model,
                    dim,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let vec_bytes: Vec<u8> = row.get(1)?;
                    let path: String = row.get(2)?;
                    Ok((id_bytes, vec_bytes, path))
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                let (id_bytes, vec_bytes, path) = r?;
                let id = PageId::from_slice(&id_bytes)?;
                let path = PagePath::new(path)?;
                let vector = bytes_to_f32_vec(&vec_bytes, dim)?;
                out.push(StoredEmbedding { id, path, vector });
            }
            Ok(out)
        })
        .await
    }

    /// Return page ids that already have a matching embedding row.
    ///
    /// This is cheaper than [`ReaderPool::load_embeddings`] for backfill paths
    /// that only need to skip already-embedded pages.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn embedded_page_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<PageId>> {
        self.embedded_page_ids_in_table(
            EmbeddingTable::Body,
            workspace_id,
            project_id,
            provider,
            model,
            dim,
        )
        .await
    }

    /// Page ids whose L0 abstract already has a matching embedding row
    /// (`page_abstract_embeddings`). The backfill uses it to skip pages
    /// whose abstract is already indexed under the current triple.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn abstract_embedded_page_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<PageId>> {
        self.embedded_page_ids_in_table(
            EmbeddingTable::Abstract,
            workspace_id,
            project_id,
            provider,
            model,
            dim,
        )
        .await
    }

    /// Latest pages of a project whose frontmatter carries a non-empty
    /// `abstract` string — the only pages the L0 stream can index. Lets the
    /// backfill decide without reading every page off disk.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn abstract_bearing_page_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<PageId>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT pages.id \
                 FROM pages \
                 WHERE pages.workspace_id = ?1 \
                   AND pages.project_id = ?2 \
                   AND pages.is_latest = 1 \
                   AND json_type(pages.frontmatter_json, '$.abstract') = 'text' \
                   AND length(trim(json_extract(pages.frontmatter_json, '$.abstract'))) > 0",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    Ok(id_bytes)
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(PageId::from_slice(&row?)?);
            }
            Ok(out)
        })
        .await
    }

    pub(super) async fn embedded_page_ids_in_table(
        &self,
        table: EmbeddingTable,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<PageId>> {
        let table = table.name();
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT {table}.page_id \
                 FROM {table} \
                 JOIN pages ON pages.id = {table}.page_id \
                 WHERE pages.workspace_id = ?1 \
                   AND pages.project_id = ?2 \
                   AND pages.is_latest = 1 \
                   AND {table}.provider = ?3 \
                   AND {table}.model = ?4 \
                   AND {table}.dim = ?5"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider,
                    model,
                    dim,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    Ok(id_bytes)
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(PageId::from_slice(&row?)?);
            }
            Ok(out)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn top_embedding_hits_for_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query_vec: Vec<f32>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: i64,
    ) -> StoreResult<Vec<(PageId, PagePath, f32)>> {
        self.top_embedding_hits_in_table(
            EmbeddingTable::Body,
            workspace_id,
            project_id,
            query_vec,
            provider,
            model,
            dim,
            limit,
            expiry_cutoff_us,
        )
        .await
    }

    /// Cosine top-`limit` over one embedding table for the latest, unexpired
    /// pages of a project. The body and L0-abstract tables share a schema,
    /// so the scan differs only in which table it reads.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn top_embedding_hits_in_table(
        &self,
        table: EmbeddingTable,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query_vec: Vec<f32>,
        provider: String,
        model: String,
        dim: u32,
        limit: usize,
        expiry_cutoff_us: i64,
    ) -> StoreResult<Vec<(PageId, PagePath, f32)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let table = table.name();
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT {table}.page_id, {table}.vector, pages.path \
                 FROM {table} \
                 JOIN pages ON pages.id = {table}.page_id \
                 WHERE pages.workspace_id = ?1 \
                   AND pages.project_id = ?2 \
                   AND pages.is_latest = 1{not_expired} \
                   AND {table}.provider = ?3 \
                   AND {table}.model = ?4 \
                   AND {table}.dim = ?5",
                not_expired = not_expired("pages", "?6"),
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider,
                    model,
                    dim,
                    expiry_cutoff_us,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let vec_bytes: Vec<u8> = row.get(1)?;
                    let path: String = row.get(2)?;
                    Ok((id_bytes, vec_bytes, path))
                },
            )?;

            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, vec_bytes, path) = row?;
                out.push((
                    PageId::from_slice(&id_bytes)?,
                    PagePath::new(path)?,
                    dot_embedding_bytes(&query_vec, &vec_bytes, dim)?,
                ));
            }
            if out.len() > limit {
                out.select_nth_unstable_by(limit, score_desc);
                out.truncate(limit);
            }
            out.sort_by(score_desc);
            Ok(out)
        })
        .await
    }

    /// Return any `(provider, model, dim)` triples currently stored
    /// that *don't* match the caller's expectation. An empty vec
    /// means "all clean". Used at startup for the refuse-on-mismatch
    /// (agentmemory #469 lesson).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn embedding_meta_for_mismatch(
        &self,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<Vec<(String, String, u32, u64)>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT pe.provider, pe.model, pe.dim, COUNT(*) \
                 FROM page_embeddings pe \
                 JOIN pages pg ON pg.id = pe.page_id AND pg.is_latest = 1 \
                 WHERE NOT (pe.provider = ?1 AND pe.model = ?2 AND pe.dim = ?3) \
                 GROUP BY pe.provider, pe.model, pe.dim",
            )?;
            let rows = stmt.query_map(params![provider, model, dim], |row| {
                let provider: String = row.get(0)?;
                let model: String = row.get(1)?;
                let dim: i64 = row.get(2)?;
                let count: i64 = row.get(3)?;
                Ok((
                    provider,
                    model,
                    u32::try_from(dim).unwrap_or(0),
                    u64::try_from(count).unwrap_or(0),
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Get all passages in a workspace/project that need embeddings.
    ///
    /// Returns passages lacking a current `(provider, model, dim)` embedding row,
    /// or all passages if `reembed` is true. Used by passage embedding backfill.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn passage_candidates(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        provider: &str,
        model: &str,
        dim: u32,
    ) -> StoreResult<Vec<PassageEmbedCandidate>> {
        let provider = provider.to_string();
        let model = model.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT pp.id, pp.text FROM page_passages pp \
                 LEFT JOIN page_passage_embeddings ppe \
                   ON pp.id = ppe.passage_id \
                   AND ppe.provider = ?3 \
                   AND ppe.model = ?4 \
                   AND ppe.dim = ?5 \
                 WHERE pp.workspace_id = ?1 AND pp.project_id = ?2 \
                   AND ppe.passage_id IS NULL",
            )?;
            let rows = stmt.query_map(
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    provider.as_str(),
                    model.as_str(),
                    dim as i64,
                ],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let text: String = row.get(1)?;
                    Ok((id_bytes, text))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (id_bytes, text) = row?;
                let id =
                    uuid::Uuid::from_slice(&id_bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
                out.push(PassageEmbedCandidate { id, text });
            }
            Ok(out)
        })
        .await
    }
}
