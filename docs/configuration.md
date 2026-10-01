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

By default, SessionMunch operates in zero-egress local mode using fast in-process FTS5 full-text search, graph traversals, and entity indexing. When embeddings are configured, hybrid Reciprocal Rank Fusion (RRF) combines text, graph, and semantic scores.

```toml
# Options: "none" (default), "openai", "voyage", "google", "openai-compat"
embedding_provider = "none"

# Model name matching your provider (e.g. text-embedding-3-small)
# embedding_model = "text-embedding-3-small"

# Embedding dimension (must match model dimension)
# embedding_dim = 1536

# Base URL for openai-compat (e.g. Ollama, vLLM, LM Studio)
# embedding_base_url = "http://127.0.0.1:11434/v1"
```

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

## Reranking (JEV & LLM)

SessionMunch supports optional post-retrieval reranking to refine hybrid candidate lists.

```toml
# Options: "none" (default), "jev", "llm"
# reranker = "jev"
```

- **JEV Reranker:** Evaluates candidates via fast JEV scoring with zero external data leakage or deterministic fail-open fallback.
- **LLM Reranker:** Calls the configured LLM provider to score snippet relevance. Note that this sends candidate titles and excerpts to the configured LLM provider.

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
