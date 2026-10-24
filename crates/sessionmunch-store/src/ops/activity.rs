//! Client activity, page access counts and page feedback.

use super::*;

/// Fold per-client MCP tool-call deltas into their `(client, day)`
/// buckets. UPSERT per bucket inside one transaction: the buffer layer
/// in the MCP server coalesces a minute of calls into a handful of
/// entries, so this stays a few tiny rows per flush regardless of
/// traffic.
pub(crate) fn bump_client_activity(
    conn: &mut Connection,
    entries: &[(String, i64, u32, u32)],
) -> StoreResult<()> {
    if entries.is_empty() {
        return Ok(());
    }
    for (client, _, _, _) in entries {
        if !is_normalized_client_activity_label(client) {
            return Err(StoreError::InvalidState(
                "client activity label is not normalized".into(),
            ));
        }
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO client_activity (client, day, reads, writes) \
             VALUES ( \
                 CASE \
                     WHEN ?1 = ?5 \
                       OR EXISTS ( \
                           SELECT 1 FROM client_activity \
                           WHERE client = ?1 AND day = ?2 \
                       ) \
                       OR ( \
                           SELECT COUNT(*) FROM client_activity \
                           WHERE day = ?2 AND client <> ?5 \
                       ) < ?6 \
                     THEN ?1 \
                     ELSE ?5 \
                 END, \
                 ?2, ?3, ?4 \
             ) \
             ON CONFLICT(client, day) DO UPDATE SET \
                 reads = CASE \
                     WHEN reads > 9223372036854775807 - excluded.reads \
                     THEN 9223372036854775807 \
                     ELSE reads + excluded.reads \
                 END, \
                 writes = CASE \
                     WHEN writes > 9223372036854775807 - excluded.writes \
                     THEN 9223372036854775807 \
                     ELSE writes + excluded.writes \
                 END",
        )?;
        for (client, day, reads, writes) in entries {
            if *reads == 0 && *writes == 0 {
                continue;
            }
            stmt.execute(params![
                client,
                day,
                reads,
                writes,
                crate::CLIENT_ACTIVITY_OVERFLOW_CLIENT,
                crate::CLIENT_ACTIVITY_MAX_CLIENTS_PER_DAY,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn is_normalized_client_activity_label(client: &str) -> bool {
    if client.is_empty()
        || client != client.trim()
        || client.chars().count() > crate::CLIENT_ACTIVITY_MAX_NAME_CHARS
        || client.chars().any(|c| c.is_control() || is_bidi_control(c))
    {
        return false;
    }

    let mut previous_space = false;
    for c in client.chars() {
        if c.is_whitespace() {
            if c != ' ' || previous_space {
                return false;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
    }
    true
}

pub(super) fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Bump `access_count` + `last_accessed_at` for the pages whose ids
/// appear in `page_ids`. Idempotent for unknown ids (no-op).
/// Used by the read path to feed the M8 reinforcement term.
///
/// Bump shared access counters and record an optional typed operator as a
/// distinct reader. Both mutations commit in the same transaction.
pub fn bump_access_for_pages_for_actor(
    conn: &mut Connection,
    page_ids: &[PageId],
    actor: Option<&IdentityKey>,
) -> StoreResult<()> {
    if page_ids.is_empty() {
        return Ok(());
    }
    if actor.is_some_and(|identity| !identity.is_valid()) {
        return Err(StoreError::InvalidState(
            "page access actor is not a normalized identity".into(),
        ));
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE pages \
             SET access_count = access_count + 1, last_accessed_at = ?1 \
             WHERE id = ?2 AND is_latest = 1",
        )?;
        for id in page_ids {
            stmt.execute(params![now, id.as_bytes()])?;
        }
        // Per-operator reinforcement, recorded in the SAME transaction so the
        // scalar and the breakdown cannot drift. Purely additive: the scalar
        // above is what the retention formula still reads by default.
        if let Some(actor) = actor {
            let actor = actor.storage_key();
            // The `WHERE EXISTS` guard is what keeps the documented idempotence
            // for unknown ids: `page_access.page_id` REFERENCES `pages(id)` and
            // the writer connection runs with `foreign_keys` ON, so a bare
            // INSERT for a page deleted between the search and this (detached,
            // post-response) bump would raise a FK violation and roll back the
            // WHOLE batch — costing every other page in the result set its
            // reinforcement. `INSERT OR IGNORE` does not help: it suppresses
            // constraint conflicts, not FK violations. The predicate mirrors the
            // UPDATE above so the scalar and the breakdown cannot drift.
            let mut per_actor = tx.prepare(
                "INSERT INTO page_access (page_id, actor) \
                 SELECT ?1, ?2 \
                 WHERE EXISTS (SELECT 1 FROM pages WHERE id = ?1 AND is_latest = 1) \
                 ON CONFLICT(page_id, actor) DO NOTHING",
            )?;
            for id in page_ids {
                per_actor.execute(params![id.as_bytes(), actor])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Record one explicit feedback signal against the latest version of a
/// page and update its derived salience, in a single transaction.
///
/// `page_feedback` is the append-only source of truth; `pages.salience`
/// is the derived value the decay formula reads. Returns the page id and
/// the salience after the update, or `None` when the path has no latest
/// version in that scope (idempotent no-op for a deleted page).
#[allow(clippy::too_many_arguments)]
pub fn record_page_feedback(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
    kind: sessionmunch_core::FeedbackKind,
    reason: Option<&str>,
    author_id: Option<sessionmunch_core::UserId>,
    params: &crate::decay::DecayParams,
) -> StoreResult<Option<(PageId, f64)>> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let existing: Option<(Vec<u8>, Option<f64>)> = tx
        .query_row(
            "SELECT id, salience FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                path.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((page_id_bytes, current_salience)) = existing else {
        return Ok(None);
    };
    let page_id = PageId::from_slice(&page_id_bytes)?;
    let next_salience = crate::decay::salience_after_feedback(params, current_salience, kind);

    tx.execute(
        "INSERT INTO page_feedback \
         (id, page_id, workspace_id, project_id, kind, reason, salience_after, author_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            sessionmunch_core::PageFeedbackId::new().as_bytes(),
            page_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            kind.as_str(),
            reason,
            next_salience,
            author_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    // Salience rides on the page *version*: a later supersession starts
    // from salience_default again, which is intentional — feedback is a
    // judgement on the content that was read, not on the path forever.
    tx.execute(
        "UPDATE pages SET salience = ?1 WHERE id = ?2",
        params![next_salience, page_id.as_bytes()],
    )?;
    audit(
        &tx,
        "page_feedback",
        Some(workspace_id.as_bytes()),
        Some(project_id.as_bytes()),
        Some(page_id.as_bytes()),
        author_id.as_ref().map(sessionmunch_core::UserId::as_bytes),
        now,
    )?;
    tx.commit()?;
    Ok(Some((page_id, next_salience)))
}
