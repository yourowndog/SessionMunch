//! `GET /handoff`: claim the pending handoff and attach managed-run context or a session brief.

use super::*;

/// Query params for `GET /handoff`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HandoffQuery {
    /// Identifier of the agent fetching the handoff. Used to mark the
    /// handoff as accepted-by; defaults to `Other` if unrecognised.
    pub agent: Option<String>,
    /// Optional receiving cwd. Automatic handoffs whose stored cwd is this
    /// directory or a path-boundary ancestor are eligible; manual handoffs are
    /// project-wide. Paths are string-normalized, not filesystem-canonicalized,
    /// so symlink aliases remain distinct.
    pub cwd: Option<String>,
    /// Workspace override (mirror of `HookQuery.workspace`). Lets the
    /// `session-start` hook fetch the handoff for the same `(workspace,
    /// project)` pair the marker file declared, without depending on
    /// the MCP `active_project` cache (which only populates after the
    /// first hook event of the session).
    pub workspace: Option<String>,
    /// Project override (mirror of `HookQuery.project`).
    pub project: Option<String>,
    /// Project strategy (mirror of `HookQuery.project_strategy`).
    pub project_strategy: Option<String>,
    /// Per-repo opt-in for the session-start project brief, forwarded by
    /// the host-side hook from `.sessionmunch.toml`'s
    /// `[briefing] inject_on_session_start`. A truthy value makes this
    /// endpoint append a compiled, char-budgeted brief of the project's
    /// pinned / `_rules/` / `_slots/` pages after any pending handoff, so
    /// the agent starts with the architecture context instead of
    /// re-exploring the codebase (#176). Off when absent.
    pub briefing: Option<String>,
    /// Char budget for the brief, forwarded from the marker's
    /// `[briefing] max_chars`. Clamped server-side to
    /// [`BRIEF_BUDGET_MIN`], [`BRIEF_BUDGET_MAX`]; defaults to
    /// [`BRIEF_BUDGET_DEFAULT`] when absent or unparsable.
    pub briefing_budget: Option<String>,
    /// Invocation-scoped managed run. When present, workstream delta delivery
    /// replaces (and never consumes) the legacy single-use handoff.
    pub managed_run: Option<String>,
    /// Native session identifier observed in the SessionStart payload.
    pub session_id: Option<String>,
}

/// Synchronous endpoint used by `session-start.sh` to discover any
/// pending handoff from a previous agent. Returns plain text Markdown
/// (or an empty body when no handoff is open) with a 1-second cap on
/// the server side so the agent never blocks measurably on startup.
///
/// Side effect: when a handoff is found, it is *marked accepted* before
/// the response is sent. Two agents starting in parallel therefore
/// race; whichever arrives first wins. That is intentional — handoffs
/// are 1:1, not broadcast.
pub(super) async fn handle_handoff(
    State(state): State<Arc<HookState>>,
    Query(query): Query<HandoffQuery>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let actor = actor_identity(actor_ext);
    let skip_webhooks = admission_skips(level_ext, &headers);
    match fetch_and_accept_handoff(&state, query, actor, skip_webhooks).await {
        Ok(Some(markdown)) => (StatusCode::OK, markdown),
        Ok(None) => (StatusCode::OK, String::new()),
        Err(e) => {
            warn!(error = %e, "handoff fetch failed");
            (StatusCode::OK, String::new())
        }
    }
}

pub(super) async fn fetch_and_accept_handoff(
    state: &HookState,
    query: HandoffQuery,
    actor: Option<IdentityKey>,
    skip_webhooks: Vec<String>,
) -> anyhow::Result<Option<String>> {
    let agent = query.agent.as_deref().map_or(AgentKind::Other, parse_agent);
    // A managed run's ledger is additive, not a replacement. Returning it here
    // skipped `latest_open_handoff` below, so a session launched by
    // `sessionmunch run` never consumed the handoff a previous session left for
    // it — the slot just stayed open, and the next managed session missed it
    // too. The brief already reaches the managed path (it is recomposed per
    // session, so resolving it twice was harmless); the handoff is single-use
    // and had no second chance.
    let managed = fetch_managed_context(state, &query, agent).await?;
    // Keep the active-project key compatible with MCP transports: the native
    // session id is carried separately below to bind a destructive handoff
    // claim to its exact receiver.
    let actor_key = sessionmunch_core::ActorKey {
        user: actor.as_ref().map(IdentityKey::storage_key),
        session_id: None,
    };
    let (ws, proj) = resolve_project_ids_inner(
        state,
        query.cwd.as_deref(),
        query.workspace.as_deref(),
        query.project.as_deref(),
        ProjectStrategy::parse(query.project_strategy.as_deref()),
    )
    .await?;
    // Session-start handoff delivery is a foreground action. Publish it so
    // static MCP callers resolve to the directory that is opening now. The
    // query carries no recall preference; the main capture path publishes
    // `default_global` when its SessionStart arrives.
    if has_publishable_scope_hint(query.cwd.as_deref(), query.project.as_deref()) {
        state.active_project.set_for(&actor_key, ws, proj, false);
    }
    // The actor is what makes this lookup safe on a shared server: without it
    // the newest open handoff in the project is returned to whoever starts a
    // session next, and the claim below consumes it — so one operator's baton
    // lands in another operator's context and is lost to its author. Handoffs
    // with no owner stay project-wide, which is the single-operator behaviour.
    let owner_filter = match &actor {
        Some(key) => sessionmunch_core::OwnerFilter::User(key.storage_key()),
        None => sessionmunch_core::OwnerFilter::Unattributed,
    };
    let handoff = state
        .reader
        .latest_open_handoff(ws, proj, query.cwd.clone(), owner_filter.clone())
        .await?;
    let handoff_md = handoff.as_ref().map(render_handoff_markdown);
    // The brief is additive and non-destructive: unlike the handoff (a
    // single-use slot claimed below), it is recomposed on every opted-in
    // session start — exactly what a Claude Code `/clear` needs (#176).
    let brief_md = render_requested_session_brief(state, &query, ws, proj, actor.as_ref()).await?;
    // Handoff first: it is a short curated pointer and must not be buried
    // under a ledger that can run tens of KB. The existing ledger-then-brief
    // order is preserved. Claim both single-use inputs only after every
    // fallible read and render has succeeded, and in one transaction so a
    // failed or racing managed claim cannot consume the handoff by itself.
    // Same reasoning as the SessionEnd insert: the session-start claim is how
    // most handoffs are consumed, so a webhook must be able to see it. Two
    // properties keep that policy call from costing the operator the baton:
    //
    // - Only a webhook the operator set to `reject` is waited for. An
    //   ignore-policy webhook cannot refuse anything, so observers are notified
    //   below, off the critical path, once the claim is durable. The deciding
    //   chain is also capped by `AUTOMATIC_HANDOFF_ADMISSION_TIMEOUT`, below the
    //   shortest shipped client's one-second deadline. Without that server-side
    //   bound, a slow approval could consume the single-use baton after the
    //   client had disconnected and could no longer receive it.
    // - A refusal (or a down reject-policy host, which a reject chain cannot
    //   tell apart) cancels only the CLAIM. The handoff stays open for the next
    //   session and the brief is still served, instead of the whole endpoint
    //   erroring out.
    let (handoff, admission_ctx) = match handoff {
        Some(pending) => {
            let authorized = tokio::time::timeout(
                AUTOMATIC_HANDOFF_ADMISSION_TIMEOUT,
                state.wiki.authorize_operation(
                    ws,
                    proj,
                    sessionmunch_wiki::AdmissionOp::HandoffAccept,
                    actor
                        .as_ref()
                        .map(IdentityKey::to_actor_context)
                        .unwrap_or_default(),
                    skip_webhooks,
                ),
            )
            .await;
            match authorized {
                Ok(Ok(ctx)) => (Some(pending), ctx),
                Ok(Err(e)) => {
                    warn!(error = %e, "handoff claim refused by admission chain; leaving it open");
                    (None, None)
                }
                Err(_) => {
                    warn!(
                        timeout_ms = AUTOMATIC_HANDOFF_ADMISSION_TIMEOUT.as_millis(),
                        "handoff admission exceeded the session-start deadline; leaving it open"
                    );
                    (None, None)
                }
            }
        }
        None => (None, None),
    };
    let accepting_session = query
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(resolve_native_session_id);
    let receiving_session = if handoff.is_some() {
        match accepting_session {
            Some(id) => Some(NewSession {
                id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: agent,
                cwd: query.cwd.as_deref().map(std::path::PathBuf::from),
                actor_user: owner_stamp_for_event(state, actor.as_ref()).await,
            }),
            None => None,
        }
    } else {
        None
    };
    let acceptance = if handoff.is_some() || managed.is_some() {
        state
            .writer
            .accept_startup_context(
                handoff
                    .as_ref()
                    .map(|handoff| sessionmunch_core::HandoffAcceptance {
                        handoff_id: handoff.scope.id,
                        workspace_id: ws,
                        project_id: proj,
                        accepting_agent: agent,
                        accepting_session,
                        accepting_user: actor.as_ref().map(IdentityKey::storage_key),
                        owner_filter,
                        receiving_cwd: query.cwd.clone(),
                    }),
                managed.as_ref().map(|managed| managed.run_id),
                receiving_session,
            )
            .await?
    } else {
        sessionmunch_store::StartupContextAcceptance::default()
    };
    let handoff_md = if acceptance.handoff_accepted {
        // The claim is durable now, so the observers can be told about it. A
        // racing session that won the row leaves `handoff_accepted` false, and
        // then nothing is dispatched: a mirror is only ever told about a baton
        // this request actually consumed.
        if let Some(ctx) = &admission_ctx {
            state.wiki.notify_operation_observers(ctx);
        }
        handoff_md
    } else {
        None
    };
    let managed_md = if acceptance.managed_context_accepted {
        managed.and_then(|managed| managed.markdown)
    } else {
        None
    };
    Ok(combine_handoff_and_brief(
        handoff_md,
        combine_handoff_and_brief(managed_md, brief_md),
    ))
}

pub(super) struct PendingManagedContext {
    run_id: ManagedRunId,
    markdown: Option<String>,
}

/// Render the managed-run ledger for this SessionStart, if there is one.
///
/// Returns `None` — never an early exit for the caller — when the run is
/// unusable (bad id, no active run, agent mismatch) or its context was
/// already delivered. The pending handoff still has to reach the agent in
/// every one of those cases.
pub(super) async fn fetch_managed_context(
    state: &HookState,
    query: &HandoffQuery,
    agent: AgentKind,
) -> anyhow::Result<Option<PendingManagedContext>> {
    let Some(raw_run_id) = query.managed_run.as_deref() else {
        return Ok(None);
    };
    let Ok(run_id) = ManagedRunId::from_str(raw_run_id) else {
        warn!(managed_run = %raw_run_id, "invalid managed run id on SessionStart");
        return Ok(None);
    };
    if let Some(native_session_id) = query
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let _ = state
            .writer
            .link_managed_run_session(run_id, agent, native_session_id)
            .await?;
    }
    let Some(context) = state.reader.managed_run_context(run_id, 256).await? else {
        warn!(managed_run = %run_id, "managed SessionStart has no active run");
        return Ok(None);
    };
    if context.agent != agent {
        warn!(
            managed_run = %run_id,
            expected = %context.agent.as_str(),
            actual = %agent.as_str(),
            "managed SessionStart agent mismatch"
        );
        return Ok(None);
    }
    if context.context_delivered {
        return Ok(None);
    }
    let rendered = render_managed_context(
        &context.events,
        &context.workstream_name,
        context.workstream_id,
        context.sync_after,
    );
    Ok(Some(PendingManagedContext {
        run_id,
        markdown: rendered,
    }))
}

pub(super) async fn render_requested_session_brief(
    state: &HookState,
    query: &HandoffQuery,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    viewer: Option<&sessionmunch_core::IdentityKey>,
) -> anyhow::Result<Option<String>> {
    if !crate::payload::query_flag_truthy(query.briefing.as_deref()) {
        return Ok(None);
    }
    let budget = query
        .briefing_budget
        .as_deref()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(BRIEF_BUDGET_DEFAULT)
        .clamp(BRIEF_BUDGET_MIN, BRIEF_BUDGET_MAX);
    let (core, recent) = state
        .reader
        .session_brief_pages_with_slot_visibility(
            workspace_id,
            project_id,
            BRIEF_CORE_PAGES_LIMIT,
            BRIEF_RECENT_PAGES_LIMIT,
            // With `[slots] per_user` on, personal slots reach only their
            // owner and shared ones reach everyone. With it off — the default
            // — no slot is anybody's, so the brief carries all of them exactly
            // as it did before the feature existed.
            sessionmunch_core::SlotVisibility::for_viewer(state.per_user_slots, viewer),
        )
        .await?;
    Ok(render_session_brief(&core, &recent, budget))
}

pub(super) fn combine_handoff_and_brief(
    handoff_md: Option<String>,
    brief_md: Option<String>,
) -> Option<String> {
    match (handoff_md, brief_md) {
        (Some(h), Some(b)) => Some(format!("{h}\n{b}")),
        (Some(h), None) => Some(h),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}
