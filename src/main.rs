use anyhow::{Result, anyhow};
use clap::Parser;
use include_dir::Dir;
use ragrig::{
    Args, ChatAgentSpec, EmbedderSpec, EmbeddingProvider, HashMetadata,
    PdfParserBackend, Provider, SystemPrompts,
    embed_documents,
    get_changed_documents, get_document_file_hashes, get_embeddings_file_path,
    parsers::{DocumentParsers, build_parsers},
    remove_deleted_embeddings,
    store::open_store,
    update_file_hashes,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
};

const FIXTURE_PREFIX: &str = "@fixtures/";

// ── CLI ─────────────────────────────────────────────────────────────────────

/// Benchmark ragrig retrieval quality across multiple folders, questions,
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
    #[arg(short = 'k', long, default_value = "10")]
    top_k: usize,

    /// Minimum hybrid RRF score for a chunk to be included.
    #[arg(short = 't', long, default_value = "0.4")]
    similarity_threshold: f64,
}

// ── Input schema ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BenchmarkConfig {
    questions: Vec<String>,
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
    _temp: Option<TempFixtureDir>,
}

/// Holds a temp directory alive until dropped, then cleans it up.
struct TempFixtureDir(PathBuf);

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ── Fixture resolution ──────────────────────────────────────────────────────

fn resolve_fixture_folder(raw: &str) -> Result<(PathBuf, String, TempFixtureDir)> {
    let format = raw.strip_prefix(FIXTURE_PREFIX)
        .ok_or_else(|| anyhow!("Fixture path must start with '{}'", FIXTURE_PREFIX))?;

    let dir: &Dir = match format {
        "pdf" => &ragrig::fixtures::pdf::DIR,
        "html" => &ragrig::fixtures::html::DIR,
        "rmd" => &ragrig::fixtures::rmd::DIR,
        other => anyhow::bail!(
            "Unknown fixture format '{}'. Available: pdf, html, rmd",
            other
        ),
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let temp = std::env::temp_dir().join(format!("ragrig_bench_{}_{}", format, ts));
    std::fs::create_dir_all(&temp)?;

    let mut count = 0;
    for entry in dir.files() {
        let name = entry
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        std::fs::write(temp.join(name), entry.contents())?;
        count += 1;
    }
    eprintln!(
        "  Extracted {} fixture files → {}",
        count,
        temp.display()
    );

    let display_name = format!("{} (fixture)", format);
    let temp_guard = TempFixtureDir(temp.clone());
    Ok((temp, display_name, temp_guard))
}

// ── Helper: construct Args for a given folder ───────────────────────────────

fn make_args(folder: &Path, cli: &Cli) -> Args {
    Args {
        folder: folder.to_path_buf(),
        provider: Provider::Ollama,
        model: String::new(),
        deepseek_api_key: None,
        deepseek_model: String::new(),
        semantic_scholar_api_key: None,
        embedding_provider: EmbeddingProvider::Ollama,
        embedding_model: cli.embed_model.clone(),
        history_model: String::new(),
        prompt_chat: None,
        prompt_rewrite: None,
        sloppy_pdf: false,
        pdf_parser: PdfParserBackend::Sink,
        threads: 4,
        embedding_concurrency: 32,
        chunk_size: 1024,
        chunk_overlap: 128,
        top_k: cli.top_k,
        similarity_threshold: cli.similarity_threshold,
        model_ctx_tokens: cli.context_size,
        context_size_forced: false,
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let raw = std::fs::read_to_string(&cli.config)?;
    let config: BenchmarkConfig = serde_json::from_str(&raw)?;

    if config.questions.is_empty() {
        anyhow::bail!("Config must contain at least one question.");
    }
    if config.folders.is_empty() {
        anyhow::bail!("Config must contain at least one folder.");
    }
    if config.agents.is_empty() {
        anyhow::bail!("Config must contain at least one agent.");
    }

    let embedder = EmbedderSpec::Ollama {
        model: cli.embed_model.clone(),
    }
    .build()?;
    let parsers = DocumentParsers::new(build_parsers());
    let prompts = SystemPrompts::default();

    // Resolve folders — fixtures get extracted to temp dirs.
    let mut folder_states: Vec<FolderState> = Vec::new();
    for raw in &config.folders {
        let (folder_path, display_name, temp_guard) = if raw.starts_with(FIXTURE_PREFIX) {
            let (p, name, guard) = resolve_fixture_folder(raw)?;
            (p, name, Some(guard))
        } else {
            let p = PathBuf::from(raw);
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw.clone());
            (p, name, None)
        };

        let args = make_args(&folder_path, &cli);
        let store = open_store(&args.folder).await?;
        eprintln!("Indexing {} …", folder_path.display());

        let current_hashes = get_document_file_hashes(&args.folder)?;
        let hashes_path = get_embeddings_file_path(&args.folder);
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
            embed_documents(&*embedder, &parsers, &args, changed, &*store).await?;
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

        for (qi, question) in config.questions.iter().enumerate() {
            println!("### Q{}: {}", qi + 1, question);
            println!();

            for folder_state in &folder_states {
                eprintln!(
                    "  {} ← \"{}\"",
                    agent_label,
                    &question[..question.len().min(60)]
                );

                let result = run_query(
                    &*embedder,
                    cli.top_k,
                    cli.similarity_threshold,
                    effective_ctx,
                    &*folder_state.store,
                    &prompts,
                    question,
                    &*chat,
                )
                .await;

                println!("#### {}", folder_state.name);
                println!();
                match result {
                    Ok(answer) => {
                        println!("{}", answer.trim());
                    }
                    Err(e) => {
                        println!("_Error: {}_", e);
                    }
                }
                println!();
            }
        }
    }

    Ok(())
}

// ── Agent builder ───────────────────────────────────────────────────────────

fn build_agent(cfg: &AgentConfig) -> Result<Box<dyn ragrig::agents::Generator>> {
    ChatAgentSpec::parse(&cfg.backend, Some(&cfg.model), cfg.api_key.as_deref())?.build()
}

// ── Single query execution (no history) ─────────────────────────────────────

async fn run_query(
    embedder: &dyn ragrig::embed::Embedder,
    top_k: usize,
    threshold: f64,
    _context_size: usize,
    store: &dyn ragrig::store::VectorStore,
    prompts: &SystemPrompts,
    question: &str,
    chat: &dyn ragrig::agents::Generator,
) -> Result<String> {
    let embedded = embedder.embed(vec![question.to_string()]).await?;
    let query_vec: Vec<f32> = embedded
        .first()
        .map(|(_, v)| v.clone())
        .ok_or_else(|| anyhow!("Failed to get query embedding"))?;

    let results = store.search(&query_vec, question, top_k, threshold).await?;
    if results.is_empty() {
        return Ok("_(no relevant documents found)_".into());
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
        "{}\n\nQuestion: {}",
        prompts.format_chat_with_docs(&context),
        question
    );

    chat.generate(&full_prompt).await
}

// ── Tiny local-date helper ──────────────────────────────────────────────────

fn chrono_lite() -> Result<String> {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map_err(|_| anyhow!("'date' command not found"))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
