mod acceptance;
mod build;
mod docs;
mod env;
mod gold;
mod lint;
mod live_acceptance;
mod maps;
mod mine;
mod mine_transcripts;
mod setup;
mod sim;
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
        /// Fail if any selected step is skipped because a prerequisite is unavailable.
        #[arg(long)]
        require_all: bool,
    },
    /// Resolve the development-tooling paths (work area, plans, frozen
    /// binaries, checkpoint, ...) by role instead of hard-coding them:
    /// defaults < .local/machine.toml < LCI_DEV_<KEY>. See
    /// docs/development/machine-values.md.
    Env(env::EnvArgs),
    /// Fail when a tracked text file names a machine-specific location (a
    /// user profile path, an AppData folder, a client session folder, or a
    /// literal from the gitignored .local/lint-machine-paths.txt). Prints
    /// file:line for each hit. Also part of `test --suite Full`. See
    /// docs/development/machine-values.md, "Lint".
    LintMachinePaths,
    /// Documentation checks. See docs/development/documentation.md, rule 9.
    Docs {
        #[command(subcommand)]
        which: DocsCommand,
    },
    /// Score, baseline and convert the local gold sets (impact, flow,
    /// where-to, history, instructions). The gold files live outside the
    /// repository and are passed with --gold. See
    /// docs/spike/multimap/v2/gold-scorers.md.
    Gold {
        #[command(subcommand)]
        which: gold::GoldCommand,
    },
    /// Multimap spine tooling: `bench` runs `lci traverse` over a gold set,
    /// scores it and appends a trend row. See docs/spike/multimap/v2/trend.md.
    Maps {
        #[command(subcommand)]
        which: maps::MapsCommand,
    },
    /// Simulated agent: a deterministic, model-free loop of greps and reads
    /// over a frozen tree, scored against a gold set (calls and tokens to the
    /// gold). Arm A0 is grep-only; A1 and A2 put one `lci search` or
    /// `lci traverse --scip` call in front of the same chain. See
    /// docs/spike/multimap/v2/sim-agent.md.
    Sim(sim::SimArgs),
    /// Structured-config measurement on the simulated agent: `run` one arm
    /// over the questions of one stratum and score them, `compare` runs with a
    /// paired bootstrap, `check-probes` that every question probes its key,
    /// `check-corpus` that the in-memory scan equals the disk scan.
    /// See docs/spike/multimap/structured-grep-prereg.md.
    SimStructured {
        #[command(subcommand)]
        which: sim::structured::StructuredCommand,
    },
    /// Mine an evaluation definition from a repository's git history: each
    /// qualifying commit message becomes a query and the supported source
    /// files it touched that still exist at HEAD become its expected paths.
    /// Reads git history only. See README "Retrieval evaluation".
    MineQueries {
        /// Root of the git repository to mine.
        #[arg(long)]
        repo: PathBuf,
        /// Workspace key written into the definition; the same key is passed
        /// to `evaluate --workspace <key>=<path>`.
        #[arg(long)]
        key: String,
        /// Evaluation definition to write, e.g. evaluations/mined/<key>.toml.
        #[arg(long)]
        out: PathBuf,
        /// Keep at most this many queries (the most recent qualifying ones).
        #[arg(long, default_value_t = 300)]
        max: usize,
    },
    /// Count question shapes over synthetic agent transcripts (the sim trace
    /// shape: a `sim.json` or JSONL of `{question, trace}`): shape and trace
    /// class counts, call and token quantiles and anonymised question
    /// templates; never a raw question or a step argument. Output defaults to
    /// test-results/transcripts/shapes.json. See
    /// docs/spike/multimap/v2/gold-scorers.md "Strata and transcript shapes".
    MineTranscripts(mine_transcripts::MineTranscriptsArgs),
}

#[derive(Subcommand)]
enum DocsCommand {
    /// Fail when a relative Markdown link or anchor in a tracked *.md file does
    /// not resolve (external links are skipped). Excludes the vendored
    /// .agents/skills/rust-skills; broken links in the frozen records under
    /// docs/spike/multimap/v2 (*-prereg*.md, *-results.md) are reported but
    /// never fail. Prints path:line: target (reason).
    Check,
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
    /// Real marksman acceptance for Markdown (headings, link definition,
    /// references to a heading).
    MarkdownLsp {
        #[arg(long)]
        marksman: Option<PathBuf>,
    },
    /// Markdown provider-isolation/degradation acceptance (missing marksman binary).
    MarkdownMissing,
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
            AcceptanceCommand::MarkdownLsp { marksman } => {
                acceptance::markdown_lsp::run(marksman).await
            }
            AcceptanceCommand::MarkdownMissing => acceptance::markdown_missing::run().await,
            AcceptanceCommand::CLsp { clangd } => acceptance::c_lsp::run(clangd).await,
            AcceptanceCommand::CMissing => acceptance::c_missing::run().await,
            AcceptanceCommand::CRecovery { clangd } => acceptance::c_recovery::run(clangd).await,
            AcceptanceCommand::CppLsp { clangd } => acceptance::cpp_lsp::run(clangd).await,
            AcceptanceCommand::CppMissing => acceptance::cpp_missing::run().await,
            AcceptanceCommand::CppRecovery { clangd } => {
                acceptance::cpp_recovery::run(clangd).await
            }
        },
        Command::LiveAcceptance {
            only,
            skip_build,
            require_all,
        } => live_acceptance::run(only, skip_build, require_all),
        Command::Env(args) => env::run(args),
        Command::LintMachinePaths => lint::run(),
        Command::Docs { which } => match which {
            DocsCommand::Check => docs::run(),
        },
        Command::Gold { which } => gold::run(which),
        Command::Maps { which } => maps::run(which),
        Command::Sim(args) => sim::run(args),
        Command::SimStructured { which } => sim::structured::run(which),
        Command::MineQueries {
            repo,
            key,
            out,
            max,
        } => mine::run(repo, key, out, max),
        Command::MineTranscripts(args) => mine_transcripts::run(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_acceptance_require_all_is_opt_in() {
        for (args, expected) in [
            (vec!["xtask", "live-acceptance"], false),
            (vec!["xtask", "live-acceptance", "--require-all"], true),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::LiveAcceptance { require_all, .. } = cli.command else {
                panic!("expected live-acceptance command");
            };
            assert_eq!(require_all, expected);
        }
    }
}
