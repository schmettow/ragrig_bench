# ragrig_bench — RAG Quality Evaluator

Systematically compare local LLMs for retrieval-augmented generation to find
the sweet spot between capability, speed, and hardware requirements for your
specific use case.

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
cargo build --release --bin ragrig_bench --bin ragrigio

# Quick test without installing:
cargo run --release --bin ragrigio <<< "What is this document about?"

# 6. Install to ~/bin
mkdir -p ~/bin
cp target/release/ragrig_bench target/release/ragrigio ~/bin/

# 7. Verify
~/bin/ragrig_bench --help
```

Binaries land in `~/bin`.  No C++ toolchain, no `cmake`, no `protoc`
— pure Rust.

If `ragrig_bench --help` says "command not found", `~/bin` isn't on your
`PATH`.  Here's how to fix it.

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

## Purpose

When you set up a local RAG system you face a trade-off: bigger models give
better answers but need more RAM and run slower.  The only way to find the
right model for *your* documents and *your* questions is to test them
side-by-side.

`ragrig_bench` does exactly that.  Give it a set of questions, a set of
document folders, and a list of models — it runs every combination and
produces a Markdown report you can read and compare.

## How It Works

You write a JSON config file describing the benchmark matrix:

```json
{
  "questions": [
    "What are the key findings?",
    "Summarize the methodology.",
    "What conclusions does the author draw?"
  ],
  "folders": [
    "/home/mart/Documents/coursework"
  ],
  "agents": [
    { "backend": "ollama", "model": "qwen2.5:1.5b", "context_size": 4096 },
    { "backend": "ollama", "model": "gemma2:latest", "context_size": 8192 },
    { "backend": "ollama", "model": "llama3.2:latest", "context_size": 4096 }
  ]
}
```

Then run:

```bash
ragrig_bench bench.json > results.md
```

**What happens under the hood:**

1. Each folder is indexed once (incremental — re-runs skip unchanged files).
2. The outer loop cycles through agents so each model is loaded once and stays
   warm for all questions and folders — no thrashing.
3. For every (agent, question, folder) combination the program embeds the
   query, runs hybrid BM25+vector search, feeds the top chunks as context,
   and collects the answer.
4. History is off — every query is independent, results are not tainted by
   previous answers.

The output is a Markdown file structured for side-by-side comparison:

```markdown
# RAG Benchmark — 2026-06-14

## ollama / gemma2:latest

### Q1: What are the key findings?

#### coursework
…answer…

### Q2: Summarize the methodology.
…

## ollama / qwen2.5:1.5b

### Q1: What are the key findings?
…
```

### Built-in test fixtures

To evaluate models without your own documents, use the `@fixtures/` prefix.
It extracts compile-time embedded test documents (the same book in PDF, HTML,
and R Markdown) into a temp directory:

```json
{
  "questions": ["What is R?"],
  "folders": ["@fixtures/pdf", "@fixtures/html", "@fixtures/rmd"],
  "agents": [
    { "backend": "ollama", "model": "gemma2:latest" },
    { "backend": "ollama", "model": "llama3.2:latest" }
  ]
}
```

### Quick single query

The crate also includes `ragrigio` — a minimal binary that indexes the current
directory and answers one question from stdin:

```bash
echo "What is the central argument?" | ragrigio
```

## Examples

### 1. Student: finding the sweet spot on a laptop

You have a semester's worth of PDFs in `~/uni/biology` and a laptop with
8 GB RAM.  You want to know: which model runs fast enough while still giving
useful answers?

```json
{
  "questions": [
    "Explain the Krebs cycle in simple terms.",
    "What is the role of mitochondria in apoptosis?",
    "Compare aerobic and anaerobic respiration."
  ],
  "folders": ["~/uni/biology"],
  "agents": [
    { "backend": "ollama", "model": "qwen2.5:1.5b", "context_size": 2048 },
    { "backend": "ollama", "model": "qwen2.5:1.5b", "context_size": 4096 },
    { "backend": "ollama", "model": "gemma2:latest", "context_size": 4096 },
    { "backend": "ollama", "model": "gemma2:latest", "context_size": 8192 },
    { "backend": "ollama", "model": "llama3.2:latest", "context_size": 4096 }
  ]
}
```

Run it, read the Markdown report, and pick the smallest model whose answers
still hold up.  Pay attention to the smaller context sizes — smaller windows
mean less memory pressure and faster responses.

### 2. Analyst: making a large literature body accessible

You maintain a collection of 200+ papers in `~/literature/renewables` and
need to answer detailed technical questions.  You have a workstation with
32 GB RAM and can afford larger models.

```json
{
  "questions": [
    "What is the current state-of-the-art in perovskite solar cell efficiency?",
    "Compare the lifecycle carbon footprint of wind and solar.",
    "What are the main barriers to grid-scale battery storage?",
    "Summarize policy recommendations for renewable energy adoption.",
    "How do different geographical regions compare in solar potential?"
  ],
  "folders": ["~/literature/renewables"],
  "agents": [
    { "backend": "ollama", "model": "gemma2:latest", "context_size": 8192 },
    { "backend": "ollama", "model": "llama3.2:latest", "context_size": 8192 },
    { "backend": "ollama", "model": "mistral:latest", "context_size": 8192 }
  ]
}
```

With a large document set, also experiment with the search parameters:

```bash
ragrig_bench -k 20 -t 0.1 bench.json > results.md
```

`-k 20` caps retrieval at 20 chunks per query (the default is 50).  `-t 0.1`
raises the cosine similarity threshold (default `0.04`) so only closely
matching chunks are retrieved — the threshold is applied to cosine scores
*before* RRF fusion, not to the fused RRF scores.

## CLI Reference

```
Usage: ragrig_bench [OPTIONS] <CONFIG>

Arguments:
  <CONFIG>  Path to the JSON benchmark configuration file

Options:
  -c, --context-size <N>       Default context window (tokens) [default: 4096]
  -e, --embed-model <MODEL>    Embedding model [default: nomic-embed-text]
  -k, --top-k <N>              Chunks retrieved per query [default: 50]
  -t, --similarity-threshold <F>  Minimum cosine similarity (0.0–1.0), applied
                                  before RRF fusion [default: 0.04]
  -h, --help                   Print help
  -V, --version                Print version
```

Agents in the config can override the default context size with an optional
`"context_size"` field.

## Requirements

| Dependency | When needed |
|---|---|
| Rust 1.94+ | Build |
| Ollama | Runtime — provides chat and embedding models |
| `nomic-embed-text` | Embedding model (pull once: `ollama pull nomic-embed-text`) |
| One or more chat models | Whatever you want to benchmark |
