use anyhow::Result;
use ragrig::{
    ChatAgentSpec, ChunkConfig, EmbedderSpec, FolderCorpus, GenerationParams, MarkdownChunker,
    RagAgent,
    parsers::{DocumentParsers, build_parsers},
    store::open_store,
    vector::sync_corpus,
};
use std::io::BufRead;

const CHAT_MODEL: &str = "gemma2:latest";
const EMBED_MODEL: &str = "nomic-embed-text";

#[tokio::main]
async fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;

    let config = ChunkConfig {
        size: 1024,
        overlap: 128,
    };

    let embedder = EmbedderSpec::ollama(EMBED_MODEL).build()?;
    let parsers = DocumentParsers::new(build_parsers());

    // ---- Incremental indexing ----
    let store = open_store(&cwd).await?;
    let corpus = FolderCorpus::named("cwd", cwd.clone());
    let stats = sync_corpus(
        &corpus,
        &parsers,
        &MarkdownChunker,
        &*embedder,
        &config,
        &*store,
    )
    .await?;
    let indexed: usize = stats.iter().map(|s| s.chunks).sum();
    eprintln!(
        "Store: {} chunks ready ({} in this run).",
        store.len(),
        indexed
    );

    // ---- Read query from stdin ----
    let mut query = String::new();
    std::io::stdin().lock().read_line(&mut query)?;
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("Empty query.");
    }

    // ---- Build agent and generate ----
    let chat = ChatAgentSpec::ollama(CHAT_MODEL, GenerationParams::default(), None).build()?;

    let agent = RagAgent::builder()
        .chat(chat)
        .embed(EmbedderSpec::ollama(EMBED_MODEL).build()?)
        .store(store)
        .top_k(25)
        .similarity_threshold(0.04)
        .build()?;

    let response = agent
        .generate_with_context_detailed(query, &[] as &[(&str, &str)])
        .await?;

    println!("{}", response.answer.trim());
    if let Some(chunks) = response.chunks_retrieved {
        eprintln!(
            "  [{} chunks, {:.1}s]",
            chunks,
            response.elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0)
        );
    }

    Ok(())
}
