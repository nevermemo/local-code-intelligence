mod acceptance;
mod build;
mod live_acceptance;
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
    /// Run the full live-acceptance/evaluation suite locally (no GitHub
    /// Actions), with each step bounded by its own timeout instead of one
    /// shared ceiling. See docs/development/local-live-acceptance.md.
    LiveAcceptance {
        /// Only run steps whose name or xtask acceptance subcommand
        /// contains one of these substrings (case-insensitive). Repeatable
        /// or comma-separated: --only python --only core, or --only python,core
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Skip the initial `cargo build --workspace` step.
        #[arg(long)]
        skip_build: bool,
    },
}

#[derive(Subcommand)]
enum AcceptanceCommand {
    /// Index/search/reindex smoke test. Defaults to this repository; pass
    /// --profile gust for the optional external GUST example.
    Core {
        #[arg(long, value_enum, default_value = "self")]
        profile: acceptance::AcceptanceProfile,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Symbols/definition/references smoke test. Defaults to this
    /// repository; pass --profile gust for the optional external GUST
    /// example.
    Lsp {
        #[arg(long, value_enum, default_value = "self")]
        profile: acceptance::AcceptanceProfile,
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
    /// Real jdtls acceptance (workspace symbols, definition, references).
    JavaLsp {
        /// Path to the jdtls installation directory (not an executable).
        #[arg(long)]
        java: Option<PathBuf>,
    },
    /// Java provider-isolation/degradation acceptance (missing jdtls install).
    JavaMissing,
    /// Java LSP persistent-process reuse and forced-kill recovery acceptance.
    JavaRecovery {
        /// Path to the jdtls installation directory (not an executable).
        #[arg(long)]
        java: Option<PathBuf>,
    },
    /// Real clangd acceptance for C (workspace symbols, definition, references).
    CLsp {
        #[arg(long)]
        clangd: Option<PathBuf>,
    },
    /// C provider-isolation/degradation acceptance (missing clangd binary).
    CMissing,
    /// C LSP persistent-process reuse and forced-kill recovery acceptance.
    CRecovery {
        #[arg(long)]
        clangd: Option<PathBuf>,
    },
    /// Real clangd acceptance for C++ (workspace symbols, definition, references).
    CppLsp {
        #[arg(long)]
        clangd: Option<PathBuf>,
    },
    /// C++ provider-isolation/degradation acceptance (missing clangd binary).
    CppMissing,
    /// C++ LSP persistent-process reuse and forced-kill recovery acceptance.
    CppRecovery {
        #[arg(long)]
        clangd: Option<PathBuf>,
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
            AcceptanceCommand::Core { profile, workspace } => {
                acceptance::core::run(profile, workspace).await
            }
            AcceptanceCommand::Lsp { profile, workspace } => {
                acceptance::lsp::run(profile, workspace).await
            }
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
            AcceptanceCommand::JavaLsp { java } => acceptance::java_lsp::run(java).await,
            AcceptanceCommand::JavaMissing => acceptance::java_missing::run().await,
            AcceptanceCommand::JavaRecovery { java } => acceptance::java_recovery::run(java).await,
            AcceptanceCommand::CLsp { clangd } => acceptance::c_lsp::run(clangd).await,
            AcceptanceCommand::CMissing => acceptance::c_missing::run().await,
            AcceptanceCommand::CRecovery { clangd } => acceptance::c_recovery::run(clangd).await,
            AcceptanceCommand::CppLsp { clangd } => acceptance::cpp_lsp::run(clangd).await,
            AcceptanceCommand::CppMissing => acceptance::cpp_missing::run().await,
            AcceptanceCommand::CppRecovery { clangd } => {
                acceptance::cpp_recovery::run(clangd).await
            }
        },
        Command::LiveAcceptance { only, skip_build } => live_acceptance::run(only, skip_build),
    }
}
