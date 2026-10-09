//! Ported from `scripts/Test.ps1`. Focused suites run with `--workspace` so
//! their module filters reach unit tests in both the root crate and
//! `lci-core` (the `chunk`/`filter`/`lsp` modules moved there in the crate
//! split), and so they share `--suite Full`'s feature resolution and build
//! cache. Every focused invocation must execute at least one test: a filter
//! that matches nothing fails the suite instead of passing vacuously.

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
};

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

/// Sums the executed (passed + failed) counts from a libtest summary line
/// such as `test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured;
/// 109 filtered out; finished in 1.87s`. Returns `None` for other lines.
fn executed_in_summary(line: &str) -> Option<usize> {
    let summary = line.trim().strip_prefix("test result: ")?;
    let mut executed = 0;
    for part in summary.split(';') {
        let mut words = part.split_whitespace().rev();
        let label = words.next();
        let count = words.next().and_then(|count| count.parse::<usize>().ok());
        if let (Some("passed" | "failed"), Some(count)) = (label, count) {
            executed += count;
        }
    }
    Some(executed)
}

/// Like [`invoke`] for a filtered `cargo test`, but streams the test output
/// while counting executed tests, and fails when the filter matched none.
fn invoke_focused(args: &[&str]) -> Result<()> {
    println!("> cargo {}", args.join(" "));
    let mut child = Command::new("cargo")
        .args(args)
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().context("capture cargo test output")?;
    let mut executed = 0;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        println!("{line}");
        executed += executed_in_summary(&line).unwrap_or(0);
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("cargo {} failed: {status}", args.join(" "));
    }
    if executed == 0 {
        bail!(
            "cargo {} ran no tests; the suite's filter matches nothing",
            args.join(" ")
        );
    }
    Ok(())
}

pub fn run(suite: Suite) -> Result<()> {
    match suite {
        Suite::Unit => invoke_focused(&["test", "--workspace", "--lib"]),
        Suite::Chunk => invoke_focused(&["test", "--workspace", "--lib", "chunk::"]),
        Suite::Filter => invoke_focused(&["test", "--workspace", "--lib", "filter::"]),
        Suite::Evaluation => {
            invoke_focused(&["test", "--workspace", "--lib", "evaluate::"])?;
            invoke_focused(&["test", "--test", "integration", "evaluation::"])
        }
        Suite::Indexing => invoke_focused(&["test", "--test", "integration", "indexing::"]),
        Suite::Readiness => invoke_focused(&["test", "--test", "integration", "readiness::"]),
        Suite::Mcp => invoke_focused(&["test", "--test", "integration", "mcp::"]),
        Suite::Watching => invoke_focused(&["test", "--test", "integration", "watching::"]),
        Suite::Lsp => {
            invoke_focused(&["test", "--workspace", "--lib", "lsp::"])?;
            invoke_focused(&["test", "--test", "integration", "navigation::"])
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

#[cfg(test)]
mod tests {
    use super::executed_in_summary;

    #[test]
    fn counts_passed_and_failed_but_not_ignored_or_filtered() {
        assert_eq!(
            executed_in_summary(
                "test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 109 filtered out; finished in 1.87s"
            ),
            Some(29)
        );
        assert_eq!(
            executed_in_summary(
                "test result: FAILED. 3 passed; 2 failed; 4 ignored; 0 measured; 0 filtered out; finished in 0.01s"
            ),
            Some(5)
        );
    }

    #[test]
    fn a_filter_that_matches_nothing_counts_zero() {
        assert_eq!(
            executed_in_summary(
                "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 17 filtered out; finished in 0.00s"
            ),
            Some(0)
        );
    }

    #[test]
    fn ignores_non_summary_lines() {
        assert_eq!(executed_in_summary("running 29 tests"), None);
        assert_eq!(executed_in_summary("test chunk::tests::x ... ok"), None);
    }
}
