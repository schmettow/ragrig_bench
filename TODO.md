# TODO

Remaining work for `ragrig_bench` (ragrig 1.0.0).  The `RagAgent` migration,
TOML configuration, corpus-based storage, and pipeline sweeps are done.

## Mock components — do first

- **Static-response mocks** — the benchmark matrix (agents × pipelines ×
  corpora × queries) costs a real Ollama round-trip per cell; a mock mode
  makes combinatorial testing fast and CI-able.  ragrig's traits make this
  trivial:
  - **mock `Generator`** — `ragrig::SimpleGenerator` (sync
    `respond(prompt) -> String`) wrapped in `MutexGenerator`, or a full
    `Generator` impl with canned answers per agent;
  - **mock `Embedder`** — deterministic toy vectors (e.g. hash-based), so
    retrieval + `RagAgent` paths run offline (see `examples/meta_citation`);
  - **mock `Corpus`** — static in-memory `Document`s, so indexing paths are
    testable without fixtures/tempdirs.
  - For generation-only runs, `EmbedderSpec::None` / `NoopEmbedder` already
    exist as the built-in "retrieval off" seam.

## Benchmark dimensions

- **Ranker sweeps** — add a ranker axis via `RagAgentBuilder::ranker()`:
  `HybridRrfRanker` (default), `WeightedFusionRanker`, `MmrDiversityRanker`,
  `LlmReranker`.
- **Embedder as a pipeline dimension** — blocked by the store's
  embedder-metadata guard (one embedder per store); needs per-embedder
  stores.  Decide whether the config gains multiple stores or the embedder
  stays global.

## Observability & robustness

- **Progress + cancellation** — `sync_corpus_with_progress` + `ProgressEvent`
  for per-file indexing output; `Cancelled` for clean Ctrl-C mid-embed.
- **Typed errors in the report** — downcast `RagrigError`
  (`EmbeddingMismatch`, `NoDocumentsFound`, `StoreCorrupt`,
  `EmbedModelNotFound`) so failures render as actionable notes instead of
  raw `anyhow` strings.
- **Workspace hygiene** — a `--fresh` flag, and pruning of stale
  `name::pipeline` corpora when pipelines are removed from the config.

## Config

- **`corpus_urls` / `UrlCorpus`** — support URL corpora like the REPL does.
- **`context_size_mode`** — wire `ChatConfig.context_size_mode` through to
  generation (currently only `context_tokens` is used).

## Housekeeping

- **Stale example configs** — the `test_*.json` files are pre-TOML and no
  longer parse; convert or delete them.  The `*.md` files in the repo root
  are old run outputs.
- **`chrono_lite()`** — replace the `date` subprocess with a real date crate
  or a `std::time`-derived timestamp.
- **`ragrigio`** — still opens its store in the CWD (its documents live
  there, so it works) but could gain the same TOML/workspace treatment.
