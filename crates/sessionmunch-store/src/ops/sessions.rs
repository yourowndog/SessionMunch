//! Session lifecycle and observation ingest, including hook-session admission.

use super::*;

/// Begin (or re-affirm) a session row keyed on the caller-supplied id.
/// Idempotent: a second call with the same id leaves the row untouched.
pub fn begin_session(conn: &mut Connection, session: &NewSession) -> StoreResult<()> {
    begin_session_row(conn, session)
}

pub(crate) fn begin_session_in_transaction(
    tx: &Transaction<'_>,
    session: &NewSession,
) -> StoreResult<()> {
    begin_session_row(tx, session)?;
    let matches_receiver: bool = tx.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM sessions \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
               AND agent_kind = ?4 AND actor_user IS ?5 \
         )",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
            session.agent_kind.as_str(),
            session.actor_user.as_deref(),
        ],
        |row| row.get(0),
    )?;
    if !matches_receiver {
        return Err(StoreError::InvalidState(
            "startup receiver id belongs to a different scope, agent, or operator".into(),
        ));
    }
    Ok(())
}

/// Whether this session was purged and must not be recreated by late-arriving
/// events. Scoped: a purge in one project does not suppress ingest for a
/// session id in another.
pub(super) fn session_is_purged(conn: &Connection, session: &NewSession) -> StoreResult<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM purged_sessions \
             WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
         )",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
        ],
        |row| row.get::<_, i64>(0),
    )? == 1)
}

pub(super) fn begin_session_row(conn: &Connection, session: &NewSession) -> StoreResult<()> {
    validate_identity_storage_key(session.actor_user.as_deref(), "session owner")?;
    // A purge must be terminal. The events that produced a session can still
    // be sitting undelivered in a client hook spool; without this check the
    // next drain recreates the session row and the observations land again,
    // silently undoing a deletion an application already promised its user
    // (#387). Every ingest path funnels through here, so this is the one
    // place the guard has to live.
    if session_is_purged(conn, session)? {
        return Err(StoreError::SessionPurged(session.id.to_string()));
    }
    let now = Timestamp::now().as_microsecond();
    let agent = session.agent_kind.as_str();
    let cwd: Option<String> = session
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    conn.execute(
        "INSERT INTO sessions \
         (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO NOTHING",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
            agent,
            cwd,
            now,
            session.actor_user.as_deref(),
        ],
    )?;
    Ok(())
}

/// Stamp a session as ended, optionally linking the synthesised summary page,
/// and record the observation generation covered by this end.
pub fn end_session(
    conn: &mut Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
) -> StoreResult<()> {
    end_session_row(conn, session_id, summary_page_id)
}

/// Atomically end a lifecycle-only session and return its startup handoff to
/// the open pool.
///
/// Only a handoff claimed by this exact receiver session is reopened. The
/// compare-and-set protects a baton that has moved to any other state, while
/// clearing all acceptance metadata makes a later claim indistinguishable
/// from the original open row.
pub fn end_lifecycle_only_session(
    conn: &mut Connection,
    session_id: &SessionId,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    let tx = conn.transaction()?;
    let outcome = end_lifecycle_only_session_in_tx(&tx, session_id)?;
    tx.commit()?;
    Ok(outcome)
}

pub(super) fn end_lifecycle_only_session_in_tx(
    tx: &Transaction<'_>,
    session_id: &SessionId,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    // Substantive is defined POSITIVELY: at least one observation that is
    // real work (a user prompt or a tool use). This MUST stay in sync with
    // `is_ephemeral_session` in sessionmunch-hooks/src/router.rs — both sides
    // must classify every observation set identically, or this atomic
    // re-check would silently revert the router's verdict. Everything else,
    // including `stop` and `notification` (some harnesses, e.g. OpenCode,
    // fire these for purely internal work), is ephemeral.
    let has_substantive_observation: bool = tx.query_row(
        "SELECT EXISTS( \
             SELECT 1 FROM observations \
             WHERE session_id = ?1 \
               AND kind IN ('user-prompt', 'pre-tool-use', 'post-tool-use') \
         )",
        params![session_id.as_bytes()],
        |row| row.get(0),
    )?;
    if has_substantive_observation {
        return Ok(LifecycleOnlyEndOutcome::Substantive);
    }
    end_session_row(tx, session_id, None)?;
    let reopened: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "UPDATE handoffs \
             SET state = 'open', accepted_by = NULL, accepted_at = NULL, \
                 accepted_by_session = NULL, accepted_by_user = NULL \
             WHERE id = ( \
                 SELECT id FROM handoffs \
                 WHERE state = 'accepted' AND accepted_by_session = ?1 \
                 ORDER BY accepted_at DESC, created_at DESC LIMIT 1 \
             ) AND state = 'accepted' AND accepted_by_session = ?1 \
             RETURNING id, workspace_id, project_id",
            params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let reopened = reopened
        .map(|(id, workspace_id, project_id)| -> StoreResult<HandoffId> {
            let handoff_id = HandoffId::from_slice(&id)?;
            let workspace_id = WorkspaceId::from_slice(&workspace_id)?;
            let project_id = ProjectId::from_slice(&project_id)?;
            audit(
                tx,
                "release_lifecycle_only_handoff",
                Some(workspace_id.as_bytes()),
                Some(project_id.as_bytes()),
                None,
                None,
                Timestamp::now().as_microsecond(),
            )?;
            Ok(handoff_id)
        })
        .transpose()?;
    Ok(LifecycleOnlyEndOutcome::Ended {
        reopened_handoff: reopened,
    })
}

pub(super) fn end_session_row(
    conn: &Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let page_blob: Option<&[u8]> = summary_page_id.map(|p| &p.as_bytes()[..]);
    conn.execute(
        "UPDATE sessions \
         SET ended_at = ?1, summary_page_id = ?2, \
             ended_observation_count = (\
                 SELECT COUNT(*) FROM observations WHERE session_id = ?3\
             ) \
         WHERE id = ?3",
        params![now, page_blob, session_id.as_bytes()],
    )?;
    Ok(())
}

/// Append a single observation. Caller is expected to have already
/// inserted the parent session via [`begin_session`].
pub fn insert_observation(
    conn: &mut Connection,
    obs: &NewObservation,
) -> StoreResult<ObservationId> {
    let tx = conn.transaction()?;
    let id = insert_observation_row(&tx, obs)?;
    tx.commit()?;
    Ok(id)
}

/// Seed an observation on a schema older than V65, before the session-event
/// ledger exists. Upgrade tests use it so the V66 backfill has history to
/// carry forward.
#[cfg(test)]
pub(crate) fn insert_observation_unmirrored(
    conn: &mut Connection,
    obs: &NewObservation,
) -> StoreResult<ObservationId> {
    let id = ObservationId::new();
    conn.execute(
        "INSERT INTO observations (id, session_id, workspace_id, project_id, kind, title, body, \
          importance, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id.as_bytes(),
            obs.session_id.as_bytes(),
            obs.workspace_id.as_bytes(),
            obs.project_id.as_bytes(),
            observation_kind_as_str(obs.kind),
            obs.title,
            obs.body,
            i64::from(obs.importance.clamp(1, 10)),
            Timestamp::now().as_microsecond(),
        ],
    )?;
    Ok(id)
}

/// Ingest-keys older than this are swept opportunistically on every keyed
/// insert. Keys only need to outlive the client spool (7 days + retry
/// backoff); 30 days is a generous margin.
pub(super) const INGEST_KEY_TTL_MICROS: i64 = 30 * 24 * 60 * 60 * 1_000_000;

/// Claim a project-scoped ingest key and append its observation atomically.
///
/// The key claim and the observation row commit in ONE transaction: either
/// both land or neither does, so a crash cannot claim a key without its
/// observation. A claimed-but-incomplete key tells the ingest path to resume
/// downstream wiki/handoff effects without inserting another observation. A
/// completed key means the whole event can be acknowledged and skipped.
pub fn insert_observation_keyed(
    conn: &mut Connection,
    obs: &NewObservation,
    ingest_key: &str,
) -> StoreResult<IngestObservationOutcome> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;

    // Sweep before looking up the current key so an expired key can be reused
    // even when no unrelated keyed event arrived in the meantime.
    tx.execute(
        "DELETE FROM ingest_keys WHERE seen_at < ?1",
        params![now - INGEST_KEY_TTL_MICROS],
    )?;
    let existing: Option<Option<i64>> = tx
        .query_row(
            "SELECT completed_at FROM ingest_keys \
             WHERE project_id = ?1 AND key = ?2",
            params![obs.project_id.as_bytes(), ingest_key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(completed_at) = existing {
        tx.commit()?;
        return Ok(if completed_at.is_some() {
            IngestObservationOutcome::AlreadyComplete
        } else {
            IngestObservationOutcome::ResumePending
        });
    }

    tx.execute(
        "INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) \
         VALUES (?1, ?2, ?3, NULL)",
        params![obs.project_id.as_bytes(), ingest_key, now],
    )?;
    let id = insert_observation_row(&tx, obs)?;
    tx.commit()?;
    Ok(IngestObservationOutcome::Inserted(id))
}

/// Find or create the hook session, validate its immutable tuple and owner,
/// optionally claim an ingest key, and append the observation in one writer
/// transaction. Validation always precedes key mutation.
pub fn admit_hook_session_event(
    conn: &mut Connection,
    session: &NewSession,
    obs: &NewObservation,
    owner_filter: &OwnerFilter,
    ingest_key: Option<&str>,
    machine_id: Option<&str>,
) -> StoreResult<HookSessionAdmission> {
    if obs.session_id != session.id
        || obs.workspace_id != session.workspace_id
        || obs.project_id != session.project_id
    {
        return Err(StoreError::InvalidState(
            "hook observation does not match its session tuple".into(),
        ));
    }
    let session_end = obs.kind == ObservationKind::SessionEnd;
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    type Row = (Vec<u8>, Vec<u8>, String, Option<String>, Option<i64>, u64);
    let existing: Option<Row> = tx.query_row(
        "SELECT workspace_id, project_id, agent_kind, actor_user, ended_at, ended_observation_count FROM sessions WHERE id = ?1",
        params![session.id.as_bytes()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    ).optional()?;
    let (owner, ended_at, ended_count) = match existing {
        Some((ws, project, agent, owner, ended_at, ended_count)) => {
            // Corrupt owners fail closed, including for Any recovery. Owner
            // and agent identify WHO the session belongs to, so a mismatch
            // there is a genuine UUID collision and is terminal.
            if owner
                .as_deref()
                .is_some_and(|value| IdentityKey::from_storage_key(value).is_none())
                || agent != session.agent_kind.as_str()
                || !owner_filter.admits(owner.as_deref())
            {
                return Err(StoreError::SessionCollision);
            }
            // Scope is NOT identity. Under the default `follow-cwd` routing a
            // mid-session `cd` into another project legitimately resolves this
            // event to a different (workspace, project) than the session row,
            // and the observation belongs in the project it names — that is
            // exactly the record-splitting `[routing] mid_session = "sticky"`
            // exists to opt out of. Treating the difference as a collision
            // silently DROPPED those events instead of recording them.
            //
            // A terminal event is the one exception: an end naming a different
            // scope is not this session's end, so it is dropped rather than
            // ending someone else's session (the pre-guard
            // `SessionEndDisposition::DropInvalid` arm).
            let scoped_to_session = ws.as_slice() == session.workspace_id.as_bytes()
                && project.as_slice() == session.project_id.as_bytes();
            if session_end && !scoped_to_session {
                tx.commit()?;
                return Ok(HookSessionAdmission::InvalidScopedEnd);
            }
            (owner, ended_at, ended_count)
        }
        None if session_end => {
            tx.commit()?;
            return Ok(HookSessionAdmission::InvalidMissingEnd);
        }
        None => {
            validate_identity_storage_key(session.actor_user.as_deref(), "session owner")?;
            if !owner_filter.admits(session.actor_user.as_deref()) {
                return Err(StoreError::SessionCollision);
            }
            tx.execute(
                "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![session.id.as_bytes(), session.workspace_id.as_bytes(), session.project_id.as_bytes(), session.agent_kind.as_str(), session.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()), now, session.actor_user.as_deref()],
            )?;
            (session.actor_user.clone(), None, 0)
        }
    };
    // Best-effort provenance: the first event that names its machine fixes
    // it. A later event never rewrites it, so a moved session keeps the
    // machine it started on.
    if let Some(machine_id) = machine_id {
        tx.execute(
            "UPDATE sessions SET machine_id = ?2 WHERE id = ?1 AND machine_id IS NULL",
            params![session.id.as_bytes(), machine_id],
        )?;
    }
    let guard = AdmittedSession {
        session_id: session.id,
        workspace_id: session.workspace_id,
        project_id: session.project_id,
        agent_kind: session.agent_kind,
        owner,
    };
    if session_end && ended_at.is_some() {
        let count: u64 = tx.query_row(
            "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
            params![session.id.as_bytes()],
            |r| r.get(0),
        )?;
        if count <= ended_count {
            tx.commit()?;
            return Ok(HookSessionAdmission::AlreadyEnded { session: guard });
        }
    }
    let ingest = if let Some(key) = ingest_key {
        tx.execute(
            "DELETE FROM ingest_keys WHERE seen_at < ?1",
            params![now - INGEST_KEY_TTL_MICROS],
        )?;
        let old: Option<Option<i64>> = tx
            .query_row(
                "SELECT completed_at FROM ingest_keys WHERE project_id = ?1 AND key = ?2",
                params![obs.project_id.as_bytes(), key],
                |r| r.get(0),
            )
            .optional()?;
        match old {
            Some(done) => {
                if done.is_some() {
                    IngestObservationOutcome::AlreadyComplete
                } else {
                    IngestObservationOutcome::ResumePending
                }
            }
            None => {
                tx.execute("INSERT INTO ingest_keys (project_id, key, seen_at, completed_at) VALUES (?1, ?2, ?3, NULL)", params![obs.project_id.as_bytes(), key, now])?;
                IngestObservationOutcome::Inserted(insert_observation_row(&tx, obs)?)
            }
        }
    } else {
        IngestObservationOutcome::Inserted(insert_observation_row(&tx, obs)?)
    };
    tx.commit()?;
    if !session_end {
        Ok(HookSessionAdmission::Observation {
            session: guard,
            ingest,
        })
    } else if ended_at.is_some() {
        Ok(HookSessionAdmission::ReEnd {
            session: guard,
            ingest,
        })
    } else {
        Ok(HookSessionAdmission::EndOpen {
            session: guard,
            ingest,
        })
    }
}

pub(super) fn validate_admitted_session(
    tx: &Transaction<'_>,
    admitted: &AdmittedSession,
) -> StoreResult<()> {
    let owner: Option<String> = tx.query_row(
        "SELECT actor_user FROM sessions WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND agent_kind = ?4",
        params![admitted.session_id.as_bytes(), admitted.workspace_id.as_bytes(), admitted.project_id.as_bytes(), admitted.agent_kind.as_str()],
        |r| r.get(0),
    ).optional()?.ok_or(StoreError::SessionCollision)?;
    if owner != admitted.owner
        || owner
            .as_deref()
            .is_some_and(|v| IdentityKey::from_storage_key(v).is_none())
    {
        return Err(StoreError::SessionCollision);
    }
    Ok(())
}

/// Guarded hook equivalent of [`end_session`].
pub fn end_admitted_session(
    conn: &mut Connection,
    admitted: &AdmittedSession,
    page: Option<&PageId>,
) -> StoreResult<()> {
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    end_session_row(&tx, &admitted.session_id, page)?;
    tx.commit()?;
    Ok(())
}

/// Guarded hook lifecycle-only end.
pub fn end_admitted_lifecycle_only_session(
    conn: &mut Connection,
    admitted: &AdmittedSession,
) -> StoreResult<LifecycleOnlyEndOutcome> {
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    let outcome = end_lifecycle_only_session_in_tx(&tx, &admitted.session_id)?;
    tx.commit()?;
    Ok(outcome)
}

/// Guarded hook end plus automatic handoff in one transaction.
pub fn end_admitted_session_with_handoff(
    conn: &mut Connection,
    admitted: &AdmittedSession,
    page: Option<&PageId>,
    handoff: &NewHandoff,
) -> StoreResult<HandoffId> {
    if handoff.from_session_id != Some(admitted.session_id)
        || handoff.workspace_id != admitted.workspace_id
        || handoff.project_id != admitted.project_id
        || handoff.owner_user != admitted.owner
    {
        return Err(StoreError::SessionCollision);
    }
    let tx = conn.transaction()?;
    validate_admitted_session(&tx, admitted)?;
    end_session_row(&tx, &admitted.session_id, page)?;
    let id = insert_handoff_row(&tx, handoff)?;
    tx.commit()?;
    Ok(id)
}

/// Mark a keyed hook event complete after its downstream effects finish.
///
/// This transition is idempotent so two resumed processors can converge on the
/// same completed key without turning a successful delivery into an error.
pub fn complete_observation_ingest(
    conn: &mut Connection,
    project_id: &ProjectId,
    ingest_key: &str,
) -> StoreResult<()> {
    if !complete_observation_ingest_if_claimed(conn, project_id, ingest_key)? {
        return Err(StoreError::InvalidState(
            "cannot complete an ingest key that was not claimed".into(),
        ));
    }
    Ok(())
}

/// Mark a keyed hook event complete when its observation claim exists.
///
/// Recovery paths use the boolean result to distinguish a pending native-hook
/// replay from an unkeyed or unrelated duplicate SessionEnd.
pub fn complete_observation_ingest_if_claimed(
    conn: &mut Connection,
    project_id: &ProjectId,
    ingest_key: &str,
) -> StoreResult<bool> {
    let completed_at = Timestamp::now().as_microsecond();
    let matched = conn.execute(
        "UPDATE ingest_keys \
         SET completed_at = COALESCE(completed_at, ?1) \
         WHERE project_id = ?2 AND key = ?3",
        params![completed_at, project_id.as_bytes(), ingest_key],
    )?;
    Ok(matched != 0)
}

/// The observation INSERT itself, shared by the plain and keyed paths
/// (`&Connection` so it also runs inside a [`rusqlite::Transaction`]). Every
/// caller holds a transaction, so the session-ledger mirror commits with it.
pub(super) fn insert_observation_row(
    conn: &Connection,
    obs: &NewObservation,
) -> StoreResult<ObservationId> {
    let id = ObservationId::new();
    let now = Timestamp::now().as_microsecond();
    let kind = observation_kind_as_str(obs.kind);
    let importance: i64 = i64::from(obs.importance.clamp(1, 10));
    let (extension, source_event) = match (&obs.extension, &obs.source_event) {
        (Some(extension), Some(source_event)) => {
            (Some(extension.as_str()), Some(source_event.as_str()))
        }
        _ => (None, None),
    };
    conn.execute(
        "INSERT INTO observations \
         (id, session_id, workspace_id, project_id, kind, extension, source_event, title, body, \
          importance, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.as_bytes(),
            obs.session_id.as_bytes(),
            obs.workspace_id.as_bytes(),
            obs.project_id.as_bytes(),
            kind,
            extension,
            source_event,
            obs.title,
            obs.body,
            importance,
            now,
        ],
    )?;
    crate::session_events::mirror_observation(conn, &id, obs, now)?;
    Ok(id)
}
