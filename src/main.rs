use anyhow::{Result, anyhow};
use ragrig::{
    Args, ChatAgentSpec, EmbedderSpec, EmbeddingProvider, HashMetadata,
    PdfParserBackend, Provider, SystemPrompts,
    embed_documents,
    get_changed_documents, get_document_file_hashes, get_embeddings_file_path,
    parsers::{DocumentParsers, build_parsers},
    remove_deleted_embeddings, search_similar,
    store::open_store,
    update_file_hashes,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
};

const EMBED_MODEL: &str = "nomic-embed-text";

// ── Input schema ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BenchmarkConfig {
    questions: Vec<String>,
    folders: Vec<PathBuf>,
    agents: Vec<AgentConfig>,
}

#[derive(Deserialize, Clone)]
struct AgentConfig {
    backend: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
}

// ── Per-folder indexed state ────────────────────────────────────────────────

struct FolderState {
    name: String,
    args: Args,
    store: Box<dyn ragrig::store::VectorStore>,
}

// ── Helper: construct Args for a given folder ───────────────────────────────

fn make_args(folder: &Path) -> Args {
    Args {
        folder: folder.to_path_buf(),
        provider: Provider::Ollama,
        model: String::new(), // not used — agents come from config
        deepseek_api_key: None,
        deepseek_model: String::new(),
        semantic_scholar_api_key: None,
        embedding_provider: EmbeddingProvider::Ollama,
        embedding_model: EMBED_MODEL.into(),
        history_model: String::new(),
        prompt_chat: None,
        prompt_rewrite: None,
        sloppy_pdf: false,
        pdf_parser: PdfParserBackend::Sink,
        threads: 4,
        embedding_concurrency: 32,
        chunk_size: 1024,
        chunk_overlap: 128,
        top_k: 10,
        similarity_threshold: 0.4,
        model_ctx_tokens: 4096,
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("Usage: ragrig_bench <config.json>"))?;

    let raw = std::fs::read_to_string(&config_path)?;
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

    // Shared embedder (all folders use the same embedding model).
    let embedder = EmbedderSpec::Ollama {
        model: EMBED_MODEL.into(),
    }
    .build()?;
    let parsers = DocumentParsers::new(build_parsers());
    let prompts = SystemPrompts::default();

    // Index every folder (incremental), collecting per-folder state.
    let mut folder_states: Vec<FolderState> = Vec::new();
    for folder in &config.folders {
        let args = make_args(folder);
        let store = open_store(&args.folder).await?;
        eprintln!("Indexing {} …", folder.display());

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
            name: folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| folder.to_string_lossy().into_owned()),
            args,
            store,
        });
    }

    // ── Header ──────────────────────────────────────────────────────────

    let today = chrono_lite()?;
    println!("# RAG Benchmark — {}", today);
    println!();

    // ── Query loop ──────────────────────────────────────────────────────

    for (qi, question) in config.questions.iter().enumerate() {
        println!("## Q{}: {}", qi + 1, question);
        println!();

        for folder_state in &folder_states {
            for agent_cfg in &config.agents {
                let label = format!(
                    "{} · {} / {}",
                    folder_state.name, agent_cfg.backend, agent_cfg.model
                );
                eprintln!("  {} ← \"{}\"", label, &question[..question.len().min(60)]);

                let chat = build_agent(agent_cfg)?;

                let result = run_query(
                    &*embedder,
                    &folder_state.args,
                    &*folder_state.store,
                    &prompts,
                    question,
                    &*chat,
                )
                .await;

                println!("### {}", label);
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
    args: &Args,
    store: &dyn ragrig::store::VectorStore,
    prompts: &SystemPrompts,
    question: &str,
    chat: &dyn ragrig::agents::Generator,
) -> Result<String> {
    let results = search_similar(embedder, args, store, question).await?;
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

// ── Tiny local-date helper (no chrono dep) ─────────────────────────────────

fn chrono_lite() -> Result<String> {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map_err(|_| anyhow!("'date' command not found"))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
