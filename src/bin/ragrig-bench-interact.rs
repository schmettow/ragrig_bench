//! The chat process: runs the benchmark matrix (agents × pipelines ×
//! corpora × queries) against the vector database built by
//! `ragrig-bench-ingest`.  Provenance is the seam: every cell is addressed
//! by its pipeline-scoped corpus name, and a helpful message is emitted when
//! a requested provenance is not in the database.

use anyhow::{Context, Result, bail};
use clap::Parser;
use ragrig::{Embedder, PipelineFilter, RagAgent, RagResponse, VectorStore};
use ragrig_bench::{
    agent_label, build_agent, build_ranker, cell_filter, corpus_names, load_config,
    pipeline_labels, ranker_configs, ranker_label, validate_matrix,
};
use std::io::Write;
use std::path::PathBuf;

/// Run the benchmark matrix against an ingested vector database.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML benchmark configuration file.
    config: String,

    /// Workspace directory holding the vector store built by
    /// `ragrig-bench-ingest`.
    #[arg(short = 'w', long, default_value = ".ragrig_bench")]
    workspace: PathBuf,

    /// Write the Markdown report to this file instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = load_config(&cli.config)?;
    validate_matrix(&config)?;

    // The vector database is built by the ingestion process.
    if !cli.workspace.join(".ragrig_store").exists() {
        bail!(
            "no vector database at {} — run `ragrig-bench-ingest {}` first.",
            cli.workspace.display(),
            cli.config
        );
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
    let corpus_names = corpus_names(&config.corpus_dirs)?;
    let pipeline_labels = pipeline_labels(&config.pipelines)?;
    let rankers = ranker_configs(&config.rankers);
    let multi_pipeline = pipeline_labels.len() > 1;
    let multi_corpus = corpus_names.len() > 1;
    let multi_ranker = rankers.len() > 1;

    // ── The benchmark matrix ───────────────────────────────────────────
    for agent_cfg in &config.agents {
        let label = agent_label(agent_cfg);
        let store = ragrig::store::open_store(&cli.workspace).await?;
        let mut agent = build_agent(agent_cfg, &config.embed, &config.mock, store)?;

        writeln!(out, "## {label}")?;
        writeln!(out)?;

        for pipeline_label in &pipeline_labels {
            if multi_pipeline {
                writeln!(out, "### {pipeline_label}")?;
                writeln!(out)?;
            }
            for corpus_name in &corpus_names {
                if multi_corpus {
                    writeln!(out, "#### {corpus_name}")?;
                    writeln!(out)?;
                }

                let scoped = format!("{corpus_name} :: {pipeline_label}");
                let filter = cell_filter(corpus_name, pipeline_label);

                // Provenance gate: the ingestion process must have built
                // this (pipeline, corpus) pair.
                if agent.store().count_matching(&filter).await == 0 {
                    let msg = format!(
                        "_Error: pipeline '{pipeline_label}' is not indexed for corpus '{corpus_name}' in the vector database at {} — run `ragrig-bench-ingest` first._",
                        cli.workspace.display()
                    );
                    eprintln!("  {msg}");
                    writeln!(out, "{msg}")?;
                    writeln!(out)?;
                    continue;
                }

                agent.set_search_filter(Some(filter.clone()));

                for ranker_cfg in &rankers {
                    if multi_ranker {
                        writeln!(out, "##### ranker: {}", ranker_label(ranker_cfg))?;
                        writeln!(out)?;
                    }
                    // The ranker lives in the store and is swapped at query
                    // time — it is not part of ingestion/provenance.
                    agent.store().set_ranker(build_ranker(ranker_cfg)?)?;

                    if chat_mode {
                        run_chat_mode(
                            &mut agent,
                            out.as_mut(),
                            &config.embed,
                            &filter,
                            &config.chat,
                            &label,
                            corpus_name,
                            &ranker_label(ranker_cfg),
                        )
                        .await?;
                    } else {
                        for (qi, query) in config.queries.iter().enumerate() {
                            eprintln!(
                                "  {label} / {pipeline_label} / {corpus_name} / {} ← \"{}\"",
                                ranker_label(ranker_cfg),
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
                                "_ranker={} · t={} · k={} → {} in context · {} ctx · {:.1}s_",
                                ranker_label(ranker_cfg),
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
    }

    out.flush()?;
    Ok(())
}

// ── Chat mode ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_chat_mode(
    agent: &mut RagAgent,
    out: &mut dyn Write,
    embed_cfg: &ragrig::types::EmbedConfig,
    filter: &PipelineFilter,
    messages: &[String],
    agent_label: &str,
    corpus_name: &str,
    ranker: &str,
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
            "_ranker={ranker} · t={} · k={} → {} in context · {} ctx · {:.1}s_",
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
/// cosine threshold, and provenance filter the generation uses.
/// `RagResponse` only reports a chunk *count*, so retrieval-quality runs need
/// these per-chunk results.  Scores come from the store's active ranker.
async fn print_retrieval(
    out: &mut dyn Write,
    embedder: &dyn Embedder,
    store: &dyn VectorStore,
    embed_cfg: &ragrig::types::EmbedConfig,
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
        .map_err(|_| anyhow::anyhow!("'date' command not found"))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
