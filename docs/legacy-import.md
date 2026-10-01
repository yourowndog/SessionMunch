# Legacy Import & Migration Guide

Migrating to SessionMunch from older installations, predecessor projects (`ai-memory`), or external agent memory stores is safe, automated, and non-destructive.

---

## 1. Upgrading from Predecessor `ai-memory`

SessionMunch was originally developed as `ai-memory`. We maintain strict backwards compatibility for existing deployments:

- **Data Directory Fallback:** If `SESSIONMUNCH_DATA_DIR` is not set, SessionMunch automatically checks `AI_MEMORY_DATA_DIR` and legacy default directories (`~/.local/share/ai-memory`).
- **Marker Files:** Repository trees containing legacy `.ai-memory.toml` markers continue to resolve projects and workspaces without modification. When both `.sessionmunch.toml` and `.ai-memory.toml` exist at the same directory depth, the newer marker takes precedence.
- **Auth & API Keys:** Existing `aim_` database user tokens and legacy authorization tokens remain fully functional.
- **CLI Forwarding Shims:** Deprecated `ai-memory` binaries output a helpful deprecation notice directing operators to invoke `sessionmunch`.

To upgrade an existing database to the latest schema:
```bash
sessionmunch --data-dir ~/.local/share/sessionmunch reindex
```

---

## 2. Importing External Corpora: `sessionmunch-importer`

SessionMunch includes a standalone, zero-dependency companion crate located at `companions/ai-memory-importer` for importing external memory sets into a running SessionMunch server.

### Supported Sources

1. **Oh-My-ClaudeCode (OMC) Markdown Wikis:**
   Reads flat Markdown wiki directories and maps them to clean destination paths under `omc/<slug>.md`.

2. **Generic External Conversations:**
   Imports conversation archives from ChatGPT, Claude Desktop, or custom tools via a minimal JSON interchange format:

```json
{
  "project": "my-project",
  "source": "chatgpt",
  "session_id": "exported-conversation-id",
  "messages": [
    { "role": "user", "content": "What is our database strategy?" },
    { "role": "assistant", "content": "We use SQLite with FTS5 and plain markdown files." }
  ]
}
```

### Safety Contract
- **Dry-run by default:** The importer plans the migration and outputs a summary. To execute writes, explicitly pass `--apply`.
- **Safe writes only:** The importer writes exclusively through `POST /admin/write-page` over HTTP with client-side credential redaction; it never modifies internal SQLite files directly.
- **Resumable:** Uses stable deduplication keys so interrupted imports can be safely resumed without duplicating pages.
