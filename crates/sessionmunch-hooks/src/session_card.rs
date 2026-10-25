//! Deterministic session cards (no LLM).
//!
//! When a session has transcript rows in the session event ledger (imported
//! or managed; hook mirrors are left out because an imported transcript of the
//! same session repeats them), its `sessions/<id>.md` page is the card rendered from those
//! events by [`sessionmunch_workstream::render_card`]: the final assistant
//! message verbatim, requests, observed work, checks, commits and files, and
//! what the events cannot show, every line citing ledger sequence numbers
//! (`eN`). A session with no transcript events keeps the rule-based
//! observation summary ([`crate::synthesize_session_page`]).
//!
//! The card is rewritten when SessionEnd arrives and after each ingested
//! event batch, and a bounded batch of stale cards (older builder version,
//! or fewer events than the ledger now holds) is refreshed with each batch.
//! Outside SessionEnd, only a missing page or a generated one is replaced:
//! a consolidated or hand-annotated page is left alone (see
//! [`sessionmunch_store::SESSION_CARD_REPLACEABLE_KEYS`]).

use sessionmunch_core::{
    ActorContext, AgentKind, IdentityKey, NewPage, PagePath, ProjectId, SessionId, Tier,
    WorkspaceId,
};
use sessionmunch_store::{ReaderPool, StoreResult};
use sessionmunch_wiki::{Wiki, WritePageRequest};
use sessionmunch_workstream::{
    CARD_BUILDER_VERSION, FactKind, NativeSignals, SessionDigest, extract_facts, render_card,
};
use tracing::{debug, warn};

/// Most transcript events one card reads. A longer session's card is built
/// from its latest events and says so.
pub const MAX_CARD_EVENTS: usize = sessionmunch_store::MAX_TRANSCRIPT_EVENTS;
/// Stale cards refreshed alongside each ingested batch.
pub const STALE_CARDS_PER_BATCH: usize = 4;
const TITLE_CHARS: usize = 100;
const SUMMARY_CHARS: usize = 240;

/// The card page for a session, or `None` when it has no transcript events.
///
/// # Errors
/// Propagates a ledger read failure.
pub async fn session_card_page(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    agent_kind: AgentKind,
) -> StoreResult<Option<NewPage>> {
    let mut events = reader
        .session_transcript_events(session_id, MAX_CARD_EVENTS)
        .await?;
    if events.is_empty() {
        return Ok(None);
    }
    // The ledger read is newest first.
    events.reverse();
    let mut losses = Vec::new();
    if events.len() == MAX_CARD_EVENTS {
        losses.push(format!(
            "card built from the latest {MAX_CARD_EVENTS} transcript events only"
        ));
    }
    let last_sequence = events.last().map_or(0, |e| e.sequence);
    let digest = extract_facts(
        agent_kind.as_str(),
        &session_id.to_string(),
        &events,
        &losses,
        &NativeSignals::default(),
    );
    let title = card_title(&digest, session_id);
    let mut frontmatter = serde_json::json!({
        "title": title,
        "session_id": session_id.to_string(),
        "agent": agent_kind.as_str(),
        "tier": "episodic",
        "generator": "session-card",
        "card": {
            "builder_version": CARD_BUILDER_VERSION,
            "events": events.len(),
            "last_sequence": last_sequence,
        },
    });
    let summary = card_summary(&digest);
    if !summary.is_empty() {
        frontmatter["summary"] = serde_json::Value::String(summary);
    }
    let path = PagePath::new(format!("sessions/{session_id}.md"))
        .expect("hard-coded sessions/<uuid>.md is always valid");
    Ok(Some(NewPage {
        workspace_id,
        project_id,
        path,
        title,
        body: render_card(&digest),
        tier: Tier::Episodic,
        frontmatter_json: frontmatter,
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }))
}

fn first_text(digest: &SessionDigest, kind: FactKind, key: &str) -> Option<String> {
    digest
        .of(kind)
        .next()
        .and_then(|f| f.payload[key].as_str())
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_string(),
    }
}

fn card_title(digest: &SessionDigest, session_id: SessionId) -> String {
    first_text(digest, FactKind::Request, "text").map_or_else(
        || format!("session {}", &session_id.to_string()[..8]),
        |request| clip(&request, TITLE_CHARS),
    )
}

/// One search-hit line: how it ended, then the counts that matter.
fn card_summary(digest: &SessionDigest) -> String {
    let ended = digest
        .of(FactKind::Ended)
        .next()
        .and_then(|f| f.payload["state"].as_str())
        .unwrap_or("unknown");
    let mut parts = vec![format!("ended {ended}")];
    for (kind, noun) in [
        (FactKind::Request, "requests"),
        (FactKind::FileTouch, "files touched"),
        (FactKind::Check, "checks"),
        (FactKind::Commit, "commits"),
    ] {
        let n = digest.of(kind).count();
        if n > 0 {
            parts.push(format!("{noun}: {n}"));
        }
    }
    let lead = first_text(digest, FactKind::FinalReply, "text")
        .map(|reply| format!("{}; ", clip(&reply, SUMMARY_CHARS)))
        .unwrap_or_default();
    format!("{lead}{}", parts.join(", "))
}

/// Write `page` through the wiki (atomic file write, single writer, index).
pub(crate) async fn write_card(
    wiki: &Wiki,
    page: NewPage,
    actor: ActorContext,
) -> anyhow::Result<sessionmunch_core::PageId> {
    Ok(wiki
        .write_page(WritePageRequest {
            workspace_id: page.workspace_id,
            project_id: page.project_id,
            path: page.path,
            frontmatter: page.frontmatter_json,
            body: page.body,
            tier: page.tier,
            pinned: page.pinned,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor,
            evidence: Vec::new(),
        })
        .await?)
}

/// Refresh one session's card when it is due. Returns whether a card was
/// written.
async fn refresh_one(
    wiki: &Wiki,
    reader: &ReaderPool,
    session_id: SessionId,
) -> anyhow::Result<bool> {
    let Some((workspace_id, project_id, _)) = reader.find_session_scope(session_id).await? else {
        return Ok(false);
    };
    let agent_kind = reader
        .session_agent_kind(session_id)
        .await?
        .unwrap_or(AgentKind::Other);
    let Some(page) =
        session_card_page(reader, workspace_id, project_id, session_id, agent_kind).await?
    else {
        return Ok(false);
    };
    // Attribute to the session's owner, as the SessionEnd page is.
    let actor = reader
        .session_actor_user(session_id)
        .await?
        .and_then(|owner| IdentityKey::from_storage_key(&owner))
        .map(|owner| owner.to_actor_context())
        .unwrap_or_default();
    write_card(wiki, page, actor).await?;
    Ok(true)
}

/// After an event batch for `session_id` is stored: refresh its card if it
/// is due, then up to [`STALE_CARDS_PER_BATCH`] other due cards. Failures
/// are logged, never returned: the events are already committed, and the
/// next batch retries anything left due.
pub async fn refresh_cards_after_ingest(wiki: &Wiki, reader: &ReaderPool, session_id: SessionId) {
    let mut due = match reader
        .session_cards_due(CARD_BUILDER_VERSION, Some(session_id), 1)
        .await
    {
        Ok(due) => due,
        Err(error) => {
            warn!(session = %session_id, error = %error, "session card check failed");
            return;
        }
    };
    match reader
        .session_cards_due(CARD_BUILDER_VERSION, None, STALE_CARDS_PER_BATCH + 1)
        .await
    {
        Ok(stale) => due.extend(
            stale
                .into_iter()
                .filter(|id| *id != session_id)
                .take(STALE_CARDS_PER_BATCH),
        ),
        Err(error) => warn!(error = %error, "stale session card scan failed"),
    }
    let mut written = 0;
    for id in due {
        match refresh_one(wiki, reader, id).await {
            Ok(true) => written += 1,
            Ok(false) => {}
            Err(error) => warn!(session = %id, error = %error, "session card refresh failed"),
        }
    }
    if written > 0 {
        match wiki.commit_all(&format!("session cards: {written} refreshed")) {
            Ok(oid) => debug!(commit = ?oid, written, "session cards committed"),
            Err(error) => warn!(error = %error, "session card auto-commit failed"),
        }
    }
}
