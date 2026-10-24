//! Bootstrap chunk progress.

use super::*;

/// One recorded bootstrap chunk, as loaded by [`load_bootstrap_progress`].
#[derive(Debug, Clone)]
pub struct BootstrapChunkRecord {
    /// Position of this chunk in the run's chunk plan (0-based).
    pub chunk_index: u32,
    /// The chunk's `BootstrapPage` batch, serialized as JSON.
    pub pages_json: String,
    /// The chunk's LLM-authored rationale.
    pub rationale: String,
}

/// Durably record one completed bootstrap chunk's output (#621).
///
/// `INSERT OR REPLACE` makes this idempotent: a non-resume run still records
/// every chunk (so a later `--resume` has something to reuse), and re-running
/// the same chunk index just overwrites its row.
///
/// # Errors
/// Returns [`StoreError`] if the SQL statement fails.
pub fn record_bootstrap_chunk(
    conn: &Connection,
    fingerprint: &str,
    chunk_index: u32,
    pages_json: &str,
    rationale: &str,
) -> StoreResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO bootstrap_chunk_progress \
         (fingerprint, chunk_index, pages_json, rationale, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            fingerprint,
            chunk_index,
            pages_json,
            rationale,
            Timestamp::now().as_microsecond(),
        ],
    )?;
    Ok(())
}

/// Load every recorded chunk for `fingerprint`, ordered by `chunk_index` — the
/// order `bootstrap --resume` needs to seed its `pages_by_path` accumulator.
///
/// # Errors
/// Returns [`StoreError`] if the SQL statement fails.
pub fn load_bootstrap_progress(
    conn: &Connection,
    fingerprint: &str,
) -> StoreResult<Vec<BootstrapChunkRecord>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_index, pages_json, rationale FROM bootstrap_chunk_progress \
         WHERE fingerprint = ?1 ORDER BY chunk_index",
    )?;
    let rows = stmt
        .query_map(params![fingerprint], |row| {
            Ok(BootstrapChunkRecord {
                chunk_index: row.get(0)?,
                pages_json: row.get(1)?,
                rationale: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Delete every recorded chunk for `fingerprint`. Called once a bootstrap run
/// completes successfully — a fresh run has nothing left to resume.
///
/// # Errors
/// Returns [`StoreError`] if the SQL statement fails.
pub fn clear_bootstrap_progress(conn: &Connection, fingerprint: &str) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM bootstrap_chunk_progress WHERE fingerprint = ?1",
        params![fingerprint],
    )?;
    Ok(())
}
