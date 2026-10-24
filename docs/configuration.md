# SessionMunch Configuration Reference

SessionMunch is configured via a TOML file (defaulting to `<data_dir>/config.toml`) or via environment variables prefixed with `SESSIONMUNCH_`.

All settings have sensible, privacy-conscious defaults designed for local-only, single-user operation.

---

## Configuration File Hierarchy

1. CLI flags (`--config <path>`, `--data-dir <path>`, `--bind <ip:port>`)
2. Environment variables (`SESSIONMUNCH_*`)
3. Configuration file (`<data_dir>/config.toml`)
4. Built-in defaults

---

## Network & Security

```toml
# HTTP bind address used by `sessionmunch serve`.
# Default: loopback only (127.0.0.1:49374).
bind = "127.0.0.1:49374"

# Host-header allowlist for DNS-rebinding defense.
# When binding to a LAN/remote address, add your hostnames/IPs here.
allowed_hosts = ["localhost", "127.0.0.1", "::1"]

# Tracing log level: error, warn, info, debug, trace.
log_level = "info"

# Strip root combinators (anyOf/oneOf/allOf) from MCP tool input schemas
# to maintain compatibility with strict model endpoints.
strip_root_combinators = false

# Collapse optional fields to single type + nullable for Google Vertex/Gemini compatibility.
gemini_safe_schemas = false
```

Environment variable overrides:
- `SESSIONMUNCH_BIND=0.0.0.0:49374`
- `SESSIONMUNCH_ALLOWED_HOSTS=localhost,127.0.0.1,homelab.local`
- `SESSIONMUNCH_LOG_LEVEL=info`
- `SESSIONMUNCH_STRIP_ROOT_COMBINATORS=true`
- `SESSIONMUNCH_GEMINI_SAFE_SCHEMAS=true`

---
## Embeddings & Vector Search

By default, SessionMunch uses **best-effort local embeddings** with Nomic Embed Text v1.5 in-process. If the local model cannot be fetched or loaded, the server degrades to the non-vector retrieval path rather than refusing to start. FTS5, entities, graph traversal, and passage retrieval remain available.

Set the provider explicitly when you want different behavior:

```toml
# Unset = best-effort local Nomic embeddings (default)
# embedding_provider = "local"   # require local embeddings; load failure is an error
# embedding_provider = "none"    # disable vector inference entirely
# embedding_provider = "openai"
# embedding_provider = "voyage"
# embedding_provider = "google"
# embedding_provider = "openai-compat"

# Model name matching your provider (hosted/self-hosted examples)
# embedding_model = "text-embedding-3-small"

# Embedding dimension (must match the configured model)
# embedding_dim = 1536

# Base URL for openai-compat (e.g. Ollama, vLLM, LM Studio)
# embedding_base_url = "http://127.0.0.1:11434/v1"
```

When vectors are active, Reciprocal Rank Fusion (RRF) combines semantic results with the lexical/entity/graph retrieval streams. Setting `embedding_provider = "none"` gives you a zero-inference retrieval path; it does **not** disable SessionMunch.

For hardware-specific recommendations and copy-paste profiles, consult [`docs/models.md`](models.md).

---

## Summarization & LLM Consolidation

SessionMunch compiles session observations into markdown wiki pages. Without an LLM, it uses deterministic rule-based compaction. Adding an LLM provider enables conversational distillation, gotcha/concept synthesis, and automated wiki maintenance.

```toml
# Options: "none" (default), "anthropic", "anthropic-oauth", "openai", "openai-oauth", "copilot", "google", "openai-compat"
summarizer_provider = "none"
# summarizer_model = "claude-haiku-4-5"
# summarizer_temperature = 0.3
# summarizer_max_output_tokens = 512

[consolidation]
max_input_tokens = 100000
max_output_tokens = 32000
```

---

## Reranking

Reranking happens **after** SessionMunch has already retrieved and fused a bounded candidate set. It can improve the order of those results, but it never owns storage or baseline recall.

### v0.1: optional LLM reranking

```toml
# Off by default. Current supported value: "llm".
# reranker = "llm"
```

With `reranker = "llm"`, SessionMunch sends the bounded query plus candidate titles/snippets to the configured LLM provider for one final relevance pass. Any provider error, timeout, malformed response, or concurrency saturation preserves the normal pre-rerank order.

### Planned: JEV reranking

The codebase already contains the generic reranker boundary and JEV-oriented fail-open benchmark scaffolding, but **`reranker = "jev"` is not a valid v0.1 configuration option**. JEV is planned as an optional post-retrieval reranker in a subsequent release. The intended contract is the same: SessionMunch performs storage and recall; JEV judges a small shortlist; failure leaves baseline ranking untouched.

---

## Privacy, Sanitization & Retention

```toml
[sanitize]
# Additional regex patterns to redact before storage or export
extra_patterns = []

# Substrings that must never be redacted
allowlist = []

[decay]
# Exponential decay parameters for episodic memory pages
lambda = 0.02
sigma = 0.6
mu = 0.04
salience_default = 1.0
cold_threshold = 0.20
hard_delete_after_days = 180

# Raw observation retention (0 = preserve all observations indefinitely)
observation_retention_days = 0
observation_prune_batch = 5000
```

---

## Scoping & Project Markers (`.sessionmunch.toml`)

In addition to global server configuration, directory trees can declare workspace and project boundaries via `.sessionmunch.toml` marker files placed in project roots:

```toml
workspace = "work"
project = "api-backend"
project_strategy = "repo-root"

[capture]
ignore_paths = ["secret/", "*.key"]
```

See [`docs/marker-file.md`](marker-file.md) for marker inheritance and syntax.
