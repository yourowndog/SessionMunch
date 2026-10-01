# Models Without the Mystique

SessionMunch can use models, but it does not need a generative LLM to be a useful memory system.

There are three different jobs that are easy to blur together:

1. **Retrieval** — finding the right old information.
2. **Summarization** — turning noisy session evidence into cleaner durable knowledge.
3. **Reranking** — taking a small set of already-good search results and deciding which should come first.

They are separate on purpose. You can change one without rebuilding your entire memory stack.

---

## Embeddings: search by meaning

Full-text search is excellent when the query uses the same words as the stored note. Embeddings cover the other case.

An embedding model turns a piece of text into a vector: effectively a set of coordinates in a high-dimensional space. Texts with related meanings tend to land near one another. That lets a search for **"login token bug"** find a note about an **"authentication credential failure"** even though the important words are different.

SessionMunch can run **Nomic Embed Text v1.5 locally in-process** for this job. No chat model or API key is required. If you prefer a hosted embedding provider, OpenAI, Voyage, Google, and OpenAI-compatible endpoints are supported.

If you want no embedding inference at all, disable it. FTS5, entities, graph signals, and passage retrieval still work.

---

## Summarization: optional synthesis

A summarizer is where a generative LLM can help turn a long session into concise concepts, gotchas, decisions, and rules.

It is **optional**. With no summarizer configured, SessionMunch still captures, stores, indexes, searches, and hands off memory. Adding a summarizer improves synthesis; it does not unlock the basic memory system.

Summarizers can be local through an OpenAI-compatible server or remote through a supported provider. The durable Markdown format remains the same either way.

---

## Reranking: one last look at the shortlist

The normal retrieval pipeline already combines several signals. Optional reranking happens only after that pipeline has produced a bounded candidate set.

In **v0.1**, the live optional reranker is an LLM relevance pass configured with `SESSIONMUNCH_RERANKER=llm`. It is fail-open: if the provider fails, times out, returns malformed scores, or is saturated, SessionMunch keeps the normal fused order.

### JEV is planned next

The codebase already has a generic reranker interface and JEV-oriented fail-open benchmark scaffolding. A user-facing `jev` reranker is **not enabled in v0.1**.

The planned design is intentionally boring in the good way: SessionMunch does storage and broad recall; JEV receives a small shortlist and helps reorder it; if JEV is absent or unhappy, memory retrieval keeps working.

---

## So what should I configure?

Start with the defaults and add complexity only when you can name the problem it solves.

- Need semantic recall? Keep local embeddings on.
- Need richer distilled knowledge? Add a summarizer.
- Need absolutely no model inference? Disable embeddings and summarization.
- Need a particular provider or local model? Swap it in without changing the memory format.

For copy-paste configuration examples and hardware notes, see [`models.md`](models.md). For the actual knobs, see [`configuration.md`](configuration.md).
