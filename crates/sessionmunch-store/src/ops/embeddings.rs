//! Embedding writes, failures and stale-vector cleanup.

use super::*;

/// Store / replace one page's embedding. Bytes are the host-endian
/// `f32` packing of the unit-normalised vector. Provider/model/dim
/// are denormalised onto the row so a single SELECT can detect
/// heterogeneity (refuse-on-mismatch path).
/// Why an embed attempt produced no embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedOutcome {
    /// The embedding provider returned an error.
    Failed,
    /// The page could not be read from the wiki.
    Unreadable,
    /// The page body was empty after trimming, so there was nothing to embed.
    /// Not an error, but permanent until the body changes — and previously
    /// indistinguishable from an idle pass.
    SkippedEmpty,
}

impl EmbedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Unreadable => "unreadable",
            Self::SkippedEmpty => "skipped_empty",
        }
    }
}

/// Longest `detail` we keep. A provider error can carry a whole response body;
/// the ledger exists to say what happened, not to archive it.
pub(super) const EMBED_FAILURE_DETAIL_MAX: usize = 500;

/// Record that an embed attempt on `page_id` did not produce an embedding.
///
/// Failure-only by design: a success writes nothing, because the existence of
/// a `page_embeddings` row already records it. That keeps the common path free
/// of an extra write and means a global `embed --force` cannot erase the
/// history here — it never touches this table (#528).
///
/// Upserts on `page_id`, so the row is always the most recent unsuccessful
/// attempt. It is deliberately left in place after a later pass succeeds: join
/// against `page_embeddings` to tell "still unresolved" from "recovered".
///
/// # Errors
/// Propagates SQL errors.
pub fn record_embed_failure(
    conn: &Connection,
    page_id: &PageId,
    outcome: EmbedOutcome,
    detail: Option<&str>,
) -> StoreResult<()> {
    let detail = detail.map(|d| {
        let mut truncated: String = d.chars().take(EMBED_FAILURE_DETAIL_MAX).collect();
        if truncated.chars().count() < d.chars().count() {
            truncated.push('…');
        }
        truncated
    });
    conn.execute(
        "INSERT INTO page_embed_failures (page_id, at, outcome, detail) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(page_id) DO UPDATE SET \
             at = excluded.at, outcome = excluded.outcome, detail = excluded.detail",
        params![
            page_id.as_bytes(),
            Timestamp::now().as_microsecond(),
            outcome.as_str(),
            detail,
        ],
    )?;
    Ok(())
}

pub fn store_embedding(
    conn: &mut Connection,
    page_id: &PageId,
    vector_bytes: &[u8],
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    conn.execute(
        "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(page_id) DO UPDATE SET \
             vector = excluded.vector, \
             provider = excluded.provider, \
             model = excluded.model, \
             dim = excluded.dim, \
             created_at = excluded.created_at",
        params![page_id.as_bytes(), vector_bytes, provider, model, dim, now,],
    )?;
    Ok(())
}

/// Store / replace a batch of page embeddings in one transaction.
pub fn store_embeddings(conn: &mut Connection, embeddings: &[EmbeddingWrite]) -> StoreResult<()> {
    store_embeddings_in_table(conn, "page_embeddings", embeddings)
}

/// Store / replace a batch of L0 abstract embeddings
/// (`page_abstract_embeddings`) in one transaction. Same row shape and
/// upsert rule as [`store_embeddings`]; only the table differs.
pub fn store_abstract_embeddings(
    conn: &mut Connection,
    embeddings: &[EmbeddingWrite],
) -> StoreResult<()> {
    store_embeddings_in_table(conn, "page_abstract_embeddings", embeddings)
}

/// Store / replace a batch of passage embeddings in one transaction.
pub fn store_passage_embeddings(
    conn: &mut Connection,
    embeddings: &[PassageEmbeddingWrite],
) -> StoreResult<()> {
    if embeddings.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let sql = "INSERT INTO page_passage_embeddings (passage_id, provider, model, dim, embedding) \
                   VALUES (?1, ?2, ?3, ?4, ?5) \
                   ON CONFLICT(passage_id) DO UPDATE SET \
                       provider = excluded.provider, \
                       model = excluded.model, \
                       dim = excluded.dim, \
                       embedding = excluded.embedding";
        let mut stmt = tx.prepare(sql)?;
        for embedding in embeddings {
            stmt.execute(params![
                embedding.passage_id.as_bytes(),
                embedding.provider.as_str(),
                embedding.model.as_str(),
                embedding.dim,
                embedding.vector_bytes.as_slice(),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn store_embeddings_in_table(
    conn: &mut Connection,
    table: &'static str,
    embeddings: &[EmbeddingWrite],
) -> StoreResult<()> {
    if embeddings.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    {
        let sql = format!(
            "INSERT INTO {table} (page_id, vector, provider, model, dim, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(page_id) DO UPDATE SET \
                 vector = excluded.vector, \
                 provider = excluded.provider, \
                 model = excluded.model, \
                 dim = excluded.dim, \
                 created_at = excluded.created_at"
        );
        let mut stmt = tx.prepare(&sql)?;
        for embedding in embeddings {
            stmt.execute(params![
                embedding.page_id.as_bytes(),
                embedding.vector_bytes.as_slice(),
                embedding.provider.as_str(),
                embedding.model.as_str(),
                embedding.dim,
                now,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Remove a page's L0 abstract embedding row, if any. Called when a page is
/// rewritten without its frontmatter `abstract:` so the abstract stream
/// never ranks on a line the page no longer carries.
pub fn delete_abstract_embedding(conn: &mut Connection, page_id: &PageId) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM page_abstract_embeddings WHERE page_id = ?1",
        params![page_id.as_bytes()],
    )?;
    Ok(())
}

/// Remove embedding rows in a workspace/project scope whose `(provider, model, dim)`
/// does not match the configured triple, plus rows tied to superseded pages.
pub fn delete_stale_page_embeddings(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let (n, orphans) = if let Some(project_id) = project_id {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1\
             ) \
               AND NOT (provider = ?3 AND model = ?4 AND dim = CAST(?5 AS INTEGER))",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                provider,
                model,
                dim
            ],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
        (n, orphans)
    } else {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 1\
             ) \
               AND NOT (provider = ?2 AND model = ?3 AND dim = CAST(?4 AS INTEGER))",
            params![workspace_id.as_bytes(), provider, model, dim],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes()],
        )?;
        (n, orphans)
    };
    tx.commit()?;
    Ok(u64::try_from(n.saturating_add(orphans)).unwrap_or(0))
}
