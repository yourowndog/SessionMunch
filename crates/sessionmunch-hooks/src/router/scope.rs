//! Project resolution for hook events, including sticky-scope rules within a session tree.

use super::*;

/// Build the `project_cache` key from the resolved cwd, overrides, and
/// project strategy. Shared by `resolve_project_ids` (insert/lookup) and
/// `process` (eviction on the stale-cache retry) so the two always agree on
/// the slot.
pub(super) fn cache_key_for(
    cwd_norm: Option<&str>,
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_strategy: ProjectStrategy,
) -> (String, String, String, String) {
    (
        cwd_norm.unwrap_or_default().to_string(),
        workspace_override.unwrap_or_default().to_string(),
        project_override.unwrap_or_default().to_string(),
        project_strategy.as_str().to_string(),
    )
}

pub(super) fn normalize_project_path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

pub(super) fn has_publishable_scope_hint(
    cwd: Option<&str>,
    project_override: Option<&str>,
) -> bool {
    cwd.is_some_and(|value| !value.is_empty()) || project_override.is_some()
}

/// Resolve the `(workspace_id, project_id)` pair for a hook event without
/// publishing it as active. Callers decide separately whether the event is a
/// foreground interaction allowed to advance fallback routing.
///
/// Precedence:
/// 1. `workspace_override` (typically declared by the agent's host-side
///    hook via a `.sessionmunch.toml` walk-up) OR `DEFAULT_WORKSPACE_NAME`.
/// 2. `project_override` OR marker-selected project strategy OR
///    `basename(cwd)` OR fallback to `state.project_id` (when `cwd` is
///    also unavailable).
///
/// Cache key is `(cwd, workspace_override, project_override,
/// project_strategy)` so the same `cwd` resolved with and without an
/// override (e.g. during a hook-script upgrade window) doesn't poison each
/// other's slot.
pub(super) async fn resolve_project_ids_inner(
    state: &HookState,
    cwd: Option<&str>,
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_strategy: ProjectStrategy,
) -> anyhow::Result<(WorkspaceId, ProjectId)> {
    let cwd_norm = cwd
        .filter(|s| !s.is_empty())
        .map(normalize_project_path_key);

    // Without cwd AND without a project override, there's nothing to
    // resolve — fall through to the server defaults.
    if cwd_norm.is_none() && project_override.is_none() {
        return Ok((state.workspace_id, state.project_id));
    }

    let cache_key = cache_key_for(
        cwd_norm.as_deref(),
        workspace_override,
        project_override,
        project_strategy,
    );

    {
        let mut cache = state.project_cache.lock().await;
        if let Some(ids) = cache.get(&cache_key) {
            return Ok(ids);
        }
    }

    let workspace_name = workspace_override
        .unwrap_or(DEFAULT_WORKSPACE_NAME)
        .to_string();

    let (project_name, repo_path) = match (project_override, cwd_norm.as_deref()) {
        (Some(p), Some(c)) => (
            p.to_string(),
            repo_path_from_project_override(c, p, project_strategy),
        ),
        (Some(p), None) => (p.to_string(), None),
        (None, Some(c)) => match derive_project_from_cwd(c, project_strategy) {
            Some(resolved) => resolved,
            None => return Ok((state.workspace_id, state.project_id)),
        },
        (None, None) => {
            // The early-return at the top of the function guards
            // against this branch; the explicit fallback here keeps
            // the resolver panic-free if that guard ever moves or
            // gets refactored. Same effect as `unreachable!`, but
            // visible at compile time instead of inside the panic
            // message.
            return Ok((state.workspace_id, state.project_id));
        }
    };

    // The reserved global preferences scope (issue #154) is written only
    // through explicit MCP `scope: "global"` requests — event capture must
    // never create it or leak observations into it, whether the name came
    // from a directory literally called `_global` or a marker-file
    // override. Fall back to the server-default project, same as a
    // cwd-less event.
    if project_name == sessionmunch_core::GLOBAL_SCOPE_PROJECT {
        debug!(
            cwd = ?cwd_norm,
            "hook router: refusing to attribute event capture to the reserved \
             global scope; using the server-default project"
        );
        return Ok((state.workspace_id, state.project_id));
    }

    pub(super) fn derive_project_from_cwd(
        cwd: &str,
        strategy: ProjectStrategy,
    ) -> Option<(String, Option<String>)> {
        // Delegate to the shared helper so the CLI's `resolve_project_name`
        // and this resolver agree on what "the project for this cwd"
        // resolves to. Map our wire-format `ProjectStrategy` onto the
        // shared library's `ProjectNameStrategy`.
        let path = std::path::Path::new(cwd);
        let strat = match strategy {
            ProjectStrategy::Basename => sessionmunch_consolidate::ProjectNameStrategy::Basename,
            ProjectStrategy::RepoRoot => {
                sessionmunch_consolidate::ProjectNameStrategy::MainRepoRoot
            }
        };
        // `repo_path` is the project's git boundary and is used as a
        // longest-prefix match KEY for future cwds, so it must be a real
        // repo root or nothing -- never the bare cwd. Recording the bare
        // cwd turned any directory an agent merely opened a session in
        // (e.g. $HOME) into a catch-all that swallowed every project
        // nested beneath it (issue #103). The NAME still follows the
        // strategy.
        //
        // The `MainRepoRoot` strategy hands back the repo root in `root`
        // and names the project after it, so name and repo_path are
        // aligned -- keep it. Under `Basename` the project is named after
        // the cwd's leaf, so `root` is None and we may discover the
        // enclosing repo. Adopt that repo root as repo_path ONLY when the
        // cwd IS the repo root; for a subdir cwd the discovered root is a
        // PREFIX of the cwd whose basename differs from the project name,
        // so storing it would make a leaf project (e.g. `backend`) a
        // catch-all that swallows the repo root and every sibling subdir
        // (issue #103). A subdir cwd therefore stores None.
        sessionmunch_consolidate::derive_project_name(path, strat).map(|(name, root)| {
            let repo_path = root
                .map(|p| {
                    normalize_project_path_key(
                        &repo_root_in_cwd_namespace(path, &p).to_string_lossy(),
                    )
                })
                .or_else(|| repo_path_from_cwd(cwd));
            (name, repo_path)
        })
    }

    pub(super) fn repo_path_from_cwd(cwd: &str) -> Option<String> {
        let path = std::path::Path::new(cwd);
        let repo_root = sessionmunch_consolidate::discover_repo_root(path).ok()?;
        cwd_is_repo_root(path, &repo_root).then(|| {
            normalize_project_path_key(
                &repo_root_in_cwd_namespace(path, &repo_root).to_string_lossy(),
            )
        })
    }

    pub(super) fn repo_root_in_cwd_namespace(
        cwd: &std::path::Path,
        repo_root: &std::path::Path,
    ) -> std::path::PathBuf {
        // On macOS, temp paths often arrive from the host as `/var/...` while
        // libgit2 reports the same directory as `/private/var/...`. Prefix
        // matching later compares the stored `repo_path` against the raw hook
        // cwd, so keep the repo root in the same spelling/namespace as `cwd`
        // whenever canonical paths prove that `cwd` is inside `repo_root`.
        if let Ok(root_canon) = std::fs::canonicalize(repo_root) {
            for ancestor in cwd.ancestors() {
                if let Ok(ancestor_canon) = std::fs::canonicalize(ancestor)
                    && ancestor_canon == root_canon
                {
                    return ancestor.to_path_buf();
                }
            }
        }
        repo_root.to_path_buf()
    }

    pub(super) fn repo_path_from_project_override(
        cwd: &str,
        project: &str,
        strategy: ProjectStrategy,
    ) -> Option<String> {
        if matches!(strategy, ProjectStrategy::RepoRoot) {
            let cwd_path = std::path::Path::new(cwd);
            if let Ok(root) = sessionmunch_consolidate::discover_main_repo_root(cwd_path) {
                let visible_root = repo_root_in_cwd_namespace(cwd_path, &root);
                if visible_root.file_name().and_then(|name| name.to_str()) == Some(project) {
                    return Some(normalize_project_path_key(&visible_root.to_string_lossy()));
                }
            }
        }
        repo_path_from_cwd(cwd)
    }

    pub(super) fn cwd_is_repo_root(cwd: &std::path::Path, repo_root: &std::path::Path) -> bool {
        // git2's workdir may carry a trailing separator and resolves symlinks;
        // canonicalize both before comparing. Fall back to a trailing-slash
        // tolerant string compare if either path can't be canonicalized
        // (both should exist in practice).
        if let (Ok(a), Ok(b)) = (std::fs::canonicalize(cwd), std::fs::canonicalize(repo_root)) {
            return a == b;
        }
        let strip = |p: &std::path::Path| p.to_string_lossy().trim_end_matches('/').to_string();
        strip(cwd) == strip(repo_root)
    }

    let ws = state
        .writer
        .get_or_create_workspace(workspace_name)
        .await
        .map_err(|e| anyhow::anyhow!("get_or_create_workspace: {e}"))?;

    // Prefix-match the cwd against any existing project's `repo_path`
    // BEFORE auto-creating a new project. Without this, a tool call
    // whose cwd was `/projects/manga-plus/reader/src/main.rs` would
    // get its observation attributed to a fresh `src`/`reader` project
    // instead of the existing `manga-plus` parent. The schema column
    // `projects.repo_path` was provisioned for exactly this match;
    // `find_project_by_cwd_prefix` returns the longest-matching parent
    // so a more-specific declared sub-project (via `.sessionmunch.toml`,
    // whose row has a longer `repo_path`) still wins over its outer
    // parent. Skipped when the operator passed an explicit
    // `project_override` (the override always wins) or when the cwd is
    // empty (cwd-less event already handled by the early returns above).
    // The match is keyed on the actual cwd (`cwd_norm`), not the stored
    // `repo_path`: `repo_path` is now the git root or None (issue #103),
    // whereas cwd->parent matching needs the full deep path.
    let proj = if project_override.is_none()
        && let Some(rp) = cwd_norm.as_deref().filter(|s| !s.is_empty())
        && let Some((parent_id, parent_name)) = state
            .reader
            .find_project_by_cwd_prefix(ws, rp.to_string(), state.home_dir.as_deref())
            .await
            .map_err(|e| anyhow::anyhow!("find_project_by_cwd_prefix: {e}"))?
        && parent_name != project_name
    {
        debug!(
            cwd = rp,
            derived = %project_name,
            parent = %parent_name,
            "hook router: cwd inside existing project — using parent instead of \
             creating fragment"
        );
        parent_id
    } else {
        state
            .writer
            .get_or_create_project(ws, project_name, repo_path)
            .await
            .map_err(|e| anyhow::anyhow!("get_or_create_project: {e}"))?
    };
    let ids = (ws, proj);
    state.project_cache.lock().await.insert(cache_key, ids);
    Ok(ids)
}

/// Back-compat wrapper used by tests: resolve without the `default_global`
/// recall preference (equivalent to a repo that never opted into
/// `[recall] default_global`). Production paths call
/// [`resolve_project_ids_inner`] directly with the marker's value.
#[cfg(test)]
pub(super) async fn resolve_project_ids(
    state: &HookState,
    cwd: Option<&str>,
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_strategy: ProjectStrategy,
    actor: &sessionmunch_core::ActorKey,
) -> anyhow::Result<(WorkspaceId, ProjectId)> {
    let ids = resolve_project_ids_inner(
        state,
        cwd,
        workspace_override,
        project_override,
        project_strategy,
    )
    .await?;
    if has_publishable_scope_hint(cwd, project_override) {
        state.active_project.set_for(actor, ids.0, ids.1, false);
    }
    Ok(ids)
}

/// Whether session-sticky attribution may apply: the event's cwd must sit
/// inside the session's own cwd subtree, and the session's cwd must be a
/// meaningful anchor — not missing, not the filesystem root, and not the
/// user's home directory (broad roots would fold every project beneath
/// them into one bucket, the #103 catch-all failure mode).
pub(super) fn sticky_within_session_tree(
    session_cwd: Option<&str>,
    event_cwd: Option<&str>,
    home_dir: Option<&str>,
) -> bool {
    let Some(session_norm) = meaningful_session_anchor(session_cwd, home_dir) else {
        return false;
    };
    // A cwd-less event inside a known session still belongs to it — there
    // is no directory evidence to contradict the session (and per-event
    // resolution would only shrug it into the server default anyway).
    let Some(event_cwd) = event_cwd.filter(|s| !s.trim().is_empty()) else {
        return true;
    };
    let event_norm = normalize_project_path_key(event_cwd);
    // Component-wise containment: "/a/b" contains "/a/b/c" but not
    // "/a/bc" (Path::starts_with is component-based, not string-based).
    std::path::Path::new(&event_norm).starts_with(std::path::Path::new(&session_norm))
}

/// The session-cwd anchor guards shared by both stickiness predicates:
/// a session with no recorded cwd, or rooted at the filesystem root or
/// the user's home, never sticks — a broad anchor would fold every
/// project beneath it into one bucket (the #103 catch-all failure
/// mode). Returns the normalized session cwd when it is a meaningful
/// anchor.
pub(super) fn meaningful_session_anchor(
    session_cwd: Option<&str>,
    home_dir: Option<&str>,
) -> Option<String> {
    let session_cwd = session_cwd.filter(|s| !s.trim().is_empty())?;
    let session_norm = normalize_project_path_key(session_cwd);
    if session_norm == "/" {
        return None;
    }
    if let Some(home) = home_dir
        && normalize_project_path_key(home) == session_norm
    {
        return None;
    }
    Some(session_norm)
}

/// Out-of-tree stickiness for `repo-root` deployments (issue #394).
///
/// Under `repo-root` the host-side hook resolves the repository root
/// itself (a containerized server cannot see the client's checkout)
/// and sends `project=<root name>`; a marker declares its project
/// explicitly. An event reaching stickiness with NO override therefore
/// has a cwd outside both — an agent scratch directory, `/tmp` — and
/// per-event derivation could only mint a basename phantom
/// (`scratchpad`, `data`, …), so the session stays the source of truth
/// even out of its tree. Deliberate rescopes still win (overrides never
/// reach stickiness), broad anchors still never stick, and the default
/// `basename` strategy is untouched: "no override" carries no
/// fell-through signal there.
pub(super) fn sticky_out_of_tree_under_repo_root(
    session_cwd: Option<&str>,
    home_dir: Option<&str>,
    strategy: ProjectStrategy,
) -> bool {
    matches!(strategy, ProjectStrategy::RepoRoot)
        && meaningful_session_anchor(session_cwd, home_dir).is_some()
}

/// Whether this event's declared overrides leave room for session-sticky
/// attribution at all (issue #394's `sticky` knob).
///
/// A marker-declared scope is a deliberate rescope in BOTH modes and always
/// wins — that is the invariant the whole knob is built around. What `sticky`
/// changes is the other kind of override: under `repo-root` the host hook
/// derives `project=<checkout name>` from wherever the cwd happens to be, which
/// carries no operator intent, so an established session may overrule it. The
/// wire-level [`ProjectSource`] is what makes the two distinguishable; a client
/// too old to tag its override reports `Unspecified` and keeps today's
/// behavior, so `sticky` degrades safely rather than silently capturing
/// deliberate rescopes.
pub(super) fn overrides_permit_sticky(
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_source: ProjectSource,
    routing: MidSessionRouting,
) -> bool {
    // A workspace override only ever comes from a marker file.
    if workspace_override.is_some() {
        return false;
    }
    if project_override.is_none() {
        return true;
    }
    routing.overrules_derived_override() && project_source.yields_to_session()
}

/// Whether the session's cwd anchor and the event's cwd admit stickiness.
///
/// `follow-cwd` keeps the two #394 paths: inside the session's own subtree, or
/// out of tree under `repo-root` where a missing override already proves the
/// cwd resolved to nothing. `sticky` extends out-of-tree inheritance to every
/// strategy — the point of the knob is that mid-session navigation never
/// rescopes, whether the agent wandered into `/tmp` or into a sibling
/// checkout. The broad-anchor guard is deliberately NOT relaxed: a session
/// rooted at `/` or `$HOME` still never sticks in either mode, or one stray
/// session would fold every project beneath it into a single bucket (the #103
/// catch-all failure mode).
pub(super) fn sticky_cwd_admits(
    session_cwd: Option<&str>,
    event_cwd: Option<&str>,
    home_dir: Option<&str>,
    strategy: ProjectStrategy,
    routing: MidSessionRouting,
) -> bool {
    sticky_within_session_tree(session_cwd, event_cwd, home_dir)
        || sticky_out_of_tree_under_repo_root(session_cwd, home_dir, strategy)
        || (routing.overrules_derived_override()
            && meaningful_session_anchor(session_cwd, home_dir).is_some())
}
