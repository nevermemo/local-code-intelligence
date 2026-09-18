mod acceptance;
mod build;
mod setup;
mod test;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "xtask",
    about = "Cross-platform dev tasks for local-code-intelligence"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Provision local build tools (protoc, and cmake guidance on macOS/Linux).
    Setup,
    /// Build the workspace, optionally running the test suite afterward.
    Build {
        #[arg(long)]
        test: bool,
    },
    /// Run one focused test suite.
    Test {
        #[arg(long, value_enum)]
        suite: test::Suite,
    },
    /// Run a live acceptance harness against a locally built binary.
    Acceptance {
        #[command(subcommand)]
        which: AcceptanceCommand,
    },
}

#[derive(Subcommand)]
enum AcceptanceCommand {
    /// Index/search/reindex smoke test against an external workspace.
    Core {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Symbols/definition/references smoke test against an external workspace.
    Lsp {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Multi-language fixture indexing/search/failure-path acceptance.
    Multilingual,
    /// Real dotnet + csharp-ls acceptance (workspace symbols, definition, references).
    CsharpLsp {
        #[arg(long)]
        csharp_ls: Option<PathBuf>,
        #[arg(long)]
        probe_only: bool,
    },
    /// C# provider-isolation/degradation acceptance (missing csharp-ls binary).
    CsharpMissing,
    /// C# LSP persistent-process reuse and forced-kill recovery acceptance.
    CsharpRecovery {
        #[arg(long)]
        csharp_ls: Option<PathBuf>,
    },
    /// Real typescript-language-server acceptance (workspace symbols, definition, references).
    TypescriptLsp {
        #[arg(long)]
        typescript_language_server: Option<PathBuf>,
    },
    /// TypeScript provider-isolation/degradation acceptance (missing typescript-language-server binary).
    TypescriptMissing,
    /// TypeScript LSP persistent-process reuse and forced-kill recovery acceptance.
    TypescriptRecovery {
        #[arg(long)]
        typescript_language_server: Option<PathBuf>,
    },
    /// Real pyright acceptance (workspace symbols, definition, references).
    PythonLsp {
        #[arg(long)]
        pyright: Option<PathBuf>,
    },
    /// Python provider-isolation/degradation acceptance (missing pyright binary).
    PythonMissing,
    /// Python LSP persistent-process reuse and forced-kill recovery acceptance.
    PythonRecovery {
        #[arg(long)]
        pyright: Option<PathBuf>,
    },
    /// Real gopls acceptance (workspace symbols, definition, references).
    GoLsp {
        #[arg(long)]
        gopls: Option<PathBuf>,
    },
    /// Go provider-isolation/degradation acceptance (missing gopls binary).
    GoMissing,
    /// Go LSP persistent-process reuse and forced-kill recovery acceptance.
    GoRecovery {
        #[arg(long)]
        gopls: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Setup => setup::run().await,
        Command::Build { test } => build::run_build(test),
        Command::Test { suite } => test::run(suite),
        Command::Acceptance { which } => match which {
            AcceptanceCommand::Core { workspace } => acceptance::core::run(workspace).await,
            AcceptanceCommand::Lsp { workspace } => acceptance::lsp::run(workspace).await,
            AcceptanceCommand::Multilingual => acceptance::multilingual::run().await,
            AcceptanceCommand::CsharpLsp {
                csharp_ls,
                probe_only,
            } => acceptance::csharp_lsp::run(csharp_ls, probe_only).await,
            AcceptanceCommand::CsharpMissing => acceptance::csharp_missing::run().await,
            AcceptanceCommand::CsharpRecovery { csharp_ls } => {
                acceptance::csharp_recovery::run(csharp_ls).await
            }
            AcceptanceCommand::TypescriptLsp {
                typescript_language_server,
            } => acceptance::typescript_lsp::run(typescript_language_server).await,
            AcceptanceCommand::TypescriptMissing => acceptance::typescript_missing::run().await,
            AcceptanceCommand::TypescriptRecovery {
                typescript_language_server,
            } => acceptance::typescript_recovery::run(typescript_language_server).await,
            AcceptanceCommand::PythonLsp { pyright } => acceptance::python_lsp::run(pyright).await,
            AcceptanceCommand::PythonMissing => acceptance::python_missing::run().await,
            AcceptanceCommand::PythonRecovery { pyright } => {
                acceptance::python_recovery::run(pyright).await
            }
            AcceptanceCommand::GoLsp { gopls } => acceptance::go_lsp::run(gopls).await,
            AcceptanceCommand::GoMissing => acceptance::go_missing::run().await,
            AcceptanceCommand::GoRecovery { gopls } => acceptance::go_recovery::run(gopls).await,
        },
    }
}
