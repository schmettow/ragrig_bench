use anyhow::{Result, anyhow};
use clap::Parser;
use ragrig::{
    ChatAgentSpec, ChunkConfig, EmbedderSpec,
    embed_documents,
    documents::{get_changed_documents, get_document_file_hashes, update_file_hashes, HashMetadata},
    parsers::{DocumentParsers, build_parsers},
    store::open_store,
    vector::{get_embeddings_file_path, remove_deleted_embeddings},
};

const CHAT_PROMPT: &str = "You are a helpful document assistant. Answer the user's question explicitly using the provided Context snippets.\n\nContext:\n{context}\n";
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
    #[arg(short = 'k', long, default_value = "20")]
    top_k: usize,

    /// Minimum hybrid RRF score for a chunk to be included.
    #[arg(short = 't', long, default_value = "0.3")]
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

// ── Per-folder indexed state ────────────────────────────────────────────────

struct FolderState {
    name: String,
    store: Box<dyn ragrig::store::VectorStore>,
    _temp: Option<tempfile::TempDir>,
}

// ── Chunking config (shared across all folders) ──────────────────────────────

fn chunk_config() -> ChunkConfig {
    ChunkConfig { size: 1024, overlap: 128 }
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

    let embedder = EmbedderSpec::Ollama {
        model: cli.embed_model.clone(),
    }
    .build()?;
    let parsers = DocumentParsers::new(build_parsers());

    // Resolve folders — fixtures get extracted to temp dirs.
    let mut folder_states: Vec<FolderState> = Vec::new();
    for raw in &config.folders {
        let (folder_path, display_name, temp_guard) =
            if let Some(format) = raw.strip_prefix("@fixtures/") {
                let (p, dir) = ragrig::fixtures::extract_fixtures(format)?;
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

        let config = chunk_config();
        let store = open_store(&folder_path).await?;
        eprintln!("Indexing {} …", folder_path.display());

        let current_hashes = get_document_file_hashes(&folder_path)?;
        let hashes_path = get_embeddings_file_path(&folder_path);
        let stored_meta: Option<HashMetadata> = if hashes_path.exists() {
            let raw = std::fs::read_to_string(&hashes_path)?;
            Some(serde_json::from_str(&raw)?)
        } else {
            None
        };
        let stored_entries = stored_meta
            .as_ref()
            .map(|m| m.file_hashes.as_slice())
            .unwrap_or(&[]);
        let changed = get_changed_documents(&current_hashes, stored_entries);

        if !changed.is_empty() {
            embed_documents(&*embedder, &parsers, &config, changed, &*store).await?;
        }
        remove_deleted_embeddings(&*store, &current_hashes).await?;
        update_file_hashes(&current_hashes, &hashes_path)?;

        eprintln!("  {} chunks ready.", store.len());
        folder_states.push(FolderState {
            name: display_name,
            store,
            _temp: temp_guard,
        });
    }

    // ── Header ──────────────────────────────────────────────────────────

    let today = chrono_lite()?;
    println!("# RAG Benchmark — {}", today);
    println!();

    // ── Query loop (agent outer → model stays loaded) ────────────────────

    for agent_cfg in &config.agents {
        let agent_label = format!("{} / {}", agent_cfg.backend, agent_cfg.model);
        let effective_ctx = agent_cfg.context_size.unwrap_or(cli.context_size);
        let chat = build_agent(agent_cfg)?;

        println!("## {}", agent_label);
        println!();

        if chat_mode {
            run_chat_mode(
                &*embedder,
                &cli,
                effective_ctx,
                &folder_states,
                &config.chat,
                &*chat,
                &agent_label,
            )
            .await?;
        } else {
            for (qi, query) in config.queries.iter().enumerate() {
                println!("### Q{}: {}", qi + 1, query);
                println!();

                for folder_state in &folder_states {
                    eprintln!(
                        "  {} ← \"{}\"",
                        agent_label,
                        &query[..query.len().min(60)]
                    );

                    let start = std::time::Instant::now();

                    let result = run_query(
                        &*embedder,
                        cli.top_k,
                        cli.similarity_threshold,
                        effective_ctx,
                        &*folder_state.store,
                        query,
                        &*chat,
                    )
                    .await;

                    let elapsed = start.elapsed();

                    println!("#### {}", folder_state.name);
                    println!();
                    match result {
                        Ok(ref qr) => {
                            println!(
                                "_t={:.1} · k={} → {} · {} ctx · {:.1}s_",
                                cli.similarity_threshold,
                                cli.top_k,
                                qr.chunks_found,
                                effective_ctx,
                                elapsed.as_secs_f64(),
                            );
                            println!();
                            println!("{}", qr.answer.trim());
                        }
                        Err(e) => {
                            println!("_Error: {}_", e);
                        }
                    }
                    println!();
                }
            }
        }
    }

    Ok(())
}

// ── Agent builder ───────────────────────────────────────────────────────────

fn build_agent(cfg: &AgentConfig) -> Result<Box<dyn ragrig::agents::Generator>> {
    ChatAgentSpec::parse(&cfg.backend, Some(&cfg.model), cfg.api_key.as_deref())?.build()
}

// ── Chat mode (sequential conversation with history) ────────────────────────

async fn run_chat_mode(
    embedder: &dyn ragrig::embed::Embedder,
    cli: &Cli,
    effective_ctx: usize,
    folder_states: &[FolderState],
    messages: &[String],
    chat: &dyn ragrig::agents::Generator,
    agent_label: &str,
) -> Result<()> {
    println!("### Chat");
    println!();

    for folder_state in folder_states {
        println!("#### {}", folder_state.name);
        println!();

        let mut history = String::new();

        for msg in messages {
            eprintln!(
                "  {} ← \"{}\"",
                agent_label,
                &msg[..msg.len().min(60)]
            );

            let start = std::time::Instant::now();

            // Search with the raw message.
            let embedded = embedder.embed(vec![msg.clone()]).await?;
            let query_vec: Vec<f32> = embedded
                .first()
                .map(|(_, v)| v.clone())
                .ok_or_else(|| anyhow!("Failed to get query embedding"))?;

            let results = folder_state
                .store
                .search(&query_vec, msg, cli.top_k, cli.similarity_threshold)
                .await?;
            let chunks_found = results.len();

            let context = results
                .iter()
                .enumerate()
                .map(|(i, sc)| {
                    format!(
                        "[{}] (source: {}, score: {:.3})\n{}\n",
                        i + 1,
                        sc.chunk.source_file,
                        sc.score,
                        sc.chunk.text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            // Build prompt with conversation history + context.
            let full_prompt = if history.is_empty() {
                format!(
                    "{}\n\nQuery: {}",
                    CHAT_PROMPT.replace("{context}", &context),
                    msg
                )
            } else {
                format!(
                    "{}\n\n{}\n\nQuery: {}",
                    history,
                    CHAT_PROMPT.replace("{context}", &context),
                    msg
                )
            };

            let answer = match chat.generate(&full_prompt).await {
                Ok(a) => a,
                Err(e) => {
                    println!("**Q:** {}", msg);
                    println!();
                    println!("_Error: {}_", e);
                    println!();
                    break;
                }
            };

            let elapsed = start.elapsed();

            println!("**Q:** {}", msg);
            println!();
            println!(
                "_t={:.1} · k={} → {} · {} ctx · {:.1}s_",
                cli.similarity_threshold,
                cli.top_k,
                chunks_found,
                effective_ctx,
                elapsed.as_secs_f64(),
            );
            println!();
            println!("{}", answer.trim());
            println!();

            // Append to history for the next turn.
            history.push_str(&format!("User: {}\nAssistant: {}\n", msg, answer));
        }
    }

    Ok(())
}

// ── Single query execution (no history) ─────────────────────────────────────

struct QueryResult {
    answer: String,
    chunks_found: usize,
}

async fn run_query(
    embedder: &dyn ragrig::embed::Embedder,
    top_k: usize,
    threshold: f64,
    _context_size: usize,
    store: &dyn ragrig::store::VectorStore,
    query: &str,
    chat: &dyn ragrig::agents::Generator,
) -> Result<QueryResult> {
    let embedded = embedder.embed(vec![query.to_string()]).await?;
    let query_vec: Vec<f32> = embedded
        .first()
        .map(|(_, v)| v.clone())
        .ok_or_else(|| anyhow!("Failed to get query embedding"))?;

    let results = store.search(&query_vec, query, top_k, threshold).await?;
    let chunks_found = results.len();
    if chunks_found == 0 {
        return Ok(QueryResult {
            answer: "_(no relevant documents found)_".into(),
            chunks_found: 0,
        });
    }

    let context = results
        .iter()
        .enumerate()
        .map(|(i, sc)| {
            format!(
                "[{}] (source: {}, score: {:.3})\n{}\n",
                i + 1,
                sc.chunk.source_file,
                sc.score,
                sc.chunk.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let full_prompt = format!(
        "{}\n\nQuery: {}",
        CHAT_PROMPT.replace("{context}", &context),
        query
    );

    let answer = chat.generate(&full_prompt).await?;
    Ok(QueryResult {
        answer,
        chunks_found,
    })
}

// ── Tiny local-date helper ──────────────────────────────────────────────────

fn chrono_lite() -> Result<String> {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map_err(|_| anyhow!("'date' command not found"))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
