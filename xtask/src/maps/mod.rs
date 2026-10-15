//! `cargo xtask maps`: tooling for the multimap spine.
//!
//! `bench` runs `lci traverse` over a gold set, scores the answers with the
//! gold scorers and appends a row to the trend file (see `bench.rs`).

pub mod bench;
pub mod compare;
pub mod fit_priors;
pub mod fit_tiebreak;
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
    /// Compare two bench runs of one set item by item: per metric the paired
    /// means, the mean difference B - A with a 95% paired-bootstrap interval,
    /// and the counts of items B is above, below or level on. Items either run
    /// stopped on the deadline or had failures on are left out unless
    /// --keep-failed.
    Compare(compare::CompareArgs),
}

pub fn run(command: MapsCommand) -> Result<()> {
    match command {
        MapsCommand::Bench(args) => bench::run(*args),
        MapsCommand::FitPriors(args) => fit_priors::run(args),
        MapsCommand::FitTiebreak(args) => fit_tiebreak::run(args),
        MapsCommand::Compare(args) => compare::run(args),
    }
}
