# TODO

Remaining work for `ragrig_bench` (ragrig 1.0.0).  The `RagAgent` migration,
TOML configuration, corpus-based storage, pipeline sweeps, the mock
components, and the ingestion/interaction split (`ragrig-bench-ingest` →
`ragrig-bench-interact`, provenance as the only seam) are done — `mock.toml`
runs the full matrix offline in milliseconds.

## Benchmark dimensions

- **Ranker as a config variable** — make the ranker a benchmark dimension
  like `chunker` / `parser`, including its parameters:
  `HybridRrfRanker { k }`, `WeightedFusionRanker { alpha }`,
  `MmrDiversityRanker { lambda }`, `LlmReranker` — and the search parameters
  (top-k, similarity threshold) — instead of the single hard-wired default
  ranker.
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

- **Memory strategy instead of `queries`/`chat` modes** — remove the
  external variation between coherent chat and one-shot questions: one
  question list plus a memory-strategy setting drives whether turns
  accumulate history.  One-shot questions = `NoopMemory` (empty transcript
  per query, the current `queries` behaviour); coherent chat carries the
  transcript forward (the current `chat` behaviour).
- **`corpus_urls` / `UrlCorpus`** — support URL corpora like the REPL does.
- **`context_size_mode`** — wire `ChatConfig.context_size_mode` through to
  generation (currently only `context_tokens` is used).

## Housekeeping

- **Stale example configs** — the `test_*.json` files are pre-TOML and no
  longer parse; convert or delete them.  The `*.md` files in the repo root
  are old run outputs.
- **`chrono_lite()`** — replace the `date` subprocess with a real date crate
  or a `std::time`-derived timestamp.
