//! Runs the full live-acceptance/evaluation suite: each step bounded by its
//! own timeout instead of one blind ceiling for the whole run, and no GitHub
//! Actions/self-hosted-runner dependency when run locally.
//! `.github/workflows/live-acceptance.yml` runs this command with
//! `--require-all` (N87). See `docs/development/local-live-acceptance.md`
//! for the rationale.
//!
//! Cross-platform like the rest of `xtask` (see `AGENTS.md`): process
//! spawning and tree-killing go through `std::process`/`sysinfo`, the same
//! toolkit `xtask::acceptance::ManagedServer` already uses, not a
//! platform-specific shell script.

use crate::acceptance::{FIXTURE_ROOT_ENV, lci_binary, test_results_dir, workspace_root};
use anyhow::{Result, bail};
use std::fs::File;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, System};

struct StepSpec {
    name: &'static str,
    cmd: &'static str,
    program: String,
    args: Vec<String>,
    timeout: Duration,
}

impl StepSpec {
    /// A [`cargo_step`] goes through [`crate::build::cargo_command`], so it
    /// shares build artefacts with a direct `cargo` run.
    fn command(&self) -> Command {
        if self.program == CARGO {
            crate::build::cargo_command()
        } else {
            Command::new(&self.program)
        }
    }
}

const CARGO: &str = "cargo";

/// Name prefix of the run-unique directory that holds one run's fixtures.
const RUN_ROOT_PREFIX: &str = "lci-live-run-";

/// The fixture root of one run: `<base>/lci-live-run-<pid>-<stamp>`. The
/// process id and the timestamp make it unique to this run.
fn run_root_path(base: &std::path::Path, pid: u32, stamp: u64) -> std::path::PathBuf {
    base.join(format!("{RUN_ROOT_PREFIX}{pid}-{stamp}"))
}

/// Removes the run's own fixture root, and nothing else. Refuses a path that
/// is not a direct child of `base` named like a run root, so a wrong argument
/// can never delete another directory; any other `lci-*` directory in `base`,
/// including another run's root, is left alone.
fn remove_run_root(base: &std::path::Path, root: &std::path::Path) {
    let ours = root.parent() == Some(base)
        && root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(RUN_ROOT_PREFIX));
    if ours {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Passed,
    Skipped,
    Failed,
    TimedOut,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Passed => "PASSED",
            Status::Skipped => "SKIPPED",
            Status::Failed => "FAILED",
            Status::TimedOut => "TIMED OUT",
        }
    }
}

struct StepResult {
    name: String,
    status: Status,
    duration: Duration,
}

fn completed_status(success: bool, stdout: &str) -> Status {
    if !success {
        Status::Failed
    } else if stdout
        .lines()
        .any(|line| line.starts_with("PREREQUISITE_UNAVAILABLE:"))
    {
        Status::Skipped
    } else {
        Status::Passed
    }
}

fn write_summary(
    results: &[StepResult],
    require_all: bool,
    writer: &mut impl std::io::Write,
) -> Result<&'static str> {
    for result in results {
        writeln!(
            writer,
            "{}\t{}\t{}s",
            result.name,
            result.status.label(),
            result.duration.as_secs()
        )?;
    }
    let failed = results
        .iter()
        .filter(|r| {
            matches!(r.status, Status::Failed | Status::TimedOut)
                || (require_all && r.status == Status::Skipped)
        })
        .count();
    if failed != 0 {
        bail!("{failed} step(s) did not pass.");
    }
    if results.iter().any(|r| r.status == Status::Skipped) {
        Ok("Completed with skipped steps.")
    } else {
        Ok("All steps passed.")
    }
}

/// The already-built `xtask` binary, mirroring `acceptance::lci_binary()`.
/// This orchestrator (`cargo xtask live-acceptance`) *is* a running
/// instance of this exact file, so each step must invoke the pre-built
/// binary directly rather than going through the `cargo xtask` alias
/// (`cargo run --package xtask --`, per `.cargo/config.toml`) -- `cargo
/// run` checks whether a rebuild is needed first, and on Windows that
/// rebuild's link step cannot replace an `.exe` that a running process
/// (this one) currently has open, failing with "Access is denied" on
/// every single step for the rest of the run.
fn xtask_binary() -> std::path::PathBuf {
    workspace_root()
        .join("target/debug")
        .join(format!("xtask{}", std::env::consts::EXE_SUFFIX))
}

fn cargo_step(
    name: &'static str,
    cmd: &'static str,
    args: &[&str],
    timeout_minutes: u64,
) -> StepSpec {
    StepSpec {
        name,
        cmd,
        program: CARGO.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        timeout: Duration::from_secs(timeout_minutes * 60),
    }
}

/// Same shape as `cargo_step`, but for a step that runs a pre-built binary
/// directly instead of going through `cargo run`/the `cargo xtask` alias.
fn binary_step(
    name: &'static str,
    cmd: &'static str,
    program: std::path::PathBuf,
    args: Vec<String>,
    timeout_minutes: u64,
) -> StepSpec {
    StepSpec {
        name,
        cmd,
        program: program.display().to_string(),
        args,
        timeout: Duration::from_secs(timeout_minutes * 60),
    }
}

/// Every descendant PID of `root` at any depth, BFS over one already-
/// refreshed process table. Mirrors `acceptance::ManagedServer`'s private
/// helper of the same shape.
pub(crate) fn descendants_of(system: &System, root: u32) -> Vec<u32> {
    let mut frontier = vec![root];
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some(parent) = frontier.pop() {
        for process in system.processes().values() {
            if process.parent().map(|p| p.as_u32()) != Some(parent) {
                continue;
            }
            let pid = process.pid().as_u32();
            if seen.insert(pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found
}

fn kill_tree(pid: u32) {
    let mut system = System::new_all();
    system.refresh_all();
    for descendant in descendants_of(&system, pid) {
        if let Some(process) = system.process(Pid::from_u32(descendant)) {
            process.kill();
        }
    }
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        process.kill();
    }
}

fn slugify(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Runs `spec`, polling every 2 seconds for exit and logging elapsed
/// time/accumulated CPU time every 30 seconds. That CPU number is
/// informational only -- a step legitimately waiting on a slow
/// embedding/LSP HTTP response can sit at near-zero CPU growth while being
/// perfectly healthy (the wait is I/O-bound), so it is never used to kill a
/// step early. The per-step timeout is the only actual kill trigger, kept
/// deliberately generous per step rather than one shared budget for the
/// whole run -- see the module doc and `docs/development/local-live-acceptance.md`.
fn run_step(
    spec: &StepSpec,
    log_dir: &std::path::Path,
    fixture_root: &std::path::Path,
) -> Result<StepResult> {
    let slug = slugify(spec.name);
    let stdout_path = log_dir.join(format!("{slug}.stdout.log"));
    let stderr_path = log_dir.join(format!("{slug}.stderr.log"));
    let stdout_file = File::create(&stdout_path)?;
    let stderr_file = File::create(&stderr_path)?;

    println!();
    println!("=== {} ===", spec.name);
    println!("{} {}", spec.program, spec.args.join(" "));

    let started = Instant::now();
    let mut child = spec
        .command()
        .args(&spec.args)
        .env(FIXTURE_ROOT_ENV, fixture_root)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()?;
    let pid = child.id();

    let mut last_log = started;
    let status = loop {
        if let Some(exit_status) = child.try_wait()? {
            break if !exit_status.success() {
                Status::Failed
            } else {
                completed_status(
                    true,
                    &String::from_utf8_lossy(&std::fs::read(&stdout_path)?),
                )
            };
        }
        std::thread::sleep(Duration::from_secs(2));
        let elapsed = started.elapsed();
        if last_log.elapsed() >= Duration::from_secs(30) {
            last_log = Instant::now();
            let cpu_ms = {
                let mut system = System::new_all();
                system.refresh_all();
                system
                    .process(Pid::from_u32(pid))
                    .map(|p| p.accumulated_cpu_time())
            };
            match cpu_ms {
                Some(ms) => println!(
                    "  still running -- elapsed {}m{:02}s, cpu {:.1}s",
                    elapsed.as_secs() / 60,
                    elapsed.as_secs() % 60,
                    ms as f64 / 1000.0
                ),
                None => println!(
                    "  still running -- elapsed {}m{:02}s, cpu n/a",
                    elapsed.as_secs() / 60,
                    elapsed.as_secs() % 60
                ),
            }
        }
        if elapsed >= spec.timeout {
            println!(
                "  TIMEOUT after {} minutes -- killing process tree",
                spec.timeout.as_secs() / 60
            );
            kill_tree(pid);
            let _ = child.wait();
            break Status::TimedOut;
        }
    };

    let duration = started.elapsed();
    println!(
        "  {} in {}m{:02}s",
        status.label(),
        duration.as_secs() / 60,
        duration.as_secs() % 60
    );
    if status != Status::Passed {
        for (label, path) in [("stdout", &stdout_path), ("stderr", &stderr_path)] {
            println!("  --- {label} (tail) ---");
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content
                    .lines()
                    .rev()
                    .take(20)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    println!("  {line}");
                }
            }
        }
    }

    Ok(StepResult {
        name: spec.name.to_string(),
        status,
        duration,
    })
}

fn should_run(only: &[String], name: &str, cmd: &str) -> bool {
    if only.is_empty() {
        return true;
    }
    only.iter().any(|needle| {
        let needle = needle.to_lowercase();
        name.to_lowercase().contains(&needle) || cmd.to_lowercase().contains(&needle)
    })
}

fn select_steps<'a>(steps: &'a [StepSpec], only: &[String]) -> Vec<&'a StepSpec> {
    steps
        .iter()
        .filter(|spec| spec.name == "Build" || should_run(only, spec.name, spec.cmd))
        .collect()
}

pub fn run(only: Vec<String>, skip_build: bool, require_all: bool) -> Result<()> {
    let root = workspace_root();
    std::env::set_current_dir(&root)?;

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let log_dir = test_results_dir()?
        .join("live-acceptance-logs")
        .join(stamp.to_string());
    std::fs::create_dir_all(&log_dir)?;

    let mut steps: Vec<StepSpec> = Vec::new();
    if !skip_build {
        // --exclude xtask: this orchestrator *is* a running instance of
        // xtask.exe, so a full `--workspace` build (which decides xtask
        // needs relinking even though `cargo run --package xtask` just
        // built it seconds ago -- a different build-unit scope apparently
        // produces a different fingerprint) fails on Windows with "Access
        // is denied" trying to replace the currently-open .exe. Excluding
        // it is also just correct: nothing here needs to rebuild its own
        // orchestrator mid-run, only the product code the acceptance steps
        // exercise.
        steps.push(cargo_step(
            "Build",
            "build",
            &["build", "--workspace", "--exclude", "xtask"],
            20,
        ));
    }

    // (name, xtask acceptance subcommand, timeout minutes). Small-fixture
    // LSP flows (full/isolation/recovery per language) are generously
    // bounded at 10 minutes even though they normally finish in well under
    // a minute -- the ceiling only matters when something is actually
    // stuck. `core` and `lsp` default to cold-embedding this repository
    // itself (documented elsewhere as "2 to 10+ minutes" of embedding time
    // alone for a workspace this size), so `core` gets 20 minutes and
    // rust-analyzer LSP gets 30.
    let acceptance_steps: &[(&str, &str, u64)] = &[
        (
            "Acceptance - core (index/search/reindex smoke test)",
            "core",
            20,
        ),
        (
            "Acceptance - multilingual (fixture indexing/search/decoy-ranking)",
            "multilingual",
            15,
        ),
        ("Acceptance - rust-analyzer LSP", "lsp", 30),
        ("Acceptance - Python LSP (pyright)", "python-lsp", 10),
        (
            "Acceptance - Python LSP provider isolation",
            "python-missing",
            10,
        ),
        ("Acceptance - Python LSP recovery", "python-recovery", 10),
        (
            "Acceptance - TypeScript/JavaScript LSP",
            "typescript-lsp",
            10,
        ),
        (
            "Acceptance - TypeScript/JavaScript LSP provider isolation",
            "typescript-missing",
            10,
        ),
        (
            "Acceptance - TypeScript/JavaScript LSP recovery",
            "typescript-recovery",
            10,
        ),
        ("Acceptance - Go LSP (gopls)", "go-lsp", 10),
        ("Acceptance - Go LSP provider isolation", "go-missing", 10),
        ("Acceptance - Go LSP recovery", "go-recovery", 10),
        ("Acceptance - Java LSP (jdtls)", "java-lsp", 10),
        (
            "Acceptance - Java LSP provider isolation",
            "java-missing",
            10,
        ),
        ("Acceptance - Java LSP recovery", "java-recovery", 10),
        ("Acceptance - Markdown LSP (marksman)", "markdown-lsp", 10),
        (
            "Acceptance - Markdown LSP provider isolation",
            "markdown-missing",
            10,
        ),
        ("Acceptance - C# LSP (dotnet + csharp-ls)", "csharp-lsp", 10),
        (
            "Acceptance - C# LSP provider isolation",
            "csharp-missing",
            10,
        ),
        ("Acceptance - C# LSP recovery", "csharp-recovery", 10),
        ("Acceptance - C LSP (clangd)", "c-lsp", 10),
        ("Acceptance - C LSP provider isolation", "c-missing", 10),
        ("Acceptance - C LSP recovery", "c-recovery", 10),
        ("Acceptance - C++ LSP (clangd)", "cpp-lsp", 10),
        ("Acceptance - C++ LSP provider isolation", "cpp-missing", 10),
        ("Acceptance - C++ LSP recovery", "cpp-recovery", 10),
    ];
    for (name, cmd, timeout) in acceptance_steps {
        if should_run(&only, name, cmd) {
            steps.push(binary_step(
                name,
                cmd,
                xtask_binary(),
                vec!["acceptance".to_string(), cmd.to_string()],
                *timeout,
            ));
        }
    }

    if should_run(&only, "Evaluate", "evaluate") {
        // The evaluation below inherits this process's environment and passes
        // no --config, so it honours the user-level local overlay and any
        // LCI_* variable (the self-hosted runner may rely on its machine
        // overlay). Neither is stripped. Instead, resolve the same
        // configuration with the same binary and environment first, so this
        // run's logs show which layers and values the evaluation used (the
        // step's stdout log is `lci config --json`).
        steps.push(binary_step(
            "Evaluate provenance - lci config --json",
            "evaluate-config",
            lci_binary(),
            vec!["config".to_string(), "--json".to_string()],
            1,
        ));
        let workspace_arg = format!("self={}", root.display());
        let output_arg = test_results_dir()?.join("evaluation.json");
        steps.push(binary_step(
            "Evaluate - retrieval quality (self workspace)",
            "evaluate",
            lci_binary(),
            vec![
                "evaluate".to_string(),
                "./evaluations/core.toml".to_string(),
                "--workspace".to_string(),
                workspace_arg,
                "--output".to_string(),
                output_arg.display().to_string(),
            ],
            15,
        ));
    }

    let selected = select_steps(&steps, &only);
    if selected.is_empty() {
        println!(
            "No steps matched -only '{}' -- nothing ran.",
            only.join(",")
        );
        std::process::exit(1);
    }

    // Every step puts its fixtures under this run's own directory
    // (FIXTURE_ROOT_ENV), so the cleanup below removes exactly what this run
    // created and never another run's fixtures or an unrelated `lci-*`
    // directory.
    let temp_dir = std::env::temp_dir();
    let run_root = run_root_path(&temp_dir, std::process::id(), stamp);
    let mut results = Vec::new();
    for spec in &selected {
        results.push(run_step(spec, &log_dir, &run_root)?);
    }

    // Removes only this run's fixture root (the workflow relies on this). Deliberately does not kill processes by name -- this is
    // your own machine, not a disposable sandbox, and a crashed run's orphan
    // needs a human glance at a process list, not a script guessing by
    // process name.
    remove_run_root(&temp_dir, &run_root);

    println!();
    println!("=== Summary ===");
    for result in &results {
        println!(
            "{:<70} {:<10} {}m{:02}s",
            result.name,
            result.status.label(),
            result.duration.as_secs() / 60,
            result.duration.as_secs() % 60
        );
    }

    let summary_path = log_dir.join("summary.txt");
    let mut summary_file = File::create(&summary_path)?;
    println!("Full logs: {}", log_dir.display());
    println!(
        "{}",
        write_summary(&results, require_all, &mut summary_file)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection_test_steps(include_build: bool) -> Vec<StepSpec> {
        let mut steps = Vec::new();
        if include_build {
            steps.push(cargo_step("Build", "build", &["build"], 20));
        }
        steps.push(binary_step(
            "Acceptance - core indexing",
            "core",
            "xtask".into(),
            vec![],
            20,
        ));
        steps.push(binary_step(
            "Acceptance - core search",
            "core",
            "xtask".into(),
            vec![],
            20,
        ));
        steps.push(binary_step(
            "Acceptance - multilingual",
            "multilingual",
            "xtask".into(),
            vec![],
            15,
        ));
        steps
    }

    #[test]
    fn only_core_keeps_build_and_core_steps() {
        let steps = selection_test_steps(true);
        let selected = select_steps(&steps, &["core".to_string()]);

        assert_eq!(
            selected.iter().map(|step| step.name).collect::<Vec<_>>(),
            [
                "Build",
                "Acceptance - core indexing",
                "Acceptance - core search"
            ]
        );
    }

    #[test]
    fn only_core_with_skip_build_keeps_core_steps() {
        let steps = selection_test_steps(false);
        let selected = select_steps(&steps, &["core".to_string()]);

        assert_eq!(
            selected.iter().map(|step| step.name).collect::<Vec<_>>(),
            ["Acceptance - core indexing", "Acceptance - core search"]
        );
    }

    #[test]
    fn no_only_filter_keeps_every_step() {
        let steps = selection_test_steps(true);
        let selected = select_steps(&steps, &[]);

        assert_eq!(selected.len(), steps.len());
    }

    #[test]
    fn cleanup_removes_only_this_runs_root() {
        let base_dir = std::env::temp_dir().join(format!(
            "xtask-cleanup-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&base_dir).unwrap();
        let base = base_dir.as_path();
        let mine = run_root_path(base, 100, 1);
        let other_run = run_root_path(base, 200, 2);
        let notes = base.join("lci-notes");
        let stray_fixture = base.join("lci-csharp-300");
        for dir in [&mine, &other_run, &notes, &stray_fixture] {
            std::fs::create_dir_all(dir.join("lci-core-100")).unwrap();
        }

        remove_run_root(base, &mine);

        assert!(!mine.exists());
        assert!(other_run.exists());
        assert!(notes.exists());
        assert!(stray_fixture.exists());

        // Refuses a path that is not a run root, or not a direct child.
        remove_run_root(base, &notes);
        remove_run_root(base, &other_run.join("lci-core-100"));
        remove_run_root(&base.join("elsewhere"), &other_run);
        assert!(notes.exists());
        assert!(other_run.join("lci-core-100").exists());
        std::fs::remove_dir_all(&base_dir).unwrap();
    }

    #[test]
    fn prerequisite_unavailable_child() {
        println!("PREREQUISITE_UNAVAILABLE: fake language server not installed");
    }

    #[test]
    fn prerequisite_unavailable_is_not_passed() {
        let log_dir = std::env::temp_dir().join(format!(
            "xtask-skip-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&log_dir).unwrap();
        let spec = binary_step(
            "Fake LSP acceptance",
            "fake-lsp",
            std::env::current_exe().unwrap(),
            vec![
                "--exact".to_string(),
                "live_acceptance::tests::prerequisite_unavailable_child".to_string(),
                "--nocapture".to_string(),
            ],
            1,
        );
        let result = run_step(&spec, &log_dir, &log_dir).unwrap();
        std::fs::remove_dir_all(&log_dir).unwrap();

        assert_eq!(result.status.label(), "SKIPPED");
    }

    #[test]
    fn skipped_step_is_listed_and_requires_all_to_fail() {
        let results = [
            StepResult {
                name: "Available LSP".to_string(),
                status: Status::Passed,
                duration: Duration::from_secs(1),
            },
            StepResult {
                name: "Missing LSP".to_string(),
                status: completed_status(true, "PREREQUISITE_UNAVAILABLE: fake server"),
                duration: Duration::ZERO,
            },
        ];
        for require_all in [false, true] {
            let mut output = Vec::new();
            let outcome = write_summary(&results, require_all, &mut output);
            assert_eq!(
                String::from_utf8(output).unwrap(),
                "Available LSP\tPASSED\t1s\nMissing LSP\tSKIPPED\t0s\n"
            );
            if require_all {
                assert_eq!(outcome.unwrap_err().to_string(), "1 step(s) did not pass.");
            } else {
                assert_eq!(outcome.unwrap(), "Completed with skipped steps.");
            }
        }
    }

    #[test]
    fn all_passed_summary_succeeds_in_both_modes() {
        let results = [StepResult {
            name: "Available LSP".to_string(),
            status: Status::Passed,
            duration: Duration::ZERO,
        }];
        for require_all in [false, true] {
            assert_eq!(
                write_summary(&results, require_all, &mut Vec::new()).unwrap(),
                "All steps passed."
            );
        }
    }

    #[test]
    fn failures_and_timeouts_fail_in_both_modes() {
        for status in [Status::Failed, Status::TimedOut] {
            let results = [StepResult {
                name: "Unsuccessful LSP".to_string(),
                status,
                duration: Duration::ZERO,
            }];
            for require_all in [false, true] {
                assert!(write_summary(&results, require_all, &mut Vec::new()).is_err());
            }
        }
    }

    /// d538: with no self-hosted runner registered, a push trigger only queues
    /// runs that never start, so the workflow runs by hand alone; never on
    /// `pull_request` either (a self-hosted runner must not run fork code).
    #[test]
    fn live_acceptance_workflow_runs_by_hand_only() {
        let path = workspace_root().join(".github/workflows/live-acceptance.yml");
        let workflow = std::fs::read_to_string(&path).unwrap();
        let triggers: Vec<&str> = workflow
            .lines()
            .skip_while(|line| line.trim_end() != "on:")
            .skip(1)
            .take_while(|line| line.is_empty() || line.starts_with(' '))
            .filter_map(|line| line.strip_prefix("  "))
            .filter(|line| !line.starts_with(' ') && !line.starts_with('#'))
            .filter_map(|line| line.split(':').next())
            .collect();
        assert_eq!(triggers, ["workflow_dispatch"], "{triggers:?}");
    }

    /// N87: the workflow must run the orchestrator with `--require-all`;
    /// run step by step, a missing tool's skip exits zero and reads green.
    #[test]
    fn live_acceptance_workflow_requires_every_tool() {
        let path = workspace_root().join(".github/workflows/live-acceptance.yml");
        let workflow = std::fs::read_to_string(&path).unwrap();
        let commands: Vec<&str> = workflow
            .lines()
            .filter_map(|line| line.trim().strip_prefix("run:"))
            .map(str::trim)
            .collect();
        assert!(
            commands.contains(&"cargo xtask live-acceptance --require-all"),
            "live-acceptance.yml must run `cargo xtask live-acceptance --require-all`: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|command| command.starts_with("cargo xtask acceptance ")),
            "a step-by-step `cargo xtask acceptance <name>` run reads a skipped tool as green"
        );
    }

    #[test]
    fn prerequisite_marker_does_not_hide_failure_or_match_incidental_text() {
        assert_eq!(
            completed_status(false, "PREREQUISITE_UNAVAILABLE: fake server"),
            Status::Failed
        );
        assert_eq!(completed_status(true, "PASS: fake server"), Status::Passed);
        assert_eq!(
            completed_status(true, "diagnostic mentions PREREQUISITE_UNAVAILABLE"),
            Status::Passed
        );
    }
}
