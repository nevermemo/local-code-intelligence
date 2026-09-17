//! Ported from `scripts/Test.ps1`. The suite-to-command mapping is preserved
//! exactly from the original script.

use anyhow::{Result, bail};
use clap::ValueEnum;
use std::process::Command;

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "PascalCase")]
pub enum Suite {
    Unit,
    Chunk,
    Filter,
    Evaluation,
    Indexing,
    Readiness,
    Mcp,
    Watching,
    Lsp,
    Full,
}

fn invoke(args: &[&str]) -> Result<()> {
    println!("> cargo {}", args.join(" "));
    let status = Command::new("cargo").args(args).status()?;
    if !status.success() {
        bail!("cargo {} failed: {status}", args.join(" "));
    }
    Ok(())
}

pub fn run(suite: Suite) -> Result<()> {
    match suite {
        Suite::Unit => invoke(&["test", "--lib"]),
        Suite::Chunk => invoke(&["test", "--lib", "chunk::tests::"]),
        Suite::Filter => invoke(&["test", "--lib", "filter::tests::"]),
        Suite::Evaluation => {
            invoke(&["test", "--lib", "evaluate::tests::"])?;
            invoke(&["test", "--test", "integration", "evaluation::"])
        }
        Suite::Indexing => invoke(&["test", "--test", "integration", "indexing::"]),
        Suite::Readiness => invoke(&["test", "--test", "integration", "readiness::"]),
        Suite::Mcp => invoke(&["test", "--test", "integration", "mcp::"]),
        Suite::Watching => invoke(&["test", "--test", "integration", "watching::"]),
        Suite::Lsp => {
            invoke(&["test", "--lib", "lsp::tests::"])?;
            invoke(&["test", "--test", "integration", "navigation::"])
        }
        Suite::Full => {
            invoke(&["fmt", "--all", "--", "--check"])?;
            invoke(&["test", "--workspace"])?;
            invoke(&[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ])
        }
    }
}
