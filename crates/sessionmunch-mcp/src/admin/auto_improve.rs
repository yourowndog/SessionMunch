//! Admin routes: auto-improve reviewer/stager.

use super::*;

pub(super) async fn handle_auto_improve(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    author_ext: Option<axum::Extension<sessionmunch_core::UserId>>,
    level_ext: Option<axum::Extension<sessionmunch_core::AuthLevel>>,
    headers: HeaderMap,
    Json(req): Json<AutoImproveRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if req.dry_run.is_some() || req.stage.is_some() || req.mode.is_some() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "auto-improve dry_run/stage/mode request fields were removed; set [auto_improve].require_approval = true for manual review"
            })),
        ));
    }
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
    let Some(llm) = state.llm.as_ref().cloned() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "LLM provider not configured on server"
            })),
        ));
    };

    let cfg = AutoImproveReviewConfig {
        min_observations: req.min_observations,
        min_session_duration_secs: req.min_session_duration_secs,
        min_confidence: req.min_confidence,
        max_input_tokens: req.max_input_tokens,
        max_proposals_per_run: req.max_proposals_per_run,
        include_raw_fallback: req.include_raw_fallback,
        proposal_actor: req.proposal_actor.clone(),
        pending_path: req.pending_path.clone(),
        max_patchable_pages: req.max_patchable_pages,
        max_patchable_body_chars: req.max_patchable_body_chars,
        max_edits_per_proposal: req.max_edits_per_proposal,
        max_edit_content_chars: req.max_edit_content_chars,
        max_changed_chars_per_proposal: req.max_changed_chars_per_proposal,
        max_patch_edits_per_run: req.max_patch_edits_per_run,
        max_rejection_context: req.max_rejection_context,
        rejection_context_days: req.rejection_context_days,
        max_final_body_chars: req.max_final_body_chars,
        max_rule_page_tokens: req.max_rule_page_tokens,
        max_procedure_page_tokens: req.max_procedure_page_tokens,
        eval: req
            .eval
            .clone()
            .unwrap_or_else(|| state.auto_improve_review_config.eval.clone()),
    };

    let report =
        run_auto_improve_review(&state.reader, &*llm, ws, proj, req.session_id, cfg.clone())
            .await
            .map_err(auto_improve_error_response)?;
    // Whose suggestion this is. It scopes the one-pending-per-target rule
    // (V42), and is taken from the authenticated actor's qualified identity
    // key rather than a `users` row, because most identified operators on a
    // shared server are named by an authenticating proxy and never get one.
    //
    // Only where operators are actually told apart, though: on a
    // single-operator server this call would otherwise stage into bucket
    // `user:<root_username>` while the scheduler and the report handlers stage
    // into the unattributed one, leaving two proposals pending for the same
    // page — the collision V42 promises cannot happen.
    //
    // Both halves go through the shared accessors — `identity_key` for "which
    // human is this", `owner_identity` for "does this deployment name them" —
    // rather than re-deriving either here. Its `memory_auto_improve` sibling
    // stages into the same V42 bucket, and a bucket computed two ways is a
    // bucket that eventually disagrees with itself.
    let staging_owner = sessionmunch_core::owner_identity(
        actor_ext
            .as_ref()
            .and_then(|axum::Extension(actor)| actor.identity_key())
            .as_ref(),
        state
            .reader
            .distinguishes_operators(state.trusted_proxy_identity)
            .await
            .map_err(|e| internal_err(e.to_string()))?,
    );
    let proposals = auto_improve_new_proposals(&state, ws, proj, &report).await?;
    let staged = stage_auto_improve_report(
        &state,
        AutoImproveStagingScope {
            ws,
            proj,
            session_id: req.session_id,
            cfg: &cfg,
            staging_owner,
        },
        &report,
        proposals,
    )
    .await?;
    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(author_id)| author_id);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let outcomes = finalize_auto_improve_proposals(
        &state,
        &staged,
        AutoImproveFinalizeContext {
            workspace_id: ws,
            project_id: proj,
            require_approval: state.auto_improve_require_approval,
            actor,
            author_id,
            admission_ctx: Some(AdmissionContext {
                op: AdmissionOp::WritePage,
                skip_webhooks,
                ..AdmissionContext::default()
            }),
        },
    )
    .await?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(AutoImproveStageResponse {
                run_id: staged.run_id.to_string(),
                approval_required: state.auto_improve_require_approval,
                approval_policy: if state.auto_improve_require_approval {
                    "manual".into()
                } else {
                    "auto_approve".into()
                },
                session_id: req.session_id.to_string(),
                summary: report.summary,
                warnings: report.warnings,
                rejected_candidates_count: report.rejected_candidates.len(),
                proposals: outcomes,
                // Store-level collisions the reviewer's run never staged.
                // Dropped silently, a run reporting N-1 proposals has nothing
                // saying the Nth ever existed.
                skipped: staged.skipped,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}

pub(super) fn auto_improve_auto_approve_actor(
    mut actor: sessionmunch_core::ActorContext,
) -> sessionmunch_core::ActorContext {
    actor.agent = Some("auto_improve_auto_approve".into());
    actor
}

pub(super) struct AutoImproveFinalizeContext {
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    require_approval: bool,
    actor: sessionmunch_core::ActorContext,
    author_id: Option<sessionmunch_core::UserId>,
    admission_ctx: Option<AdmissionContext>,
}

pub(super) async fn finalize_auto_improve_proposals(
    state: &AdminState,
    staged: &StagedAutoImproveData,
    ctx: AutoImproveFinalizeContext,
) -> Result<Vec<AutoImproveProposalOutcome>, (StatusCode, Json<serde_json::Value>)> {
    let mut outcomes = Vec::with_capacity(staged.proposal_ids.len());
    for (proposal_id, sidecar_path) in staged.proposal_ids.iter().zip(staged.sidecar_paths.iter()) {
        if ctx.require_approval {
            outcomes.push(AutoImproveProposalOutcome {
                id: proposal_id.to_string(),
                sidecar_path: sidecar_path.clone(),
                status: "pending".into(),
                page_id: None,
            });
            continue;
        }

        let approval_actor = auto_improve_auto_approve_actor(ctx.actor.clone());
        match state
            .wiki
            .approve_auto_improve_proposal(
                ctx.workspace_id,
                ctx.project_id,
                *proposal_id,
                approval_actor,
                ctx.author_id,
                ctx.admission_ctx.clone(),
            )
            .await
        {
            Ok(ApproveAutoImproveProposalResult::Approved { page_id }) => {
                outcomes.push(AutoImproveProposalOutcome {
                    id: proposal_id.to_string(),
                    sidecar_path: sidecar_path.clone(),
                    status: "approved".into(),
                    page_id: Some(page_id.to_string()),
                });
            }
            Ok(ApproveAutoImproveProposalResult::Conflict) => {
                outcomes.push(AutoImproveProposalOutcome {
                    id: proposal_id.to_string(),
                    sidecar_path: sidecar_path.clone(),
                    status: "conflict".into(),
                    page_id: None,
                });
            }
            Err(e) => return Err(internal_err(e.to_string())),
        }
    }
    Ok(outcomes)
}

pub(super) async fn auto_improve_new_proposals(
    state: &AdminState,
    ws: WorkspaceId,
    proj: ProjectId,
    report: &sessionmunch_consolidate::AutoImproveReport,
) -> Result<Vec<NewAutoImproveProposal>, (StatusCode, Json<serde_json::Value>)> {
    let mut proposals = Vec::with_capacity(report.proposals.len());
    for p in &report.proposals {
        let path = PagePath::new(p.path.clone()).map_err(|e| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "error": format!("invalid proposal path: {e}") })),
            )
        })?;
        let target_exists = state
            .reader
            .page_body_by_ids(ws, proj, path.as_str())
            .await
            .map_err(|e| internal_err(e.to_string()))?
            .is_some();
        let operation = if p.edit_mode == "patch"
            || (target_exists && path.as_str() == "_slots/current-focus.md")
        {
            AutoImproveProposalOperation::Update
        } else {
            AutoImproveProposalOperation::Create
        };
        let expected_base_body_sha256 = p
            .expected_base_body_sha256
            .as_deref()
            .map(hex_to_sha256)
            .transpose()
            .map_err(|e| internal_err(format!("invalid expected_base_body_sha256: {e}")))?;
        proposals.push(NewAutoImproveProposal {
            operation,
            target_path: path,
            kind: p.kind.clone(),
            title: p.title.clone(),
            confidence: f64::from(p.confidence),
            rationale: p.rationale.clone(),
            evidence_json: serde_json::to_value(&p.evidence)
                .map_err(|e| internal_err(e.to_string()))?,
            body_markdown: p.body_markdown.clone(),
            artifact_sha256: None,
            edit_mode: Some(p.edit_mode.clone()),
            patch_json: serde_json::to_value(&p.edits).ok(),
            expected_base_body_sha256,
        });
    }
    Ok(proposals)
}

pub(super) struct StagedAutoImproveData {
    run_id: sessionmunch_core::AutoImproveRunId,
    proposal_ids: Vec<AutoImproveProposalId>,
    sidecar_paths: Vec<String>,
    /// Proposals the store refused to stage. Dropping these here would hand the
    /// operator a run reporting N-1 proposals with nothing saying the Nth
    /// existed — the silent drop the per-proposal skip was meant to end.
    skipped: Vec<SkippedProposal>,
}

/// Everything `stage_auto_improve_report` needs about WHO and WHERE, bundled so
/// the helper keeps a readable arity.
pub(super) struct AutoImproveStagingScope<'a> {
    ws: WorkspaceId,
    proj: ProjectId,
    session_id: SessionId,
    cfg: &'a AutoImproveReviewConfig,
    /// Typed operator that staged the run; also scopes the
    /// one-pending-per-target rule.
    staging_owner: Option<sessionmunch_core::IdentityKey>,
}

pub(super) async fn stage_auto_improve_report(
    state: &AdminState,
    scope: AutoImproveStagingScope<'_>,
    report: &sessionmunch_consolidate::AutoImproveReport,
    proposals: Vec<NewAutoImproveProposal>,
) -> Result<StagedAutoImproveData, (StatusCode, Json<serde_json::Value>)> {
    let AutoImproveStagingScope {
        ws,
        proj,
        session_id,
        cfg,
        staging_owner,
    } = scope;
    let staged = state
        .writer
        .stage_auto_improve_run_for_owner(
            StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: Some(session_id),
                provider: Some(report.provider.clone()),
                model: Some(report.model.clone()),
                summary: Some(report.summary.clone()),
                warnings_json: serde_json::to_value(&report.warnings)
                    .unwrap_or_else(|_| serde_json::json!([])),
                rejected_candidates_json: serde_json::to_value(&report.rejected_candidates)
                    .unwrap_or_else(|_| serde_json::json!([])),
                config_json: serde_json::json!({
                    "min_observations": cfg.min_observations,
                    "min_session_duration_secs": cfg.min_session_duration_secs,
                    "min_confidence": cfg.min_confidence,
                    "max_input_tokens": cfg.max_input_tokens,
                    "max_proposals_per_run": cfg.max_proposals_per_run,
                    "include_raw_fallback": cfg.include_raw_fallback,
                    "max_patchable_pages": cfg.max_patchable_pages,
                    "max_patchable_body_chars": cfg.max_patchable_body_chars,
                    "max_edits_per_proposal": cfg.max_edits_per_proposal,
                    "max_edit_content_chars": cfg.max_edit_content_chars,
                    "max_changed_chars_per_proposal": cfg.max_changed_chars_per_proposal,
                    "max_patch_edits_per_run": cfg.max_patch_edits_per_run,
                    "max_rejection_context": cfg.max_rejection_context,
                    "rejection_context_days": cfg.rejection_context_days,
                    "max_final_body_chars": cfg.max_final_body_chars,
                    "max_rule_page_tokens": cfg.max_rule_page_tokens,
                    "max_procedure_page_tokens": cfg.max_procedure_page_tokens,
                    "eval": cfg.eval,
                }),
                proposal_actor: sessionmunch_core::ActorContext {
                    agent: Some(cfg.proposal_actor.clone()),
                    ..sessionmunch_core::ActorContext::default()
                },
                proposals,
            },
            staging_owner,
        )
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let sidecar_paths = write_auto_improve_sidecars(state, ws, proj, &staged.proposal_ids).await?;
    Ok(StagedAutoImproveData {
        run_id: staged.run_id,
        proposal_ids: staged.proposal_ids,
        sidecar_paths,
        skipped: staged.skipped,
    })
}

pub(super) async fn target_operation_for_page(
    state: &AdminState,
    ws: WorkspaceId,
    proj: ProjectId,
    target: &PagePath,
) -> Result<AutoImproveProposalOperation, (StatusCode, Json<serde_json::Value>)> {
    if state
        .reader
        .page_body_by_ids(ws, proj, target.as_str())
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .is_some()
    {
        Ok(AutoImproveProposalOperation::Update)
    } else {
        Ok(AutoImproveProposalOperation::Create)
    }
}

pub(super) async fn write_auto_improve_sidecars(
    state: &AdminState,
    ws: WorkspaceId,
    proj: ProjectId,
    proposal_ids: &[AutoImproveProposalId],
) -> Result<Vec<String>, (StatusCode, Json<serde_json::Value>)> {
    let mut sidecar_paths = Vec::with_capacity(proposal_ids.len());
    for id in proposal_ids {
        let path = state
            .wiki
            .write_auto_improve_sidecar(ws, proj, *id)
            .await
            .map_err(|e| internal_err(e.to_string()))?;
        sidecar_paths.push(path.display().to_string());
    }
    Ok(sidecar_paths)
}

pub(super) fn auto_improve_error_response(
    e: sessionmunch_consolidate::AutoImproveError,
) -> (StatusCode, Json<serde_json::Value>) {
    use sessionmunch_consolidate::AutoImproveError;
    let status = match &e {
        AutoImproveError::SessionNotFound(_) => StatusCode::NOT_FOUND,
        AutoImproveError::SessionOutOfScope { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        AutoImproveError::Llm(_) => StatusCode::BAD_GATEWAY,
        AutoImproveError::Eval(_) => StatusCode::BAD_GATEWAY,
        AutoImproveError::Memory(_) => StatusCode::BAD_REQUEST,
        AutoImproveError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    log_server_error(status, "auto-improve", &e);
    (status, Json(serde_json::json!({ "error": e.to_string() })))
}
