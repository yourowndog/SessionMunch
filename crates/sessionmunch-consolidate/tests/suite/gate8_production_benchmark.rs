//! Gate-8 production benchmark (card t_30a08e43).
//!
//! Runs the frozen H1 corpus + probes against the PRODUCTION embedder
//! (`all-MiniLM-L6-v2` via `LocalEmbedder`, 384-dim, provider "local")
//! instead of the hermetic word-hash test embedder, and reports the
//! deployment-gate numbers: indexing time, dense-index time, peak RSS,
//! disk footprint, per-mode p50/p95 query latency, and recall@k / MRR /
//! nDCG@5 for page-mode, passage lexical-only, and passage dense-fused.
//!
//! `#[ignore]`d: needs the ~87 MB model files (not in CI). Run on a
//! Weakling-class CPU-only host (or under `taskset -c 0-5` to simulate
//! Weakling's 6 cores) with:
//!
//! ```bash
//! SESSIONMUNCH_TEST_MODELS_DIR=<dir containing all-MiniLM-L6-v2> \
//!   cargo test -p sessionmunch-consolidate --lib \
//!   integration::gate8_production_benchmark -- --ignored --nocapture
//! ```

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use sessionmunch_core::{NewPage, PagePath, Tier};
use sessionmunch_llm::{Embedder, LocalEmbedder};
use sessionmunch_store::{PassageEmbeddingWrite, Store, create_explicit_scope, f32_vec_to_bytes};
use tempfile::TempDir;

const RECALL_KS: &[usize] = &[1, 3, 5, 10];

fn make_corpus() -> Vec<(&'static str, String)> {
    let mut tail_body = String::from("# Tail Test\n\n");
    for i in 0..1500 {
        tail_body.push_str(&format!("filler{i} "));
    }
    tail_body.push_str("UNIQUE_TAIL_TOKEN_9000 ");
    for i in 1500..1600 {
        tail_body.push_str(&format!("morefiller{i} "));
    }
    assert!(
        tail_body.len() > 8000,
        "byte-8000 tail fixture must genuinely exceed 8000 bytes, got {}",
        tail_body.len()
    );

    vec![
        ("notes/byte_8000_tail.md", tail_body),
        (
            "notes/heading_relevance.md",
            "# Main Heading\n\n## Section Alpha\n\nContent about alpha.\n\n## Section Beta\n\nContent about beta."
                .to_string(),
        ),
        (
            "notes/compact_budget.md",
            "# Budget Test\n\nShort word0 word1 word2 word3 word4 word5 word6 word7 word8 word9 word10"
                .to_string(),
        ),
        (
            "notes/lexical_dense_fusion.md",
            "# Fusion Test\n\nThis passage contains unique keywords for both lexical and dense matching."
                .to_string(),
        ),
        (
            "notes/explicit_scope.md",
            "# Scope Test\n\nContent scoped to explicit workspace and project."
                .to_string(),
        ),
        (
            "notes/global_default.md",
            "# Global Default Test\n\nContent for testing global search semantics."
                .to_string(),
        ),
        (
            "notes/raw_fallback.md",
            "# Raw Fallback Test\n\nContent to test raw observation fallback behavior."
                .to_string(),
        ),
        (
            "notes/page_mode_compat.md",
            "# Page Mode Test\n\nContent for testing page-mode retrieval compatibility."
                .to_string(),
        ),
        (
            "notes/parser_fixtures.md",
            "# Parser Fixture\n\n## ATX Heading\n\nSome inline code and bold text.\n\n### Level 3\n\nMore content."
                .to_string(),
        ),
        (
            "notes/capture_regression.md",
            "# Capture Regression\n\nContent to verify capture pipeline works correctly."
                .to_string(),
        ),
        (
            "notes/migration_backfill.md",
            "# Migration Backfill\n\nContent testing V64 migration and section passage backfill."
                .to_string(),
        ),
        (
            "notes/failure_fallback.md",
            "# Failure Fallback\n\nContent testing graceful handling of retrieval failures."
                .to_string(),
        ),
    ]
}

fn make_probes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("UNIQUE_TAIL_TOKEN_9000", "notes/byte_8000_tail.md"),
        ("Section Alpha", "notes/heading_relevance.md"),
        ("Section Beta", "notes/heading_relevance.md"),
        ("Budget Test", "notes/compact_budget.md"),
        ("unique keywords", "notes/lexical_dense_fusion.md"),
        ("Scope Test", "notes/explicit_scope.md"),
        ("Global Default", "notes/global_default.md"),
        ("raw observation fallback", "notes/raw_fallback.md"),
        ("Page Mode", "notes/page_mode_compat.md"),
        ("inline code", "notes/parser_fixtures.md"),
        ("Capture Regression", "notes/capture_regression.md"),
        ("Migration Backfill", "notes/migration_backfill.md"),
        ("Failure Fallback", "notes/failure_fallback.md"),
    ]
}

/// Peak RSS of this process in MiB (VmHWM from /proc; Linux-only).
fn peak_rss_mib() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: f64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024.0);
        }
    }
    None
}

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn percentile(sorted_ms: &mut [f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    sorted_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((sorted_ms.len() as f64 * p).floor() as usize).min(sorted_ms.len() - 1);
    sorted_ms[idx]
}

/// Standard per-probe metrics: rank per probe (1-based, None = miss in top-5).
fn report(ranks: &[Option<usize>], total: usize) -> (Vec<(usize, f64)>, f64, f64) {
    let recall = RECALL_KS
        .iter()
        .map(|&k| {
            let hits = ranks
                .iter()
                .filter(|r| r.is_some_and(|rank| rank <= k))
                .count();
            (k, hits as f64 / total as f64)
        })
        .collect();
    let mrr = if ranks.is_empty() {
        0.0
    } else {
        ranks
            .iter()
            .map(|r| r.map_or(0.0, |rank| 1.0 / rank as f64))
            .sum::<f64>()
            / ranks.len() as f64
    };
    let ndcg = if ranks.is_empty() {
        0.0
    } else {
        ranks
            .iter()
            .map(|r| match r {
                Some(rank) if *rank <= 5 => 1.0 / ((*rank as f64) + 1.0).log2(),
                _ => 0.0,
            })
            .sum::<f64>()
            / ranks.len() as f64
    };
    (recall, mrr, ndcg)
}

#[tokio::test]
#[ignore = "needs the fetched all-MiniLM-L6-v2 model files (SESSIONMUNCH_TEST_MODELS_DIR)"]
async fn gate8_production_embedder_benchmark() {
    let models_root = std::env::var("SESSIONMUNCH_TEST_MODELS_DIR")
        .expect("set SESSIONMUNCH_TEST_MODELS_DIR to a dir containing all-MiniLM-L6-v2");

    let load_start = Instant::now();
    let embedder = LocalEmbedder::load(Path::new(&models_root)).expect("load production embedder");
    let model_load_time = load_start.elapsed();
    assert_eq!(embedder.dim(), 384);
    let provider = embedder.provider().to_string();
    let model = embedder.model().to_string();
    let dim = embedder.dim();

    let corpus = make_corpus();
    let probes = make_probes();

    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let scope = create_explicit_scope(&store.writer, "gate8-workspace", "gate8-project")
        .await
        .expect("create scope");
    let ws = scope.workspace_id;
    let proj = scope.project_id;

    let indexing_start = Instant::now();
    for (path, body) in &corpus {
        let page = NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path.to_string()).expect("path"),
            title: path.to_string(),
            body: body.clone(),
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

    // Dense phase through the real production write path.
    let dense_start = Instant::now();
    let candidates = store
        .reader
        .passage_candidates(ws, proj, &provider, &model, dim)
        .await
        .expect("enumerate passage candidates");
    assert!(
        !candidates.is_empty(),
        "corpus must produce passages (dense phase would be vacuous)"
    );
    let candidate_count = candidates.len();
    let mut embed_time = Duration::ZERO;
    let mut writes = Vec::with_capacity(candidate_count);
    for c in &candidates {
        let t = Instant::now();
        let vec = embedder.embed(&c.text).await.expect("embed passage");
        embed_time += t.elapsed();
        assert_eq!(vec.len(), dim as usize);
        writes.push(PassageEmbeddingWrite {
            passage_id: c.id,
            vector_bytes: f32_vec_to_bytes(&vec),
            provider: provider.clone(),
            model: model.clone(),
            dim,
        });
    }
    store
        .writer
        .store_passage_embeddings(writes)
        .await
        .expect("seed passage embeddings");
    let dense_indexing_time = dense_start.elapsed();

    let disk_bytes = dir_bytes(tmp.path());
    let peak_rss = peak_rss_mib();

    // Probe loop: three modes, per-query latency each.
    let mut page_ranks: Vec<Option<usize>> = Vec::new();
    let mut lex_ranks: Vec<Option<usize>> = Vec::new();
    let mut dense_ranks: Vec<Option<usize>> = Vec::new();
    let mut page_lat: Vec<f64> = Vec::new();
    let mut lex_lat: Vec<f64> = Vec::new();
    let mut dense_lat: Vec<f64> = Vec::new();
    let mut dense_ranked_hits = 0usize;

    for (query, expected_path) in &probes {
        let t = Instant::now();
        let page_hits = store
            .reader
            .search_pages_for_project(ws, proj, query.to_string(), 10, None)
            .await
            .expect("search pages");
        page_lat.push(t.elapsed().as_secs_f64() * 1000.0);
        page_ranks.push(
            page_hits
                .iter()
                .position(|hit| hit.path.as_str() == *expected_path)
                .map(|p| p + 1)
                .filter(|r| *r <= 5),
        );

        let t = Instant::now();
        let lex_hits = store
            .reader
            .search_passages_hybrid(
                ws,
                proj,
                query.to_string(),
                None,
                String::new(),
                String::new(),
                0,
                10,
            )
            .await
            .expect("search passages lexical");
        lex_lat.push(t.elapsed().as_secs_f64() * 1000.0);
        lex_ranks.push(
            lex_hits
                .iter()
                .position(|hit| hit.page_path.as_str() == *expected_path)
                .map(|p| p + 1)
                .filter(|r| *r <= 5),
        );

        let qvec = embedder.embed(query).await.expect("embed query");
        let t = Instant::now();
        let dense_hits = store
            .reader
            .search_passages_hybrid(
                ws,
                proj,
                query.to_string(),
                Some(qvec),
                provider.clone(),
                model.clone(),
                dim,
                10,
            )
            .await
            .expect("search passages dense");
        dense_lat.push(t.elapsed().as_secs_f64() * 1000.0);
        if dense_hits.iter().any(|hit| hit.dense_rank.is_some()) {
            dense_ranked_hits += 1;
        }
        dense_ranks.push(
            dense_hits
                .iter()
                .position(|hit| hit.page_path.as_str() == *expected_path)
                .map(|p| p + 1)
                .filter(|r| *r <= 5),
        );
    }

    let total = probes.len();
    let (page_recall, page_mrr, page_ndcg) = report(&page_ranks, total);
    let (lex_recall, lex_mrr, lex_ndcg) = report(&lex_ranks, total);
    let (dense_recall, dense_mrr, dense_ndcg) = report(&dense_ranks, total);

    println!("=== GATE-8 PRODUCTION BENCHMARK (all-MiniLM-L6-v2, 384-dim) ===");
    println!("Embedder: provider={provider} model={model} dim={dim}");
    println!(
        "Corpus: {} pages, {} passages embedded",
        corpus.len(),
        candidate_count
    );
    println!("Probes: {total}");
    println!();
    println!("RESOURCE FOOTPRINT:");
    println!("  Model load time:      {model_load_time:?}");
    println!("  Page indexing time:   {indexing_time:?}");
    println!("  Dense indexing time:  {dense_indexing_time:?} (embed {embed_time:?} + write)");
    println!(
        "  Disk footprint:       {:.2} MB",
        disk_bytes as f64 / 1_048_576.0
    );
    match peak_rss {
        Some(mib) => println!("  Peak RSS (VmHWM):     {mib:.1} MiB"),
        None => println!("  Peak RSS (VmHWM):     n/a (no /proc)"),
    }
    println!();
    println!("QUERY LATENCY (ms, {total} probes per mode):");
    println!(
        "  page-mode:    p50 {:.2}  p95 {:.2}",
        percentile(&mut page_lat.clone(), 0.5),
        percentile(&mut page_lat.clone(), 0.95)
    );
    println!(
        "  passage-lex:  p50 {:.2}  p95 {:.2}",
        percentile(&mut lex_lat.clone(), 0.5),
        percentile(&mut lex_lat.clone(), 0.95)
    );
    println!(
        "  passage-dense:p50 {:.2}  p95 {:.2}",
        percentile(&mut dense_lat.clone(), 0.5),
        percentile(&mut dense_lat.clone(), 0.95)
    );
    println!();
    println!("RETRIEVAL (rank = fused top-10 position, hit = rank <= 5):");
    let fmt_recall = |r: &[(usize, f64)]| {
        r.iter()
            .map(|(k, v)| format!("recall@{k}: {v:.3}"))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!(
        "  page-mode:     {}  MRR {:.3}  nDCG@5 {:.3}",
        fmt_recall(&page_recall),
        page_mrr,
        page_ndcg
    );
    println!(
        "  passage-lex:   {}  MRR {:.3}  nDCG@5 {:.3}",
        fmt_recall(&lex_recall),
        lex_mrr,
        lex_ndcg
    );
    println!(
        "  passage-dense: {}  MRR {:.3}  nDCG@5 {:.3}",
        fmt_recall(&dense_recall),
        dense_mrr,
        dense_ndcg
    );
    println!("  Probes w/ dense-ranked hits: {dense_ranked_hits}/{total}");

    // Gate assertions: dense stream genuinely participates and clears the
    // same absolute floor the frozen lexical gate is held to.
    assert!(
        dense_ranked_hits > 0,
        "dense stream must participate in fusion"
    );
    let dense_r5 = dense_recall
        .iter()
        .find(|(k, _)| *k == 5)
        .map(|(_, v)| *v)
        .unwrap_or(0.0);
    assert!(
        dense_r5 >= 0.65,
        "dense passage recall@5 {dense_r5:.3} below 0.65 floor"
    );
}
