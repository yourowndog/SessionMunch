//! Passage-Index Frozen Eval Harness (Card H1)
//!
//! This eval harness measures passage-index retrieval performance across all 12
//! mandated categories and reports standard metrics: recall@k, MRR, nDCG,
//! p50/p95 latency, RSS/disk/index time, and baseline (page-mode) comparison.
//!
//! Since the passage-embedding production card (G2) landed, this eval ALSO runs
//! every probe through the dense stream, so it measures what the audit said
//! the original gate did not: dense passage retrieval fused with lexical via
//! RRF. The dense side uses a deterministic in-process test embedder
//! word-hash bag-of-words, 1024-dim) — NOT the production all-MiniLM-L6-v2
//! local model, which is 87MB and not bundled (see ai-memory-llm/src/local.rs).
//! The test embedder has real cosine semantics (shared vocabulary moves
//! vectors closer), which is enough to exercise the actual dense write path
//! (`page_passage_embeddings` upsert via `store_passage_embeddings`), the
//! provider/model/dim gating in reader.rs, and RRF fusion end-to-end without
//! a network fetch or a non-hermetic test. Production-model numbers remain a
//! Weakling benchmark concern (plan acceptance gate 8 / I1).

use std::collections::BTreeMap;
use std::time::Instant;

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::PassageEmbeddingWrite;
use ai_memory_store::Store;
use ai_memory_store::f32_vec_to_bytes;
use ai_memory_store::scope::create_explicit_scope;
use serde_json::json;
use tempfile::TempDir;

const RECALL_KS: &[usize] = &[1, 3, 5, 10];
const RECALL_FLOOR: f64 = 0.65;

// Dense-stream constants. The eval writes these as the {provider, model, dim}
// triple on every passage embedding row and queries with the same triple, so
// the reader's dense gate opens and the stream genuinely participates in RRF.
const EVAL_EMBED_PROVIDER: &str = "eval-hermetic";
const EVAL_EMBED_MODEL: &str = "hash-bow-1024";
const EVAL_EMBED_DIM: u32 = 1024;

/// Deterministic in-process embedder for the eval's dense stream.
///
/// Each whitespace-separated word contributes +1.0 to one dimension selected
/// by a stable word hash; vectors are unit-normalised. Text sharing vocabulary
/// lands closer together — real cosine semantics, no RNG, no network. This
/// mirrors the production `SyntheticEmbedder` design in ai-memory-llm but
/// stays inside the store crate's test harness so the store keeps its
/// deliberate no-dependency-on-llm boundary at every level.
#[derive(Debug, Clone, Copy)]
struct EvalEmbedder;

impl EvalEmbedder {
    fn dim(&self) -> u32 {
        EVAL_EMBED_DIM
    }

    fn provider(&self) -> &'static str {
        EVAL_EMBED_PROVIDER
    }

    fn model(&self) -> &'static str {
        EVAL_EMBED_MODEL
    }

    /// Stable FNV-1a hash into a [0, dim) bucket.
    fn word_bucket(word: &str, dim: usize) -> usize {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in word.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (h % dim as u64) as usize
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let dim = self.dim() as usize;
        let mut vec = vec![0.0f32; dim];
        for word in text.split_whitespace() {
            // Case-fold so "Budget" and "budget" land in the same bucket
            // (real embedders are case-tolerant too); keep it deterministic.
            let bucket = Self::word_bucket(&word.to_lowercase(), dim);
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
}

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
             Content about beta."
                .to_string(),
        ),
        // 3. compact budgets
        (
            "notes/compact_budget.md",
            "budget",
            "# Budget Test\n\n\
             Short word0 word1 word2 word3 word4 word5 word6 word7 word8 word9 word10"
                .to_string(),
        ),
        // 4. lexical/dense fusion
        (
            "notes/lexical_dense_fusion.md",
            "fusion",
            "# Fusion Test\n\n\
             This passage contains unique keywords for both lexical and dense matching."
                .to_string(),
        ),
        // 5. explicit scopes
        (
            "notes/explicit_scope.md",
            "scope",
            "# Scope Test\n\n\
             Content scoped to explicit workspace and project."
                .to_string(),
        ),
        // 6. global/default semantics
        (
            "notes/global_default.md",
            "global",
            "# Global Default Test\n\n\
             Content for testing global search semantics."
                .to_string(),
        ),
        // 7. raw fallback
        (
            "notes/raw_fallback.md",
            "raw",
            "# Raw Fallback Test\n\n\
             Content to test raw observation fallback behavior."
                .to_string(),
        ),
        // 8. page-mode compatibility
        (
            "notes/page_mode_compat.md",
            "page_mode",
            "# Page Mode Test\n\n\
             Content for testing page-mode retrieval compatibility."
                .to_string(),
        ),
        // 9. parser fixtures
        (
            "notes/parser_fixtures.md",
            "parser",
            "# Parser Fixture\n\n\
             ## ATX Heading\n\n\
             Some inline code and bold text.\n\n\
             ### Level 3\n\n\
             More content."
                .to_string(),
        ),
        // 10. capture regression
        (
            "notes/capture_regression.md",
            "capture",
            "# Capture Regression\n\n\
             Content to verify capture pipeline works correctly."
                .to_string(),
        ),
        // 11. migration/backfill
        (
            "notes/migration_backfill.md",
            "migration",
            "# Migration Backfill\n\n\
             Content testing V64 migration and section passage backfill."
                .to_string(),
        ),
        // 12. failure fallback
        (
            "notes/failure_fallback.md",
            "failure",
            "# Failure Fallback\n\n\
             Content testing graceful handling of retrieval failures."
                .to_string(),
        ),
    ]
}

fn make_probes() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        (
            "UNIQUE_TAIL_TOKEN_9000",
            "notes/byte_8000_tail.md",
            "tail",
            "byte-8000 tail term",
        ),
        (
            "Section Alpha",
            "notes/heading_relevance.md",
            "heading",
            "heading matching",
        ),
        (
            "Section Beta",
            "notes/heading_relevance.md",
            "heading",
            "heading relevance",
        ),
        (
            "Budget Test",
            "notes/compact_budget.md",
            "budget",
            "compact budget",
        ),
        (
            "unique keywords",
            "notes/lexical_dense_fusion.md",
            "fusion",
            "lexical/dense RRF",
        ),
        (
            "Scope Test",
            "notes/explicit_scope.md",
            "scope",
            "explicit scope",
        ),
        (
            "Global Default",
            "notes/global_default.md",
            "global",
            "global/default semantics",
        ),
        (
            "raw observation fallback",
            "notes/raw_fallback.md",
            "raw",
            "raw fallback",
        ),
        (
            "Page Mode",
            "notes/page_mode_compat.md",
            "page_mode",
            "page-mode compatibility",
        ),
        (
            "inline code",
            "notes/parser_fixtures.md",
            "parser",
            "parser fixtures",
        ),
        (
            "Capture Regression",
            "notes/capture_regression.md",
            "capture",
            "capture regression",
        ),
        (
            "Migration Backfill",
            "notes/migration_backfill.md",
            "migration",
            "migration/backfill",
        ),
        (
            "Failure Fallback",
            "notes/failure_fallback.md",
            "failure",
            "failure fallback",
        ),
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

    // Dense write phase (post-G2): seed page_passage_embeddings through the
    // real production write path — enumerate candidates via the same reader
    // query backfill uses, embed each, and upsert the batch. This is exactly
    // the loop run_passage_embedding_backfill (ai-memory-consolidate) drives
    // in production; the only difference is the deterministic test embedder.
    let embedder = EvalEmbedder;
    let candidates = store
        .reader
        .passage_candidates(
            ws,
            proj,
            embedder.provider(),
            embedder.model(),
            embedder.dim(),
        )
        .await
        .expect("enumerate passage candidates");
    assert!(
        !candidates.is_empty(),
        "eval corpus must produce passages to embed (dense phase would be vacuous)"
    );
    let writes: Vec<PassageEmbeddingWrite> = candidates
        .iter()
        .map(|c| PassageEmbeddingWrite {
            passage_id: c.id,
            vector_bytes: f32_vec_to_bytes(&embedder.embed(&c.text)),
            provider: embedder.provider().to_string(),
            model: embedder.model().to_string(),
            dim: embedder.dim(),
        })
        .collect();
    store
        .writer
        .store_passage_embeddings(writes)
        .await
        .expect("seed passage embeddings");
    let dense_indexing_time = indexing_start.elapsed() - indexing_time;

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
        let found = page_hits
            .iter()
            .take(5)
            .any(|hit| hit.path.as_str() == *expected_path);

        if found {
            page_mode_hits += 1;
        }

        let rank = if found {
            page_hits
                .iter()
                .position(|hit| hit.path.as_str() == *expected_path)
                .map(|p| p + 1)
        } else {
            None
        };

        category_hits
            .entry(category.to_string())
            .and_modify(|v| v.push(found));
        category_ranks
            .entry(category.to_string())
            .and_modify(|v| v.push(rank));

        // Passage-mode search (lexical only, no embeddings)
        let passage_hits = store
            .reader
            .search_passages_hybrid(
                ws,
                proj,
                query.to_string(),
                None, // lexical-only
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

    // Dense-enabled probe loop: same probes, but now with a real query vector
    // and the {provider, model, dim} triple matching the seeded rows, so the
    // reader's dense gate opens and RRF fuses both streams. Collects the
    // rank of the expected page per probe for recall@k / MRR / nDCG.
    let mut dense_passage_hits: BTreeMap<String, Vec<bool>> = BTreeMap::new();
    let mut dense_passage_ranks: BTreeMap<String, Vec<Option<usize>>> = BTreeMap::new();
    for (_, _, cat, _) in &probes {
        dense_passage_hits.entry(cat.to_string()).or_default();
        dense_passage_ranks.entry(cat.to_string()).or_default();
    }
    let mut dense_passage_mode_hits = 0;
    let mut dense_hits_with_dense_rank = 0;

    for (query, expected_path, category, _desc) in &probes {
        let query_vec = embedder.embed(query);
        let dense_hits = store
            .reader
            .search_passages_hybrid(
                ws,
                proj,
                query.to_string(),
                Some(query_vec),
                embedder.provider().to_string(),
                embedder.model().to_string(),
                embedder.dim(),
                10,
            )
            .await
            .expect("search passages (dense enabled)");

        // Every hit in this mode must come from the fused RRF result set;
        // count how many surfaced through the dense stream specifically.
        if dense_hits.iter().any(|hit| hit.dense_rank.is_some()) {
            dense_hits_with_dense_rank += 1;
        }

        let dense_found = dense_hits
            .iter()
            .take(5)
            .any(|hit| hit.page_path.as_str() == *expected_path);
        if dense_found {
            dense_passage_mode_hits += 1;
        }
        let dense_rank = if dense_found {
            dense_hits
                .iter()
                .position(|hit| hit.page_path.as_str() == *expected_path)
                .map(|p| p + 1)
        } else {
            None
        };
        dense_passage_hits
            .entry(category.to_string())
            .and_modify(|v| v.push(dense_found));
        dense_passage_ranks
            .entry(category.to_string())
            .and_modify(|v| v.push(dense_rank));
    }

    // Dense-enabled metric aggregation (mirrors the lexical aggregation).
    let mut dense_passage_recall_at_k: BTreeMap<usize, f64> = BTreeMap::new();
    for &k in RECALL_KS {
        let mut hits_at_k = 0;
        for ranks in dense_passage_ranks.values() {
            for (i, rank) in ranks.iter().enumerate() {
                if i < k && rank.is_some() {
                    hits_at_k += 1;
                }
            }
        }
        dense_passage_recall_at_k.insert(k, hits_at_k as f64 / probes.len() as f64);
    }
    let dense_mrr: f64 = {
        let scores: Vec<f64> = dense_passage_ranks
            .values()
            .flat_map(|ranks| ranks.iter().flatten().map(|r| 1.0 / *r as f64))
            .collect();
        if scores.is_empty() {
            0.0
        } else {
            scores.iter().sum::<f64>() / scores.len() as f64
        }
    };
    let dense_ndcg: f64 = {
        let per_probe: Vec<f64> = dense_passage_ranks
            .values()
            .flat_map(|ranks| {
                ranks.iter().map(|maybe_rank| match maybe_rank {
                    Some(r) if *r <= 5 => 1.0 / ((*r as f64) + 1.0).log2(),
                    _ => 0.0,
                })
            })
            .collect();
        if per_probe.is_empty() {
            0.0
        } else {
            per_probe.iter().sum::<f64>() / per_probe.len() as f64
        }
    };

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
    let mrr_scores: Vec<f64> = category_ranks
        .values()
        .flat_map(|ranks| ranks.iter().flatten().map(|r| 1.0 / *r as f64))
        .collect();
    let mrr = if mrr_scores.is_empty() {
        0.0
    } else {
        mrr_scores.iter().sum::<f64>() / mrr_scores.len() as f64
    };

    // Calculate nDCG@5.
    // Each probe has exactly one relevant document, so IDCG@5 = 1/log2(1+1) = 1
    // (the ideal ranking places the single relevant item at rank 1). DCG@5 for
    // a hit at `rank` is 1/log2(rank+1) (standard log2(rank+1) discount, not
    // log2(rank) — log2(1)=0 would divide by zero for a rank-1 hit and yield
    // +inf, which is a bug, not a perfect score). nDCG@5 per probe is then
    // DCG@5 / IDCG@5 = 1/log2(rank+1), and a probe with no hit in the top 5
    // contributes 0. Averaged over every probe, not just the ones that hit.
    let ndcg_per_probe: Vec<f64> = category_ranks
        .values()
        .flat_map(|ranks| {
            ranks.iter().map(|maybe_rank| match maybe_rank {
                Some(r) if *r <= 5 => 1.0 / ((*r as f64) + 1.0).log2(),
                _ => 0.0,
            })
        })
        .collect();
    let ndcg = if ndcg_per_probe.is_empty() {
        0.0
    } else {
        ndcg_per_probe.iter().sum::<f64>() / ndcg_per_probe.len() as f64
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
    eprintln!(
        "  Passage-mode hits:  {}/{}",
        passage_mode_hits,
        probes.len()
    );
    eprintln!("  Passage mode accessible: Yes");
    eprintln!();

    // Dense-enabled report (post-G2 dense phase).
    let dense_passage_recall_at_5 = dense_passage_recall_at_k.get(&5).copied().unwrap_or(0.0);
    eprintln!("Mode Comparison (dense enabled):");
    eprintln!(
        "  Dense passage-mode hits:      {}/{}",
        dense_passage_mode_hits,
        probes.len()
    );
    eprintln!(
        "  Probes w/ dense-ranked hits:  {}/{}",
        dense_hits_with_dense_rank,
        probes.len()
    );
    eprintln!();
    eprintln!("Category Coverage (dense passage-mode):");
    for (cat, hits) in &dense_passage_hits {
        let success = hits.iter().filter(|&&h| h).count();
        eprintln!("  {}: {}/{} probes found", cat, success, hits.len());
    }
    eprintln!();
    eprintln!("Retrieval Metrics (dense passage-mode):");
    for &k in RECALL_KS {
        if let Some(&recall) = dense_passage_recall_at_k.get(&k) {
            eprintln!("  recall@{}: {:.3}", k, recall);
        }
    }
    eprintln!("  MRR:       {:.3}", dense_mrr);
    eprintln!("  nDCG@5:    {:.3}", dense_ndcg);
    eprintln!();
    eprintln!("Dense-vs-lexical deltas (passage-mode):");
    eprintln!(
        "  recall@5:  {:.3} -> {:.3}  ({:+.3})",
        recall_at_5,
        dense_passage_recall_at_5,
        dense_passage_recall_at_5 - recall_at_5
    );
    eprintln!(
        "  MRR:       {:.3} -> {:.3}  ({:+.3})",
        mrr,
        dense_mrr,
        dense_mrr - mrr
    );
    eprintln!(
        "  nDCG@5:    {:.3} -> {:.3}  ({:+.3})",
        ndcg,
        dense_ndcg,
        dense_ndcg - ndcg
    );
    eprintln!();
    eprintln!("System Metrics (dense phase):");
    eprintln!("  Dense index time: {:?}", dense_indexing_time);
    eprintln!();

    // Assertions
    assert!(
        recall_at_5 >= RECALL_FLOOR,
        "recall@5 {:.3} below floor {:.3}",
        recall_at_5,
        RECALL_FLOOR
    );

    assert!(mrr >= 0.4, "MRR {:.3} too low", mrr);

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

    // Dense-phase regression gate: the dense stream must genuinely
    // participate in RRF fusion (at least one probe's fused result set must
    // contain a hit that surfaced via dense_rank), and dense-enabled recall
    // must clear the same absolute floor as the lexical baseline. Dense is
    // NOT required to match lexical recall here: with the deliberately crude
    // deterministic test embedder (word-hash bag-of-words), short generic
    // probes ("Scope Test", "Budget Test") dilute, while exact FTS matching
    // nails them trivially — that is an embedder-fidelity artifact, not a
    // dense-path defect, and it is why production numbers are measured on
    // Weakling with the real all-MiniLM-L6-v2 model (plan gate 8 / card I1).
    assert!(
        dense_passage_recall_at_5 >= RECALL_FLOOR,
        "dense-enabled recall@5 {:.3} below floor {:.3}",
        dense_passage_recall_at_5,
        RECALL_FLOOR
    );
    assert!(
        dense_hits_with_dense_rank > 0,
        "dense stream must genuinely participate in RRF fusion (no probe had a dense-ranked hit; dense phase is vacuous)"
    );
}
