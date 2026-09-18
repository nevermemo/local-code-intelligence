use anyhow::Result;
use clap::{Parser, Subcommand};
use local_code_intelligence::{app::App, config::Config, evaluate, server};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(
    version,
    about = "Standalone local Rust, TypeScript, JavaScript, Python, C#, Go, Java, and C code retrieval over MCP (C: syntax indexing and retrieval only; the other seven additionally support optional language-server navigation)"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Serve,
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
    let app = Arc::new(App::open(Config::load(cli.config.as_deref())?).await?);
    let mut evaluation_failed = false;
    let mut output_path = None;
    let output = match cli.command.unwrap_or(Command::Serve) {
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
        } => {
            let definition = evaluate::load_definition(&definition)?;
            let workspaces = evaluate::parse_workspace_mappings(&workspaces)?;
            let report = evaluate::run_evaluation(&app, &definition, &workspaces).await?;
            evaluation_failed = evaluate::has_failures(&report);
            output_path = output;
            serde_json::to_value(report)?
        }
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
        Command::Serve => {
            let cancellation = tokio_util::sync::CancellationToken::new();
            let router = server::router(app, cancellation.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:8768").await?;
            tracing::info!("MCP: http://127.0.0.1:8768/mcp; health: http://127.0.0.1:8768/health");
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
