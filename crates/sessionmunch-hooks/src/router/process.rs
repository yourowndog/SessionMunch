//! Event processing: session lifecycle, observation storage, automatic handoffs and session-end consolidation.

use super::*;

/// Returns `true` when the event cleared the writer, so the caller can stamp
/// the ingest "last write" metric. A rejected or failed event returns `false`:
/// nothing was persisted, and pretending otherwise hides exactly the outage
/// the metric exists to expose.
pub(super) async fn process_envelope(
    state: Arc<HookState>,
    env: HookEnvelope,
    actor: Option<IdentityKey>,
    level: sessionmunch_core::AuthLevel,
    skip_webhooks: Vec<String>,
) -> bool {
    if let Err(e) = process_authorized(&state, env, actor, level, skip_webhooks).await {
        if matches!(
            e.downcast_ref::<StoreError>(),
            Some(StoreError::SessionCollision)
        ) {
            warn!("hook session collision dropped");
        } else {
            warn!(error = %e, "hook processing failed");
        }
        return false;
    }
    true
}

pub(super) async fn enqueue_session_end_consolidation(
    state: &HookState,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> anyhow::Result<()> {
    if !state.consolidate_on_session_end || state.consolidator.is_none() {
        return Ok(());
    }
    let Some(notify) = state.session_consolidation_notify.as_ref() else {
        warn!(
            session = %session_id,
            "SessionEnd LLM consolidation enabled without a queue worker"
        );
        return Ok(());
    };
    let inserted = state
        .writer
        .enqueue_session_consolidation(workspace_id, project_id, session_id)
        .await?;
    notify.notify_one();
    debug!(
        session = %session_id,
        inserted,
        "SessionEnd LLM consolidation queued"
    );
    Ok(())
}

/// Only events that begin or actively advance work may move the legacy
/// process-wide and identity-only fallbacks. Completion events can arrive
/// after the operator has moved to another project, especially when a native
/// hook spool drains after an agent process exits unexpectedly (#372).
pub(super) const fn advances_active_project_fallback(event: HookEvent) -> bool {
    matches!(
        event,
        HookEvent::SessionStart | HookEvent::UserPrompt | HookEvent::PreToolUse
    )
}

pub(super) fn publish_active_project_for_event(
    state: &HookState,
    event: HookEvent,
    actor: &ActorKey,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    default_global: bool,
) {
    if advances_active_project_fallback(event) {
        state
            .active_project
            .set_for(actor, workspace_id, project_id, default_global);
    } else {
        state
            .active_project
            .set_scoped_for(actor, workspace_id, project_id, default_global);
    }
}

#[cfg(test)]
pub(super) async fn process(
    state: &HookState,
    env: HookEnvelope,
    actor: Option<IdentityKey>,
    skip_webhooks: Vec<String>,
) -> anyhow::Result<()> {
    match process_authorized(
        state,
        env,
        actor,
        sessionmunch_core::AuthLevel::Anonymous,
        skip_webhooks,
    )
    .await
    {
        // Match the asynchronous hook envelope: collisions are terminal
        // rejections, but fire-and-forget ingress acknowledges the delivery.
        Err(error)
            if matches!(
                error.downcast_ref::<StoreError>(),
                Some(StoreError::SessionCollision)
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

pub(super) async fn process_authorized(
    state: &HookState,
    env: HookEnvelope,
    actor: Option<IdentityKey>,
    level: sessionmunch_core::AuthLevel,
    // Admission webhooks this request opted out of (see `admission_skips`).
    // Empty for every caller with no HTTP request behind it.
    skip_webhooks: Vec<String>,
) -> anyhow::Result<()> {
    let session_id = resolve_session_id(&env)?;
    // Build the actor key used to scope the in-process `ActiveProject`
    // pointer. `user` is the qualified storage key of whatever identity the
    // auth middleware extracted from this request; `session_id` is the RAW
    // string from the payload (NOT the resolved UUID) — agents that forward an
    // opaque session id over `X-Memory-Actor-Session-Id` on /mcp pass the same
    // raw string, so set and get land on the same map slot. The MCP server's
    // `actor_key_from_parts` mirrors this convention. Empty actor
    // (anonymous + no session) is allowed — `set_for` falls back to the
    // single slot.
    let actor_key = sessionmunch_core::ActorKey {
        user: actor.as_ref().map(IdentityKey::storage_key),
        session_id: env.session_id.clone(),
    };
    // Session-sticky attribution: for an event whose session already
    // exists, the session's own scope is the source of truth (the same
    // rationale as the V19 repair). Per-event cwd derivation only decides
    // for session-CREATING events. Without this, a mid-session `cd subdir/`
    // inside a NON-GIT project scattered observations into basename
    // fragments ("sources", "desktop", …): the v0.12.2 prefix match keys on
    // `repo_path`, and #103 deliberately never records one for non-git
    // parents, so the match had nothing to anchor to.
    //
    // Stickiness is bounded so it can never become a catch-all:
    // - Explicit marker-file overrides still win — a `.sessionmunch.toml`
    //   naming a project is a deliberate rescope, not drift.
    // - The event's cwd must sit INSIDE the session's own cwd subtree;
    //   `cd`-ing out of the session's tree (into a different project)
    //   falls back to per-event resolution as before — except under
    //   `repo-root`, where a no-override cwd is already proven outside
    //   any repo and marker, so the session sticks instead (#394).
    // - A session rooted at a broad directory — the filesystem root or
    //   the user's home — never sticks, or a stray session started in
    //   `$HOME` would fold every project beneath it into one bucket
    //   (the same catch-all failure #103 healed for repo_path keys).
    // - Under `[routing] mid_session = "sticky"` the session also overrules a
    //   host-derived `repo-root` override, closing the cross-repo `cd` case;
    //   marker-declared scopes still win. See `overrides_permit_sticky`.
    let sticky_scope = if overrides_permit_sticky(
        env.workspace_override.as_deref(),
        env.project_override.as_deref(),
        env.project_source,
        state.mid_session_routing,
    ) {
        state
            .reader
            .find_session_scope(session_id)
            .await?
            .filter(|(_, _, session_cwd)| {
                sticky_cwd_admits(
                    session_cwd.as_deref(),
                    env.cwd.as_deref(),
                    state.home_dir.as_deref(),
                    env.project_strategy,
                    state.mid_session_routing,
                )
            })
    } else {
        None
    };
    let publishable_scope = sticky_scope.is_some()
        || has_publishable_scope_hint(env.cwd.as_deref(), env.project_override.as_deref());
    let (mut ws, mut proj) = match sticky_scope {
        Some((session_ws, session_proj, _)) => (session_ws, session_proj),
        None => {
            resolve_project_ids_inner(
                state,
                env.cwd.as_deref(),
                env.workspace_override.as_deref(),
                env.project_override.as_deref(),
                env.project_strategy,
            )
            .await?
        }
    };

    // Hooks are fire-and-forget and may arrive out of order. Begin the
    // session idempotently before every observation so a resumed agent
    // session, or a prompt racing ahead of SessionStart, cannot trip the
    // observations.session_id foreign key.
    // Stamp the authenticated operator so anything that later picks "the open
    // session for this scope" can tell whose it is — but only where operators
    // are actually told apart. On an unauthenticated server, and on one that
    // authenticates a single operator, this stays None and the session is
    // shared, as before. The same value owns the SessionEnd handoff below, so
    // the session and its baton can never disagree about who they belong to.
    let owner_stamp = owner_stamp_for_event(state, actor.as_ref()).await;
    // A keyed event claims its project-scoped key in the same transaction as
    // the observation. A pending replay resumes the downstream wiki/handoff
    // effects without duplicating the observation; only a delivery whose
    // effects were marked complete is skipped.
    let ingest_key = env.ingest_key.clone();
    let owner_filter = if env.all_owners_requested {
        if !matches!(env.event, HookEvent::SessionEnd) {
            return Err(StoreError::SessionCollision.into());
        }
        let distinguishes = state
            .reader
            .distinguishes_operators(state.trusted_proxy_identity)
            .await?;
        if level
            .authorize(sessionmunch_core::Capability::Admin, distinguishes)
            .is_err()
        {
            return Err(StoreError::SessionCollision.into());
        }
        sessionmunch_core::OwnerFilter::Any
    } else {
        actor
            .as_ref()
            .map_or(sessionmunch_core::OwnerFilter::Unattributed, |id| {
                sessionmunch_core::OwnerFilter::User(id.storage_key())
            })
    };
    let cwd_norm = env
        .cwd
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(normalize_project_path_key);
    let cache_key = cache_key_for(
        cwd_norm.as_deref(),
        env.workspace_override.as_deref(),
        env.project_override.as_deref(),
        env.project_strategy,
    );
    let mut attempts = 0;
    // Keep the successful keyed-ingest gate until every downstream effect has
    // completed. A replay that observes `ResumePending` must not race the first
    // delivery's page, handoff, consolidation, or key-completion tail.
    let (admission, log_title, _ingest_guard) = loop {
        // Rebuild both records on retry: the store validates and persists this
        // exact scope pair atomically with the keyed observation claim.
        let new_session = NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: env.agent,
            cwd: env.cwd.as_ref().map(std::path::PathBuf::from),
            actor_user: owner_stamp.clone(),
        };
        let kind = env.event.to_observation_kind();
        let raw_obs = NewObservation {
            session_id,
            workspace_id: ws,
            project_id: proj,
            kind,
            extension: env.extension.clone(),
            source_event: env.source_event.clone(),
            title: env
                .title_hint
                .clone()
                .unwrap_or_else(|| kind.as_str().to_string()),
            body: env.body_excerpt.clone().unwrap_or_default(),
            importance: importance_for(env.event),
        };
        let sanitized = Sanitized::new(raw_obs, &state.sanitizer);
        let log_title = sanitized.inner().title.clone();
        let ingest_guard = if let Some(key) = ingest_key.as_deref() {
            Some(state.ingest_gates.lock(proj, key).await)
        } else {
            None
        };
        let result = state
            .writer
            .admit_hook_session_event(
                new_session,
                sanitized,
                owner_filter.clone(),
                ingest_key.clone(),
                env.machine.clone(),
                env.captured_at,
            )
            .await;
        match result {
            Ok(admission) => break (admission, log_title, ingest_guard),
            Err(error) if attempts == 0 && error.is_stale_session_scope_reference() => {
                // Do not hold an old-project gate while resolving and acquiring
                // the refreshed scope's gate; that would permit nested gates.
                drop(ingest_guard);
                attempts += 1;
                warn!(error = %error, "hook admission used a stale project cache entry; evicting and retrying once");
                state.project_cache.lock().await.remove(&cache_key);
                (ws, proj) = resolve_project_ids_inner(
                    state,
                    env.cwd.as_deref(),
                    env.workspace_override.as_deref(),
                    env.project_override.as_deref(),
                    env.project_strategy,
                )
                .await?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    // Admission is the first mutation/effect after scope resolution.

    // `SESSIONMUNCH_RUN_ID` is invocation-scoped. A valid active run links the
    // native session and switches only this hook invocation to managed
    // workstream semantics; direct harness launches keep the legacy path.
    let managed = env.managed_run.is_some();
    let managed_run = env.managed_run.as_deref().and_then(|raw| {
        ManagedRunId::from_str(raw)
            .map_err(|error| {
                warn!(managed_run = %raw, error = %error, "invalid managed run id on hook event");
                error
            })
            .ok()
    });
    let (admitted, ingest) = match admission {
        HookSessionAdmission::InvalidMissingEnd => {
            info!(session = %session_id, "ignoring missing SessionEnd");
            return Ok(());
        }
        HookSessionAdmission::InvalidScopedEnd => {
            info!(
                session = %session_id,
                agent = %env.agent.as_str(),
                "ignoring SessionEnd naming a different scope than its session"
            );
            return Ok(());
        }
        HookSessionAdmission::AlreadyEnded { session } => {
            let commit_msg = format!("repair session {}", short_id(&session_id.to_string()));
            match state.wiki.commit_all(&commit_msg) {
                Ok(Some(oid)) => debug!(commit = %oid, "wiki recovery auto-commit"),
                Ok(None) => debug!("wiki clean during SessionEnd recovery"),
                Err(e) => warn!(error = %e, "SessionEnd recovery auto-commit failed"),
            }
            let observations = state.reader.observations_for_session(session_id).await?;
            if !is_ephemeral_session(&observations) {
                enqueue_session_end_consolidation(
                    state,
                    session_id,
                    session.workspace_id(),
                    session.project_id(),
                )
                .await?;
            }
            if let Some(key) = ingest_key {
                let _ = state
                    .writer
                    .complete_observation_ingest_if_claimed(session.project_id(), key)
                    .await?;
            }
            return Ok(());
        }
        HookSessionAdmission::Observation { session, ingest }
        | HookSessionAdmission::EndOpen { session, ingest }
        | HookSessionAdmission::ReEnd { session, ingest } => (session, ingest),
    };
    if ingest == IngestObservationOutcome::AlreadyComplete {
        return Ok(());
    }
    if ingest == IngestObservationOutcome::ResumePending {
        debug!("resuming incomplete keyed hook event");
    }
    ws = admitted.workspace_id();
    proj = admitted.project_id();
    if let Some(run_id) = managed_run
        && let Some(native_session_id) = env
            .session_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    {
        let _ = state
            .writer
            .link_managed_run_session(run_id, env.agent, native_session_id)
            .await?;
    }
    if publishable_scope {
        publish_active_project_for_event(
            state,
            env.event,
            &actor_key,
            ws,
            proj,
            env.recall_default_global_requested,
        );
    }

    // Append the log line to the per-project log.md.
    if let Err(e) = log::append_event(
        &state.wiki,
        ws,
        proj,
        Timestamp::now(),
        env.event,
        log_title.as_str(),
    ) {
        warn!(error = %e, "log.md append failed");
    }

    // On PreCompact, refresh `sessions/<id>.md` so the wiki captures
    // the working state before the agent's compaction throws it out
    // of context. Does NOT end the session and does NOT create a
    // handoff. The eventual SessionEnd supersedes this page.
    //
    // On PostCompaction (Devin), refresh `sessions/<id>.md` after the
    // agent's compaction has completed. This is a post-facto checkpoint
    // that captures the state after compaction. Does NOT end the session
    // and does NOT create a handoff. The eventual SessionEnd supersedes
    // this page.
    let checkpoint_label = match env.event {
        HookEvent::PreCompact => Some("pre-compact"),
        HookEvent::PostCompaction => Some("post-compaction"),
        _ => None,
    };
    // Whose session this is, as recorded at session start (a qualified storage
    // key). Everything written on its behalf is attributed to that owner, not
    // to whoever delivered the event. A NULL owner stays shared. V40
    // deliberately migrated legacy rows to NULL,
    // and current actorless sessions use the same value; attributing either to
    // whoever delivers SessionEnd would silently rebucket shared context. A
    // store failure aborts this event rather than guessing an owner.
    let session_owner = parse_session_owner(admitted.owner().map(str::to_owned))?;
    let session_actor = session_owner
        .as_ref()
        .map(IdentityKey::to_actor_context)
        .unwrap_or_default();
    if let Some(checkpoint_label) = checkpoint_label
        && let Err(e) = consolidate_or_synth(
            state,
            session_id,
            ws,
            proj,
            admitted.agent_kind(),
            checkpoint_label,
            session_actor.clone(),
        )
        .await
    {
        warn!(error = %e, "PreCompact/PostCompaction consolidation failed; continuing");
    }

    // On SessionEnd, close boundary-only sessions without generated artifacts.
    // Substantive sessions synthesize the summary page and auto-handoff below.
    if matches!(env.event, HookEvent::SessionEnd) {
        let mut observations = state.reader.observations_for_session(session_id).await?;
        if is_ephemeral_session(&observations) {
            let outcome = state
                .writer
                .end_admitted_lifecycle_only_session(admitted.clone())
                .await?;
            match outcome {
                sessionmunch_store::LifecycleOnlyEndOutcome::Ended { reopened_handoff } => {
                    let commit_msg = format!(
                        "lifecycle-only session {}",
                        short_id(&session_id.to_string()),
                    );
                    match state.wiki.commit_all(&commit_msg) {
                        Ok(Some(oid)) => debug!(commit = %oid, "lifecycle-only log auto-commit"),
                        Ok(None) => debug!("wiki clean after lifecycle-only SessionEnd"),
                        Err(e) => warn!(error = %e, "lifecycle-only log auto-commit failed"),
                    }
                    info!(
                        session = %session_id,
                        reopened_handoff = ?reopened_handoff,
                        "lifecycle-only session ended without summary, handoff, or consolidation",
                    );
                    if let Some(key) = ingest_key {
                        state.writer.complete_observation_ingest(proj, key).await?;
                    }
                    return Ok(());
                }
                sessionmunch_store::LifecycleOnlyEndOutcome::Substantive => {
                    observations = state.reader.observations_for_session(session_id).await?;
                    debug!(
                        session = %session_id,
                        "substantive work raced lifecycle-only SessionEnd; using normal end path",
                    );
                    // The writer observed substantive work, so the refreshed
                    // set cannot still satisfy the lifecycle-only predicate.
                    debug_assert!(!is_ephemeral_session(&observations));
                }
            }
        }
        // The card needs the event ledger (imported or managed transcripts);
        // a hook-only session keeps the observation summary. A ledger read
        // failure falls back too: SessionEnd must still close the session.
        let card = match session_card_page(
            &state.reader,
            ws,
            proj,
            session_id,
            admitted.agent_kind(),
        )
        .await
        {
            Ok(card) => card,
            Err(e) => {
                warn!(session = %session_id, error = %e, "session card unavailable; using observation summary");
                None
            }
        };
        let new_page = card.unwrap_or_else(|| {
            synthesize_session_page(ws, proj, session_id, admitted.agent_kind(), &observations)
        });
        let page_id = state
            .wiki
            .write_page(sessionmunch_wiki::WritePageRequest {
                workspace_id: new_page.workspace_id,
                project_id: new_page.project_id,
                path: new_page.path.clone(),
                frontmatter: new_page.frontmatter_json.clone(),
                body: new_page.body.clone(),
                tier: new_page.tier,
                pinned: new_page.pinned,
                title: None,
                admission_ctx: None,
                author_id: None,
                // Attribute to the operator who OWNED the session, read back
                // from the session row, not to whoever delivered this
                // SessionEnd — a spool drain, an operator finalizing a stuck
                // session, or a shared hook token can all carry a different
                // identity. NULL stays anonymous/shared, including rows that
                // predate owner recording.
                actor: session_actor.clone(),
                evidence: Vec::new(),
            })
            .await?;
        // The baton follows the SESSION's owner, so it reaches the person who
        // was working, not whoever flushed the event. Run through the ownership
        // gate again because even an attributed session remains shared on a
        // deployment that does not tell its operators apart — otherwise the
        // baton lands in a bucket the operator's actorless transport cannot
        // read.
        let handoff_owner = owner_stamp_for_event(state, session_owner.as_ref()).await;
        let handoff = (!managed).then(|| {
            build_auto_handoff(
                ws,
                proj,
                env.agent,
                session_id,
                env.cwd.clone(),
                &observations,
                handoff_owner,
            )
        });
        // Automatic SessionEnd handoffs are the bulk of handoff traffic;
        // skipping admission here would leave a webhook seeing only the
        // MCP-initiated minority and silently not enforcing its policy.
        //
        // Asked BEFORE the write below, which commits and cannot be undone: a
        // policy answer that arrives after it can only take the baton away
        // from a session that no longer exists. And whatever the answer is, it
        // never aborts the handler — a refusal (or a webhook host that is down
        // or slow, which a reject policy cannot tell apart) skips the handoff
        // and is logged, while the session page, the queued consolidation and
        // the auto-commit below still run.
        //
        // Only the webhooks that can refuse are awaited here; the observers are
        // told below, once the row exists, so a mirror is never announced a
        // baton that a failed insert (or a refusal further down the chain)
        // meant never happened. Same order as the session-start claim and the
        // MCP tools that raise this op.
        let (handoff, admission_ctx) = match handoff {
            Some(handoff) => {
                match state
                    .wiki
                    .authorize_operation(
                        ws,
                        proj,
                        sessionmunch_wiki::AdmissionOp::HandoffBegin,
                        session_actor.clone(),
                        skip_webhooks,
                    )
                    .await
                {
                    Ok(ctx) => (Some(handoff), ctx),
                    Err(e) => {
                        warn!(
                            session = %session_id,
                            error = %e,
                            "auto handoff refused by admission chain; session ends without a baton",
                        );
                        (None, None)
                    }
                }
            }
            None => (None, None),
        };
        // The end stamp and the baton commit in one transaction: a crash
        // between them would leave an ended session whose successor has
        // nothing to pick up. A managed run and an admission refusal both take
        // the second arm, ending the session with no handoff at all.
        let handoff_id = match handoff {
            Some(handoff) => Some(
                state
                    .writer
                    .end_admitted_session_with_handoff(admitted.clone(), Some(page_id), handoff)
                    .await?,
            ),
            None => {
                state
                    .writer
                    .end_admitted_session(admitted.clone(), Some(page_id))
                    .await?;
                None
            }
        };
        if handoff_id.is_some()
            && let Some(ctx) = &admission_ctx
        {
            state.wiki.notify_operation_observers(ctx);
        }
        // Auto-commit the wiki tree so the session/handoff/log.md
        // changes land in git in one atomic snapshot.
        let commit_msg = format!(
            "session {}: {}",
            short_id(&session_id.to_string()),
            new_page.title.chars().take(60).collect::<String>(),
        );
        match state.wiki.commit_all(&commit_msg) {
            Ok(Some(oid)) => debug!(commit = %oid, "wiki auto-commit"),
            Ok(None) => debug!("wiki clean; no auto-commit"),
            Err(e) => warn!(error = %e, "auto-commit failed"),
        }
        // Persist the optional LLM work before acknowledging the hook, after
        // deterministic wiki writes are committed so the worker cannot race
        // their git snapshot. Stale redelivery above repairs cancellation in
        // the narrow window after `end_session`.
        enqueue_session_end_consolidation(state, session_id, ws, proj).await?;
        if let Some(handoff_id) = handoff_id {
            info!(
                session = %session_id,
                page = %new_page.path,
                handoff = %handoff_id,
                "session ended; summary page + open handoff created",
            );
        } else if managed {
            info!(
                session = %session_id,
                page = %new_page.path,
                managed_run = ?managed_run,
                "managed session ended; summary page written without duplicate legacy handoff",
            );
        } else {
            // Only reachable through the admission refusal above, which already
            // warned with the reason; without this arm the refusal would be
            // logged as a managed session end and hide why the baton is gone.
            info!(
                session = %session_id,
                page = %new_page.path,
                "session ended; summary page written without a handoff (admission refused)",
            );
        }
    }

    if let Some(key) = ingest_key {
        state.writer.complete_observation_ingest(proj, key).await?;
    }

    Ok(())
}

/// Parse the persisted owner without turning corrupt owned data into shared
/// data. Only SQL NULL has the intentional legacy/shared meaning.
pub(super) fn parse_session_owner(owner: Option<String>) -> anyhow::Result<Option<IdentityKey>> {
    owner
        .map(|owner| {
            IdentityKey::from_storage_key(&owner)
                .ok_or_else(|| anyhow::anyhow!("malformed stored session owner identity"))
        })
        .transpose()
}

pub(super) fn resolve_session_id(env: &HookEnvelope) -> anyhow::Result<SessionId> {
    if let Some(raw) = &env.session_id {
        return Ok(resolve_native_session_id(raw));
    }
    if matches!(env.event, HookEvent::SessionStart) {
        return Ok(SessionId::new());
    }
    anyhow::bail!("hook payload missing session_id and event is not session-start")
}

pub(super) fn resolve_native_session_id(raw: &str) -> SessionId {
    // Accept either a UUID (canonical) or any string, hashing the latter to a
    // deterministic UUID v5 so hook POSTs and startup GETs share one key.
    SessionId::from_str(raw)
        .unwrap_or_else(|_| SessionId(Uuid::new_v5(&Uuid::NAMESPACE_OID, raw.as_bytes())))
}

/// A session is substantive iff it logged at least one `UserPrompt`,
/// `PreToolUse`, or `PostToolUse` observation. Everything else — including
/// `Stop` and `Notification`, which some harnesses (e.g. OpenCode) fire for
/// purely internal work such as branch-naming — is ephemeral/no-op and must
/// not synthesize a session page. This positive definition MUST stay in sync
/// with the store-side SQL in `end_lifecycle_only_session_in_tx`
/// (sessionmunch-store/src/ops.rs): both sides classify every observation set
/// identically, or the atomic store re-check silently reverts this verdict.
pub(super) fn is_ephemeral_session(observations: &[sessionmunch_core::Observation]) -> bool {
    !observations.iter().any(|observation| {
        matches!(
            observation.kind,
            ObservationKind::UserPrompt
                | ObservationKind::PreToolUse
                | ObservationKind::PostToolUse
        )
    })
}

pub(super) fn build_auto_handoff(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    from_agent: AgentKind,
    session_id: SessionId,
    cwd: Option<String>,
    observations: &[sessionmunch_core::Observation],
    owner_user: Option<String>,
) -> NewHandoff {
    // Prefer obs.body (the full prompt) over obs.title (first-line +
    // truncated to 80 chars for log/list display). When body is
    // empty fall back to title so we never produce an empty entry.
    pub(super) fn pick_text(obs: &sessionmunch_core::Observation) -> &str {
        if !obs.body.is_empty() {
            obs.body.as_str()
        } else {
            obs.title.as_str()
        }
    }
    /// Cap so a single 10-page prompt doesn't blow up the handoff.
    /// The body is already scrubbed at insert time; this is just a
    /// length budget. 1500 chars ≈ 250 words ≈ a paragraph.
    pub(super) fn cap(s: &str) -> String {
        const MAX: usize = 1500;
        if s.chars().count() <= MAX {
            s.to_string()
        } else {
            let truncated: String = s.chars().take(MAX).collect();
            format!("{truncated}…")
        }
    }
    let mut prompts: Vec<String> = Vec::new();
    let mut tools: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for obs in observations {
        match obs.kind {
            ObservationKind::UserPrompt => {
                let text = pick_text(obs);
                if !text.is_empty() {
                    prompts.push(text.to_string());
                }
            }
            ObservationKind::PostToolUse | ObservationKind::PreToolUse if !obs.title.is_empty() => {
                tools.insert(obs.title.as_str());
            }
            _ => {}
        }
    }
    let first_prompt = prompts.first().cloned();
    let last_prompt = prompts.last().cloned();
    let summary = match (&first_prompt, &last_prompt) {
        (Some(first), Some(last)) if first == last => format!("Session focused on: {}", cap(first)),
        (Some(first), Some(last)) => format!("Started: {}\n\nLast: {}", cap(first), cap(last),),
        (Some(first), None) => format!("Started: {}", cap(first)),
        _ => format!(
            "Session ended; {} observations recorded.",
            observations.len()
        ),
    };
    let open_questions = derive_open_questions(observations, &last_prompt);
    let next_steps = if tools.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Tools used: {}",
            tools.into_iter().collect::<Vec<_>>().join(", ")
        )]
    };
    NewHandoff {
        workspace_id,
        project_id,
        from_session_id: Some(session_id),
        from_agent,
        to_agent: None,
        cwd: cwd.map(std::path::PathBuf::from),
        summary,
        open_questions,
        next_steps,
        files_touched: Vec::new(),
        owner_user,
    }
}

/// Derive the `open_questions` field for an automatic SessionEnd handoff
/// using multi-signal heuristics, instead of blindly copying the last user
/// prompt.
///
/// The previous implementation always did `"Continue from: <last prompt>"`,
/// which produced useless entries when the last prompt was an acknowledgment
/// ("ok", "thanks", "好的") or when the session ended mid-task after an edit
/// but before verification. This function inspects the *tail* of the
/// observation stream to choose the most informative framing.
///
/// Heuristics (first match wins):
/// 1. Last prompt ends with `?` or `？` → tag as an unresolved question.
/// 2. Last tool was an edit/write/patch with no subsequent Stop → the
///    session ended before verification; advise the receiver to run tests.
/// 3. Has Stop but no SessionEnd → mid-task exit; use the last
///    *substantive* prompt (filtering acknowledgments).
/// 4. Normal end → last substantive prompt as "Continue from: …".
/// 5. Fallback → empty Vec (same as the old behaviour when no prompts).
pub(super) fn derive_open_questions(
    observations: &[sessionmunch_core::Observation],
    last_prompt: &Option<String>,
) -> Vec<String> {
    let last_of_kind = |kind: ObservationKind| -> Option<&sessionmunch_core::Observation> {
        observations.iter().rev().find(|o| o.kind == kind)
    };

    let last_prompt_obs = last_of_kind(ObservationKind::UserPrompt);
    let last_tool = last_of_kind(ObservationKind::PostToolUse);
    let last_stop = last_of_kind(ObservationKind::Stop);
    let has_session_end = observations
        .iter()
        .any(|o| o.kind == ObservationKind::SessionEnd);

    // Heuristic 1: last prompt is a question → it is the open question.
    if let Some(p) = last_prompt_obs {
        let body = p.body.trim();
        if body.ends_with('?') || body.ends_with('？') {
            return vec![format!("Unresolved question: {}", cap_handoff_text(body))];
        }
    }

    // Heuristic 2: file activity with no subsequent Stop → the session ended
    // abnormally while working in the tree.
    //
    // The signal is the tool *family*, not the tool name. A PostToolUse
    // observation's title is `canonical_tool_name(tool_family)`, which is
    // only ever "file" / "search-list" / "non-file" / "unknown" — the
    // reserved protocol deliberately carries no raw tool names. Matching
    // "edit"/"write"/"patch" against that title can never succeed.
    //
    // That same closed schema means read and write are indistinguishable:
    // `ToolFamily::File` covers both, and `ToolOutcome` is only
    // success/error/unknown. So this cannot honestly claim changes were
    // made — only that the session touched files and then ended without a
    // normal Stop, which is worth telling the receiver either way.
    if let Some(tool) = last_tool {
        let touched_files = tool.title == canonical_tool_name(ToolFamily::File);
        if touched_files && last_stop.is_none() {
            return vec![
                "Session ended without a normal stop while working with files".into(),
                "Check the working tree for uncommitted or unverified changes".into(),
            ];
        }
    }

    // Heuristic 3 & 4: use the last *substantive* prompt.
    if let Some(p) = last_prompt {
        let body = p.trim();
        if !body.is_empty() && !is_acknowledgment(body) {
            // Heuristic 3 signal: no SessionEnd → flag as mid-task.
            if !has_session_end && last_stop.is_some() {
                return vec![format!(
                    "Continue from (mid-task exit): {}",
                    cap_handoff_text(body)
                )];
            }
            // Heuristic 4: normal end.
            return vec![format!("Continue from: {}", cap_handoff_text(body))];
        }
    }

    // Fallback: no substantive prompt → empty (same as old behaviour).
    Vec::new()
}

/// Whether a prompt is a pure acknowledgment with no substantive content.
/// Used by [`derive_open_questions`] to skip "ok" / "thanks" / "好的" so
/// the handoff does not carry a useless `Continue from: 好的` line.
///
/// Intentionally conservative: only short, well-known phrases are caught.
/// Anything longer than 20 chars or containing a verb-like word is kept.
pub(super) fn is_acknowledgment(text: &str) -> bool {
    let lower = text.to_lowercase();
    let trimmed = lower.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Short single-word/phrase acknowledgments.
    const ACK_PHRASES: &[&str] = &[
        "ok",
        "okay",
        "okk",
        "k",
        "kk",
        "thanks",
        "thank you",
        "thx",
        "ty",
        "got it",
        "done",
        "great",
        "nice",
        "cool",
        "perfect",
        "sure",
        "sounds good",
        "will do",
        "understood",
        // CJK acknowledgments.
        "好的",
        "好",
        "嗯",
        "谢谢",
        "收到",
        "明白",
        "了解",
        "可以的",
    ];
    if trimmed.chars().count() <= 20 && ACK_PHRASES.contains(&trimmed) {
        return true;
    }
    // Also catch "ok, thanks" / "好的，谢谢" style compounds up to 20 chars.
    if trimmed.chars().count() <= 20 && ACK_PHRASES.iter().any(|phrase| trimmed.starts_with(phrase))
    {
        let remainder = trimmed
            .trim_start_matches(|c: char| c.is_ascii_alphanumeric() || !c.is_ascii())
            .trim_start_matches([',', '.', ' ', '，', '。']);
        if remainder.is_empty() {
            return true;
        }
    }
    false
}

/// Cap so a single long prompt does not blow up the handoff body.
/// Mirrors the inner `cap` in [`build_auto_handoff`] but is standalone so
/// [`derive_open_questions`] can use it without access to the closure.
pub(super) fn cap_handoff_text(s: &str) -> String {
    const MAX: usize = 1500;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

/// Whether a failed LLM consolidation may degrade to the rule-based
/// checkpoint that a zero-LLM install would have written anyway.
///
/// Only the "the model could not answer" class degrades: a provider refusing
/// the request (context overflow, rate limit, outage) or returning an
/// unmappable response says nothing about whether this session deserves a page.
///
/// Everything else is policy or infrastructure: an admission webhook rejecting
/// this actor or scope, a wiki/store failure, or a session that does not
/// resolve. Those fail closed. The fallback write keeps the `consolidate`
/// admission operation, so it cannot bypass operation-specific policy.
pub(super) fn checkpoint_degrades_to_synth(error: &ConsolidatorError) -> bool {
    matches!(error, ConsolidatorError::Llm(_))
}

/// Write a fresh `sessions/<id>.md` for the current session without
/// ending it. Used by the PreCompact and PostCompaction branches to checkpoint
/// state before/after the agent's working context collapses.
pub(super) async fn consolidate_or_synth(
    state: &HookState,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    agent_kind: AgentKind,
    checkpoint_label: &str,
    actor: sessionmunch_core::ActorContext,
) -> anyhow::Result<()> {
    let fallback_from_llm = state.consolidator.is_some();
    if let Some(c) = state.consolidator.as_ref() {
        let result = c
            // The hook path has no per-call override, so `None` lets the
            // project's standing `_prompts/consolidation.md` preferences
            // apply; the actor is the session's own operator so the
            // checkpoint is attributed to them, not to whoever delivered
            // the event.
            .consolidate_session(session_id, false, actor.clone(), None, None)
            .await;
        match result {
            Ok(outcome) => {
                debug!(
                    session = %session_id,
                    path = %outcome.path,
                    "{}: LLM consolidation written",
                    checkpoint_label
                );
                let _ = state
                    .wiki
                    .commit_all(&format!(
                        "{}(session {}): checkpoint",
                        checkpoint_label,
                        short_id(&session_id.to_string()),
                    ))
                    .map_err(|e| {
                        tracing::warn!(
                            error = %e,
                            "{}: checkpoint auto-commit failed",
                            checkpoint_label
                        );
                        e
                    });
                return Ok(());
            }
            // Nothing to checkpoint. The rule-based path below no-ops on the
            // same condition, so this is success, not a failure worth logging.
            Err(ConsolidatorError::EmptySession(_)) => return Ok(()),
            Err(e) if checkpoint_degrades_to_synth(&e) => {
                warn!(
                    error = %e,
                    session = %session_id,
                    "{}: LLM consolidation unavailable; falling back to rule-based checkpoint",
                    checkpoint_label
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    let observations = state.reader.observations_for_session(session_id).await?;
    if is_ephemeral_session(&observations) {
        return Ok(());
    }
    let new_page = synthesize_session_page(
        workspace_id,
        project_id,
        session_id,
        agent_kind,
        &observations,
    );
    state
        .wiki
        .write_page(sessionmunch_wiki::WritePageRequest {
            workspace_id: new_page.workspace_id,
            project_id: new_page.project_id,
            path: new_page.path,
            frontmatter: new_page.frontmatter_json,
            body: new_page.body,
            tier: new_page.tier,
            pinned: new_page.pinned,
            title: None,
            admission_ctx: fallback_from_llm.then(|| AdmissionContext {
                op: AdmissionOp::Consolidate,
                actor: actor.clone(),
                ..Default::default()
            }),
            author_id: None,
            actor,
            evidence: Vec::new(),
        })
        .await?;
    let _ = state
        .wiki
        .commit_all(&format!(
            "{}(session {}): checkpoint",
            checkpoint_label,
            short_id(&session_id.to_string()),
        ))
        .map_err(|e| {
            tracing::warn!(error = %e, "{}: checkpoint auto-commit failed", checkpoint_label);
            e
        });
    debug!(session = %session_id, "{}: rule-based checkpoint written", checkpoint_label);
    Ok(())
}

pub(super) fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

pub(super) const fn importance_for(event: HookEvent) -> u8 {
    match event {
        HookEvent::SessionStart | HookEvent::SessionEnd => 7,
        HookEvent::UserPrompt => 8,
        HookEvent::PostToolUse | HookEvent::PreToolUse => 5,
        HookEvent::Stop | HookEvent::PreCompact | HookEvent::PostCompaction => 6,
        HookEvent::Notification
        | HookEvent::Other
        | HookEvent::SubagentStart
        | HookEvent::SubagentStop => 3,
    }
}
