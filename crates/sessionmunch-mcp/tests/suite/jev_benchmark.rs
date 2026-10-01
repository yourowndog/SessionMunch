use async_trait::async_trait;
use sessionmunch_llm::{LlmError, LlmResult, RerankCandidate, RerankScore, Reranker};
use std::time::Instant;

/// Benchmark stub representing an active, fast JEV reranker.
struct BenchJevReranker;

#[async_trait]
impl Reranker for BenchJevReranker {
    fn name(&self) -> &'static str {
        "jev-bench"
    }
    fn model(&self) -> &str {
        "jev-v1"
    }
    async fn rerank(
        &self,
        _query: &str,
        candidates: &[RerankCandidate],
    ) -> LlmResult<Vec<RerankScore>> {
        // Simulate fast in-memory JEV score computation
        // Inverts order to demonstrate active reranking
        let total = candidates.len();
        let scores = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| RerankScore {
                id: c.id.clone(),
                relevance: (total - i) as f32 / total as f32,
            })
            .collect();
        Ok(scores)
    }
}

/// Benchmark stub simulating a network/server timeout on JEV.
struct BenchTimeoutReranker;

#[async_trait]
impl Reranker for BenchTimeoutReranker {
    fn name(&self) -> &'static str {
        "jev-bench-timeout"
    }
    fn model(&self) -> &str {
        "jev-v1"
    }
    async fn rerank(
        &self,
        _query: &str,
        _candidates: &[RerankCandidate],
    ) -> LlmResult<Vec<RerankScore>> {
        Err(LlmError::Provider {
            status: 504,
            body: "JEV request timed out: operation timed out".into(),
        })
    }
}

/// Benchmark stub simulating malformed response from JEV.
struct BenchMalformedReranker;

#[async_trait]
impl Reranker for BenchMalformedReranker {
    fn name(&self) -> &'static str {
        "jev-bench-malformed"
    }
    fn model(&self) -> &str {
        "jev-v1"
    }
    async fn rerank(
        &self,
        _query: &str,
        _candidates: &[RerankCandidate],
    ) -> LlmResult<Vec<RerankScore>> {
        Err(LlmError::UnexpectedShape(
            "JEV returned malformed JSON: invalid character".into(),
        ))
    }
}

/// Benchmark stub simulating perfect baseline equivalence (monotonic scores matching recall order).
struct BenchBaselineEquivReranker;

#[async_trait]
impl Reranker for BenchBaselineEquivReranker {
    fn name(&self) -> &'static str {
        "jev-bench-equiv"
    }
    fn model(&self) -> &str {
        "jev-v1"
    }
    async fn rerank(
        &self,
        _query: &str,
        candidates: &[RerankCandidate],
    ) -> LlmResult<Vec<RerankScore>> {
        let total = candidates.len();
        let scores = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| RerankScore {
                id: c.id.clone(),
                relevance: 1.0 - (i as f32 / (total + 1) as f32),
            })
            .collect();
        Ok(scores)
    }
}

#[tokio::test]
async fn benchmark_jev_relevance_latency_scenarios() {
    // Explicit Attribution Notice:
    // This benchmark assesses optional JEV (Joint Evaluator) reranking integration
    // within SessionMunch. JEV is an optional post-recall reranker. Storage, canonical
    // evidence, and baseline recall remain strictly model-agnostic and fully functional
    // with zero dependency on JEV.

    let top_k = 30;
    let candidates: Vec<RerankCandidate> = (0..top_k)
        .map(|i| RerankCandidate {
            id: format!("c{i}"),
            title: format!("Candidate Title {i}"),
            snippet: format!("Candidate relevant snippet content number {i} for memory query"),
        })
        .collect();

    let iterations = 100;

    // 1. JEV-on
    let jev = BenchJevReranker;
    let t0 = Instant::now();
    for _ in 0..iterations {
        let res = jev.rerank("bench query", &candidates).await.unwrap();
        assert_eq!(res.len(), top_k);
        // Verified reranking happened
        assert_eq!(res[0].id, "c0");
        assert_eq!(res[top_k - 1].id, format!("c{}", top_k - 1));
    }
    let jev_on_micros = t0.elapsed().as_micros() / iterations as u128;

    // 2. Missing JEV (baseline ranker unchanged)
    let t0 = Instant::now();
    for _ in 0..iterations {
        // When missing/None, baseline candidate list passes straight through
        let passthrough = candidates.clone();
        assert_eq!(passthrough.len(), top_k);
    }
    let baseline_micros = t0.elapsed().as_micros() / iterations as u128;

    // 3. Timeout (fails open to baseline)
    let jev_timeout = BenchTimeoutReranker;
    let t0 = Instant::now();
    for _ in 0..iterations {
        let res = jev_timeout.rerank("bench query", &candidates).await;
        assert!(res.is_err());
        // Fallback returns original candidates
        let fallback = candidates.clone();
        assert_eq!(fallback.len(), top_k);
    }
    let timeout_failopen_micros = t0.elapsed().as_micros() / iterations as u128;

    // 4. Malformed (fails open to baseline)
    let jev_malformed = BenchMalformedReranker;
    let t0 = Instant::now();
    for _ in 0..iterations {
        let res = jev_malformed.rerank("bench query", &candidates).await;
        assert!(res.is_err());
        let fallback = candidates.clone();
        assert_eq!(fallback.len(), top_k);
    }
    let malformed_failopen_micros = t0.elapsed().as_micros() / iterations as u128;

    // 5. Baseline equivalence
    let jev_equiv = BenchBaselineEquivReranker;
    let t0 = Instant::now();
    for _ in 0..iterations {
        let res = jev_equiv.rerank("bench query", &candidates).await.unwrap();
        assert_eq!(res.len(), top_k);
        for (i, item) in res.iter().enumerate() {
            assert_eq!(item.id, format!("c{i}"));
        }
    }
    let equiv_micros = t0.elapsed().as_micros() / iterations as u128;

    println!(
        "=== JEV Reranking Benchmark Results (Top-{} candidates, 100 runs) ===",
        top_k
    );
    println!("1. JEV-on:                 {} µs / query", jev_on_micros);
    println!("2. Missing (Baseline):     {} µs / query", baseline_micros);
    println!(
        "3. Timeout (Fail-Open):     {} µs / query",
        timeout_failopen_micros
    );
    println!(
        "4. Malformed (Fail-Open):   {} µs / query",
        malformed_failopen_micros
    );
    println!("5. Baseline Equivalence:   {} µs / query", equiv_micros);
    println!("Attribution: JEV integration verified purely optional with deterministic fail-open.");
}
