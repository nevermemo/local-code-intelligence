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
    /// Build the `sym.defs` probe (`xtask/bench-defs-probe`) against the
    /// `lci-core` and `lci-maps` of the source tree `--src` (a checkout of
    /// the commit being benched) and copy it to `--out`, with `<out>.json`
    /// recording the tree's commit. No build exposes `sym.defs` through a
    /// CLI, so level B `defs` measures the map through this probe.
    FreezeDefs {
        #[arg(long)]
        src: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Run levels B and C for one frozen build against a bench set and
    /// write a results JSON. With --baseline also print (and with --table
    /// write) the comparison against an earlier results file.
    Run(Box<RunArgs>),
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

#[derive(clap::Args)]
pub struct RunArgs {
    /// Frozen binary (from `bench freeze`).
    #[arg(long)]
    pub bin: PathBuf,
    /// Bench set.
    #[arg(long, default_value = DEFAULT_SET)]
    pub set: PathBuf,
    /// Work directory: checkouts, data directories, scratch configs.
    /// Must not be inside an indexed repository.
    #[arg(long)]
    pub work: PathBuf,
    /// Cache of clones (default: <work>/clones).
    #[arg(long)]
    pub clones: Option<PathBuf>,
    /// Results JSON to write.
    #[arg(long)]
    pub output: PathBuf,
    /// Levels to run: B, C or both (comma-separated).
    #[arg(long, default_value = "B,C")]
    pub levels: String,
    /// Arms (comma-separated): fusion, grep.
    #[arg(long, default_value = "fusion,grep")]
    pub arms: String,
    /// Use this OpenAI-compatible embedding service instead of the fake
    /// one (level D). Needs --embedding-model.
    #[arg(long, requires = "embedding_model")]
    pub embedding_url: Option<String>,
    #[arg(long)]
    pub embedding_model: Option<String>,
    /// Only these repositories (repeatable).
    #[arg(long = "repo")]
    pub repos: Vec<String>,
    /// Only these task ids (repeatable). A task run alone is indexed
    /// from scratch at its base commit (the incremental-equals-full
    /// check).
    #[arg(long = "task")]
    pub tasks: Vec<String>,
    /// At most this many tasks per repository (the oldest first).
    #[arg(long)]
    pub max_per_repo: Option<usize>,
    /// Free-text label stored in the results.
    #[arg(long)]
    pub label: Option<String>,
    /// Frozen `sym.defs` probe (`bench freeze-defs`), frozen from the same
    /// commit as --bin: level B then scores `defs`.
    #[arg(long)]
    pub defs_probe: Option<PathBuf>,
    /// Earlier results file to compare against.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Write the comparison table (Markdown) here.
    #[arg(long, requires = "baseline")]
    pub table: Option<PathBuf>,
    /// Exit 1 when the comparison with --baseline lists a regression.
    #[arg(long, requires = "baseline")]
    pub fail_on_regression: bool,
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

/// Source of the `sym.defs` probe, built outside the workspace.
const DEFS_PROBE_SOURCE: &str = include_str!("../../bench-defs-probe/main.rs");
/// Name of the probe's package and binary.
const DEFS_PROBE: &str = "bench-defs-probe";

/// Manifest of the probe's scratch package: path dependencies on the
/// frozen tree's crates, its own empty workspace.
pub fn defs_probe_manifest(src: &Path) -> Result<String> {
    Ok(format!(
        "[package]\nname = \"{DEFS_PROBE}\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
         [[bin]]\nname = \"{DEFS_PROBE}\"\npath = \"src/main.rs\"\n\n\
         [dependencies]\nlci-core = {{ path = {} }}\nlci-maps = {{ path = {} }}\nserde_json = \"1\"\n\n\
         [workspace]\n",
        run::toml_path(&src.join("lci-core"))?,
        run::toml_path(&src.join("lci-maps"))?,
    ))
}

/// `name version` of every package in the probe's lock that the tree's
/// lock does not hold (the probe's own package aside).
pub fn foreign_lock_packages(tree: &str, probe: &str) -> Result<Vec<String>> {
    fn packages(lock: &str) -> Result<Vec<(String, String)>> {
        let lock: toml::Value = toml::from_str(lock).context("reading a Cargo.lock")?;
        Ok(lock
            .get("package")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .map(|package| {
                let field = |key| package.get(key).and_then(toml::Value::as_str);
                (
                    field("name").unwrap_or_default().to_owned(),
                    field("version").unwrap_or_default().to_owned(),
                )
            })
            .collect())
    }
    let tree = packages(tree)?;
    Ok(packages(probe)?
        .into_iter()
        .filter(|(name, _)| name != DEFS_PROBE)
        .filter(|package| !tree.contains(package))
        .map(|(name, version)| format!("{name} {version}"))
        .collect())
}

fn freeze_defs(src: &Path, out: &Path) -> Result<()> {
    let src = std::path::absolute(src)?;
    let out = std::path::absolute(out)?;
    if !src.join("lci-maps/src/symdefs.rs").is_file() {
        bail!(
            "{} has no sym.defs (lci-maps/src/symdefs.rs)",
            src.display()
        );
    }
    // The package lives beside the output, never inside the frozen tree.
    let package = out.with_extension("src");
    std::fs::create_dir_all(package.join("src"))?;
    std::fs::write(package.join("Cargo.toml"), defs_probe_manifest(&src)?)?;
    std::fs::write(package.join("src/main.rs"), DEFS_PROBE_SOURCE)?;
    // The tree's lock pins every shared dependency to the frozen versions.
    std::fs::copy(src.join("Cargo.lock"), package.join("Cargo.lock"))
        .context("copying the tree's Cargo.lock")?;
    // Absolute, so cargo (run in the package) and the copy below agree.
    let target = std::path::absolute(
        std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| package.join("target")),
    )?;
    let mut build = Command::new("cargo");
    build
        .current_dir(&package)
        .args(["build", "--offline", "--bin", DEFS_PROBE])
        .env("CARGO_TARGET_DIR", &target);
    use_local_protoc(&mut build);
    let status = build.status().context("running cargo build")?;
    anyhow::ensure!(
        status.success(),
        "cargo build of the defs probe failed: {status}"
    );
    // `--locked` cannot be passed (cargo prunes the copy), so check that
    // the resolved lock holds only versions of the tree's lock.
    let foreign = foreign_lock_packages(
        &std::fs::read_to_string(src.join("Cargo.lock"))?,
        &std::fs::read_to_string(package.join("Cargo.lock"))?,
    )?;
    anyhow::ensure!(
        foreign.is_empty(),
        "the defs probe resolved packages outside the tree's Cargo.lock: {}",
        foreign.join(", ")
    );
    let built = target
        .join("debug")
        .join(format!("{DEFS_PROBE}{}", std::env::consts::EXE_SUFFIX));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&built, &out)
        .with_context(|| format!("copying {} to {}", built.display(), out.display()))?;
    let commit = git(&src, &["rev-parse", "HEAD"])?.trim().to_owned();
    let dirty = !git(&src, &["status", "--porcelain", "--untracked-files=no"])?
        .trim()
        .is_empty();
    let sidecar = json!({"commit": commit, "dirty": dirty, "profile": "debug", "kind": DEFS_PROBE});
    std::fs::write(out.with_extension("json"), format!("{sidecar:#}\n"))?;
    eprintln!(
        "froze the defs probe of {commit}{} to {}",
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
        BenchCommand::FreezeDefs { src, out } => freeze_defs(&src, &out),
        BenchCommand::Run(args) => {
            let RunArgs {
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
                defs_probe,
                baseline,
                table,
                fail_on_regression,
            } = *args;
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
                defs_probe,
            })
            .await?;
            if let Some(baseline) = baseline {
                let verdict = print_comparison(
                    &baseline,
                    &output,
                    table.as_deref(),
                    compare::DEFAULT_SEED,
                    compare::DEFAULT_RESAMPLES,
                )?;
                if fail_on_regression && !verdict.regressions.is_empty() {
                    bail!(
                        "{} regression(s) under the pre-registered rules",
                        verdict.regressions.len()
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_lock_with_a_version_the_tree_lacks_is_named() {
        let tree = "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.1\"\n\n\
                    [[package]]\nname = \"lci-core\"\nversion = \"0.1.0\"\n";
        let same = "version = 4\n\n[[package]]\nname = \"bench-defs-probe\"\nversion = \"0.0.0\"\n\n\
                    [[package]]\nname = \"serde\"\nversion = \"1.0.1\"\n";
        assert!(foreign_lock_packages(tree, same).unwrap().is_empty());
        let newer = same.replace("1.0.1", "1.0.2");
        assert_eq!(
            foreign_lock_packages(tree, &newer).unwrap(),
            vec!["serde 1.0.2".to_owned()]
        );
        assert!(foreign_lock_packages("not toml [", same).is_err());
    }

    #[test]
    fn the_defs_probe_package_depends_only_on_the_frozen_trees_crates() {
        let manifest = defs_probe_manifest(Path::new("/frozen/tree")).unwrap();
        let parsed: toml::Value = toml::from_str(&manifest).unwrap();
        let dependencies = parsed["dependencies"].as_table().unwrap();
        assert_eq!(
            dependencies["lci-core"]["path"].as_str(),
            Some("/frozen/tree/lci-core")
        );
        assert_eq!(
            dependencies["lci-maps"]["path"].as_str(),
            Some("/frozen/tree/lci-maps")
        );
        assert_eq!(dependencies.len(), 3, "{manifest}");
        // Its own workspace, so cargo never joins it to another one.
        assert!(parsed["workspace"].as_table().unwrap().is_empty());
        assert_eq!(parsed["bin"][0]["name"].as_str(), Some(DEFS_PROBE));
        // The probe answers through the map's own ranking.
        assert!(DEFS_PROBE_SOURCE.contains("symdefs::{SymDefs, answer}"));
    }
}
