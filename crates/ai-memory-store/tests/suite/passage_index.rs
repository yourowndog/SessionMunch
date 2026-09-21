//! Tests for replace_page_sections_and_passages and backfill_sections.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{PassageEmbeddingWrite, Store, f32_vec_to_bytes};
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

fn count_sections(conn: &Connection, page_id: &[u8]) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM page_sections WHERE page_id = ?1",
        [page_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn count_passages(conn: &Connection, page_id: &[u8]) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM page_passages WHERE page_id = ?1",
        [page_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn section_heading(conn: &Connection, page_id: &[u8]) -> String {
    conn.query_row(
        "SELECT heading FROM page_sections WHERE page_id = ?1 AND ordinal = 0",
        [page_id],
        |row| row.get(0),
    )
    .unwrap()
}

/// The write path (`upsert_page_in_tx`) already calls
/// `replace_page_sections_and_passages` inside the same transaction as the
/// page write (AGENTS.md invariant #3). An uncommitted replace must leave
/// zero trace: not just the same row count, but the exact same content.
#[tokio::test]
async fn replace_rolls_back_on_mid_transaction_failure() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let page = make_page(ws, proj, "notes/test.md", "# Hello\n\nWorld.");
    let page_id = store.writer.upsert_page(page).await.unwrap();

    let conn = Connection::open(store.db_path()).unwrap();
    let section_count = count_sections(&conn, page_id.as_bytes());
    let heading = section_heading(&conn, page_id.as_bytes());
    assert!(
        section_count > 0,
        "expected sections to be created by the writer path"
    );
    assert_eq!(heading, "Hello");

    // Simulate a mid-transaction failure: run the replace with a body whose
    // heading text differs ("Replaced" vs "Hello"), then drop the
    // transaction without committing (an explicit rollback, standing in for
    // whatever caused the caller's transaction to abort).
    let mut conn2 = Connection::open(store.db_path()).unwrap();
    {
        let tx = conn2.transaction().unwrap();
        ai_memory_store::passage_index::replace_page_sections_and_passages(
            &tx,
            page_id,
            ws,
            proj,
            "# Replaced\n\nNew body.",
        )
        .unwrap();
        // tx dropped here without .commit() -> rollback.
    }

    let conn3 = Connection::open(store.db_path()).unwrap();
    let section_count_after_rollback = count_sections(&conn3, page_id.as_bytes());
    let heading_after_rollback = section_heading(&conn3, page_id.as_bytes());
    assert_eq!(
        section_count, section_count_after_rollback,
        "an uncommitted replace must not persist any section rows"
    );
    assert_eq!(
        heading, heading_after_rollback,
        "an uncommitted replace must not change existing section content \
         (this is the real proof of rollback: a row-count match alone \
         cannot distinguish 'rolled back' from 'replaced with a body that \
         happens to produce the same number of sections')"
    );
}

/// Gate item 2's real claim: superseding a page THROUGH THE WRITER PATH
/// (`upsert_page`, not a direct call to `replace_page_sections_and_passages`)
/// replaces its derived rows with the new version's content, and the old
/// version's page row remains reachable (supersession never destroys history
/// — AGENTS.md invariant #16).
#[tokio::test]
async fn supersede_through_writer_replaces_derived_rows() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let v1 = make_page(ws, proj, "notes/supersede.md", "# First\n\nOriginal body.");
    let page_id_v1 = store.writer.upsert_page(v1).await.unwrap();

    let conn = Connection::open(store.db_path()).unwrap();
    assert_eq!(section_heading(&conn, page_id_v1.as_bytes()), "First");

    let v2 = make_page(ws, proj, "notes/supersede.md", "# Second\n\nNew body.");
    let page_id_v2 = store.writer.upsert_page(v2).await.unwrap();

    assert_ne!(
        page_id_v1, page_id_v2,
        "supersession must create a new page version id"
    );

    // The new version's derived rows reflect the new content.
    assert_eq!(section_heading(&conn, page_id_v2.as_bytes()), "Second");
    assert!(count_sections(&conn, page_id_v2.as_bytes()) > 0);

    // The old version's page row remains reachable (history preserved),
    // even though its derived section/passage rows are page-version-scoped
    // and were not carried forward (only the new version has current rows).
    let old_page_still_reachable: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE id = ?1",
            [page_id_v1.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        old_page_still_reachable, 1,
        "the superseded page row must remain reachable, never deleted"
    );
}

/// Deleting a page's sections must cascade-delete its passages (FK
/// `page_passages.section_id -> page_sections.id ON DELETE CASCADE`), and the
/// `page_passages_fts` triggers must keep the FTS index in sync.
#[tokio::test]
async fn fk_cascade_deletes_sections_and_passages() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let page = make_page(
        ws,
        proj,
        "notes/cascade.md",
        "# Heading\n\nSome body text for the passage.",
    );
    let page_id = store.writer.upsert_page(page).await.unwrap();

    let conn = Connection::open(store.db_path()).unwrap();
    assert!(count_sections(&conn, page_id.as_bytes()) > 0);
    assert!(count_passages(&conn, page_id.as_bytes()) > 0);

    conn.execute(
        "DELETE FROM page_sections WHERE page_id = ?1",
        [page_id.as_bytes()],
    )
    .unwrap();

    assert_eq!(count_sections(&conn, page_id.as_bytes()), 0);
    assert_eq!(
        count_passages(&conn, page_id.as_bytes()),
        0,
        "FK cascade must delete passages when their section is deleted"
    );

    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM page_passages_fts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        fts_count, 0,
        "the page_passages_fts AFTER DELETE trigger must clean up FTS rows"
    );
}

/// Re-running the replace for the same page_id must not create duplicate or
/// orphaned rows: it is a delete-then-reinsert, so the counts stay the same.
#[tokio::test]
async fn replace_is_idempotent_on_rerun() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let page = make_page(ws, proj, "notes/idempotent.md", "# First\n\nBody.");
    let page_id = store.writer.upsert_page(page).await.unwrap();

    let mut conn = Connection::open(store.db_path()).unwrap();
    let before_sections = count_sections(&conn, page_id.as_bytes());
    let before_passages = count_passages(&conn, page_id.as_bytes());

    {
        let tx = conn.transaction().unwrap();
        ai_memory_store::passage_index::replace_page_sections_and_passages(
            &tx,
            page_id,
            ws,
            proj,
            "# First\n\nBody.",
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let after_sections = count_sections(&conn, page_id.as_bytes());
    let after_passages = count_passages(&conn, page_id.as_bytes());

    assert_eq!(
        before_sections, after_sections,
        "re-running replace with identical body must not duplicate sections"
    );
    assert_eq!(
        before_passages, after_passages,
        "re-running replace with identical body must not duplicate passages"
    );
}

/// `backfill_sections` must process every latest page in the scope exactly
/// once regardless of batch size, and never touch pages outside the scope.
#[tokio::test]
async fn backfill_batch_size_one() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    let page1 = make_page(ws, proj, "notes/one.md", "# One\n\nBody one.");
    let page2 = make_page(ws, proj, "notes/two.md", "# Two\n\nBody two.");
    store.writer.upsert_page(page1).await.unwrap();
    store.writer.upsert_page(page2).await.unwrap();

    let processed = store.writer.backfill_sections(ws, proj, 1).await.unwrap();

    assert_eq!(processed, 2, "backfill must process both latest pages");
}

/// Backfill batch size must be bounded to at least 1 (never divide by zero
/// or loop unboundedly) and must not exceed the number of eligible pages.
#[tokio::test]
async fn backfill_batch_bounds() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    for i in 0..5 {
        let page = make_page(
            ws,
            proj,
            &format!("notes/page{i}.md"),
            &format!("# Page {i}\n\nBody {i}."),
        );
        store.writer.upsert_page(page).await.unwrap();
    }

    // A batch_size of 0 must be treated as at least 1, not panic or loop
    // forever.
    let processed_zero_batch = store.writer.backfill_sections(ws, proj, 0).await.unwrap();
    assert_eq!(processed_zero_batch, 5);

    // A batch_size larger than the row count must process all rows exactly
    // once, not error or overcount.
    let processed_large_batch = store
        .writer
        .backfill_sections(ws, proj, 1000)
        .await
        .unwrap();
    assert_eq!(processed_large_batch, 5);
}

/// Writing a page through the writer path creates section/passage rows.
/// A subsequent call to `store_passage_embeddings` persists real embedding
/// vectors to `page_passage_embeddings`, and `passage_candidates` returns
/// no more candidates for that triple (the gap is filled).
///
/// This is the TDD proof that the dense write path actually produces
/// rows on a real page upsert — not just test fixtures seeded by hand.
#[tokio::test]
async fn upsert_page_then_store_passage_embeddings_persists_dense_vector() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Write a page with a heading + body so it produces at least one passage.
    let page = make_page(
        ws,
        proj,
        "notes/embed_test.md",
        "# Embedding Test\n\nThis passage contains distinctive vocabulary for testing.",
    );
    let page_id = store.writer.upsert_page(page).await.unwrap();

    // Verify sections and passages were created.
    let conn = Connection::open(store.db_path()).unwrap();
    assert!(
        count_sections(&conn, page_id.as_bytes()) > 0,
        "upsert must create section rows"
    );
    assert!(
        count_passages(&conn, page_id.as_bytes()) > 0,
        "upsert must create passage rows"
    );

    // Passage embeddings table is empty before we write any.
    let embeds_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_passage_embeddings",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(embeds_before, 0, "no embeddings written yet");

    // Enumerate candidates (passages missing embeddings for the test triple).
    const PROVIDER: &str = "test-embedder";
    const MODEL: &str = "test-model-128";
    const DIM: u32 = 128;

    let candidates = store
        .reader
        .passage_candidates(ws, proj, PROVIDER, MODEL, DIM)
        .await
        .unwrap();
    assert!(
        !candidates.is_empty(),
        "page with passages must appear as candidate (no embeddings yet)"
    );

    // Write deterministic embeddings for each candidate.
    let writes: Vec<PassageEmbeddingWrite> = candidates
        .iter()
        .map(|c| {
            // Deterministic 128-dim vector: all zeros except index 0 = 1.0
            let mut vec = vec![0.0f32; DIM as usize];
            vec[0] = 1.0;
            PassageEmbeddingWrite {
                passage_id: c.id,
                vector_bytes: f32_vec_to_bytes(&vec),
                provider: PROVIDER.to_string(),
                model: MODEL.to_string(),
                dim: DIM,
            }
        })
        .collect();

    store.writer.store_passage_embeddings(writes).await.unwrap();

    // Verify embedding rows landed.
    let embeds_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_passage_embeddings",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        embeds_after > 0,
        "store_passage_embeddings must have written rows to page_passage_embeddings"
    );

    // Candidates for the same triple must now be empty.
    let candidates_after = store
        .reader
        .passage_candidates(ws, proj, PROVIDER, MODEL, DIM)
        .await
        .unwrap();
    assert!(
        candidates_after.is_empty(),
        "passage_candidates must return empty after embeddings are written"
    );
}

/// Backfill path: pages written through the writer path that predate the
/// embedding model should be fillable via `passage_candidates` +
/// `store_passage_embeddings`. After backfill, `search_passages_hybrid`
/// with a matching query vector must return hits with `dense_rank` set,
/// proving the dense stream genuinely participates in RRF fusion.
#[tokio::test]
async fn backfill_populates_embeddings_and_dense_stream_participates_in_hybrid_search() {
    let (_tmp, store) = make_store();
    let (ws, proj) = make_scope(&store).await;

    // Write two pages with distinctive vocabulary.
    let page_a = make_page(
        ws,
        proj,
        "notes/alpha.md",
        "# Alpha Page\n\nThe alpha section contains unique alpha tokens.",
    );
    let page_b = make_page(
        ws,
        proj,
        "notes/beta.md",
        "# Beta Page\n\nThe beta section contains unique beta tokens.",
    );
    store.writer.upsert_page(page_a).await.unwrap();
    store.writer.upsert_page(page_b).await.unwrap();

    // No embeddings yet — all passages are candidates.
    const PROVIDER: &str = "hybrid-test-embedder";
    const MODEL: &str = "hash-128";
    const DIM: u32 = 128;

    let candidates = store
        .reader
        .passage_candidates(ws, proj, PROVIDER, MODEL, DIM)
        .await
        .unwrap();
    assert!(
        !candidates.is_empty(),
        "both pages should produce passage candidates"
    );

    // Deterministic embedder: each word hashes to one dimension.
    fn deterministic_embed(text: &str, dim: usize) -> Vec<f32> {
        let mut vec = vec![0.0f32; dim];
        for word in text.split_whitespace() {
            let w = word.to_lowercase();
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in w.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            let bucket = (h % dim as u64) as usize;
            vec[bucket] += 1.0;
        }
        let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v /= norm;
            }
        }
        vec
    }

    // Backfill: embed each candidate and write.
    let writes: Vec<PassageEmbeddingWrite> = candidates
        .iter()
        .map(|c| PassageEmbeddingWrite {
            passage_id: c.id,
            vector_bytes: f32_vec_to_bytes(&deterministic_embed(&c.text, DIM as usize)),
            provider: PROVIDER.to_string(),
            model: MODEL.to_string(),
            dim: DIM,
        })
        .collect();
    store.writer.store_passage_embeddings(writes).await.unwrap();

    // Query for "alpha" — the dense vector should be computed with the same
    // embedder, and search_passages_hybrid should fuse dense + lexical via RRF.
    let query_vec = deterministic_embed("alpha", DIM as usize);
    let hits = store
        .reader
        .search_passages_hybrid(
            ws,
            proj,
            "alpha".to_string(),
            Some(query_vec),
            PROVIDER.to_string(),
            MODEL.to_string(),
            DIM,
            10,
        )
        .await
        .unwrap();

    // At least one hit must have surfaced via the dense stream.
    let dense_participating = hits.iter().filter(|h| h.dense_rank.is_some()).count();
    assert!(
        dense_participating > 0,
        "dense stream must participate in RRF fusion after backfill \
         (got {} hits total, {} with dense_rank set)",
        hits.len(),
        dense_participating
    );

    // The alpha page should appear in the results.
    let alpha_found = hits
        .iter()
        .take(5)
        .any(|h| h.page_path.as_str() == "notes/alpha.md");
    assert!(
        alpha_found,
        "alpha page must rank in top 5 for 'alpha' query when dense is enabled"
    );
}
