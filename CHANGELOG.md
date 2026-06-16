# Changelog

All notable changes to `ragrig_bench` will be documented in this file.

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
