//! Runs the full live-acceptance/evaluation suite locally: the same
//! commands and order as `.github/workflows/live-acceptance.yml`, but with
//! each step bounded by its own timeout instead of one blind ceiling for
//! the whole run, and no GitHub Actions/self-hosted-runner dependency at
//! all. See `docs/development/local-live-acceptance.md` for the rationale.
//!
//! Cross-platform like the rest of `xtask` (see `AGENTS.md`): process
//! spawning and tree-killing go through `std::process`/`sysinfo`, the same
//! toolkit `xtask::acceptance::ManagedServer` already uses, not a
//! platform-specific shell script.

use crate::acceptance::{lci_binary, test_results_dir, workspace_root};
use anyhow::Result;
use std::fs::File;
use std::io::Write as _;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Passed,
    Failed,
    TimedOut,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Passed => "PASSED",
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
        program: "cargo".to_string(),
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
fn descendants_of(system: &System, root: u32) -> Vec<u32> {
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
fn run_step(spec: &StepSpec, log_dir: &std::path::Path) -> Result<StepResult> {
    let slug = slugify(spec.name);
    let stdout_path = log_dir.join(format!("{slug}.stdout.log"));
    let stderr_path = log_dir.join(format!("{slug}.stderr.log"));
    let stdout_file = File::create(&stdout_path)?;
    let stderr_file = File::create(&stderr_path)?;

    println!();
    println!("=== {} ===", spec.name);
    println!("{} {}", spec.program, spec.args.join(" "));

    let started = Instant::now();
    let mut child = Command::new(&spec.program)
        .args(&spec.args)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()?;
    let pid = child.id();

    let mut last_log = started;
    let status = loop {
        if let Some(exit_status) = child.try_wait()? {
            break if exit_status.success() {
                Status::Passed
            } else {
                Status::Failed
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

pub fn run(only: Vec<String>, skip_build: bool) -> Result<()> {
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
    // stuck. rust-analyzer LSP gets 30 minutes since it cold-embeds a real
    // external Cargo workspace (documented elsewhere as "2 to 10+ minutes"
    // of embedding time alone).
    let acceptance_steps: &[(&str, &str, u64)] = &[
        (
            "Acceptance - core (index/search/reindex smoke test)",
            "core",
            10,
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
        ("Acceptance - C# LSP (dotnet + csharp-ls)", "csharp-lsp", 10),
        (
            "Acceptance - C# LSP provider isolation",
            "csharp-missing",
            10,
        ),
        ("Acceptance - C# LSP recovery", "csharp-recovery", 10),
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

    let selected: Vec<&StepSpec> = steps
        .iter()
        .filter(|spec| should_run(&only, spec.name, spec.cmd))
        .collect();
    if selected.is_empty() {
        println!(
            "No steps matched -only '{}' -- nothing ran.",
            only.join(",")
        );
        std::process::exit(1);
    }

    let mut results = Vec::new();
    for spec in &selected {
        results.push(run_step(spec, &log_dir)?);
    }

    // Mirrors live-acceptance.yml's cleanup step: only removes fixture temp
    // directories (named lci-<fixture>-<pid> by Fixture::create, so this
    // pattern can't match anything else). Deliberately does not kill
    // processes by name -- this is your own machine, not a disposable
    // sandbox, and a crashed run's orphan needs a human glance at a process
    // list, not a script guessing by process name.
    let temp_dir = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(&temp_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("lci-") && entry.path().is_dir() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

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
    for result in &results {
        writeln!(
            summary_file,
            "{}\t{}\t{}s",
            result.name,
            result.status.label(),
            result.duration.as_secs()
        )?;
    }
    println!("Full logs: {}", log_dir.display());

    let failed: Vec<&StepResult> = results
        .iter()
        .filter(|r| r.status != Status::Passed)
        .collect();
    if !failed.is_empty() {
        println!("{} step(s) did not pass.", failed.len());
        std::process::exit(1);
    }
    println!("All steps passed.");
    Ok(())
}
