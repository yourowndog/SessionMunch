# G3 — CLI backfill/reindex command for sections/passages

**Card:** #98 (G3) · **Branch:** `retrieval-section-index` · **Date:** 2026-09-15
**Origin:** independent audit finding 2 — `ARCHITECTURE_RECON_AND_PLAN.md`
"Index ownership and lifecycle" item 4 requires a reindex/backfill command.
`WriterHandle::backfill_sections` / `ops::backfill_sections` shipped in D2
(card 84) but nothing in `ai-memory-cli` reached them, so pages written before
the G1 cutover had no `page_sections` / `page_passages` rows and no operator
way to build them.

## State inherited (commit `70ddb4d7`, verified before this card continued)

The CLI plumbing was already committed by the orchestrator after salvaging a
stalled worker:

| File | Already present |
| --- | --- |
| `crates/ai-memory-cli/src/commands/backfill.rs` | `backfill` subcommand; POSTs `/admin/backfill-sections`; 2 unit tests over `--force` / `--project` clap parsing. |
| `crates/ai-memory-mcp/src/admin.rs` | `handle_backfill_sections` + `backfill_sections_for_project` (route registered at `admin.rs:627`). |
| `docs/ARCHITECTURE.md` | `backfill` added to the CLI subcommand list. |

The card's remaining TDD requirement was unmet: *"CLI integration test
exercising the new subcommand against a fixture DB with pre-G1 pages,
asserting sections/passages get created."* Only clap-parsing unit tests
existed.

## Diff added by this card

| File | Change |
| --- | --- |
| `crates/ai-memory-cli/tests/suite/backfill_sections.rs` | **New.** End-to-end test `backfill_rebuilds_the_section_index_for_pre_g1_pages`. |
| `crates/ai-memory-cli/tests/suite/main.rs` | `mod backfill_sections;` (the suite's single-binary registration; `repo_layout` fails on an undeclared file). |
| `crates/ai-memory-cli/Cargo.toml` | `rusqlite.workspace = true` under `[dev-dependencies]`, with a comment for why. |
| `crates/ai-memory-cli/src/commands/mod.rs` | Moved `pub mod backfill;` above `pub mod backup;` — the salvaged commit left it misordered and `cargo fmt --check` flagged it. |

4 files plus a one-line `Cargo.lock` update (`rusqlite` under `ai-memory-cli`):
+343 / -1, of which 337 lines are the new test.

### What the test actually does

1. **Fixture**: opens a real `Store` on a tempdir, creates scope
   `backfill-ws` / `backfill-proj` via `create_explicit_scope`, writes two
   pages (two `#`/`##` headings each) through `WriterHandle::upsert_page`,
   then drops the store.
2. **Pre-G1 shape**: asserts the write path *did* build a section index (so a
   later non-zero count can't be stale rows), then deletes every
   `page_passages` and `page_sections` row over a direct `rusqlite`
   connection. This is the only way to produce the pre-G1 state — invariant §3
   means every write path builds sections in the page's own transaction, so
   there is no "write a page without sections" API. Deleting through SQL also
   fires the `page_passages_fts_ad` trigger, so the FTS shadow is emptied too.
   Asserts `pages = 2`, `page_sections = 0`, `page_passages = 0`.
3. **Server**: spawns the real binary, `serve --transport http --bind
   127.0.0.1:<reserved port> --no-watcher`, hermetic
   (`AI_MEMORY_EMBEDDING_PROVIDER=none`, pinned `HOME`/data dir, no auth
   token), and blocks on the `MCP HTTP server ready` stderr line. Migrations
   run before the listener binds, so a connect-poll would report "up" too
   early. Killed on `Drop`.
4. **Dry run**: runs the real `ai-memory backfill --dry-run --workspace …
   --project …` with `AI_MEMORY_SERVER_URL` pointed at that server. Asserts
   stdout `dry-run: would backfill 2 page(s)` **and** that
   `page_sections` / `page_passages` are still `0` — the card's
   "reports a page count without mutating" requirement.
5. **Live run**: same command without `--dry-run`. Asserts stdout
   `backfilled 2 page(s)`, `page_sections = 4` (two headings × two pages),
   `page_passages >= 4`, `COUNT(DISTINCT page_id) = 2` (the batch loop covers
   both pages, not just the first), and that
   `page_passages_fts MATCH 'rollback'` returns a hit — the lexical index the
   backfill exists to feed is queryable, not merely populated.

### Scope notes

- **Embeddings are deliberately not asserted.** The card text says
  "sections/passages/embeddings", but the writer path `backfill_sections`
  drives is index-only: `ops::backfill_sections` calls
  `passage_index::replace_page_sections_and_passages` and never touches
  `page_passage_embeddings`. Passage embeddings come from the separate
  embedding path (`ai-memory embed` / the scheduled backfill tick, G2/G4),
  which needs a configured provider this hermetic test does not have.
  Asserting "embeddings created" here would either fail or require a fake
  provider testing a different code path. Documented in the test's module
  doc comment.
- **Not tiered as slow.** The test spawns three processes yet runs in 0.20s
  consistently, well inside AGENTS.md's ~1s everyday budget, so it lives in
  the default tier and `cargo t` covers it rather than only `cargo tf`.
- **`--force` is still a no-op on the server** (`BackfillSectionsRequest.force`
  carries `#[allow(dead_code)]`; `ops::backfill_sections` always rebuilds).
  The test does not assert skip-if-exists semantics that do not exist; the
  existing clap unit tests cover the flag's fan-out intent.

## Falsification check

The test was confirmed non-vacuous before being accepted: changing the live
run to `--dry-run` makes it fail —

```
thread '…backfill_rebuilds_the_section_index_for_pre_g1_pages' panicked at
crates/ai-memory-cli/tests/suite/backfill_sections.rs:299:9:
live run should report both pages, got: dry-run: would backfill 2 page(s), 0 already have sections
```

The file was restored and re-run green. Because the `0` assertion (after
dry-run) and the `4` assertion (after the live run) hit the same database,
the pass proves the index genuinely transitions `0 → 4` as a result of the
CLI invocation.

## Verification

The primary worktree is shared with another agent whose G2/G4 passage-embedding
work was uncommitted and in flight during this card. Mid-session it broke the
workspace build (`serve.rs` importing `run_passage_embedding_backfill` before
it was exported) and left two `commands::serve::tests` red — neither reachable
from this card's diff. That agent has since landed it as `1810e138`, and both
are green again.

To keep the result attributable, the gate was first run in a **clean detached
worktree at `70ddb4d7` + this card's four files only**:

```
$ cargo build --workspace
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4m 52s

$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 12s   (no warnings)

$ cargo fmt --all -- --check        # filtered to ai-memory-cli
    (no diffs)

$ cargo test --workspace --all-targets
running 886 tests   test result: ok. 885 passed; 0 failed; 1 ignored   (ai-memory-cli lib)
running 119 tests   test result: ok. 118 passed; 0 failed; 1 ignored   (ai-memory-cli suite)
running 222 tests   test result: ok. 218 passed; 0 failed; 4 ignored
running 211 tests   test result: ok. 211 passed; 0 failed; 0 ignored
running  18 tests   test result: ok.  18 passed; 0 failed; 0 ignored
running 304 tests   test result: ok. 304 passed; 0 failed; 0 ignored
running 232 tests   test result: ok. 230 passed; 0 failed; 2 ignored
running 425 tests   test result: ok. 425 passed; 0 failed; 0 ignored
running 466 tests   test result: ok. 465 passed; 0 failed; 1 ignored
running 114 tests   test result: ok. 114 passed; 0 failed; 0 ignored
running 186 tests   test result: ok. 185 passed; 0 failed; 1 ignored
running  95 tests   test result: ok.  95 passed; 0 failed; 0 ignored
```

Zero failures across the workspace — the same command CI runs. The new test
in isolation:

```
$ cargo test -p ai-memory-cli --test suite backfill_sections
running 1 test
test backfill_sections::backfill_rebuilds_the_section_index_for_pre_g1_pages ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 118 filtered out; finished in 0.21s
```

Re-run in the primary worktree after the other agent landed `1810e138`
(current branch HEAD), `cargo clippy --workspace --all-targets -- -D warnings`
is clean and `cargo test --workspace --all-targets` is green with the same
counts (885 / 118 / 218 / 211 / 18 / 304 / 230 / 425 / 465 / 114 / 185 / 95,
zero failures).

The pre-existing `commands::backfill` unit tests stay green:

```
$ cargo test -p ai-memory-cli --lib backfill
test commands::backfill::tests::force_without_project_fans_out_to_all_projects ... ok
test commands::backfill::tests::force_with_explicit_project_stays_scoped ... ok
```

## Pre-existing issues found, not fixed (out of card scope)

1. **`cargo test -p ai-memory-cli --doc` fails at `70ddb4d7`**, independent of
   this card: `crates/ai-memory-cli/src/commands/setup_agent.rs:12` has an
   unfenced `docker run --rm \ …` shell snippet in a doc comment, which rustdoc
   compiles as Rust (`error: unknown start of token: \`, 8 errors). Neither CI
   (`cargo test --workspace --all-targets`) nor nextest runs doctests, so it is
   latent. One-line fix: fence it as ```text.
2. **Branch-wide `cargo fmt` drift** from earlier passage-index commits —
   `ai-memory-core/src/sections.rs`, `store/src/passage_index.rs`,
   `store/tests/suite/hybrid_search.rs`, `store/tests/suite/passage_index_eval.rs`,
   `mcp/src/server.rs`, and others are unformatted at HEAD. `cargo fmt --all --
   --check` is a release gate, so this needs a formatting-only commit before the
   branch merges. Not touched here — `cargo fmt --all` would also rewrite the
   other agent's in-flight files.
