use anyhow::Result;
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
use std::io::BufRead;

const CHAT_MODEL: &str = "gemma2:latest";
const EMBED_MODEL: &str = "nomic-embed-text";

#[tokio::main]
async fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;

    let args = Args {
        folder: cwd,
        provider: Provider::Ollama,
        model: CHAT_MODEL.into(),
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
        top_k: 25,
        similarity_threshold: 0.3,
        model_ctx_tokens: 4096,
        context_size_forced: false,
    };

    let embedder = EmbedderSpec::Ollama {
        model: EMBED_MODEL.into(),
    }
    .build()?;
    let chat = ChatAgentSpec::Ollama {
        model: CHAT_MODEL.into(),
    }
    .build()?;
    let parsers = DocumentParsers::new(build_parsers());
    let prompts = SystemPrompts::default();

    // ---- Incremental indexing ----
    let store = open_store(&args.folder).await?;
    let hashes_path = get_embeddings_file_path(&args.folder);

    let current_hashes = get_document_file_hashes(&args.folder)?;
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
        eprintln!("Indexing {} changed document(s) …", changed.len());
        embed_documents(&*embedder, &parsers, &args, changed, &*store).await?;
    }
    remove_deleted_embeddings(&*store, &current_hashes).await?;
    update_file_hashes(&current_hashes, &hashes_path)?;
    eprintln!("Store: {} chunks ready.", store.len());

    // ---- Read query from stdin ----
    let mut query = String::new();
    std::io::stdin().lock().read_line(&mut query)?;
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("Empty query.");
    }

    // ---- Search ----
    let embedded = embedder.embed(vec![query.to_string()]).await?;
    let query_vec: Vec<f32> = embedded
        .first()
        .map(|(_, v)| v.clone())
        .ok_or_else(|| anyhow::anyhow!("Failed to get query embedding"))?;

    let results = store
        .search(&query_vec, query, args.top_k, args.similarity_threshold)
        .await?;
    if results.is_empty() {
        println!("No relevant documents found.");
        return Ok(());
    }

    // ---- Generate ----
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
        query
    );

    chat.generate_stream(&full_prompt, &|token| {
        print!("{}", token);
        let _ = std::io::Write::flush(&mut std::io::stdout());
    })
    .await?;
    println!();

    Ok(())
}
