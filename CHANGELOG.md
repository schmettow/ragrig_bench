# Changelog

All notable changes to `ragrig_bench` will be documented in this file.

## [Unreleased]

### Changed

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

- **TOML configuration via library config types**: the benchmark file is now
  TOML, and the `[agents.chat]`, `[embed]`, and `[parse]` sections reuse
  ragrig's own `ChatConfig`, `EmbedConfig`, and `ParseConfig` — fields omitted
  fall back to the library defaults.  The `-c/-e/-k/-t` CLI flags are gone;
  every benchmark parameter lives in the config and the CLI keeps only
  `--workspace`.
- **Corpora instead of per-folder stores**: `corpus_dirs = ["name=path"]`
  indexes every corpus into ONE shared store in the workspace (no more
  `.ragrig_store` state written into document folders), queried per corpus via
  `PipelineFilter::for_corpus`.
- **Pipeline benchmarking**: `[[pipelines]]` pins (parser, chunker)
  combinations; every pipeline indexes all corpora into the shared store and
  is queried separately at runtime via pipeline provenance.  Each
  (corpus, pipeline) pair is stored under a pipeline-scoped corpus name, so a
  corpus-only filter selects exactly one pipeline's chunks.
- **Mock components (offline mode)**: `[mock] embedder = true` swaps in a
  deterministic bag-of-words embedder (word-overlap cosine retrieval); an
  agent with `answer = "…"` (a `{query}` template) uses a canned mock
  generator built on `SimpleGenerator` + `MutexGenerator`; `@mock/<n>` corpus
  specs synthesise `n` in-memory Markdown documents.  `mock.toml` runs the
  full matrix with zero network, zero disk, in milliseconds.
- **`--out/-o <FILE>`**: the Markdown report can be written to a file instead
  of stdout; progress messages stay on stderr.

### Removed

- `clean_fixture_state` — ragrig's build script now stages fixtures without
  `.ragrig*` state, so extracted fixtures are clean by construction.

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
