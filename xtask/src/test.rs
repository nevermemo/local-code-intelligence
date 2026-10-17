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
    Config,
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
    let mut command = Command::new("cargo");
    command.args(args);
    crate::build::use_local_protoc(&mut command);
    let status = command.status()?;
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
    let mut command = Command::new("cargo");
    command.args(args).stdout(Stdio::piped());
    crate::build::use_local_protoc(&mut command);
    let mut child = command.spawn()?;
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

/// The root crate's feature that compiles the test-only map entry points
/// (the T0 bit-for-bit map check and the `RunRecord` paths); the suites that run their tests enable it, so they do not
/// vanish from the gate (merge condition M3).
const MAP_EQUIVALENCE: &str = "local-code-intelligence/map-equivalence";

/// The cargo calls of [`Suite::Full`] after the path lint and the docs check, in order. The ones
/// that resolve dependencies run `--locked`, so a pull request that edits a
/// `Cargo.toml` and forgets `Cargo.lock` fails here (as `cargo xtask build`
/// does) instead of passing and failing later in a locked build. `cargo fmt`
/// reads no lockfile. The focused suites stay unlocked, for a developer who is
/// in the middle of an edit.
const FULL_STEPS: [&[&str]; 3] = [
    &["fmt", "--all", "--", "--check"],
    &[
        "test",
        "--workspace",
        "--locked",
        "--features",
        MAP_EQUIVALENCE,
    ],
    &[
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--features",
        MAP_EQUIVALENCE,
        "--",
        "-D",
        "warnings",
    ],
];

pub fn run(suite: Suite) -> Result<()> {
    match suite {
        Suite::Unit => invoke_focused(&["test", "--workspace", "--lib"]),
        Suite::Chunk => invoke_focused(&["test", "--workspace", "--lib", "chunk::"]),
        Suite::Filter => invoke_focused(&["test", "--workspace", "--lib", "filter::"]),
        Suite::Config => {
            invoke_focused(&["test", "--workspace", "--lib", "config::"])?;
            invoke_focused(&["test", "--workspace", "--lib", "init::"])?;
            invoke_focused(&["test", "--test", "integration", "config::"])
        }
        Suite::Evaluation => {
            invoke_focused(&["test", "--workspace", "--lib", "evaluate::"])?;
            invoke_focused(&["test", "--test", "integration", "evaluation::"])
        }
        Suite::Indexing => invoke_focused(&[
            "test",
            "--test",
            "integration",
            "--features",
            MAP_EQUIVALENCE,
            "indexing::",
        ]),
        Suite::Readiness => invoke_focused(&["test", "--test", "integration", "readiness::"]),
        Suite::Mcp => invoke_focused(&["test", "--test", "integration", "mcp::"]),
        Suite::Watching => invoke_focused(&["test", "--test", "integration", "watching::"]),
        Suite::Lsp => {
            invoke_focused(&["test", "--workspace", "--lib", "lsp::"])?;
            invoke_focused(&["test", "--test", "integration", "navigation::"])
        }
        Suite::Full => {
            println!("> cargo xtask lint-machine-paths");
            crate::lint::run()?;
            println!("> cargo xtask docs check");
            crate::docs::run()?;
            for step in FULL_STEPS {
                invoke(step)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FULL_STEPS, MAP_EQUIVALENCE, executed_in_summary};

    #[test]
    fn the_full_suite_locks_every_step_that_resolves_dependencies() {
        let steps: Vec<_> = FULL_STEPS.iter().map(|step| step[0]).collect();
        assert_eq!(steps, ["fmt", "test", "clippy"]);
        for step in FULL_STEPS.iter().filter(|step| step[0] != "fmt") {
            // `--locked` must come before the `--` that starts the tool's own arguments.
            let cargo_arguments = step.iter().take_while(|argument| **argument != "--");
            assert!(
                cargo_arguments
                    .clone()
                    .any(|argument| *argument == "--locked"),
                "{step:?} is not --locked"
            );
        }
    }

    #[test]
    fn the_full_suite_tests_and_lints_with_the_map_equivalence_feature() {
        // Without it the tests of the test-only map entry points would
        // compile out of the gate.
        for step in FULL_STEPS.iter().filter(|step| step[0] != "fmt") {
            let cargo_arguments: Vec<_> = step
                .iter()
                .take_while(|argument| **argument != "--")
                .collect();
            assert!(
                cargo_arguments
                    .windows(2)
                    .any(|pair| *pair[0] == "--features" && *pair[1] == MAP_EQUIVALENCE),
                "{step:?} does not enable {MAP_EQUIVALENCE}"
            );
        }
    }

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
