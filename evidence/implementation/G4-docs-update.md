# G4 — MEMORY_INSTRUCTIONS / SNIPPET_BODY document `unit=passage`

**Card:** #99 (G4) · **Branch:** `retrieval-section-index` · **Date:** 2026-09-15
**Origin:** independent audit finding 3 — `memory_query` gained `unit`,
`parent_expansion`, and `content_budget` in G1, but no agent-facing prompt
surface mentioned them, so passage mode was effectively undiscoverable.

## Diff summary

| File | Change |
| --- | --- |
| `crates/ai-memory-mcp/src/server.rs` | `MEMORY_INSTRUCTIONS`: extended the `memory_query` bullet with passage-unit guidance. Added regression test `prompts_document_passage_retrieval_unit`. |
| `crates/ai-memory-core/src/routing_snippet.rs` | `SNIPPET_BODY`: new paragraph in the retrieval block covering page vs passage, `parent_expansion`, `content_budget`. |
| `AGENTS.md` | Regenerated managed block (same paragraph) — `routing_snippet::tests::committed_agents_md_matches_snippet_body` asserts byte equality with `full_block()`. |
| `crates/ai-memory-core/src/routing_skills/ai-memory-retrieval/SKILL.md` | New `## Page or passage` section: the argument-level routing that `SNIPPET_BODY` defers to the installed retrieval skill. |

4 files, +78 / -1.

### What the new text says (documented against the shipped code, not the plan)

- `unit="page"` (default) returns whole pages; `unit="passage"` returns
  ingest-time passages — pick passage when the answer is one span inside a long
  page, page when the whole page is the unit of meaning.
- Passage mode fuses FTS5 with the dense passage stream when an embedder is
  configured and degrades to FTS5-only when none is
  (`server.rs::memory_query_passage` → `reader::search_passages_hybrid`).
- Passage mode reads the current project or explicit `scopes` only;
  `global=true`, `as_of`, and `include_expired` are not honoured on that path
  (`server.rs:2190` returns before the page-mode global/recall-global logic, and
  `memory_query_passage` never reads `args.global` / `args.as_of` /
  `include_expired`).
- `parent_expansion`: `none` (default) = passage alone, `section` = parent
  heading path + section id, `document` = also page workspace/project/path/title
  (`PassageParent`, `PassageParentDocument`).
- `content_budget`: caps characters in each passage's `text`; default 8192
  (`DEFAULT_CONTENT_BUDGET`), `0` disables the cap (`server.rs:2109`).

`SNIPPET_BODY` deliberately names no MCP tool: the existing test
`snippet_omits_detailed_tool_routing_table` asserts the slim snippet leaves
per-tool routing to the managed skills, so the passage paragraph is written
tool-name-free and the argument detail lives in the retrieval skill.

## Tests

`cargo test -p ai-memory-mcp -p ai-memory-core --all-targets`

```
running 211 tests   (ai-memory-core)
test routing_snippet::tests::committed_agents_md_matches_snippet_body ... ok
test result: ok. 211 passed; 0 failed; 0 ignored

running 425 tests   (ai-memory-mcp)
test server::tests::prompts_document_passage_retrieval_unit ... ok
test server::tests::prompts_cover_every_registered_mcp_tool ... ok
test server::tests::snippet_keeps_always_loaded_invariants ... ok
test server::tests::snippet_omits_detailed_tool_routing_table ... ok
test server::tests::routing_prompt_surfaces_share_the_client_aware_scope_contract ... ok
test result: ok. 425 passed; 0 failed; 0 ignored
```

No pre-existing prompt-surface assertion needed rewriting: every existing test
asserts required substrings or forbidden substrings, and the added text neither
removes a required string nor introduces a forbidden one. The new test is the
guard that the passage unit stays documented in all three surfaces.

`cargo build -p ai-memory-mcp -p ai-memory-core` — clean (one pre-existing
`dead_code` warning in `ai-memory-consolidate::embed::flush_passage_embedding_batch`,
from the G1 WIP commit, untouched here).

`cargo clippy -p ai-memory-mcp -p ai-memory-core --all-targets -- -D warnings` —
clean, exit 0, no diagnostics.

`git diff --check` — clean (exit 0).

## Known-red, not caused by this card

`cargo fmt --all -- --check` is red in this worktree. It is red at `HEAD` too:
commit `a1a334fc` (G1 WIP) landed unformatted code in `sections.rs`,
`passage_index.rs`, `hybrid_search.rs`, `store/lib.rs`, `reader.rs`, `writer.rs`
and in the `unit`/`parent_expansion`/`content_budget` literals inside
`server.rs` test fixtures. Measured per file with the pinned 1.95 rustfmt:

```
crates/ai-memory-mcp/src/server.rs        HEAD=24 diffs   with G4=24 diffs
crates/ai-memory-core/src/routing_snippet.rs  HEAD=0 diffs   with G4=0 diffs
```

G4 adds zero formatting debt. Fixing the G1 formatting is a separate card.

## Not done / flagged

- **No CHANGELOG entry.** The repo's merge gate wants one with a `(#NNN)`
  reference; the passage feature (G1) shipped on this branch without one, and
  this card has no GitHub issue number to cite. Recommend one combined
  `### Added` entry when the passage series is squared up for merge.
- **`QueryArgs::content_budget` doc comment overstates.** It says the budget
  applies to "`snippet` (page mode) or `text` (passage mode)"; page mode passes
  `hit.snippet` through untouched (`server.rs:1864`) and never reads
  `content_budget`. Left as G1 shipped it — a behaviour/doc decision, not a
  prompt-surface fix. The prompt surfaces written here only claim the passage
  behaviour that actually exists.
- Capture substrate (`ai-memory-hooks`, `ai-memory-consolidate`,
  `ai-memory-wiki`) untouched. No deploy.
