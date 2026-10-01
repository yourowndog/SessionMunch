# Opinionated Model Guide: Embedding and Summarization

SessionMunch uses two distinct model workloads:

1. **Embedding**: Powers semantic/vector hybrid search alongside FTS5 lexical search and graph traversal. Evaluated against retrieval benchmarks (e.g. LongMemEval-S hit@5).
2. **Summarization**: Generates concise session summaries, handoffs, and distilled wiki concept/rule pages at session close or consolidation passes.

Model selection should not be a research project. This guide recommends exactly **one verified configuration per hardware tier**, balanced for reliability, memory limits, and proven retrieval performance.

---

## Default Embedding Baseline: Nomic Embed Text v1.5 (768d)

**Default Local Embedding Model:** `nomic-ai/nomic-embed-text-v1.5` (768 dimensions).

- **Citation & Evidence:** Nomic Embed Text v1.5 is SessionMunch's pinned default local embedding model across all local tiers. Headroom and retrieval evaluations (see `evidence/implementation/embedding-upgrade-assessment.md` and `docs/benchmarks/longmemeval-s-2026-09-01-local.md`) demonstrate that Nomic v1.5 / 768d provides high-quality multi-session retrieval, 2048-token input capacity, and matryoshka dimensionality support while avoiding the high latency of 1024-dim large models on CPU.
- **Zero-Egress In-Process Execution:** Under the `local` provider, SessionMunch executes Nomic Embed Text in-process via pure-Rust `candle` inference using checksum-verified quantized weights (~137 MB). No external sidecar, Python environment, or GPU daemon is required.

---

## Hardware Tier Recommendations

### Quick Reference

| Tier | Hardware Profile | Recommended Embedder | Recommended Summarizer | Memory Budget | License Notes |
|---|---|---|---|---|---|
| **1. CPU-Only Small** | Dual-core / 4-8 GB RAM | `local` (Nomic Embed Text v1.5, 768d) | `none` (deterministic rule-based) | ~200-500 MB RAM | Apache-2.0 |
| **2. Stronger CPU** | 8+ Cores / 16-32 GB RAM | `local` (Nomic Embed Text v1.5, 768d) | `openai-compat` (Qwen2.5-3B-Instruct Q4_K_M) | ~2.5-3.5 GB RAM | Apache-2.0 |
| **3. ~6-8 GB VRAM** | Entry GPU (e.g. RTX 3060/4060, Apple M-series 8-16 GB) | `local` (Nomic Embed Text v1.5, 768d) | `openai-compat` (Qwen2.5-7B-Instruct Q4_K_M) | ~5-6 GB VRAM | Apache-2.0 |
| **4. ~12-16 GB VRAM** | Mid-range GPU (e.g. RTX 4070/4080, Apple M-series 18-24 GB) | `local` (Nomic Embed Text v1.5, 768d) | `openai-compat` (Qwen2.5-14B-Instruct Q4_K_M) | ~10-12 GB VRAM | Apache-2.0 |
| **5. 24 GB+ VRAM** | High-end GPU / Workstation (RTX 3090/4090, Apple Mac Studio) | `local` (Nomic Embed Text v1.5, 768d) | `openai-compat` (Qwen2.5-32B-Instruct Q4_K_M) | ~20-22 GB VRAM | Apache-2.0 |
| **6. API / Hosted** | No local compute or cloud-preferred | `google` (`gemini-embedding-001`, 768d) or `openai` (`text-embedding-3-small`, 1536d) | `openai` (`gpt-4o-mini`) or `google` (`gemini-1.5-flash`) | Zero local RAM/VRAM | Commercial API terms |

---

### Tier 1: CPU-Only Small Machine (4–8 GB System RAM, SBC / VPS / Thin Laptop)

- **Embedding Model:** `local` (`nomic-embed-text-v1.5`, 768d, in-process via candle)
- **Summarizer:** `none` (Deterministic rule-based summary and handoff generation)
- **Estimated Footprint:** ~180–250 MiB RSS peak; ~137 MB disk footprint in `<data_dir>/models/`
- **Speed vs. Quality Tradeoff:** Zero summarization latency overhead. Full-text search (FTS5) and dense semantic vector retrieval run completely in-process. Session close produces structured wiki pages from captured observations without any LLM compute delay.
- **Context & Quantization:** int8 quantized weights downloaded and verified at startup; 512 token attention window for embeddings.
- **Licensing & Commercial Use:** **Apache-2.0** (Nomic Embed Text v1.5). Free for personal and commercial deployment.
- **Copy-Paste SessionMunch Config (`config.toml`):**

```toml
# Tier 1: CPU-Only Small Machine
bind = "127.0.0.1:49374"

embedding_provider = "local"
embedding_model = "nomic-embed-text-v1.5"
embedding_dim = 768

summarizer_provider = "none"
```

---

### Tier 2: Stronger CPU Workstation (8+ Cores, 16–32 GB System RAM, No GPU)

- **Embedding Model:** `local` (`nomic-embed-text-v1.5`, 768d, in-process via candle)
- **Summarizer:** `openai-compat` with `qwen2.5:3b-instruct-q4_K_M` (served via Ollama or llama.cpp on loopback)
- **Estimated Footprint:** ~2.5–3.5 GB system RAM total (Ollama ~2 GB for model weights + KV cache, SessionMunch ~250 MB)
- **Speed vs. Quality Tradeoff:** 25–40 tokens/sec summarization on modern CPU. Qwen 2.5 3B produces clean, structured JSON summaries and adheres tightly to consolidation prompts with minimal memory footprint.
- **Context & Quantization:** 4-bit quantization (Q4_K_M). Set consolidation context ceiling to 8,000–16,000 tokens to preserve CPU memory.
- **Licensing & Commercial Use:** **Apache-2.0** for both Nomic Embed Text v1.5 and Qwen2.5-3B-Instruct. Unrestricted commercial use.
- **Copy-Paste SessionMunch Config (`config.toml`):**

```toml
# Tier 2: Stronger CPU Workstation
bind = "127.0.0.1:49374"

embedding_provider = "local"
embedding_model = "nomic-embed-text-v1.5"
embedding_dim = 768

summarizer_provider = "openai-compat"
summarizer_model = "qwen2.5:3b-instruct-q4_K_M"
summarizer_base_url = "http://127.0.0.1:11434/v1"
summarizer_temperature = 0.2
summarizer_max_output_tokens = 512

[consolidation]
max_input_tokens = 16000
max_output_tokens = 4000
```

---

### Tier 3: ~6–8 GB VRAM (Entry Dedicated GPU or Apple Silicon 8–16 GB)

- **Embedding Model:** `local` (`nomic-embed-text-v1.5`, 768d, in-process via candle)
- **Summarizer:** `openai-compat` with `qwen2.5:7b-instruct-q4_K_M` (served via Ollama/vLLM/LM Studio)
- **Estimated Footprint:** ~5.2–6.0 GB VRAM allocated; host RAM overhead < 500 MB
- **Speed vs. Quality Tradeoff:** 45–70 tokens/sec. 7B provides noticeably sharper instruction following for complex session consolidation, topic taxonomy clustering, and edge extraction.
- **Context & Quantization:** Q4_K_M quantization; 16,000–32,000 token context window fits cleanly within 6-8 GB VRAM.
- **Licensing & Commercial Use:** **Apache-2.0** for Nomic Embed Text v1.5 and Qwen2.5-7B-Instruct. Permissive commercial use.
- **Copy-Paste SessionMunch Config (`config.toml`):**

```toml
# Tier 3: ~6-8 GB VRAM
bind = "127.0.0.1:49374"

embedding_provider = "local"
embedding_model = "nomic-embed-text-v1.5"
embedding_dim = 768

summarizer_provider = "openai-compat"
summarizer_model = "qwen2.5:7b-instruct-q4_K_M"
summarizer_base_url = "http://127.0.0.1:11434/v1"
summarizer_temperature = 0.2
summarizer_max_output_tokens = 768

[consolidation]
max_input_tokens = 32000
max_output_tokens = 8000
```

---

### Tier 4: ~12–16 GB VRAM (Mid-Range Dedicated GPU or Apple Silicon 18–32 GB)

- **Embedding Model:** `local` (`nomic-embed-text-v1.5`, 768d, in-process via candle)
- **Summarizer:** `openai-compat` with `qwen2.5:14b-instruct-q4_K_M` (served via Ollama/vLLM)
- **Estimated Footprint:** ~10.5–12.5 GB VRAM allocated; host RAM < 1 GB
- **Speed vs. Quality Tradeoff:** 35–55 tokens/sec. Qwen 2.5 14B approaches GPT-4o-mini synthesis capability; handles complex multi-turn coding sessions, long diffs, and deep architectural decisions without hallucinating dependencies.
- **Context & Quantization:** Q4_K_M quantization; supports 32k context comfortably.
- **Licensing & Commercial Use:** **Apache-2.0** for Nomic Embed Text v1.5 and Qwen2.5-14B-Instruct. Permissive commercial use.
- **Copy-Paste SessionMunch Config (`config.toml`):**

```toml
# Tier 4: ~12-16 GB VRAM
bind = "127.0.0.1:49374"

embedding_provider = "local"
embedding_model = "nomic-embed-text-v1.5"
embedding_dim = 768

summarizer_provider = "openai-compat"
summarizer_model = "qwen2.5:14b-instruct-q4_K_M"
summarizer_base_url = "http://127.0.0.1:11434/v1"
summarizer_temperature = 0.2
summarizer_max_output_tokens = 1024

[consolidation]
max_input_tokens = 48000
max_output_tokens = 12000
```

---

### Tier 5: 24 GB+ VRAM (Workstation GPU: RTX 3090/4090 or Apple Mac Studio 36 GB+)

- **Embedding Model:** `local` (`nomic-embed-text-v1.5`, 768d, in-process via candle)
- **Summarizer:** `openai-compat` with `qwen2.5:32b-instruct-q4_K_M` (served via Ollama/vLLM)
- **Estimated Footprint:** ~20–22 GB VRAM allocated
- **Speed vs. Quality Tradeoff:** 25–40 tokens/sec. High reasoning fidelity, frontier-level instruction following for structured wiki extraction, and near-zero failure rate on schema constraints.
- **Context & Quantization:** Q4_K_M or Q5_K_M quantization; supports 64k+ context window for large batch consolidations.
- **Licensing & Commercial Use:** **Apache-2.0** for Nomic Embed Text v1.5 and Qwen2.5-32B-Instruct. Permissive commercial use.
- **Copy-Paste SessionMunch Config (`config.toml`):**

```toml
# Tier 5: 24 GB+ VRAM
bind = "127.0.0.1:49374"

embedding_provider = "local"
embedding_model = "nomic-embed-text-v1.5"
embedding_dim = 768

summarizer_provider = "openai-compat"
summarizer_model = "qwen2.5:32b-instruct-q4_K_M"
summarizer_base_url = "http://127.0.0.1:11434/v1"
summarizer_temperature = 0.2
summarizer_max_output_tokens = 1024

[consolidation]
max_input_tokens = 64000
max_output_tokens = 16000
```

---

### Tier 6: API / No-Local-Compute (Cloud Hosted)

Use this tier when local GPU or CPU compute is unavailable or when you prefer delegating model execution to managed cloud providers through SessionMunch's provider-neutral interface. No vendor-specific code exists in SessionMunch core; all API integrations use standard endpoints configured via environment variables or `config.toml`.

#### Option A: OpenAI (Platform API)
- **Embedding Model:** `text-embedding-3-small` (1536d)
- **Summarizer:** `gpt-4o-mini`
- **Estimated Footprint:** 0 MB local RAM/VRAM; minimal network bandwidth
- **Speed vs. Quality Tradeoff:** Sub-second remote API latency, high quality, minimal cost (< $0.01 per day of heavy usage).
- **Credentials:** Set `OPENAI_API_KEY` in environment.
- **Licensing & Terms:** Subject to OpenAI Commercial Terms of Service. Data processed according to OpenAI API data privacy policies (no training on API data).
- **Copy-Paste SessionMunch Config (`config.toml`):**

```toml
# Tier 6A: OpenAI Hosted
bind = "127.0.0.1:49374"

embedding_provider = "openai"
embedding_model = "text-embedding-3-small"
embedding_dim = 1536

summarizer_provider = "openai"
summarizer_model = "gpt-4o-mini"
summarizer_temperature = 0.2
summarizer_max_output_tokens = 512
```

#### Option B: Google Gemini
- **Embedding Model:** `gemini-embedding-001` (768d)
- **Summarizer:** `gemini-1.5-flash`
- **Estimated Footprint:** 0 MB local RAM/VRAM
- **Speed vs. Quality Tradeoff:** Fast response times, generous free/pay-as-you-go tiers, native 768d embedding dimension.
- **Credentials:** Set `GEMINI_API_KEY` or `GOOGLE_API_KEY` in environment.
- **Licensing & Terms:** Subject to Google Cloud / Google AI Terms of Service.
- **Copy-Paste SessionMunch Config (`config.toml`):**

```toml
# Tier 6B: Google Gemini Hosted
bind = "127.0.0.1:49374"

embedding_provider = "google"
embedding_model = "gemini-embedding-001"
embedding_dim = 768

summarizer_provider = "google"
summarizer_model = "gemini-1.5-flash"
summarizer_temperature = 0.2
summarizer_max_output_tokens = 512
```

---

## Verification and Operational Notes

1. **Zero-Telemetry Assurance:** SessionMunch core emits no analytics or telemetry. When running `local` embeddings and `local` (or `none`) summarization, zero tokens or vectors leave your host.
2. **Index Rebuilding upon Dimension Change:** Switching between 768d (Nomic, Gemini) and 1536d (OpenAI) requires re-indexing stored pages because SQLite vector columns are dimension-pinned. Run `sessionmunch embed --force` whenever altering `embedding_dim`.
3. **Structured Outputs:** For all `openai-compat` summarizers, ensure your local inference engine (Ollama ≥ 0.3.0, vLLM, llama.cpp) supports JSON Schema responses (`response_format: { type: "json_object" }`).
