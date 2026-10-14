//! `cargo xtask maps`: tooling for the multimap spine.
//!
//! `bench` runs `lci traverse` over a gold set, scores the answers with the
//! gold scorers and appends a row to the trend file (see `bench.rs`).

pub mod bench;
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
}

pub fn run(command: MapsCommand) -> Result<()> {
    match command {
        MapsCommand::Bench(args) => bench::run(*args),
    }
}
