# Privacy, Security & Threat Model

SessionMunch treats memory with the seriousness of credential management. Coding agents frequently touch sensitive repositories, infrastructure keys, and proprietary logic; memory infrastructure must never become an unmonitored backdoor.

---

## 1. Zero Telemetry & Local Isolation

SessionMunch enforces strict local isolation verified by offline unit and regression tests (`tests/suite/zero_telemetry.rs`):
- **Zero Tracking or Telemetry SDKs:** No analytics packages (PostHog, Mixpanel, Datadog, Sentry, Segment, etc.) exist in the workspace dependencies.
- **Local-Only by Default:** In default configuration (FTS5 search and local embeddings/summarization), 100% of memory parsing, indexing, and querying executes locally on the host machine. Zero network requests are initiated.
- **Loopback Binding:** `sessionmunch serve` binds exclusively to `127.0.0.1:49374` by default. Binding to non-loopback interfaces without configuring token authentication fails closed unless explicitly permitted via `--allow-insecure-no-auth`.
- **DNS Rebinding Defense:** The HTTP server validates all incoming `Host` headers against `allowed_hosts` (`localhost`, `127.0.0.1`, `::1` by default).

---

## 2. Privacy Strip & Credential Sanitization

Before observations or markdown files are committed to disk, SessionMunch passes all text through a multi-pass regex sanitizer:
- **Built-in redactions:** Automatically identifies and redacts API keys (`sk-...`, `ghp_...`, `AKIA...`), bearer tokens, private keys (PEM headers), database connection strings containing passwords (`postgres://user:pass@host`), and sensitive filesystem paths (`~/.ssh`, `~/.aws`, `~/.kube`, `~/.gnupg`).
- **Double Opt-in for Assistant Captures:** Assistant turn capture is disabled by default (`capture_assistant = false`). Enabling it requires explicit opt-in on both the server and client hooks.

---

## 3. Remote Provider Egress Disclosure

The **only** outbound connections initiated by SessionMunch occur when you explicitly configure optional remote operations. These operations leave the machine and are **not** local/private:
- Remote embedding calls (`embedding_provider = "openai" | "voyage" | "google" | "openai-compat"`)
- Remote summarization calls (`summarizer_provider = "anthropic" | "openai" | "google" | "openai-compat"`)
- Remote LLM reranking (`SESSIONMUNCH_RERANKER=llm`)
- JEV (Justification/Evaluation/Verification) webhook calls
- API integrations (e.g. Jira/GitHub issue sync hooks)

When a remote provider is configured, payloads travel directly and exclusively over encrypted HTTPS to that provider's configured endpoint. No secondary telemetry or payload mirror exists.

---

## 4. Threat Model & Security Boundaries

- **Single-User Trust Domain:** Default installations assume a single trusted machine operator. Database users provide attribution and auditing, not adversarial multi-tenant access control lists (ACLs).
- **Prompt Injection Defense in Depth:** Injected memory handoffs and project briefs are explicitly marked as untrusted historical data in downstream context injection schemas.
- **Audit Logging:** Every administrative mutation, page write, and purge operation is durably logged with attribution.
