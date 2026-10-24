//! Admin routes: auto-improve telemetry report.

use super::*;

pub(super) async fn handle_auto_improve_report(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<AutoImproveTelemetryReportRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
    let params = AutoImproveTelemetryParams {
        since_days: req.since_days,
        top_limit: req.limit.clamp(1, 100),
    };
    let report = run_auto_improve_telemetry_report(
        &state.reader,
        ws,
        proj,
        &req.workspace,
        &req.project,
        params.clone(),
    )
    .await
    .map_err(|e| internal_err(e.to_string()))?;
    if req.stage {
        let body_markdown = render_auto_improve_telemetry_report_markdown(&report);
        let target_path = auto_improve_report_target_path();
        let target = PagePath::new(target_path).map_err(|e| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "error": format!("invalid auto-improve report target path: {e}") })),
            )
        })?;
        let operation = target_operation_for_page(&state, ws, proj, &target).await?;
        let staged = state
            .writer
            .stage_auto_improve_run_for_owner(
                StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: None,
                provider: None,
                model: None,
                summary: Some(format!("Auto-improve telemetry report: {}", report.summary)),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({
                    "mode": "stage",
                    "auto_improve_report": true,
                    "params": params,
                }),
                proposal_actor: sessionmunch_core::ActorContext {
                    agent: Some("auto_improve_report".into()),
                    ..sessionmunch_core::ActorContext::default()
                },
                proposals: vec![NewAutoImproveProposal {
                    operation,
                    target_path: target,
                    kind: "auto_improve_report".into(),
                    title: "Auto-improve Telemetry Report".into(),
                    confidence: 1.0,
                    rationale: "Telemetry report only; approval writes the report page and performs no learning-memory changes.".into(),
                    evidence_json: serde_json::json!({
                        "summary": report.summary.clone(),
                        "findings": report.findings.clone(),
                        "params": report.params.clone(),
                    }),
                    body_markdown,
                    artifact_sha256: None,
                    edit_mode: None,
                    patch_json: None,
                    expected_base_body_sha256: None,
                }],
                },
                None,
            )
            .await
            .map_err(|e| internal_err(e.to_string()))?;
        let sidecar_paths =
            write_auto_improve_sidecars(&state, ws, proj, &staged.proposal_ids).await?;
        return Ok((
            StatusCode::OK,
            Json(
                serde_json::to_value(AutoImproveTelemetryReportStageResponse {
                    run_id: staged.run_id.to_string(),
                    proposal_ids: staged
                        .proposal_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    sidecar_paths,
                    skipped: staged.skipped,
                    report,
                })
                .unwrap_or_else(|_| serde_json::json!({})),
            ),
        ));
    }
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}

pub(super) async fn handle_curator(
    State(state): State<Arc<AdminState>>,
    axum::Extension(tuning): axum::Extension<SweepTuning>,
    Json(req): Json<CuratorRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let mode = req.mode.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let wants_stage = req.stage || matches!(mode, Some("stage"));
    let explicit_dry_run = req.dry_run || matches!(mode, Some("dry_run" | "dry-run"));
    if wants_stage && explicit_dry_run {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "choose either dry_run or stage, not both" })),
        ));
    }
    if let Some(other) = mode
        && !matches!(other, "stage" | "dry_run" | "dry-run")
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("unknown curator mode '{other}'") })),
        ));
    }
    let wants_dry_run = explicit_dry_run || !wants_stage;

    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
    let params = CuratorParams::default();
    let mut report = run_curator_report_with_breadth(
        &state.reader,
        ws,
        proj,
        &req.workspace,
        &req.project,
        params.clone(),
        tuning.breadth_weight,
    )
    .await
    .map_err(|e| internal_err(e.to_string()))?;
    if wants_dry_run {
        report.dry_run = true;
        return Ok((
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        ));
    }

    report.dry_run = false;
    let body_markdown = render_curator_report_markdown(&report);
    let target_path = curator_target_path();
    let target = PagePath::new(target_path).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid curator target path: {e}") })),
        )
    })?;
    let operation = target_operation_for_page(&state, ws, proj, &target).await?;

    let staged = state
            .writer
            .stage_auto_improve_run_for_owner(
                StageAutoImproveRun {
                    workspace_id: ws,
                    project_id: proj,
                    session_id: None,
                    provider: None,
                    model: None,
                    summary: Some(report.summary.clone()),
                    warnings_json: serde_json::json!([]),
                    rejected_candidates_json: serde_json::json!([]),
                    config_json: serde_json::json!({
                        "mode": "stage",
                        "curator": true,
                        "params": params,
                    }),
                    proposal_actor: sessionmunch_core::ActorContext {
                        agent: Some("curator".into()),
                        ..sessionmunch_core::ActorContext::default()
                    },
                    proposals: vec![NewAutoImproveProposal {
                        operation,
                        target_path: target,
                        kind: "curator_report".into(),
                        title: "Curator Report".into(),
                        confidence: 1.0,
                        rationale: "Rule-based curator report only; approval writes the report page and performs no maintenance actions.".into(),
                        evidence_json: serde_json::json!({
                            "summary": report.summary.clone(),
                            "findings": report.findings.clone(),
                        }),
                        body_markdown,
                        artifact_sha256: None,
                        edit_mode: None,
                        patch_json: None,
                        expected_base_body_sha256: None,
                    }],
                },
            None,
        )
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let sidecar_paths = write_auto_improve_sidecars(&state, ws, proj, &staged.proposal_ids).await?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(CuratorStageResponse {
                run_id: staged.run_id.to_string(),
                proposal_ids: staged
                    .proposal_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                sidecar_paths,
                skipped: staged.skipped,
                report,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}

pub(super) fn curator_target_path() -> String {
    let now = jiff::Timestamp::now().to_string();
    let date = now.get(0..10).unwrap_or("latest");
    format!("notes/curator-{date}.md")
}

pub(super) fn auto_improve_report_target_path() -> String {
    let stamp = jiff::Timestamp::now().as_microsecond();
    format!("notes/auto-improve-report-{stamp}.md")
}

/// List open handoffs so an operator can cancel one (#513).
///
/// `memory_handoff_cancel` requires an exact id and nothing exposed one, so a
/// backlog showed up as a count in `status` and could not be addressed.
/// Read-only, oldest first, and content-free: identity, provenance and age,
/// never the handoff body.
pub(super) async fn handle_open_handoffs_list(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<OpenHandoffsQuery>,
) -> impl IntoResponse {
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let limit = query.limit.clamp(1, 500);
    match state
        .reader
        .open_handoffs_for_project(ws, proj, limit)
        .await
    {
        Ok(handoffs) => (
            StatusCode::OK,
            Json(serde_json::json!({ "handoffs": handoffs })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// `POST /admin/handoffs/expire` request body.
#[derive(Deserialize)]
pub(super) struct ExpireHandoffsRequest {
    workspace: String,
    project: String,
    /// Mandatory. Without `confirm: true` the server returns 400.
    confirm: bool,
    /// Only expire handoffs at least this many days old. Omitted clears the
    /// whole open backlog for the scope.
    #[serde(default)]
    older_than_days: Option<u32>,
}

/// `POST /admin/handoffs/expire` — clear the open handoff backlog for one
/// scope.
///
/// Unlike the automatic sweep this does not spare manual or
/// different-directory handoffs. Those exemptions are what the leftover
/// backlog is made of, so honouring them here would expire nothing. It is a
/// state change rather than a delete: the summary and provenance survive and
/// the handoff simply stops being consumable (#513).
pub(super) async fn handle_expire_handoffs(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    Json(req): Json<ExpireHandoffsRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "expiring the handoff backlog requires confirm=true"
            })),
        );
    }
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await {
        Ok(ids) => ids,
        Err(e) => return e,
    };

    // Same owner scoping as cancel: a caller clears their own and shared
    // batons, never somebody else's.
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let owner_filter = sessionmunch_core::OwnerFilter::for_actor_context(&actor);
    let author_id = author_ext.map(|axum::Extension(u)| u);

    let older_than_us = req.older_than_days.map(|days| {
        jiff::Timestamp::now().as_microsecond() - i64::from(days) * 24 * 60 * 60 * 1_000_000
    });

    match state
        .writer
        .expire_open_handoffs(ws, proj, owner_filter, older_than_us, author_id)
        .await
    {
        Ok(expired) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "expired": expired,
                "workspace": req.workspace,
                "project": req.project,
            })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

pub(super) async fn handle_pending_writes_list(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<PendingWritesQuery>,
) -> impl IntoResponse {
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let status = match query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => match raw.parse::<AutoImproveProposalStatus>() {
            Ok(status) => Some(status),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": e.to_string() })),
                );
            }
        },
        None => None,
    };
    match state
        .reader
        .list_auto_improve_proposals(ws, proj, status, query.limit.clamp(1, 200))
        .await
    {
        Ok(list) => (
            StatusCode::OK,
            Json(serde_json::to_value(list).unwrap_or_else(|_| serde_json::json!([]))),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

pub(super) async fn pending_detail(
    state: &AdminState,
    raw_id: &str,
    query: &PendingWriteScopeQuery,
) -> Result<
    (
        WorkspaceId,
        ProjectId,
        sessionmunch_store::OwnedAutoImproveProposalDetail,
    ),
    (StatusCode, Json<serde_json::Value>),
> {
    let id: AutoImproveProposalId = raw_id.parse().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid proposal id: {e}") })),
        )
    })?;
    let (ws, proj) = lookup_ws_proj_no_create(state, &query.workspace, &query.project).await?;
    let detail = state
        .reader
        .auto_improve_proposal_detail_with_owner(ws, proj, id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "proposal not found in scope" })),
            )
        })?;
    Ok((ws, proj, detail))
}

pub(super) async fn handle_pending_write_detail(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
) -> impl IntoResponse {
    match pending_detail(&state, &id, &query).await {
        Ok((_, _, detail)) => (
            StatusCode::OK,
            Json(serde_json::to_value(detail).unwrap_or_else(|_| serde_json::json!({}))),
        ),
        Err(e) => e,
    }
}

pub(super) async fn handle_pending_write_diff(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
) -> impl IntoResponse {
    match pending_detail(&state, &id, &query).await {
        Ok((ws, proj, owned)) => {
            let detail = &owned.detail;
            let before = state
                .reader
                .page_body_by_ids(ws, proj, detail.summary.target_path.as_str())
                .await
                .ok()
                .flatten()
                .map(|p| p.body)
                .unwrap_or_default();
            let diff = format!(
                "--- before/{path}\n+++ after/{path}\n@@\n{before}\n--- proposed ---\n{after}\n",
                path = detail.summary.target_path.as_str(),
                before = before,
                after = detail.body_markdown
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({ "proposal_id": id, "diff": diff })),
            )
        }
        Err(e) => e,
    }
}

pub(super) async fn handle_pending_write_approve(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
) -> impl IntoResponse {
    let proposal_id: AutoImproveProposalId = match id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid proposal id: {e}") })),
            );
        }
    };
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(author_id)| author_id);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let admission_ctx = Some(AdmissionContext {
        op: AdmissionOp::WritePage,
        skip_webhooks,
        ..AdmissionContext::default()
    });
    match state
        .wiki
        .approve_auto_improve_proposal(ws, proj, proposal_id, actor, author_id, admission_ctx)
        .await
    {
        Ok(ApproveAutoImproveProposalResult::Approved { page_id }) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "approved", "page_id": page_id.to_string() })),
        ),
        Ok(ApproveAutoImproveProposalResult::Conflict) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "status": "conflict" })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

pub(super) async fn handle_pending_write_reject(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
    Json(body): Json<PendingRejectRequest>,
) -> impl IntoResponse {
    let proposal_id: AutoImproveProposalId = match id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid proposal id: {e}") })),
            );
        }
    };
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    match state
        .writer
        .reject_auto_improve_proposal(RejectAutoImproveProposal {
            workspace_id: ws,
            project_id: proj,
            proposal_id,
            reason: body.reason,
            actor,
            author_id: author_ext.map(|axum::Extension(author_id)| author_id),
        })
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "rejected" })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}
