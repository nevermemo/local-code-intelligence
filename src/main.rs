use anyhow::Result;
use clap::{Parser, Subcommand};
use local_code_intelligence::{app::App, config::Config, server};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(
    version,
    about = "Standalone local Rust, TypeScript, JavaScript, and Python code retrieval over MCP (Python: syntax indexing and retrieval only)"
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
    Search {
        workspace: PathBuf,
        query: String,
        #[arg(long)]
        top_k: Option<usize>,
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
    let output = match cli.command.unwrap_or(Command::Serve) {
        Command::Index { workspace } => serde_json::to_value(app.index(&workspace).await?)?,
        Command::Status { workspace } => serde_json::to_value(app.status(&workspace).await?)?,
        Command::Search {
            workspace,
            query,
            top_k,
        } => serde_json::to_value(app.search(&workspace, &query, top_k).await?)?,
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
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
