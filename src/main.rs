use anyhow::Result;
use clap::{Parser, Subcommand};
use local_code_intelligence::{
    app::App,
    config::Config,
    evaluate::{self, CompareOptions, DEFAULT_RESAMPLES, DEFAULT_SEED},
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
        } => serde_json::to_value(
            app.search_with_filters(
                &workspace,
                &query,
                top_k,
                local_code_intelligence::filter::FilterRequest {
                    languages: (!languages.is_empty()).then_some(languages),
                    include_paths: (!include_paths.is_empty()).then_some(include_paths),
                    exclude_paths: (!exclude_paths.is_empty()).then_some(exclude_paths),
                    source_roles: (!source_roles.is_empty()).then_some(source_roles),
                },
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
        } => {
            let definition = evaluate::load_definition(&definition)?;
            let workspaces = evaluate::parse_workspace_mappings(&workspaces)?;
            let report = evaluate::run_evaluation(&app, &definition, &workspaces).await?;
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
