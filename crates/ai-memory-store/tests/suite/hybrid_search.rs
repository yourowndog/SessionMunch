//! Tests for search_passages_hybrid and related passage retrieval functions.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::Store;
use ai_memory_store::reader::f32_vec_to_bytes;
use ai_memory_store::scope::create_explicit_scope;
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

/// Open a fresh store, keeping the backing `TempDir` alive for the caller's
/// entire scope (dropping it early deletes the on-disk database file out
/// from under any later `Connection::open` in the same test).
fn make_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

/// Create a real (workspace_id, project_id) pair via the store's only
/// scope-creating helper. A page's workspace_id must match its project's
/// workspace_id (enforced by a DB constraint); two independently-random
/// `WorkspaceId::new()`/`ProjectId::new()` values never satisfy that.
async fn make_scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let scope = create_explicit_scope(&store.writer, "test-workspace", "test-project")
        .await
        .unwrap();
    (scope.workspace_id, scope.project_id)
}

fn make_page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: "Test".to_string(),
        body: body.to_string(),
        tier: Tier::Working,
        frontmatter_json: json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

/// Insert a fake passage embedding for testing dense search.
async fn insert_passage_embedding(
    conn: &Connection,
    passage_id: &[u8],
    provider: &str,
    model: &str,
    dim: u32,
    vec: &[f32],
) {
    let bytes = f32_vec_to_bytes(vec);
    conn.execute(
        "INSERT INTO page_passage_embeddings (passage_id, provider, model, dim, embedding)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![passage_id, provider, model, dim as i64, bytes],
    )
    .unwrap();
}

/// Create a normalized query vector for testing.
fn make_query_vec(dim: u32) -> Vec<f32> {
    let mut v = vec![0.0; dim as usize];
    // Simple pattern: 1.0 at index 0, rest 0, then normalize
    v[0] = 1.0;
    // Normalize
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

/// Create a different query vector for testing.
#[allow(dead_code)]
fn make_query_vec_alt(dim: u32) -> Vec<f32> {
    let mut v = vec![0.0; dim as usize];
    v[1] = 1.0;
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

#[tokio::test]
async fn search_passages_lexical_only_returns_results() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Create a page with a long body to ensure passages are created
    let long_body = "# Heading\n\n".to_string() + &"word ".repeat(200);
    let page = make_page(ws, proj, "notes/lexical.md", &long_body);
    store.writer.upsert_page(page).await.unwrap();

    // Search with only lexical (no query_vector)
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "word".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(!hits.is_empty(), "expected lexical hits");
    for hit in &hits {
        assert!(
            hit.lexical_rank.is_some(),
            "all hits should have lexical_rank"
        );
        assert!(
            hit.dense_rank.is_none(),
            "no dense_rank without query_vector"
        );
        assert!(hit.rrf_score.is_some());
    }
}

#[tokio::test]
async fn search_passages_hybrid_empty_query_returns_empty() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let page = make_page(ws, proj, "notes/empty.md", "# Title\n\nBody text.");
    store.writer.upsert_page(page).await.unwrap();

    // Empty query should return empty
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(hits.is_empty(), "empty query should return empty results");
}

#[tokio::test]
async fn search_passages_hybrid_zero_limit_returns_empty() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let page = make_page(ws, proj, "notes/zero.md", "# Title\n\nBody text.");
    store.writer.upsert_page(page).await.unwrap();

    // Zero limit should return empty
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "text".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            0,
        )
        .await
        .unwrap();

    assert!(hits.is_empty(), "zero limit should return empty results");
}

#[tokio::test]
async fn search_passages_dense_rejects_wrong_dim() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let page = make_page(ws, proj, "notes/dim.md", "# Title\n\nBody text.");
    store.writer.upsert_page(page).await.unwrap();

    // Query with wrong dim (512 elements) but dim param set to 768
    // should error at the dim check before even querying embeddings
    let result = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "text".to_string(),
            Some(make_query_vec(512)),
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await;

    assert!(
        result.is_err(),
        "dense search with mismatched dim should error"
    );
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("dim"),
        "error should mention dimension mismatch: {err_str}"
    );
}

#[tokio::test]
async fn search_passages_hybrid_workspace_isolation() {
    let (_tmp, store) = make_store();
    let (ws1, proj1) = make_scope(&store).await;

    // Create a second workspace/project
    let scope2 = create_explicit_scope(&store.writer, "other-workspace", "other-project")
        .await
        .unwrap();
    let (ws2, proj2) = (scope2.workspace_id, scope2.project_id);

    // Page in workspace 1
    let page1 = make_page(
        ws1,
        proj1,
        "notes/ws1.md",
        "# Title\n\nUniqueTokenWorkspace1.",
    );
    store.writer.upsert_page(page1).await.unwrap();

    // Page in workspace 2
    let page2 = make_page(
        ws2,
        proj2,
        "notes/ws2.md",
        "# Title\n\nUniqueTokenWorkspace2.",
    );
    store.writer.upsert_page(page2).await.unwrap();

    // Search in workspace 1 should NOT find workspace 2's passage
    let hits = store
        .reader
        .search_passages_hybrid(
            ws1,
            proj1,
            "UniqueTokenWorkspace2".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(
        hits.is_empty(),
        "passage from different workspace must not appear"
    );

    // Search in workspace 2 should find its own passage
    let hits2 = store
        .reader
        .search_passages_hybrid(
            ws2,
            proj2,
            "UniqueTokenWorkspace2".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(
        !hits2.is_empty(),
        "should find passage in its own workspace"
    );
}

#[tokio::test]
async fn search_passages_hybrid_project_isolation() {
    let (_tmp, store) = make_store();
    let (ws, proj1) = make_scope(&store).await;

    // Create a second project in the same workspace
    let scope2 = create_explicit_scope(&store.writer, "test-workspace", "other-project")
        .await
        .unwrap();
    let (_, proj2) = (scope2.workspace_id, scope2.project_id);

    // Page in project 1
    let page1 = make_page(
        ws,
        proj1,
        "notes/proj1.md",
        "# Title\n\nUniqueTokenProject1.",
    );
    store.writer.upsert_page(page1).await.unwrap();

    // Page in project 2
    let page2 = make_page(
        ws,
        proj2,
        "notes/proj2.md",
        "# Title\n\nUniqueTokenProject2.",
    );
    store.writer.upsert_page(page2).await.unwrap();

    // Search in project 1 should NOT find project 2's passage
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj1,
            "UniqueTokenProject2".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(
        hits.is_empty(),
        "passage from different project must not appear"
    );

    // Search in project 2 should find its own passage
    let hits2 = store
        .reader
        .search_passages_hybrid(
            ws,
            proj2,
            "UniqueTokenProject2".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(!hits2.is_empty(), "should find passage in its own project");
}

#[tokio::test]
async fn search_passages_hybrid_dense_only_works_when_lexical_empty() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Create a page with content that won't match lexically
    let page = make_page(
        ws,
        proj,
        "notes/dense.md",
        "# Heading\n\nThis passage has unique dense content.",
    );
    let page_id = store.writer.upsert_page(page).await.unwrap();

    // Get passage ID
    let conn = Connection::open(store.db_path()).unwrap();
    let passage_id: Vec<u8> = conn
        .query_row(
            "SELECT id FROM page_passages WHERE page_id = ?1 LIMIT 1",
            [page_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();

    // Insert embedding matching our query vector
    let qv = make_query_vec(768);
    insert_passage_embedding(&conn, &passage_id, "test", "model", 768, &qv).await;

    // Search with query vector but a query that won't match lexically
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "nonexistentxyz".to_string(), // won't match in FTS
            Some(qv.clone()),
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(
        !hits.is_empty(),
        "dense-only path should work when lexical returns nothing"
    );
    for hit in &hits {
        assert!(hit.dense_rank.is_some(), "hits should have dense_rank");
    }
}

#[tokio::test]
async fn search_passages_hybrid_rrf_fusion_prefers_both_streams() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Create a page with content that matches both lexically and densely
    let page = make_page(
        ws,
        proj,
        "notes/fusion.md",
        "# Heading\n\nThis unique keyword appears here for testing fusion.",
    );
    let page_id = store.writer.upsert_page(page).await.unwrap();

    // Get passage ID
    let conn = Connection::open(store.db_path()).unwrap();
    let passage_id: Vec<u8> = conn
        .query_row(
            "SELECT id FROM page_passages WHERE page_id = ?1 LIMIT 1",
            [page_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();

    // Insert embedding matching our query vector
    let qv = make_query_vec(768);
    insert_passage_embedding(&conn, &passage_id, "test", "model", 768, &qv).await;

    // Search with both lexical query and matching dense vector
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "fusion".to_string(), // matches lexically
            Some(qv),
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    assert!(!hits.is_empty(), "hybrid search should return hits");
    // The passage appearing in both streams should have both ranks set
    let hit = &hits[0];
    assert!(
        hit.lexical_rank.is_some(),
        "should have lexical_rank from FTS"
    );
    assert!(
        hit.dense_rank.is_some(),
        "should have dense_rank from embeddings"
    );
    assert!(hit.rrf_score.is_some(), "should have RRF score");
    // RRF score for appearing in both should be higher than single-stream
    let rrf = hit.rrf_score.unwrap();
    // 1/(60+1) + 1/(60+1) ≈ 0.0328 for rank 1 in both
    assert!(
        rrf > 1.0 / 61.0,
        "RRF score for dual-stream hit should exceed single-stream"
    );
}

#[tokio::test]
async fn search_passages_hybrid_overlapping_passages_deduplicated() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Create a page with long content to generate multiple overlapping passages
    // D1a creates passages with 48-token overlap, so a long section will have
    // passages that share text content
    let mut body = "# Heading\n\n".to_string();
    for i in 0..100 {
        body.push_str(&format!("Token{} ", i));
    }
    let page = make_page(ws, proj, "notes/overlap.md", &body);
    store.writer.upsert_page(page).await.unwrap();

    // Search for a token that appears in multiple passages
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "Token50".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    // Each passage has a unique passage_id, so they should all be distinct hits
    // but we verify no duplicate passage_ids
    let mut seen = std::collections::HashSet::new();
    for hit in &hits {
        assert!(
            seen.insert(hit.passage_id),
            "passage_id must be unique in results"
        );
    }
}

#[tokio::test]
async fn search_passages_hybrid_tail_blindness_regression() {
    // This is the critical tail-blindness test: a term appearing only after
    // byte 8000 in a page body MUST be found by passage dense retrieval.
    // The whole project exists to fix this - page-level search truncates at ~8000 bytes
    // but passage-level search should find content anywhere in the page.

    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Build a very long page body - the target term appears at ~byte 10000
    let mut body = "# Heading\n\n".to_string();
    // Add ~10000 bytes of filler
    for i in 0..1500 {
        body.push_str(&format!("FillerWord{} ", i));
    }
    // Target term at the tail
    body.push_str("TAIL_TARGET_TOKEN ");
    // Add more after
    for i in 1500..1600 {
        body.push_str(&format!("MoreFiller{} ", i));
    }

    let page = make_page(ws, proj, "notes/tail.md", &body);
    let page_id = store.writer.upsert_page(page).await.unwrap();

    // Verify the page body is long enough
    let conn = Connection::open(store.db_path()).unwrap();
    let page_body_len: i64 = conn
        .query_row(
            "SELECT LENGTH(body) FROM pages WHERE id = ?1",
            [page_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        page_body_len > 10000,
        "page body must be >10000 bytes for tail test"
    );

    // Get passage ID for a passage at the tail
    let passage_id: Vec<u8> = conn
        .query_row(
            "SELECT id FROM page_passages WHERE page_id = ?1 ORDER BY start_byte DESC LIMIT 1",
            [page_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();

    // Insert embedding for the tail passage
    let qv = make_query_vec(768);
    insert_passage_embedding(&conn, &passage_id, "test", "model", 768, &qv).await;

    // Search with dense vector matching the tail passage
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "nonexistent".to_string(), // won't match lexically in FTS
            Some(qv),
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    // The tail passage should be found despite being at byte >8000
    assert!(
        !hits.is_empty(),
        "tail-blindness: dense retrieval must find passage at byte >8000"
    );
    let hit = &hits[0];
    assert!(
        hit.start_byte > 8000,
        "found passage must be at byte >8000, got {}",
        hit.start_byte
    );
    assert!(
        hit.dense_rank.is_some(),
        "should have dense_rank from tail passage embedding"
    );
}

#[tokio::test]
async fn search_passages_hybrid_rrf_deterministic_tie_break() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Create two pages with identical content structure, same lexical match
    let page1 = make_page(
        ws,
        proj,
        "notes/a.md",
        "# Title\n\nShared keyword appears here.",
    );
    let page2 = make_page(
        ws,
        proj,
        "notes/b.md",
        "# Title\n\nShared keyword appears here.",
    );
    store.writer.upsert_page(page1).await.unwrap();
    store.writer.upsert_page(page2).await.unwrap();

    // Search with same query twice - results must be deterministically ordered
    let hits1 = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "Shared".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    let hits2 = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "Shared".to_string(),
            None,
            "test".to_string(),
            "model".to_string(),
            768,
            10,
        )
        .await
        .unwrap();

    // Results must be identically ordered (deterministic tie-break by passage_id)
    assert_eq!(
        hits1.len(),
        hits2.len(),
        "both runs should return same number of hits"
    );
    for (h1, h2) in hits1.iter().zip(hits2.iter()) {
        assert_eq!(
            h1.passage_id, h2.passage_id,
            "same query must return passages in same order (deterministic tie-break by passage_id)"
        );
    }
}

#[tokio::test]
async fn store_passage_embeddings_writes_to_table() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Create a page with a passage
    let page = make_page(
        ws,
        proj,
        "notes/embed_test.md",
        "# Heading\n\nTest passage content here.",
    );
    let page_id = store.writer.upsert_page(page).await.unwrap();

    // Get the passage ID that was created
    let conn = Connection::open(store.db_path()).unwrap();
    let passage_id: Vec<u8> = conn
        .query_row(
            "SELECT id FROM page_passages WHERE page_id = ?1 LIMIT 1",
            [page_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();

    // Create embedding vectors
    let vec = vec![0.1, 0.2, 0.3, 0.4];
    let embedding_bytes = f32_vec_to_bytes(&vec);
    let passage_id_uuid = uuid::Uuid::from_slice(&passage_id).unwrap();

    // Write embedding using the production store_passage_embeddings API
    let embedding_write = ai_memory_store::PassageEmbeddingWrite {
        passage_id: passage_id_uuid,
        vector_bytes: embedding_bytes.clone(),
        provider: "test".to_string(),
        model: "model".to_string(),
        dim: 4,
    };

    store
        .writer
        .store_passage_embeddings(vec![embedding_write])
        .await
        .unwrap();

    // Verify the embedding was written to the database
    let row_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_passage_embeddings WHERE passage_id = ?1",
            rusqlite::params![&passage_id],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(row_count, 1, "passage embedding should be written to table");

    // Verify the embedding can be queried
    let retrieved_vec: Vec<u8> = conn
        .query_row(
            "SELECT embedding FROM page_passage_embeddings WHERE passage_id = ?1",
            rusqlite::params![&passage_id],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        retrieved_vec, embedding_bytes,
        "retrieved embedding should match written vector"
    );
}
