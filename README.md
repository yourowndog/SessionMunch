<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.png">
    <img alt="SessionMunch" src="docs/logo-light.png" width="460">
  </picture>
</p>

<p align="center">
  <strong>Persistent memory for coding agents that survives the session, the model, and the machine.</strong><br>
  Local-first • Plain Markdown source of truth • Hybrid retrieval • MCP + HTTP • Single Rust binary
</p>

<p align="center">
  <a href="#what-is-sessionmunch">What it is</a> •
  <a href="#quick-start">Quick start</a> •
  <a href="#how-it-works">How it works</a> •
  <a href="#model-guide">Model guide</a> •
  <a href="#supported-agent-harnesses">Integrations</a> •
  <a href="#privacy-security-and-data-ownership">Privacy</a> •
  <a href="#project-lineage">Lineage</a> •
  <a href="#documentation-suite">Docs</a>
</p>

---

## What is SessionMunch?

Coding agents are very good at working memory and surprisingly bad at filing cabinets.

A useful session accumulates things worth keeping: architectural decisions, failed approaches, project conventions, debugging discoveries, handoffs, and the exact incantation that fixed the weird thing. Then the context window fills, the session ends, or you switch from Claude Code to Codex to Hermes and suddenly everyone is meeting the codebase for the first time again.

**SessionMunch is a local-first persistent memory runtime for AI coding agents.** It turns transient agent sessions into a durable, searchable project knowledge base that later sessions can query without hauling the entire transcript back into context.

- **Plain Markdown source of truth:** Durable knowledge lives in ordinary `.md` files. Inspect it, grep it, edit it, back it up, or put it under Git.
- **Rebuildable SQLite acceleration:** FTS5, passages, entities, graph relationships, and vector indexes make retrieval fast. If the derived database disappears, `sessionmunch reindex` rebuilds it from Markdown.
- **Hybrid retrieval:** Lexical search, local vectors, graph/entity signals, passages, and Reciprocal Rank Fusion can cooperate instead of asking one embedding model to become a religion.
- **Local embeddings by default:** Nomic Embed Text v1.5 runs in-process with no API key required. Vector search can also be disabled entirely.
- **Optional LLM consolidation:** Use no summarizer, a local OpenAI-compatible model, or a hosted provider to distill raw session evidence into cleaner concepts, gotchas, rules, and summaries.
- **Agent-neutral interfaces:** MCP, HTTP, lifecycle hooks, managed workstreams, and a native Hermes MemoryProvider let one memory service outlive whichever agent is fashionable this quarter.
- **Privacy-conscious defaults:** Loopback-only networking, secret sanitization, zero product telemetry, and no summarizer enabled unless you choose one.

The durable knowledge is yours. The database is an acceleration layer, not a hostage situation.

---

## Status

SessionMunch is currently a **v0.1.0 release candidate**.

The release candidate has passed the project ship audit, including a clean-machine build, workspace tests, packaging checks, live MCP/HTTP calls, legacy-import verification, privacy checks, and documentation review.

The source tree is usable now. The tagged GitHub Release and its prebuilt artifacts are the next release step, so this README deliberately does **not** claim a package exists before it has actually been published. Revolutionary stuff.

---

## Why SessionMunch?

Most useful agent state is neither source code nor chat history. It is the layer in between: *why we did this, what failed, what must stay true, what to try next*.

SessionMunch treats raw chat and tool events as evidence, then keeps durable project knowledge separately:

> **Chat history is evidence. Durable project knowledge is the product.**

That leads to a deliberately Unix-ish design: ordinary files own the truth; databases and models are replaceable machinery; agents get the smallest useful context instead of a transcript landfill.

| Feature | SessionMunch | Vendor Memory APIs | Raw Chat History Files |
| :--- | :--- | :--- | :--- |
| **Storage Format** | Plain Git-versioned Markdown | Proprietary Cloud DB | Massive unstructured JSON |
| **Tool Freedom** | Any MCP-compatible agent | Vendor-locked | CLI-specific |
| **Egress & Telemetry** | Zero egress in default mode | Full conversation egress | Local, but unindexed |
| **Search Model** | Hybrid RRF (FTS5 + Graph + Vector) | Pure dense vector | Grep or unindexed |
| **Cross-Agent Handoff** | Atomic baton passes | None | Manual copy-pasting |

---

## How It Works

```text
[Agent Session] ──▶ [Lifecycle Hooks] ──▶ [Sanitization] ──▶ [Raw Spool]
       │                                                         │
       │                                                         ▼
       ▼                                                 [Consolidation]
[Agent Resume]  ◀── [Context Brief] ◀── [Derived SQLite] ◀── [Markdown Wiki]
```

1. **Capture:** Shell and agent hooks passively record prompt and tool interactions without latency.
2. **Sanitize:** Sensitive credentials, private keys, and environment tokens are automatically redacted.
3. **Consolidate:** At session exit, observations are distilled into concise markdown pages (architectural decisions, gotchas, concepts).
4. **Retrieve:** Agents query memory through standard MCP tools or REST endpoints using hybrid Reciprocal Rank Fusion.

---

## Quick Start

Get running on loopback in under two minutes.

### 1. Installation

Until the first tagged release artifacts are published, the honest install path is a source build:

```bash
git clone https://github.com/yourowndog/SessionMunch.git
cd SessionMunch
cargo build --release --workspace

install -Dm755 target/release/sessionmunch ~/.local/bin/sessionmunch
```

Initialize and start it:

```bash
sessionmunch init
sessionmunch serve --transport http --bind 127.0.0.1:49374
```

The release machinery for native archives, Docker, systemd, and AUR packaging is already in the repo. Those become distribution channels when `v0.1.0` is actually tagged and published; they are not being advertised here as imaginary packages from the future.

*By default, SessionMunch binds strictly to `127.0.0.1:49374`, safe for local single-user operation.*

### 2. Connect Your Agent

Register SessionMunch with your preferred harness in two commands:

```bash
# Claude Code
sessionmunch install-mcp --client claude-code --apply
sessionmunch install-hooks --agent claude-code --apply

# OpenAI Codex
sessionmunch install-mcp --client codex --apply
sessionmunch install-hooks --agent codex --apply

# OpenCode
sessionmunch install-mcp --client opencode --apply
sessionmunch install-hooks --agent opencode --apply
```

For Hermes Agent, configure the native MemoryProvider plugin in your profile (see [`docs/hermes.md`](docs/hermes.md)).

### 3. Experience Cross-Session Continuity

1. Work in any project repository as normal.
2. Exit your session — SessionMunch automatically processes and indexes the session.
3. Start another agent in the same project directory — startup hooks immediately supply the active context brief.
4. Query memory explicitly during work: *"What gotchas did we hit with auth cookies?"* (invokes `memory_query`).

---

## Model Guide

SessionMunch uses models for **two separate jobs**:

1. **Embeddings** help retrieval find semantically related material.
2. **Summarization** turns session evidence into cleaner durable knowledge.

Keeping those jobs separate keeps the system cheaper, easier to reason about, and less dependent on one vendor.

### The default

For retrieval, SessionMunch defaults to **Nomic Embed Text v1.5 (768d)** running locally in-process. No API key is required.

For summarization, the default is **none**. The memory service still works: lexical search, graph/entity signals, passages, local embeddings, handoffs, and persistent Markdown do not require a remote LLM.

### Opinionated starting points

| Goal | Embeddings | Summarizer |
| :--- | :--- | :--- |
| **Start here / private** | Local Nomic v1.5 | None |
| **Best practical local setup** | Local Nomic v1.5 | Local OpenAI-compatible instruct model |
| **Low-hassle hosted setup** | Local Nomic v1.5 | Small/fast hosted model |
| **Minimum footprint** | None | None |
| **Cloud-managed** | OpenAI / Google / Voyage-compatible | Supported hosted provider |

The useful rule of thumb is: **keep retrieval local unless you have a reason not to; spend the expensive model on synthesis, not on remembering where the wrench is.**

For local summarization, use the smallest instruct model that reliably follows the consolidation format on your hardware. The full guide includes copy-paste profiles from CPU-only machines through 24 GB+ GPUs: [`docs/models.md`](docs/models.md).

Provider paths include local embeddings, OpenAI, Google/Gemini, Voyage embeddings, OpenAI-compatible endpoints, and supported OAuth/provider integrations for LLM work. See [`docs/models-summary.md`](docs/models-summary.md) and [`docs/configuration.md`](docs/configuration.md).

---

## Supported Agent Harnesses

SessionMunch supports any client speaking the **Model Context Protocol (MCP)** or running lifecycle hooks:

| Harness | MCP Tools | Session Hooks | Status |
| :--- | :---: | :---: | :--- |
| **Claude Code** | ✓ | ✓ | First-class supported |
| **OpenAI Codex** | ✓ | ✓ | First-class supported |
| **Hermes Agent** | ✓ | ✓ | First-class MemoryProvider plugin |
| **OpenCode** | ✓ | ✓ | First-class supported |
| **Cursor / Windsurf** | ✓ | Manual/MCP | Supported |
| **Gemini CLI / Antigravity** | ✓ | ✓ | Supported (Gemini-safe schemas) |
| **Aider** | ✓ | ✓ | Supported |
| **Pi / Crush / Devin** | ✓ | Managed | Supported |

For complete harness setup instructions, see [`docs/install.md`](docs/install.md) and [`docs/mcp-install.md`](docs/mcp-install.md).

---

## Migration, Backup, and Recovery

Existing `ai-memory` installations can be detected without modifying the source:

```bash
sessionmunch legacy-import --path /path/to/old/data
```

Apply the import explicitly:

```bash
sessionmunch legacy-import --path /path/to/old/data --apply
```

The legacy source is not moved or deleted. SessionMunch also includes backup, restore, reindex, Git checkpoints, and page recovery:

```bash
sessionmunch backup
sessionmunch restore <backup.tar.gz>
sessionmunch reindex
sessionmunch checkpoints
```

See [`docs/legacy-import.md`](docs/legacy-import.md) and [`docs/upgrade-backup.md`](docs/upgrade-backup.md).

---

## Privacy, Security, and Data Ownership

The default posture is local and conservative:

- loopback-only HTTP on `127.0.0.1:49374`;
- zero product analytics or telemetry;
- local embeddings available without an API key;
- no summarizer enabled until the operator chooses one;
- built-in sanitization for common API keys, tokens, private-key material, credential-bearing URLs, provider credentials, and sensitive filesystem paths;
- assistant-output capture is separately opt-in because an agent can quote sensitive material you never intended to archive.

Remote/non-loopback deployment is supported, but it is an explicit operational choice rather than the default.

No sanitizer is magic. A machine that can read your repository should still be treated like a machine that can read your repository.

See [`docs/privacy-security.md`](docs/privacy-security.md).

---

## Documentation Suite

- **[Architecture & Data Ownership](docs/architecture.md):** Markdown storage layout, Git synchronization, single-writer pattern, and SQLite derivation.
- **[Quickstart Guide](docs/quickstart.md):** 2-minute setup, installation options, and first useful retrieval.
- **[Configuration Reference](docs/configuration.md):** TOML configuration reference, environment variables, and directory markers.
- **[Opinionated Model & Hardware Guide](docs/models.md):** Copy-paste local/API profiles from CPU-only systems through 24 GB+ GPUs.
- **[Model Guide Summary](docs/models-summary.md):** Short explanation of embeddings vs. summarization and provider choices.
- **[MCP Interface Reference](docs/mcp-reference.md):** MCP tool descriptions, schemas, arguments, and Gemini/Bedrock compatibility.
- **[HTTP & Frontend API](docs/http-api.md):** Read-only `/api/v1` REST endpoints, dual authentication, and web UI hosting.
- **[Hermes Agent Integration](docs/hermes.md):** First-class MemoryProvider plugin configuration and lifecycle integration.
- **[Service & Supervision](docs/service-supervision.md):** Running via systemd user/system services, launchd, or Docker.
- **[Privacy, Security & Threat Model](docs/privacy-security.md):** Zero-telemetry guarantees, secret sanitization, and network boundaries.
- **[Troubleshooting & Diagnostics](docs/troubleshooting.md):** Resolving locking issues, hook execution, and DNS rebinding errors.
- **[Upgrade, Backup & Recovery](docs/upgrade-backup.md):** Automated backups, migration steps, and disaster recovery.
- **[Legacy Import & Compatibility](docs/legacy-import.md):** Importing from predecessor `ai-memory` and external agent logs.
- **[Acknowledgements & Lineage](docs/acknowledgements.md):** Project lineage, upstream credits, and foundational inspirations.

---

## Project Lineage

SessionMunch did **not** begin from a blank editor.

It directly descends from **[akitaonrails/ai-memory](https://github.com/akitaonrails/ai-memory)** by **Fabio Akita (AkitaOnRails)**. The original project supplied the foundation and a substantial body of architecture and implementation that made this downstream work possible.

The SessionMunch v0.1.0 release candidate is based on the `ai-memory` v2.2.1 lineage and retains the upstream MIT license and copyright attribution.

From that base, SessionMunch has been developed toward a broader product goal: a standalone local-first memory runtime that can serve multiple agent harnesses, expose stable MCP/HTTP interfaces, preserve human-readable durable knowledge, provide migration and operations tooling, and integrate directly with Hermes without depending on Hermes.

This is a downstream evolution, not an attempt to quietly put a new sticker on somebody else's work.

Additional architectural influences and prior art — including jCodeMunch, jDocMunch, Basic Memory, Cognee, A-MEM, and others — are credited in [`docs/acknowledgements.md`](docs/acknowledgements.md).

**Thank you to Fabio Akita and the `ai-memory` contributors for building the ground we got to stand on.**

---

## License

SessionMunch is open-source software licensed under the MIT License. See [LICENSE](LICENSE) for details.
