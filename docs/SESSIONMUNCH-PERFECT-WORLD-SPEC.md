# SessionMunch Perfect-World Architecture — Investigation Prompt Spec

## Mission
Investigate and evolve our ai-memory fork toward **SessionMunch**: a source-grounded memory substrate that preserves the history of human/agent/tool work, structures it into progressively retrievable evidence, and gives agents the smallest sufficient slice of prior experience needed for the current task.

Treat this as the recovered target architecture and investigation brief. Separate **existing implementation**, **near-term design**, and **future capability**. Do not silently turn speculative ideas into current requirements.

## Core invariants
1. **Evidence survives.** Raw transcripts, artifacts, tool activity, decisions, research, failures, and verification remain canonical. Summaries, embeddings, indexes, and relationships are derived and rebuildable.
2. **Retrieval is structural, not page-level.** Parse document/session -> section -> passage. Preserve heading paths, ordering, source IDs, hashes, scope, and exact source offsets.
3. **Progressive disclosure.** Retrieve the smallest useful passage first; expand passage -> section -> document/session -> raw source only as needed.
4. **Deterministic retrieval works without an LLM.** LLM enrichment may add titles, summaries, entities, classifications, decisions, or relationships, but cannot be required for basic indexing/retrieval.
5. **Scope fails closed.** Workspace/project/session scope is explicit and independently applied to every retrieval stream. Cross-project recall is intentional, never accidental leakage.
6. **Provenance is first-class.** Every memory result is traceable to exact source, scope, location, time where available, and derivation chain.
7. **Compression never becomes truth.** Derived summaries accelerate retrieval but never replace canonical evidence.
8. **Caller controls context budget.** Return the best evidence fitting the requesting agent's budget instead of dumping whole pages.
9. **Derived state is disposable.** SQLite/FTS/vector indexes, summaries, embeddings, and relationship indexes should be rebuildable from canonical evidence.

## Core retrieval target
Use structurally bounded passages, initially around ~320 target tokens, ~420 hard maximum, ~48 overlap, preferring boundaries:
section -> block -> paragraph -> sentence -> token -> UTF-8-safe byte boundary.

Hybrid retrieval:
query -> BM25/FTS lexical + dense semantic -> Reciprocal Rank Fusion (RRF, baseline k=60) -> optional local cross-encoder rerank -> bounded PassageHits -> optional parent expansion -> caller context budget.

Reranking is fail-open: if unavailable, crashed, or timed out, preserve fused ordering. Do not put a general-purpose LLM in the hot retrieval path.

## Memory model beyond passages
Investigate typed historical episodes/trajectories:
- goal / initial state / final state
- decisions and reasoning
- implementation and tool actions
- experiments
- failures and diagnoses
- rejected approaches
- verification/results
- handoffs and artifacts
- corrections and superseding decisions

Distinguish "we proposed X", "we tried X", "X failed", and "X was replaced by Y". Historical truth remains while current truth can be resolved through temporal/supersession relationships.

Tool calls belong in history. Large outputs may receive compact representations, but raw expandable evidence remains reachable.

## SessionMunch vs LCM
Do **not** make SessionMunch a replacement for Hermes LCM.
- **SessionMunch:** durable historical memory and selective retrieval across accumulated work.
- **LCM:** pressure management for the currently active conversation/provider context.

Good retrieval/delegation/context assembly should reduce aggressive compaction; LCM remains the live-window pressure valve.

## Context ecosystem
Treat SessionMunch as one evidence source in a planned context system:
- jCodeMunch -> code structure/evidence
- jDocMunch -> document structure/evidence
- jDataMunch -> structured/tabular evidence
- SessionMunch -> historical sessions, episodes, decisions, trajectories, provenance
- LCM -> active-session context pressure

Investigate a turn/context planner that first asks **what evidence is needed**, then routes retrieval to the appropriate source(s), rather than searching/stuffing every corpus every turn.

## Long-term RSI role
Eventually provide historical evidence for controlled self-improvement:
task -> trajectory -> actions/tools -> failures/retries -> result -> verifier/evaluation -> historical comparison.

This may support evidence-based changes to skills, prompts/rules, tool policy, context/retrieval policy, agent topology, and eventually harness behavior. This is a later layer, not permission to contaminate core retrieval.

## Implementation layers
### Layer 1 — SessionMunch Core
Canonical source -> structural parser -> sections/passages -> lexical+dense retrieval -> RRF -> optional reranker -> scope -> provenance -> parent expansion -> budgeted retrieval -> MCP/Hermes MemoryProvider integration.

### Layer 2 — SessionMunch Intelligence
Episodes, typed decisions/failures/results, supersession/temporal truth, artifact relationships, hierarchical derived summaries, retrieval planning, context assembly.

### Layer 3 — SessionMunch Evidence/RSI
Trajectory capture, verifier outcomes, success/failure comparison, proposed improvements, controlled experiments, regression evaluation, promotion/rejection.

## Known failure to avoid
The prior ai-memory design used page-level retrieval and represented a page with a vector derived from only an early prefix (~8 KB), creating tail blindness and a mismatch between retrieval unit and evidence location. Do not "fix" this merely by changing embedding models. The structural retrieval unit is the foundational correction.

## Investigation instructions
Audit the current fork against this target. For every capability classify it as:
- already implemented and verified
- implemented but incomplete/unverified
- available upstream or via an existing library/tool
- straightforward extension
- research/experimental
- future/speculative
- obsolete because another component owns it better

Prefer native/current facilities and established libraries before custom infrastructure. Preserve current working behavior. Do not perform broad rewrites merely to match vocabulary in this spec.

Produce:
1. current-state architecture map,
2. gap matrix against Layers 1–3,
3. concrete evidence (files/symbols/tests) for claims,
4. recommended boundaries with Hermes/LCM/JEV/Munch tools,
5. smallest high-leverage implementation sequence,
6. explicit risks/migrations,
7. tests/evals proving retrieval quality, scope isolation, provenance, tail recall, latency/RSS/disk behavior, and rollback safety.

## North star
**SessionMunch preserves the history of work as canonical evidence, structures and indexes that history into progressively retrievable memory, and gives agents the smallest sufficient slice of prior experience needed to act intelligently now.**
