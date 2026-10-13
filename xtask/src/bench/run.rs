//! `cargo xtask bench run`: levels B and C of the spike bench for one frozen
//! build (`docs/spike/multimap/bench.md`, sections 2, 3 and 5).
//!
//! Per repository of the set: a cached clone, one `git worktree` checkout and
//! one sibling data directory under the work directory. Tasks are replayed
//! oldest first: check out the base commit, `index`, search with each arm
//! through the binary's `evaluate` command, then check out the fix commit and
//! `index` again (the update cost). Freshness is `manual`, so no search
//! refreshes an index.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::score::{distinct_files, score_lookup, score_task};
use crate::mine::git;

/// Schema id of a results file.
pub const RESULTS_SCHEMA: &str = "lci.bench-results/1";
/// Results window of the task query (the `@10` metrics need 10).
const TOP_K: usize = 10;
/// Hits asked of `sym.defs` per name: enough chunks for 5 distinct files
/// when one file declares a name several times.
const DEFS_HITS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    /// Semantic plus lexical, no reranker, no LSP.
    Fusion,
    /// Lexical only (the grep arm): reported, never a gate.
    Grep,
}

impl Arm {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fusion => "fusion",
            Self::Grep => "grep",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "fusion" => Ok(Self::Fusion),
            "grep" => Ok(Self::Grep),
            _ => bail!("unknown arm {text:?} (fusion, grep)"),
        }
    }
}

/// Where the embeddings come from.
pub enum Embedder {
    /// The bench's own deterministic fake (levels B and C).
    Fake,
    /// An already running OpenAI-compatible service (level D).
    External { url: String, model: String },
}

pub struct Options {
    pub bin: PathBuf,
    pub set: PathBuf,
    pub work: PathBuf,
    pub clones: Option<PathBuf>,
    pub output: PathBuf,
    pub level_b: bool,
    pub level_c: bool,
    pub arms: Vec<Arm>,
    pub embedder: Embedder,
    pub repos: Vec<String>,
    /// Only these task ids (empty: all).
    pub tasks: Vec<String>,
    pub max_per_repo: Option<usize>,
    pub label: Option<String>,
    /// Frozen `sym.defs` probe (`bench freeze-defs`) for level B `defs`.
    pub defs_probe: Option<PathBuf>,
}

/// UTC timestamp `YYYY-MM-DDTHH:MM:SSZ` of now (civil-from-days, no deps).
pub fn utc_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The `machine` fingerprint of rule 4: CPU model, logical cores, OS,
/// architecture and ripgrep version. No host name, user or path.
fn machine() -> Value {
    let mut system = sysinfo::System::new();
    system.refresh_cpu_all();
    let cpu = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_owned())
        .unwrap_or_default();
    let ripgrep = Command::new("rg")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unavailable".to_owned());
    json!({
        "cpu": cpu,
        "logical_cores": system.cpus().len(),
        "os": sysinfo::System::long_os_version().unwrap_or_default(),
        "arch": std::env::consts::ARCH,
        "ripgrep": ripgrep,
    })
}

/// Total size in bytes of every file under `dir`.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

/// TOML literal string for a path (single quotes; paths never hold one).
pub(crate) fn toml_path(path: &Path) -> Result<String> {
    let text = path.to_string_lossy().replace('\\', "/");
    ensure!(!text.contains('\''), "path {text} holds a quote");
    Ok(format!("'{text}'"))
}

pub fn arm_config(arm: Arm, data_dir: &Path, url: &str, model: &str) -> Result<String> {
    Ok(format!(
        "data_dir = {}\nembedding_url = \"{url}\"\nembedding_model = \"{model}\"\n\n\
         [channels]\nsemantic = {}\nlexical = true\nlsp = false\nreranker = false\n\n\
         [index]\nfreshness = \"manual\"\n",
        toml_path(data_dir)?,
        arm == Arm::Fusion,
    ))
}

fn toml_string(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

/// One evaluation query: text, expected paths, tag.
pub struct Query {
    pub text: String,
    pub expected: Vec<String>,
    pub tag: &'static str,
}

pub fn definition(queries: &[Query]) -> String {
    let mut out = String::from("[[workspaces]]\nkey = \"task\"\nrequired = true\n");
    for query in queries {
        let expected: Vec<String> = query.expected.iter().map(|p| toml_string(p)).collect();
        out.push_str(&format!(
            "\n[[queries]]\nworkspace = \"task\"\nquery = {}\ntop_k = {TOP_K}\nexpected_paths = [{}]\ntags = [\"{}\"]\n",
            toml_string(&query.text),
            expected.join(", "),
            query.tag,
        ));
    }
    out
}

/// The binary, run with a scratch configuration and no inherited `LCI_*`
/// variables, so nothing on the machine changes what is measured.
fn lci(bin: &Path, config: &Path) -> Command {
    let mut command = Command::new(bin);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("LCI_") {
            command.env_remove(&name);
        }
    }
    command.arg("--config").arg(config).arg("--no-local-config");
    command
}

fn run_json(mut command: Command, what: &str) -> Result<(Value, f64)> {
    let start = Instant::now();
    let output = command
        .output()
        .with_context(|| format!("running {what}"))?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    if !output.status.success() {
        bail!(
            "{what} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    let value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    Ok((value, ms))
}

fn index(bin: &Path, config: &Path, checkout: &Path) -> Result<Value> {
    let mut command = lci(bin, config);
    command.arg("index").arg(checkout);
    let (report, ms) = run_json(command, "lci index")?;
    Ok(json!({
        "ms": ms,
        "files": report["files"],
        "chunks": report["chunks"],
        "embedded_chunks": report["embedded_chunks"],
        "reused_chunks": report["reused_chunks"],
    }))
}

/// The `sym.defs` probe's report for `symbols` at the checkout's current
/// commit, and its wall time.
fn run_defs_probe(
    probe: &Path,
    checkout: &Path,
    store: &Path,
    symbols: &[(String, Vec<String>)],
) -> Result<(Value, f64)> {
    use std::io::Write;
    let names: String = symbols
        .iter()
        .map(|(name, _)| format!("{name}\n"))
        .collect();
    let start = Instant::now();
    let mut child = Command::new(probe)
        .arg(checkout)
        .arg(store)
        .arg(DEFS_HITS.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running the defs probe")?;
    child
        .stdin
        .take()
        .context("defs probe stdin")?
        .write_all(names.as_bytes())?;
    let output = child.wait_with_output()?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    if !output.status.success() {
        bail!(
            "defs probe failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    let report = serde_json::from_slice(&output.stdout).context("reading the defs probe report")?;
    Ok((report, ms))
}

/// Ranked evidence paths and timing per query text, from an evaluate report.
fn evaluate(
    bin: &Path,
    config: &Path,
    checkout: &Path,
    definition_path: &Path,
    report_path: &Path,
) -> Result<Vec<(String, Value)>> {
    let _ = std::fs::remove_file(report_path);
    let mut command = lci(bin, config);
    command
        .arg("evaluate")
        .arg(definition_path)
        .arg("--workspace")
        .arg(format!("task={}", checkout.display()))
        .arg("--output")
        .arg(report_path);
    let output = command.output().context("running lci evaluate")?;
    let text = std::fs::read_to_string(report_path).with_context(|| {
        format!(
            "lci evaluate wrote no report ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .join(" | ")
        )
    })?;
    let report: Value = serde_json::from_str(&text).context("reading the evaluate report")?;
    parse_report(&report)
}

/// Ranked distinct files, timing and any fail-open warning per query text
/// of an evaluate report. A search that failed is reported by `evaluate` as
/// outcome `failure` with no timings, and a skipped query has none either:
/// both become errors. (`failure` alone also means "ran but missed the
/// expected file", so the outcome is not the test.)
pub fn parse_report(report: &Value) -> Result<Vec<(String, Value)>> {
    let queries = report["queries"]
        .as_array()
        .context("evaluate report has no queries")?;
    Ok(queries
        .iter()
        .map(|query| {
            let text = query["query"]["query"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let result = if query["skipped"].as_bool() == Some(true) || query["timings"].is_null() {
                json!({"error": format!(
                    "{}: {}",
                    query["outcome"].as_str().unwrap_or("skipped"),
                    query["reason"].as_str().unwrap_or_default()
                )})
            } else {
                let paths: Vec<String> = query["evidence"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| row["relative_file_path"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut row = json!({
                    "files": distinct_files(&paths, TOP_K),
                    "total_ms": query["timings"]["total_ms"],
                });
                if let Some(warning) = query["warning"].as_str() {
                    row["warning"] = json!(warning);
                }
                row
            };
            (text, result)
        })
        .collect())
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Level B `defs` rows for a task whose probe never answered: one error row
/// per gold name, so a failure counts in the same unit as the scored names.
pub fn defs_error_rows(symbols: &[(String, Vec<String>)], reason: &str) -> Vec<Value> {
    symbols
        .iter()
        .map(|(name, paths)| json!({"name": name, "gold_files": paths, "error": reason}))
        .collect()
}

/// A task's gold symbols grouped by bare name, in set order: each name with
/// the gold files whose changed declaration it is.
pub fn gold_symbols(task: &Value) -> Vec<(String, Vec<String>)> {
    let mut symbols: Vec<(String, Vec<String>)> = Vec::new();
    for symbol in task["gold_symbols"].as_array().into_iter().flatten() {
        let name = symbol["name"].as_str().unwrap_or_default().to_owned();
        let path = symbol["path"].as_str().unwrap_or_default().to_owned();
        match symbols.iter_mut().find(|(n, _)| *n == name) {
            Some((_, paths)) => paths.push(path),
            None => symbols.push((name, vec![path])),
        }
    }
    symbols
}

/// Level B `defs` rows from a `sym.defs` probe report: per gold symbol name,
/// the distinct files of the map's answer in rank order, scored like the
/// lookup proxy (hit and precision over the top 5 distinct files). A name
/// the report does not answer is an error row, never a zero.
pub fn defs_rows(report: &Value, symbols: &[(String, Vec<String>)]) -> Vec<Value> {
    let answers = report["answers"].as_array().map_or(&[][..], Vec::as_slice);
    symbols
        .iter()
        .map(|(name, paths)| {
            let mut row = json!({"name": name, "gold_files": paths});
            match answers
                .iter()
                .find(|answer| answer["name"] == name.as_str())
            {
                Some(answer) => {
                    let hits: Vec<String> = strings(&answer["files"])
                        .into_iter()
                        .filter(|path| !path.is_empty())
                        .collect();
                    let files = distinct_files(&hits, TOP_K);
                    let (hit5, precision5) = score_lookup(&files, paths);
                    row["files"] = json!(files);
                    row["hit5"] = json!(hit5);
                    row["precision5"] = json!(precision5);
                    row["ms"] = answer["ms"].clone();
                }
                None => row["error"] = json!("name missing from the probe report"),
            }
            row
        })
        .collect()
}

/// The clone of `url` under `clones/key`, fetched with blobs on demand.
fn ensure_clone(clones: &Path, key: &str, url: &str, snapshot: &str) -> Result<PathBuf> {
    let clone = clones.join(key);
    if !clone.join(".git").exists() && !clone.join("HEAD").exists() {
        std::fs::create_dir_all(clones)?;
        let status = Command::new("git")
            .args([
                "clone",
                "--quiet",
                "--filter=blob:none",
                "--no-checkout",
                url,
            ])
            .arg(&clone)
            .status()
            .context("running git clone")?;
        ensure!(status.success(), "git clone {url} failed");
    }
    if git(
        &clone,
        &["cat-file", "-e", &format!("{snapshot}^{{commit}}")],
    )
    .is_err()
    {
        git(&clone, &["fetch", "--quiet", "origin", snapshot])?;
    }
    Ok(clone)
}

fn checkout(worktree: &Path, commit: &str) -> Result<()> {
    git(
        worktree,
        &["checkout", "--quiet", "--force", "--detach", commit],
    )?;
    git(worktree, &["clean", "--quiet", "-fdx"])?;
    Ok(())
}

pub async fn run(options: Options) -> Result<()> {
    let started = Instant::now();
    let started_at = utc_now();
    let set: Value = serde_json::from_str(
        &std::fs::read_to_string(&options.set)
            .with_context(|| format!("reading {}", options.set.display()))?,
    )?;
    ensure!(
        set["schema"] == super::mine::SET_SCHEMA,
        "{} is not a bench set",
        options.set.display()
    );
    let bin = std::path::absolute(&options.bin)?;
    ensure!(bin.is_file(), "no binary at {}", bin.display());
    let build = std::fs::read_to_string(bin.with_extension("json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({"commit": "unknown"}));
    // Absolute, so a bare file name is never looked up on PATH.
    let probe = options
        .defs_probe
        .as_deref()
        .map(std::path::absolute)
        .transpose()?;
    let defs_probe = match &probe {
        Some(probe) => {
            ensure!(probe.is_file(), "no defs probe at {}", probe.display());
            ensure!(options.level_b, "--defs-probe needs level B");
            let sidecar = std::fs::read_to_string(probe.with_extension("json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or_else(|| json!({"commit": "unknown"}));
            if sidecar["commit"] != build["commit"] {
                eprintln!(
                    "warning: the defs probe ({}) and the binary ({}) are not frozen from one commit",
                    sidecar["commit"], build["commit"]
                );
            }
            sidecar
        }
        None => Value::Null,
    };
    let work = std::path::absolute(&options.work)?;
    // The runner deletes checkouts and data directories under the work
    // directory: only in one it created (marker file) or an empty one.
    let marker = work.join(".lci-bench-work");
    if work.exists() && !marker.exists() && std::fs::read_dir(&work)?.next().is_some() {
        bail!(
            "{} is not empty and was not created by `bench run`; pass a new or empty directory",
            work.display()
        );
    }
    std::fs::create_dir_all(&work)?;
    std::fs::write(&marker, "cargo xtask bench run work directory\n")?;
    let clones = match &options.clones {
        Some(dir) => std::path::absolute(dir)?,
        None => work.join("clones"),
    };

    let (url, model, embedder) = match &options.embedder {
        Embedder::Fake => (
            super::embedder::start()?,
            super::embedder::MODEL.to_owned(),
            json!({"kind": "fake", "model": super::embedder::MODEL, "dimension": super::embedder::DIMENSION}),
        ),
        Embedder::External { url, model } => (
            url.clone(),
            model.clone(),
            json!({"kind": "external", "model": model}),
        ),
    };

    let mut repos_out = Vec::new();
    let mut tasks_out = Vec::new();
    for repo in set["repos"].as_array().context("set has no repos")? {
        let key = repo["key"].as_str().context("repo key")?;
        if !options.repos.is_empty() && !options.repos.iter().any(|r| r == key) {
            continue;
        }
        let mut tasks: Vec<&Value> = set["tasks"]
            .as_array()
            .context("set has no tasks")?
            .iter()
            .filter(|task| task["repo"] == key)
            .filter(|task| {
                options.tasks.is_empty() || options.tasks.iter().any(|id| task["id"] == id.as_str())
            })
            .collect();
        if let Some(max) = options.max_per_repo {
            tasks.truncate(max);
        }
        if tasks.is_empty() {
            continue;
        }
        let clone = ensure_clone(
            &clones,
            key,
            repo["url"].as_str().context("repo url")?,
            repo["snapshot"].as_str().context("repo snapshot")?,
        )?;
        let root = work.join(key);
        let worktree = root.join("checkout");
        let data = root.join("data");
        if worktree.exists() {
            let _ = git(
                &clone,
                &["worktree", "remove", "--force", &worktree.to_string_lossy()],
            );
            let _ = std::fs::remove_dir_all(&worktree);
        }
        let _ = git(&clone, &["worktree", "prune"]);
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(root.join("defs-store"));
        std::fs::create_dir_all(&root)?;
        let first_base = tasks[0]["base_commit"].as_str().context("base commit")?;
        git(
            &clone,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                "--force",
                &worktree.to_string_lossy(),
                first_base,
            ],
        )?;
        let mut configs = Vec::new();
        for arm in &options.arms {
            let path = root.join(format!("{}.toml", arm.name()));
            std::fs::write(&path, arm_config(*arm, &data, &url, &model)?)?;
            configs.push((*arm, path));
        }
        let index_config = &configs[0].1;

        eprintln!("{key}: {} tasks", tasks.len());
        let mut repo_index = Value::Null;
        for (position, task) in tasks.iter().enumerate() {
            let id = task["id"].as_str().context("task id")?;
            let base = task["base_commit"].as_str().context("base commit")?;
            let fix = task["fix_commit"].as_str().context("fix commit")?;
            let gold = strings(&task["gold_files"]);
            checkout(&worktree, base)?;
            let refresh = match index(&bin, index_config, &worktree) {
                Ok(report) => report,
                Err(error) => {
                    let error = format!("{error:#}");
                    let mut row =
                        json!({"id": id, "repo": key, "tags": task["tags"], "error": error});
                    // Every gold name the probe would have answered stays
                    // counted, as an error.
                    if options.defs_probe.is_some() && options.level_b {
                        row["defs"] = json!(defs_error_rows(
                            &gold_symbols(task),
                            &format!("index failed: {error}")
                        ));
                    }
                    tasks_out.push(row);
                    continue;
                }
            };
            if position == 0 {
                repo_index = json!({
                    "ms": refresh["ms"],
                    "files": refresh["files"],
                    "chunks": refresh["chunks"],
                    "size_bytes": dir_size(&data),
                });
            }

            let mut arms = serde_json::Map::new();
            let mut lookup = Vec::new();
            for (arm, config) in &configs {
                let mut queries = vec![Query {
                    text: task["query"].as_str().unwrap_or_default().to_owned(),
                    expected: gold.clone(),
                    tag: "task",
                }];
                let mut symbols: Vec<(String, Vec<String>)> = Vec::new();
                if options.level_b && *arm == Arm::Fusion {
                    symbols = gold_symbols(task);
                    // A symbol named exactly like the task query would pair
                    // with the wrong row; skip it.
                    symbols.retain(|(name, _)| *name != queries[0].text);
                    queries.extend(symbols.iter().map(|(name, paths)| Query {
                        text: name.clone(),
                        expected: paths.clone(),
                        tag: "lookup",
                    }));
                }
                if !options.level_c && symbols.is_empty() {
                    continue;
                }
                let definition_path = root.join(format!("{}-definition.toml", arm.name()));
                std::fs::write(&definition_path, definition(&queries))?;
                let report_path = root.join(format!("{}-report.json", arm.name()));
                let results = evaluate(&bin, config, &worktree, &definition_path, &report_path);
                let find = |text: &str| -> Value {
                    match &results {
                        Ok(rows) => rows.iter().find(|(query, _)| query == text).map_or_else(
                            || json!({"error": "query missing from the report"}),
                            |(_, v)| v.clone(),
                        ),
                        Err(error) => json!({"error": format!("{error:#}")}),
                    }
                };
                if options.level_c {
                    let mut row = find(&queries[0].text);
                    if row.get("error").is_none() {
                        let files = strings(&row["files"]);
                        let score = score_task(&files, &gold);
                        row["rr"] = json!(score.rr);
                        row["hit1"] = json!(score.hit1);
                        row["hit5"] = json!(score.hit5);
                        row["hit10"] = json!(score.hit10);
                        row["recall10"] = json!(score.recall10);
                    }
                    arms.insert(arm.name().to_owned(), row);
                }
                for (name, paths) in &symbols {
                    let mut row = find(name);
                    if row.get("error").is_none() {
                        let (hit5, precision5) = score_lookup(&strings(&row["files"]), paths);
                        row["hit5"] = json!(hit5);
                        row["precision5"] = json!(precision5);
                    }
                    row["name"] = json!(name);
                    row["gold_files"] = json!(paths);
                    lookup.push(row);
                }
            }

            // `sym.defs` at the base commit, through the probe frozen from
            // the same tree (level B `defs`); its bundle store persists
            // across the repository's tasks, as a map's data would.
            let (defs, defs_build) = match (&probe, options.level_b) {
                (Some(probe), true) => {
                    let symbols = gold_symbols(task);
                    match run_defs_probe(probe, &worktree, &root.join("defs-store"), &symbols) {
                        Ok((report, ms)) => (
                            json!(defs_rows(&report, &symbols)),
                            json!({
                                "ms": ms,
                                "files": report["files"],
                                "chunks": report["chunks"],
                                "declarations": report["declarations"],
                                "scan_ms": report["scan_ms"],
                                "build_ms": report["build_ms"],
                                "reused": report["reused"],
                                "extracted": report["extracted"],
                            }),
                        ),
                        Err(error) => {
                            let error = format!("{error:#}");
                            (
                                // `run_defs_probe` already names the probe.
                                json!(defs_error_rows(&symbols, &error)),
                                json!({"error": error}),
                            )
                        }
                    }
                }
                _ => (Value::Null, Value::Null),
            };

            let update = if options.level_b {
                checkout(&worktree, fix)?;
                match index(&bin, index_config, &worktree) {
                    Ok(report) => report,
                    Err(error) => json!({"error": format!("{error:#}")}),
                }
            } else {
                Value::Null
            };
            eprintln!(
                "  {id}: fusion rr {} grep rr {}",
                arms.get("fusion").map_or(Value::Null, |a| a["rr"].clone()),
                arms.get("grep").map_or(Value::Null, |a| a["rr"].clone()),
            );
            tasks_out.push(json!({
                "id": id,
                "repo": key,
                "tags": task["tags"],
                "arms": arms,
                "lookup": if options.level_b { json!(lookup) } else { Value::Null },
                "defs": defs,
                "defs_build": defs_build,
                "refresh": refresh,
                "update": update,
            }));
        }
        repos_out.push(json!({
            "key": key,
            "tasks": tasks.len(),
            "index": repo_index,
        }));
        let _ = git(
            &clone,
            &["worktree", "remove", "--force", &worktree.to_string_lossy()],
        );
    }

    let mut unavailable = serde_json::Map::new();
    if options.defs_probe.is_none() {
        unavailable.insert(
            "defs".to_owned(),
            json!("no sym.defs probe was given (bench.md 2.1, D2)"),
        );
    }
    unavailable.insert(
        "navigation".to_owned(),
        json!("the runner has no language servers (bench.md 2.1, D2)"),
    );
    let levels: Vec<&str> = [("B", options.level_b), ("C", options.level_c)]
        .iter()
        .filter(|(_, on)| *on)
        .map(|(level, _)| *level)
        .collect();
    let results = json!({
        "schema": RESULTS_SCHEMA,
        "label": options.label,
        "bench_set": {"version": set["version"], "tasks": set["tasks"].as_array().map_or(0, Vec::len)},
        "build": build,
        "binary": bin.file_name().map(|n| n.to_string_lossy().into_owned()),
        "machine": machine(),
        "embedder": embedder,
        "levels": levels,
        "defs_probe": defs_probe,
        "unavailable": unavailable,
        "arms": options.arms.iter().map(|arm| arm.name()).collect::<Vec<_>>(),
        "started_at": started_at,
        "finished_at": utc_now(),
        "wall_ms": started.elapsed().as_secs_f64() * 1000.0,
        "repos": repos_out,
        "tasks": tasks_out,
    });
    if let Some(parent) = options.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&results)?;
    text.push('\n');
    std::fs::write(&options.output, text)
        .with_context(|| format!("writing {}", options.output.display()))?;
    eprintln!("wrote {}", options.output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_now_has_the_rfc3339_shape() {
        let now = utc_now();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z') && now.as_bytes()[10] == b'T', "{now}");
        assert!(now.as_str() >= "2026-01-01T00:00:00Z", "{now}");
    }

    #[test]
    fn arm_configs_switch_only_the_semantic_channel() {
        let fusion =
            arm_config(Arm::Fusion, Path::new("/d"), "http://127.0.0.1:1/v1", "m").unwrap();
        let grep = arm_config(Arm::Grep, Path::new("/d"), "http://127.0.0.1:1/v1", "m").unwrap();
        assert!(fusion.contains("semantic = true") && grep.contains("semantic = false"));
        for config in [&fusion, &grep] {
            let parsed: toml::Value = toml::from_str(config).unwrap();
            assert_eq!(parsed["channels"]["reranker"].as_bool(), Some(false));
            assert_eq!(parsed["channels"]["lsp"].as_bool(), Some(false));
            assert_eq!(parsed["index"]["freshness"].as_str(), Some("manual"));
            assert_eq!(parsed["data_dir"].as_str(), Some("/d"));
        }
    }

    #[test]
    fn report_parsing_turns_failed_searches_into_errors() {
        let report = json!({"queries": [
            {"query": {"query": "ok"}, "skipped": false, "outcome": "failure", "reason": "expected not found",
             "evidence": [{"relative_file_path": "a.rs"}, {"relative_file_path": "a.rs"}, {"relative_file_path": "b.rs"}],
             "timings": {"total_ms": 12.5}, "warning": "semantic channel unavailable"},
            {"query": {"query": "broken"}, "skipped": false, "outcome": "failure",
             "reason": "search failed: boom", "evidence": []},
            {"query": {"query": "gone"}, "skipped": true, "outcome": "skipped", "reason": "no workspace"},
        ]});
        let rows = parse_report(&report).unwrap();
        assert_eq!(rows[0].0, "ok");
        assert_eq!(rows[0].1["files"], json!(["a.rs", "b.rs"]));
        assert_eq!(rows[0].1["total_ms"], json!(12.5));
        assert_eq!(rows[0].1["warning"], json!("semantic channel unavailable"));
        assert!(rows[0].1.get("error").is_none());
        assert_eq!(rows[1].1["error"], json!("failure: search failed: boom"));
        assert!(rows[2].1["error"].as_str().unwrap().starts_with("skipped"));
        assert!(parse_report(&json!({})).is_err());
    }

    #[test]
    fn gold_symbols_group_paths_by_name_in_set_order() {
        let task = json!({"gold_symbols": [
            {"name": "poll", "path": "a.rs"},
            {"name": "Inner", "path": "a.rs"},
            {"name": "poll", "path": "b.rs"},
        ]});
        assert_eq!(
            gold_symbols(&task),
            vec![
                (
                    "poll".to_owned(),
                    vec!["a.rs".to_owned(), "b.rs".to_owned()]
                ),
                ("Inner".to_owned(), vec!["a.rs".to_owned()]),
            ]
        );
        assert!(gold_symbols(&json!({})).is_empty());
    }

    #[test]
    fn defs_rows_score_the_maps_distinct_files_against_the_gold() {
        let report = json!({"answers": [
            // Two chunks of one file count once; the unknown chunk ("")
            // is not a file.
            {"name": "poll", "files": ["x.rs", "x.rs", "", "a.rs", "y.rs"], "ms": 0.5},
            {"name": "Inner", "files": ["p.rs", "q.rs", "r.rs", "s.rs", "t.rs", "a.rs"], "ms": 0.1},
            {"name": "absent", "files": [], "ms": 0.1},
        ]});
        let symbols = vec![
            ("poll".to_owned(), vec!["a.rs".to_owned()]),
            ("Inner".to_owned(), vec!["a.rs".to_owned()]),
            ("absent".to_owned(), vec!["a.rs".to_owned()]),
            ("unanswered".to_owned(), vec!["a.rs".to_owned()]),
        ];
        let rows = defs_rows(&report, &symbols);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["files"], json!(["x.rs", "a.rs", "y.rs"]));
        assert_eq!(rows[0]["hit5"], json!(true));
        assert!((rows[0]["precision5"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-12);
        // The gold file sixth among distinct files is outside the top 5.
        assert_eq!(rows[1]["hit5"], json!(false));
        assert_eq!(rows[1]["precision5"], json!(0.0));
        // An empty answer is scored (a miss); a missing one is an error.
        assert_eq!(
            (rows[2]["hit5"].clone(), rows[2].get("error")),
            (json!(false), None)
        );
        assert!(rows[3]["error"].is_string());
        assert!(rows[3].get("hit5").is_none());
        for (row, (name, gold)) in rows.iter().zip(&symbols) {
            assert_eq!(row["name"], json!(name));
            assert_eq!(row["gold_files"], json!(gold));
        }
    }

    #[test]
    fn a_task_the_probe_never_answered_has_one_error_row_per_gold_name() {
        let symbols = vec![
            (
                "poll".to_owned(),
                vec!["a.rs".to_owned(), "b.rs".to_owned()],
            ),
            ("Inner".to_owned(), vec!["a.rs".to_owned()]),
        ];
        let rows = defs_error_rows(&symbols, "defs probe failed: boom");
        assert_eq!(rows.len(), 2);
        for (row, (name, gold)) in rows.iter().zip(&symbols) {
            assert_eq!(row["name"], json!(name));
            assert_eq!(row["gold_files"], json!(gold));
            assert_eq!(row["error"], json!("defs probe failed: boom"));
            assert!(row.get("hit5").is_none() && row.get("precision5").is_none());
        }
    }

    #[test]
    fn definitions_parse_and_quote_queries() {
        let text = definition(&[
            Query {
                text: "fix \"quoted\" path\\thing".into(),
                expected: vec!["src/a.rs".into()],
                tag: "task",
            },
            Query {
                text: "poll_ready".into(),
                expected: vec!["src/a.rs".into(), "src/b.rs".into()],
                tag: "lookup",
            },
        ]);
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let queries = parsed["queries"].as_array().unwrap();
        assert_eq!(
            queries[0]["query"].as_str(),
            Some("fix \"quoted\" path\\thing")
        );
        assert_eq!(queries[0]["top_k"].as_integer(), Some(10));
        assert_eq!(queries[1]["expected_paths"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["workspaces"][0]["key"].as_str(), Some("task"));
    }
}
