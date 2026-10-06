//! Session-owned event ledger and provenance store operations.
//!
//! Unlike the managed `workstream_events` (keyed by a workstream id and driven
//! only through `sessionmunch run`), the rows here belong to a real `sessions`
//! row so hook capture, transcript import, and managed runs converge on one
//! session identity. Event insertion is idempotent on the stable
//! source-derived `event_id`, and provenance is written best-effort.

use std::str::FromStr as _;

use jiff::Timestamp;
use rusqlite::{Connection, Transaction, params};
use sessionmunch_core::{
    NewSession, NewSessionEvent, ProjectId, SessionEvent, SessionEventHit, SessionId,
    SessionProvenance, SessionSource, WorkspaceId, WorkstreamEventKind,
};

use crate::StoreResult;

/// Store-boundary bound on a session event's free-text `content` (16 KiB,
/// matching the observation and workstream-event caps). Defense in depth: the
/// capture layer is expected to bound tighter, but the store is the last gate
/// before durable persistence.
const SESSION_EVENT_CONTENT_MAX_BYTES: usize = 16 * 1024;

/// Begin a session row, writing harness (the `agent_kind` free-text column)
/// and any optional provenance the caller can supply.
///
/// This is the promoted begin path: it accepts the same idempotent session
/// identity as `ops::begin_session` but populates the provenance columns the
/// V65 migration added. A second call with the same id leaves the row as-is
/// (same idempotency contract). The base row is ensured in the same
/// transaction so global/import sessions converge on one identity.
pub(crate) fn begin_session_with_provenance(
    conn: &mut Connection,
    session: &NewSession,
    provenance: &SessionProvenance,
) -> StoreResult<()> {
    let tx = conn.transaction()?;
    // Ensure the base session row exists first (idempotent, carries the
    // purge/sanitize guards on the existing path).
    crate::ops::begin_session_in_transaction(&tx, session)?;
    ensure_provenance_in_transaction(&tx, session.id.as_bytes(), provenance)?;
    tx.commit()?;
    Ok(())
}

/// Append events to a session-owned ledger, idempotently skipping any whose
/// stable `event_id` already exists for that session. Returns the number of
/// newly inserted rows.
pub(crate) fn append_session_events(
    conn: &mut Connection,
    session_id: &SessionId,
    events: &[NewSessionEvent],
) -> StoreResult<usize> {
    let mut appended = 0;
    let tx = conn.transaction()?;
    for event in events {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_events \
             WHERE session_id = ?1 AND event_id = ?2)",
            params![session_id.as_bytes(), event.event_id],
            |row| row.get(0),
        )?;
        if exists {
            continue;
        }
        let next_sequence: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM session_events \
             WHERE session_id = ?1",
            params![session_id.as_bytes()],
            |row| row.get(0),
        )?;
        let content =
            sessionmunch_core::truncate_utf8_bytes(&event.content, SESSION_EVENT_CONTENT_MAX_BYTES);
        tx.execute(
            "INSERT INTO session_events( \
                 session_id, sequence, event_id, kind, role, content, \
                 occurred_at, metadata_json, source, created_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id.as_bytes(),
                next_sequence,
                event.event_id,
                event.kind.as_str(),
                event.role,
                content,
                event.occurred_at,
                serde_json::to_string(&event.metadata)?,
                event.source.as_str(),
                Timestamp::now().as_microsecond(),
            ],
        )?;
        appended += 1;
    }
    tx.commit()?;
    Ok(appended)
}

/// Read the newest `limit` events for a session, newest first.
pub(crate) fn list_session_events(
    conn: &Connection,
    session_id: &SessionId,
    limit: usize,
) -> StoreResult<Vec<SessionEvent>> {
    let limit = limit.clamp(1, 1000) as i64;
    let mut statement = conn.prepare(
        "SELECT sequence, event_id, kind, role, content, occurred_at, metadata_json \
         FROM session_events WHERE session_id = ?1 \
         ORDER BY sequence DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![session_id.as_bytes(), limit], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (sequence, event_id, kind, role, content, occurred_at, metadata) = row?;
        events.push(SessionEvent {
            sequence,
            event_id,
            kind: WorkstreamEventKind::from_str(&kind)?,
            role,
            content,
            occurred_at,
            metadata: serde_json::from_str(&metadata)?,
        });
    }
    Ok(events)
}

/// Best-effort update of a session's provenance columns.
pub(crate) fn update_session_provenance(
    conn: &mut Connection,
    session_id: &SessionId,
    provenance: &SessionProvenance,
) -> StoreResult<bool> {
    let changed = conn.execute(
        "UPDATE sessions SET \
             native_session_id = COALESCE(?1, native_session_id), \
             machine_id        = COALESCE(?2, machine_id), \
             git_root          = COALESCE(?3, git_root), \
             git_branch        = COALESCE(?4, git_branch), \
             commit_at_start   = COALESCE(?5, commit_at_start), \
             import_cursor     = COALESCE(?6, import_cursor), \
             source            = ?7 \
         WHERE id = ?8",
        params![
            provenance.native_session_id,
            provenance.machine_id,
            provenance.git_root,
            provenance.git_branch,
            provenance.commit_at_start,
            provenance.import_cursor,
            provenance.source.as_str(),
            session_id.as_bytes(),
        ],
    )?;
    Ok(changed > 0)
}

/// Ensure provenance is present on a session within an existing transaction.
fn ensure_provenance_in_transaction(
    tx: &Transaction<'_>,
    session_id: &[u8],
    provenance: &SessionProvenance,
) -> StoreResult<()> {
    tx.execute(
        "UPDATE sessions SET \
             native_session_id = COALESCE(?1, native_session_id), \
             machine_id        = COALESCE(?2, machine_id), \
             git_root          = COALESCE(?3, git_root), \
             git_branch        = COALESCE(?4, git_branch), \
             commit_at_start   = COALESCE(?5, commit_at_start), \
             import_cursor     = COALESCE(?6, import_cursor), \
             source            = ?7 \
         WHERE id = ?8",
        params![
            provenance.native_session_id,
            provenance.machine_id,
            provenance.git_root,
            provenance.git_branch,
            provenance.commit_at_start,
            provenance.import_cursor,
            provenance.source.as_str(),
            session_id,
        ],
    )?;
    Ok(())
}

/// Search session events globally using full-text search over `session_events_fts`.
pub(crate) fn search_session_events(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> StoreResult<Vec<SessionEventHit>> {
    let fts_query = crate::prepare_fts5_query(query);
    if fts_query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 1000) as i64;
    let mut stmt = conn.prepare(
        "SELECT session_events.session_id, \
                session_events.sequence, \
                session_events.event_id, \
                session_events.kind, \
                session_events.role, \
                session_events.content, \
                snippet(session_events_fts, 0, '<mark>', '</mark>', '…', 24) AS snip, \
                session_events_fts.rank, \
                session_events.occurred_at, \
                session_events.metadata_json, \
                session_events.source, \
                sessions.workspace_id, \
                sessions.project_id, \
                sessions.agent_kind, \
                sessions.machine_id \
         FROM session_events_fts \
         JOIN session_events ON session_events.rowid = session_events_fts.rowid \
         JOIN sessions ON sessions.id = session_events.session_id \
         WHERE session_events_fts MATCH ?1 \
         ORDER BY session_events_fts.rank \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![fts_query, limit], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, f64>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, Option<Vec<u8>>>(11)?,
            row.get::<_, Option<Vec<u8>>>(12)?,
            row.get::<_, String>(13)?,
            row.get::<_, Option<String>>(14)?,
        ))
    })?;

    let mut hits = Vec::new();
    for row in rows {
        let (
            session_id_bytes,
            sequence,
            event_id,
            kind_str,
            role,
            content,
            snippet,
            rank,
            occurred_at,
            metadata_json,
            source_str,
            ws_bytes,
            proj_bytes,
            harness,
            machine_id,
        ) = row?;

        hits.push(SessionEventHit {
            session_id: SessionId::from_slice(&session_id_bytes)?,
            sequence,
            event_id,
            kind: WorkstreamEventKind::from_str(&kind_str)?,
            role,
            content,
            snippet,
            rank,
            occurred_at,
            metadata: serde_json::from_str(&metadata_json)?,
            source: SessionSource::from_wire(&source_str),
            workspace_id: ws_bytes
                .as_deref()
                .map(WorkspaceId::from_slice)
                .transpose()?,
            project_id: proj_bytes
                .as_deref()
                .map(ProjectId::from_slice)
                .transpose()?,
            harness,
            machine_id,
        });
    }
    Ok(hits)
}

/// Search session events scoped to a workspace and project.
pub(crate) fn search_session_events_scoped(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    query: &str,
    limit: usize,
) -> StoreResult<Vec<SessionEventHit>> {
    let fts_query = crate::prepare_fts5_query(query);
    if fts_query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 1000) as i64;
    let mut stmt = conn.prepare(
        "SELECT session_events.session_id, \
                session_events.sequence, \
                session_events.event_id, \
                session_events.kind, \
                session_events.role, \
                session_events.content, \
                snippet(session_events_fts, 0, '<mark>', '</mark>', '…', 24) AS snip, \
                session_events_fts.rank, \
                session_events.occurred_at, \
                session_events.metadata_json, \
                session_events.source, \
                sessions.workspace_id, \
                sessions.project_id, \
                sessions.agent_kind, \
                sessions.machine_id \
         FROM session_events_fts \
         JOIN session_events ON session_events.rowid = session_events_fts.rowid \
         JOIN sessions ON sessions.id = session_events.session_id \
         WHERE session_events_fts MATCH ?1 \
           AND sessions.workspace_id = ?2 \
           AND sessions.project_id = ?3 \
         ORDER BY session_events_fts.rank \
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![
            fts_query,
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            limit
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, f64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Option<Vec<u8>>>(11)?,
                row.get::<_, Option<Vec<u8>>>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, Option<String>>(14)?,
            ))
        },
    )?;

    let mut hits = Vec::new();
    for row in rows {
        let (
            session_id_bytes,
            sequence,
            event_id,
            kind_str,
            role,
            content,
            snippet,
            rank,
            occurred_at,
            metadata_json,
            source_str,
            ws_bytes,
            proj_bytes,
            harness,
            machine_id,
        ) = row?;

        hits.push(SessionEventHit {
            session_id: SessionId::from_slice(&session_id_bytes)?,
            sequence,
            event_id,
            kind: WorkstreamEventKind::from_str(&kind_str)?,
            role,
            content,
            snippet,
            rank,
            occurred_at,
            metadata: serde_json::from_str(&metadata_json)?,
            source: SessionSource::from_wire(&source_str),
            workspace_id: ws_bytes
                .as_deref()
                .map(WorkspaceId::from_slice)
                .transpose()?,
            project_id: proj_bytes
                .as_deref()
                .map(ProjectId::from_slice)
                .transpose()?,
            harness,
            machine_id,
        });
    }
    Ok(hits)
}
