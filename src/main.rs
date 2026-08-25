use anyhow::{Result, anyhow};
use clap::Parser;
use ragrig::{
    ChatAgentSpec, ChunkConfig, Embedder, EmbedderSpec, FolderCorpus, MarkdownChunker, RagAgent,
    RagResponse, VectorStore,
    parsers::{DocumentParsers, build_parsers},
    store::open_store,
    vector::{search_similar, sync_corpus},
};
use serde::Deserialize;
use std::path::PathBuf;

// ── CLI ─────────────────────────────────────────────────────────────────────

/// Benchmark ragrig retrieval quality across multiple folders, queries,
/// and chat backends.  Results are written to stdout as Markdown.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the JSON benchmark configuration file.
    config: String,

    /// Context window budget for prompt truncation (tokens).
    #[arg(short = 'c', long, default_value = "4096")]
    context_size: usize,

    /// Embedding model passed to Ollama.
    #[arg(short = 'e', long, default_value = "nomic-embed-text")]
    embed_model: String,

    /// Number of chunks to retrieve per query (top-k).
    #[arg(short = 'k', long, default_value = "50")]
    top_k: usize,

    /// Minimum cosine similarity (0.0–1.0) for a chunk to be retrieved.
    /// Applied to cosine scores *before* RRF fusion.
    #[arg(short = 't', long, default_value = "0.04")]
    similarity_threshold: f64,
}

// ── Input schema ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BenchmarkConfig {
    /// Independent queries — no history between them.
    #[serde(default, alias = "questions")]
    queries: Vec<String>,
    /// Sequential chat — each response feeds into the next prompt.
    #[serde(default)]
    chat: Vec<String>,
    folders: Vec<String>,
    agents: Vec<AgentConfig>,
}

#[derive(Deserialize, Clone)]
struct AgentConfig {
    backend: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    /// Per-agent context window override (tokens).  Falls back to the CLI
    /// `--context-size` value when absent.
    #[serde(default)]
    context_size: Option<usize>,
}

// ── Per-folder metadata ─────────────────────────────────────────────────────

struct FolderMeta {
    name: String,
    path: PathBuf,
    _temp: Option<tempfile::TempDir>,
}

// ── Chunking config (shared across all folders) ──────────────────────────────

fn chunk_config() -> ChunkConfig {
    ChunkConfig {
        size: 1024,
        overlap: 128,
    }
}

/// Fixture folders are extracted fresh from compile-time embedded data, so any
/// ragrig state files that ship alongside them (e.g. a stale vector store from
/// a previous indexing run) are artifacts, not fixture content.  Wipe them so
/// every run indexes the fixture documents from scratch, deterministically —
/// stale stores would otherwise trip ragrig 1.0's embedder-metadata guard.
fn clean_fixture_state(dir: &std::path::Path) {
    for name in [
        ".ragrig_store",
        ".ragrig_embeddings.json",
        ".ragrig_history",
    ] {
        let path = dir.join(name);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    let ragrig_dir = dir.join(".ragrig");
    if ragrig_dir.exists() {
        let _ = std::fs::remove_dir_all(ragrig_dir);
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let raw = std::fs::read_to_string(&cli.config)?;
    let config: BenchmarkConfig = serde_json::from_str(&raw)?;

    if config.queries.is_empty() && config.chat.is_empty() {
        anyhow::bail!("Config must contain 'queries' or 'chat'.");
    }
    if !config.queries.is_empty() && !config.chat.is_empty() {
        anyhow::bail!("Config must not contain both 'queries' and 'chat' — pick one.");
    }
    if config.folders.is_empty() {
        anyhow::bail!("Config must contain at least one folder.");
    }
    if config.agents.is_empty() {
        anyhow::bail!("Config must contain at least one agent.");
    }

    let chat_mode = !config.chat.is_empty();

    let embedder = EmbedderSpec::ollama(cli.embed_model.clone()).build()?;
    let parsers = DocumentParsers::new(build_parsers());

    // ── Phase 1: Index all folders ──────────────────────────────────────

    let mut folders: Vec<FolderMeta> = Vec::new();
    for raw in &config.folders {
        let (folder_path, display_name, temp_guard) =
            if let Some(format) = raw.strip_prefix("@fixtures/") {
                let (p, dir) = ragrig::fixtures::extract_fixtures(format)?;
                clean_fixture_state(&p);
                let name = format!("{} (fixture)", format);
                eprintln!("  Extracted {} fixtures → {}", format, p.display());
                (p, name, Some(dir))
            } else {
                let p = PathBuf::from(raw);
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| raw.clone());
                (p, name, None)
            };

        let cc = chunk_config();
        let store = open_store(&folder_path).await?;
        eprintln!("Indexing {} …", folder_path.display());

        // One corpus per folder, incrementally synced: new/changed documents
        // are indexed, removed ones deleted, unchanged ones skipped via the
        // store's manifest.  This corpus path replaced the hand-rolled
        // `.ragrig_embeddings.json` hash bookkeeping in ragrig 1.0.0.
        let corpus = FolderCorpus::named(display_name.clone(), folder_path.clone());
        let stats = sync_corpus(
            &corpus,
            &parsers,
            &MarkdownChunker,
            &*embedder,
            &cc,
            &*store,
        )
        .await?;
        let indexed: usize = stats.iter().map(|s| s.chunks).sum();

        eprintln!("  {} chunks ready ({} in this run).", store.len(), indexed);
        folders.push(FolderMeta {
            name: display_name,
            path: folder_path,
            _temp: temp_guard,
        });
    }

    // ── Header ──────────────────────────────────────────────────────────

    let today = chrono_lite()?;
    println!("# RAG Benchmark — {}", today);
    println!();

    // ── Phase 2: Run benchmarks ─────────────────────────────────────────

    for agent_cfg in &config.agents {
        let agent_label = format!("{} / {}", agent_cfg.backend, agent_cfg.model);
        let effective_ctx = agent_cfg.context_size.unwrap_or(cli.context_size);

        // Build ONE RagAgent per (backend, model).  The store is hot-swapped
        // per folder / mode below.  The default system prompt is kept — it
        // contains the `{context}` placeholder, so retrieved chunks actually
        // reach the model (an empty prompt would silently drop them).
        let bootstrap_store = open_store(&folders[0].path).await?;
        let mut agent = RagAgent::builder()
            .chat(build_chat_agent(agent_cfg)?)
            .embed(EmbedderSpec::ollama(cli.embed_model.clone()).build()?)
            .store(bootstrap_store)
            .context_tokens(effective_ctx)
            .top_k(cli.top_k)
            .similarity_threshold(cli.similarity_threshold)
            .build()?;

        println!("## {}", agent_label);
        println!();

        if chat_mode {
            run_chat_mode(&mut agent, &cli, &folders, &config.chat, &agent_label).await?;
        } else {
            for folder in &folders {
                // Hot-swap the store.
                let store = open_store(&folder.path).await?;
                agent.set_store(store);

                for (qi, query) in config.queries.iter().enumerate() {
                    eprintln!(
                        "  {} / {} ← \"{}\"",
                        agent_label,
                        folder.name,
                        &query[..query.len().min(60)]
                    );

                    println!("### Q{}: {}", qi + 1, query);
                    println!();
                    println!("#### {}", folder.name);
                    println!();

                    print_retrieval(agent.embedder(), agent.store(), &cli, query).await;

                    let response = agent
                        .generate_with_context_detailed(query, &[] as &[(&str, &str)])
                        .await
                        .unwrap_or_else(|e| RagResponse {
                            answer: format!("_Error: {}_", e),
                            system_prompt: String::new(),
                            user_prompt: query.to_string(),
                            chunks_retrieved: None,
                            documents: None,
                            rewritten_query: None,
                            elapsed: None,
                        });

                    let chunks = response.chunks_retrieved.unwrap_or(0);
                    let secs = response.elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                    println!(
                        "_t={} · k={} → {} in context · {} ctx · {:.1}s_",
                        cli.similarity_threshold, cli.top_k, chunks, effective_ctx, secs,
                    );
                    println!();
                    println!("{}", response.answer.trim());
                    println!();
                }
            }
        }
    }

    Ok(())
}

// ── Chat mode ───────────────────────────────────────────────────────────────

async fn run_chat_mode(
    agent: &mut RagAgent,
    cli: &Cli,
    folders: &[FolderMeta],
    messages: &[String],
    agent_label: &str,
) -> Result<()> {
    println!("### Chat");
    println!();

    for folder in folders {
        let store = open_store(&folder.path).await?;
        agent.set_store(store);

        println!("#### {}", folder.name);
        println!();

        let mut transcript: Vec<(String, String)> = Vec::new();

        for msg in messages {
            eprintln!(
                "  {} / {} ← \"{}\"",
                agent_label,
                folder.name,
                &msg[..msg.len().min(60)]
            );

            print_retrieval(agent.embedder(), agent.store(), cli, msg).await;

            let response = agent
                .generate_with_context_detailed(
                    msg,
                    &transcript
                        .iter()
                        .map(|(u, a)| (u.as_str(), a.as_str()))
                        .collect::<Vec<_>>(),
                )
                .await
                .unwrap_or_else(|e| RagResponse {
                    answer: format!("_Error: {}_", e),
                    system_prompt: String::new(),
                    user_prompt: msg.to_string(),
                    chunks_retrieved: None,
                    documents: None,
                    rewritten_query: None,
                    elapsed: None,
                });

            let chunks = response.chunks_retrieved.unwrap_or(0);
            let secs = response.elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0);

            println!("**Q:** {}", msg);
            println!();
            println!(
                "_t={} · k={} → {} in context · {} ctx · {:.1}s_",
                cli.similarity_threshold,
                cli.top_k,
                chunks,
                agent.context_tokens(),
                secs,
            );
            println!();
            println!("{}", response.answer.trim());
            println!();

            transcript.push((msg.clone(), response.answer));
        }
    }

    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Print the ranked retrieval results for `query` against the agent's current
/// store — the same embedder, top-k, and (pre-RRF) cosine threshold the
/// generation pipeline uses.  `RagResponse` only reports a chunk *count*, so
/// retrieval-quality benchmarks need these per-chunk results.  Scores come
/// from the store's active ranker (e.g. RRF fusion values, not raw cosine).
async fn print_retrieval(embedder: &dyn Embedder, store: &dyn VectorStore, cli: &Cli, query: &str) {
    match search_similar(embedder, cli.top_k, cli.similarity_threshold, store, query).await {
        Ok(results) => {
            if results.is_empty() {
                println!("_No chunks passed the similarity threshold._");
            } else {
                println!("**Retrieved** (ranked by store ranker):");
                for (i, r) in results.iter().enumerate() {
                    let snippet: String = r
                        .chunk
                        .text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let snippet: String = snippet.chars().take(96).collect();
                    println!(
                        "{}. **{}** — {} — _{snippet}_",
                        i + 1,
                        r.chunk.document,
                        r.score
                    );
                }
            }
            println!();
        }
        Err(e) => {
            println!("_Retrieval error: {e}_");
            println!();
        }
    }
}

fn build_chat_agent(cfg: &AgentConfig) -> Result<Box<dyn ragrig::agents::Generator>> {
    ChatAgentSpec::parse(&cfg.backend, Some(&cfg.model), cfg.api_key.as_deref(), None)?.build()
}

fn chrono_lite() -> Result<String> {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map_err(|_| anyhow!("'date' command not found"))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
