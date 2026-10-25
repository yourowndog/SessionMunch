//! Session-owned event ledger and provenance store operations.
//!
//! Unlike the managed `workstream_events` (keyed by a workstream id and driven
//! only through `sessionmunch run`), the rows here belong to a real `sessions`
//! row so hook capture, transcript import, and managed runs converge on one
//! session identity. Event insertion is idempotent on the stable
//! source-derived `event_id`, and provenance is written best-effort.

use std::str::FromStr as _;

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use sessionmunch_core::{
    NewObservation, NewSession, NewSessionEvent, ObservationId, ObservationKind, OwnerFilter,
    ProjectId, SessionEvent, SessionEventHit, SessionEventSearch, SessionId, SessionProvenance,
    SessionSource, WorkspaceId, WorkstreamEventKind,
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
    let existing: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT agent_kind, actor_user FROM sessions WHERE id = ?1",
            params![session.id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let created = existing.is_none();
    match existing {
        // Scope is not identity: an import that guessed a different project
        // for a session the hooks already recorded attaches to that session
        // (where its hook history lives) instead of failing the batch. The
        // harness and a named owner are identity; a mismatch is a collision.
        Some((agent, owner)) => {
            let owner_conflicts = session
                .actor_user
                .as_deref()
                .is_some_and(|wanted| owner.as_deref() != Some(wanted));
            if agent != session.agent_kind.as_str() || owner_conflicts {
                return Err(crate::StoreError::SessionCollision);
            }
        }
        // Ensure the base session row exists first (idempotent, carries the
        // purge/sanitize guards on the existing path).
        None => crate::ops::begin_session_in_transaction(&tx, session)?,
    }
    ensure_provenance_in_transaction(&tx, session.id.as_bytes(), provenance, created)?;
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

/// Ledger shape of a hook observation: the event family, the message role
/// where one applies, and the stable event id `obs:<observation id>`.
pub(crate) fn observation_event_kind(
    kind: ObservationKind,
) -> (WorkstreamEventKind, Option<&'static str>) {
    match kind {
        ObservationKind::UserPrompt => (WorkstreamEventKind::Message, Some("user")),
        ObservationKind::Stop => (WorkstreamEventKind::Message, Some("assistant")),
        ObservationKind::PreToolUse => (WorkstreamEventKind::ToolCall, None),
        ObservationKind::PostToolUse => (WorkstreamEventKind::ToolResult, None),
        ObservationKind::PreCompact | ObservationKind::PostCompaction => {
            (WorkstreamEventKind::Compaction, None)
        }
        ObservationKind::SessionStart
        | ObservationKind::SessionEnd
        | ObservationKind::Notification
        | ObservationKind::Other => (WorkstreamEventKind::Annotation, None),
    }
}

/// Mirror one just-inserted hook observation into its session's event
/// ledger, so hook capture is searchable history and not only consolidation
/// input. Runs on the caller's transaction (index-with-data), and is
/// idempotent on `obs:<id>` like every other ledger append. `occurred_at`
/// is when the event happened (the client's capture time when known);
/// `created_at` is when this server stored it.
pub(crate) fn mirror_observation(
    conn: &Connection,
    id: &ObservationId,
    obs: &NewObservation,
    occurred_at_micros: i64,
    created_at_micros: i64,
) -> StoreResult<()> {
    let (kind, role) = observation_event_kind(obs.kind);
    // Prompt hooks title the observation with the prompt itself; keep one copy.
    let content = match (obs.title.trim(), obs.body.trim()) {
        (title, "") => title.to_owned(),
        (title, body) if title.is_empty() || body.starts_with(title) => body.to_owned(),
        (title, body) => format!("{title}\n{body}"),
    };
    let content = sessionmunch_core::truncate_utf8_bytes(&content, SESSION_EVENT_CONTENT_MAX_BYTES);
    let occurred_at = Timestamp::from_microsecond(occurred_at_micros)
        .ok()
        .map(|at| at.to_string());
    let metadata = serde_json::json!({
        "observation_kind": obs.kind.as_str(),
        "extension": obs.extension,
        "source_event": obs.source_event,
    });
    conn.execute(
        "INSERT OR IGNORE INTO session_events( \
             session_id, sequence, event_id, kind, role, content, \
             occurred_at, metadata_json, source, created_at \
         ) VALUES ( \
             ?1, \
             (SELECT COALESCE(MAX(sequence), 0) + 1 FROM session_events WHERE session_id = ?1), \
             ?2, ?3, ?4, ?5, ?6, ?7, 'hook', ?8 \
         )",
        params![
            obs.session_id.as_bytes(),
            format!("obs:{id}"),
            kind.as_str(),
            role,
            content,
            occurred_at,
            serde_json::to_string(&metadata)?,
            created_at_micros,
        ],
    )?;
    Ok(())
}

/// Read the newest `limit` events for a session, newest first.
pub(crate) fn list_session_events(
    conn: &Connection,
    session_id: &SessionId,
    limit: usize,
) -> StoreResult<Vec<SessionEvent>> {
    query_session_events(conn, session_id, limit.clamp(1, 1000), false)
}

/// Most transcript events (imported or managed, not hook mirrors) one read
/// returns. Content is bounded per event, so this bounds the read.
pub const MAX_TRANSCRIPT_EVENTS: usize = 5_000;

/// The session's transcript events, newest first, at most `limit` (capped at
/// [`MAX_TRANSCRIPT_EVENTS`]). Hook mirrors are excluded: they repeat what an
/// imported transcript of the same session already holds.
pub(crate) fn list_transcript_events(
    conn: &Connection,
    session_id: &SessionId,
    limit: usize,
) -> StoreResult<Vec<SessionEvent>> {
    query_session_events(
        conn,
        session_id,
        limit.clamp(1, MAX_TRANSCRIPT_EVENTS),
        true,
    )
}

fn query_session_events(
    conn: &Connection,
    session_id: &SessionId,
    limit: usize,
    transcript_only: bool,
) -> StoreResult<Vec<SessionEvent>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let filter = if transcript_only {
        "AND source != 'hook' "
    } else {
        ""
    };
    let mut statement = conn.prepare(&format!(
        "SELECT sequence, event_id, kind, role, content, occurred_at, metadata_json \
         FROM session_events WHERE session_id = ?1 {filter}\
         ORDER BY sequence DESC LIMIT ?2"
    ))?;
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
    created: bool,
) -> StoreResult<()> {
    // Identity provenance is first-writer-wins: a second machine importing a
    // copied transcript store must not relabel where the session ran.
    tx.execute(
        "UPDATE sessions SET \
             native_session_id = COALESCE(native_session_id, ?1), \
             machine_id        = COALESCE(machine_id, ?2), \
             git_root          = COALESCE(git_root, ?3), \
             git_branch        = COALESCE(git_branch, ?4), \
             commit_at_start   = COALESCE(commit_at_start, ?5), \
             import_cursor     = COALESCE(?6, import_cursor), \
             source            = CASE WHEN ?9 THEN ?7 ELSE source END \
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
            created,
        ],
    )?;
    Ok(())
}

/// Lexical search over the session event ledger with optional scope,
/// machine, harness, and time filters. Ordered by BM25 relevance, or newest
/// first when asked ("when did we last run X").
pub(crate) fn search_session_events(
    conn: &Connection,
    search: &SessionEventSearch,
) -> StoreResult<Vec<SessionEventHit>> {
    let fts_query = crate::prepare_fts5_query(&search.query);
    if fts_query.is_empty() || search.limit == 0 {
        return Ok(Vec::new());
    }
    let limit = search.limit.clamp(1, 1000) as i64;
    let (scope_ws, scope_proj) = match &search.scope {
        Some((ws, proj)) => (Some(ws.as_bytes().to_vec()), Some(proj.as_bytes().to_vec())),
        None => (None, None),
    };
    let order = if search.newest_first {
        "session_events.created_at DESC, session_events.sequence DESC"
    } else {
        "session_events_fts.rank"
    };
    let sql = format!(
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
                sessions.machine_id, \
                workspaces.name, \
                projects.name, \
                sessions.cwd, \
                session_events.created_at \
         FROM session_events_fts \
         JOIN session_events ON session_events.rowid = session_events_fts.rowid \
         JOIN sessions ON sessions.id = session_events.session_id \
         LEFT JOIN workspaces ON workspaces.id = sessions.workspace_id \
         LEFT JOIN projects ON projects.id = sessions.project_id \
         WHERE session_events_fts MATCH ?1 \
           AND (?2 IS NULL OR (sessions.workspace_id = ?2 AND sessions.project_id = ?3)) \
           AND (?4 IS NULL OR COALESCE(json_extract(session_events.metadata_json, '$.machine'), \
                sessions.machine_id) = ?4 COLLATE NOCASE) \
           AND (?5 IS NULL OR sessions.agent_kind = ?5 COLLATE NOCASE) \
           AND (?6 IS NULL OR session_events.created_at >= ?6) \
           AND (?7 IS NULL OR session_events.created_at < ?7) \
           AND (?9 = 0 OR sessions.actor_user IS NULL \
                OR (?9 = 1 AND sessions.actor_user = ?10)) \
         ORDER BY {order} \
         LIMIT ?8"
    );
    let (owner_mode, owner_key): (i64, Option<&str>) = match &search.owner {
        None | Some(OwnerFilter::Any) => (0, None),
        Some(OwnerFilter::User(key)) => (1, Some(key.as_str())),
        Some(OwnerFilter::Unattributed) => (2, None),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            fts_query,
            scope_ws,
            scope_proj,
            search.machine,
            search.harness,
            search.since_micros,
            search.until_micros,
            limit,
            owner_mode,
            owner_key,
        ],
        |row| {
            Ok(RawHit {
                session_id: row.get(0)?,
                sequence: row.get(1)?,
                event_id: row.get(2)?,
                kind: row.get(3)?,
                role: row.get(4)?,
                content: row.get(5)?,
                snippet: row.get(6)?,
                rank: row.get(7)?,
                occurred_at: row.get(8)?,
                metadata_json: row.get(9)?,
                source: row.get(10)?,
                workspace_id: row.get(11)?,
                project_id: row.get(12)?,
                harness: row.get(13)?,
                machine_id: row.get(14)?,
                workspace: row.get(15)?,
                project: row.get(16)?,
                cwd: row.get(17)?,
                created_at: row.get(18)?,
            })
        },
    )?;
    let mut hits = Vec::new();
    for row in rows {
        let raw = row?;
        hits.push(SessionEventHit {
            session_id: SessionId::from_slice(&raw.session_id)?,
            sequence: raw.sequence,
            event_id: raw.event_id,
            kind: WorkstreamEventKind::from_str(&raw.kind)?,
            role: raw.role,
            content: raw.content,
            snippet: raw.snippet,
            rank: raw.rank,
            occurred_at: raw.occurred_at,
            metadata: serde_json::from_str(&raw.metadata_json)?,
            source: SessionSource::from_wire(&raw.source),
            workspace_id: raw
                .workspace_id
                .as_deref()
                .map(WorkspaceId::from_slice)
                .transpose()?,
            project_id: raw
                .project_id
                .as_deref()
                .map(ProjectId::from_slice)
                .transpose()?,
            harness: raw.harness,
            machine_id: raw.machine_id,
            workspace: raw.workspace,
            project: raw.project,
            cwd: raw.cwd,
            created_at: raw.created_at,
        });
    }
    Ok(hits)
}

/// One search row before id/enum parsing.
struct RawHit {
    session_id: Vec<u8>,
    sequence: i64,
    event_id: String,
    kind: String,
    role: Option<String>,
    content: String,
    snippet: String,
    rank: f64,
    occurred_at: Option<String>,
    metadata_json: String,
    source: String,
    workspace_id: Option<Vec<u8>>,
    project_id: Option<Vec<u8>>,
    harness: String,
    machine_id: Option<String>,
    workspace: Option<String>,
    project: Option<String>,
    cwd: Option<String>,
    created_at: i64,
}

/// Replace a session's cached summary-job answers with `jobs`
/// (`(job_id, model, fields_json)`): rows for jobs the session no longer
/// uses go, in the same transaction.
pub(crate) fn replace_summary_jobs(
    conn: &mut Connection,
    session_id: &SessionId,
    jobs: &[(String, String, String)],
) -> StoreResult<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM session_summary_jobs WHERE session_id = ?1",
        params![session_id.as_bytes()],
    )?;
    let now = Timestamp::now().as_microsecond();
    for (job_id, model, fields_json) in jobs {
        tx.execute(
            "INSERT INTO session_summary_jobs (session_id, job_id, model, fields_json, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id.as_bytes(), job_id, model, fields_json, now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// A session's cached summary-job answers as `(job_id, model, fields_json)`.
pub(crate) fn summary_jobs(
    conn: &Connection,
    session_id: &SessionId,
) -> StoreResult<Vec<(String, String, String)>> {
    let mut statement = conn.prepare(
        "SELECT job_id, model, fields_json FROM session_summary_jobs \
         WHERE session_id = ?1 ORDER BY job_id",
    )?;
    let rows = statement.query_map(params![session_id.as_bytes()], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}
