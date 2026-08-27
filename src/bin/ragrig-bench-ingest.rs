//! The ingestion process: walks every requested provenance (pipeline ×
//! corpus) and builds the combined vector database in the workspace.

use anyhow::{Context, Result};
use clap::Parser;
use ragrig::sync_corpus_with_pipeline;
use ragrig_bench::{
    apply_mock_mode, build_embedder, chunk_config, load_config, resolve_corpora, resolve_pipelines,
    validate_corpora,
};
use std::io::Write;
use std::path::PathBuf;

/// Ingest every requested provenance into the workspace vector store.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML benchmark configuration file.
    config: String,

    /// Workspace directory for the vector store (created if missing).
    #[arg(short = 'w', long, default_value = ".ragrig_bench")]
    workspace: PathBuf,

    /// Write the ingestion log to this file instead of stderr.
    #[arg(short = 'o', long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Run with the offline mock embedder (deterministic bag-of-words)
    /// instead of the `[embed]` backend — test the pipeline structure
    /// without maintaining a separate mock config.
    #[arg(short = 'm', long)]
    mock: bool,

    /// Delete the existing vector database and rebuild it from scratch.
    /// The database is bound to the embedder that built it, so switch
    /// embedders (e.g. live → `--mock`, or back) with `--reindex` — without
    /// it, ingestion fails with an embedder-mismatch error.
    #[arg(short = 'r', long)]
    reindex: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = load_config(&cli.config)?;
    if cli.mock {
        apply_mock_mode(&mut config);
        eprintln!("Mock mode: deterministic embedder + canned answers.");
    }
    validate_corpora(&config)?;

    std::fs::create_dir_all(&cli.workspace)
        .with_context(|| format!("creating workspace {}", cli.workspace.display()))?;

    // ── Ingestion log: stderr, or the `--out` file ──────────────────────
    let mut log: Box<dyn Write> = match &cli.out {
        Some(path) => {
            eprintln!("Writing ingestion log to {} …", path.display());
            Box::new(std::io::BufWriter::new(
                std::fs::File::create(path)
                    .with_context(|| format!("creating ingestion log {}", path.display()))?,
            ))
        }
        None => Box::new(std::io::stderr()),
    };

    let embedder = build_embedder(&config.embed, config.mock.embedder)?;
    // The workspace database is bound to ONE embedder: ragrig's
    // embedder-metadata guard rejects a different model/dimension on first
    // insert.  Print the identity up front so mismatches are self-evident.
    writeln!(
        log,
        "Workspace: {} — embedder: {}/{} ({} dims)",
        cli.workspace.display(),
        embedder.backend_name(),
        embedder.model_name(),
        embedder.dimension()
    )?;

    // ── --reindex: drop the existing database so the new embedder can
    //    rebuild it.  A store built by another embedder cannot be reused
    //    (ragrig's embedder-metadata guard), so this is the recovery path
    //    for the embedder-mismatch error.
    if cli.reindex {
        let store_path = cli.workspace.join(".ragrig_store");
        if store_path.is_dir() {
            writeln!(
                log,
                "Reindex: removing existing vector database {} …",
                store_path.display()
            )?;
            std::fs::remove_dir_all(&store_path).with_context(|| {
                format!("removing existing vector database {}", store_path.display())
            })?;
        } else if store_path.is_file() {
            // The BruteForceStore is a single MessagePack file.
            writeln!(
                log,
                "Reindex: removing existing vector database {} …",
                store_path.display()
            )?;
            std::fs::remove_file(&store_path).with_context(|| {
                format!("removing existing vector database {}", store_path.display())
            })?;
        }
    }
    let chunk_cfg = chunk_config(&config.parse);
    let corpus_entries = resolve_corpora(&config.corpus_dirs, log.as_mut())?;
    let pipelines = resolve_pipelines(&config.pipelines)?;

    let store = ragrig::store::open_store(&cli.workspace).await?;
    for pipeline in &pipelines {
        for corpus_entry in &corpus_entries {
            writeln!(
                log,
                "Ingesting pipeline '{}' into corpus '{}' …",
                pipeline.label, corpus_entry.name
            )?;
            let corpus = corpus_entry.corpus_for();
            let stats = sync_corpus_with_pipeline(
                &*corpus,
                &pipeline.parsers,
                &*pipeline.chunker,
                &*embedder,
                &chunk_cfg,
                &*store,
                &pipeline.label,
            )
            .await?;
            let indexed: usize = stats.iter().map(|s| s.chunks).sum();
            writeln!(
                log,
                "  {} chunks in store ({indexed} indexed in this run).",
                store.len()
            )?;
        }
    }

    writeln!(
        log,
        "Done: {} provenance(s) indexed in {}.",
        pipelines.len() * corpus_entries.len(),
        cli.workspace.display()
    )?;
    log.flush()?;
    Ok(())
}
