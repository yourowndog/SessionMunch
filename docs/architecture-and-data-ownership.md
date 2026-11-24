# Architecture & Data Ownership

SessionMunch is built on a clear, uncompromising premise: **your memory belongs to you, in formats you can inspect and manipulate with standard Unix tools.**

We reject the opaque black-box approach where long-term agent memories are trapped in proprietary databases, vendor-hosted clouds, or fragile binary vector indexes.

---

## 1. Storage Architecture: Plain Markdown First

The wiki source of truth is an ordinary directory of plain Markdown files (`.md`), versioned with Git. Session history and other structured state are authoritative in SQLite; large evidence is kept separately:

```text
<data_dir>/
├── wiki/            # Markdown source of truth, git-versioned
│   ├── <workspace>/
│   │   └── <project>/
│   │       ├── index.md
│   │       ├── log.md
│   │       ├── concepts/
│   │       ├── gotchas/
│   │       └── sessions/
├── raw/             # Bounded, sanitized raw observation segments
├── db/              # SQLite session/event ledger, operational state and search indexes
├── artifacts/       # Content-addressed oversized evidence (configured artifact root)
├── models/          # Optional local model weights (ONNX)
└── logs/            # Structured runtime logs
```

### Key Properties
- **Inspectable & Diffable:** Open your wiki in Obsidian, Neovim, VS Code, or read it with `grep` and `cat`. Git provides full version history and audit trails.
- **SQLite ledger and indexes:** SQLite holds authoritative structured history as well as derived search indexes. Reindexing Markdown cannot recover missing sessions, event provenance, handoffs or operational state. Restore the database from a verified backup; preserve associated artifact blobs and the wiki too.
- **Single Writer Pattern:** All writes pass through a dedicated single-writer channel to coordinate writes while serving concurrent read queries.

---

## 2. Ingestion & Consolidation Lifecycle

```text
[Agent Turn] ──▶ [Lifecycle Hooks] ──▶ [Sanitization] ──▶ [Raw Spool]
                                                             │
                                                             ▼
[Agent Resume] ◀── [Context Brief] ◀── [Markdown Wiki] ◀── [Consolidation]
```

1. **Capture:** Lightweight lifecycle hooks intercept tool invocations, prompts, and session bounds.
2. **Sanitize:** The client and server enforce strict privacy filters, redacting API keys, bearer tokens, SSH paths, and credentials before persistence.
3. **Consolidate:** At session exit (or on pre-compact), observations are compacted into structured markdown pages (rule-based or LLM-enhanced).
4. **Retrieve:** Agents query memory using narrow, expressive MCP tools, receiving unified rankings across text, entities, and semantic vectors.

---
## 3. Data Ownership & Migration

- **Zero Lock-in:** You can export, copy, rsync, or Git-push your wiki anywhere. The native backup includes the wiki, SQLite snapshot and configuration. Complete recovery also requires separately preserving artifact blobs, native snapshots and client outboxes.
- **Self-Contained Single Binary:** No external services, daemon dependencies, or complex orchestration required.
- **Portability:** Move effortlessly between single-user laptops, homelab servers, and team workstations.
