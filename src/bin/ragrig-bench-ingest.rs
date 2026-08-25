//! The ingestion process: walks every requested provenance (pipeline ×
//! corpus) and builds the combined vector database in the workspace.

use anyhow::{Context, Result};
use clap::Parser;
use ragrig::sync_corpus;
use ragrig_bench::{
    build_embedder, chunk_config, load_config, resolve_corpora, resolve_pipelines, validate_corpora,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = load_config(&cli.config)?;
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
    let chunk_cfg = chunk_config(&config.parse);
    let corpus_entries = resolve_corpora(&config.corpus_dirs, log.as_mut())?;
    let pipelines = resolve_pipelines(&config.pipelines)?;

    let store = ragrig::store::open_store(&cli.workspace).await?;
    for pipeline in &pipelines {
        for corpus_entry in &corpus_entries {
            let scoped = corpus_entry.scoped_name(pipeline);
            writeln!(log, "Ingesting provenance '{scoped}' …")?;
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
