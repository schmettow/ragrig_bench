//! Shared configuration, resolution, mock components, and backend builders
//! for the `ragrig-bench` binaries.
//!
//! The workflow is split into two strictly sequential processes that share
//! the TOML config and the workspace vector store:
//!
//! - [`ragrig-bench-ingest`](https://docs.rs/ragrig-bench) walks every
//!   requested provenance (pipeline × corpus) and builds the combined vector
//!   database.
//! - `ragrig-bench-interact` runs the benchmark matrix against that database.
//!
//! Chunk provenance — the pipeline-scoped corpus names (`name::pipeline`) —
//! is the only addressing seam between the two processes; both derive them
//! deterministically from the shared config.

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use ragrig::{
    ChatAgentSpec, ChunkConfig, Chunker, Corpus, Document, DocumentData, DocumentId, Embedder,
    EmbedderSpec, FolderCorpus, Generator, HybridRrfRanker, MarkdownChunker, MmrDiversityRanker,
    MutexGenerator, Ranker, SimpleGenerator, VectorStore, WeightedFusionRanker, available_chunkers,
    parsers::{DocumentParsers, build_parsers},
    types::{ChatConfig, EmbedConfig, EmbeddingProvider, ParseConfig, Provider},
};
use serde::Deserialize;
use std::path::PathBuf;

// ── Config schema ───────────────────────────────────────────────────────────

/// The TOML benchmark configuration, shared by ingest and interact.
#[derive(Deserialize)]
pub struct BenchmarkConfig {
    /// Independent queries — no history between them.
    #[serde(default, alias = "questions")]
    pub queries: Vec<String>,
    /// Sequential chat — each response feeds into the next prompt.
    #[serde(default)]
    pub chat: Vec<String>,
    /// Named document corpora in `name=path` form.  A path prefixed with
    /// `@fixtures/` extracts the library's embedded fixture set
    /// (`pdf`, `rmd`, `html`) into a temp directory; `@mock/<n>` synthesises
    /// `n` deterministic in-memory documents.
    #[serde(default)]
    pub corpus_dirs: Vec<String>,
    /// Chat agents forming the benchmark matrix.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// (parser, chunker) pipelines to compare.  Every pipeline indexes all
    /// corpora into the shared store and is queried separately at runtime
    /// via pipeline provenance.  Empty = one default pipeline (full parser
    /// registry, `MarkdownChunker`).
    #[serde(default)]
    pub pipelines: Vec<PipelineConfig>,
    /// Embedding / retrieval settings.  Omitted fields use the library
    /// defaults (`EmbedConfig::default()`).
    #[serde(default, deserialize_with = "deserialize_with_defaults")]
    pub embed: EmbedConfig,
    /// Chunk size / overlap applied by every pipeline.
    #[serde(default, deserialize_with = "deserialize_with_defaults")]
    pub parse: ParseConfig,
    /// Mock components — offline, deterministic replacements for the live
    /// backends (see `mock.toml`).
    #[serde(default)]
    pub mock: MockConfig,
    /// Rankers to sweep in the benchmark matrix (interaction side — ranking
    /// happens at query time, so rankers are not part of ingestion or
    /// provenance).  Empty = the single default hybrid RRF ranker.
    #[serde(default)]
    pub rankers: Vec<RankerConfig>,
}

/// One ranker variant of the benchmark matrix.
#[derive(Deserialize, Clone, Default)]
pub struct RankerConfig {
    /// Ranker name: `"rrf"` (default hybrid), `"cosine"` (α=1), `"bm25"`
    /// (α=0), `"weighted"` (α=0.5 or custom), `"mmr"` (diversity re-ranking
    /// over RRF).
    pub name: String,
    /// [`WeightedFusionRanker`] alpha (0.0–1.0): 0 = BM25, 1 = cosine.
    pub alpha: Option<f64>,
    /// [`MmrDiversityRanker`] lambda (0.0–1.0 diversity penalty).
    pub lambda: Option<f64>,
}

/// Offline, deterministic mock backends for fast combinatorial runs.
#[derive(Deserialize, Default)]
pub struct MockConfig {
    /// Replace the live embedder with a deterministic bag-of-words
    /// embedder — retrieval works offline (word-overlap cosine).
    #[serde(default)]
    pub embedder: bool,
}

/// One benchmark matrix agent.
#[derive(Deserialize)]
pub struct AgentConfig {
    /// Optional display label; defaults to `<provider> / <model>`, or
    /// `mock` when `answer` is set.
    pub label: Option<String>,
    /// When set, the agent answers with this template (the `{query}`
    /// placeholder is replaced by the user query) via a mock generator —
    /// no network, deterministic output.
    pub answer: Option<String>,
    /// Chat settings — the library's `ChatConfig` (provider, model,
    /// generation params, context window, system-prompt file, timeout).
    /// Ignored when `answer` is set; omitted fields use `ChatConfig::default()`.
    #[serde(default, deserialize_with = "deserialize_with_defaults")]
    pub chat: ChatConfig,
}

/// One (parser, chunker) pipeline to index and query.
#[derive(Deserialize, Clone, Default)]
pub struct PipelineConfig {
    /// PDF parser by name (`"unpdf"`, `"pdf-extract"`, `"pdfsink"`,
    /// `"sloppy-pdf"`, `"kreuzberg"`, `"vision-pdf"`).  `None` = the full
    /// registry — the first parser that succeeds (the default pipeline).
    pub parser: Option<String>,
    /// Chunker by name (`"markdown"`, `"token"`, `"chunkedrs-*"`, …).
    /// `None` = `MarkdownChunker`.
    pub chunker: Option<String>,
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

/// Load the shared TOML config.
pub fn load_config(path: &str) -> Result<BenchmarkConfig> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading config {path}"))?;
    toml::from_str(&raw).with_context(|| format!("parsing config {path}"))
}

/// Ingestion-side validation: at least one corpus.
pub fn validate_corpora(config: &BenchmarkConfig) -> Result<()> {
    if config.corpus_dirs.is_empty() {
        bail!("config must contain at least one 'corpus_dirs' entry.");
    }
    Ok(())
}

/// Interaction-side validation: the benchmark matrix is well-formed.
pub fn validate_matrix(config: &BenchmarkConfig) -> Result<()> {
    if config.queries.is_empty() && config.chat.is_empty() {
        bail!("config must contain 'queries' or 'chat'.");
    }
    if !config.queries.is_empty() && !config.chat.is_empty() {
        bail!("config must not contain both 'queries' and 'chat' — pick one.");
    }
    if config.agents.is_empty() {
        bail!("config must contain at least one [[agents]] entry.");
    }
    validate_corpora(config)
}

// ── Provenance naming (the ingest ↔ interact seam) ──────────────────────────

/// The corpus name part of a `name=path` spec.
pub fn corpus_name(spec: &str) -> Result<String> {
    spec.split_once('=')
        .map(|(name, _)| name.to_string())
        .ok_or_else(|| anyhow!("corpus '{spec}' must have the form 'name=path'"))
}

/// All corpus names from the config, in order.
pub fn corpus_names(specs: &[String]) -> Result<Vec<String>> {
    specs.iter().map(|spec| corpus_name(spec)).collect()
}

/// The stable pipeline label — shared by ingest and interact so the
/// provenance names they derive always match.
pub fn pipeline_label(spec: &PipelineConfig) -> Result<String> {
    let parser_label = spec
        .parser
        .clone()
        .unwrap_or_else(|| "all-parsers".to_string());
    let chunker_name = match &spec.chunker {
        Some(name) => {
            if !available_chunkers().iter().any(|c| c.name() == name) {
                bail!("unknown chunker '{name}'. Available: {}", chunker_names());
            }
            name.clone()
        }
        None => "markdown".to_string(),
    };
    Ok(format!("{parser_label} · {chunker_name}"))
}

/// All pipeline labels, in order.  An empty pipeline list yields the single
/// default pipeline (`all-parsers · markdown`).
pub fn pipeline_labels(specs: &[PipelineConfig]) -> Result<Vec<String>> {
    let specs: Vec<PipelineConfig> = if specs.is_empty() {
        vec![PipelineConfig::default()]
    } else {
        specs.to_vec()
    };
    specs.iter().map(pipeline_label).collect()
}

/// The store-level corpus name for one (corpus, pipeline) pair — the
/// provenance by which interact addresses what ingest built.
pub fn scoped_corpus_name(corpus: &str, pipeline: &str) -> String {
    format!("{corpus}::{pipeline}")
}

// ── Resolution (ingestion side) ─────────────────────────────────────────────

/// A pipeline resolved to concrete backends, ready to ingest.
pub struct ResolvedPipeline {
    /// Stable label — doubles as part of the provenance name.
    pub label: String,
    pub parsers: DocumentParsers,
    pub chunker: Box<dyn Chunker>,
}

/// A corpus source resolved from the config.
pub struct ResolvedCorpus {
    /// User-facing corpus name from `corpus_dirs`.
    pub name: String,
    pub source: CorpusSource,
    /// Keeps extracted fixture temp dirs alive for the whole run.
    pub _temp: Option<tempfile::TempDir>,
}

/// Where a corpus's raw documents come from.
pub enum CorpusSource {
    /// A directory of documents.
    Folder(PathBuf),
    /// `n` synthetic in-memory documents (deterministic topics).
    Mock(usize),
}

impl ResolvedCorpus {
    /// The store-level corpus name for one pipeline — see
    /// [`scoped_corpus_name`].
    pub fn scoped_name(&self, pipeline: &ResolvedPipeline) -> String {
        scoped_corpus_name(&self.name, &pipeline.label)
    }

    /// The corpus to ingest for one pipeline.
    pub fn corpus_for(&self, pipeline: &ResolvedPipeline) -> Box<dyn Corpus> {
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

/// Resolve the `name=path` corpus specs.  `@fixtures/<fmt>` extracts the
/// library's fixture set into a temp dir; `@mock/<n>` synthesises `n`
/// deterministic in-memory documents.  Resolution notes go to `log`.
pub fn resolve_corpora(
    specs: &[String],
    log: &mut dyn std::io::Write,
) -> Result<Vec<ResolvedCorpus>> {
    specs
        .iter()
        .map(|spec| {
            let (name, raw) = spec
                .split_once('=')
                .ok_or_else(|| anyhow!("corpus '{spec}' must have the form 'name=path'"))?;
            let (source, temp) = if let Some(format) = raw.strip_prefix("@fixtures/") {
                let (p, dir) = ragrig::fixtures::extract_fixtures(format)?;
                writeln!(log, "  Extracted {format} fixtures → {}", p.display())?;
                (CorpusSource::Folder(p), Some(dir))
            } else if let Some(count) = raw.strip_prefix("@mock/") {
                let count: usize = count
                    .parse()
                    .with_context(|| format!("mock corpus size '{count}' in corpus '{spec}'"))?;
                writeln!(log, "  Mock corpus '{name}': {count} synthetic documents.")?;
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
pub fn resolve_pipelines(specs: &[PipelineConfig]) -> Result<Vec<ResolvedPipeline>> {
    let specs: Vec<PipelineConfig> = if specs.is_empty() {
        vec![PipelineConfig::default()]
    } else {
        specs.to_vec()
    };
    specs
        .iter()
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
            Ok(ResolvedPipeline {
                label: pipeline_label(spec)?,
                parsers: parsers_for(spec.parser.as_deref())?,
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

// ── Rankers (interaction side) ──────────────────────────────────────────────

/// The ranker list to sweep, defaulting to the single default hybrid RRF
/// ranker when the config omits `[[rankers]]`.
pub fn ranker_configs(specs: &[RankerConfig]) -> Vec<RankerConfig> {
    if specs.is_empty() {
        vec![RankerConfig::default()]
    } else {
        specs.to_vec()
    }
}

/// Display label for a ranker config (`"rrf"` when the name is empty).
pub fn ranker_label(cfg: &RankerConfig) -> String {
    match cfg.name.as_str() {
        "" => "rrf".to_string(),
        name => name.to_string(),
    }
}

/// Resolve a ranker config into a concrete ranker.  The ranker is swapped
/// into the store at query time (`VectorStore::set_ranker`) — it is not part
/// of ingestion or provenance.
pub fn build_ranker(cfg: &RankerConfig) -> Result<Box<dyn Ranker>> {
    match cfg.name.as_str() {
        "" | "rrf" => Ok(Box::new(HybridRrfRanker::default())),
        "cosine" => Ok(Box::new(WeightedFusionRanker { alpha: 1.0 })),
        "bm25" => Ok(Box::new(WeightedFusionRanker { alpha: 0.0 })),
        "weighted" => Ok(Box::new(WeightedFusionRanker {
            alpha: cfg.alpha.unwrap_or(0.5),
        })),
        "mmr" => Ok(Box::new(MmrDiversityRanker {
            lambda: cfg.lambda.unwrap_or(0.5),
            inner: Box::new(HybridRrfRanker::default()),
        })),
        other => bail!("unknown ranker '{other}'. Available: rrf, cosine, bm25, weighted, mmr"),
    }
}

// ── Mock components (offline, deterministic) ────────────────────────────────

/// Canned-answer generator.  Implemented as a [`SimpleGenerator`] — the
/// library's sync seam — wrapped in [`MutexGenerator`] to satisfy `Generator`.
#[derive(Debug, Clone)]
pub struct MockResponder {
    pub label: String,
    pub template: String,
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
pub struct MockEmbedder;

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
pub struct MockCorpus {
    pub name: String,
    pub count: usize,
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

/// Build the embedding backend from `[embed]`, or the mock embedder when
/// `[mock] embedder` is set.
pub fn build_embedder(embed: &EmbedConfig, mock: bool) -> Result<Box<dyn Embedder>> {
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

/// Build one [`RagAgent`](ragrig::RagAgent) for the matrix — a mock generator
/// when the agent sets `answer`, otherwise the configured chat backend.
pub fn build_agent(
    agent_cfg: &AgentConfig,
    embed_cfg: &EmbedConfig,
    mock_cfg: &MockConfig,
    store: Box<dyn VectorStore>,
) -> Result<ragrig::RagAgent> {
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
    let mut builder = ragrig::RagAgent::builder()
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

/// Display label for a matrix agent.
pub fn agent_label(cfg: &AgentConfig) -> String {
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

/// The chunking configuration shared by every pipeline.
pub fn chunk_config(parse: &ParseConfig) -> ChunkConfig {
    ChunkConfig {
        size: parse.chunk_size,
        overlap: parse.chunk_overlap,
    }
}
