//! Ported from `scripts/Test.ps1`. Focused suites run with `--workspace` and
//! the `map-equivalence` feature so their module filters reach unit tests in
//! both the root crate and `lci-core` (the `chunk`/`filter`/`lsp` modules
//! moved there in the crate split), and so they share `--suite Full`'s feature
//! resolution and build cache. Every focused invocation must execute at least
//! one test: a filter that matches nothing fails the suite instead of passing
//! vacuously.

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::{
    ffi::OsStr,
    io::{BufRead, BufReader},
    process::Stdio,
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
    Freshness,
    Lsp,
    Full,
}

fn invoke(args: &[&str]) -> Result<()> {
    println!("> cargo {}", args.join(" "));
    let mut command = crate::build::cargo_command();
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
    let mut command = crate::build::cargo_command();
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

const FMT_STEP: &[&str] = &["fmt", "--all", "--", "--check"];

/// Full's test step as a plain `cargo test`: one serial process per test
/// binary. CI runs this path.
const CARGO_TEST_STEP: &[&str] = &[
    "test",
    "--workspace",
    "--locked",
    "--features",
    MAP_EQUIVALENCE,
];

/// Full's test step under cargo-nextest when it is installed: one process per
/// test, run in parallel. The default profile has no retries, so a flaky test
/// fails the gate. nextest does not run doctests, so a `cargo test --doc` step
/// follows.
const NEXTEST_STEPS: [&[&str]; 2] = [
    &[
        "nextest",
        "run",
        "--workspace",
        "--locked",
        "--features",
        MAP_EQUIVALENCE,
        "--no-fail-fast",
    ],
    &[
        "test",
        "--workspace",
        "--locked",
        "--features",
        MAP_EQUIVALENCE,
        "--doc",
    ],
];

/// Full's lint step: every target of the workspace, warnings denied.
const CLIPPY_STEP: &[&str] = &[
    "clippy",
    "--workspace",
    "--all-targets",
    "--locked",
    "--features",
    MAP_EQUIVALENCE,
    "--",
    "-D",
    "warnings",
];

/// The cargo calls of [`Suite::Full`] after the path lint and the docs check,
/// in order: fmt, the test step (plain or nextest), clippy. The ones that
/// resolve dependencies run `--locked`, so a pull request that edits a
/// `Cargo.toml` and forgets `Cargo.lock` fails here (as `cargo xtask build`
/// does) instead of passing and failing later in a locked build. `cargo fmt`
/// reads no lockfile. The focused suites stay unlocked, for a developer who is
/// in the middle of an edit.
fn full_steps(nextest: bool) -> Vec<&'static [&'static str]> {
    let mut steps = vec![FMT_STEP];
    if nextest {
        steps.extend(NEXTEST_STEPS);
    } else {
        steps.push(CARGO_TEST_STEP);
    }
    steps.push(CLIPPY_STEP);
    steps
}

/// Whether `LCI_NO_NEXTEST` asks for the plain `cargo test` path that CI uses:
/// set to anything except the empty string or `0`.
fn no_nextest_requested(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// Whether Full's test step runs under nextest, with the line that says why
/// not when it does not: it runs there when `cargo nextest --version`
/// succeeds and `LCI_NO_NEXTEST` does not ask otherwise.
fn nextest_choice() -> (bool, &'static str) {
    if no_nextest_requested(std::env::var_os("LCI_NO_NEXTEST").as_deref()) {
        return (false, "cargo test (LCI_NO_NEXTEST is set)");
    }
    let found = crate::build::cargo_command()
        .args(["nextest", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if found {
        (
            true,
            "cargo nextest (default profile) plus cargo test --doc",
        )
    } else {
        (
            false,
            "cargo test (`cargo nextest --version` failed: not installed or broken)",
        )
    }
}

/// The `cargo test` invocations of a focused suite. Every one carries
/// `--workspace` and the `map-equivalence` feature, so it resolves dependency
/// features exactly as [`Suite::Full`]'s test step does and reuses its build
/// artefacts: resolver 2 unifies features over the selected packages, and a
/// bare `--test integration` selects only the root package, which rebuilds
/// reqwest and the lance stack. `None` for [`Suite::Full`].
fn focused_invocations(suite: Suite) -> Option<Vec<Vec<&'static str>>> {
    let lib = |filter: &'static str| {
        let mut args = vec!["--lib"];
        args.extend((!filter.is_empty()).then_some(filter));
        args
    };
    let integration = |filter: &'static str| vec!["--test", "integration", filter];
    let targets = match suite {
        Suite::Unit => vec![lib("")],
        Suite::Chunk => vec![lib("chunk::")],
        Suite::Filter => vec![lib("filter::")],
        Suite::Config => vec![lib("config::"), lib("init::"), integration("config::")],
        Suite::Evaluation => vec![lib("evaluate::"), integration("evaluation::")],
        Suite::Indexing => vec![integration("indexing::")],
        Suite::Readiness => vec![integration("readiness::")],
        Suite::Mcp => vec![integration("mcp::")],
        Suite::Watching => vec![integration("watching::")],
        Suite::Freshness => vec![integration("freshness::")],
        Suite::Lsp => vec![
            lib("lsp::"),
            integration("navigation::"),
            integration("lsp_concurrent_open::"),
            integration("lsp_diagnostics_wait::"),
        ],
        Suite::Full => return None,
    };
    Some(
        targets
            .into_iter()
            .map(|target| {
                let mut args = vec!["test", "--workspace", "--features", MAP_EQUIVALENCE];
                args.extend(target);
                args
            })
            .collect(),
    )
}

pub fn run(suite: Suite) -> Result<()> {
    if let Some(invocations) = focused_invocations(suite) {
        for args in invocations {
            invoke_focused(&args)?;
        }
        return Ok(());
    }
    println!("> cargo xtask lint-machine-paths");
    crate::lint::run()?;
    println!("> cargo xtask docs check");
    crate::docs::run()?;
    let (nextest, description) = nextest_choice();
    println!("> test step: {description}");
    for step in full_steps(nextest) {
        invoke(step)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MAP_EQUIVALENCE, Suite, executed_in_summary, focused_invocations, full_steps,
        no_nextest_requested,
    };
    use clap::ValueEnum;
    use std::ffi::OsStr;

    /// The cargo arguments of a step: those before the `--` that starts the
    /// tool's own arguments.
    fn cargo_arguments<'a>(step: &[&'a str]) -> Vec<&'a str> {
        step.iter()
            .copied()
            .take_while(|argument| *argument != "--")
            .collect()
    }

    fn has_feature(arguments: &[&str]) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair[0] == "--features" && pair[1] == MAP_EQUIVALENCE)
    }

    #[test]
    fn the_full_suite_locks_every_step_that_resolves_dependencies() {
        let plain: Vec<_> = full_steps(false).iter().map(|step| step[0]).collect();
        assert_eq!(plain, ["fmt", "test", "clippy"]);
        let nextest: Vec<_> = full_steps(true).iter().map(|step| step[0]).collect();
        assert_eq!(nextest, ["fmt", "nextest", "test", "clippy"]);
        for steps in [full_steps(false), full_steps(true)] {
            for step in steps.iter().filter(|step| step[0] != "fmt") {
                // `--locked` must come before the `--` that starts the tool's own arguments.
                assert!(
                    cargo_arguments(step).contains(&"--locked"),
                    "{step:?} is not --locked"
                );
            }
        }
    }

    #[test]
    fn the_full_suite_tests_and_lints_with_the_map_equivalence_feature() {
        // Without it the tests of the test-only map entry points would
        // compile out of the gate.
        for steps in [full_steps(false), full_steps(true)] {
            for step in steps.iter().filter(|step| step[0] != "fmt") {
                assert!(
                    has_feature(&cargo_arguments(step)),
                    "{step:?} does not enable {MAP_EQUIVALENCE}"
                );
            }
        }
    }

    #[test]
    fn every_full_step_but_fmt_spans_the_workspace() {
        for steps in [full_steps(false), full_steps(true)] {
            for step in steps.iter().filter(|step| step[0] != "fmt") {
                assert!(
                    cargo_arguments(step).contains(&"--workspace"),
                    "{step:?} lacks --workspace"
                );
            }
        }
    }

    #[test]
    fn an_unset_empty_or_zero_no_nextest_variable_keeps_nextest() {
        assert!(!no_nextest_requested(None));
        assert!(!no_nextest_requested(Some(OsStr::new(""))));
        assert!(!no_nextest_requested(Some(OsStr::new("0"))));
        assert!(no_nextest_requested(Some(OsStr::new("1"))));
        assert!(no_nextest_requested(Some(OsStr::new("yes"))));
    }

    #[test]
    fn the_nextest_path_also_runs_the_doctests_and_every_test() {
        let steps = full_steps(true);
        assert!(steps[1].contains(&"--no-fail-fast"), "{:?}", steps[1]);
        assert!(!steps[1].contains(&"--profile"), "{:?}", steps[1]);
        assert!(steps[2].contains(&"--doc"), "{:?}", steps[2]);
        assert!(steps[2].contains(&"--workspace"), "{:?}", steps[2]);
    }

    #[test]
    fn every_focused_invocation_resolves_features_like_the_full_suite() {
        // A bare `--test integration` selects only the root package, which
        // unifies dependency features differently and rebuilds the lance stack.
        for suite in Suite::value_variants() {
            let Some(invocations) = focused_invocations(*suite) else {
                assert!(matches!(suite, Suite::Full));
                continue;
            };
            assert!(!invocations.is_empty(), "{suite:?} runs nothing");
            for args in invocations {
                let arguments = cargo_arguments(&args);
                assert_eq!(arguments[0], "test", "{suite:?}: {args:?}");
                assert!(
                    arguments.contains(&"--workspace"),
                    "{suite:?}: {args:?} lacks --workspace"
                );
                assert!(
                    has_feature(&arguments),
                    "{suite:?}: {args:?} lacks the feature"
                );
            }
        }
    }

    #[test]
    fn focused_suites_keep_their_filters() {
        let args = |suite| focused_invocations(suite).unwrap();
        assert_eq!(
            args(Suite::Mcp),
            [[
                "test",
                "--workspace",
                "--features",
                MAP_EQUIVALENCE,
                "--test",
                "integration",
                "mcp::"
            ]]
        );
        assert_eq!(
            args(Suite::Unit),
            [[
                "test",
                "--workspace",
                "--features",
                MAP_EQUIVALENCE,
                "--lib"
            ]]
        );
        assert_eq!(args(Suite::Config).len(), 3);
        assert_eq!(args(Suite::Lsp).len(), 3);
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
