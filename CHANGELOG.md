# Changelog

All notable changes to `ragrig_bench` will be documented in this file.

## [Unreleased]

## [0.3.0] — 2026-08-27

### Changed

- **Rankers built via validated constructors** — ragrig's ranker fields are
  private now; `build_ranker` goes through `WeightedFusionRanker::new` /
  `MmrDiversityRanker::new`, so an out-of-range `alpha`/`lambda` in the
  config fails at startup with a clear message instead of at query time.
- **Scores are scale-tagged in the report** — every retrieved chunk prints
  its `ScoreKind` next to the score (`0.0323 (rrf)`, `0.87 (cosine)`), so
  scores from different rankers are no longer silently incomparable.
- **`MockEmbedder` implements the `Embedding` contract** — ragrig's
  `Embedder::embed` now returns `Vec<Embedding>` (`{ text, vector }`)
  instead of `Vec<(String, Vec<f32>)`; the mock returns the named struct.
- **Retrieval list sourced from `RagResponse.retrieved`** — ragrig's detailed
  responses now carry the store-ranked chunks, so the report prints them from
  the response instead of re-running `search_filtered` before every cell:
  the query is embedded **once** per cell (by the agent), not twice.
- **Per-stage timings in the meta line** — the report line gained
  `emb` / `gen` / `total` (from `RagResponse.timings`), so each cell shows
  where its time went in addition to the total.
- **Backend construction via library conversions**: `build_chat_agent` and
  `build_embedder` now go through `ChatAgentSpec::try_from(&ChatConfig)` /
  `EmbedderSpec::try_from(&EmbedConfig)` — the config→spec mapping moved into
  ragrig, and the bench no longer hand-rolls it or stringifies a `Debug` impl
  for feature-gated embedding providers.
- **Partial config tables handled by the library**: ragrig's config structs
  now carry container-level `#[serde(default)]`, so the bench's own
  `deserialize_with_defaults` TOML merge helper is deleted — `[agents.chat]`,
  `[embed]`, and `[parse]` fill omitted fields from the library defaults
  directly.
- **`MockResponder` uses the structured-prompt seam**: the current query is
  recorded by the agent's `prompt_hook` (`ChatPrompt.user`) into a shared
  slot read back at generation time — the `rsplit_once("<|user|>\n")` /
  `split("<|assistant|>")` wire-format parsing is gone, so the mock no longer
  depends on ragrig's prompt layout.
- **ragrig 1.0.0 API alignment**: Replaced the removed `embed_documents`
  entry point and the hand-rolled `.ragrig_embeddings.json` hash bookkeeping
  with `sync_corpus` over a `FolderCorpus::named` — new/changed documents are
  indexed, removed ones deleted, unchanged ones skipped via the store's
  manifest.
- **`RagAgent::builder().build()`** now returns `Result<RagAgent>` — both
  binaries propagate with `?`.
- **`RagResponse`**: renamed `sources` → `documents` (the field no longer
  carries scored chunks).
- **Spec enums**: `EmbedderSpec::Ollama` and `ChatAgentSpec::Ollama` gained
  `request_timeout_secs` — switched to the `EmbedderSpec::ollama()` /
  `ChatAgentSpec::ollama()` convenience constructors.

### Fixed

- **Threshold semantics**: `--similarity-threshold` documented a "hybrid RRF
  score" on the 0.0–0.033 RRF scale, while the library applies it to cosine
  similarity (0.0–1.0) *before* RRF fusion — the old default of 0.3 filtered
  out every chunk.  Default is now 0.04 (matching the library) with corrected
  help text; the `t=` line prints the raw value instead of rounding to one
  decimal.
- **Retrieval metrics**: `RagResponse` only reports a chunk *count*.  Both
  modes now run `search_similar` with the same embedder/top-k/threshold and
  print the ranker-scored chunk list (document, score, snippet) per query.
- **`top_k` default** aligned with the library default (50).
- **Context injection restored**: the agent was built with an empty system
  prompt, and the ragrig builder only substitutes `{context}` into prompts
  that contain the placeholder — so retrieved chunks were silently dropped
  and the benchmark compared bare-LLM answers.  The library's default
  document-assistant prompt is now used.
- **Fixture state wiped before indexing**: the embedded HTML fixture folder
  shipped stale ragrig state (`.ragrig_store`, `.ragrig_embeddings.json`,
  `.ragrig/`), which tripped ragrig 1.0's embedder-metadata guard on
  `@fixtures/html`.  Worked around by wiping state after extraction — later
  superseded by ragrig's build-script fixture staging (see Removed).

### Added

- **MIT license** — `LICENSE` file and the `license = "MIT"` package
  metadata (same terms and copyright holder as ragrig).
- **README overhaul** — reorganized around the goals, the two-process
  pipeline, the complete configuration reference (queries vs. chat, agents,
  corpora, pipelines, rankers, embed/parse, including the two backend name
  spaces), mock testing (`--mock`, `mock.toml`, `--reindex`), and the MIT
  license.
- **TOML configuration via library config types**: the benchmark file is now
  TOML, and the `[agents.chat]`, `[embed]`, and `[parse]` sections reuse
  ragrig's own `ChatConfig`, `EmbedConfig`, and `ParseConfig` — fields omitted
  fall back to the library defaults.  The `-c/-e/-k/-t` CLI flags are gone;
  every benchmark parameter lives in the config and the CLI keeps only
  `--workspace`.
- **Corpora instead of per-folder stores**: `corpus_dirs = ["name=path"]`
  indexes every corpus into ONE shared store in the workspace (no more
  `.ragrig_store` state written into document folders), queried per corpus via
  `PipelineFilter`.
- **Pipeline benchmarking**: `[[pipelines]]` pins (parser, chunker)
  combinations; every pipeline indexes all corpora into the shared store and
  is queried separately at runtime via its **pipeline id**
  (`PipelineProvenance.pipeline` → `PipelineFilter::for_pipeline`).
- **Mock components (offline mode)**: `[mock] embedder = true` swaps in a
  deterministic bag-of-words embedder (word-overlap cosine retrieval); an
  agent with `answer = "…"` (a `{query}` template) uses a canned mock
  generator built on `SimpleGenerator` + `MutexGenerator`; `@mock/<n>` corpus
  specs synthesise `n` in-memory Markdown documents.  `mock.toml` runs the
  full matrix with zero network, zero disk, in milliseconds.
- **`--out/-o <FILE>`**: the Markdown report can be written to a file instead
  of stdout; progress messages stay on stderr.
- **`--mock/-m`** on both binaries: run any real config through the offline
  mock components — the deterministic embedder plus a canned
  `[mock] answer for: {query}` response for every agent without an explicit
  `answer` — so the matrix structure can be tested without maintaining a
  separate mock config.  Agents with their own `answer` keep it.
- **`--reindex/-r`** on ingest: delete the existing vector database and
  rebuild it from scratch — the recovery path after switching embedders in
  one workspace (the store is bound to the embedder that built it, so
  without it ingestion fails with an embedder-mismatch error).  A no-op when
  no database exists yet.
- **Ranker as a config variable**: `[[rankers]]` sweeps ranking strategies at
  query time (`rrf`, `cosine`, `bm25`, `weighted` with alpha, `mmr` with
  lambda); the store's ranker is swapped per cell via `set_ranker` and the
  report shows a `##### ranker:` heading plus the ranker in the meta line.
- **Ingestion and interaction split into two binaries**: the workflow is now
  two strictly sequential processes sharing the TOML config and the workspace
  — `ragrig-bench-ingest` walks every requested provenance (pipeline ×
  corpus) and builds the combined vector database, `ragrig-bench-interact`
  runs the benchmark matrix against it.  Both binaries take `--out` (ingest:
  the ingestion log; interact: the Markdown report).  Provenance is the only
  seam: every chunk is stamped with the pipeline id (`sync_corpus_with_pipeline`)
  and a requested pipeline/corpus missing from the database produces a helpful
  error pointing back at the ingest step (`count_matching` gate, plus a
  "no vector database" error when the workspace store is absent).  Ingest
  also prints the workspace/embedder binding up front — the database is
  bound to one embedder (ragrig's embedder-metadata guard), so mock and
  live runs need separate workspaces.

### Removed

- `clean_fixture_state` — ragrig's build script now stages fixtures without
  `.ragrig*` state, so extracted fixtures are clean by construction.
- `ragrigio` — the stdin-piped single-query binary.  The main `ragrig-bench`
  tool covers that flow (`mock.toml` for quick offline runs, or a one-question
  TOML config); keeping one binary simplifies the crate.

## [0.2.0] — 2026-06-16

### Changed

- **ragrig v0.6 API decoupling**: Replaced `Args` construction with `ChunkConfig`
  and direct `&Path` parameters throughout. The ragrig library no longer accepts
  its CLI struct in function signatures.
- **Fixture extraction delegated to library**: Removed hand-rolled
  `TempFixtureDir`, `resolve_fixture_folder()`, and `FIXTURE_PREFIX` constant.
  Fixture extraction now calls `ragrig::fixtures::extract_fixtures()` which
  returns a `tempfile::TempDir` for automatic cleanup.
- **`ragrigio` binary**: Switched from constructing a dummy `Args` to using
  hardcoded `top_k` / `similarity_threshold` locals and a `ChunkConfig`.

### Removed

- `include_dir` dependency (no longer needed — fixture extraction lives in the
  ragrig library behind its `test-fixtures` feature).
- `TempFixtureDir` struct and its `Drop` impl.
- `resolve_fixture_folder()` helper function.
- `make_args()` function (replaced by `chunk_config()`).

### Added

- `tempfile` dependency (transitive dep of ragrig's fixture extraction).

## [0.1.0] — 2026-06-15

### Added

- Config-driven benchmark binary that evaluates ragrig retrieval quality across
  multiple document folders, query sets, and chat backends.
- JSON configuration schema with `queries`/`chat`, `folders`, and `agents` arrays.
- Two evaluation modes:
  - **Oneshot** (`queries[]`): independent queries, no conversation history.
  - **Chat** (`chat[]`): sequential multi-turn conversation with `TranscriptMemory`.
- Incremental per-folder indexing via SHA-256 change detection.
- Fixture document support: `@fixtures/pdf`, `@fixtures/html`, `@fixtures/rmd`
  extract compile-time embedded test documents to temp directories.
- Structured Markdown output to stdout with agent/query/folder headers, timing
  metadata, and chunk counts.
- CLI flags: `--context-size`, `--embed-model`, `--top-k`, `--similarity-threshold`.
- RAII temp directory cleanup via `TempFixtureDir`.
- `ragrigio` binary: simpler stdin-piped query pipeline for the current directory.
