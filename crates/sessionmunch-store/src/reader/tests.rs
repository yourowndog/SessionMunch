/// The percentage is what an operator actually reads, and it divides by a
/// figure that is zero on a fresh database.
#[test]
fn reclaimable_pct_is_zero_on_an_empty_database_rather_than_nan() {
    let empty = StorageStatus::default();
    assert_eq!(empty.database_bytes, 0);
    assert_eq!(empty.reclaimable_pct(), 0.0);
    assert!(empty.reclaimable_pct().is_finite(), "never NaN or inf");

    let quarter = StorageStatus {
        page_size: 4096,
        page_count: 400,
        freelist_count: 100,
        database_bytes: 400 * 4096,
        reclaimable_bytes: 100 * 4096,
        data_dir_free_bytes: None,
    };
    assert!((quarter.reclaimable_pct() - 25.0).abs() < f64::EPSILON);
}

/// The signal this field exists for: a store's own size says nothing
/// about disk headroom, so `storage_status` must also report the
/// filesystem's free space. A temp dir always has some, so this is
/// `Some(n)` with `n > 0`, not just "doesn't crash".
#[tokio::test]
async fn storage_status_reports_filesystem_free_space() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let status = store.reader.storage_status().await.unwrap();
    let free = status
        .data_dir_free_bytes
        .expect("free space should be readable for a real temp dir");
    assert!(free > 0);
}

/// The single-scope manifest lookup the wiki resolves a new scope's
/// `_meta.md` names from. It carries `repo_path`, and it is keyed by the
/// full pair: a project id offered under the wrong workspace resolves to
/// nothing rather than leaking the other workspace's name into a
/// manifest.
#[tokio::test]
async fn scope_row_by_ids_returns_manifest_names_and_isolates_workspaces() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("acme").await.unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "webapp", Some("/repo/webapp".into()))
        .await
        .unwrap();
    let other_ws = store.writer.get_or_create_workspace("other").await.unwrap();

    let row = store
        .reader
        .scope_row_by_ids(ws, proj)
        .await
        .unwrap()
        .expect("the scope exists");
    assert_eq!(row.workspace_name, "acme");
    assert_eq!(row.project_name, "webapp");
    assert_eq!(row.repo_path.as_deref(), Some("/repo/webapp"));

    assert!(
        store
            .reader
            .scope_row_by_ids(other_ws, proj)
            .await
            .unwrap()
            .is_none(),
        "a project id under the wrong workspace resolves to nothing"
    );
    assert!(
        store
            .reader
            .scope_row_by_ids(ws, sessionmunch_core::ProjectId::new())
            .await
            .unwrap()
            .is_none(),
        "an unknown project id is None, not an error"
    );
}

use super::{
    DESCRIPTOR_MAX_CHARS, StorageStatus, entity_query_tokens, handoff_listing_sql, like_escape,
    page_descriptor, page_descriptor_expr,
};
use crate::Store;

#[test]
fn a_metadata_styled_descriptor_source_is_echoed_verbatim() {
    // Every line here is a metadata bullet, so all of them are skipped
    // and nothing accumulates. The fallback then echoes the raw input —
    // the result is not empty, it is the unusable input reproduced whole.
    //
    // This is the failure mode for a writer that fills frontmatter
    // `summary` in the same house style it renders a session page's
    // metadata block in. Because `summary` wins the COALESCE, such a
    // page trades a usable body-derived descriptor for this, and nothing
    // anywhere reports an error. Writers must emit plain prose.
    let metadata_styled = "- **session_id:** `9f2c`\n- **observations:** 2";
    assert_eq!(
        page_descriptor(metadata_styled, "Some title"),
        metadata_styled
    );
}

use sessionmunch_core::{
    AgentKind, Handoff, HandoffContent, HandoffId, HandoffLifecycle, HandoffOrigin, HandoffScope,
    HandoffState, NewHandoff, NewSession, OwnerFilter, ProjectId, SessionId, WorkspaceId,
};

#[test]
fn named_handoff_listing_uses_owner_index_for_both_ranges() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let ws = WorkspaceId::new();
    let proj = ProjectId::new();
    let (sql, owner) = handoff_listing_sql(false, &OwnerFilter::User("user:alice".into()), 50);
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let details = stmt
        .query_map(
            rusqlite::params![ws.as_bytes(), proj.as_bytes(), owner.unwrap()],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        details
            .iter()
            .filter(|detail| detail.contains("idx_handoffs_project_owner_recent"))
            .count(),
        2,
        "shared and owned ranges must both use the owner index: {details:?}"
    );
}

#[test]
fn authority_candidate_window_is_bounded_and_saturating() {
    use super::authority_candidate_limit;

    assert_eq!(authority_candidate_limit(0), 0);
    assert_eq!(authority_candidate_limit(1), 20);
    assert_eq!(authority_candidate_limit(10), 40);
    assert_eq!(authority_candidate_limit(1_000), 1_300);
    assert_eq!(authority_candidate_limit(usize::MAX), usize::MAX);
}

/// Build an open handoff for the pure-selection tests. `manual` toggles
/// the manual (`from_session_id == None`) vs auto distinction; `t` is the
/// creation time in microseconds; `summary` doubles as a label to assert
/// which handoff won.
fn handoff(summary: &str, cwd: Option<&str>, manual: bool, t: i64) -> Handoff {
    Handoff {
        scope: HandoffScope {
            id: HandoffId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
        },
        origin: HandoffOrigin {
            from_session_id: if manual { None } else { Some(SessionId::new()) },
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: cwd.map(str::to_string),
            owner_user: None,
        },
        content: HandoffContent {
            summary: summary.to_string(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        },
        lifecycle: HandoffLifecycle {
            state: HandoffState::Open,
            created_at: jiff::Timestamp::from_microsecond(t).unwrap(),
            accepted_by: None,
            accepted_at: None,
            accepted_by_session: None,
            accepted_by_user: None,
        },
    }
}

fn pick(candidates: Vec<Handoff>, cwd: Option<&str>) -> String {
    super::select_open_handoff(candidates, cwd)
        .map_or_else(|| "—".to_string(), |h| h.content.summary)
}

#[test]
fn cwd_within_respects_component_boundaries() {
    use super::cwd_within;
    assert!(cwd_within("/repo", "/repo"));
    assert!(cwd_within("/repo", "/repo/api"));
    assert!(cwd_within("/repo", "/repo/api/v2"));
    assert!(cwd_within("/repo/", "/repo/api")); // trailing slash normalized
    assert!(cwd_within("/", "/anything/here"));
    assert!(!cwd_within("/repo", "/repo-other")); // sibling, not descendant
    assert!(!cwd_within("/repo", "/repository")); // longer name, not a child
    assert!(!cwd_within("/repo/api", "/repo")); // parent is not within child
}

#[test]
fn cwd_within_handles_windows_backslash_boundaries() {
    use super::cwd_within;
    assert!(cwd_within(r"C:\repo", r"C:\repo\api"));
    assert!(cwd_within(r"C:\repo\", r"C:\repo\api"));
    assert!(!cwd_within(r"C:\repo", r"C:\repo-other"));
}

// The realistic scenario matrix (see the cwd the SessionEnd hook injects):
// a manual handoff is cwd=NULL, an auto handoff carries the session cwd.

#[test]
fn handoff_manual_beats_auto_same_dir() {
    // The reported bug: a detailed manual handoff must win over the vague
    // SessionEnd auto handoff even though the auto one is newer.
    let c = vec![
        handoff("MAN-detail", None, true, 1),
        handoff("auto-vague", Some("/repo"), false, 2),
    ];
    assert_eq!(pick(c, Some("/repo")), "MAN-detail");
}

#[test]
fn handoff_auto_reaches_subdir() {
    let c = vec![handoff("auto", Some("/repo"), false, 1)];
    assert_eq!(pick(c, Some("/repo/api")), "auto");
}

#[test]
fn handoff_manual_survives_from_subdir() {
    let c = vec![
        handoff("MAN-detail", None, true, 1),
        handoff("auto-vague", Some("/repo"), false, 2),
    ];
    assert_eq!(pick(c, Some("/repo/api")), "MAN-detail");
}

#[test]
fn handoff_tolerates_trailing_slash_in_session_cwd() {
    let c = vec![handoff("auto", Some("/repo"), false, 1)];
    assert_eq!(pick(c, Some("/repo/")), "auto");
}

#[test]
fn handoff_lone_manual_is_delivered() {
    let c = vec![handoff("MAN", None, true, 1)];
    assert_eq!(pick(c, Some("/repo")), "MAN");
}

#[test]
fn handoff_newest_manual_wins_even_if_older_manual_has_cwd() {
    // An older manual that happens to carry a cwd must NOT outrank the
    // newest manual just because its cwd string is longer. The cwd
    // specificity tiebreak applies only to auto handoffs; among manuals the
    // newest wins. (Regression seen in real data: a cwd-bearing manual was
    // delivered ahead of the newer cwd-less one.)
    let c = vec![
        handoff(
            "old-with-cwd",
            Some("/var/home/luka/Trabalho/waba/bsp-core"),
            true,
            1,
        ),
        handoff("newest-null", None, true, 2),
    ];
    assert_eq!(
        pick(c, Some("/var/home/luka/Trabalho/waba/bsp-core")),
        "newest-null"
    );
}

#[test]
fn handoff_manual_with_nonmatching_cwd_still_beats_auto() {
    // Even if the model passed a cwd on the manual handoff that does not
    // match the next session's dir, the manual handoff is project-wide and
    // still wins over the auto one. Deterministic on from_session_id, not
    // on the manual handoff happening to have a NULL cwd.
    let c = vec![
        handoff("MAN", Some("/some/other/place"), true, 1),
        handoff("auto", Some("/repo"), false, 2),
    ];
    assert_eq!(pick(c, Some("/repo")), "MAN");
}

#[test]
fn handoff_sibling_subdirs_do_not_leak() {
    // A /mono/web session must not receive the /mono/api handoff, even
    // though that sibling handoff is newer.
    let c = vec![
        handoff("web", Some("/mono/web"), false, 1),
        handoff("api", Some("/mono/api"), false, 2),
    ];
    assert_eq!(pick(c, Some("/mono/web")), "web");
}

#[test]
fn handoff_dangerous_prefix_never_matches() {
    let c = vec![handoff("repo", Some("/repo"), false, 1)];
    assert_eq!(pick(c, Some("/repo-other")), "—");
}

#[test]
fn handoff_newer_parent_beats_stale_specific_ancestor() {
    let c = vec![
        handoff("a", Some("/a"), false, 2),
        handoff("ab", Some("/a/b"), false, 1),
    ];
    assert_eq!(pick(c, Some("/a/b/c")), "a");
}

#[test]
fn handoff_legacy_trailing_slash_does_not_beat_newer_equivalent_auto() {
    let c = vec![
        handoff("old-slash", Some("/repo/"), false, 1),
        handoff("new-no-slash", Some("/repo"), false, 2),
    ];
    assert_eq!(pick(c, Some("/repo/api")), "new-no-slash");
}

#[test]
fn handoff_wildcard_characters_are_literal_path_components() {
    let c = vec![
        handoff("percent", Some("/repo/%"), false, 2),
        handoff("underscore", Some("/repo/_x"), false, 3),
        handoff("repo", Some("/repo"), false, 1),
    ];
    assert_eq!(pick(c.clone(), Some("/repo/abc")), "repo");
    assert_eq!(pick(c, Some("/repo/ax")), "repo");
}

#[test]
fn handoff_parent_session_does_not_inherit_deep_handoff() {
    // A handoff left deep in /repo/api is not delivered to a /repo session.
    let c = vec![handoff("deep", Some("/repo/api"), false, 1)];
    assert_eq!(pick(c, Some("/repo")), "—");
}

#[test]
fn handoff_none_filter_is_project_wide() {
    // The web overview passes no cwd: every open handoff is a candidate,
    // manual still preferred.
    let c = vec![
        handoff("auto-deep", Some("/repo/api"), false, 1),
        handoff("MAN", None, true, 2),
    ];
    assert_eq!(pick(c, None), "MAN");
}

#[tokio::test]
async fn latest_open_handoff_preserves_workspace_project_isolation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws_a = store.writer.get_or_create_workspace("a").await.unwrap();
    let ws_b = store.writer.get_or_create_workspace("b").await.unwrap();
    let proj_a = store
        .writer
        .get_or_create_project(ws_a, "same-name", None)
        .await
        .unwrap();
    let proj_b = store
        .writer
        .get_or_create_project(ws_b, "same-name", None)
        .await
        .unwrap();

    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws_b,
            project_id: proj_b,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "wrong workspace".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws_a,
            project_id: proj_a,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "right project".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();

    let handoff = store
        .reader
        .latest_open_handoff(
            ws_a,
            proj_a,
            Some("/repo".into()),
            sessionmunch_core::OwnerFilter::Any,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handoff.content.summary, "right project");
}

#[tokio::test]
async fn accepting_newest_auto_expires_only_older_matching_autos() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "repo", Some("/repo".into()))
        .await
        .unwrap();

    async fn insert_auto(
        store: &Store,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cwd: &str,
        summary: &str,
    ) -> HandoffId {
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id,
                project_id,
                agent_kind: AgentKind::ClaudeCode,
                cwd: Some(cwd.into()),
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_handoff(NewHandoff {
                workspace_id,
                project_id,
                from_session_id: Some(session_id),
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: Some(cwd.into()),
                summary: summary.into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                owner_user: None,
            })
            .await
            .unwrap()
    }

    let superseded_same_cwd = insert_auto(&store, ws, proj, "/repo/api", "older same cwd").await;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let stale_specific = insert_auto(&store, ws, proj, "/repo/api", "stale specific").await;
    assert_eq!(
        store
            .reader
            .handoff_by_id(superseded_same_cwd)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        HandoffState::Expired,
        "a newer auto from the exact cwd must bound pre-accept accumulation"
    );
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let newest_parent = insert_auto(&store, ws, proj, "/repo", "newest parent").await;
    let sibling = insert_auto(&store, ws, proj, "/repo/web", "sibling").await;

    let selected = store
        .reader
        .latest_open_handoff(
            ws,
            proj,
            Some("/repo/api/src".into()),
            sessionmunch_core::OwnerFilter::Any,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        selected.scope.id, newest_parent,
        "newest eligible auto must win"
    );

    // A manual handoff appearing between selection and acceptance must not
    // be swept by automatic-handoff cleanup.
    let manual = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: None,
            summary: "manual".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .accept_handoff(sessionmunch_core::HandoffAcceptance {
            handoff_id: selected.scope.id,
            workspace_id: ws,
            project_id: proj,
            accepting_agent: AgentKind::Codex,
            accepting_session: None,
            accepting_user: None,
            owner_filter: sessionmunch_core::OwnerFilter::Any,
            receiving_cwd: Some("/repo/api/src".into()),
        })
        .await
        .unwrap();

    assert_eq!(
        store
            .reader
            .handoff_by_id(stale_specific)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        HandoffState::Expired
    );
    assert_eq!(
        store
            .reader
            .handoff_by_id(newest_parent)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        HandoffState::Accepted
    );
    for untouched in [sibling, manual] {
        assert_eq!(
            store
                .reader
                .handoff_by_id(untouched)
                .await
                .unwrap()
                .unwrap()
                .lifecycle
                .state,
            HandoffState::Open
        );
    }
}

/// A stored `repo_path` equal to the operator's `$HOME` must never be
/// prefix-matched: such a row would be a catch-all parent for every
/// project beneath the home directory. A normal nested repo under
/// `$HOME` must still match.
#[tokio::test]
async fn prefix_match_skips_home_dir_but_keeps_nested_repo() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    // A "$HOME catch-all" project whose repo_path is the home dir.
    store
        .writer
        .get_or_create_project(ws, "home", Some(String::from("/home/tester")))
        .await
        .unwrap();
    // A real nested repo beneath the home dir.
    let app_id = store
        .writer
        .get_or_create_project(ws, "app", Some(String::from("/home/tester/projects/app")))
        .await
        .unwrap();

    // A cwd somewhere under $HOME but not under any real repo must NOT
    // resolve to the $HOME catch-all row.
    let matched = store
        .reader
        .find_project_by_cwd_prefix(
            ws,
            String::from("/home/tester/random/dir"),
            Some("/home/tester"),
        )
        .await
        .unwrap();
    assert_eq!(
        matched, None,
        "a stored repo_path equal to $HOME must never be prefix-matched"
    );

    // A cwd inside a genuine nested repo still resolves to that repo.
    let matched = store
        .reader
        .find_project_by_cwd_prefix(
            ws,
            String::from("/home/tester/projects/app/src"),
            Some("/home/tester"),
        )
        .await
        .unwrap();
    assert_eq!(
        matched.map(|(id, _)| id),
        Some(app_id),
        "a genuine nested repo under $HOME must still prefix-match"
    );
}

#[tokio::test]
async fn prefix_match_treats_percent_and_underscore_as_literal_path_bytes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let literal_id = store
        .writer
        .get_or_create_project(
            ws,
            "literal",
            Some(String::from("/tmp/sessionmunch_%literal/repo_under")),
        )
        .await
        .unwrap();

    let matched = store
        .reader
        .find_project_by_cwd_prefix(
            ws,
            String::from("/tmp/sessionmunch_%literal/repo_under/src"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(matched.map(|(id, _)| id), Some(literal_id));

    let widened = store
        .reader
        .find_project_by_cwd_prefix(
            ws,
            String::from("/tmp/aiXmemory_Aliteral/repoXunder/src"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(widened, None, "wildcards in repo_path must stay literal");
}

#[tokio::test]
async fn prefix_match_handles_legacy_windows_backslash_repo_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project_id = ProjectId::new();
    let conn = rusqlite::Connection::open(tmp.path().join("db").join(crate::DB_FILENAME)).unwrap();
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            project_id.as_bytes(),
            ws.as_bytes(),
            "app",
            r"C:\Users\tester\app",
            jiff::Timestamp::now().as_microsecond()
        ],
    )
    .unwrap();

    let matched = store
        .reader
        .find_project_by_cwd_prefix(
            ws,
            String::from("C:/Users/tester/app/crates/core"),
            Some(r"C:\Users\tester"),
        )
        .await
        .unwrap();

    assert_eq!(matched.map(|(id, _)| id), Some(project_id));
}

#[tokio::test]
async fn prefix_match_ignores_legacy_trailing_separator_repo_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project_id = ProjectId::new();
    let conn = rusqlite::Connection::open(tmp.path().join("db").join(crate::DB_FILENAME)).unwrap();
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            project_id.as_bytes(),
            ws.as_bytes(),
            "legacy-trailing",
            "/repo/foo/",
            jiff::Timestamp::now().as_microsecond()
        ],
    )
    .unwrap();

    let matched = store
        .reader
        .find_project_by_cwd_prefix(ws, String::from("/repo/foo/bar"), None)
        .await
        .unwrap();

    assert_eq!(matched, None, "legacy trailing separator must not match");
}

#[test]
fn entity_query_tokens_lowercases_filters_short_and_bounds() {
    // A compound token yields the whole form AND its parts, so a
    // query can spell an entity `writer-actor` or `writer actor`.
    assert_eq!(
        entity_query_tokens("How does the Writer-Actor use SQLite?"),
        vec![
            "how".to_string(),
            "does".to_string(),
            "the".to_string(),
            "writer-actor".to_string(),
            "writer".to_string(),
            "actor".to_string(),
            "use".to_string(),
            "sqlite".to_string(),
        ],
    );
    assert!(
        entity_query_tokens("a of an X").is_empty(),
        "sub-3-char tokens are noise for prefix matching",
    );
    assert_eq!(
        entity_query_tokens("dup dup dup"),
        vec!["dup".to_string()],
        "tokens de-duplicate",
    );
    assert!(entity_query_tokens(&"word ".repeat(50)).len() <= 12);
    assert!(
        entity_query_tokens(&"x".repeat(1_000_000)).is_empty(),
        "an oversized untrusted token must be rejected before lowercase allocation"
    );
    assert_eq!(
        entity_query_tokens(&format!("{} usable", "x".repeat(1_000_000))),
        vec!["usable".to_string()],
        "a rejected compound must not hide later bounded tokens"
    );
}

#[test]
fn like_escape_neutralises_wildcards() {
    // `_` is LIKE's single-char wildcard and appears in real entity
    // names (`writer_actor`), so it must not match `writerxactor`.
    assert_eq!(like_escape("writer_actor"), "writer\\_actor");
    assert_eq!(like_escape("100%"), "100\\%");
    assert_eq!(like_escape("back\\slash"), "back\\\\slash");
    assert_eq!(like_escape("plain-name"), "plain-name");
}

#[test]
fn page_descriptor_skips_heading_and_rule_lines() {
    let body = concat!(
        "# Session 2026-04-20\n",
        "\n",
        "---\n",
        "\n",
        "## Summary\n",
        "\n",
        "Switched the scheduler to a single writer actor.\n",
    );
    assert_eq!(
        page_descriptor(body, ""),
        "Switched the scheduler to a single writer actor."
    );
}

#[test]
fn page_descriptor_fills_the_budget_across_prose_lines() {
    // A short opening line must not cost the rest of the budget: the
    // `_lint` pages open with "N finding(s)." and everything that tells
    // you what the findings were comes after it.
    let body = concat!(
        "# Lint findings\n",
        "\n",
        "501 finding(s).\n",
        "\n",
        "## 1 - stale (info)\n",
        "\n",
        "Episodic page sessions/e24f6fbc.md is 30 days old with zero accesses.\n",
    );
    assert_eq!(
        page_descriptor(body, ""),
        "501 finding(s). Episodic page sessions/e24f6fbc.md is 30 days old \
             with zero accesses."
    );
}

#[test]
fn page_descriptor_falls_back_to_raw_text_when_all_lines_are_structural() {
    assert_eq!(page_descriptor("# Only a title\n", ""), "# Only a title");
    assert_eq!(page_descriptor("   ", ""), "");
}

#[test]
fn page_descriptor_skips_metadata_bullets_and_the_repeated_title() {
    // The compiled session-page shape: the title again as prompt 1, with
    // the metadata block in between. What the reader does not already
    // have is prompt 2 onward.
    let title = "vamos revisar o deploy";
    let body = concat!(
        "# vamos revisar o deploy\n",
        "\n",
        "## Session metadata\n",
        "\n",
        "- **session_id:** `4e79a7ce-55e9-44c1-bcb5-74a6fe740900`\n",
        "- **started_at:** 2026-08-22T19:26:07Z\n",
        "- **observations:** 59\n",
        "\n",
        "## Prompts\n",
        "\n",
        "1. vamos revisar o deploy\n",
        "2. o rollback falhou no passo do migrate\n",
    );
    assert_eq!(
        page_descriptor(body, title),
        "o rollback falhou no passo do migrate"
    );
}

#[test]
fn page_descriptor_keeps_a_line_that_only_starts_with_a_complete_title() {
    // `truncate_for_title` appends the ellipsis only past 80 chars, so a
    // title without one is complete. A longer line that merely opens with
    // it is a different sentence, not a repeat.
    let title = "vamos revisar o deploy";
    let body = "# vamos revisar o deploy\n\n1. vamos revisar o deploy\n2. vamos revisar o deploy e o rollback do migrate\n";
    assert_eq!(
        page_descriptor(body, title),
        "vamos revisar o deploy e o rollback do migrate"
    );
}

#[test]
fn page_descriptor_matches_a_title_truncated_with_an_ellipsis() {
    let title = "vamos revisar o deploy do SHVIA-WEB e o rollback\u{2026}";
    let body = "# x\n\n1. vamos revisar o deploy do SHVIA-WEB e o rollback do migrate\n2. depois o resto\n";
    assert_eq!(page_descriptor(body, title), "depois o resto");
}

#[test]
fn page_descriptor_truncates_on_a_character_boundary() {
    let body = "\u{e7}".repeat(DESCRIPTOR_MAX_CHARS + 10);
    let out = page_descriptor(&body, "");
    assert_eq!(out.chars().count(), DESCRIPTOR_MAX_CHARS);
    assert!(out.ends_with('\u{2026}'));
}

#[test]
fn page_descriptor_expr_prefers_a_non_blank_frontmatter_summary() {
    let sql = page_descriptor_expr("pages.body", "pages.frontmatter_json");
    assert!(sql.contains("json_extract(pages.frontmatter_json, '$.summary')"));
    assert!(sql.contains("NULLIF(TRIM("));
    assert!(sql.contains("substr(pages.body, 1, 600)"));
}
