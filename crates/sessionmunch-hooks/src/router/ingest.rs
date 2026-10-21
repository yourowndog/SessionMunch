//! `POST /hook` and `POST /hook/batch`: envelope inspection, subagent filtering and batch acknowledgement.

use super::*;

pub(super) async fn handle_hook(
    State(state): State<Arc<HookState>>,
    Query(query): Query<HookQuery>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Unconditional backstop (#196): drop any raw assistant-message field on the
    // `Value` before it becomes a `HookEnvelope`, so the field can never reach
    // `body_excerpt`, tracing, or the store — regardless of client version.
    crate::assistant_capture::strip_assistant_message_raw(&mut body);
    let mut env = HookEnvelope::from_query_and_body(query, body);
    // Consume the opt-in `_sessionmunch_assistant` marker and, when both opt-ins are
    // on for a supported Stop, populate the Stop body with the sanitized excerpt.
    // Any gate failure leaves an empty Stop with the same 202 "queued" response.
    crate::assistant_capture::apply_assistant_backstop(&mut env, state.capture_assistant_enabled);
    let Some(env) = inspect_capture_envelope(env) else {
        state.ingest_metrics.record_dropped_by_policy();
        return (StatusCode::ACCEPTED, "capture policy dropped");
    };
    // Accept-but-drop subagent captures (incl. the unmarked tail of tracked
    // subagent sessions) when the operator opts in. Returning 202 (not an error)
    // means the client treats the event as delivered and never retries/spools
    // it. Runs before the semaphore so a dropped event consumes no capacity.
    // The auth middleware in front of `/hook` injects the request's
    // [`ActorContext`] (rung 1 root, rung 2 DB user, or anonymous). We
    // capture the identity it names NOW — before the spawn drops the request
    // extensions — so `process()` can key the `ActiveProject` map by the
    // authenticated identity when `[auto_scope] mode = per_actor` is on.
    let actor = actor_identity(actor_ext);
    // Same reason: the skip-list header is read here, while the request
    // extensions still exist, and travels with the event into `process()`.
    let skip_webhooks = admission_skips(level_ext, &headers);
    let actor_storage_key = actor.as_ref().map(IdentityKey::storage_key);
    if should_drop_subagent(&state, &env).await {
        state.ingest_metrics.record_dropped_by_policy();
        return (StatusCode::ACCEPTED, "subagent capture dropped");
    }
    let Ok(permit) = state.ingest_semaphore.clone().try_acquire_owned() else {
        state.ingest_metrics.record_shed_saturated();
        warn!("hook ingest saturated; dropping event with 429");
        return (StatusCode::TOO_MANY_REQUESTS, "hook queue full");
    };
    let rate_key = ingest_rate_key(&env, actor_storage_key.as_deref());
    if !state
        .ingest_rate
        .lock()
        .await
        .try_take(&rate_key, std::time::Instant::now())
    {
        state.ingest_metrics.record_shed_rate_limited();
        warn!(source = %log_rate_key(&rate_key), "hook ingest rate limit exceeded for source; dropping event with 429");
        return (StatusCode::TOO_MANY_REQUESTS, "hook source rate limited");
    }
    state.ingest_metrics.record_accepted();
    tokio::spawn(async move {
        let _permit = permit;
        let metrics = state.ingest_metrics.clone();
        let persisted = process_envelope(
            state,
            env,
            actor,
            level_ext.map_or(sessionmunch_core::AuthLevel::Anonymous, |v| v.0),
            skip_webhooks,
        )
        .await;
        // Stamped only when `process_envelope` actually cleared the writer.
        // This is the signal an operator uses to tell "hooks are arriving but
        // nothing is landing" from "nothing is arriving" — the two look
        // identical from the accepted count alone — so a failed write must
        // NOT advance it, or a store that is rejecting every event still
        // reads as a healthy writer in `sessionmunch status`.
        if persisted {
            metrics.record_persisted(now_unix_ms());
        }
    });
    (StatusCode::ACCEPTED, "queued")
}

/// One event in a `POST /hook/batch` request — the same `{url, body}` pair a
/// single `POST /hook` would carry, so the server reuses the per-event query
/// parsing instead of inventing a second wire shape.
#[derive(Debug, Deserialize)]
pub struct HookBatchItem {
    /// Full hook URL including the `?event=…&agent=…` query (as the client
    /// spooled it); only the query is read here — the host/path are the
    /// client's record of where the event was bound.
    pub url: String,
    /// Raw JSON event payload.
    #[serde(default)]
    pub body: serde_json::Value,
}

/// Response to `POST /hook/batch`: legacy clients read the contiguous leading
/// prefix in `accepted`; newer clients prefer `accepted_indices` when present to
/// retain only non-contiguous items skipped by per-source rate limiting.
#[derive(Debug, Serialize)]
pub struct HookBatchAck {
    /// Contiguous leading prefix committed, oldest-first. Kept for old spool
    /// drains and as a safe lower bound when `accepted_indices` is absent.
    pub accepted: usize,
    /// Non-contiguous item indexes committed by a server new enough to keep
    /// scanning past per-source rate-limited items. Omitted when it is exactly
    /// the legacy accepted prefix so older clients keep working.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_indices: Option<Vec<usize>>,
    /// Item index that failed processing after earlier rate-limited skips. New
    /// spool drains charge this item, not the first unaccepted one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_index: Option<usize>,
}

impl HookBatchAck {
    pub(super) fn prefix(accepted: usize) -> Self {
        Self {
            accepted,
            accepted_indices: None,
            failed_index: None,
        }
    }

    pub(super) fn indexed(indices: Vec<usize>) -> Self {
        Self::indexed_with_empty(indices, false, None)
    }

    pub(super) fn indexed_full_scan(indices: Vec<usize>) -> Self {
        Self::indexed_with_empty(indices, true, None)
    }

    pub(super) fn indexed_failed(indices: Vec<usize>, failed_index: usize) -> Self {
        Self::indexed_with_empty(indices, true, Some(failed_index))
    }

    pub(super) fn indexed_with_empty(
        indices: Vec<usize>,
        include_empty_indices: bool,
        failed_index: Option<usize>,
    ) -> Self {
        let legacy_prefix = indices
            .iter()
            .copied()
            .enumerate()
            .take_while(|(pos, idx)| *pos == *idx)
            .count();
        let contiguous = indices
            .iter()
            .copied()
            .enumerate()
            .all(|(pos, idx)| pos == idx);
        Self {
            accepted: legacy_prefix,
            accepted_indices: if contiguous && !(include_empty_indices && indices.is_empty()) {
                None
            } else {
                Some(indices)
            },
            failed_index,
        }
    }
}

/// Batch sibling of [`handle_hook`]. Accepts many spooled events in ONE request
/// so a draining client amortizes the per-request cost (TLS + network RTT + the
/// edge auth hop) over the whole batch instead of paying it per event — the
/// dominant cost when a backlog drains to a remote, gated server, and the reason
/// a sequential per-event drain falls behind under parallel load.
///
/// Unlike `handle_hook` (which spawns and answers `202` immediately), stored
/// batch items are processed INLINE so every item's side effects (a SessionEnd
/// writes a session page + a handoff) stay inside the response window. Real
/// processing errors still fail-fast. Per-source rate-limit misses are different:
/// the item is skipped and later unrelated sources continue, with
/// `accepted_indices` telling new spool drains exactly which entries committed.
pub(super) async fn handle_hook_batch(
    State(state): State<Arc<HookState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: HeaderMap,
    Json(items): Json<Vec<HookBatchItem>>,
) -> impl IntoResponse {
    if items.len() > MAX_HOOK_BATCH_ITEMS {
        warn!(
            items = items.len(),
            max = MAX_HOOK_BATCH_ITEMS,
            "hook batch too large; rejecting before processing"
        );
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(HookBatchAck::prefix(0)));
    }
    // All items in a batch share the drain's single identity, so the actor is
    // captured once from the batch request (mirrors `handle_hook`).
    let actor = actor_identity(actor_ext);
    let skip_webhooks = admission_skips(level_ext, &headers);
    let actor_storage_key = actor.as_ref().map(IdentityKey::storage_key);
    let mut accepted_indices = Vec::new();
    let total_items = items.len();
    for (idx, mut item) in items.into_iter().enumerate() {
        // Same unconditional assistant-message backstop as `handle_hook`, applied
        // per item before the envelope is built (#196).
        crate::assistant_capture::strip_assistant_message_raw(&mut item.body);
        let query = parse_hook_query(&item.url);
        let mut env = HookEnvelope::from_query_and_body(query, item.body);
        crate::assistant_capture::apply_assistant_backstop(
            &mut env,
            state.capture_assistant_enabled,
        );
        let Some(env) = inspect_capture_envelope(env) else {
            // A protocol-directed drop is committed from the spool's point of
            // view, but intentionally spends neither ingress capacity nor a
            // source-rate token.
            state.ingest_metrics.record_dropped_by_policy();
            accepted_indices.push(idx);
            continue;
        };
        // Accept-but-drop subagent captures (see `handle_hook`): count the item
        // as committed so the client clears it from its spool, but do not store
        // it. Keeps the contiguous-prefix ack contract intact.
        if should_drop_subagent(&state, &env).await {
            state.ingest_metrics.record_dropped_by_policy();
            accepted_indices.push(idx);
            continue;
        }
        let Ok(permit) = state.ingest_semaphore.clone().try_acquire_owned() else {
            // The 429 rejects this item AND every item behind it, so count all
            // of them: on `/hook` one 429 is one shed event, and a batch that
            // sheds 200 events must not read as 1.
            let shed = total_items.saturating_sub(idx);
            for _ in 0..shed {
                state.ingest_metrics.record_shed_saturated();
            }
            warn!(
                accepted = accepted_indices.len(),
                shed, "hook batch ingest saturated; rejecting with 429"
            );
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(HookBatchAck::indexed(accepted_indices)),
            );
        };
        let rate_key = ingest_rate_key(&env, actor_storage_key.as_deref());
        if !state
            .ingest_rate
            .lock()
            .await
            .try_take(&rate_key, std::time::Instant::now())
        {
            drop(permit);
            state.ingest_metrics.record_shed_rate_limited();
            warn!(accepted = accepted_indices.len(), source = %log_rate_key(&rate_key), "hook batch source rate limited; skipping item and continuing");
            continue;
        }
        let _permit = permit;
        state.ingest_metrics.record_accepted();
        if let Err(e) = process_authorized(
            &state,
            env,
            actor.clone(),
            level_ext.map_or(sessionmunch_core::AuthLevel::Anonymous, |v| v.0),
            skip_webhooks.clone(),
        )
        .await
        {
            if matches!(
                e.downcast_ref::<StoreError>(),
                Some(StoreError::SessionCollision)
            ) {
                warn!("hook batch session collision/recovery rejection dropped");
                accepted_indices.push(idx);
                continue;
            }
            warn!(error = %e, accepted = accepted_indices.len(), "hook batch item failed; stopping (fail-fast)");
            return (
                StatusCode::OK,
                Json(HookBatchAck::indexed_failed(accepted_indices, idx)),
            );
        }
        // Stamped per item, for the same reason `handle_hook` stamps after
        // `process_envelope`: this is the point the event cleared the writer.
        state.ingest_metrics.record_persisted(now_unix_ms());
        accepted_indices.push(idx);
    }
    (
        StatusCode::OK,
        Json(HookBatchAck::indexed_full_scan(accepted_indices)),
    )
}

/// Apply the client capture protocol before any admission or store work.
/// `None` is an acknowledged Drop; `Some` is either the legacy envelope or a
/// strict metadata-only replacement. This deliberately never logs protocol or
/// raw tool data: those fields can contain paths and captured content.
pub(super) fn inspect_capture_envelope(env: HookEnvelope) -> Option<HookEnvelope> {
    let Some(raw_protocol) = env.raw.get("_sessionmunch_capture") else {
        return Some(env);
    };
    let cwd = env.cwd.as_deref().unwrap_or("/");
    let direct = |state| capture_inspector(state, cwd).inspect(env.agent, &env.raw, cwd);

    let Some(protocol) = CaptureProtocol::parse(raw_protocol) else {
        // A new/malformed marker must not make a recognized file operation
        // less private. Mark the replacement invalid: an inactive/keep
        // protocol would falsely describe the server's privacy fallback.
        // Non-file legacy payloads retain their old behavior.
        let decision = direct(PolicyState::Invalid);
        if decision.protocol().tool_family() == ToolFamily::File {
            return Some(metadata_envelope(env, &decision, None));
        }
        return Some(env);
    };

    // Parsed Drops are terminal client-side policy decisions. They must stay
    // admission-free even when their other protocol fields are nonsensical.
    if protocol.disposition() == CaptureDisposition::Drop {
        return None;
    }

    if protocol.disposition() == CaptureDisposition::MetadataOnly {
        return metadata_only_protocol_envelope(env, &protocol);
    }

    let decision = direct(protocol.policy_state());
    let inspected = decision.protocol();
    if !protocol_matches_inspection(&protocol, inspected) {
        // A client protocol that contradicts direct inspection cannot be used
        // to retain a recognized file tool's raw arguments or response. The
        // invalid decision also gives the replacement a canonical, truthful
        // invalid/metadata-only protocol rather than echoing the bad claim.
        if protocol.tool_family() == ToolFamily::File || inspected.tool_family() == ToolFamily::File
        {
            let fallback = direct(PolicyState::Invalid);
            return Some(metadata_envelope(env, &fallback, None));
        }
        return match inspected.disposition() {
            CaptureDisposition::Drop => None,
            CaptureDisposition::MetadataOnly => Some(metadata_envelope(env, &decision, None)),
            CaptureDisposition::Keep => Some(env),
        };
    }

    Some(env)
}

/// Validate and rebuild a client-stripped metadata body without attempting to
/// recover its intentionally omitted tool arguments. Invalid stripped shapes
/// are dropped rather than risking retention of an unallowlisted field.
pub(super) fn metadata_only_protocol_envelope(
    mut env: HookEnvelope,
    protocol: &CaptureProtocol,
) -> Option<HookEnvelope> {
    if !metadata_protocol_is_legal(protocol) {
        return None;
    }
    let object = env.raw.as_object()?;
    // Unknown fields are deliberately ignored below: the reconstructed body
    // contains only this allowlist, so a stale or malicious extra cannot leak.
    let valid_scalar = |key: &str, max: usize| {
        object.get(key).is_none_or(|value| {
            value
                .as_str()
                .is_some_and(|value| !value.is_empty() && value.len() <= max)
        })
    };
    if !valid_scalar("session_id", 512)
        || !valid_scalar("cwd", 4_096)
        || !object
            .get("tool_call_id")
            .is_none_or(valid_metadata_call_id)
        || object.get("tool_family") != Some(&serde_json::json!(protocol.tool_family()))
        || object.get("tool_name")
            != Some(&serde_json::json!(canonical_tool_name(
                protocol.tool_family()
            )))
    {
        return None;
    }

    let mut raw = serde_json::Map::new();
    for key in ["session_id", "cwd", "tool_call_id"] {
        if let Some(value) = object.get(key) {
            raw.insert(key.into(), value.clone());
        }
    }
    raw.insert(
        "tool_family".into(),
        serde_json::json!(protocol.tool_family()),
    );
    raw.insert(
        "tool_name".into(),
        serde_json::json!(canonical_tool_name(protocol.tool_family())),
    );
    raw.insert("_sessionmunch_capture".into(), serde_json::json!(protocol));
    env.title_hint = Some(canonical_tool_name(protocol.tool_family()).into());
    env.body_excerpt = metadata_summary_with_outcome(
        env.event,
        protocol.tool_family(),
        object.get("tool_call_id"),
        "unknown",
    );
    env.raw = serde_json::Value::Object(raw);
    Some(env)
}

pub(super) const fn metadata_protocol_is_legal(protocol: &CaptureProtocol) -> bool {
    matches!(
        (protocol.policy_state(), protocol.tool_family()),
        (PolicyState::Active | PolicyState::Invalid, ToolFamily::File)
    )
}

pub(super) fn valid_metadata_call_id(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(valid_call_id)
}

/// Whether a claimed protocol can be produced by the claimed policy state for
/// the directly inspected tool. Active, successfully-extracted file calls may
/// be either kept or dropped because the server intentionally does not receive
/// the client's private ignore patterns; every other disposition is fixed by
/// the shared decision table.
pub(super) fn protocol_matches_inspection(
    protocol: &CaptureProtocol,
    inspected: &CaptureProtocol,
) -> bool {
    if protocol.policy_state() != inspected.policy_state()
        || protocol.tool_family() != inspected.tool_family()
        || protocol.extraction_state() != inspected.extraction_state()
        || protocol.path_count() != inspected.path_count()
    {
        return false;
    }

    match (
        protocol.policy_state(),
        protocol.tool_family(),
        inspected.disposition(),
    ) {
        (PolicyState::Active, ToolFamily::File, CaptureDisposition::Keep) => matches!(
            protocol.disposition(),
            CaptureDisposition::Keep | CaptureDisposition::Drop
        ),
        _ => protocol.disposition() == inspected.disposition(),
    }
}

/// Build a policy solely to use the shared, fixture-backed direct schema
/// inspection API. The active sentinel cannot match a normalized real path;
/// active extracted file calls can therefore be either Keep or Drop during
/// protocol validation.
pub(super) fn capture_inspector(state: PolicyState, cwd: &str) -> CapturePolicy {
    const ACTIVE_SENTINEL: &str = "/__sessionmunch_capture_server_inspection_never_match__";
    match state {
        PolicyState::Active => CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec![ACTIVE_SENTINEL.into()],
            }),
            cwd,
            None,
        ),
        PolicyState::Invalid => CapturePolicy::resolve(CaptureSource::Invalid, cwd, None),
        PolicyState::Inactive => CapturePolicy::resolve(CaptureSource::Absent, cwd, None),
    }
}

pub(super) fn metadata_envelope(
    mut env: HookEnvelope,
    decision: &crate::capture_policy::CaptureDecision,
    protocol: Option<&CaptureProtocol>,
) -> HookEnvelope {
    let protocol = protocol.unwrap_or(decision.protocol());
    let outcome = tool_observation_outcome(env.agent, &env.raw);
    let mut raw = metadata_only_body(env.session_id.as_deref(), env.cwd.as_deref(), decision);
    if let Some(object) = raw.as_object_mut() {
        object.insert(
            "tool_family".into(),
            serde_json::json!(protocol.tool_family()),
        );
        object.insert(
            "tool_name".into(),
            serde_json::json!(canonical_tool_name(protocol.tool_family())),
        );
        object.insert("_sessionmunch_capture".into(), serde_json::json!(protocol));
    }
    // These were extracted from the pre-replacement raw body. Clearing them is
    // essential: normal extraction must never retain a removed payload.
    env.title_hint = Some(canonical_tool_name(protocol.tool_family()).into());
    env.body_excerpt = metadata_summary_with_outcome(
        env.event,
        protocol.tool_family(),
        raw.get("tool_call_id"),
        outcome.as_str(),
    );
    env.raw = raw;
    env
}

pub(super) fn metadata_summary_with_outcome(
    event: HookEvent,
    family: ToolFamily,
    call_id: Option<&serde_json::Value>,
    outcome: &str,
) -> Option<String> {
    if !matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse) {
        return None;
    }
    let mut body = format!("tool_family: {}", canonical_tool_name(family));
    if let Some(id) = call_id.and_then(serde_json::Value::as_str) {
        body.push_str("\ntool_call_id: ");
        body.push_str(id);
    }
    if event == HookEvent::PostToolUse {
        body.push_str("\noutcome: ");
        body.push_str(outcome);
    }
    Some(body)
}

pub(super) const fn canonical_tool_name(family: ToolFamily) -> &'static str {
    match family {
        ToolFamily::File => "file",
        ToolFamily::SearchList => "search-list",
        ToolFamily::NonFile => "non-file",
        ToolFamily::Unknown => "unknown",
    }
}

/// Decide whether to accept-but-drop this event under `drop_subagent_captures`,
/// maintaining the subagent-session set. Returns `true` to drop. Seeds the
/// session on `SubagentStart` and on any marker-bearing event; keeps it through
/// `SubagentStop`; and drops the **unmarked tail** (`user_prompt_submit` /
/// `stop` / `session_end`) of a session already known to be a subagent. No-op
/// (returns `false`) unless this event's project opted in via the per-event
/// `drop_subagent` flag (sourced from its `.sessionmunch.toml`).
pub(super) async fn should_drop_subagent(state: &HookState, env: &HookEnvelope) -> bool {
    if !env.drop_subagent_requested {
        return false;
    }
    let Ok(session_id) = resolve_session_id(env) else {
        return false;
    };
    let Ok((workspace_id, project_id)) = resolve_project_ids_inner(
        state,
        env.cwd.as_deref(),
        env.workspace_override.as_deref(),
        env.project_override.as_deref(),
        env.project_strategy,
    )
    .await
    else {
        return false;
    };
    let key = SubagentSessionKey {
        workspace_id,
        project_id,
        session_id,
    };
    let marked = matches!(
        env.event,
        HookEvent::SubagentStart | HookEvent::SubagentStop
    ) || body_is_subagent(&env.raw);

    if marked {
        state.subagent_sessions.lock().await.insert(key);
        return true;
    }

    let tracked = state.subagent_sessions.lock().await.contains(&key);
    if tracked && matches!(env.event, HookEvent::SessionEnd) {
        state.subagent_sessions.lock().await.remove(&key);
    }
    tracked
}

/// Parse the `?event=…&agent=…` query of a spooled hook URL into [`HookQuery`],
/// mirroring axum's `Query` extractor (both use `serde_urlencoded`). A URL with
/// no query, or an unparseable one, yields the default query; downstream
/// fail-fast batch handling decides whether that default envelope can be stored.
pub(super) fn parse_hook_query(url: &str) -> HookQuery {
    let qs = url.split_once('?').map_or("", |(_, q)| q);
    serde_urlencoded::from_str(qs).unwrap_or_default()
}
