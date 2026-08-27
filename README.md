# ragrig_bench — RAG Quality Evaluator

Systematically compare local LLMs for retrieval-augmented generation to find
the sweet spot between capability, speed, and hardware requirements for your
specific use case.

## Purpose

When you set up a local RAG system you face a trade-off: bigger models give
better answers but need more RAM and run slower.  The only way to find the
right model for *your* documents and *your* questions is to test them
side-by-side.

`ragrig_bench` does exactly that.  Give it a set of questions, a set of
document folders, and a list of models — it runs every combination and
produces a Markdown report you can read and compare.

## How It Works

You write a TOML config file describing the benchmark matrix.  The `chat`,
`embed`, and `parse` sections reuse ragrig's own library config types
(`ChatConfig`, `EmbedConfig`, `ParseConfig`), so the field vocabulary is
identical to the REPL's profiles — omitted fields use the library defaults.

```toml
queries = [
  "What are the key findings?",
  "Summarize the methodology.",
  "What conclusions does the author draw?",
]

corpus_dirs = ["coursework=/home/mart/Documents/coursework"]

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

[embed]
model = "nomic-embed-text:latest"
top_k = 50
similarity_threshold = 0.04

[parse]
chunk_size = 1024
chunk_overlap = 128
```

Then run the two processes in sequence — ingestion builds the vector
database, interaction runs the benchmark against it:

```bash
ragrig-bench-ingest bench.toml
ragrig-bench-interact bench.toml > results.md
```

## Installation

You need Rust, Ollama, and the embedding model.  Then pull whichever chat
models you want to benchmark.

```bash
# 1. Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. Install Ollama — https://ollama.com/download

# 3. Pull the embedding model (required — not something you benchmark)
ollama pull nomic-embed-text

# 4. Pull the chat models you want to compare
ollama pull gemma2:latest
ollama pull llama3.2:latest
ollama pull qwen2.5:1.5b

# 5. Build
cargo build --release

# Quick test without installing:
cargo run --release --bin ragrig-bench-ingest -- -w /tmp/bench_ws mock.toml
cargo run --release --bin ragrig-bench-interact -- -w /tmp/bench_ws -o /tmp/report.md mock.toml

# 6. Install to ~/bin
mkdir -p ~/bin
cp target/release/ragrig-bench-ingest target/release/ragrig-bench-interact ~/bin/

# 7. Verify
~/bin/ragrig-bench-ingest --help
~/bin/ragrig-bench-interact --help
```

The binaries land in `~/bin`.  No C++ toolchain, no `cmake`, no `protoc`
— pure Rust.

If `ragrig-bench-ingest --help` says "command not found", `~/bin` isn't on
your `PATH`.  Here's how to fix it.

**Check whether it's already set:**

```bash
echo $PATH | grep --color "$HOME/bin"   # Linux / macOS / WSL
```

```powershell
$env:Path -split ";" | Select-String "$env:USERPROFILE\bin"   # Windows PowerShell
```

**Add it if missing:**

*Linux / WSL / macOS* — append to your shell config (`~/.bashrc`, `~/.zshrc`,
or `~/.profile` depending on your shell):

```bash
echo 'export PATH="$HOME/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

*Windows (PowerShell)* — add permanently for your user:

```powershell
[Environment]::SetEnvironmentVariable(
    "Path",
    "$env:USERPROFILE\bin;" + [Environment]::GetEnvironmentVariable("Path", "User"),
    "User"
)
$env:Path = "$env:USERPROFILE\bin;$env:Path"   # apply to current session
```

Restart your terminal after the permanent change, or `source` the config file
on Linux/macOS to apply immediately.



## What happens under the hood:

1. **Ingestion** (`ragrig-bench-ingest`) walks every requested provenance
   (pipeline × corpus) and builds the combined vector database — one shared
   store in the workspace (`--workspace`, default `.ragrig_bench/`).  Every
   chunk is stamped with the pipeline's id (`PipelineProvenance.pipeline`),
   so a `PipelineFilter` on `corpus` + `pipeline` selects exactly one
   pipeline's whole chunk set at query time — including shared chunks that
   parser/chunker-only filters cannot isolate on mixed corpora.  Indexing is
   incremental — re-runs skip unchanged documents.
   **One workspace per embedder**: the database is bound to the embedder
   that built it (ragrig's embedder-metadata guard), so keep mock and live
   runs in separate workspaces.
2. **Interaction** (`ragrig-bench-interact`) runs the benchmark matrix
   against that database.  Provenance is the only seam between the two
   processes: a requested provenance missing from the database produces a
   helpful error pointing back at the ingest step.
3. The outer loop cycles through agents so each model is loaded once and stays
   warm for all pipelines, corpora, and questions — no thrashing.
4. For every (agent, pipeline, corpus, question) combination the program
   embeds the query, runs hybrid BM25+vector search through the filtered
   store, feeds the top chunks as context, and collects the answer.
5. History is off — every query is independent, results are not tainted by
   previous answers.

The output is a Markdown file structured for side-by-side comparison:

```markdown
# RAG Benchmark — 2026-06-14

## ollama / gemma2:latest

### unpdf · markdown

#### coursework

**Q1:** What are the key findings?
…answer…
```

Pipeline and corpus headings only appear when the matrix has more than one
of each.

### Benchmarking pipelines

Each `[[pipelines]]` entry pins the document parser and the chunker used to index
every corpus; all pipelines share the one store and are queried separately
via pipeline provenance.  Omit the section to run the default pipeline (the
full parser registry + `MarkdownChunker`).  Chunker names come from
`ragrig::available_chunkers()` — `markdown`, `token`, `chunkedrs-markdown`,
`chunkedrs-recursive`, `chunkedrs-code`, `chunkedrs-html`, …; parser names are
`unpdf`, `pdf-extract`, `pdfsink`, `sloppy-pdf`, `kreuzberg`, `vision-pdf`.

```toml
[[pipelines]]
parser = "unpdf"
chunker = "markdown"

[[pipelines]]
parser = "unpdf"
chunker = "chunkedrs-markdown"
```

### Benchmarking rankers

Ranking happens at query time, so rankers are an interaction-side dimension
(`[[rankers]]`), swapped into the store per cell — not part of ingestion or
provenance.  Omit the section for the default hybrid RRF ranker.  Available:
`rrf` (default), `cosine` (α=1), `bm25` (α=0), `weighted` (α, default 0.5),
and `mmr` (diversity `lambda` over RRF).

```toml
[[rankers]]
name = "rrf"

[[rankers]]
name = "cosine"
```

### Mock mode — fast, offline combinatorics

For testing the matrix itself (headers, ordering, output structure) without a
running Ollama, the mock components replace every slow backend with a
deterministic one:

- `[mock] embedder = true` — a deterministic bag-of-words embedder;
  retrieval becomes word-overlap cosine and runs offline.
- an agent with `answer = "..."` — a canned mock generator; `{query}` is
  replaced by the user query, so each cell is distinguishable.
- a `corpus_dirs` entry `name=@mock/<n>` — `n` synthetic in-memory Markdown
  documents cycling through fixed topics.

A dedicated `mock.toml` exercises all three.  Alternatively, run **any** real
config through the mock components with the `--mock` / `-m` flag on both
binaries — no separate mock config to maintain:

```bash
# Same config, same matrix, same report structure — no Ollama, no network:
ragrig-bench-ingest  -m -w /tmp/mock_ws test_rankers.toml
ragrig-bench-interact -m -w /tmp/mock_ws -o report.md test_rankers.toml
```

`--mock` forces the mock embedder and gives every agent without an explicit
`answer` a canned `[mock] answer for: {query}` response; agents with their
own `answer` keep it, and corpora are untouched.  Use the same flag on both
binaries — the workspace store is bound to one embedder.  **Switching
embedders in one workspace** (live → `--mock`, or back) requires re-running
ingest with `--reindex`: the old database cannot be reused, and without it
ingestion fails with an embedder-mismatch error (e.g. `index was created
with nomic-embed-text:latest (768 dims) but current embedder is mock-bow
(128 dims)`).

See `mock.toml` — the whole (agents × pipelines × queries) matrix runs in
milliseconds with no network and no files on disk:

```bash
ragrig-bench-ingest -w /tmp/mock_ws mock.toml
ragrig-bench-interact -w /tmp/mock_ws mock.toml
```

### Built-in test fixtures

To evaluate models without your own documents, use the `@fixtures/` prefix.
It extracts compile-time embedded test documents (the same book in PDF, HTML,
and R Markdown) into a temp directory:

```toml
queries = ["What is R?"]
corpus_dirs = ["book-pdf=@fixtures/pdf", "book-html=@fixtures/html", "book-rmd=@fixtures/rmd"]

[[agents]]
[agents.chat]
provider = "Ollama"
model = "gemma2:latest"
```

### Quick start (offline)

`mock.toml` runs a fully offline matrix — mock generator, mock embedder,
synthetic corpus — no Ollama, no documents on disk.  Ingest first, then
interact:

```bash
ragrig-bench-ingest -w /tmp/bench_ws mock.toml
ragrig-bench-interact -w /tmp/bench_ws -o report.md mock.toml
```

## Examples

### 1. Student: finding the sweet spot on a laptop

You have a semester's worth of PDFs in `~/uni/biology` and a laptop with
8 GB RAM.  You want to know: which model runs fast enough while still giving
useful answers?

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

Run it, read the Markdown report, and pick the smallest model whose answers
still hold up.  Pay attention to the smaller context sizes — smaller windows
mean less memory pressure and faster responses.

### 2. Analyst: making a large literature body accessible

You maintain a collection of 200+ papers in `~/literature/renewables` and
need to answer detailed technical questions.  You have a workstation with
32 GB RAM and can afford larger models.

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
```

With a large document set, also experiment with the retrieval and chunking
parameters in the `[embed]` and `[parse]` sections:

```toml
[embed]
model = "nomic-embed-text:latest"
top_k = 20
similarity_threshold = 0.1

[parse]
chunk_size = 512
chunk_overlap = 64
```

`top_k = 20` caps retrieval at 20 chunks per query (the default is 50).
`similarity_threshold = 0.1` raises the cosine similarity threshold (default
`0.04`) so only closely matching chunks are retrieved — the threshold is
applied to cosine scores *before* RRF fusion, not to the fused RRF scores.

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

All benchmark parameters (models, top-k, thresholds, chunking, pipelines)
live in the TOML config; the CLIs only pick the file and the workspace.

## Requirements

| Dependency | When needed |
|---|---|
| Rust 1.94+ | Build |
| Ollama | Runtime — provides chat and embedding models |
| `nomic-embed-text` | Embedding model (pull once: `ollama pull nomic-embed-text`) |
| One or more chat models | Whatever you want to benchmark |
