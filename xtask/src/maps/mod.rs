//! `cargo xtask maps`: tooling for the multimap spine.
//!
//! `bench` runs `lci traverse` over a gold set, scores the answers with the
//! gold scorers and appends a row to the trend file (see `bench.rs`).

pub mod baseline;
pub mod bench;
pub mod compare;
pub mod fit_priors;
pub mod fit_tiebreak;
pub mod lsp_cost;
pub mod oracle;
pub mod supplement;
pub mod trend;

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum MapsCommand {
    /// Run `lci traverse` once per gold question, score the answers with the
    /// gold scorers and append one trend row per (wave, set) to
    /// docs/spike/multimap/v2/trend.md. The gold files and the frozen tree are
    /// private and passed with --gold-dir and --tree.
    Bench(Box<bench::BenchArgs>),
    /// Fit the per-question-type hop priors from the traces of bench runs and
    /// write them as JSON, with the item ids held out of the fit.
    FitPriors(fit_priors::FitPriorsArgs),
    /// Fit the tie-break (a weighted sum of result features that orders results
    /// tied at the same rank) from the raw outputs of a bench run of one set,
    /// leaving the hold-out items out of the fit.
    FitTiebreak(fit_tiebreak::FitTiebreakArgs),
    /// What `lsp.live` costs on a live tree (K11d-1): one `serve` process per
    /// arm on 127.0.0.1:8769, the committed questions asked over MCP
    /// `search_code mode=traverse`, cold and warm ms per expansion, external
    /// calls, pops per 5 s, RSS, budget hits and the version probe's ms, quiet
    /// and under a CPU-only synthetic load. The report goes to
    /// test-results/lsp-cost/<stamp>/.
    LspCost(lsp_cost::LspCostArgs),
    /// Compare two bench runs of one set item by item: per metric the paired
    /// means, the mean difference B - A with a 95% paired-bootstrap interval,
    /// and the counts of items B is above, below or level on. Items either run
    /// stopped on the deadline or had failures on are left out unless
    /// --keep-failed.
    Compare(compare::CompareArgs),
    /// Simple non-walk arms, written as run dirs that `maps compare` reads:
    /// `impact` (M1.4 arm N, the exact closure of the anchor the question's
    /// names give) and `history` (`git log -G/-S/--grep` on the frozen eval
    /// tree). Neither is given the gold.
    Baseline {
        #[command(subcommand)]
        which: baseline::BaselineCommand,
    },
    /// BENCH-ONLY, GOLD-AWARE (violates the rule that the traversal never sees
    /// the gold; a steering ceiling, not a result). Writes one steering policy
    /// file per gold item that prunes frontier nodes outside the item's gold
    /// regions (files; see `maps bench --policy-dir`).
    OraclePolicy(oracle::OraclePolicyArgs),
    /// BENCH-ONLY, GOLD-AWARE control for `oracle-policy`. Keeps only the
    /// results of a plain walk's run dir that lie in the gold regions, so the
    /// ceiling can be read as steering (policy) versus result filtering
    /// (this). Run the plain walk with `--top-k 40` for a pool.
    OracleFilter(oracle::OracleFilterArgs),
}

pub fn run(command: MapsCommand) -> Result<()> {
    match command {
        MapsCommand::Bench(args) => bench::run(*args),
        MapsCommand::FitPriors(args) => fit_priors::run(args),
        MapsCommand::FitTiebreak(args) => fit_tiebreak::run(args),
        MapsCommand::LspCost(args) => lsp_cost::run(args),
        MapsCommand::Compare(args) => compare::run(args),
        MapsCommand::Baseline { which } => baseline::run(which),
        MapsCommand::OraclePolicy(args) => oracle::run_policy(args),
        MapsCommand::OracleFilter(args) => oracle::run_filter(args),
    }
}
