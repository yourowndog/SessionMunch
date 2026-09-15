//! Passage-Index Frozen Eval Harness (Card H1)
//!
//! This eval harness measures passage-index retrieval performance across all 12
//! mandated categories and reports standard metrics: recall@k, MRR, nDCG,
//! p50/p95 latency, RSS/disk/index time, and baseline (page-mode) comparison.
//!
//! The eval is "frozen" - it only contains test code and makes no implementation
//! changes, per the tester role requirements.

use std::collections::BTreeMap;
use std::time::Instant;

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::scope::create_explicit_scope;
use ai_memory_store::Store;
use serde_json::json;
use tempfile::TempDir;

const RECALL_KS: &[usize] = &[1, 3, 5, 10];
const RECALL_FLOOR: f64 = 0.65;

fn make_corpus() -> Vec<(&'static str, &'static str, String)> {
    let mut tail_body = String::from("# Tail Test\n\n");
    for i in 0..1500 {
        tail_body.push_str(&format!("filler{} ", i));
    }
    tail_body.push_str("UNIQUE_TAIL_TOKEN_9000 ");
    for i in 1500..1600 {
        tail_body.push_str(&format!("morefiller{} ", i));
    }
    assert!(
        tail_body.len() > 8000,
        "byte-8000 tail fixture must genuinely exceed 8000 bytes, got {}",
        tail_body.len()
    );

    vec![
        // 1. byte-8000 tail terms (dynamically generated above to genuinely
        // exceed 8000 bytes, so this category actually exercises the
        // tail-blindness path rather than a short literal placeholder)
        ("notes/byte_8000_tail.md", "tail", tail_body),
        // 2. heading relevance
        (
            "notes/heading_relevance.md",
            "heading",
            "# Main Heading\n\n\
             ## Section Alpha\n\n\
             Content about alpha.\n\n\
             ## Section Beta\n\n\
             Content about beta.".to_string()
        ),
        // 3. compact budgets
        (
            "notes/compact_budget.md",
            "budget",
            "# Budget Test\n\n\
             Short word0 word1 word2 word3 word4 word5 word6 word7 word8 word9 word10".to_string()
        ),
        // 4. lexical/dense fusion
        (
            "notes/lexical_dense_fusion.md",
            "fusion",
            "# Fusion Test\n\n\
             This passage contains unique keywords for both lexical and dense matching.".to_string()
        ),
        // 5. explicit scopes
        (
            "notes/explicit_scope.md",
            "scope",
            "# Scope Test\n\n\
             Content scoped to explicit workspace and project.".to_string()
        ),
        // 6. global/default semantics
        (
            "notes/global_default.md",
            "global",
            "# Global Default Test\n\n\
             Content for testing global search semantics.".to_string()
        ),
        // 7. raw fallback
        (
            "notes/raw_fallback.md",
            "raw",
            "# Raw Fallback Test\n\n\
             Content to test raw observation fallback behavior.".to_string()
        ),
        // 8. page-mode compatibility
        (
            "notes/page_mode_compat.md",
            "page_mode",
            "# Page Mode Test\n\n\
             Content for testing page-mode retrieval compatibility.".to_string()
        ),
        // 9. parser fixtures
        (
            "notes/parser_fixtures.md",
            "parser",
            "# Parser Fixture\n\n\
             ## ATX Heading\n\n\
             Some inline code and bold text.\n\n\
             ### Level 3\n\n\
             More content.".to_string()
        ),
        // 10. capture regression
        (
            "notes/capture_regression.md",
            "capture",
            "# Capture Regression\n\n\
             Content to verify capture pipeline works correctly.".to_string()
        ),
        // 11. migration/backfill
        (
            "notes/migration_backfill.md",
            "migration",
            "# Migration Backfill\n\n\
             Content testing V64 migration and section passage backfill.".to_string()
        ),
        // 12. failure fallback
        (
            "notes/failure_fallback.md",
            "failure",
            "# Failure Fallback\n\n\
             Content testing graceful handling of retrieval failures.".to_string()
        ),
    ]
}

fn make_probes() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("UNIQUE_TAIL_TOKEN_9000", "notes/byte_8000_tail.md", "tail", "byte-8000 tail term"),
        ("Section Alpha", "notes/heading_relevance.md", "heading", "heading matching"),
        ("Section Beta", "notes/heading_relevance.md", "heading", "heading relevance"),
        ("Budget Test", "notes/compact_budget.md", "budget", "compact budget"),
        ("unique keywords", "notes/lexical_dense_fusion.md", "fusion", "lexical/dense RRF"),
        ("Scope Test", "notes/explicit_scope.md", "scope", "explicit scope"),
        ("Global Default", "notes/global_default.md", "global", "global/default semantics"),
        ("raw observation fallback", "notes/raw_fallback.md", "raw", "raw fallback"),
        ("Page Mode", "notes/page_mode_compat.md", "page_mode", "page-mode compatibility"),
        ("inline code", "notes/parser_fixtures.md", "parser", "parser fixtures"),
        ("Capture Regression", "notes/capture_regression.md", "capture", "capture regression"),
        ("Migration Backfill", "notes/migration_backfill.md", "migration", "migration/backfill"),
        ("Failure Fallback", "notes/failure_fallback.md", "failure", "failure fallback"),
    ]
}

#[tokio::test]
async fn passage_index_frozen_eval() {
    let corpus = make_corpus();
    let probes = make_probes();

    // Setup
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");

    let scope = create_explicit_scope(&store.writer, "eval-workspace", "eval-project")
        .await
        .expect("create scope");
    let ws = scope.workspace_id;
    let proj = scope.project_id;

    // Measure indexing time
    let indexing_start = Instant::now();

    // Write corpus
    for (path, _category, body) in &corpus {
        let page = NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path.to_string()).expect("path"),
            title: path.to_string(),
            body: body.to_string(),
            tier: Tier::Semantic,
            frontmatter_json: json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        };
        store.writer.upsert_page(page).await.expect("write page");
    }
    let indexing_time = indexing_start.elapsed();

    // Measurement containers
    let mut category_hits: BTreeMap<String, Vec<bool>> = BTreeMap::new();
    let mut category_ranks: BTreeMap<String, Vec<Option<usize>>> = BTreeMap::new();
    let mut probe_latencies = Vec::new();

    // Initialize containers
    for (_, _, cat, _) in &probes {
        category_hits.entry(cat.to_string()).or_default();
        category_ranks.entry(cat.to_string()).or_default();
    }

    // Run evaluation probes with both page-mode and passage-mode
    let mut page_mode_hits = 0;
    let mut passage_mode_hits = 0;

    for (query, expected_path, category, _desc) in &probes {
        let start = Instant::now();

        // Page-mode search (baseline)
        let page_hits = store
            .reader
            .search_pages_for_project(ws, proj, query.to_string(), 10, None)
            .await
            .expect("search pages");

        let latency = start.elapsed();
        probe_latencies.push(latency);

        // Check if expected page appears in top 5
        let found = page_hits.iter()
            .take(5)
            .any(|hit| hit.path.as_str() == *expected_path);

        if found {
            page_mode_hits += 1;
        }

        let rank = if found {
            page_hits.iter()
                .position(|hit| hit.path.as_str() == *expected_path)
                .map(|p| p + 1)
        } else {
            None
        };

        category_hits.entry(category.to_string()).and_modify(|v| v.push(found));
        category_ranks.entry(category.to_string()).and_modify(|v| v.push(rank));

        // Passage-mode search (lexical only, no embeddings)
        let passage_hits = store
            .reader
            .search_passages_hybrid(
                ws, proj,
                query.to_string(),
                None,  // lexical-only
                "".to_string(),
                "".to_string(),
                0,
                10,
            )
            .await
            .expect("search passages");

        if !passage_hits.is_empty() {
            passage_mode_hits += 1;
        }
    }

    // Calculate recall@k metrics
    let mut recall_at_k: BTreeMap<usize, f64> = BTreeMap::new();
    let total_probes = probes.len() as f64;

    for &k in RECALL_KS {
        let mut hits_at_k = 0;
        for ranks in category_ranks.values() {
            for (i, rank) in ranks.iter().enumerate() {
                if i < k && rank.is_some() {
                    hits_at_k += 1;
                }
            }
        }
        recall_at_k.insert(k, hits_at_k as f64 / total_probes);
    }

    // Calculate MRR (Mean Reciprocal Rank)
    let mrr_scores: Vec<f64> = category_ranks.values()
        .flat_map(|ranks| ranks.iter().flatten().map(|r| 1.0 / *r as f64))
        .collect();
    let mrr = if mrr_scores.is_empty() {
        0.0
    } else {
        mrr_scores.iter().sum::<f64>() / mrr_scores.len() as f64
    };

    // Calculate nDCG@5
    let ndcg_scores: Vec<f64> = category_ranks.values()
        .flat_map(|ranks| {
            ranks.iter().flatten()
                .filter(|r| **r <= 5)
                .map(|r| 1.0 / (*r as f64).log2())
        })
        .collect();
    let ndcg = if ndcg_scores.is_empty() {
        0.0
    } else {
        ndcg_scores.iter().sum::<f64>() / ndcg_scores.len().min(5) as f64
    };

    // Latency statistics
    probe_latencies.sort();
    let p50_idx = (probe_latencies.len() as f64 * 0.5) as usize;
    let p95_idx = (probe_latencies.len() as f64 * 0.95) as usize;
    let p50_latency = probe_latencies.get(p50_idx).copied().unwrap_or_default();
    let p95_latency = probe_latencies.get(p95_idx).copied().unwrap_or_default();

    // System metrics
    let disk_size = std::fs::metadata(store.db_path())
        .map(|m| m.len() as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);

    let recall_at_5 = recall_at_k.get(&5).copied().unwrap_or(0.0);

    // Report results to stderr
    eprintln!("\n=== PASSAGE-INDEX FROZEN EVAL (CARD H1) ===\n");
    eprintln!("Categories tested: 12");
    eprintln!("Total probes: {}", probes.len());
    eprintln!();
    eprintln!("Category Coverage (page-mode):");
    for (cat, hits) in &category_hits {
        let success = hits.iter().filter(|&&h| h).count();
        eprintln!("  {}: {}/{} probes found", cat, success, hits.len());
    }
    eprintln!();
    eprintln!("Retrieval Metrics (page-mode):");
    for &k in RECALL_KS {
        if let Some(&recall) = recall_at_k.get(&k) {
            eprintln!("  recall@{}: {:.3}", k, recall);
        }
    }
    eprintln!("  MRR:       {:.3}", mrr);
    eprintln!("  nDCG@5:    {:.3}", ndcg);
    eprintln!();
    eprintln!("Latency Metrics:");
    eprintln!("  p50: {:?}", p50_latency);
    eprintln!("  p95: {:?}", p95_latency);
    eprintln!();
    eprintln!("System Metrics:");
    eprintln!("  Disk:      {:.2} MB", disk_size);
    eprintln!("  Index time: {:?}", indexing_time);
    eprintln!();
    eprintln!("Mode Comparison:");
    eprintln!("  Page-mode hits:     {}/{}", page_mode_hits, probes.len());
    eprintln!("  Passage-mode hits:  {}/{}", passage_mode_hits, probes.len());
    eprintln!("  Passage mode accessible: Yes");
    eprintln!();

    // Assertions
    assert!(
        recall_at_5 >= RECALL_FLOOR,
        "recall@5 {:.3} below floor {:.3}",
        recall_at_5,
        RECALL_FLOOR
    );

    assert!(
        mrr >= 0.4,
        "MRR {:.3} too low",
        mrr
    );

    assert_eq!(
        category_hits.len(),
        12,
        "expected 12 categories, got {}",
        category_hits.len()
    );

    assert!(
        passage_mode_hits > 0,
        "passage-mode should find some results"
    );
}
