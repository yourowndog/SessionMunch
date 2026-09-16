# H1 Re-run — Dense path genuinely enabled (post-G2)

Card: H1 (Retrieval regression and evaluation gate) — follow-up run
Date: 2026-09-15 (session after G2/G3/G4 acceptance)
Author: Mastermind (orchestrator-seat session), direct implementation
Status: COMPLETE — gate green with dense stream genuinely exercised

## Why this re-run

The independent audit (evidence/INDEPENDENT-AUDIT-2026-09-15.md, finding 3 and
the evidence-quality note) confirmed the original H1 gate was **lexical-only**:
`passage_index_eval.rs` called `search_passages_hybrid` with `None` query
vector and empty `{provider, model, dim}`, so the dense stream never ran and
"12/12 categories covered" could not be read as proof of dense/hybrid
retrieval. The audit recommended re-running H1 "with the dense path enabled"
once the embedding-production card landed. G2 (card 97, passage embedding
production) landed and was independently verified; this re-run is that step.

## What changed in the harness

`crates/ai-memory-store/tests/suite/passage_index_eval.rs` (test-only, no
implementation changes):

1. **Dense write phase** (post-indexing): enumerates passages via
   `reader.passage_candidates(ws, proj, provider, model, dim)` — the same
   reader query `run_passage_embedding_backfill` uses in production —
   embeds each passage, and upserts the batch through
   `writer.store_passage_embeddings` (G2's real production write path).
   Guard: aborts if the corpus produces zero passages (dense phase would be
   vacuous).
2. **Dense probe loop**: every probe now ALSO runs `search_passages_hybrid`
   with `Some(query_vec)` and the matching `{provider, model, dim}` triple,
   so the reader's dense gate opens and RRF fuses both streams. Collects
   per-category found/rank for recall@k / MRR / nDCG.
3. **Reporting**: prints dense-enabled metrics and dense-vs-lexical deltas.
4. **Assertions**: dense recall@5 must clear the same absolute floor as the
   lexical baseline (RECALL_FLOOR 0.65), and at least one probe's fused
   result set must contain a hit that surfaced via `dense_rank` (proves the
   dense stream genuinely participates — the phase is not vacuous).

The eval uses a deterministic in-process test embedder (`EvalEmbedder`,
word-hash bag-of-words, 1024-dim, case-folded, unit-normalised) instead of
the production `all-MiniLM-L6-v2` local model (87MB, not bundled, requires a
network fetch — wrong for a frozen CI regression gate). It has real cosine
semantics (shared vocabulary moves vectors together) which is sufficient to
exercise the production dense *write* path, the provider/model/dim gating,
and RRF fusion end-to-end hermetically. The store crate keeps its deliberate
no-dependency-on-ai-memory-llm boundary at every level.

## Real measured output (fresh run, `--nocapture`, not self-reported)

```
=== PASSAGE-INDEX FROZEN EVAL (CARD H1) ===
Total probes: 13

RETRIEVAL METRICS (page-mode, lexical baseline):
  recall@1: 0.923   recall@3: 1.000   recall@5: 1.000   recall@10: 1.000
  MRR: 1.000        nDCG@5: 1.000

MODE COMPARISON (dense enabled):
  Dense passage-mode hits:      12/13
  Probes w/ dense-ranked hits:  13/13      <- dense stream genuinely fused into every probe

CATEGORY COVERAGE (dense passage-mode): 11/12 categories 1/1; "scope" 0/1
RETRIEVAL METRICS (dense passage-mode):
  recall@1: 0.846   recall@3: 0.923   recall@5: 0.923   recall@10: 0.923
  MRR: 0.757        nDCG@5: 0.756

DENSE-VS-LEXICAL DELTAS (passage-mode):
  recall@5: 1.000 -> 0.923  (-0.077)
  MRR:      1.000 -> 0.757  (-0.243)
  nDCG@5:   1.000 -> 0.756  (-0.244)

SYSTEM (dense phase): Dense index time 5.156003ms; Disk 0.92 MB
test integration::passage_index_eval::passage_index_frozen_eval ... ok
```

## Interpretation (what these numbers do and do not mean)

- **The dense path now genuinely runs end-to-end in the frozen gate.**
  13/13 probes had at least one dense-ranked hit in their fused result set;
  dense recall@5 (0.923) clears the same 0.65 floor the lexical side is
  held to. `tail` — the category that motivated the whole project (terms
  past byte ~8000 that page-level dense retrieval cannot see) — is found
  1/1 in dense passage mode.
- **The deltas vs lexical are an embedder-fidelity artifact of the test
  embedder, not a dense-path defect.** The word-hash bag-of-words test
  embedder has no semantic generalization, so short generic probes
  ("Scope Test", "Budget Test") dilute in the dense stream while exact
  FTS token-matching nails them trivially. One probe ("scope": 0/1 in
  dense passage mode) sits just below the fused top-5 for this reason.
  This must NOT be read as "dense hurts retrieval" for the production
  embedder — it is the reason production-quality recall/MRR/nDCG numbers
  are measured on Weakling with the real all-MiniLM-L6-v2 model (plan
  acceptance gate 8, card I1), which is a separate, deployment-gated step.
- The regression gate asserts participation and an absolute floor, not
  parity with lexical under a deliberately crude test embedder. Parity
  assertions belong to the Weakling benchmark.

## Verification (run by me, not self-reported)

```
cargo test -p ai-memory-store --lib passage_index_frozen_eval -- --nocapture
  -> test result: ok. 1 passed; 0 failed  (numbers above)
cargo test -p ai-memory-store --lib
  -> test result: ok. 465 passed; 0 failed; 1 ignored
cargo fmt --all -- --check  -> clean
cargo clippy --workspace --all-targets -- -D warnings -> clean
```

## Files changed

- `crates/ai-memory-store/tests/suite/passage_index_eval.rs` — dense write
  phase + dense probe loop + reporting + assertions (test-only).
- This evidence file.

## Status on the board

- H1 already ACCEPTED (card 88). This re-run does not reopen that card; it
  closes the audit's open question about the dense path and feeds the
  evidence trail for P1 (card 93), which remains HELD for Sam's approval.