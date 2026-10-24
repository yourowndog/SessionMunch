# Local embeddings

*2.0 item 5.* `SESSIONMUNCH_EMBEDDING_PROVIDER=local` runs sentence
embeddings **in-process** — no API key, no external server, no GPU
required. Pure-Rust NomicBert inference (candle) with
`nomic-embed-text-v1.5` (768-dim, RoPE + SwiGLU, Matryoshka
representation learning), the current standard for local CPU
embeddings.

**As of 2.0 this is the default**: an install with no
`embedding_provider` configured gets local embeddings automatically —
best-effort. The model downloads in the background on the first start
(hybrid search enables on the next restart), existing pages are
backfilled automatically, and a host that cannot fetch the model (or a
slim build without the feature) simply keeps the FTS-only behaviour
with a warning. Opt out with `embedding_provider = \"none\"`; an
explicitly configured provider is never overridden.

```toml
# config.toml — all optional as of 2.0
embedding_provider = "local"     # explicit: hard-fails if unavailable
# embedding_provider = "none"    # opt out of vectors entirely
```

## Why: semantic recall without data egress

Search is hybrid: FTS5 (lexical), entities, graph, and — when an
embedder is configured — vectors. The vector stream is what lets
*"how do we deploy"* find the page that says *"release procedure"*:
paraphrase recall, no shared keywords required. Before this provider,
enabling it cost one of two things:

- **an API key** (OpenAI / Voyage / Google): per-call spend, and every
  page body and every search query leaves your machine. For a system
  whose job is recording everything you do, that is not a small ask;
- **a self-hosted engine** (Ollama / LM Studio via `openai-compat`):
  keyless, but another server to run, warm, and keep on the same
  network as sessionmunch.

`local` removes both. Use it when any of these describe you:

- you run the zero-LLM path and want better recall without handing a
  provider your memory;
- the install is offline or air-gapped (drop the model files in
  manually — see below);
- a homelab/team server where "one binary, one volume" is the whole
  operational story and adding an Ollama sidecar just for embeddings
  is not worth it;
- you want reproducible retrieval: same model files (checksum-pinned),
  same vectors, forever — no provider-side model deprecations.

Stick with a hosted or self-hosted provider when you already run one
happily, want a larger/multilingual model, or want embedding compute
off the memory server's CPU.

### Migration from all-MiniLM-L6-v2 (384-dim)

Prior to v0.2 the local default was `all-MiniLM-L6-v2` (384 dim).
Existing `page_embeddings` rows stored with a different
`(provider, model, dim)` triple are correctly preserved and ignored by
the current embedder. To migrate old data, run
`sessionmunch purge-embeddings` and let the auto-backfill re-embed
under the new model; or reconfigure `embedding_model` and
`embedding_dim` explicitly to match your existing data.

## Why NomicBert?

Nomic Embed Text v1.5 is the current standard for local CPU embedding:
it matches or outperforms far larger models on the MTEB benchmark,
supports 2048-token contexts (vs 512 for the previous MiniLM model),
and uses a modern architecture (RoPE + SwiGLU) that the `candle`
pure-Rust inference engine supports natively.

## The model files

The binary does not bundle the model (~522 MB). On the first start
with `local` configured, the server fetches three files into
`<data_dir>/models/nomic-embed-text-v1.5/` — each verified against a
sha256 pinned in the source, so a drifted or tampered upstream file
fails loudly instead of silently changing every vector:

| file | sha256 (pinned 2026-10-01) |
|---|---|
| `model.safetensors` | `9e7d262b…` |
| `tokenizer.json` | `d241a60d…` |
| `config.json` | `9ab00bd9…` |

**Offline installs**: download the three files from
`https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main/`
on any machine and drop them into the directory above; the loader
verifies the same checksums and never touches the network. The model
is Apache-2.0.

## Coexistence and migration

Nothing is forced. `(provider, model, dim)` is stored on every
embedding row, and hybrid search ignores vectors whose triple does not
match the configured embedder — so local vectors sit beside any
provider vectors you already have, and switching back is a config
change. `sessionmunch embed --force` re-embeds a project under the
current provider when you want one consistent set.

Existing installs keep their configured provider; `local` is opt-in.
Fresh-install defaults are decided by benchmark numbers, not vibes —
see `docs/benchmarks/` for the zero-LLM vs local-embeddings
LongMemEval rows.

## Operational notes

- Inference is CPU, off the async runtime (`spawn_blocking`);
  first-load reads ~522 MB into memory once per server process.
- The `local-embeddings` cargo feature (default on) carries the ML
  dependency tree; a slim build can disable it and keep every other
  provider.
- Tokenization truncates at 2048 tokens — the model's trained
  positional limit; page bodies beyond that contribute their head,
  same contract as the hosted embedders.
- The old `all-MiniLM-L6-v2` model (384-dim) is still available by
  explicitly configuring `embedding_model = "all-MiniLM-L6-v2"` and
  `embedding_dim = 384`, but it must be downloaded manually into
  `<data_dir>/models/all-MiniLM-L6-v2/`.