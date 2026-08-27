//! The chat process: runs the benchmark matrix (agents × pipelines ×
//! corpora × queries) against the vector database built by
//! `ragrig-bench-ingest`.  Provenance is the seam: every cell is addressed
//! by its pipeline-scoped corpus name, and a helpful message is emitted when
//! a requested provenance is not in the database.

use anyhow::{Context, Result, bail};
use clap::Parser;
use ragrig::{RagAgent, RagResponse};
use ragrig_bench::{
    agent_label, apply_mock_mode, build_agent, build_ranker, cell_filter, corpus_names,
    load_config, pipeline_labels, ranker_configs, ranker_label, validate_matrix,
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

    /// Run with offline mock components — deterministic embedder and
    /// canned answers for every agent without an explicit `answer` — to
    /// test the matrix structure without a separate mock config.
    #[arg(short = 'm', long)]
    mock: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = load_config(&cli.config)?;
    if cli.mock {
        apply_mock_mode(&mut config);
        eprintln!("Mock mode: deterministic embedder + canned answers.");
    }
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

                            let response = agent
                                .generate_with_context_detailed(query, &[] as &[(&str, &str)])
                                .await
                                .unwrap_or_else(|e| RagResponse {
                                    answer: format!("_Error: {e}_"),
                                    system_prompt: String::new(),
                                    user_prompt: query.to_string(),
                                    chunks_retrieved: None,
                                    documents: None,
                                    retrieved: None,
                                    rewritten_query: None,
                                    elapsed: None,
                                    timings: ragrig::StageTimings::default(),
                                });

                            let chunks = response.chunks_retrieved.unwrap_or(0);
                            let secs = response.elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                            writeln!(
                                out,
                                "_ranker={} · t={} · k={} → {} in context · {} ctx · emb {:.2}s · gen {:.2}s · total {:.1}s_",
                                ranker_label(ranker_cfg),
                                config.embed.similarity_threshold,
                                config.embed.top_k,
                                chunks,
                                agent.context_tokens(),
                                response.timings.embed.as_secs_f64(),
                                response.timings.generate.as_secs_f64(),
                                secs,
                            )?;
                            writeln!(out)?;
                            print_retrieved(out.as_mut(), &response)?;
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

async fn run_chat_mode(
    agent: &mut RagAgent,
    out: &mut dyn Write,
    embed_cfg: &ragrig::types::EmbedConfig,
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

        writeln!(out, "**Q:** {msg}")?;
        writeln!(out)?;

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
                retrieved: None,
                rewritten_query: None,
                elapsed: None,
                timings: ragrig::StageTimings::default(),
            });

        let chunks = response.chunks_retrieved.unwrap_or(0);
        let secs = response.elapsed.map(|d| d.as_secs_f64()).unwrap_or(0.0);
        writeln!(
            out,
            "_ranker={ranker} · t={} · k={} → {} in context · {} ctx · emb {:.2}s · gen {:.2}s · total {:.1}s_",
            embed_cfg.similarity_threshold,
            embed_cfg.top_k,
            chunks,
            agent.context_tokens(),
            response.timings.embed.as_secs_f64(),
            response.timings.generate.as_secs_f64(),
            secs,
        )?;
        writeln!(out)?;
        print_retrieved(out, &response)?;
        writeln!(out, "{}", response.answer.trim())?;
        writeln!(out)?;

        transcript.push((msg.clone(), response.answer));
    }

    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Print the retrieved chunks from the response's `retrieved` field — the
/// exact store-ranked chunks the agent injected as context, so the query is
/// embedded once (by the agent) instead of twice.  A successful cell with
/// nothing retrieved means every chunk fell below the similarity threshold.
fn print_retrieved(out: &mut dyn Write, response: &RagResponse) -> Result<()> {
    match &response.retrieved {
        Some(chunks) => {
            writeln!(out, "**Retrieved** (ranked by store ranker):")?;
            for (i, r) in chunks.iter().enumerate() {
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
            writeln!(out)?;
        }
        // An error cell reports its failure through the answer; a successful
        // cell with nothing retrieved hit the similarity threshold.
        None if !response.answer.starts_with("_Error:") => {
            writeln!(out, "_No chunks passed the similarity threshold._")?;
            writeln!(out)?;
        }
        None => {}
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
