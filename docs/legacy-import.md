# Legacy Import & Migration Guide (via `sessionmunch legacy-import`)

Migrating to SessionMunch from older `ai-memory` installations is safe,
automated, and **non-destructive**. The `sessionmunch legacy-import` command
detects, plans, and copies legacy wiki pages into a SessionMunch data
directory without ever modifying the source.

## Quick Start

```bash
# 1. Detect a legacy installation (read-only)
sessionmunch legacy-import --path ~/.local/share/ai-memory

# 2. Import pages (dry-run first if unsure — see above)
sessionmunch legacy-import --path ~/.local/share/ai-memory \
  --apply --create-destination
```

---

## Detection (`read-only`, no `--apply`)

By default `legacy-import` runs in **detection mode**: it inspects the
given path, reports what it finds, and exits without writing anything.

```bash
sessionmunch legacy-import --path ~/.local/share/ai-memory
```

Output shows:
- Whether `config.toml`, `.ai-memory.toml`, wiki, and database exist
- The number and paths of wiki pages found
- The configured workspace name from the source config

Detection is always safe to run on a live data directory — it never opens
the SQLite database for writes and only reads markdown file headers/hashes
after printing its findings.

### Auto-detection

When `--path` is omitted, the command searches these locations (in order):

1. `$AI_MEMORY_DATA_DIR` environment variable
2. `$HOME/.local/share/ai-memory`
3. `$HOME/.ai-memory`
4. `$HOME/.config/ai-memory`

---

## Import Plan (dry-run output)

After detection, the command prints an **import plan** showing every
page that would be copied, its source path, and its destination path.
The plan also indicates which pages were already imported in a prior
run (marked `Copied` vs `Planned`).

No files are written during the plan phase.

---

## Import (`--apply`)

To actually copy pages, pass `--apply`:

```bash
sessionmunch legacy-import --path ~/.local/share/ai-memory \
  --apply --create-destination
```

### What happens

1. **Source verification** — every page is SHA-256 hashed before copy.
   If a page changed between the plan and the copy, the import aborts
   with a clear error.
2. **Atomic copy** — files are read, written byte-for-byte to the
   destination, then re-hashed to confirm integrity.
3. **Manifest tracking** — a file `.legacy-import-manifest.json` is
   written alongside the source data (never inside SessionMunch's own
   data dir). This records every page, its status, and both hashes.
4. **Final integrity check** — after all pages are copied, every source
   file is re-read and re-hashed to prove it was unchanged by the
   import process.

### Destination

Pages are copied into:
`<data_dir>/wiki/<workspace>/<project>/<page-path>`

- `workspace` defaults to the source config's declared workspace, or
  `"default"`.
- `project` defaults to the source config's declared project, or the
  source directory's basename.
- Override either with `--workspace` or `--project`.

### `--create-destination`

By default the command refuses if the target `wiki/<workspace>/<project>/`
directory does not exist. Pass `--create-destination` to auto-create it.
This guard prevents accidental import into a misspelled workspace or
project.

---

## Destinations and Scope

| Flag | Default | Description |
|------|---------|-------------|
| `--path` | auto-detected | Source legacy ai-memory data directory |
| `--workspace` | from source config | Workspace name for imported pages |
| `--project` | from source config / dir basename | Project name for imported pages |
| `--apply` | off (detection only) | Actually copy pages |
| `--create-destination` | off | Auto-create destination directory |
| `--manifest-out` | `<source>/.legacy-import-manifest.json` | Path for the import manifest |

---

## Verification & Integrity

The import guarantees:

- **Source files are never modified.** The command is strictly
  copy-only. After import, every source file is re-hashed and the
  result compared against the pre-import hash. If any differ, the
  final report shows a failure count.
- **Copied files match.** Each destination file is hashed
  immediately after write and compared against the source hash.
- **Manifest is the audit trail.** The JSON manifest records every
  source path, its SHA-256, the destination path, and the
  verification status. Check it any time:
  ```bash
  cat ~/.local/share/ai-memory/.legacy-import-manifest.json
  ```

---

## Safety: What `legacy-import` Does NOT Do

- ❌ Does **not** modify, move, rename, or delete source files
- ❌ Does **not** write to the source SQLite database
- ❌ Does **not** write to SessionMunch's SQLite database (pages
  must be indexed separately with `sessionmunch reindex` or by
  starting the server with the wiki watcher)
- ❌ Does **not** create ai-memory directories in a fresh
  SessionMunch install (no accidental compat cruft)
- ❌ Does **not** pull, push, tag, or release anything

---

## Rollback

Because the source is never modified, there is nothing to roll back
on the source side. To undo the import on the SessionMunch side:

1. Remove imported pages from the wiki directory:
   ```bash
   rm -rf <data_dir>/wiki/<workspace>/<project>/
   ```
2. Remove the manifest from the source:
   ```bash
   rm <source>/.legacy-import-manifest.json
   ```

---

## Resume After Interruption

If the import is interrupted (power loss, Ctrl+C, crash), it is safe
to re-run with the same arguments:

- Pages that were already copied (and recorded in the manifest) are
  **skipped** — their hash is compared against the source to ensure
  nothing changed.
- Only pages with status `Planned` are processed.
- The manifest is written atomically (tmp + rename) after every
  page, so the recovery point is always consistent.

---

## Fresh Install Guarantee

A `sessionmunch init` into an empty or new data directory never
creates legacy ai-memory directories or files. The marker created
is always `.sessionmunch.toml`. Compatibility with existing
`.ai-memory.toml` markers is read-only and only activates when
the legacy file already exists.