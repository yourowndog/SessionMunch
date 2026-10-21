//! Admin routes: move-project.

use super::*;

/// JSON request body for `POST /admin/move-project`.
#[derive(Deserialize)]
pub(super) struct MoveProjectRequest {
    /// Source workspace. Must already exist; we 404 if absent.
    pub(super) from_workspace: String,
    /// Project name to move. Must already exist in `from_workspace`; 404 if absent.
    pub(super) project: String,
    /// Destination workspace. Auto-created if absent.
    pub(super) to_workspace: String,
    /// Mandatory confirmation flag. The move PURGES the source after
    /// copying, so without `confirm: true` the server returns 400.
    pub(super) confirm: bool,
    /// Override the active-project guard. By default the server refuses (409)
    /// to move the project the hook router is currently writing to, since a
    /// live session's next observation would carry a stale workspace id.
    /// `force: true` proceeds anyway (still safe: the move republishes the
    /// active pointer and the (workspace_id, project_id) trigger makes any
    /// stale write fail cleanly rather than corrupt). It never overrides a
    /// live managed-workstream lease in the destructive copy-purge path.
    #[serde(default)]
    pub(super) force: bool,
    /// Policy for the copy-purge MERGE path when a source page's path already
    /// exists in the destination with DIFFERENT content. Ignored by true-move
    /// (no copy). Default `block` — the safe choice for a destructive op.
    #[serde(default)]
    pub(super) on_conflict: OnConflict,
}

/// What to do when a merged page path collides with an existing destination
/// page of different content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub(super) enum OnConflict {
    /// Abort the whole move (source untouched), listing the conflicting paths.
    /// The operator resolves them or picks another policy explicitly.
    #[default]
    Block,
    /// The source page supersedes the destination page at the same path (the
    /// destination's prior version becomes history).
    Overwrite,
    /// Keep BOTH: the source page lands under a de-duplicated path
    /// (`<stem>-from-<src_workspace>.md`), reported in `conflicts`.
    Duplicate,
}

/// Wire-format report returned by `POST /admin/move-project`.
#[derive(Debug, Serialize)]
pub struct MoveProjectReport {
    /// `from_workspace/project` label.
    pub from: String,
    /// `to_workspace/project` label.
    pub to: String,
    /// `true` when the destination workspace already held a same-named
    /// project — the copy MERGED into it rather than creating a fresh one.
    pub merged_into_existing: bool,
    /// How the move was performed:
    /// - `"true-move"`: a lossless cross-workspace re-stamp (same `project_id`,
    ///   one SQL transaction + one dir rename). Used when the destination has
    ///   no same-named project. Sessions/observations/handoffs and the full
    ///   supersession history all survive; nothing is re-embedded.
    /// - `"copy-purge"`: the destination already held a same-named project, so
    ///   the source's latest pages were copied in and merged, then the source
    ///   purged. Only durable pages move; episodic rows are dropped.
    pub moved_via: &'static str,
    /// Number of latest pages copied into the destination (copy-purge) or
    /// re-stamped in place (true-move).
    pub pages_copied: u64,
    /// Managed workstreams re-stamped in place by a lossless true move.
    /// Copy-purge reports zero because it does not transfer managed history.
    pub workstreams_moved: u64,
    /// Source paths whose on-disk file could not be read (copy skipped).
    /// When non-empty the source is NOT purged so a fixed re-run is safe.
    pub pages_skipped: Vec<String>,
    /// Whether the source project was purged (only when every page copied).
    pub source_purged: bool,
    /// Source `pages` rows deleted by the purge (all versions).
    pub source_pages_deleted: u64,
    /// Source `sessions` rows deleted by the purge.
    pub source_sessions_deleted: u64,
    /// Source `observations` rows deleted by the purge.
    pub source_observations_deleted: u64,
    /// Source `handoffs` rows deleted by the purge.
    pub source_handoffs_deleted: u64,
    /// Source `page_embeddings` rows deleted by the purge.
    pub source_embeddings_deleted: u64,
    /// Source on-disk dirs removed.
    pub files_deleted: Vec<String>,
    /// Source on-disk dirs that could not be removed (non-fatal).
    pub files_failed: Vec<String>,
    /// Same-path conflicts in the copy-purge merge: a source page whose path
    /// already existed in the destination (with different content) was landed
    /// under a de-duplicated path so BOTH survive. Each entry is the original
    /// source path and the de-duplicated destination path it was written to.
    pub conflicts: Vec<PathConflict>,
    /// Pre-move checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-move checkpoint, if the move changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// One same-path collision resolved by de-duplicating the source page's path.
#[derive(Debug, Serialize)]
pub struct PathConflict {
    /// The source page's original path (also the destination's existing path).
    pub path: String,
    /// The de-duplicated path the source page was written to instead.
    pub moved_to: String,
}

/// An error response from a move: the HTTP status + JSON body a handler would
/// have returned directly. The move core returns this as the `Err` arm so a
/// single move can be surfaced verbatim by `handle_move_project` or aggregated
/// by `handle_merge_workspace`.
pub(super) type MoveErr = (StatusCode, Json<serde_json::Value>);

/// Lossless cross-workspace move: under the wiki mutation gate, rename the
/// project's on-disk dir to the destination workspace, then re-stamp its
/// `workspace_id` across every domain table (one transaction, same
/// `project_id`). The caller has already verified the destination has no
/// same-named project.
pub(super) async fn true_move_project(
    state: &Arc<AdminState>,
    req: &MoveProjectRequest,
    src_ws: WorkspaceId,
    src_proj: ProjectId,
    pre_checkpoint: Option<String>,
    actor: sessionmunch_core::ActorContext,
) -> Result<MoveProjectReport, MoveErr> {
    // Ensure the destination workspace ROW exists (FK target for the
    // re-stamp) without creating a new project — the existing project_id is
    // what we move.
    let dst_ws = match state
        .writer
        .get_or_create_workspace(req.to_workspace.clone())
        .await
    {
        Ok(ws) => ws,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // A true move targets a FRESH destination, so its dir must not already
    // exist. Wiki::move_project_workspace repeats this check under the
    // exclusive mutation guard before it renames anything.
    let dst_dir = state.wiki.project_root(dst_ws, src_proj);
    if dst_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "destination dir already exists: {}; refusing true-move",
                    dst_dir.display()
                )
            })),
        ));
    }

    let move_ctx = AdmissionContext {
        workspace: req.from_workspace.clone(),
        project: req.project.clone(),
        destination_workspace: Some(req.to_workspace.clone()),
        destination_project: Some(req.project.clone()),
        op: AdmissionOp::MoveProject,
        actor,
        ..Default::default()
    };

    // Wiki owns the critical section: it runs move admission, renames the dir,
    // re-stamps SQLite, and rolls the dir back on SQL failure while normal page
    // writes/reindexes are blocked by the same process-local gate.
    let summary = match state
        .wiki
        .move_project_workspace(src_proj, src_ws, dst_ws, Some(move_ctx))
        .await
    {
        Ok(s) => s,
        Err(WikiError::Store(StoreError::NotFound(msg))) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": msg })),
            ));
        }
        Err(e @ WikiError::DestinationExists(_)) => {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
        Err(e) => {
            warn!(error = %e, "true-move: move aborted before completion");
            return Err(internal_err(format!("move aborted (nothing changed): {e}")));
        }
    };

    // Keep the in-process active-project pointer correct: the project_id is
    // unchanged, only its workspace moved. If a hook had published this project
    // as active, republish it under the destination workspace so the next event
    // resolves cleanly (rather than tripping the pairing trigger first).
    state
        .active_project
        .retarget_project_workspace(src_proj, dst_ws);
    // Proactively drop any stale per-cwd cache entry for the moved project.
    invalidate_scope_cache(state, ScopeInvalidation::Project(src_proj)).await;

    if let Err(e) = state.wiki.backfill_scope_manifests().await {
        warn!(error = %e, "true-move: scope-manifest backfill failed after move");
    }
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "move-project {}/{} -> {}/{}",
            req.from_workspace, req.project, req.to_workspace, req.project
        ),
    );

    let report = MoveProjectReport {
        from: format!("{}/{}", req.from_workspace, req.project),
        to: format!("{}/{}", req.to_workspace, req.project),
        merged_into_existing: false,
        moved_via: "true-move",
        pages_copied: summary.pages_moved,
        workstreams_moved: summary.workstreams_moved,
        pages_skipped: Vec::new(),
        // Nothing is purged in a true move — the source rows ARE the
        // destination rows, just re-stamped.
        source_purged: false,
        source_pages_deleted: 0,
        source_sessions_deleted: 0,
        source_observations_deleted: 0,
        source_handoffs_deleted: 0,
        source_embeddings_deleted: 0,
        files_deleted: Vec::new(),
        files_failed: Vec::new(),
        // A true move never copies pages, so it never has a path conflict.
        conflicts: Vec::new(),
        pre_checkpoint,
        checkpoint,
    };
    Ok(report)
}

/// Token inserted between the original stem and the source-workspace
/// slug when copy-purge merges conflicting page paths
/// (`<stem>-from-<src_workspace>.md`). The literal also appears in
/// `docs/lifecycle-ops.md`; keep these two in sync.
pub(super) const DEDUP_FROM_TOKEN: &str = "-from-";

/// Char allowlist for the source-workspace slug embedded in a deduped
/// destination path. ASCII alphanumeric plus `-` / `_` keeps the slug
/// safe inside a filesystem path component on every supported platform
/// (Windows treats `:`, `*`, `?`, `<`, `>`, `|` as illegal; Linux
/// tolerates more but UTF-8 mojibake in filenames is a maintenance
/// hazard). Everything else collapses to a single `-` separator,
/// preserving readability without introducing path-traversal vectors.
pub(super) fn is_dedup_slug_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Pick a destination path that collides with neither an existing destination
/// page nor one already claimed in this run. Keeps the source page's stem and
/// appends `<DEDUP_FROM_TOKEN><src_workspace>` (then `-2`, `-3`, …). Used by
/// the copy-purge merge to keep BOTH pages when a path conflicts.
pub(super) async fn dedup_dest_path(
    state: &AdminState,
    dst_ws: WorkspaceId,
    dst_proj: ProjectId,
    src_path: &str,
    src_workspace: &str,
    used: &std::collections::HashSet<String>,
) -> PagePath {
    let (stem, ext) = match src_path.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (src_path.to_string(), String::new()),
    };
    let slug: String = src_workspace
        .chars()
        .map(|c| if is_dedup_slug_char(c) { c } else { '-' })
        .collect();
    let base = format!("{stem}{DEDUP_FROM_TOKEN}{slug}");
    let mut n = 0u32;
    loop {
        let cand = if n == 0 {
            format!("{base}{ext}")
        } else {
            format!("{base}-{n}{ext}")
        };
        let collides = used.contains(&cand)
            || matches!(
                state.reader.page_body_by_ids(dst_ws, dst_proj, &cand).await,
                Ok(Some(_))
            );
        if !collides && let Ok(p) = PagePath::new(cand) {
            return p;
        }
        n += 1;
    }
}

pub(super) fn page_copy_differs(
    existing: &sessionmunch_store::StoredPageBody,
    source: &Markdown,
    source_title: &str,
    source_tier: Tier,
    source_pinned: bool,
) -> bool {
    // Frontmatter comparison is modulo the per-version `generated.at`,
    // matching the store's own idempotency projection: two pages whose
    // only difference is WHEN they were written are the same content.
    // Comparing it raw made merge-conflict detection timing-flaky — the
    // same seeded page written across a second boundary 409'd as
    // "different content" (found via the copy_purge flake on Windows).
    let existing_frontmatter: serde_json::Value =
        serde_json::from_str(&existing.frontmatter_json).unwrap_or_default();
    existing.body != source.body
        || sessionmunch_core::okf::strip_generated_at(&existing_frontmatter)
            != sessionmunch_core::okf::strip_generated_at(&source.frontmatter)
        || existing.title != source_title
        || existing.tier != source_tier.as_str()
        || existing.pinned != source_pinned
}

#[cfg(test)]
mod page_copy_differs_tests;

pub(super) async fn handle_move_project(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<sessionmunch_core::ActorContext>>,
    Json(req): Json<MoveProjectRequest>,
) -> impl IntoResponse {
    // Carry the authenticated actor into both legs (move admission + the
    // source-purge admission) so a scope-guard webhook authorizes by user.
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(sessionmunch_core::ActorContext::anonymous);
    match move_project_core(&state, &req, actor).await {
        Ok(report) => (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        ),
        Err(resp) => resp,
    }
}

/// Validate one move request, pick the true-move vs copy-purge path, and
/// execute it — returning the report as data (or the HTTP error a handler
/// surfaces verbatim). Shared by `handle_move_project` (one move) and
/// `handle_merge_workspace` (a move per source project).
pub(super) async fn move_project_core(
    state: &Arc<AdminState>,
    req: &MoveProjectRequest,
    actor: sessionmunch_core::ActorContext,
) -> Result<MoveProjectReport, MoveErr> {
    // Destructive: it purges the source after copying. Require confirm.
    if !req.confirm {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "destructive operation requires confirm=true"
            })),
        ));
    }

    // A same-workspace "move" would get-or-create the SAME project as both
    // source and destination, copy it onto itself, then purge it — data
    // loss. Reject; in-workspace renames go through rename-project.
    if req.from_workspace == req.to_workspace {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "from_workspace and to_workspace are identical; use rename-project instead"
            })),
        ));
    }

    // Resolve the SOURCE without auto-creating — 404 on a typo.
    let (src_ws, src_proj) =
        lookup_ws_proj_no_create(state, &req.from_workspace, &req.project).await?;

    // Live-session guard: refuse to move the project the hook router is
    // currently writing to. A live session's next observation/log would carry
    // the now-stale workspace id (the (workspace_id, project_id) trigger would
    // make it fail, but the operator should consciously opt in). `force: true`
    // proceeds — safe because the move republishes the active pointer below.
    if !req.force && state.active_project.contains_project(src_proj) {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{}/{} is the active session's project; a live move risks stale-cache writes. \
                     Re-run with force=true to proceed.",
                    req.from_workspace, req.project
                )
            })),
        ));
    }

    // Detect MERGE: does the destination workspace already hold a same-named
    // project? (find_workspace may be None when the dest ws doesn't exist yet.)
    let merged_into_existing = match state.reader.find_workspace(req.to_workspace.clone()).await {
        Ok(Some(dst_ws)) => matches!(
            state.reader.find_project(dst_ws, req.project.clone()).await,
            Ok(Some(_))
        ),
        Ok(None) => false,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    let pre_checkpoint = checkpoint_or_500(
        &state.wiki,
        format!(
            "pre-move-project {}/{} -> {}/{}",
            req.from_workspace, req.project, req.to_workspace, req.project
        ),
    )?;

    // FRESH destination (no same-named project there) → lossless TRUE MOVE.
    // Re-stamp the source project's workspace_id across every domain table in
    // one transaction (same project_id), then rename its on-disk dir. This
    // keeps sessions/observations/handoffs and the full supersession history,
    // and is O(1) instead of O(pages) — no per-page re-write, re-embed, or
    // admission webhook. The copy+purge path below is reserved for the MERGE
    // case, where two project_ids can't be re-stamped into one
    // (UNIQUE(workspace_id, name) collision).
    if !merged_into_existing {
        return true_move_project(state, req, src_ws, src_proj, pre_checkpoint, actor).await;
    }

    // MERGE: the destination already holds a same-named project. Get-or-create
    // it (auto-creating the destination workspace) and copy the source's
    // latest pages into it, then purge the source.
    let (dst_ws, dst_proj) = create_ws_proj(state, &req.to_workspace, &req.project).await?;

    copy_purge_merge(
        state,
        req,
        src_ws,
        src_proj,
        dst_ws,
        dst_proj,
        pre_checkpoint,
        actor,
    )
    .await
}
