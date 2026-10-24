//! Admin routes: open-sessions.

use super::*;

/// Query string for `GET /admin/open-sessions` — open (not yet ended)
/// sessions for one scope + agent, newest first. Backs the thin
/// `sessionmunch finalize-session` command, which posts synthetic
/// session-end hooks for whatever this returns.
#[derive(Debug, Deserialize)]
pub(super) struct OpenSessionsQuery {
    /// Workspace name (required).
    workspace: String,
    /// Project name (required).
    project: String,
    /// Agent kind filter, kebab-case (`codex`, `claude-code`, …).
    agent: String,
    /// When true, return every open session; otherwise just the newest.
    #[serde(default)]
    all: bool,
    /// Include sessions belonging to OTHER operators.
    ///
    /// Off by default because the caller of this endpoint (`finalize-session`)
    /// acts destructively on what it returns: it ends the session, synthesises
    /// a page from its observations and mints a handoff carrying its raw
    /// prompts. Picking "the newest open session in the scope" across everyone
    /// would do all of that to a colleague's live session. Sessions with no
    /// recorded operator stay visible either way, so a single-operator server
    /// is unaffected.
    #[serde(default)]
    all_owners: bool,
    /// When set, narrow to exactly this session id (still subject to the
    /// owner filter above) instead of newest-first selection — lets a
    /// caller that captured its own session id at spawn time finalize
    /// precisely that session, even when other sessions for the same agent
    /// are open concurrently in the same project (e.g. multiple terminal
    /// tabs each running Kiro CLI against one repo).
    #[serde(default)]
    session_id: Option<SessionId>,
}

/// Wire shape for one open session in the `GET /admin/open-sessions`
/// response.
#[derive(Debug, Serialize)]
pub(super) struct OpenSessionEntry {
    session_id: String,
    cwd: Option<String>,
}

/// Query string for `GET /admin/activity/by-client` — MCP tool-call
/// counts per client. Server-wide: MCP-only clients (the reason this
/// exists) are not reliably scoped to one project per call, and the
/// endpoint mirrors the buffer's own granularity.
///
/// `since_days = 0` means the whole history, mirroring
/// `/admin/sessions/by-agent`.
#[derive(Debug, Deserialize)]
pub(super) struct ActivityByClientQuery {
    #[serde(default)]
    since_days: u32,
}

pub(super) async fn handle_activity_by_client(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<ActivityByClientQuery>,
) -> impl IntoResponse {
    let since_day = (query.since_days > 0).then(|| {
        jiff::Timestamp::now()
            .as_microsecond()
            .saturating_sub(i64::from(query.since_days).saturating_mul(US_PER_DAY))
            .div_euclid(US_PER_DAY)
    });
    match state.reader.client_activity_since(since_day).await {
        Ok(rows) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(&rows)
                    .map(|list| serde_json::json!({ "by_client": list }))
                    .unwrap_or_else(|_| serde_json::json!({ "by_client": [] })),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Query string for `GET /admin/sessions/by-agent` — how many sessions each
/// agent CLI opened in one scope.
///
/// `since_days = 0` means "no lower bound": count the project's whole
/// history rather than an empty window, which is what a dashboard asking for
/// "all time" wants and what a `u32` cannot express as `None`.
#[derive(Debug, Deserialize)]
pub(super) struct SessionsByAgentQuery {
    /// Workspace name (required).
    workspace: String,
    /// Project name (required).
    project: String,
    /// Inclusive lookback in days; zero means all history.
    #[serde(default)]
    since_days: u32,
    /// Report every operator's sessions instead of just the caller's. Same
    /// recovery switch the other scoped admin reads expose.
    #[serde(default)]
    all_owners: bool,
}

/// Microseconds in a day, for the `since_days` window.
pub(super) const US_PER_DAY: i64 = 86_400_000_000;

pub(super) async fn handle_sessions_by_agent(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    Query(query): Query<SessionsByAgentQuery>,
) -> impl IntoResponse {
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let since_us = (query.since_days > 0).then(|| {
        jiff::Timestamp::now()
            .as_microsecond()
            .saturating_sub(i64::from(query.since_days).saturating_mul(US_PER_DAY))
    });
    let owner_filter = if query.all_owners {
        sessionmunch_core::OwnerFilter::Any
    } else {
        sessionmunch_core::OwnerFilter::for_actor_context(
            &actor_ext.map_or_else(sessionmunch_core::ActorContext::anonymous, |ext| ext.0),
        )
    };
    let counts: sessionmunch_store::StoreResult<Vec<sessionmunch_store::AgentSessionCount>> = state
        .reader
        .session_counts_by_agent(ws, proj, owner_filter, since_us)
        .await;
    match counts {
        Ok(counts) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(&counts)
                    .map(|list| serde_json::json!({ "by_agent": list }))
                    .unwrap_or_else(|_| serde_json::json!({ "by_agent": [] })),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Parse a kebab-case agent wire string into an [`AgentKind`].
pub(super) fn parse_agent_kind(raw: &str) -> Option<AgentKind> {
    AgentKind::ALL.into_iter().find(|kind| kind.as_str() == raw)
}

pub(super) async fn handle_open_sessions(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    Query(query): Query<OpenSessionsQuery>,
) -> impl IntoResponse {
    if query.all && query.session_id.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "all and session_id cannot be combined"
            })),
        );
    }
    let Some(agent) = parse_agent_kind(&query.agent) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("unknown agent kind: {}", query.agent)
            })),
        );
    };
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let owner_filter = if query.all_owners {
        sessionmunch_core::OwnerFilter::Any
    } else {
        sessionmunch_core::OwnerFilter::for_actor_context(
            &actor_ext.map_or_else(sessionmunch_core::ActorContext::anonymous, |ext| ext.0),
        )
    };
    let sessions = if let Some(session_id) = query.session_id {
        state
            .reader
            .open_session_for_scope_agent_by_id(ws, proj, agent, owner_filter, session_id)
            .await
            .map(|session| session.into_iter().collect())
    } else {
        let limit = if query.all { None } else { Some(1) };
        state
            .reader
            .open_sessions_for_scope_agent(ws, proj, agent, owner_filter, limit)
            .await
    };
    match sessions {
        Ok(sessions) => {
            let sessions: Vec<OpenSessionEntry> = sessions
                .into_iter()
                .map(|s| OpenSessionEntry {
                    session_id: s.session_id.to_string(),
                    cwd: s.cwd,
                })
                .collect();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(&sessions)
                        .map(|list| serde_json::json!({ "sessions": list }))
                        .unwrap_or_else(|_| serde_json::json!({ "sessions": [] })),
                ),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}
