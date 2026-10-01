# Model Guide: Local vs. API Embeddings & Summarization

SessionMunch uses models for two distinct operations: **Embeddings** (hybrid search vector representations) and **Summarization / Consolidation** (distilling raw session logs into durable concepts and gotchas).

SessionMunch is **model-agnostic** and runs fully functional in **zero-LLM mode** without any external API keys.

---

## Local vs. API: Choosing Your Strategy

| Feature | Zero-LLM / Local | Hosted API (Anthropic / OpenAI / Google) |
| :--- | :--- | :--- |
| **Privacy & Data Egress** | Zero bytes leave your host | Payloads sent to provider over HTTPS |
| **Cost** | Completely free ($0) | Pennies per day (using lightweight models) |
| **Setup Complexity** | Zero setup (built-in FTS5 + local embedder) | Requires API key or subscription token |
| **Search Retrieval** | High accuracy (FTS5 + Graph + Local Vector) | Enhanced semantic matching (Voyage/OpenAI) |
| **Consolidation Quality**| Clean rule-based compaction | Rich, synthesized architectural summaries |

---

## Hardware Tiers & Recommendations

For full copy-paste configuration snippets across all hardware tiers, consult the comprehensive guide at [`docs/models.md`](models.md).

### 1. Zero-LLM & Local Workstation
- **Embeddings:** `none` (FTS5 + entities) or `local` (in-process Nomic Embed v1.5 / MiniLM).
- **Summarization:** `none` (deterministic rule-based session summaries).
- **Ideal for:** Private air-gapped systems, single laptops, and developers who prefer deterministic operation.

### 2. Lightweight Cloud Hosted
- **Embeddings:** `openai` (`text-embedding-3-small`) or `voyage` (`voyage-3-lite`).
- **Summarization:** `anthropic` (`claude-haiku-4-5`) or `openai` (`gpt-5.4-mini`).
- **Ideal for:** Best balance of high-quality synthesis, low latency, and negligible API cost.

### 3. Subscription & OAuth Backends
- Support for `anthropic-oauth` (`claude setup-token`), `openai-oauth` (`sessionmunch auth login openai-oauth`), and GitHub `copilot` allows leveraging existing consumer subscriptions without pay-per-token metering.

---

## Reranking: JEV vs. LLM Reranker

Post-retrieval reranking scores candidate pages to optimize hybrid rank fusion:
- **JEV Reranker:** Evaluates candidate quality locally using fast, deterministic JEV heuristic scoring. Completely fail-open; zero external calls.
- **LLM Reranker:** Calls the configured LLM to score snippet relevance. Sends titles and candidate snippets to the provider.
