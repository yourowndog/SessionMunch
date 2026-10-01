<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.png">
    <img alt="SessionMunch" src="docs/logo-light.png" width="460">
  </picture>
</p>

<p align="center">
  <strong>Persistent, shared memory across sessions, tools, and machines for coding agents.</strong><br>
  Fast local-first retrieval • Plain Markdown source of truth • Zero lock-in • Single Rust binary
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> •
  <a href="#why-sessionmunch">Why SessionMunch</a> •
  <a href="#how-it-works">How it works</a> •
  <a href="#supported-agent-harnesses">Supported agents</a> •
  <a href="docs/architecture.md">Architecture</a> •
  <a href="docs/models-summary.md">Model guide</a> •
  <a href="docs/troubleshooting.md">Troubleshooting</a>
</p>

---

## What is SessionMunch?

Coding agents start every session with amnesia. Context windows fill rapidly, handoffs between tools are clumsy or lost, and when you switch between Claude Code, OpenAI Codex, or Hermes Agent, architectural decisions and debugging discoveries evaporate.

**SessionMunch** provides a durable, shared memory layer across all your AI coding agents.

- **Plain Markdown Source of Truth:** Your memory lives on disk in plain `.md` files under Git version control. No vendor cloud, no proprietary database traps. Inspect, grep, or edit notes using Neovim, VS Code, or Obsidian.
- **Derived SQLite Acceleration:** A high-speed SQLite layer handles fast FTS5 full-text search, entity relationship graphs, and hybrid vector indexing. If the database file is lost, `sessionmunch reindex` regenerates it cleanly from your markdown files.
- **Fast & Single-Binary:** Written in Rust with zero daemon sprawl. Runs as a background service or container on your local machine or private server.
- **Model-Agnostic & Zero-LLM Ready:** Works completely offline with zero API keys using FTS5 and optional local ONNX embeddings. Seamlessly upgrades to API providers (Anthropic, OpenAI, Google) or existing subscription OAuth tokens.
- **Strict Privacy & Zero Telemetry:** Built-in sanitization redacts secrets, API keys, and private paths before writes. Zero tracking libraries. Single-user loopback safety by default.

---

## Why SessionMunch?

Existing memory tools tend to fall into two failure modes:
1. **Proprietary cloud silos** that lock your memory behind metered APIs and black-box embeddings.
2. **Fragile JSON blobs or ephemeral scratchpads** that are impossible to search, maintain, or share across different agent harnesses.

SessionMunch takes an understated, Unix-first approach inspired by the **jCodeMunch** and **jDocMunch** philosophy: ingest accurately, index deterministically, and retrieve cleanly without unnecessary overhead.

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

#### Arch Linux (AUR)
```bash
yay -S sessionmunch-bin
systemctl --user enable --now sessionmunch.service
```

#### Prebuilt Binary (Linux / macOS)
Download the latest release binary to your `PATH` (e.g. `~/.local/bin/sessionmunch`):
```bash
sessionmunch --data-dir ~/.local/share/sessionmunch init
sessionmunch serve --data-dir ~/.local/share/sessionmunch
```

#### Docker Container
```bash
docker run -d --name sessionmunch \
  --restart unless-stopped \
  -p 127.0.0.1:49374:49374 \
  -v sessionmunch-data:/data \
  yourowndog/sessionmunch:latest
```

*By default, SessionMunch binds strictly to `127.0.0.1:49374` without authentication, safe for single-user workstations.*

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

## Documentation Suite

- **[Architecture & Data Ownership](docs/architecture.md):** Markdown storage layout, Git synchronization, single-writer pattern, and SQLite derivation.
- **[Quickstart Guide](docs/quickstart.md):** 2-minute setup, installation options, and first useful retrieval.
- **[Configuration Reference](docs/configuration.md):** TOML configuration reference, environment variables, and directory markers.
- **[Model Guide (Local vs. API)](docs/models-summary.md):** Hardware profiles, local embeddings, cloud models, and subscription OAuth.
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

## Upstream Lineage & Acknowledgements

SessionMunch directly evolves the [ai-memory](https://github.com/akitaonrails/ai-memory) project created by Fabio Akita (AkitaOnRails), with architectural inspiration drawn from **jCodeMunch** and **jDocMunch**.

For complete lineage, acknowledgements, and related prior art, see [`docs/acknowledgements.md`](docs/acknowledgements.md).

---

## License

SessionMunch is open-source software licensed under the MIT License. See [LICENSE](LICENSE) for details.
