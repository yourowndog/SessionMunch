//! Admin routes: bootstrap.

use super::*;

pub(super) async fn handle_bootstrap(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<BootstrapRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Dry-runs are READ-ONLY: no LLM call, no project creation, no
    // wiki write, no git commit. Early-return BEFORE any state-touching
    // call so a smoke-test (e.g. `--dry-run` from a tempdir) cannot
    // pollute the project list with throwaway names. Dry-runs can also
    // proceed in parallel with anything else — no mutex needed.
    if req.dry_run {
        return dry_run_outcome(
            req.sources,
            req.sources_collected,
            req.max_input_tokens,
            req.chunk_input_tokens,
        );
    }

    // Live runs from here on need LLM + workspace/project resolution.
    if state.llm.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "LLM provider not configured on server"
            })),
        ));
    }
    let (ws, proj) = create_ws_proj(&state, &req.workspace, &req.project).await?;

    // Serialise live bootstrap runs. Two parallel `process_sources`
    // calls would race the wiki's `commit_all` (libgit2 ops on the
    // same repo) and stack LLM cost unnecessarily. The wait is
    // operator-visible only in log lines; the request stays open
    // until the lock is acquired, then proceeds normally.
    if state.bootstrap_lock.try_lock().is_err() {
        info!(
            "another bootstrap is in progress; queueing — \
             this request waits for the active one to finish"
        );
    }
    let _bootstrap_guard = state.bootstrap_lock.lock().await;

    let llm = Arc::clone(
        state
            .llm
            .as_ref()
            .expect("llm is Some: checked above (non-dry-run without LLM returns 503)"),
    );

    let cfg = BootstrapConfig {
        // repo_path is unused by process_sources — the path field is
        // only consumed by collect_sources on the client side.
        repo_path: std::path::PathBuf::new(),
        workspace_id: ws,
        project_id: proj,
        max_input_tokens: req.max_input_tokens,
        chunk_input_tokens: req.chunk_input_tokens,
        sources_collected: req.sources_collected,
        // The individual include_* flags don't matter here: sources
        // are already collected; process_sources ignores them.
        include_git: true,
        include_readme: true,
        include_docs: true,
        include_code: true,
        since: None,
        dry_run: req.dry_run,
        force: req.force,
        resume: req.resume,
    };

    let bootstrap = Bootstrap {
        reader: state.reader.clone(),
        wiki: state.wiki.clone(),
        llm,
        writer: state.writer.clone(),
    };

    match bootstrap.process_sources(&cfg, req.sources).await {
        Ok(outcome) => Ok((
            StatusCode::OK,
            Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
        )),
        Err(e) => Err(bootstrap_error_response(e)),
    }
}

/// Build a dry-run [`BootstrapOutcome`] without an LLM by applying the
/// same budget-pruning logic that `Bootstrap::process_sources` would use.
/// Map a [`BootstrapError`] to the appropriate HTTP status code.
///
/// - `NoSources` / `AlreadyBootstrapped` → 422 (validation failures).
/// - `Llm` → 502 (upstream provider failure).
/// - Everything else → 500 (unexpected server-side error).
pub(super) fn bootstrap_error_response(
    e: sessionmunch_consolidate::BootstrapError,
) -> (StatusCode, Json<serde_json::Value>) {
    use sessionmunch_consolidate::BootstrapError;
    let status = match &e {
        BootstrapError::NoSources | BootstrapError::AlreadyBootstrapped => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        BootstrapError::Llm(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    log_server_error(status, "bootstrap", &e);
    (status, Json(serde_json::json!({ "error": e.to_string() })))
}

/// Record a failure the server owns.
///
/// A 5xx says the request was fine and *we* could not serve it, so the reason
/// belongs in the server's log: the response body reaches one client once and
/// is gone when its process exits, which is how an upstream provider 404
/// managed to fail every bootstrap while the log showed only the run starting
/// (#692). A 4xx stays quiet — the caller was told, and the caller was at
/// fault.
pub(super) fn log_server_error(status: StatusCode, operation: &str, error: &dyn std::fmt::Display) {
    if status.is_server_error() {
        warn!(%status, operation, %error, "admin request failed");
    }
}

/// Build a dry-run [`BootstrapOutcome`] without an LLM by applying the
/// same budget-pruning logic that `Bootstrap::process_sources` would use.
pub(super) fn dry_run_outcome(
    sources: Vec<BootstrapSource>,
    sources_collected: Option<usize>,
    max_input_tokens: usize,
    chunk_input_tokens: usize,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    use sessionmunch_consolidate::BootstrapError;
    if sources.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": BootstrapError::NoSources.to_string()
            })),
        ));
    }
    let incoming = sources.len();
    let (kept, _dropped, total) = prune_sources_to_budget(sources, max_input_tokens);
    if kept.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": BootstrapError::NoSources.to_string()
            })),
        ));
    }
    let collected = sources_collected.unwrap_or(incoming);
    let sources_sent = kept.len();
    let sources_dropped = collected.saturating_sub(sources_sent);
    let counts = SourceCounts::from_sources(&kept);
    let chunk_budget =
        sessionmunch_consolidate::effective_chunk_budget(chunk_input_tokens, max_input_tokens);
    let llm_chunks =
        sessionmunch_consolidate::plan_bootstrap_chunks(kept.clone(), chunk_budget).len();
    let outcome = BootstrapOutcome {
        sources_collected: collected,
        sources_sent,
        sources_dropped,
        sources_by_kind: counts,
        estimated_input_tokens: total,
        pages_written: Vec::new(),
        rationale: "(dry-run; LLM not invoked)".to_string(),
        dry_run: true,
        llm_chunks,
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}
