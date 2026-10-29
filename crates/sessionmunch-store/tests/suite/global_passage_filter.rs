//! Tests for cross-project (global) passage retrieval with hard filters —
//! `search_passages_hybrid_global` and the `GlobalPassageFilter`.

use rusqlite::Connection;
use serde_json::json;
use sessionmunch_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use sessionmunch_store::Store;
use sessionmunch_store::reader::GlobalPassageFilter;
use sessionmunch_store::scope::create_explicit_scope;
use tempfile::TempDir;

fn make_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

async fn make_scope(store: &Store, ws_name: &str, proj_name: &str) -> (WorkspaceId, ProjectId) {
    let scope = create_explicit_scope(&store.writer, ws_name, proj_name)
        .await
        .unwrap();
    (scope.workspace_id, scope.project_id)
}

fn make_page(
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    body: &str,
    frontmatter: serde_json::Value,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: "Test".to_string(),
        body: body.to_string(),
        tier: Tier::Working,
        frontmatter_json: frontmatter,
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

fn word_body(word: &str, count: usize) -> String {
    "# Heading\n\n".to_string() + &(word.to_string() + " ").repeat(count)
}

/// Rewrite a page's `updated_at` (Unix microseconds) directly, to exercise the
/// `since_us` / `until_us` hard filters against real schema columns.
fn set_updated_at(conn: &Connection, path: &str, updated_us: i64) {
    conn.execute(
        "UPDATE pages SET updated_at = ?1 WHERE path = ?2",
        rusqlite::params![updated_us, path],
    )
    .unwrap();
}

/// Create three projects with distinguishable passages so a no-filter global
/// search spans all of them. Returns `(ws_a, proj_a, ws_b, proj_b, ws_c, proj_c)`.
async fn seed_three_projects(
    store: &Store,
) -> (
    WorkspaceId,
    ProjectId,
    WorkspaceId,
    ProjectId,
    WorkspaceId,
    ProjectId,
) {
    let (ws_a, proj_a) = make_scope(store, "global-a", "proj-a").await;
    let (ws_b, proj_b) = make_scope(store, "global-b", "proj-b").await;
    let (ws_c, proj_c) = make_scope(store, "global-c", "proj-c").await;
    // Distinct marker terms so lexical hits differ per project.
    store
        .writer
        .upsert_page(make_page(
            ws_a,
            proj_a,
            "notes/alpha.md",
            &word_body("quark", 40),
            json!({"machine": "mac-a", "agent": "harness-x"}),
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(make_page(
            ws_b,
            proj_b,
            "notes/betaquark.md",
            &word_body("quark", 40),
            json!({"machine": "mac-b", "agent": "harness-y"}),
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(make_page(
            ws_c,
            proj_c,
            "notes/gammaquark.md",
            &word_body("quark", 40),
            json!({"machine": "mac-c", "agent": "harness-z"}),
        ))
        .await
        .unwrap();
    (ws_a, proj_a, ws_b, proj_b, ws_c, proj_c)
}

#[tokio::test]
async fn global_hybrid_spans_all_projects_without_filters() {
    let (_tmp, store) = make_store();
    let (_, pa, _, pb, _, pc) = seed_three_projects(&store).await;

    let mut seen_projects = std::collections::HashSet::new();
    // "quark" matches all three marker words (alpa/beta/gamma…quark).
    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter::default(),
            "quark".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    for hit in &hits {
        seen_projects.insert(hit.project_id);
    }
    assert!(
        seen_projects.contains(&pa) && seen_projects.contains(&pb) && seen_projects.contains(&pc),
        "no-filter global search must span all projects, got {seen_projects:?}"
    );
    assert!(!hits.is_empty());
}

#[tokio::test]
async fn global_limit_zero_returns_empty_without_querying() {
    let (_tmp, store) = make_store();
    let (_, pa, _, _, _, _) = seed_three_projects(&store).await;
    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter::default(),
            "quark".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            0,
        )
        .await
        .unwrap();
    assert!(hits.is_empty());
    let _ = pa;
}

#[tokio::test]
async fn project_allowlist_excludes_other_projects() {
    let (_tmp, store) = make_store();
    let (_, pa, _, _, _, _) = seed_three_projects(&store).await;

    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter {
                project_ids: Some(vec![pa]),
                ..Default::default()
            },
            "quark".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(
            hit.project_id, pa,
            "project filter must exclude every other project"
        );
    }
}

#[tokio::test]
async fn empty_project_allowlist_excludes_everything() {
    let (_tmp, store) = make_store();
    let (_, _, _, _, _, _) = seed_three_projects(&store).await;

    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter {
                project_ids: Some(Vec::new()),
                ..Default::default()
            },
            "quark".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(hits.is_empty(), "empty allowlist is a hard 'exclude all'");
}

#[tokio::test]
async fn harness_filter_excludes_nonmatching_agents() {
    let (_tmp, store) = make_store();
    let (_, _, _, _, _, _) = seed_three_projects(&store).await;

    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter {
                harness: Some(vec!["harness-x".to_string()]),
                ..Default::default()
            },
            "quark".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(!hits.is_empty());
    // Only the page whose $.agent == "harness-x" (notes/alpha.md) may appear.
    for hit in &hits {
        assert_eq!(
            hit.page_path.as_str(),
            "notes/alpha.md",
            "harness filter admitted a non-harness-x page"
        );
    }
}

#[tokio::test]
async fn machine_filter_excludes_nonmatching_machines() {
    let (_tmp, store) = make_store();
    let (_, _, _, _, _, _) = seed_three_projects(&store).await;

    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter {
                machine: Some(vec!["mac-b".to_string()]),
                ..Default::default()
            },
            "quark".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(hit.page_path.as_str(), "notes/betaquark.md");
    }
}

#[tokio::test]
async fn kind_filter_restricts_to_named_kinds() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store, "kind-a", "kind-p").await;
    store
        .writer
        .upsert_page(make_page(
            ws,
            proj,
            "notes/a-note.md",
            &word_body("kindterm", 40),
            json!({}),
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(make_page(
            ws,
            proj,
            "sessions/a-session.md",
            &word_body("kindterm", 40),
            json!({}),
        ))
        .await
        .unwrap();

    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter {
                kind: Some(vec!["note".to_string()]),
                ..Default::default()
            },
            "kindterm".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(hit.page_path.as_str(), "notes/a-note.md");
    }
}

#[tokio::test]
async fn since_until_filters_apply_to_updated_at() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store, "time-a", "time-p").await;
    store
        .writer
        .upsert_page(make_page(
            ws,
            proj,
            "notes/old.md",
            &word_body("timeword", 40),
            json!({}),
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(make_page(
            ws,
            proj,
            "notes/new.md",
            &word_body("timeword", 40),
            json!({}),
        ))
        .await
        .unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    set_updated_at(&conn, "notes/old.md", 1_000_000_000_000_000); // 2001
    set_updated_at(&conn, "notes/new.md", 2_000_000_000_000_000); // 2033

    let old = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter {
                until_us: Some(1_500_000_000_000_000),
                ..Default::default()
            },
            "timeword".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(!old.is_empty());
    for hit in &old {
        assert_eq!(hit.page_path.as_str(), "notes/old.md");
    }

    let new = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter {
                since_us: Some(1_500_000_000_000_000),
                ..Default::default()
            },
            "timeword".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(!new.is_empty());
    for hit in &new {
        assert_eq!(hit.page_path.as_str(), "notes/new.md");
    }
}

#[tokio::test]
async fn per_page_cap_prevents_one_page_from_flooding() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store, "cap-a", "cap-p").await;

    // Two contributing pages; each has several distinct-passage headings so
    // many passages match the marker term.
    for (i, sub) in ["one", "two", "three", "four"].iter().enumerate() {
        let body = format!(
            "# Part {sub}\n\n{}\n\n# Part {sub}-b\n\n{}",
            word_body("capterm", 30),
            word_body("capterm", 30)
        );
        store
            .writer
            .upsert_page(make_page(
                ws,
                proj,
                &format!("notes/flood-{i}.md"),
                &body,
                json!({}),
            ))
            .await
            .unwrap();
    }

    let hits = store
        .reader
        .search_passages_hybrid_global(
            GlobalPassageFilter::default(),
            "capterm".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            100,
        )
        .await
        .unwrap();
    assert!(
        hits.len() >= 2,
        "need multiple pages with matching passages for a meaningful cap test"
    );

    let mut per_page: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    for hit in &hits {
        *per_page.entry(hit.page_id.as_bytes().to_vec()).or_insert(0) += 1;
    }
    for (_page, count) in &per_page {
        assert!(
            *count <= 3,
            "per-page cap violated: page {_page:?} contributed {count} hits"
        );
    }
}

#[tokio::test]
async fn global_result_order_is_stable_across_calls() {
    let (_tmp, store) = make_store();
    let (_, _, _, _, _, _) = seed_three_projects(&store).await;
    let run = || async {
        store
            .reader
            .search_passages_hybrid_global(
                GlobalPassageFilter::default(),
                "quark".to_string(),
                None,
                "test".to_string(),
                "model".to_string(),
                768,
                100,
            )
            .await
            .unwrap()
    };
    let first = run().await;
    let second = run().await;
    assert_eq!(
        first.len(),
        second.len(),
        "identical queries must return the same number of hits"
    );
    let order: Vec<_> = first
        .iter()
        .map(|h| h.passage_id.as_bytes().to_vec())
        .collect();
    let order2: Vec<_> = second
        .iter()
        .map(|h| h.passage_id.as_bytes().to_vec())
        .collect();
    assert_eq!(
        order, order2,
        "tied/rank order must be deterministic across identical queries"
    );
}
