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
//! SessionEnd writes the card itself. Everything else is maintenance: the
//! scheduled card tick ([`run_session_card_tick`]) rewrites, in bounded
//! batches, the cards that are due: missing (an imported session that never
//! saw a SessionEnd), built by an older builder, or behind the ledger. The
//! ingest path does no wiki work. The tick replaces only a missing page or a
//! generated one; a consolidated or hand-annotated page is left alone (see
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

/// Cards one maintenance tick rewrites at most.
pub const CARDS_PER_TICK: usize = 25;
/// How often the server runs the card tick while maintenance is enabled.
pub const SESSION_CARD_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Most transcript events one card reads. A longer session's card is built
/// from its latest events and says so.
pub const MAX_CARD_EVENTS: usize = sessionmunch_store::MAX_TRANSCRIPT_EVENTS;
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
    Ok(
        card_parts(reader, workspace_id, project_id, session_id, agent_kind)
            .await?
            .map(|card| card.page),
    )
}

/// A card page with the events and digest it was rendered from.
pub(crate) struct CardParts {
    pub(crate) page: NewPage,
    pub(crate) events: Vec<sessionmunch_core::SessionEvent>,
    pub(crate) digest: SessionDigest,
    pub(crate) last_sequence: i64,
}

pub(crate) async fn card_parts(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    agent_kind: AgentKind,
) -> StoreResult<Option<CardParts>> {
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
    let page = NewPage {
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
    };
    Ok(Some(CardParts {
        page,
        events,
        digest,
        last_sequence,
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

/// Rewrite one session's card. Returns whether a card was written.
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
    write_card(wiki, page, session_actor(reader, session_id).await?).await?;
    Ok(true)
}

/// The session's owner, to attribute generated pages to, as the SessionEnd
/// page is.
pub(crate) async fn session_actor(
    reader: &ReaderPool,
    session_id: SessionId,
) -> StoreResult<ActorContext> {
    Ok(reader
        .session_actor_user(session_id)
        .await?
        .and_then(|owner| IdentityKey::from_storage_key(&owner))
        .map(|owner| owner.to_actor_context())
        .unwrap_or_default())
}

/// What one card tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionCardTick {
    /// Due sessions this tick looked at.
    pub due: usize,
    /// Cards written.
    pub written: usize,
    /// Cards that failed (logged; retried on a later pass).
    pub failed: usize,
    /// Where the next tick continues: the last session looked at, or `None`
    /// when this pass reached the end and the next one starts over.
    pub next_after: Option<SessionId>,
}

/// One maintenance tick: rewrite at most `limit` due cards (see
/// [`ReaderPool::session_cards_due`]) after `after` in session-id order, then
/// make one wiki commit. Ticks walk the due set like a ring, so a session
/// whose card keeps failing cannot hold up the rest. Imported sessions get
/// their page here, off the ingest path; SessionEnd writes its own card.
///
/// # Errors
/// Propagates a failure of the due-set query; per-card failures are logged
/// and counted instead.
pub async fn run_session_card_tick(
    reader: &ReaderPool,
    wiki: &Wiki,
    after: Option<SessionId>,
    limit: usize,
) -> StoreResult<SessionCardTick> {
    let due = reader
        .session_cards_due(CARD_BUILDER_VERSION, None, after, limit)
        .await?;
    let mut tick = SessionCardTick {
        due: due.len(),
        next_after: (due.len() == limit).then(|| due.last().copied()).flatten(),
        ..SessionCardTick::default()
    };
    for id in due {
        match refresh_one(wiki, reader, id).await {
            Ok(true) => tick.written += 1,
            Ok(false) => {}
            Err(error) => {
                tick.failed += 1;
                warn!(session = %id, error = %error, "session card refresh failed");
            }
        }
    }
    if tick.written > 0 {
        match wiki.commit_all(&format!("session cards: {} refreshed", tick.written)) {
            Ok(oid) => debug!(commit = ?oid, written = tick.written, "session cards committed"),
            Err(error) => warn!(error = %error, "session card auto-commit failed"),
        }
    }
    Ok(tick)
}
