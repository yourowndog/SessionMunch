# Acknowledgements & Lineage

SessionMunch builds upon foundational research, prior open-source initiatives, and lessons learned across the evolving agent memory landscape.

---

## Upstream Lineage: `ai-memory`

SessionMunch originates from and directly evolves the [ai-memory](https://github.com/akitaonrails/ai-memory) project created by Fabio Akita (AkitaOnRails). We express deep gratitude for the original architecture, design decisions, and robust foundation that made SessionMunch possible.

---

## Architectural Inspiration: jCodeMunch & jDocMunch

The project's identity and retrieval philosophy are inspired by **jCodeMunch** and **jDocMunch** — tools designed to ingest, chunk, index, and retrieve dense technical knowledge cleanly without unnecessary ceremony, fragile abstractions, or heavy infrastructure baggage.

---

## Influences & Related Prior Art

- **[Karpathy LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f):** The core thesis that agent memory is an actively compiled, cross-linked markdown encyclopedia rather than a raw dump of uncurated chat logs.
- **[agentmemory](https://github.com/rohitg00/agentmemory):** Early exploration of temporal separation between short-term context and long-term memory; SessionMunch serves as a high-performance, type-safe Rust successor.
- **[basic-memory](https://github.com/basicmachines-co/basic-memory):** Grounding the source of truth in ordinary markdown files on the local filesystem rather than proprietary binary databases.
- **[cognee](https://github.com/topoteretes/cognee):** Pipeline modularity and knowledge-graph entity extraction.
- **[Hermes Agent](https://github.com/NousResearch/hermes-agent):** The post-turn evaluation loops, curator review stage, and autonomous agent orchestration patterns.
- **[JEV](https://github.com/ourines/hermes-jev):** Fast heuristic scoring and decision reranking sidekick.
- **[A-MEM](https://arxiv.org/abs/2502.12110):** Zettelkasten-style atomic notes with link evolution and relational edges.
