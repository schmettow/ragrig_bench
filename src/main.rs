use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use ragrig::{
    ChatAgentSpec, ChunkConfig, Chunker, Embedder, EmbedderSpec, FolderCorpus, Generator,
    MarkdownChunker, PipelineFilter, RagAgent, RagResponse, VectorStore, available_chunkers,
    parsers::{DocumentParsers, build_parsers},
    store::open_store,
    sync_corpus,
    types::{ChatConfig, EmbedConfig, EmbeddingProvider, ParseConfig, Provider},
};
use serde::Deserialize;
use std::path::PathBuf;

// ── CLI ─────────────────────────────────────────────────────────────────────

/// Benchmark ragrig retrieval quality across document corpora, queries,
/// (parser, chunker) pipelines, and chat backends.  Results are written to
/// stdout as Markdown.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML benchmark configuration file.
    config: String,

    /// Workspace directory for the vector store (created if missing).
    /// One shared store holds every indexed pipeline.
    #[arg(short = 'w', long, default_value = ".ragrig_bench")]
    workspace: PathBuf,
}

// ── Config schema ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BenchmarkConfig {
    /// Independent queries — no history between them.
    #[serde(default, alias = "questions")]
    queries: Vec<String>,
    /// Sequential chat — each response feeds into the next prompt.
    #[serde(default)]
    chat: Vec<String>,
    /// Named document corpora in `name=path` form.  A path prefixed with
    /// `@fixtures/` extracts the library's embedded fixture set
    /// (`pdf`, `rmd`, `html`) into a temp directory.
    #[serde(default)]
    corpus_dirs: Vec<String>,
    /// Chat agents forming the benchmark matrix.
    #[serde(default)]
    agents: Vec<AgentConfig>,
    /// (parser, chunker) pipelines to compare.  Every pipeline indexes all
    /// corpora into the shared store and is queried separately at runtime
    /// via `PipelineFilter`.  Empty = one default pipeline (full parser
    /// registry, `MarkdownChunker`).
    #[serde(default)]
    pipelines: Vec<PipelineConfig>,
    /// Embedding / retrieval settings.  Omitted fields use the library
    /// defaults (`EmbedConfig::default()`).
    #[serde(default, deserialize_with = "deserialize_with_defaults")]
    embed: EmbedConfig,
    /// Chunk size / overlap applied by every pipeline.
    #[serde(default, deserialize_with = "deserialize_with_defaults")]
    parse: ParseConfig,
}

#[derive(Deserialize)]
struct AgentConfig {
    /// Optional display label; defaults to `<provider> / <model>`.
    label: Option<String>,
    /// Chat settings — the library's `ChatConfig` (provider, model,
    /// generation params, context window, system-prompt file, timeout).
    /// Omitted fields use `ChatConfig::default()`.
    #[serde(default, deserialize_with = "deserialize_with_defaults")]
    chat: ChatConfig,
}

#[derive(Deserialize, Clone, Default)]
struct PipelineConfig {
    /// PDF parser by name (`"unpdf"`, `"pdf-extract"`, `"pdfsink"`,
    /// `"sloppy-pdf"`, `"kreuzberg"`, `"vision-pdf"`).  `None` = the full
    /// registry — the first parser that succeeds (the default pipeline).
    parser: Option<String>,
    /// Chunker by name (`"markdown"`, `"token"`, `"chunkedrs-*"`, …).
    /// `None` = `MarkdownChunker`.
    chunker: Option<String>,
}

/// Deserialize `T` with missing fields filled from `T::default()`.  The
/// library config types require every field, but benchmark files should only
/// mention what they change.
fn deserialize_with_defaults<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + serde::Serialize + Default,
{
    let provided = toml::Value::deserialize(deserializer)?;
    let mut merged = serde_json::to_value(T::default()).map_err(serde::de::Error::custom)?;
    let provided = serde_json::to_value(provided).map_err(serde::de::Error::custom)?;
    if let (serde_json::Value::Object(defaults), serde_json::Value::Object(provided)) =
        (&mut merged, provided)
    {
        for (key, value) in provided {
            defaults.insert(key, value);
        }
    }
    serde_json::from_value(merged).map_err(serde::de::Error::custom)
}

// ── Resolved pipeline / corpus ─────────────────────────────────────────────

struct ResolvedPipeline {
    label: String,
    parsers: DocumentParsers,
    chunker: Box<dyn Chunker>,
}

struct ResolvedCorpus {
    /// User-facing corpus name from `corpus_dirs`.
    name: String,
    path: PathBuf,
    /// Keeps extracted fixture temp dirs alive for the whole run.
    _temp: Option<tempfile::TempDir>,
}

impl ResolvedCorpus {
    /// Store-level corpus name: every pipeline indexes each source corpus
    /// under a pipeline-scoped name, so a `PipelineFilter::for_corpus` alone
    /// selects exactly one pipeline's chunks.  A filter on the parser field
    /// cannot express "pdf chunks from parser X plus all non-pdf chunks", and
    /// two pipelines sharing a chunker would otherwise mix.
    fn scoped_name(&self, pipeline: &ResolvedPipeline) -> String {
        format!("{}::{}", self.name, pipeline.label)
    }
}

// ── Resolution helpers ──────────────────────────────────────────────────────

/// Resolve the `name=path` corpus specs.  Fixture paths (`@fixtures/<fmt>`)
/// are extracted to temp dirs held alive by the returned entries.
fn resolve_corpora(specs: &[String]) -> Result<Vec<ResolvedCorpus>> {
    specs
        .iter()
        .map(|spec| {
            let (name, raw) = spec
                .split_once('=')
                .ok_or_else(|| anyhow!("corpus '{spec}' must have the form 'name=path'"))?;
            let (path, temp) = if let Some(format) = raw.strip_prefix("@fixtures/") {
                let (p, dir) = ragrig::fixtures::extract_fixtures(format)?;
                eprintln!("  Extracted {format} fixtures → {}", p.display());
                (p, Some(dir))
            } else {
                (PathBuf::from(raw), None)
            };
            Ok(ResolvedCorpus {
                name: name.to_string(),
                path,
                _temp: temp,
            })
        })
        .collect()
}

fn chunker_names() -> String {
    let names: Vec<&str> = available_chunkers().iter().map(|c| c.name()).collect();
    names.join(", ")
}

/// Resolve the pipeline list.  An empty config yields one default pipeline:
/// the full parser registry + `MarkdownChunker` (historical behaviour).
fn resolve_pipelines(specs: &[PipelineConfig]) -> Result<Vec<ResolvedPipeline>> {
    let specs: Vec<PipelineConfig> = if specs.is_empty() {
        vec![PipelineConfig::default()]
    } else {
        specs.to_vec()
    };
    specs
        .into_iter()
        .map(|spec| {
            let chunker: Box<dyn Chunker> = match &spec.chunker {
                Some(name) => available_chunkers()
                    .into_iter()
                    .find(|c| c.name() == name)
                    .ok_or_else(|| {
                        anyhow!("unknown chunker '{name}'. Available: {}", chunker_names())
                    })?,
                None => Box::new(MarkdownChunker),
            };
            let parsers = parsers_for(spec.parser.as_deref())?;
            let parser_label = spec
                .parser
                .clone()
                .unwrap_or_else(|| "all-parsers".to_string());
            Ok(ResolvedPipeline {
                label: format!("{parser_label} · {}", chunker.name()),
                parsers,
                chunker,
            })
        })
        .collect()
}

/// Parser registry for a pipeline.  `None` = every built-in parser; a name
/// selects that PDF parser while keeping the single parsers for the other
/// formats (epub, html, docx, markdown).
fn parsers_for(pdf_parser: Option<&str>) -> Result<DocumentParsers> {
    let registry = build_parsers();
    let Some(name) = pdf_parser else {
        return Ok(DocumentParsers::new(registry));
    };
    let pdf_names: Vec<&str> = registry
        .iter()
        .filter(|p| p.extensions().contains(&"pdf"))
        .map(|p| p.name())
        .collect();
    if !pdf_names.contains(&name) {
        bail!(
            "unknown PDF parser '{name}'. Available: {}",
            pdf_names.join(", ")
        );
    }
    let filtered = registry
        .into_iter()
        .filter(|p| p.name() == name || !p.extensions().contains(&"pdf"))
        .collect();
    Ok(DocumentParsers::new(filtered))
}

// ── Backend builders (library spec enums) ──────────────────────────────────

fn build_embedder(embed: &EmbedConfig) -> Result<Box<dyn Embedder>> {
    let spec = match &embed.provider {
        EmbeddingProvider::Ollama => EmbedderSpec::Ollama {
            model: embed.model.clone(),
            request_timeout_secs: embed.request_timeout_secs,
        },
        // Fastembed (when compiled in) and future variants go through
        // `parse`, which reports the backends available in this build.
        other => EmbedderSpec::parse(&format!("{other:?}").to_lowercase(), Some(&embed.model))?,
    };
    spec.build()
}

fn build_chat_agent(chat: &ChatConfig) -> Result<Box<dyn Generator>> {
    let spec = match &chat.provider {
        Provider::Ollama => ChatAgentSpec::ollama(
            chat.model.clone(),
            chat.params.clone(),
            chat.request_timeout_secs,
        ),
        Provider::Deepseek => ChatAgentSpec::deepseek(
            chat.deepseek_model.clone(),
            chat.deepseek_api_key.clone(),
            chat.params.clone(),
            chat.request_timeout_secs,
        ),
        other => bail!("chat provider {other:?} is not supported by this build"),
    };
    spec.build()
}

fn build_agent(
    agent_cfg: &AgentConfig,
    embed_cfg: &EmbedConfig,
    store: Box<dyn VectorStore>,
) -> Result<RagAgent> {
    let chat = &agent_cfg.chat;
    let mut builder = RagAgent::builder()
        .chat(build_chat_agent(chat)?)
        .embed(build_embedder(embed_cfg)?)
        .store(store)
        .context_tokens(chat.context_tokens)
        .top_k(embed_cfg.top_k)
        .similarity_threshold(embed_cfg.similarity_threshold);
    if let Some(path) = &chat.system_prompt_path {
        let prompt = std::fs::read_to_string(path)
            .with_context(|| format!("reading system prompt {}", path.display()))?;
        builder = builder.system_prompt(prompt);
    }
    builder.build()
}

fn agent_label(cfg: &AgentConfig) -> String {
    if let Some(label) = &cfg.label {
        return label.clone();
    }
    let model = match &cfg.chat.provider {
        Provider::Ollama => &cfg.chat.model,
        Provider::Deepseek => &cfg.chat.deepseek_model,
        _ => "unknown",
    };
    format!(
        "{} / {model}",
        format!("{:?}", cfg.chat.provider).to_lowercase()
    )
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let raw = std::fs::read_to_string(&cli.config)
        .with_context(|| format!("reading config {}", cli.config))?;
    let config: BenchmarkConfig =
        toml::from_str(&raw).with_context(|| format!("parsing config {}", cli.config))?;

    if config.queries.is_empty() && config.chat.is_empty() {
        bail!("config must contain 'queries' or 'chat'.");
    }
    if !config.queries.is_empty() && !config.chat.is_empty() {
        bail!("config must not contain both 'queries' and 'chat' — pick one.");
    }
    if config.corpus_dirs.is_empty() {
        bail!("config must contain at least one 'corpus_dirs' entry.");
    }
    if config.agents.is_empty() {
        bail!("config must contain at least one [[agents]] entry.");
    }

    std::fs::create_dir_all(&cli.workspace)
        .with_context(|| format!("creating workspace {}", cli.workspace.display()))?;

    let embedder = build_embedder(&config.embed)?;
    let chunk_cfg = ChunkConfig {
        size: config.parse.chunk_size,
        overlap: config.parse.chunk_overlap,
    };
    let corpus_entries = resolve_corpora(&config.corpus_dirs)?;
    let pipelines = resolve_pipelines(&config.pipelines)?;

    // ── Phase 1: index every pipeline into the shared store ─────────────
    {
        let store = open_store(&cli.workspace).await?;
        for pipeline in &pipelines {
            eprintln!("Indexing pipeline '{}' …", pipeline.label);
            let mut indexed = 0;
            for corpus_entry in &corpus_entries {
                let corpus =
                    FolderCorpus::named(corpus_entry.scoped_name(pipeline), &corpus_entry.path);
                let stats = sync_corpus(
                    &corpus,
                    &pipeline.parsers,
                    &*pipeline.chunker,
                    &*embedder,
                    &chunk_cfg,
                    &*store,
                )
                .await?;
                indexed += stats.iter().map(|s| s.chunks).sum::<usize>();
            }
            eprintln!(
                "  {} chunks in store ({} indexed in this run).",
                store.len(),
                indexed
            );
        }
    }

    let today = chrono_lite()?;
    println!("# RAG Benchmark — {today}");
    println!();

    let chat_mode = !config.chat.is_empty();
    let multi_pipeline = pipelines.len() > 1;
    let multi_corpus = corpus_entries.len() > 1;

    // ── Phase 2: run the benchmark matrix ───────────────────────────────
    for agent_cfg in &config.agents {
        let label = agent_label(agent_cfg);
        let store = open_store(&cli.workspace).await?;
        let mut agent = build_agent(agent_cfg, &config.embed, store)?;

        println!("## {label}");
        println!();

        for pipeline in &pipelines {
            if multi_pipeline {
                println!("### {}", pipeline.label);
                println!();
            }
            for corpus_entry in &corpus_entries {
                if multi_corpus {
                    println!("#### {}", corpus_entry.name);
                    println!();
                }
                let filter = PipelineFilter::for_corpus(corpus_entry.scoped_name(pipeline));
                agent.set_search_filter(Some(filter.clone()));

                if chat_mode {
                    run_chat_mode(
                        &mut agent,
                        &config.embed,
                        &filter,
                        &config.chat,
                        &label,
                        &corpus_entry.name,
                    )
                    .await?;
                } else {
                    for (qi, query) in config.queries.iter().enumerate() {
                        eprintln!(
                            "  {label} / {} / {} ← \"{}\"",
                            pipeline.label,
                            corpus_entry.name,
                            &query[..query.len().min(60)]
                        );

                        println!("**Q{}:** {query}", qi + 1);
                        println!();

                        print_retrieval(
                            agent.embedder(),
                            agent.store(),
                            &config.embed,
                            &filter,
                            query,
                        )
                        .await;

                        let response = agent
                            .generate_with_context_detailed(query, &[] as &[(&str, &str)])
                            .await
                            .unwrap_or_else(|e| RagResponse {
                                answer: format!("_Error: {e}_"),
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
                            config.embed.similarity_threshold,
                            config.embed.top_k,
                            chunks,
                            agent.context_tokens(),
                            secs,
                        );
                        println!();
                        println!("{}", response.answer.trim());
                        println!();
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Chat mode ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_chat_mode(
    agent: &mut RagAgent,
    embed_cfg: &EmbedConfig,
    filter: &PipelineFilter,
    messages: &[String],
    agent_label: &str,
    corpus_name: &str,
) -> Result<()> {
    let mut transcript: Vec<(String, String)> = Vec::new();

    for msg in messages {
        eprintln!(
            "  {agent_label} / {corpus_name} ← \"{}\"",
            &msg[..msg.len().min(60)]
        );

        print_retrieval(agent.embedder(), agent.store(), embed_cfg, filter, msg).await;

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
                answer: format!("_Error: {e}_"),
                system_prompt: String::new(),
                user_prompt: msg.to_string(),
                chunks_retrieved: None,
                documents: None,
                rewritten_query: None,
                elapsed: None,
            });

        let chunks = response.chunks_retrieved.unwrap_or(0);
        let secs = response.elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0);

        println!("**Q:** {msg}");
        println!();
        println!(
            "_t={} · k={} → {} in context · {} ctx · {:.1}s_",
            embed_cfg.similarity_threshold,
            embed_cfg.top_k,
            chunks,
            agent.context_tokens(),
            secs,
        );
        println!();
        println!("{}", response.answer.trim());
        println!();

        transcript.push((msg.clone(), response.answer));
    }

    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Print the ranked retrieval results for `query` — the same embedder, top-k,
/// cosine threshold, and pipeline/corpus filter the generation uses.
/// `RagResponse` only reports a chunk *count*, so retrieval-quality runs need
/// these per-chunk results.  Scores come from the store's active ranker.
async fn print_retrieval(
    embedder: &dyn Embedder,
    store: &dyn VectorStore,
    embed_cfg: &EmbedConfig,
    filter: &PipelineFilter,
    query: &str,
) {
    let embedded = match embedder.embed(vec![query.to_string()]).await {
        Ok(embedded) => embedded,
        Err(e) => {
            println!("_Embedding error: {e}_");
            println!();
            return;
        }
    };
    let Some((_, query_vec)) = embedded.into_iter().next() else {
        println!("_Embedding error: no vector returned._");
        println!();
        return;
    };
    match store
        .search_filtered(
            &query_vec,
            query,
            embed_cfg.top_k,
            embed_cfg.similarity_threshold,
            filter,
        )
        .await
    {
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

fn chrono_lite() -> Result<String> {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map_err(|_| anyhow!("'date' command not found"))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
