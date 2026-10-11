use anyhow::Result;
use clap::{Parser, Subcommand};
use local_code_intelligence::{
    app::App,
    config::Config,
    evaluate::{
        self, CapStage, CompareOptions, DEFAULT_RESAMPLES, DEFAULT_SEED, ReplayOptions, RerankMode,
        RunOptions,
    },
    server,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Parser)]
#[command(
    version,
    about = "Standalone local Rust, TypeScript, JavaScript, Python, C#, Go, Java, C, and C++ code retrieval over MCP or plain REST/OpenAPI, with optional language-server navigation for every language (rust-analyzer, csharp-ls, typescript-language-server, pyright, gopls, jdtls, clangd)"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Serve {
        /// TCP port to bind the MCP/health server to.
        #[arg(long, default_value_t = 8768)]
        port: u16,
    },
    Index {
        workspace: PathBuf,
    },
    Status {
        workspace: PathBuf,
    },
    ListFiles {
        workspace: PathBuf,
    },
    Search {
        workspace: PathBuf,
        query: String,
        #[arg(long)]
        top_k: Option<usize>,
        #[arg(long = "language")]
        languages: Vec<String>,
        #[arg(long = "include-path")]
        include_paths: Vec<String>,
        #[arg(long = "exclude-path")]
        exclude_paths: Vec<String>,
        #[arg(long = "source-role")]
        source_roles: Vec<String>,
        /// Run the search with this `[search_profiles.<name>]` of the
        /// configuration applied over `[channels]` and
        /// `rerank_candidate_count`. The response then carries
        /// `search_profile`. An unknown name is an error that lists the
        /// configured ones.
        #[arg(long = "search-profile")]
        search_profile: Option<String>,
    },
    Evaluate {
        definition: PathBuf,
        #[arg(long = "workspace")]
        workspaces: Vec<String>,
        #[arg(long)]
        output: Option<PathBuf>,
        /// Compare this run against an earlier evaluation report: embeds a
        /// `comparison` field in the output and prints the summary to stderr.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Bootstrap seed for the `--baseline` comparison.
        #[arg(long, requires = "baseline")]
        seed: Option<u64>,
        /// Bootstrap resamples for the `--baseline` comparison.
        #[arg(long, requires = "baseline", value_parser = parse_resamples)]
        resamples: Option<usize>,
        /// Record every query's whole fused candidate pool (channel ranks,
        /// scores, per-file cap and shortlist position, reranker scores) and
        /// the retrieval configuration in the report, so other arms can be
        /// scored from it without services. Results and metrics are
        /// unchanged; the pretty-printed report grows by roughly 100 KB per query.
        #[arg(long)]
        dump_candidates: bool,
    },
    /// Compare two saved evaluation reports query by query. Works offline: it
    /// needs no services, configuration, or index. A regression is not an
    /// error; the exit code is nonzero only when a report cannot be read.
    Compare {
        /// Report of the run to compare against.
        baseline: PathBuf,
        /// Report of the run being judged.
        candidate: PathBuf,
        /// Also write the full JSON comparison report to this file.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Seed of the paired bootstrap.
        #[arg(long, default_value_t = DEFAULT_SEED)]
        seed: u64,
        /// Number of bootstrap resamples.
        #[arg(long, default_value_t = DEFAULT_RESAMPLES, value_parser = parse_resamples)]
        resamples: usize,
    },
    /// Score other retrieval settings from the candidate dump of an earlier
    /// `evaluate --dump-candidates` report, without services. Works offline
    /// like `compare`: it needs no configuration or index, and its report
    /// (`--output`) is an ordinary evaluation report that `compare` pairs with
    /// real ones. Settings you leave out keep the dumped arm's values.
    /// Queries it cannot score exactly are refused, counted, and reported as
    /// skipped; `reranking_ms` and `total_ms` are cost-model estimates.
    Replay {
        /// Report written by `evaluate --dump-candidates`.
        report: PathBuf,
        /// Write the replayed evaluation report (JSON) to this file. The
        /// summary is printed either way.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Rerank window: how many leading candidates the reranker orders.
        /// The rest of the fused order follows them. At most the dumped
        /// shortlist can be scored (default: the dumped
        /// `rerank_candidate_count`).
        #[arg(long, value_parser = parse_window)]
        window: Option<usize>,
        /// Chunks of one file that keep their place before the rest of that
        /// file's chunks are demoted (0 is unlimited; default: the dumped
        /// `max_chunks_per_file`).
        #[arg(long, value_parser = parse_cap)]
        cap: Option<u32>,
        /// Where the cap runs: `shortlist` (before the window cut, as in
        /// production, the default) or `results` (after reranking).
        #[arg(long)]
        cap_stage: Option<CapStage>,
        /// Weight of the semantic channel in fusion.
        #[arg(long, value_parser = parse_positive)]
        semantic_weight: Option<f32>,
        /// Weight of the lexical channel in fusion.
        #[arg(long, value_parser = parse_positive)]
        lexical_weight: Option<f32>,
        /// Weight of the language-server channel in fusion.
        #[arg(long, value_parser = parse_positive)]
        lsp_weight: Option<f32>,
        /// Reciprocal Rank Fusion constant.
        #[arg(long, value_parser = parse_positive)]
        rrf_k: Option<f32>,
        /// Source-role prior of `source` chunks (production: 0.004).
        #[arg(long, value_parser = parse_non_negative)]
        prior: Option<f32>,
        /// How the reranker's scores order the window: `replace` (production),
        /// `vote` (fusion score plus --vote-weight / (rrf_k + reranker rank)),
        /// or `off` (fused order). Default: the dumped mode.
        #[arg(long)]
        rerank_mode: Option<RerankMode>,
        /// Weight of the reranker's vote; required by, and only valid with,
        /// `--rerank-mode vote`.
        #[arg(long, value_parser = parse_non_negative)]
        vote_weight: Option<f32>,
    },
    Symbols {
        workspace: PathBuf,
        query: String,
    },
    Definition {
        workspace: PathBuf,
        relative_file_path: String,
        line: u32,
        character: u32,
    },
    References {
        workspace: PathBuf,
        relative_file_path: String,
        line: u32,
        character: u32,
        #[arg(long)]
        include_declaration: bool,
    },
}

fn parse_resamples(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_owned()),
        Ok(count) => Ok(count),
        Err(error) => Err(error.to_string()),
    }
}

fn parse_window(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_owned()),
        Ok(window) => Ok(window),
        Err(error) => Err(error.to_string()),
    }
}

fn parse_cap(value: &str) -> Result<u32, String> {
    let cap = value.parse::<u32>().map_err(|error| error.to_string())?;
    if cap > local_code_intelligence::config::FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT {
        return Err(format!(
            "must be 0..={}",
            local_code_intelligence::config::FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT
        ));
    }
    Ok(cap)
}

fn parse_positive(value: &str) -> Result<f32, String> {
    let number = value.parse::<f32>().map_err(|error| error.to_string())?;
    if number.is_finite() && number > 0.0 {
        Ok(number)
    } else {
        Err("must be a finite number greater than zero".to_owned())
    }
}

fn parse_non_negative(value: &str) -> Result<f32, String> {
    let number = value.parse::<f32>().map_err(|error| error.to_string())?;
    if number.is_finite() && number >= 0.0 {
        Ok(number)
    } else {
        Err("must be a finite number, zero or more".to_owned())
    }
}

/// `replay`: read a candidate dump, score the replayed arm, print the summary,
/// optionally write the replayed report. Runs without opening the application.
fn run_replay(source: &Path, output: Option<&Path>, options: &ReplayOptions) -> Result<()> {
    let replayed = evaluate::replay_file(source, options)?;
    print!("{}", replayed.render_summary());
    if let Some(path) = output {
        let json = serde_json::to_string_pretty(&replayed.to_json()?)?;
        std::fs::write(
            path,
            format!(
                "{json}
"
            ),
        )?;
    }
    Ok(())
}

/// `compare`: read two saved reports, print the summary, optionally write the
/// JSON report. Runs without opening the application.
fn run_compare(
    baseline: &Path,
    candidate: &Path,
    output: Option<&Path>,
    options: &CompareOptions,
) -> Result<()> {
    let comparison = evaluate::compare_files(baseline, candidate, options)?;
    print!("{}", comparison.render_summary());
    if let Some(path) = output {
        let json = serde_json::to_string_pretty(&comparison)?;
        std::fs::write(path, format!("{json}\n"))?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "local_code_intelligence=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve { port: 8768 });
    if let Command::Compare {
        baseline,
        candidate,
        output,
        seed,
        resamples,
    } = command
    {
        return run_compare(
            &baseline,
            &candidate,
            output.as_deref(),
            &CompareOptions { seed, resamples },
        );
    }
    if let Command::Replay {
        report,
        output,
        window,
        cap,
        cap_stage,
        semantic_weight,
        lexical_weight,
        lsp_weight,
        rrf_k,
        prior,
        rerank_mode,
        vote_weight,
    } = command
    {
        return run_replay(
            &report,
            output.as_deref(),
            &ReplayOptions {
                window,
                cap,
                cap_stage,
                semantic_weight,
                lexical_weight,
                lsp_weight,
                rrf_k,
                prior,
                rerank_mode,
                vote_weight,
            },
        );
    }
    // Read the baseline report before opening the application, so an
    // unreadable path fails now rather than after a long evaluation.
    let baseline_report = match &command {
        Command::Evaluate {
            baseline: Some(path),
            ..
        } => Some((path.clone(), evaluate::load_report(path)?)),
        _ => None,
    };
    let app = Arc::new(App::open(Config::load(cli.config.as_deref())?).await?);
    let mut evaluation_failed = false;
    let mut output_path = None;
    let output = match command {
        Command::Index { workspace } => serde_json::to_value(app.index(&workspace).await?)?,
        Command::Status { workspace } => serde_json::to_value(app.status(&workspace).await?)?,
        Command::ListFiles { workspace } => {
            serde_json::to_value(app.indexed_files(&workspace).await?)?
        }
        Command::Search {
            workspace,
            query,
            top_k,
            languages,
            include_paths,
            exclude_paths,
            source_roles,
            search_profile,
        } => serde_json::to_value(
            app.search_in_profile(
                &workspace,
                &query,
                top_k,
                local_code_intelligence::filter::FilterRequest {
                    languages: (!languages.is_empty()).then_some(languages),
                    include_paths: (!include_paths.is_empty()).then_some(include_paths),
                    exclude_paths: (!exclude_paths.is_empty()).then_some(exclude_paths),
                    source_roles: (!source_roles.is_empty()).then_some(source_roles),
                },
                search_profile.as_deref(),
            )
            .await?,
        )?,
        Command::Evaluate {
            definition,
            workspaces,
            output,
            baseline: _,
            seed,
            resamples,
            dump_candidates,
        } => {
            let definition = evaluate::load_definition(&definition)?;
            let workspaces = evaluate::parse_workspace_mappings(&workspaces)?;
            let report = evaluate::run_evaluation_with_options(
                &app,
                &definition,
                &workspaces,
                RunOptions { dump_candidates },
            )
            .await?;
            evaluation_failed = evaluate::has_failures(&report);
            output_path = output;
            let comparison = baseline_report.map(|(path, baseline)| {
                let options = CompareOptions {
                    seed: seed.unwrap_or(DEFAULT_SEED),
                    resamples: resamples.unwrap_or(DEFAULT_RESAMPLES),
                };
                let mut comparison = evaluate::compare(&baseline, &report, &options);
                comparison.baseline_source = Some(path.display().to_string());
                comparison
            });
            if let Some(comparison) = &comparison {
                // Stderr, so a report on stdout stays valid JSON.
                eprint!("{}", comparison.render_summary());
            }
            evaluate::report_with_comparison(&report, comparison.as_ref())?
        }
        Command::Compare { .. } => unreachable!("compare returns before the application opens"),
        Command::Replay { .. } => unreachable!("replay returns before the application opens"),
        Command::Symbols { workspace, query } => {
            serde_json::to_value(app.symbols(&workspace, &query).await?)?
        }
        Command::Definition {
            workspace,
            relative_file_path,
            line,
            character,
        } => serde_json::to_value(
            app.definition(&workspace, &relative_file_path, line, character)
                .await?,
        )?,
        Command::References {
            workspace,
            relative_file_path,
            line,
            character,
            include_declaration,
        } => serde_json::to_value(
            app.references(
                &workspace,
                &relative_file_path,
                line,
                character,
                include_declaration,
            )
            .await?,
        )?,
        Command::Serve { port } => {
            let cancellation = tokio_util::sync::CancellationToken::new();
            let router = server::router(app, cancellation.clone(), port);
            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
            tracing::info!(
                "MCP: http://127.0.0.1:{port}/mcp; health: http://127.0.0.1:{port}/health"
            );
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    cancellation.cancel();
                })
                .await?;
            return Ok(());
        }
    };
    let json = serde_json::to_string_pretty(&output)?;
    if let Some(path) = output_path {
        std::fs::write(&path, format!("{json}\n"))?;
    } else {
        println!("{json}");
    }
    if evaluation_failed {
        anyhow::bail!("one or more required evaluation expectations failed");
    }
    Ok(())
}
