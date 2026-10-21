//! Handoff creation, acceptance, expiry and cancellation.

use super::*;

/// Insert a new handoff in state=open.
pub fn insert_handoff(conn: &mut Connection, h: &NewHandoff) -> StoreResult<HandoffId> {
    let tx = conn.transaction()?;
    let id = insert_handoff_row(&tx, h)?;
    tx.commit()?;
    Ok(id)
}

/// Atomically stamp a session ended and insert its automatic handoff.
///
/// A failed handoff insert rolls the end stamp back, so a keyed retry can run
/// the complete SessionEnd path instead of observing a partially ended session.
pub fn end_session_with_handoff(
    conn: &mut Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
    handoff: &NewHandoff,
) -> StoreResult<HandoffId> {
    if handoff.from_session_id.as_ref() != Some(session_id) {
        return Err(StoreError::InvalidState(
            "automatic handoff source does not match the ended session".into(),
        ));
    }
    let tx = conn.transaction()?;
    let session_scope: Option<(Vec<u8>, Vec<u8>, Option<String>)> = tx
        .query_row(
            "SELECT workspace_id, project_id, actor_user FROM sessions WHERE id = ?1",
            params![session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((workspace_id, project_id, actor_user)) = session_scope else {
        return Err(StoreError::InvalidState(
            "cannot end a missing session with an automatic handoff".into(),
        ));
    };
    if workspace_id.as_slice() != handoff.workspace_id.as_bytes()
        || project_id.as_slice() != handoff.project_id.as_bytes()
    {
        return Err(StoreError::InvalidState(
            "automatic handoff scope does not match the ended session".into(),
        ));
    }
    if actor_user != handoff.owner_user {
        return Err(StoreError::InvalidState(
            "automatic handoff owner does not match the ended session".into(),
        ));
    }
    end_session_row(&tx, session_id, summary_page_id)?;
    let id = insert_handoff_row(&tx, handoff)?;
    tx.commit()?;
    Ok(id)
}

/// Store-boundary bound for a single handoff prose field. Generous
/// (16 KiB, matching the observation body) — a backstop against
/// unbounded content, not a functional cap; callers cap tighter.
pub(super) const HANDOFF_FIELD_MAX_BYTES: usize = 16 * 1024;

/// Store-boundary bound on the number of items in a handoff list field.
pub(super) const HANDOFF_LIST_MAX_ITEMS: usize = 256;

pub(super) fn bound_handoff_field(value: &str) -> String {
    sessionmunch_core::truncate_utf8_bytes(value, HANDOFF_FIELD_MAX_BYTES)
}

pub(super) fn bound_handoff_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .take(HANDOFF_LIST_MAX_ITEMS)
        .map(|s| bound_handoff_field(s))
        .collect()
}

pub(super) fn insert_handoff_row(conn: &Transaction<'_>, h: &NewHandoff) -> StoreResult<HandoffId> {
    validate_identity_storage_key(h.owner_user.as_deref(), "handoff owner")?;
    let id = HandoffId::new();
    let now = Timestamp::now().as_microsecond();
    // Store-boundary bound (defense in depth): the MCP/hook callers already
    // scrub and cap handoff prose, but the store is the last gate before
    // durable persistence — mirror the observation body's bound so a caller
    // that ever forgets cannot write unbounded content to the DB. Generous
    // enough never to fire under the callers' tighter caps.
    let summary = bound_handoff_field(&h.summary);
    let open_q = serde_json::to_string(&bound_handoff_list(&h.open_questions))?;
    let next_s = serde_json::to_string(&bound_handoff_list(&h.next_steps))?;
    let files = serde_json::to_string(&bound_handoff_list(&h.files_touched))?;
    let from_session: Option<&[u8]> = h.from_session_id.as_ref().map(|s| &s.as_bytes()[..]);
    // Normalize the stored cwd: strip trailing path separators (keep a bare root
    // as "/"). The hook extractor preserves whatever the agent payload sent,
    // so this single write point guarantees a consistent stored form for both
    // manual and auto (SessionEnd) handoffs, keeping the next session's
    // path-boundary match robust to trailing slash/backslash drift.
    let cwd: Option<String> = h.cwd.as_ref().map(|p| {
        let s = p.to_string_lossy();
        let trimmed = s.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    });
    let from_agent = h.from_agent.as_str();
    let to_agent = h.to_agent.map(AgentKind::as_str);
    // A newer automatic handoff from the exact same cwd is the only one that
    // can ever win there, even before a SessionStart occurs. Bound abandoned
    // same-directory sessions without touching deliberate manual handoffs or
    // independent parent/sibling cwd scopes — and without crossing an operator
    // boundary: the same directory inside a shared container is the norm, so
    // owner equality is the only thing keeping one operator's SessionEnd from
    // retiring another's pending baton.
    if from_session.is_some() {
        let expired = conn.execute(
            "UPDATE handoffs SET state = 'expired' \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND state = 'open' AND from_session_id IS NOT NULL \
               AND (cwd = ?3 OR (cwd IS NULL AND ?3 IS NULL)) \
               AND owner_user IS ?4",
            params![
                h.workspace_id.as_bytes(),
                h.project_id.as_bytes(),
                cwd,
                h.owner_user.as_deref()
            ],
        )?;
        if expired > 0 {
            audit(
                conn,
                "expire_superseded_handoffs",
                Some(h.workspace_id.as_bytes()),
                Some(h.project_id.as_bytes()),
                None,
                None,
                now,
            )?;
        }
    }
    // Insert + audit atomically. Handoffs are keyed by agent/session, not a DB
    // user, so the audit author is NULL — the row records the lifecycle event
    // (op + workspace/project + time), not an operator identity.
    conn.execute(
        "INSERT INTO handoffs \
         (id, workspace_id, project_id, from_session_id, from_agent, to_agent, cwd, summary, \
          open_questions, next_steps, files_touched, state, created_at, owner_user) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'open', ?12, ?13)",
        params![
            id.as_bytes(),
            h.workspace_id.as_bytes(),
            h.project_id.as_bytes(),
            from_session,
            from_agent,
            to_agent,
            cwd,
            summary,
            open_q,
            next_s,
            files,
            now,
            // NULL owner = shared with the whole project: what a caller with no
            // actor writes, and what every pre-V39 row already looks like.
            h.owner_user.as_deref(),
        ],
    )?;
    audit(
        conn,
        "insert_handoff",
        Some(h.workspace_id.as_bytes()),
        Some(h.project_id.as_bytes()),
        None,
        None,
        now,
    )?;
    Ok(id)
}

/// Mark a handoff accepted by `accepting_agent` / `accepting_session`.
///
/// Returns whether this call is the one that actually claimed it: `false` means
/// the row was already accepted/expired, or its owner does not admit this
/// caller. Callers must not hand the body to the agent on `false`, or a lost
/// race delivers the same baton twice.
///
/// `receiving_cwd` is where the *claiming* session is starting, not where the
/// claimed handoff came from; it bounds the post-claim sweep of stale automatic
/// handoffs.
pub fn accept_handoff(conn: &mut Connection, acceptance: &HandoffAcceptance) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let claimed = accept_handoff_in_transaction(&tx, acceptance)?;
    tx.commit()?;
    Ok(claimed)
}

pub(crate) fn accept_handoff_in_transaction(
    tx: &Transaction<'_>,
    acceptance: &HandoffAcceptance,
) -> StoreResult<bool> {
    let HandoffAcceptance {
        handoff_id,
        workspace_id,
        project_id,
        accepting_agent,
        accepting_session,
        accepting_user,
        owner_filter,
        receiving_cwd,
    } = acceptance;
    let accepting_user = accepting_user.as_deref();
    validate_identity_storage_key(accepting_user, "accepting handoff operator")?;
    match (owner_filter, accepting_user) {
        (OwnerFilter::User(expected), Some(actual)) if expected != actual => {
            return Err(StoreError::InvalidState(
                "accepting handoff operator does not match its owner filter".into(),
            ));
        }
        (OwnerFilter::User(_), None) => {
            return Err(StoreError::InvalidState(
                "an owner-scoped handoff claim requires an accepting operator".into(),
            ));
        }
        (OwnerFilter::Unattributed, Some(_)) => {
            return Err(StoreError::InvalidState(
                "an unattributed handoff claim cannot record a named operator".into(),
            ));
        }
        _ => {}
    }
    let now = Timestamp::now().as_microsecond();
    let agent = accepting_agent.as_str();
    let session: Option<&[u8]> = accepting_session.as_ref().map(|s| &s.as_bytes()[..]);
    if let Some(accepting_session) = accepting_session {
        let receiver: Option<(bool, bool)> = tx
            .query_row(
                "SELECT workspace_id = ?2 AND project_id = ?3 AND agent_kind = ?4, \
                        ended_at IS NULL \
                 FROM sessions WHERE id = ?1",
                params![
                    accepting_session.as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    accepting_agent.as_str(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((matching_scope_and_agent, open)) = receiver else {
            return Err(StoreError::InvalidState(
                "handoff receiver session does not exist".into(),
            ));
        };
        if !matching_scope_and_agent {
            return Err(StoreError::InvalidState(
                "handoff receiver session does not match the accepting scope and agent".into(),
            ));
        }
        if !open {
            return Err(StoreError::InvalidState(
                "an ended session cannot accept a handoff".into(),
            ));
        }
        let already_claimed: bool = tx.query_row(
            "SELECT EXISTS( \
                 SELECT 1 FROM handoffs \
                 WHERE state = 'accepted' AND accepted_by_session = ?1 \
             )",
            params![accepting_session.as_bytes()],
            |row| row.get(0),
        )?;
        if already_claimed {
            // SessionStart can be retried. One receiver must never consume a
            // second baton, or an empty end could return only one and strand
            // the first accepted row.
            return Ok(false);
        }
    }
    let metadata = tx
        .query_row(
            "SELECT from_session_id IS NOT NULL, cwd, created_at, owner_user \
             FROM handoffs \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND state = 'open'",
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, bool>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((automatic, cwd, created_at, owner_user)) = metadata else {
        return Ok(false);
    };
    // The ownership check rides along in the UPDATE's WHERE rather than being a
    // separate read: the claim stays a single atomic compare-and-set (only one
    // racing session can flip 'open' -> 'accepted'), and a caller who is not
    // allowed to take this baton simply changes 0 rows.
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?8)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'accepted', accepted_by = ?1, accepted_at = ?2, \
         accepted_by_session = ?3, accepted_by_user = ?4 \
         WHERE id = ?5 AND workspace_id = ?6 AND project_id = ?7 \
           AND state = 'open'{owner_clause}"
    );
    let changed = match owner_filter {
        OwnerFilter::User(user) => tx.execute(
            &sql,
            params![
                agent,
                now,
                session,
                accepting_user,
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                user
            ],
        )?,
        _ => tx.execute(
            &sql,
            params![
                agent,
                now,
                session,
                accepting_user,
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )?,
    };
    // Only a real state transition ('open' -> 'accepted') is audited.
    if changed > 0 {
        audit(
            tx,
            "accept_handoff",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            now,
        )?;
        if automatic {
            // The sweep inherits the claimed handoff's owner: retiring stale
            // batons must never reach across an operator boundary, or one
            // person's session start silently destroys another's pending
            // handoff — a worse failure than the misdelivery ownership exists
            // to prevent, because nothing is delivered at all.
            let expired = expire_superseded_auto_handoffs(
                tx,
                handoff_id,
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                cwd.as_deref(),
                created_at,
                receiving_cwd.as_deref(),
                owner_user.as_deref(),
            )?;
            if expired > 0 {
                audit(
                    tx,
                    "expire_superseded_handoffs",
                    Some(workspace_id.as_bytes()),
                    Some(project_id.as_bytes()),
                    None,
                    None,
                    now,
                )?;
            }
        }
    }
    Ok(changed > 0)
}

pub(super) fn validate_identity_storage_key(value: Option<&str>, label: &str) -> StoreResult<()> {
    if value.is_some_and(|value| IdentityKey::from_storage_key(value).is_none()) {
        return Err(StoreError::InvalidState(format!(
            "{label} is not a qualified identity storage key"
        )));
    }
    Ok(())
}

/// Expire older automatic handoffs that were eligible for the same receiving
/// cwd as the accepted handoff. Manual and sibling-directory handoffs are
/// deliberately excluded, and so is every handoff belonging to a different
/// operator: a NULL owner is visible to *everyone*, so expiring one on Alice's
/// behalf would take it away from Bob too. Equality on `owner_user` keeps the
/// unattributed single-operator case (every row NULL) behaving exactly as it
/// does without ownership.
#[allow(clippy::too_many_arguments)]
pub(super) fn expire_superseded_auto_handoffs(
    tx: &Transaction<'_>,
    accepted_id: &HandoffId,
    workspace_id: &[u8],
    project_id: &[u8],
    accepted_cwd: Option<&str>,
    accepted_created_at: i64,
    receiving_cwd: Option<&str>,
    accepted_owner: Option<&str>,
) -> StoreResult<usize> {
    let receiving_cwd = receiving_cwd.or(accepted_cwd);
    let accepted_key = crate::reader::handoff_selection_key(
        false,
        accepted_created_at,
        accepted_cwd,
        *accepted_id,
    );
    let mut stmt = tx.prepare(
        "SELECT id, cwd, created_at FROM handoffs \
         WHERE workspace_id = ?1 AND project_id = ?2 \
           AND state = 'open' AND from_session_id IS NOT NULL \
           AND owner_user IS ?3",
    )?;
    let rows = stmt.query_map(params![workspace_id, project_id, accepted_owner], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let (id_bytes, cwd, created_at) = row?;
        let id = HandoffId::from_slice(&id_bytes)?;
        if crate::reader::auto_handoff_matches_cwd(cwd.as_deref(), receiving_cwd)
            && crate::reader::handoff_selection_key(false, created_at, cwd.as_deref(), id)
                < accepted_key
        {
            ids.push(id_bytes);
        }
    }
    drop(stmt);

    let mut expired = 0;
    for chunk in ids.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        expired += tx.execute(
            &format!(
                "UPDATE handoffs SET state = 'expired' \
                 WHERE state = 'open' AND id IN ({placeholders})"
            ),
            rusqlite::params_from_iter(chunk.iter().map(Vec::as_slice)),
        )?;
    }
    Ok(expired)
}

/// Expire every open handoff in one scope, optionally only those older than
/// `older_than_us`. Returns how many rows changed.
///
/// # Why this ignores the automatic expiry's exemptions
///
/// [`expire_superseded_auto_handoffs`] deliberately spares two kinds of open
/// handoff: manual ones (`from_session_id IS NULL`, written by a person on
/// purpose) and ones whose `cwd` does not match the accepting session's. Those
/// exemptions are correct for an automatic sweep triggered by an unrelated
/// accept.
///
/// They also mean the leftover backlog consists, by construction, of exactly
/// the handoffs the sweep will never touch. An operator-driven bulk expiry
/// that honoured the same exemptions would therefore expire nothing at all —
/// it would be a no-op for the only situation it exists to resolve. So this
/// applies to every open handoff in scope, which is what an operator asking to
/// clear a backlog is asking for.
///
/// Owner scoping still applies, and lives inside the `UPDATE` rather than a
/// preceding read, so expiring another user's baton is a zero-row no-op rather
/// than a read-then-write race — the same shape [`cancel_handoff`] uses.
///
/// This is a state change, not a delete: the summary, provenance and
/// timestamps survive. An expired handoff simply stops being consumable.
///
/// # Errors
/// Propagates SQL errors.
pub fn expire_open_handoffs(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    owner_filter: &OwnerFilter,
    older_than_us: Option<i64>,
    author_id: Option<sessionmunch_core::UserId>,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = :owner)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let age_clause = if older_than_us.is_some() {
        " AND created_at <= :cutoff"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'expired' \
         WHERE workspace_id = :ws AND project_id = :proj \
           AND state = 'open'{owner_clause}{age_clause}"
    );

    let ws_bytes: Vec<u8> = workspace_id.as_bytes().to_vec();
    let proj_bytes: Vec<u8> = project_id.as_bytes().to_vec();
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
        (":ws", &ws_bytes as &dyn rusqlite::ToSql),
        (":proj", &proj_bytes as &dyn rusqlite::ToSql),
    ];
    let owner_value;
    if let OwnerFilter::User(user) = owner_filter {
        owner_value = user.clone();
        params.push((":owner", &owner_value as &dyn rusqlite::ToSql));
    }
    let cutoff_value;
    if let Some(cutoff) = older_than_us {
        cutoff_value = cutoff;
        params.push((":cutoff", &cutoff_value as &dyn rusqlite::ToSql));
    }

    let changed = tx.execute(&sql, params.as_slice())? as u64;

    if changed > 0 {
        audit_with_detail(
            &tx,
            "expire_open_handoffs",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
            Timestamp::now().as_microsecond(),
            &format!("{{\"expired\":{changed}}}"),
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Mark an open handoff expired so it will no longer be consumed.
pub fn cancel_handoff(
    conn: &mut Connection,
    handoff_id: &HandoffId,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    owner_filter: &OwnerFilter,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    // Same reasoning as the accept path: the owner predicate lives in the
    // UPDATE so cancelling somebody else's baton is a 0-row no-op instead of a
    // read-then-write race.
    let owner_clause = match owner_filter {
        OwnerFilter::Any => "",
        OwnerFilter::User(_) => " AND (owner_user IS NULL OR owner_user = ?4)",
        OwnerFilter::Unattributed => " AND owner_user IS NULL",
    };
    let sql = format!(
        "UPDATE handoffs SET state = 'expired' \
         WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
           AND state = 'open'{owner_clause}"
    );
    let changed = match owner_filter {
        OwnerFilter::User(user) => tx.execute(
            &sql,
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                user,
            ],
        )?,
        _ => tx.execute(
            &sql,
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )?,
    };
    if changed > 0 {
        audit(
            &tx,
            "cancel_handoff",
            Some(workspace_id.as_bytes()),
            Some(project_id.as_bytes()),
            None,
            None,
            now,
        )?;
    }
    tx.commit()?;
    Ok(changed > 0)
}
