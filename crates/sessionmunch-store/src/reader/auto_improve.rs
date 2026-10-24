//! Auto-improvement proposals, rejections and telemetry.

use super::*;

impl ReaderPool {
    /// List auto-improvement proposals for one scope, optionally filtered by status.
    pub async fn list_auto_improve_proposals(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        status: Option<AutoImproveProposalStatus>,
        limit: usize,
    ) -> StoreResult<Vec<AutoImproveProposalSummary>> {
        self.with_conn(move |conn| {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let sql = if status.is_some() {
                "SELECT id, run_id, workspace_id, project_id, status, operation, target_path, \
                        kind, title, confidence, staged_at, decided_at \
                 FROM auto_improve_proposals \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND status = ?3 \
                 ORDER BY staged_at DESC LIMIT ?4"
            } else {
                "SELECT id, run_id, workspace_id, project_id, status, operation, target_path, \
                        kind, title, confidence, staged_at, decided_at \
                 FROM auto_improve_proposals \
                 WHERE workspace_id = ?1 AND project_id = ?2 \
                 ORDER BY staged_at DESC LIMIT ?3"
            };
            let mut stmt = conn.prepare(sql)?;
            let mut out = Vec::new();
            if let Some(status) = status {
                let rows = stmt.query_map(
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        status.as_str(),
                        limit
                    ],
                    summary_from_row,
                )?;
                for row in rows {
                    out.push(row?);
                }
            } else {
                let rows = stmt.query_map(
                    params![workspace_id.as_bytes(), project_id.as_bytes(), limit],
                    summary_from_row,
                )?;
                for row in rows {
                    out.push(row?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Read one proposal by id, failing closed when the scope does not match.
    pub async fn auto_improve_proposal_detail(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        proposal_id: AutoImproveProposalId,
    ) -> StoreResult<Option<AutoImproveProposalDetail>> {
        self.auto_improve_proposal_detail_with_owner(workspace_id, project_id, proposal_id)
            .await
            .map(|detail| detail.map(|owned| owned.detail))
    }

    /// Read one proposal plus the operator that staged it, failing closed when
    /// the scope does not match.
    pub async fn auto_improve_proposal_detail_with_owner(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        proposal_id: AutoImproveProposalId,
    ) -> StoreResult<Option<OwnedAutoImproveProposalDetail>> {
        self.with_conn(move |conn| {
            let row = conn
                .query_row(
                    "SELECT id, run_id, workspace_id, project_id, status, operation, target_path, \
                            kind, title, confidence, staged_at, decided_at, rationale, \
                            evidence_json, body_markdown, body_sha256, artifact_path, \
                            artifact_sha256, target_latest_page_id_at_stage, \
                            target_body_sha256_at_stage, target_updated_at_at_stage, \
                            decision_reason, decided_by_author_id, decided_by_actor_json, \
                            applied_page_id, checkpoint, edit_mode, patch_json, \
                            expected_base_body_sha256, materialized_base_body_sha256, \
                            staged_by_actor_user \
                     FROM auto_improve_proposals \
                     WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
                    params![
                        proposal_id.as_bytes(),
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                    ],
                    |row| {
                        let summary = summary_from_row(row)?;
                        let evidence_raw: String = row.get(13)?;
                        let body_hash = bytes32(row.get(15)?).map_err(to_sql_err)?;
                        let artifact_hash = opt_bytes32(row.get(17)?).map_err(to_sql_err)?;
                        let staged_page_id = row
                            .get::<_, Option<Vec<u8>>>(18)?
                            .map(|b| PageId::from_slice(&b))
                            .transpose()
                            .map_err(to_sql_err)?;
                        let staged_body_hash = opt_bytes32(row.get(19)?).map_err(to_sql_err)?;
                        let decided_author = row
                            .get::<_, Option<Vec<u8>>>(22)?
                            .map(|b| UserId::from_slice(&b))
                            .transpose()
                            .map_err(to_sql_err)?;
                        let decided_actor_raw: Option<String> = row.get(23)?;
                        let applied_page_id = row
                            .get::<_, Option<Vec<u8>>>(24)?
                            .map(|b| PageId::from_slice(&b))
                            .transpose()
                            .map_err(to_sql_err)?;
                        let patch_raw: Option<String> = row.get(27)?;
                        Ok(OwnedAutoImproveProposalDetail {
                            detail: AutoImproveProposalDetail {
                                summary,
                                rationale: row.get(12)?,
                                evidence_json: serde_json::from_str(&evidence_raw)
                                    .map_err(to_sql_err)?,
                                body_markdown: row.get(14)?,
                                body_sha256: body_hash,
                                artifact_path: row.get(16)?,
                                artifact_sha256: artifact_hash,
                                target_latest_page_id_at_stage: staged_page_id,
                                target_body_sha256_at_stage: staged_body_hash,
                                target_updated_at_at_stage: row.get(20)?,
                                decision_reason: row.get(21)?,
                                decided_by_author_id: decided_author,
                                decided_by_actor_json: decided_actor_raw
                                    .map(|raw| serde_json::from_str(&raw))
                                    .transpose()
                                    .map_err(to_sql_err)?,
                                applied_page_id,
                                checkpoint: row.get(25)?,
                                edit_mode: row.get(26)?,
                                patch_json: patch_raw
                                    .map(|raw| serde_json::from_str(&raw))
                                    .transpose()
                                    .map_err(to_sql_err)?,
                                expected_base_body_sha256: opt_bytes32(row.get(28)?)
                                    .map_err(to_sql_err)?,
                                materialized_base_body_sha256: opt_bytes32(row.get(29)?)
                                    .map_err(to_sql_err)?,
                                events: Vec::new(),
                            },
                            staged_by_actor_user: row.get(30)?,
                        })
                    },
                )
                .optional()?;
            let Some(mut detail) = row else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(
                "SELECT id, proposal_id, event, actor_json, author_id, detail_json, at \
                 FROM auto_improve_proposal_events \
                 WHERE proposal_id = ?1 ORDER BY at ASC, id ASC",
            )?;
            let rows = stmt.query_map(params![proposal_id.as_bytes()], |row| {
                let proposal_id = AutoImproveProposalId::from_slice(&row.get::<_, Vec<u8>>(1)?)
                    .map_err(to_sql_err)?;
                let actor_raw: String = row.get(3)?;
                let detail_raw: String = row.get(5)?;
                let author_id = row
                    .get::<_, Option<Vec<u8>>>(4)?
                    .map(|b| UserId::from_slice(&b))
                    .transpose()
                    .map_err(to_sql_err)?;
                Ok(AutoImproveProposalEvent {
                    id: row.get(0)?,
                    proposal_id,
                    event: row.get(2)?,
                    actor_json: serde_json::from_str(&actor_raw).map_err(to_sql_err)?,
                    author_id,
                    detail_json: serde_json::from_str(&detail_raw).map_err(to_sql_err)?,
                    at: row.get(6)?,
                })
            })?;
            for row in rows {
                detail.detail.events.push(row?);
            }
            Ok(Some(detail))
        })
        .await
    }

    /// Read recent rejection-buffer entries for one workspace/project scope.
    pub async fn recent_auto_improve_rejections(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        limit: usize,
        since_created_at: Option<i64>,
    ) -> StoreResult<Vec<AutoImproveRejectionSummary>> {
        self.with_conn(move |conn| {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let since = since_created_at.unwrap_or(0);
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, project_id, target_path, kind, operation, edit_mode, \
                        reason, normalized_fingerprint, summary, evidence_json, source_run_id, \
                        source_proposal_id, created_at \
                 FROM auto_improve_rejections \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND created_at >= ?3 \
                 ORDER BY created_at DESC LIMIT ?4",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes(), since, limit],
                |row| {
                    let id_bytes: Vec<u8> = row.get(0)?;
                    let id = Uuid::from_slice(&id_bytes).map_err(to_sql_err)?.to_string();
                    let workspace_id =
                        WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(1)?).map_err(to_sql_err)?;
                    let project_id =
                        ProjectId::from_slice(&row.get::<_, Vec<u8>>(2)?).map_err(to_sql_err)?;
                    let evidence_raw: String = row.get(10)?;
                    let source_run_id = row
                        .get::<_, Option<Vec<u8>>>(11)?
                        .map(|b| AutoImproveRunId::from_slice(&b))
                        .transpose()
                        .map_err(to_sql_err)?;
                    let source_proposal_id = row
                        .get::<_, Option<Vec<u8>>>(12)?
                        .map(|b| AutoImproveProposalId::from_slice(&b))
                        .transpose()
                        .map_err(to_sql_err)?;
                    Ok(AutoImproveRejectionSummary {
                        id,
                        workspace_id,
                        project_id,
                        target_path: row.get(3)?,
                        kind: row.get(4)?,
                        operation: row.get(5)?,
                        edit_mode: row.get(6)?,
                        reason: row.get(7)?,
                        normalized_fingerprint: row.get(8)?,
                        summary: row.get(9)?,
                        evidence_json: serde_json::from_str(&evidence_raw).map_err(to_sql_err)?,
                        source_run_id,
                        source_proposal_id,
                        created_at: row.get(13)?,
                    })
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Aggregate auto-improvement telemetry for one workspace/project scope.
    ///
    /// `since_created_at` filters by run/proposal/rejection creation time in Unix
    /// microseconds. Maintenance/report proposals are excluded from learning
    /// metrics and counted separately.
    pub async fn auto_improve_telemetry_aggregate(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        since_created_at: i64,
        top_limit: usize,
    ) -> StoreResult<AutoImproveTelemetryAggregate> {
        self.with_conn(move |conn| {
            let top_limit = i64::try_from(top_limit).unwrap_or(i64::MAX);
            let run_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM auto_improve_runs
                 WHERE workspace_id = ?1 AND project_id = ?2 AND created_at >= ?3",
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    since_created_at
                ],
                |row| row.get(0),
            )?;
            let runs_with_learning_proposals: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT run_id) FROM auto_improve_proposals
                 WHERE workspace_id = ?1 AND project_id = ?2 AND staged_at >= ?3
                   AND kind NOT IN ('curator_report', 'auto_improve_report')",
                params![
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    since_created_at
                ],
                |row| row.get(0),
            )?;
            Ok(AutoImproveTelemetryAggregate {
                run_count: usize::try_from(run_count).unwrap_or(usize::MAX),
                runs_with_learning_proposals: usize::try_from(runs_with_learning_proposals)
                    .unwrap_or(usize::MAX),
                proposals_by_status: auto_improve_group_counts(
                    conn,
                    "status",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                proposals_by_operation: auto_improve_group_counts(
                    conn,
                    "operation",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                proposals_by_edit_mode: auto_improve_group_counts(
                    conn,
                    "edit_mode",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                proposals_by_kind: auto_improve_group_counts(
                    conn,
                    "kind",
                    workspace_id,
                    project_id,
                    since_created_at,
                    None,
                )?,
                maintenance_proposals_by_kind: auto_improve_group_counts(
                    conn,
                    "kind",
                    workspace_id,
                    project_id,
                    since_created_at,
                    Some("kind IN ('curator_report', 'auto_improve_report')"),
                )?,
                top_targets: auto_improve_group_counts(
                    conn,
                    "target_path",
                    workspace_id,
                    project_id,
                    since_created_at,
                    Some("kind NOT IN ('curator_report', 'auto_improve_report')"),
                )?
                .into_iter()
                .take(usize::try_from(top_limit).unwrap_or(usize::MAX))
                .collect(),
                rejections_by_reason: auto_improve_rejection_group_counts(
                    conn,
                    "r.reason",
                    workspace_id,
                    project_id,
                    since_created_at,
                    top_limit,
                )?,
                repeated_rejection_fingerprints: auto_improve_repeated_rejection_fingerprints(
                    conn,
                    workspace_id,
                    project_id,
                    since_created_at,
                    top_limit,
                )?,
                rejected_targets: auto_improve_rejection_group_counts(
                    conn,
                    "COALESCE(r.target_path, '(none)')",
                    workspace_id,
                    project_id,
                    since_created_at,
                    top_limit,
                )?,
            })
        })
        .await
    }
}
