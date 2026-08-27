# ragrig_bench — RAG Quality Evaluator

Systematically compare retrieval-augmented generation (RAG) setups on
*your* documents and *your* questions, and find the sweet spot between
answer quality, speed, and hardware requirements.

> **Early development.** `ragrig_bench` is pre-1.0: the TOML config
> schema, the CLI flags, and the report format may change between minor
> releases, with breaking changes.  Expect to touch up your configs when
> upgrading — the [changelog](CHANGELOG.md) lists every breaking change.

## Goals

When you set up a local RAG system you face a trade-off: bigger models give
better answers but need more RAM and run slower — and the same is true for
chunking strategies, ranking strategies, and context windows.  The only way
to find the right combination for **your** documents and **your** questions
is to test them side by side.

`ragrig_bench` exists to make that comparison cheap and reproducible:

- **One config, one report.**  A single TOML file describes the whole
  benchmark matrix — models, context sizes, corpora, (parser × chunker)
  pipelines, and rankers — and a Markdown report lays every combination out
  side by side for reading and diffing.
- **Your real documents, your real questions.**  Folders are indexed
  incrementally into one shared vector database; every cell retrieves from
  that database, so answers reflect the actual retrieval pipeline, not a
  synthetic approximation.
- **The structure is testable offline.**  Mock components (deterministic
  embedder, canned answers, synthetic corpora) run the entire matrix in
  milliseconds with no network — so the combinatorics, headers, and report
  layout can be validated before spending compute on real models.
- **Provenance-aware.**  Every chunk is stamped with the pipeline that
  built it (`PipelineProvenance.pipeline`), so pipelines sharing a chunker
  or a corpus can never silently mix during retrieval.

## Usage — the two-process pipeline

The workflow is two strictly sequential processes that share the TOML
config and a workspace:

1. **`ragrig-bench-ingest`** builds the vector database — it walks every
   requested (pipeline × corpus) combination and indexes it into one shared
   store in the workspace.
2. **`ragrig-bench-interact`** runs the benchmark matrix against that
   database and writes the Markdown report.

Provenance is the only seam between them: if a requested pipeline/corpus is
missing from the database, interact prints a helpful error pointing back at
the ingest step.

Minimal example — a config comparing two chat models over one document
folder:

```toml
queries = [
  "What are the key findings?",
  "Summarize the methodology.",
  "What conclusions does the author draw?",
]

corpus_dirs = ["coursework=~/Documents/coursework"]

[[agents]]
[agents.chat]
provider = "Ollama"
model = "qwen2.5:1.5b"
context_tokens = 4096

[[agents]]
[agents.chat]
provider = "Ollama"
model = "gemma2:latest"
context_tokens = 8192
```

Run the two processes in sequence:

```bash
ragrig-bench-ingest bench.toml
ragrig-bench-interact bench.toml > results.md
```

Both binaries default to the workspace `.ragrig_bench/`; pass
`-w/--workspace` to use another one, and `-o/--out` to write the log/report
to a file instead of stderr/stdout.  **One workspace per embedder**: the
database is bound to the embedder that built it (ragrig's embedder-metadata
guard).  Switching embedders in a workspace requires re-running ingest with
`--reindex` (see [Mock testing](#mock-testing)).

## Configuration

The benchmark file is TOML.  The `[agents.chat]`, `[embed]`, and `[parse]`
sections reuse ragrig's own library config types (`ChatConfig`,
`EmbedConfig`, `ParseConfig`) — the field vocabulary is identical to the
REPL's profiles, and omitted fields fall back to the library defaults.

### Top-level keys

| Key | Description |
|---|---|
| `queries` | Independent one-shot questions — every query runs with an empty transcript (no history between cells).  Alias: `questions`. |
| `chat` | A coherent multi-turn conversation — each message is sent with the transcript of the previous turns, and the answers feed back into it.  Exactly one of `queries` / `chat` must be set. |
| `corpus_dirs` | Named document corpora (see below).  At least one required. |
| `[[agents]]` | The chat agents forming the matrix.  At least one required. |
| `[[pipelines]]` | (Parser, chunker) pipelines to index and compare.  Empty = one default pipeline (full parser registry + `MarkdownChunker`). |
| `[[rankers]]` | Ranking strategies swept at query time.  Empty = the default hybrid RRF ranker. |
| `[embed]` | Embedding / retrieval settings (top-k, similarity threshold). |
| `[parse]` | Chunk size / overlap. |
| `[mock]` | Offline mock components (see [Mock testing](#mock-testing)). |

### `[[agents]]` — the chat dimension

```toml
[[agents]]
label = "small-model"          # optional display label
# answer = "canned {query}"    # optional: mock generator, replaces [agents.chat]
[agents.chat]
provider = "Ollama"            # "Ollama" | "Deepseek"
model = "qwen2.5:1.5b"         # ignored for Deepseek
context_tokens = 4096          # context budget for retrieved chunks
# deepseek_model = "deepseek-chat"   # Deepseek only
# deepseek_api_key = "..."            # Deepseek only (or DEEPSEEK_API_KEY env)
# context_size_mode = "Auto"          # "Auto" | "Forced"
# system_prompt_path = "prompt.md"    # custom system prompt ({context} placeholder)
# request_timeout_secs = 300          # per-request timeout, None = no timeout
# [agents.chat.params]                # generation overrides (all optional)
# temperature = 0.7
# top_p = 0.9
# max_tokens = 1024
# seed = 42
```

When an agent sets `answer = "…"`, a canned mock generator replies with
that template (the `{query}` placeholder is replaced by the user query) and
`[agents.chat]` is ignored — no network, deterministic output.  Without a
`label`, the heading is `<provider> / <model>` (or `mock` for canned
answers).

### `corpus_dirs` — the document dimension

Each entry is `name=path`; the name becomes the corpus part of the chunk
provenance and the heading in the report.

```toml
corpus_dirs = [
  "coursework=~/Documents/coursework",   # a folder on disk
  "book-pdf=@fixtures/pdf",              # embedded test fixture (offline)
  "book-html=@fixtures/html",            #   pdf | html | rmd
  "synth=@mock/12",                      # 12 synthetic in-memory documents
]
```

`@fixtures/<pdf|html|rmd>` extracts compile-time embedded test documents
(the same book in three formats) into a temp directory — no documents of
your own needed.  `@mock/<n>` synthesises `n` deterministic Markdown
documents cycling through fixed topics.  Folders are indexed recursively;
supported formats are PDF, EPUB, HTML, DOCX, and Markdown.

### `[[pipelines]]` — the ingestion dimension

```toml
[[pipelines]]
parser = "unpdf"              # PDF parser: unpdf | pdf-extract | pdfsink |
                              #   sloppy-pdf | kreuzberg | vision-pdf
chunker = "markdown"          # chunker: markdown | token | chunkedrs-markdown |
                              #   chunkedrs-recursive | chunkedrs-code |
                              #   chunkedrs-html (| feature-gated ones)

[[pipelines]]
chunker = "chunkedrs-markdown"   # parser omitted = full registry (first parser
                                 # that succeeds), the default
```

Every pipeline indexes all corpora into the shared store; interact queries
each pipeline separately via its pipeline id, so pipelines can share a
chunker or a corpus without mixing chunks.  Omit the section entirely for
the default pipeline.

### `[[rankers]]` — the query-time dimension

Ranking happens at query time, so rankers are swapped per cell and are not
part of ingestion or provenance.

```toml
[[rankers]]
name = "rrf"        # default hybrid BM25+vector Reciprocal Rank Fusion

[[rankers]]
name = "cosine"     # pure cosine (WeightedFusionRanker alpha = 1)

[[rankers]]
name = "bm25"       # pure BM25 (alpha = 0)

[[rankers]]
name = "weighted"   # fused, alpha = 0.5 (or set `alpha`)
alpha = 0.7

[[rankers]]
name = "mmr"        # Maximal Marginal Relevance re-ranking over RRF
lambda = 0.5        # diversity penalty (0.0–1.0)
```

### `[embed]` and `[parse]`

```toml
[embed]
provider = "Ollama"                  # "Ollama" | "Fastembed" (internal-embed build)
model = "nomic-embed-text:latest"
top_k = 50                           # chunks retrieved per search
similarity_threshold = 0.04          # applied to cosine scores BEFORE RRF fusion
# request_timeout_secs = 300

[parse]
pdf_parser = "Extract"               # ParseConfig backend: "Extract" (default) |
                                     #   "Unpdf" | "Sink" | "Internal" |
                                     #   "Kreuzberg" | "Vision" (feature-gated)
sloppy_pdf = false                   # never-panicking fallback parser
chunk_size = 1024                    # target tokens per chunk
chunk_overlap = 128                  # token overlap between chunks
```

Note the two name spaces, mirroring ragrig: `[embed] provider` and
`[agents.chat] provider` use the serde variant names (`"Ollama"`,
`"Deepseek"`, `"Extract"`), while `[[pipelines]] parser` and `chunker` use
the lowercase backend names (`"unpdf"`, `"markdown"`, …).

## Mock testing

For validating the matrix itself — headers, ordering, output structure,
combinatorics — without a running Ollama, mock components replace every slow
backend with a deterministic one.  Two ways in:

**1. The `--mock/-m` flag on both binaries** — run *any* real config
through the mock components, no separate config to maintain:

```bash
ragrig-bench-ingest  -m -w /tmp/mock_ws test_rankers.toml
ragrig-bench-interact -m -w /tmp/mock_ws -o report.md test_rankers.toml
```

`--mock` forces the deterministic bag-of-words embedder (word-overlap
cosine retrieval, zero network) and gives every agent without an explicit
`answer` the canned response `[mock] answer for: {query}`; agents with their
own `answer` keep it, and corpora are untouched.  Use the same flag on both
binaries — the workspace database is bound to one embedder.

**2. A dedicated mock config** — see `mock.toml`, which combines the three
config-level knobs:

- `[mock] embedder = true` — the bag-of-words embedder.
- an agent with `answer = "…"` — the canned generator.
- a `corpus_dirs` entry `name=@mock/<n>` — synthetic in-memory documents.

```bash
ragrig-bench-ingest -w /tmp/mock_ws mock.toml
ragrig-bench-interact -w /tmp/mock_ws -o report.md mock.toml
```

The whole (agents × pipelines × queries) matrix runs in milliseconds with
no network and no files on disk.

**Switching embedders in one workspace** (live → `--mock`, or back)
requires re-running ingest with `--reindex`: the old database cannot be
reused, and without it ingestion fails with an embedder-mismatch error
(`index was created with nomic-embed-text:latest (768 dims) but current
embedder is mock-bow (128 dims)`).  `--reindex` deletes the existing
database and rebuilds it from scratch.

## How it works

1. **Ingestion** walks every requested provenance (pipeline × corpus) and
   builds the combined vector database — one shared store in the workspace.
   Every chunk is stamped with the pipeline's id
   (`PipelineProvenance.pipeline`), so a `PipelineFilter` on
   `corpus` + `pipeline` selects exactly one pipeline's whole chunk set at
   query time — including shared chunks that parser/chunker-only filters
   cannot isolate on mixed corpora.  Indexing is incremental: re-runs skip
   unchanged documents.
2. **Interaction** runs the matrix against that database.  The outer loop
   cycles through agents so each model is loaded once and stays warm for
   all pipelines, corpora, and questions — no model thrashing.
3. For every (agent, pipeline, corpus, question) cell the program embeds
   the query, runs hybrid BM25+vector search through the filtered store,
   feeds the top chunks as context, and collects the answer.
4. History depends on the mode: one-shot `queries` keep every cell
   independent; `chat` carries the transcript forward turn by turn.

The report is Markdown structured for side-by-side comparison — pipeline,
corpus, and ranker headings only appear when the matrix has more than one
of each:

```markdown
# RAG Benchmark — 2026-08-27

## ollama / gemma2:latest

### unpdf · markdown

#### coursework

**Q1:** What are the key findings?

_ranker=rrf · t=0.04 · k=50 → 12 in context · 8192 ctx · emb 0.1s · gen 2.9s · total 3.2s_

**Retrieved** (ranked by store ranker):
1. **lecture-03.pdf** — 0.0325 (rrf) — _chunk snippet …_

…answer…
```

Each cell lists the retrieved chunks (document, score, snippet — sourced
from `RagResponse.retrieved`, so the query is embedded exactly once) and
per-stage timings (`emb` / `gen` / `total`) for comparing where time goes
across models and pipelines.

## Examples

### 1. Student: finding the sweet spot on a laptop

A semester's worth of PDFs in `~/uni/biology`, 8 GB RAM — which model runs
fast enough while still giving useful answers?

```toml
queries = [
  "Explain the Krebs cycle in simple terms.",
  "What is the role of mitochondria in apoptosis?",
  "Compare aerobic and anaerobic respiration.",
]

corpus_dirs = ["biology=~/uni/biology"]

[[agents]]
[agents.chat]
provider = "Ollama"
model = "qwen2.5:1.5b"
context_tokens = 2048

[[agents]]
[agents.chat]
provider = "Ollama"
model = "qwen2.5:1.5b"
context_tokens = 4096

[[agents]]
[agents.chat]
provider = "Ollama"
model = "gemma2:latest"
context_tokens = 8192
```

Read the report and pick the smallest model whose answers still hold up.
Smaller context windows mean less memory pressure and faster responses.

### 2. Analyst: making a large literature body accessible

200+ papers in `~/literature/renewables`, detailed technical questions,
32 GB RAM — larger models are affordable, retrieval tuning matters:

```toml
queries = [
  "What is the current state-of-the-art in perovskite solar cell efficiency?",
  "Compare the lifecycle carbon footprint of wind and solar.",
  "What are the main barriers to grid-scale battery storage?",
]

corpus_dirs = ["renewables=~/literature/renewables"]

[[agents]]
[agents.chat]
provider = "Ollama"
model = "gemma2:latest"
context_tokens = 8192

[[agents]]
[agents.chat]
provider = "Ollama"
model = "llama3.2:latest"
context_tokens = 8192

[embed]
model = "nomic-embed-text:latest"
top_k = 20
similarity_threshold = 0.1

[parse]
chunk_size = 512
chunk_overlap = 64
```

`top_k = 20` caps retrieval at 20 chunks per query (default 50);
`similarity_threshold = 0.1` raises the cosine cutoff (default `0.04`) so
only closely matching chunks are retrieved — the threshold applies to
cosine scores *before* RRF fusion, not to the fused RRF scores.

## Installation

You need Rust, Ollama, and the embedding model; then pull whichever chat
models you want to benchmark.

```bash
# 1. Install Rust: https://rustup.rs

# 2. Install Ollama: https://ollama.com/download

# 3. Pull the embedding model (required — not something you benchmark)
ollama pull nomic-embed-text

# 4. Pull the chat models you want to compare
ollama pull gemma2:latest
ollama pull qwen2.5:1.5b

# 5. Build
cargo build --release

# Quick test without installing (fully offline):
cargo run --release --bin ragrig-bench-ingest -- -w /tmp/bench_ws mock.toml
cargo run --release --bin ragrig-bench-interact -- -w /tmp/bench_ws -o /tmp/report.md mock.toml

# 6. Install the binaries somewhere on your PATH, e.g.:
mkdir -p ~/bin
cp target/release/ragrig-bench-ingest target/release/ragrig-bench-interact ~/bin/
```

If the binaries are "not found", add `~/bin` (or your install dir) to
`PATH` — e.g. `export PATH="$HOME/bin:$PATH"` in `~/.bashrc` /
`~/.zshrc` / `~/.profile` (Windows PowerShell:
`$env:Path = "$env:USERPROFILE\bin;$env:Path"`).  No C++ toolchain,
`cmake`, or `protoc` required — the default build is pure Rust.

## CLI Reference

Two binaries share the TOML config and the workspace.  Ingest first, then
interact.

```
Usage: ragrig-bench-ingest [OPTIONS] <CONFIG>

Arguments:
  <CONFIG>  Path to the TOML benchmark configuration file

Options:
  -w, --workspace <DIR>  Workspace directory for the vector store
                         [default: .ragrig_bench]
  -o, --out <FILE>       Write the ingestion log to this file instead
                         of stderr
  -m, --mock             Run with the offline mock embedder — test the
                         pipeline structure without a separate mock config
  -r, --reindex          Delete the existing vector database and rebuild
                         it from scratch — required after switching
                         embedders (the store is bound to the embedder
                         that built it)
  -h, --help             Print help
  -V, --version          Print version
```

```
Usage: ragrig-bench-interact [OPTIONS] <CONFIG>

Arguments:
  <CONFIG>  Path to the TOML benchmark configuration file

Options:
  -w, --workspace <DIR>  Workspace directory holding the vector store
                         built by ragrig-bench-ingest [default: .ragrig_bench]
  -o, --out <FILE>       Write the Markdown report to this file
                         instead of stdout
  -m, --mock             Run with offline mock components — deterministic
                         embedder, canned answers — to test the matrix
                         structure without a separate mock config
  -h, --help             Print help
  -V, --version          Print version
```

All benchmark parameters (models, top-k, thresholds, chunking, pipelines,
rankers) live in the TOML config; the CLIs only pick the file and the
workspace.

## Requirements

| Dependency | When needed |
|---|---|
| Rust 1.88+ | Build |
| Ollama | Runtime — provides chat and embedding models |
| `nomic-embed-text` | Embedding model (pull once: `ollama pull nomic-embed-text`) |
| One or more chat models | Whatever you want to benchmark |

The mock components need none of the runtime dependencies.

## License

[MIT](LICENSE) — Copyright (c) 2025 Martin Schmettow.
