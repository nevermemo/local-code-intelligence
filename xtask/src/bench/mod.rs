//! `cargo xtask bench ...`: the multimap spike bench
//! (`docs/spike/multimap/bench.md`).

pub mod compare;
pub mod embedder;
pub mod mine;
pub mod run;
pub mod score;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::acceptance::workspace_root;
use crate::build::use_local_protoc;
use crate::mine::git;

/// The bench set the runner and the workflow use by default.
const DEFAULT_SET: &str = "docs/spike/multimap/bench/bench-set-v1.json";

#[derive(Subcommand)]
pub enum BenchCommand {
    /// Mine replayed-commit tasks (a bench set) from local clones of public
    /// repositories. Reads git history only; writes data, never code.
    Mine {
        /// `<key>=<path to a clone>`; repeat per repository. The URL is read
        /// from the clone's `origin` remote and the snapshot is its HEAD.
        #[arg(long = "source", required = true)]
        sources: Vec<String>,
        /// `<key>=<SPDX licence id>` for every source, checked by hand.
        #[arg(long = "license", required = true)]
        licenses: Vec<String>,
        /// Keep at most this many tasks per repository (the most recent).
        #[arg(long, default_value_t = 20)]
        max: usize,
        /// Version label written into the set.
        #[arg(long, default_value = "v1")]
        version: String,
        /// Bench set file to write.
        #[arg(long)]
        out: PathBuf,
    },
    /// Build the `local-code-intelligence` binary (debug profile) and copy it
    /// to `--out`, with `<out>.json` recording the commit and whether the
    /// tree was dirty. Run the bench from the copy, never from target/.
    Freeze {
        #[arg(long)]
        out: PathBuf,
    },
    /// Run levels B and C for one frozen build against a bench set and
    /// write a results JSON. With --baseline also print (and with --table
    /// write) the comparison against an earlier results file.
    Run {
        /// Frozen binary (from `bench freeze`).
        #[arg(long)]
        bin: PathBuf,
        /// Bench set.
        #[arg(long, default_value = DEFAULT_SET)]
        set: PathBuf,
        /// Work directory: checkouts, data directories, scratch configs.
        /// Must not be inside an indexed repository.
        #[arg(long)]
        work: PathBuf,
        /// Cache of clones (default: <work>/clones).
        #[arg(long)]
        clones: Option<PathBuf>,
        /// Results JSON to write.
        #[arg(long)]
        output: PathBuf,
        /// Levels to run: B, C or both (comma-separated).
        #[arg(long, default_value = "B,C")]
        levels: String,
        /// Arms (comma-separated): fusion, grep.
        #[arg(long, default_value = "fusion,grep")]
        arms: String,
        /// Use this OpenAI-compatible embedding service instead of the fake
        /// one (level D). Needs --embedding-model.
        #[arg(long, requires = "embedding_model")]
        embedding_url: Option<String>,
        #[arg(long)]
        embedding_model: Option<String>,
        /// Only these repositories (repeatable).
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Only these task ids (repeatable). A task run alone is indexed
        /// from scratch at its base commit (the incremental-equals-full
        /// check).
        #[arg(long = "task")]
        tasks: Vec<String>,
        /// At most this many tasks per repository (the oldest first).
        #[arg(long)]
        max_per_repo: Option<usize>,
        /// Free-text label stored in the results.
        #[arg(long)]
        label: Option<String>,
        /// Earlier results file to compare against.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Write the comparison table (Markdown) here.
        #[arg(long, requires = "baseline")]
        table: Option<PathBuf>,
    },
    /// Compare two results files (base, then new) under the pre-registered
    /// rules; prints a Markdown table and the verdict.
    Compare {
        base: PathBuf,
        new: PathBuf,
        /// Write the table here as well.
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = compare::DEFAULT_SEED)]
        seed: u64,
        #[arg(long, default_value_t = compare::DEFAULT_RESAMPLES)]
        resamples: usize,
        /// Exit 1 when the verdict lists a regression.
        #[arg(long)]
        fail_on_regression: bool,
    },
}

fn freeze(out: &Path) -> Result<()> {
    let root = workspace_root();
    let mut build = Command::new("cargo");
    build
        .current_dir(&root)
        .args(["build", "--locked", "--bin", "local-code-intelligence"]);
    use_local_protoc(&mut build);
    let status = build.status().context("running cargo build")?;
    anyhow::ensure!(status.success(), "cargo build failed: {status}");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        root.join(target)
    };
    let built = target.join("debug").join(format!(
        "local-code-intelligence{}",
        std::env::consts::EXE_SUFFIX
    ));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&built, out)
        .with_context(|| format!("copying {} to {}", built.display(), out.display()))?;
    let commit = git(&root, &["rev-parse", "HEAD"])?.trim().to_owned();
    let dirty = !git(&root, &["status", "--porcelain", "--untracked-files=no"])?
        .trim()
        .is_empty();
    let sidecar = json!({"commit": commit, "dirty": dirty, "profile": "debug"});
    std::fs::write(out.with_extension("json"), format!("{sidecar:#}\n"))?;
    eprintln!(
        "froze {commit}{} to {}",
        if dirty { " (dirty)" } else { "" },
        out.display()
    );
    Ok(())
}

fn print_comparison(
    base: &Path,
    new: &Path,
    output: Option<&Path>,
    seed: u64,
    resamples: usize,
) -> Result<compare::Verdict> {
    let verdict = compare::compare(&compare::load(base)?, &compare::load(new)?, seed, resamples);
    println!("{}", verdict.table);
    if let Some(path) = output {
        std::fs::write(path, &verdict.table)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(verdict)
}

fn list<T>(text: &str, parse: impl Fn(&str) -> Result<T>) -> Result<Vec<T>> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(parse)
        .collect()
}

fn split_pair(text: &str, what: &str) -> Result<(String, String)> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() && !value.is_empty() => {
            Ok((key.to_owned(), value.to_owned()))
        }
        _ => bail!("{what} must be <key>=<value>, got {text:?}"),
    }
}

pub async fn run(command: BenchCommand) -> Result<()> {
    match command {
        BenchCommand::Freeze { out } => freeze(&out),
        BenchCommand::Run {
            bin,
            set,
            work,
            clones,
            output,
            levels,
            arms,
            embedding_url,
            embedding_model,
            repos,
            tasks,
            max_per_repo,
            label,
            baseline,
            table,
        } => {
            let levels = list(&levels, |level| match level {
                "B" | "b" => Ok('B'),
                "C" | "c" => Ok('C'),
                _ => bail!("unknown level {level:?} (B, C)"),
            })?;
            let arms = list(&arms, run::Arm::parse)?;
            anyhow::ensure!(!arms.is_empty(), "pass at least one arm");
            anyhow::ensure!(
                !levels.contains(&'B') || arms.contains(&run::Arm::Fusion),
                "level B's lookup needs the fusion arm"
            );
            let embedder = match (embedding_url, embedding_model) {
                (Some(url), Some(model)) => run::Embedder::External { url, model },
                _ => run::Embedder::Fake,
            };
            run::run(run::Options {
                bin,
                set,
                work,
                clones,
                output: output.clone(),
                level_b: levels.contains(&'B'),
                level_c: levels.contains(&'C'),
                arms,
                embedder,
                repos,
                tasks,
                max_per_repo,
                label,
            })
            .await?;
            if let Some(baseline) = baseline {
                print_comparison(
                    &baseline,
                    &output,
                    table.as_deref(),
                    compare::DEFAULT_SEED,
                    compare::DEFAULT_RESAMPLES,
                )?;
            }
            Ok(())
        }
        BenchCommand::Compare {
            base,
            new,
            output,
            seed,
            resamples,
            fail_on_regression,
        } => {
            let verdict = print_comparison(&base, &new, output.as_deref(), seed, resamples)?;
            if fail_on_regression && !verdict.regressions.is_empty() {
                bail!(
                    "{} regression(s) under the pre-registered rules",
                    verdict.regressions.len()
                );
            }
            Ok(())
        }
        BenchCommand::Mine {
            sources,
            licenses,
            max,
            version,
            out,
        } => {
            let licenses = licenses
                .iter()
                .map(|text| split_pair(text, "--license"))
                .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
            let sources = sources
                .iter()
                .map(|text| {
                    let (key, path) = split_pair(text, "--source")?;
                    let Some(license) = licenses.get(&key) else {
                        bail!("no --license given for source {key}");
                    };
                    Ok(mine::Source {
                        license: license.clone(),
                        key,
                        path: PathBuf::from(path),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            mine::run(sources, max, &version, &out)
        }
    }
}
