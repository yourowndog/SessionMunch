# SessionMunch North Star
## Architecture, Metrics, and Roadmap

**Status:** Canonical architecture guide
**Adopted:** 2026-10-02
**Canonical host:** `sleeper`
**Canonical working tree:** `~/projects/sessionmunch`

## Purpose

This document is the durable reference for what SessionMunch is, what it is not, how its major pieces fit together, and what evidence must justify future architectural changes.

Workers may implement pieces of this roadmap, but they do not get to redefine the product while doing so. Local task success is not enough if it damages the global architecture.

SessionMunch exists to make past agent work recoverable as evidence and reusable as knowledge. It must preserve what actually happened, make that history searchable, and allow curated knowledge to cite the underlying evidence.

The core rule is simple:

> **A summary may help us find the truth. It must never become the only copy of the truth.**

SessionMunch is not a behavioral-learning or personalization engine. It remembers evidence; it does not autonomously learn how to rank Sam, projects, models, or preferences.
## 1. Product Model: Three Layers

### SOURCE — immutable evidence

The source layer records what actually happened.

- sessions
- messages and tool events
- commands and tool results
- compactions/system events
- full large outputs stored as artifacts
- provenance such as machine, harness, repository, branch, commit, and time

Source evidence is append-oriented and provenance-bearing. Derived systems may be rebuilt from it. Curated knowledge may cite it. Neither may silently rewrite it.

### DERIVED — disposable indexes and interpretations

Derived state exists to make source and curated material useful.

- searchable passages over wiki pages and session history
- FTS indexes
- embeddings
- entities and links
- deterministic session digests
- error fingerprints
- session relationships

Derived state must be rebuildable from source evidence plus the curated wiki.

### CURATED — durable knowledge

The existing git-versioned Markdown wiki remains the curated layer: decisions, gotchas, handoffs, conclusions, and consolidated project knowledge.

Raw transcripts do **not** become wiki pages.
## 2. Architectural Invariants

1. **Evidence survives summarization.** Raw recoverable evidence is never intentionally replaced by an LLM summary.
2. **One retrieval engine.** Project search and global history search share the same lexical/dense retrieval machinery instead of maintaining a weaker global path.
3. **Project is a label and candidate pool, not a silo.** Global history remains globally searchable; project can filter or supply a local candidate pool.
4. **No silent loss.** Unsupported, unreadable, truncated, compacted, or otherwise lost source material must be reported explicitly.
5. **No new service without measured need.** SQLite plus local artifact storage remains the default. No separate vector database or search service by default.
6. **No ANN by fashion.** Approximate-nearest-neighbor indexing is introduced only after measured thresholds justify it.
7. **Sanitize before persistence.** Secrets must be removed before artifact bytes are written, hashed, deduplicated, or indexed.
8. **Derived state is replaceable.** Indexes, embeddings, digests, fingerprints, and relationships carry sufficient version/source information to be rebuilt.
9. **History is not personalization.** No learning-to-rank, behavioral profile, self-tuning ranking, or autonomous preference model.
10. **Workers obey the whole system.** A task that passes its local test but violates an invariant is not complete.

Changing an invariant requires an explicit architecture decision, not an incidental implementation choice inside a worker card.
## 3. Accepted System Decisions

### Canonical deployment

- `sleeper` is the canonical SessionMunch brain and database host.
- Other machines send live capture/imported history to Sleeper.
- Every machine keeps a local spool/queue so temporary network or Sleeper outages do not lose evidence.
- We are **not** building independent per-node databases plus distributed synchronization.

### Repository and runtime layout

- Source working tree: `~/projects/sessionmunch`
- Configuration: `~/.config/sessionmunch/`
- Runtime/database/artifacts: `~/.local/share/sessionmunch/`

The former implementation path remains a compatibility symlink during transition.

**Important cleanup constraint:** the current working tree is still a linked Git worktree whose common Git administration/object store lives under the older evaluation tree. Do not delete or relocate that common source repository while release/blocker worktrees depend on it. Normalize that Git-common-dir arrangement only after those worktrees are retired.

### Retention

- Sanitized raw evidence is retained indefinitely by default.
- Explicit administrative purge must remain possible for secrets, corruption, deliberate deletion, or exceptional cleanup.
- “Immutable” means SessionMunch does not silently rewrite evidence; it does **not** mean the owner is forbidden from deleting it.
### Model and harness metadata

- Harness names become free text rather than a schema CHECK enum that forces table rebuilds for every new harness.
- Model/provider/reasoning metadata is best-effort **per event**, because models may change during a session.
- “Models seen in this session” is derived from the events.
- We do not build a dedicated model-configuration history subsystem.

### Hermes

Hermes is a Phase 1 history source, not a Phase 4 afterthought. It is now central enough to the real workflow that a “global history” system excluding Hermes would be misleadingly incomplete.

### Cutover sequencing

- Raw history snapshotting may begin immediately because source transcript stores are perishable.
- Major P1 schema/history restructuring waits until the current production cutover and release blockers are closed.
- This avoids stacking a large migration on top of an unfinished production cutover while still preventing avoidable history loss.

## 4. Source Data Shape

### Session — first-class fields

A session should carry:

- id
- harness
- native_session_id
- machine_id
- nullable project_id
- cwd
- git_root, branch, commit_at_start (best effort)
- started_at, ended_at
- source: hook | import | managed
- import_cursor
### Event — first-class fields

An event belongs to a session and should carry:

- session_id, sequence, timestamp
- kind: user | assistant | tool_call | tool_result | system/compaction
- call/link identifier where applicable
- tool_name
- bounded inline text
- optional artifact_sha256 for the complete large payload
- exit_code / is_error
- metadata_json for harness-specific details, including model/provider when known

The existing portable workstream-event concept should become the general session-owned event ledger rather than creating a third parallel event system.

### Artifacts

Large sanitized outputs are stored content-addressed, compressed on disk.

The event keeps:
- command or identifying text inline
- a useful head + tail excerpt
- byte length
- sha256
- pointer to the complete artifact

Large artifact text is indexed lexically by default. We do not embed megabytes of log chunks merely because we can.

### Derived relationships

Files touched, shared commits, handoffs, related sessions, and similar relationships are derived rather than promoted into an ever-growing set of special-purpose source tables.
## 5. Capture and Historical Backfill

### Live capture

Hook-path latency stays small:

1. validate
2. sanitize
3. spool / establish ingest identity
4. return

Heavier work is asynchronous.

### Historical import

The user-facing shape is approximately:

`sessionmunch import sessions --since 7d [--harness ...] [--machine-local] [--dry-run]`

Import must:

1. discover known harness stores and explicitly report missing/unreadable/unsupported ones
2. enumerate sessions independent of the current working directory
3. reuse existing native transcript readers where possible
4. deduplicate sessions by machine + harness + native session id
5. deduplicate events by stable source/event identity
6. persist cursors so repeated imports append only new tails
7. merge native transcript content with hook timing/provenance rather than creating duplicate histories
8. store complete large sanitized evidence in artifacts
9. queue passage/index/embedding work asynchronously
10. report found/imported/already-known/changed/lost/unsupported counts per harness and machine

### Emergency preservation

Before parsing support is perfect, SessionMunch may snapshot native harness stores into hashed read-only artifacts. This is a preservation step, not a claim that the content has already been interpreted correctly.
## 6. One Global Retrieval Engine

The global path must stop being a weaker page-only FTS shortcut.

Candidate generation should combine:

- lexical passage search across all eligible history/knowledge
- dense passage search across the same eligible corpus
- optional caller-requested filters: time, harness, machine, project, event kind
- when a current project is known, an additional candidate pool restricted to that project

The local-project pool is a deliberate relevance prior, not magical truth. Keep it only while evaluation shows it helps.

Existing RRF fusion remains the deterministic first-stage combiner.

After fusion:
- deduplicate aggressively enough that one session/page cannot flood the result set
- send only a bounded shortlist (roughly top 40) to JEV
- JEV judges semantic relevance, not provenance policy or hidden metadata weighting
- JEV must be bounded, timeout-controlled, and fail-open

Wiki-only authority adjustments/entity-neighbor expansion stay out of the initial global-history path unless evaluation demonstrates that they improve real global queries.

## 7. Progressive Disclosure

Search returns handles and small previews, not transcript dumps.

The stable conceptual verbs are:

1. **query** — find relevant handles
2. **expand** — reveal the passage or its parent context
3. **timeline** — show nearby events around a result
4. **read** — read a bounded range/grep from an event or artifact

Existing handoff, consolidate, recent-memory, and wiki-writing functions remain separate product capabilities.

Do not proliferate tool verbs when these four can express the history-navigation job.
## 8. Indexing Lifecycle and “Dreaming”

### Asynchronous indexing

After capture/import:

1. write event rows and artifacts
2. build passages and lexical indexes
3. run/retry embedding jobs

At session end:

1. reconcile against the native transcript using its cursor
2. create a deterministic session digest
3. optionally invoke existing LLM consolidation into curated wiki knowledge

The deterministic digest should summarize facts such as first prompt, commands, files touched, errors, final assistant message, time span, harness, machine, and models seen. It is navigation metadata, not the authoritative record.

### Dreaming

“Dreaming” means bounded maintenance, not an autonomous personality-learning loop.

A nightly idempotent/interruptible work queue may:

- reconcile sessions missing clean ends
- create missing deterministic digests
- retry failed embeddings
- compute error fingerprints
- derive files-touched entities
- link sessions by commit/files/handoff
- detect stale builder versions
- propose near-duplicate wiki supersession through the existing approval path

It must not rewrite source evidence, self-tune ranking, autonomously alter curated knowledge outside the approval gate, or require an LLM by default.
## 9. Metrics and Promotion Gates

Architecture changes graduate on evidence, not enthusiasm.

### Preservation metrics

- **Silent source loss:** target 0. Every known loss is explicit and attributable.
- **Discovery coverage:** every supported harness store is found or explicitly reported unavailable.
- **Importer idempotency:** rerunning an unchanged import creates no duplicate sessions/events/artifacts.
- **Artifact integrity:** stored payload hash verification succeeds.
- **Reconciliation:** hook and transcript data converge on one session identity instead of duplicating history.

### Retrieval quality metrics

Maintain a versioned evaluation set made from real questions such as:
- “Have we seen this exact error?”
- “When did we run this command on Sleeper?”
- “What did we decide about X?”
- “Show the session where this regression appeared.”
- “What happened immediately before this failure?”

Before a ranking/indexing change becomes default:
1. record the existing baseline
2. run the same evaluation set
3. compare retrieval quality and latency
4. keep the change only if it provides a measurable benefit without violating invariants

JEV is a reranker over a bounded shortlist, not a substitute for a healthy candidate generator.

### Performance thresholds

The first dense-search optimization is a pre-normalized flat vector matrix/cache rather than ANN.

Consider ANN only when, on the real target node after the flat-matrix improvement:
- dense retrieval exceeds ~150 ms p95, **or**
- vector RAM exceeds ~25% of free host memory, **or**
- the corpus reaches roughly 1–2 million embedded passages

These are trigger thresholds for investigation, not an instruction to install HNSW automatically.
### Operational metrics

Track/report:
- ingest/spool failures
- asynchronous indexing backlog
- embedding failures/retries
- time from event capture to lexical searchability
- time from lexical searchability to embedding availability
- artifact bytes and passage counts
- import counts/losses by harness and machine
- query latency by lexical, dense, fusion, and rerank stage

Do not add telemetry to a remote service. These are local operational measurements.

## 10. Phased Roadmap

### P0 — Stop losing history

- snapshot perishable native harness stores into hashed read-only artifacts
- confirm/fix the reported lexical passage ranking-order bug and add a regression test
- finish the existing production cutover and current release blockers
- preserve explicit reports of unsupported/lost source material

**Exit condition:** history is no longer disappearing merely because design work is still underway, and the current release/cutover foundation is clean enough for schema work.

### P1 — Events become history

- promote the portable event model to session-owned history
- add machine/git/native-session provenance
- make harness free text
- add artifacts with head/tail excerpts
- implement global historical import by time range
- make events lexically searchable
- add Hermes reader/import support
- keep local spools on non-Sleeper nodes and central canonical persistence on Sleeper

**Exit condition:** “When did we run X on Sleeper?” can be answered from preserved evidence without requiring an LLM summary to have mentioned it.
### P2 — One global engine

- generalize passages to cover session ranges as well as wiki pages
- enable global lexical + dense hybrid retrieval
- support explicit time/harness/machine/project/kind filters
- add the current-project candidate pool subject to evaluation
- expose expand / timeline / read
- generate deterministic session digests

**Exit condition:** project memory and historical memory use one coherent retrieval pipeline with progressive disclosure.

### P3 — Speed and relevance

- add the flat vector matrix/cache
- enable JEV behind the existing reranker boundary
- add normalized error fingerprints
- build and version the real-world retrieval evaluation set
- measure before adding any ANN index

**Exit condition:** relevance and latency improvements are demonstrated on repeatable evaluation rather than anecdote.

### P4 — Maintenance and wider coverage

- add the nightly dreaming/maintenance queue
- add additional important harness readers (Gemini and others) according to actual usage
- harden multi-machine import/reporting and operational dashboards as needed
- revisit ANN or other scaling architecture only if thresholds have been crossed

## 11. Explicit Non-Goals

Do not build by default:

- a separate vector database/service
- ANN before measured thresholds
- embeddings for every large log chunk
- raw transcripts as wiki pages
- a per-turn context-injection/personalization engine
- learning-to-rank or self-tuning ranking
- metadata score soup (“same machine ×1.2”, “same project ×1.3”, etc.)
- a new knowledge graph beyond useful existing entity/link machinery
- special tables for every tool type
- a first-class model-configuration history subsystem
- autonomous wiki rewriting outside existing approval controls
## 12. Worker Contract for Future Decomposition

Every implementation card derived from this roadmap should state:

1. **Roadmap phase and section**
2. **Capability/outcome being delivered**
3. **Existing mechanism being extended rather than duplicated**
4. **Relevant invariant(s) that must remain true**
5. **Acceptance test**
6. **Metric or evidence proving completion**
7. **Dependencies and what may run in parallel**
8. **Migration/rollback implications, if any**

A worker must not silently solve a local problem by:
- creating a parallel store/search path
- weakening provenance
- dropping full evidence
- adding an unmeasured ranking weight
- introducing a new service
- changing retention/security policy
- redefining product behavior

If the clean implementation appears to require one of those moves, the card returns for architecture review.

## 13. Baseline Snapshot — 2026-10-02

This section is a dated status snapshot, **not** a permanent invariant.

At the audit:
- production SessionMunch cutover card `t_2b0e89c4` was blocked
- release blocker `t_7165303b`: rustls advisory
- release blocker `t_5ba99218`: prove CI has no outbound network access
- release blocker `t_8d1bc67d`: CI drift / Rust hygiene
- live SessionMunch was reported on `127.0.0.1:49375`
- documentation still referenced MCP on `49374`, which was occupied by opencode
- production memory still primarily lived under `~/.local/share/ai-memory`

The working tree was moved on 2026-10-02 from the old evaluation implementation path to `~/projects/sessionmunch`, with a compatibility symlink left behind and nested Git worktree registrations repaired.

## 14. Definition of Success

SessionMunch succeeds when an agent can ask about something from months or years ago and move naturally from:

**relevant memory → exact historical session → nearby events → original command/output/evidence**

without depending on a previous LLM summary having guessed that the detail would matter later.

That is the North Star.
