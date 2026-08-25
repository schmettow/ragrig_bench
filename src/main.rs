use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::Parser;
use ragrig::{
    ChatAgentSpec, ChunkConfig, Chunker, Corpus, Document, DocumentData, DocumentId, Embedder,
    EmbedderSpec, FolderCorpus, Generator, MarkdownChunker, MutexGenerator, PipelineFilter,
    RagAgent, RagResponse, SimpleGenerator, VectorStore, available_chunkers,
    parsers::{DocumentParsers, build_parsers},
    store::open_store,
    sync_corpus,
    types::{ChatConfig, EmbedConfig, EmbeddingProvider, ParseConfig, Provider},
};
use serde::Deserialize;
use std::io::Write;
use std::path::PathBuf;

// ── CLI ─────────────────────────────────────────────────────────────────────

/// Benchmark ragrig retrieval quality across document corpora, queries,
/// (parser, chunker) pipelines, and chat backends.  Results are written to
/// stdout (or the `--out` file) as Markdown.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML benchmark configuration file.
    config: String,

    /// Workspace directory for the vector store (created if missing).
    /// One shared store holds every indexed pipeline.
    #[arg(short = 'w', long, default_value = ".ragrig_bench")]
    workspace: PathBuf,

    /// Write the Markdown report to this file instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE")]
    out: Option<PathBuf>,
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
    /// Mock components — offline, deterministic replacements for the live
    /// backends (see `mock.toml`).
    #[serde(default)]
    mock: MockConfig,
}

/// Offline, deterministic mock backends for fast combinatorial runs.
#[derive(Deserialize, Default)]
struct MockConfig {
    /// Replace the live embedder with a deterministic bag-of-words
    /// embedder — retrieval works offline (word-overlap cosine).
    #[serde(default)]
    embedder: bool,
}

#[derive(Deserialize)]
struct AgentConfig {
    /// Optional display label; defaults to `<provider> / <model>`, or
    /// `mock` when `answer` is set.
    label: Option<String>,
    /// When set, the agent answers with this template (the `{query}`
    /// placeholder is replaced by the user query) via a mock generator —
    /// no network, deterministic output.
    answer: Option<String>,
    /// Chat settings — the library's `ChatConfig` (provider, model,
    /// generation params, context window, system-prompt file, timeout).
    /// Ignored when `answer` is set; omitted fields use `ChatConfig::default()`.
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
    source: CorpusSource,
    /// Keeps extracted fixture temp dirs alive for the whole run.
    _temp: Option<tempfile::TempDir>,
}

enum CorpusSource {
    /// A directory of documents.
    Folder(PathBuf),
    /// `n` synthetic in-memory documents (deterministic topics).
    Mock(usize),
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

    /// The corpus to ingest for one pipeline.
    fn corpus_for(&self, pipeline: &ResolvedPipeline) -> Box<dyn Corpus> {
        let name = self.scoped_name(pipeline);
        match &self.source {
            CorpusSource::Folder(path) => Box::new(FolderCorpus::named(name, path.clone())),
            CorpusSource::Mock(count) => Box::new(MockCorpus {
                name,
                count: *count,
            }),
        }
    }
}

// ── Resolution helpers ──────────────────────────────────────────────────────

/// Resolve the `name=path` corpus specs.  `@fixtures/<fmt>` extracts the
/// library's fixture set into a temp dir; `@mock/<n>` synthesises `n`
/// deterministic in-memory documents.
fn resolve_corpora(specs: &[String]) -> Result<Vec<ResolvedCorpus>> {
    specs
        .iter()
        .map(|spec| {
            let (name, raw) = spec
                .split_once('=')
                .ok_or_else(|| anyhow!("corpus '{spec}' must have the form 'name=path'"))?;
            let (source, temp) = if let Some(format) = raw.strip_prefix("@fixtures/") {
                let (p, dir) = ragrig::fixtures::extract_fixtures(format)?;
                eprintln!("  Extracted {format} fixtures → {}", p.display());
                (CorpusSource::Folder(p), Some(dir))
            } else if let Some(count) = raw.strip_prefix("@mock/") {
                let count: usize = count
                    .parse()
                    .with_context(|| format!("mock corpus size '{count}' in corpus '{spec}'"))?;
                eprintln!("  Mock corpus '{name}': {count} synthetic documents.");
                (CorpusSource::Mock(count), None)
            } else {
                (CorpusSource::Folder(PathBuf::from(raw)), None)
            };
            Ok(ResolvedCorpus {
                name: name.to_string(),
                source,
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

// ── Mock components (offline, deterministic) ────────────────────────────────

/// Canned-answer generator.  Implemented as a [`SimpleGenerator`] — the
/// library's sync seam — wrapped in [`MutexGenerator`] to satisfy `Generator`.
#[derive(Debug, Clone)]
struct MockResponder {
    label: String,
    template: String,
}

impl SimpleGenerator for MockResponder {
    fn respond(&mut self, prompt: &str) -> String {
        // The prompt ends with `<|user|>\n<query>\n<|assistant|>\n`; take the
        // last user turn so `{query}` works in chat mode too.
        let query = prompt
            .rsplit_once("<|user|>\n")
            .map(|(_, tail)| tail)
            .unwrap_or(prompt)
            .split("<|assistant|>")
            .next()
            .unwrap_or("")
            .trim();
        self.template.replace("{query}", query)
    }

    fn backend_name(&self) -> &'static str {
        "Mock"
    }

    fn model_name(&self) -> String {
        self.label.clone()
    }
}

/// Deterministic bag-of-words embedder: every alphanumeric word hashes to one
/// dimension and contributes +1.  Similar texts share words, so cosine
/// retrieval behaves like word-overlap search — good enough to exercise the
/// full retrieval pipeline offline.
#[derive(Debug, Clone)]
struct MockEmbedder;

/// Vector dimensionality for [`MockEmbedder`].
const MOCK_EMBED_DIM: usize = 128;

impl MockEmbedder {
    fn vectorize(text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; MOCK_EMBED_DIM];
        for word in text.split(|c: char| !c.is_alphanumeric()) {
            if word.is_empty() {
                continue;
            }
            let hash = fnv1a(&word.to_lowercase());
            vector[(hash as usize) % MOCK_EMBED_DIM] += 1.0;
        }
        vector
    }
}

/// FNV-1a 64-bit hash — deterministic across runs and platforms.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[async_trait]
impl Embedder for MockEmbedder {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<(String, Vec<f32>)>> {
        Ok(texts
            .into_iter()
            .map(|text| {
                let vector = Self::vectorize(&text);
                (text, vector)
            })
            .collect())
    }

    fn backend_name(&self) -> &'static str {
        "Mock"
    }

    fn model_name(&self) -> &str {
        "mock-bow"
    }

    fn dimension(&self) -> usize {
        MOCK_EMBED_DIM
    }
}

/// Synthetic in-memory corpus (`@mock/<n>` specs): `n` deterministic
/// Markdown documents cycling through a few fixed topics, so the mock
/// embedder's word-overlap retrieval returns thematically coherent chunks.
#[derive(Debug)]
struct MockCorpus {
    name: String,
    count: usize,
}

fn mock_document_text(i: usize) -> String {
    const TOPICS: [(&str, &str); 4] = [
        (
            "Quantum entanglement",
            "Entangled particles share a correlated quantum state; measuring one particle instantly determines the other, regardless of the distance between them.",
        ),
        (
            "Linear regression",
            "Linear regression models a response variable as a weighted sum of predictors plus Gaussian noise, fitted by ordinary least squares.",
        ),
        (
            "Bayesian statistics",
            "Bayesian statistics updates prior beliefs with observed data through Bayes' theorem to produce posterior probability distributions.",
        ),
        (
            "Vector stores",
            "A vector store indexes text chunks by embedding similarity; hybrid search fuses BM25 keyword scoring with cosine vector ranking.",
        ),
    ];
    let (title, body) = &TOPICS[i % TOPICS.len()];
    let variant = i / TOPICS.len();
    format!(
        "# {title} — part {variant}\n\n{body}\n\nPart {variant} of {title} discusses the same topic in different words.\n"
    )
}

#[async_trait]
impl Corpus for MockCorpus {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn documents(&self) -> Result<Vec<Document>> {
        Ok((0..self.count)
            .map(|i| Document {
                corpus: self.name.clone(),
                id: DocumentId::from(format!("doc-{i:03}.md")),
                format: "md".to_string(),
                data: DocumentData::Bytes(mock_document_text(i).into_bytes()),
                meta: Default::default(),
            })
            .collect())
    }
}

// ── Backend builders (library spec enums) ──────────────────────────────────

fn build_embedder(embed: &EmbedConfig, mock: bool) -> Result<Box<dyn Embedder>> {
    if mock {
        return Ok(Box::new(MockEmbedder));
    }
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
    mock_cfg: &MockConfig,
    store: Box<dyn VectorStore>,
) -> Result<RagAgent> {
    let chat: Box<dyn Generator> = match &agent_cfg.answer {
        Some(template) => Box::new(MutexGenerator::new(MockResponder {
            label: agent_cfg
                .label
                .clone()
                .unwrap_or_else(|| "mock".to_string()),
            template: template.clone(),
        })),
        None => build_chat_agent(&agent_cfg.chat)?,
    };
    let chat_cfg = &agent_cfg.chat;
    let mut builder = RagAgent::builder()
        .chat(chat)
        .embed(build_embedder(embed_cfg, mock_cfg.embedder)?)
        .store(store)
        .context_tokens(chat_cfg.context_tokens)
        .top_k(embed_cfg.top_k)
        .similarity_threshold(embed_cfg.similarity_threshold);
    if let Some(path) = &chat_cfg.system_prompt_path {
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
    if cfg.answer.is_some() {
        return "mock".to_string();
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

    let embedder = build_embedder(&config.embed, config.mock.embedder)?;
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
                let corpus = corpus_entry.corpus_for(pipeline);
                let stats = sync_corpus(
                    &*corpus,
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

    // ── Report writer: stdout, or the `--out` file ─────────────────────
    let mut out: Box<dyn Write> = match &cli.out {
        Some(path) => {
            eprintln!("Writing report to {} …", path.display());
            Box::new(std::io::BufWriter::new(
                std::fs::File::create(path)
                    .with_context(|| format!("creating report {}", path.display()))?,
            ))
        }
        None => Box::new(std::io::stdout()),
    };

    let today = chrono_lite()?;
    writeln!(out, "# RAG Benchmark — {today}")?;
    writeln!(out)?;

    let chat_mode = !config.chat.is_empty();
    let multi_pipeline = pipelines.len() > 1;
    let multi_corpus = corpus_entries.len() > 1;

    // ── Phase 2: run the benchmark matrix ───────────────────────────────
    for agent_cfg in &config.agents {
        let label = agent_label(agent_cfg);
        let store = open_store(&cli.workspace).await?;
        let mut agent = build_agent(agent_cfg, &config.embed, &config.mock, store)?;

        writeln!(out, "## {label}")?;
        writeln!(out)?;

        for pipeline in &pipelines {
            if multi_pipeline {
                writeln!(out, "### {}", pipeline.label)?;
                writeln!(out)?;
            }
            for corpus_entry in &corpus_entries {
                if multi_corpus {
                    writeln!(out, "#### {}", corpus_entry.name)?;
                    writeln!(out)?;
                }
                let filter = PipelineFilter::for_corpus(corpus_entry.scoped_name(pipeline));
                agent.set_search_filter(Some(filter.clone()));

                if chat_mode {
                    run_chat_mode(
                        &mut agent,
                        out.as_mut(),
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

                        writeln!(out, "**Q{}:** {query}", qi + 1)?;
                        writeln!(out)?;

                        print_retrieval(
                            out.as_mut(),
                            agent.embedder(),
                            agent.store(),
                            &config.embed,
                            &filter,
                            query,
                        )
                        .await?;

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
                        writeln!(
                            out,
                            "_t={} · k={} → {} in context · {} ctx · {:.1}s_",
                            config.embed.similarity_threshold,
                            config.embed.top_k,
                            chunks,
                            agent.context_tokens(),
                            secs,
                        )?;
                        writeln!(out)?;
                        writeln!(out, "{}", response.answer.trim())?;
                        writeln!(out)?;
                    }
                }
            }
        }
    }

    out.flush()?;
    Ok(())
}

// ── Chat mode ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_chat_mode(
    agent: &mut RagAgent,
    out: &mut dyn Write,
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

        print_retrieval(out, agent.embedder(), agent.store(), embed_cfg, filter, msg).await?;

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

        writeln!(out, "**Q:** {msg}")?;
        writeln!(out)?;
        writeln!(
            out,
            "_t={} · k={} → {} in context · {} ctx · {:.1}s_",
            embed_cfg.similarity_threshold,
            embed_cfg.top_k,
            chunks,
            agent.context_tokens(),
            secs,
        )?;
        writeln!(out)?;
        writeln!(out, "{}", response.answer.trim())?;
        writeln!(out)?;

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
    out: &mut dyn Write,
    embedder: &dyn Embedder,
    store: &dyn VectorStore,
    embed_cfg: &EmbedConfig,
    filter: &PipelineFilter,
    query: &str,
) -> Result<()> {
    let embedded = match embedder.embed(vec![query.to_string()]).await {
        Ok(embedded) => embedded,
        Err(e) => {
            writeln!(out, "_Embedding error: {e}_")?;
            writeln!(out)?;
            return Ok(());
        }
    };
    let Some((_, query_vec)) = embedded.into_iter().next() else {
        writeln!(out, "_Embedding error: no vector returned._")?;
        writeln!(out)?;
        return Ok(());
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
                writeln!(out, "_No chunks passed the similarity threshold._")?;
            } else {
                writeln!(out, "**Retrieved** (ranked by store ranker):")?;
                for (i, r) in results.iter().enumerate() {
                    let snippet: String = r
                        .chunk
                        .text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let snippet: String = snippet.chars().take(96).collect();
                    writeln!(
                        out,
                        "{}. **{}** — {} — _{snippet}_",
                        i + 1,
                        r.chunk.document,
                        r.score
                    )?;
                }
            }
            writeln!(out)?;
        }
        Err(e) => {
            writeln!(out, "_Retrieval error: {e}_")?;
            writeln!(out)?;
        }
    }
    Ok(())
}

fn chrono_lite() -> Result<String> {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .map_err(|_| anyhow!("'date' command not found"))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
