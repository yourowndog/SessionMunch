# SessionMunch Landing Page

> Canonical public-facing copy and content structure for the SessionMunch website.
> Keep this aligned with the README and shipped behavior. Do not turn future plans into present-tense features.

<p align="center">
  <img alt="SessionMunch — Stop pasting handoff prompts" src="sessionmunch-hero.webp" width="1000">
</p>

The original-quality hero master is `docs/sessionmunch-hero.png`; the WebP is the lightweight web/README derivative.

---

## Hero

# Stop pasting handoff prompts.

**One persistent project memory for your coding agents — across sessions, models, tools, and machines.**

SessionMunch captures what your agents learn, turns it into durable searchable knowledge, and makes it available to the next agent without dragging an entire transcript back into context.

**Primary CTA:** View / install from GitHub
**Secondary CTA:** See how it works

Small release note: **v0.1.0 is currently a release candidate.** Source installation works today; packaged release channels arrive with the first public tag.

---

## The problem
Your coding agent can reason brilliantly for three hours and then wake up tomorrow like the project was introduced at a networking event.

The useful stuff is rarely just source code or just chat history. It is the layer in between:

- why a decision was made;
- what already failed;
- the bug that looked unrelated but absolutely was not;
- conventions the repo depends on;
- what the last agent finished;
- what still needs doing.

SessionMunch makes that layer persistent.

> **Chat history is evidence. Durable project knowledge is the product.**

---

## The mental model

If jCodeMunch makes **code** searchable and jDocMunch makes **documentation** searchable, SessionMunch does the same for the **history of work around both**.

It is not a bigger clipboard. It is a shared project memory service.

Claude Code can learn something. Codex can retrieve it later. Hermes can use the same memory through its native provider. OpenCode or another MCP client can ask the same database. The model can change; the project memory does not have to.

---

## What each piece gets you
### Plain Markdown source of truth
The durable knowledge is ordinary Markdown.

**You get:** inspectability, Git history, easy backups, human edits, and no proprietary memory hostage situation.

### Rebuildable SQLite index
SQLite is the fast card catalog over the Markdown, not the owner of it.

**You get:** fast retrieval with a database that can be rebuilt using `sessionmunch reindex`.

### FTS5 full-text search
Looks for the actual words and phrases you used.

**You get:** excellent recall for error messages, symbols, commands, filenames, and exact terminology.

### Embeddings
Turns text into vectors so related meanings can be found even when the words differ.

**You get:** a search for “login token bug” can still find a note about an “authentication credential failure.”

### Entities, graph links, and passages
Tracks useful relationships and indexes long notes as smaller retrievable sections.

**You get:** the relevant paragraph and its connections instead of twelve screens of transcript archaeology.

### Reciprocal Rank Fusion
Lets multiple retrieval methods vote instead of betting the entire memory system on one scorer.

**You get:** exact-match, semantic, entity, graph, and passage signals can reinforce one another.

### Briefs, handoffs, MCP, HTTP, and lifecycle hooks
Connect the memory to actual agent workflows.

**You get:** automatic startup context where supported, explicit search everywhere MCP works, and one shared service instead of one memory silo per agent.
---

## How another agent knows

There is no telepathy involved, disappointingly.

Lifecycle hooks tell SessionMunch what project and session are active and feed it bounded events worth remembering. MCP tools let agents query and update memory explicitly. Startup hooks can inject a small relevant brief. HTTP exposes the same runtime to other clients. Hermes can connect through its native MemoryProvider.

All of those are doors into the **same memory service**.

The result is the behavior users actually want:

**work → learn → end session → switch agent → continue**

without the traditional ritual of:

**summarize everything → copy giant prompt → paste giant prompt → discover three missing details → sigh**

---

## No LLM required. Really.

SessionMunch separates **remembering** from **generating**.

A generative LLM is optional. Without one, SessionMunch can still capture sessions, preserve Markdown, search with FTS5/entity/graph signals, build passage indexes, generate handoffs, and serve memory over MCP/HTTP.

By default it can also use Nomic Embed Text v1.5 locally in-process for semantic search. An embedding model is not a chat model; it maps text into vectors so similar meanings can be found.

Want zero model inference? Disable embeddings too.

Want richer distilled notes? Add a local or hosted summarizer.

**Bring your own intelligence; keep your memory.**

---

## Reranking
SessionMunch first retrieves broadly using its normal hybrid search. Optional reranking gets only a bounded shortlist and may improve the final order.

### v0.1
Optional LLM reranking is supported for project/scoped searches and is fail-open. Timeouts, malformed results, provider failures, or saturation preserve the normal fused order.

### Next: JEV
The generic reranker boundary and JEV-oriented benchmark scaffolding already exist, but **JEV is not a user-facing v0.1 switch**.

The intended integration is simple:

**SessionMunch recalls → JEV judges the shortlist → SessionMunch keeps working if JEV is unavailable.**

Storage and baseline recall never depend on the optional judge.

---

## Works with the agent you are already using

SessionMunch is agent-neutral by design.

First-class or supported paths include Claude Code, OpenAI Codex, Hermes Agent, OpenCode, Cursor, Windsurf, Gemini CLI, Aider, Pi, Crush, Devin, and other MCP-capable clients.

The important compatibility claim is not the logo wall. It is this:

> **If your agent speaks MCP, SessionMunch already has a door it can walk through.**

Lifecycle hooks make integrations nicer where the harness exposes them; MCP keeps the core portable.

---

## Privacy and ownership
The default posture is local and conservative:

- loopback-only networking by default;
- zero product telemetry;
- secret sanitization before durable writes;
- local embeddings available without API keys;
- no summarizer enabled unless the operator chooses one;
- plain Markdown remains the durable source of truth.

Remote embeddings, summarization, or LLM reranking are explicit choices and create provider egress. Say that plainly.

No sanitizer is magic. A machine allowed to read your repository should still be treated like a machine allowed to read your repository.

---

## Lineage

SessionMunch directly descends from Fabio Akita's **ai-memory** project and retains the upstream MIT attribution.

Say this clearly and appreciatively. Do not frame SessionMunch by attacking its ancestor.

The distinction to communicate is product direction: SessionMunch is evolving toward a standalone, shared, model-agnostic memory runtime with portable MCP/HTTP access, human-readable durable storage, lifecycle automation, migration/operations tooling, and first-class Hermes integration.

Additional prior art and acknowledgements belong in `docs/acknowledgements.md`.

---

## Closing CTA

### Your model can forget. Your project does not have to.

Install SessionMunch, connect an agent, and make the next session somebody else's problem.

**CTA:** View SessionMunch on GitHub
