//! `cargo xtask gold external`: external held-out gold. Bug-localisation rows
//! from two public datasets (SWE-PolyBench Verified, MIT; LongCodeArena bug
//! localization, Apache-2.0) become where-to `exemplar` items at file
//! granularity, so the existing where-to scorer and `to-evaluate` read them
//! without a new set.
//!
//! The contract:
//!
//! * Held out. The items say so (`external.heldout: true`) and nothing here is
//!   called from a tuning, preset or default-choosing path. Cycle 2 reads the
//!   set once, at the end, as a held-out check.
//! * No data in the repository. The converter reads the dataset files given on
//!   the command line and writes the gold under a directory given on the
//!   command line; the owner's local paths live only in local configs and
//!   scripts. The report holds counts and hashes, never question or patch
//!   text.
//! * The converter reads only the rows: no tree, no network, no clone.
//!
//! `fetch` downloads the official files pinned at one dataset revision and
//! records their hashes in `fetch.json`. `convert` reads a SWE-PolyBench CSV
//! directly. LongCodeArena ships parquet, and the repository has no parquet
//! reader (d499): a one-off conversion outside the repository turns each
//! `<subset>/test-00000-of-00001.parquet` into `<subset>.jsonl`, one JSON object
//! per row with the parquet column names, and `convert` reads those.

use super::strata::{GENERIC_STEMS, file_stem, holds_stem};
use super::write_file;
use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The question goes on a command line in the bench; Windows caps a command
/// line near 32k characters.
pub(crate) const QUESTION_MAX_CHARS: usize = 8000;

/// Directory tokens that say nothing when a question holds them.
const GENERIC_DIR_TOKENS: [&str; 11] = [
    "src", "lib", "main", "app", "core", "test", "java", "python", "node", "packages", "common",
];

/// Shortest directory token that counts for the leak check.
const MIN_DIR_TOKEN: usize = 4;

/// The LongCodeArena subsets and the one split of each that is used.
const LCA_SUBSETS: [&str; 3] = ["py", "java", "kt"];
const LCA_TEST_FILE: &str = "test-00000-of-00001.parquet";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Source {
    SwePolybenchVerified,
    LcaBugloc,
}

impl Source {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Source::SwePolybenchVerified => "swe-polybench-verified",
            Source::LcaBugloc => "lca-bugloc",
        }
    }

    pub(crate) fn dataset(self) -> &'static str {
        match self {
            Source::SwePolybenchVerified => "AmazonScience/SWE-PolyBench_Verified",
            Source::LcaBugloc => "JetBrains-Research/lca-bug-localization",
        }
    }

    pub(crate) fn licence(self) -> &'static str {
        match self {
            Source::SwePolybenchVerified => "MIT",
            Source::LcaBugloc => "Apache-2.0",
        }
    }

    pub(crate) fn url(self) -> String {
        format!("https://huggingface.co/datasets/{}", self.dataset())
    }

    /// The dataset files `fetch` downloads, relative to the dataset root.
    pub(crate) fn fetch_files(self, subsets: &[String]) -> Vec<String> {
        match self {
            Source::SwePolybenchVerified => vec!["test.csv".to_string()],
            Source::LcaBugloc => {
                let mut files: Vec<String> = subsets
                    .iter()
                    .map(|s| format!("{s}/{LCA_TEST_FILE}"))
                    .collect();
                files.push("repos.json".to_string());
                files
            }
        }
    }
}

#[derive(Subcommand)]
pub enum ExternalCommand {
    /// Download the official files of one source from its Hugging Face dataset
    /// page into `<out>/<source>/`, pinned at the revision `main` has now, and
    /// record revision, size and sha256 of each in `fetch.json`. Refuses to
    /// overwrite a file with different content unless `--force`.
    Fetch {
        #[arg(long, value_enum)]
        source: Source,
        /// Directory that receives `<source>/` (outside the repository).
        #[arg(long)]
        out: PathBuf,
        /// lca-bugloc subsets to fetch.
        #[arg(long, value_delimiter = ',', default_value = "py,java,kt")]
        subset: Vec<String>,
        #[arg(long)]
        force: bool,
    },
    /// Convert one downloaded dataset to a where-to gold file
    /// (`external-<source>.json`, a JSON array in id order) and a counts
    /// report. Reads the files and nothing else. Held out: never use the
    /// output to tune or to choose a default.
    Convert {
        #[arg(long, value_enum)]
        source: Source,
        /// Directory holding the source's files (`test.csv` for
        /// swe-polybench-verified; `<subset>.jsonl` per subset, converted from
        /// the parquet files outside the repository, for lca-bugloc).
        #[arg(long = "in")]
        input: PathBuf,
        /// Gold directory (outside the repository) that receives the files.
        #[arg(long)]
        out: PathBuf,
        /// A repository (`owner/name`) of the owner's local gold trees;
        /// matching items get `external.overlap: true`. Repeatable.
        #[arg(long = "known-repo", value_name = "OWNER/NAME")]
        known_repo: Vec<String>,
        /// Revision of the dataset the files came from (default: the one in
        /// `fetch.json` of the input directory), recorded in every item and
        /// in the report.
        #[arg(long)]
        revision: Option<String>,
        /// Report JSON path (default: `<out>/external-<source>.report.json`).
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

pub fn run(command: ExternalCommand) -> Result<()> {
    match command {
        ExternalCommand::Fetch {
            source,
            out,
            subset,
            force,
        } => {
            let summary = fetch(&HttpFetcher::new()?, source, &out, &subset, force)?;
            println!("{summary}");
            Ok(())
        }
        ExternalCommand::Convert {
            source,
            input,
            out,
            known_repo,
            revision,
            report,
        } => run_convert(
            source,
            &input,
            &out,
            &known_repo,
            revision.as_deref(),
            report.as_deref(),
        ),
    }
}

// ------------------------------------------------------------------- fetch

/// What `fetch` needs from the network, so tests run without a socket.
pub(crate) trait Fetcher {
    /// The commit `main` points at for a dataset.
    fn revision(&self, dataset: &str) -> Result<String>;
    fn get(&self, url: &str) -> Result<Vec<u8>>;
}

struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .user_agent("lci-xtask-gold-external")
            .build()
            .context("failed to build the HTTP client")?;
        Ok(Self { client })
    }

    /// One GET on a fresh runtime on its own thread: `run` is called from
    /// inside the xtask runtime, where `block_on` would panic.
    fn download(&self, url: &str) -> Result<Vec<u8>> {
        std::thread::scope(|scope| {
            scope
                .spawn(|| -> Result<Vec<u8>> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(async {
                        let response = self
                            .client
                            .get(url)
                            .send()
                            .await
                            .with_context(|| format!("GET {url} failed"))?;
                        let status = response.status();
                        if !status.is_success() {
                            bail!("GET {url} answered {status}");
                        }
                        Ok(response.bytes().await?.to_vec())
                    })
                })
                .join()
                .map_err(|_| anyhow::anyhow!("the download thread panicked"))?
        })
    }
}

impl Fetcher for HttpFetcher {
    fn revision(&self, dataset: &str) -> Result<String> {
        let url = format!("https://huggingface.co/api/datasets/{dataset}/revision/main");
        let value: Value =
            serde_json::from_slice(&self.download(&url)?).context("revision answer is not JSON")?;
        value["sha"]
            .as_str()
            .map(str::to_string)
            .context("revision answer has no sha")
    }

    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.download(url)
    }
}

/// Download the source's files at one pinned revision into `<out>/<source>/`.
/// Everything is downloaded and compared with what is on disk before anything
/// is written, so a refusal leaves the directory as it was. Returns the
/// summary line.
pub(crate) fn fetch(
    fetcher: &dyn Fetcher,
    source: Source,
    out: &Path,
    subsets: &[String],
    force: bool,
) -> Result<String> {
    if source == Source::LcaBugloc {
        for subset in subsets {
            if !LCA_SUBSETS.contains(&subset.as_str()) {
                bail!("unknown lca-bugloc subset {subset:?} (py, java, kt)");
            }
        }
    }
    let dir = out.join(source.name());
    let revision = fetcher.revision(source.dataset())?;
    let record_path = dir.join("fetch.json");
    let mut files: Map<String, Value> = read_fetch_record(&dir)
        .and_then(|v| v["files"].as_object().cloned())
        .unwrap_or_default();
    let mut total = 0usize;
    let mut downloaded = Vec::new();
    for relative in source.fetch_files(subsets) {
        let url = format!(
            "https://huggingface.co/datasets/{}/resolve/{revision}/{relative}",
            source.dataset()
        );
        let bytes = fetcher.get(&url)?;
        let digest = hex(&Sha256::digest(&bytes));
        let target = dir.join(&relative);
        if let Ok(existing) = std::fs::read(&target)
            && hex(&Sha256::digest(&existing)) != digest
            && !force
        {
            bail!(
                "{} already exists with different content; pass --force to overwrite",
                target.display()
            );
        }
        total += bytes.len();
        files.insert(
            relative.clone(),
            json!({"url": url, "bytes": bytes.len(), "sha256": digest}),
        );
        downloaded.push((target, bytes));
    }
    for (target, bytes) in &downloaded {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, bytes)
            .with_context(|| format!("failed to write {}", target.display()))?;
    }
    let record = json!({
        "source": source.name(),
        "dataset": source.dataset(),
        "licence": source.licence(),
        "url": source.url(),
        "revision": revision,
        "files": files,
    });
    write_file(
        &record_path,
        &(serde_json::to_string_pretty(&record)? + "\n"),
    )?;
    Ok(format!(
        "fetched {}: {} files, {total} bytes, revision {revision}",
        source.name(),
        downloaded.len(),
    ))
}

/// The record `fetch` wrote, when the directory has one.
fn read_fetch_record(dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(dir.join("fetch.json")).ok()?;
    serde_json::from_str(&text).ok()
}

// ----------------------------------------------------------------- convert

fn run_convert(
    source: Source,
    input: &Path,
    out: &Path,
    known_repos: &[String],
    revision: Option<&str>,
    report_path: Option<&Path>,
) -> Result<()> {
    for repo in known_repos {
        if repo.split('/').filter(|p| !p.is_empty()).count() != 2 {
            bail!("--known-repo {repo:?} must be owner/name");
        }
    }
    let record = read_fetch_record(input);
    let revision = revision
        .map(str::to_string)
        .or_else(|| record.as_ref()?["revision"].as_str().map(str::to_string));
    let (rows, provenance) = match source {
        Source::SwePolybenchVerified => {
            let file = input.join("test.csv");
            let bytes = std::fs::read(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let sha256 = hex(&Sha256::digest(&bytes));
            let files = BTreeMap::from([("test.csv".to_string(), sha256.clone())]);
            (
                read_polybench_csv(&bytes)?,
                Provenance {
                    revision,
                    sha256,
                    files,
                },
            )
        }
        Source::LcaBugloc => read_lca_dir(input, revision, record.as_ref())?,
    };
    let conversion = convert(source, &rows, &provenance, known_repos);
    let items_path = out.join(format!("external-{}.json", source.name()));
    write_file(
        &items_path,
        &(serde_json::to_string_pretty(&conversion.items)? + "\n"),
    )?;
    let report_path = report_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| out.join(format!("external-{}.report.json", source.name())));
    write_file(
        &report_path,
        &(serde_json::to_string_pretty(&conversion.report)? + "\n"),
    )?;
    println!("{}", headline(&conversion.report));
    Ok(())
}

/// The rows of every `<subset>.jsonl` in the directory, each tagged with the
/// hash of the parquet file it came from (the JSONL's own hash when the parquet
/// file is not there). When `fetch.json` lists a parquet file, the file on disk
/// must still match it. The report's `sha256` is the hash of the sorted
/// `<name> <sha256>` lines of the parquet files (of the JSONL files when no
/// parquet file is present).
fn read_lca_dir(
    input: &Path,
    revision: Option<String>,
    record: Option<&Value>,
) -> Result<(Vec<Row>, Provenance)> {
    let mut rows = Vec::new();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut converted: BTreeMap<String, String> = BTreeMap::new();
    for subset in LCA_SUBSETS {
        let jsonl = input.join(format!("{subset}.jsonl"));
        if !jsonl.is_file() {
            continue;
        }
        let bytes =
            std::fs::read(&jsonl).with_context(|| format!("failed to read {}", jsonl.display()))?;
        let converted_sha = hex(&Sha256::digest(&bytes));
        let parquet_name = format!("{subset}/{LCA_TEST_FILE}");
        let source_sha = match std::fs::read(input.join(&parquet_name)) {
            Ok(parquet) => {
                let sha = hex(&Sha256::digest(&parquet));
                let listed = record.and_then(|r| r["files"][&parquet_name]["sha256"].as_str());
                if listed.is_some_and(|listed| listed != sha) {
                    bail!("{parquet_name} no longer matches fetch.json");
                }
                files.insert(parquet_name, sha.clone());
                sha
            }
            Err(_) => converted_sha.clone(),
        };
        converted.insert(format!("{subset}.jsonl"), converted_sha);
        for mut row in read_lca_jsonl(subset, &bytes)? {
            row.source_sha256 = Some(source_sha.clone());
            rows.push(row);
        }
    }
    if converted.is_empty() {
        bail!(
            "no <subset>.jsonl (py, java, kt) in {}; convert the parquet files first",
            input.display()
        );
    }
    let hashed = if files.is_empty() { &converted } else { &files };
    let mut digest = Sha256::new();
    for (name, sha) in hashed {
        digest.update(format!("{name} {sha}\n"));
    }
    files.extend(converted);
    Ok((
        rows,
        Provenance {
            revision,
            sha256: hex(&digest.finalize()),
            files,
        },
    ))
}

/// One LongCodeArena subset from its JSONL: one JSON object per line, the
/// parquet column names as keys, list columns as arrays.
pub(crate) fn read_lca_jsonl(subset: &str, bytes: &[u8]) -> Result<Vec<Row>> {
    let text = std::str::from_utf8(bytes).context("LCA JSONL is not UTF-8")?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("LCA {subset} JSONL line {}", index + 1))?;
        let Some(columns) = value.as_object() else {
            bail!("LCA {subset} JSONL line {} is not an object", index + 1);
        };
        rows.push(lca_row_from_columns(subset, index, columns)?);
    }
    Ok(rows)
}

// ------------------------------------------------------------------- rows

/// One dataset row, whatever the reader. The converter and its tests see only
/// this.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Row {
    /// 0-based position in the data file.
    pub row: usize,
    /// `polybench:<instance_id>` or `lca:<subset>:<id>`.
    pub id: String,
    /// `owner/name`.
    pub repo: String,
    pub base_commit: String,
    /// Lower-cased language name.
    pub language: String,
    /// Question pieces, joined by a blank line after dropping empty ones.
    pub question_parts: Vec<String>,
    /// The patch (PolyBench) or the diff (LCA).
    pub patch: Option<String>,
    /// The row's own list of changed files (LCA); when set it, not the patch,
    /// names the candidate files and the patch only finds the created ones.
    pub changed_files: Option<Vec<String>>,
    /// PolyBench `modified_nodes`: informational.
    pub nodes: Option<String>,
    /// sha256 of the dataset file the row came from, when it differs from the
    /// conversion's own (LCA: one parquet file per subset).
    pub source_sha256: Option<String>,
}

/// The SWE-PolyBench Verified file: a CSV with quoted multi-line cells.
/// Columns are read by name; a missing required column fails with its name.
pub(crate) fn read_polybench_csv(bytes: &[u8]) -> Result<Vec<Row>> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut reader = csv::ReaderBuilder::new().from_reader(bytes);
    let headers = reader
        .headers()
        .context("SWE-PolyBench CSV has no readable header")?
        .clone();
    let find = |name: &str| headers.iter().position(|h| h == name);
    let need = |name: &str| {
        find(name).with_context(|| {
            format!(
                "SWE-PolyBench CSV has no column {name:?} (header: {})",
                headers.iter().collect::<Vec<_>>().join(", ")
            )
        })
    };
    let (c_id, c_repo, c_base) = (need("instance_id")?, need("repo")?, need("base_commit")?);
    let (c_lang, c_patch, c_text) = (
        need("language")?,
        need("patch")?,
        need("problem_statement")?,
    );
    let c_nodes = find("modified_nodes");
    let mut rows = Vec::new();
    for (index, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("SWE-PolyBench CSV row {index}"))?;
        let cell = |c: usize| record.get(c).unwrap_or_default().to_string();
        let instance = cell(c_id);
        if instance.trim().is_empty() {
            bail!("SWE-PolyBench CSV row {index} has an empty instance_id");
        }
        rows.push(Row {
            row: index,
            id: format!("polybench:{instance}"),
            repo: cell(c_repo),
            base_commit: cell(c_base),
            language: cell(c_lang).to_lowercase(),
            question_parts: vec![cell(c_text)],
            patch: Some(cell(c_patch)),
            changed_files: None,
            nodes: c_nodes.map(cell),
            source_sha256: None,
        });
    }
    Ok(rows)
}

/// The LongCodeArena adapter: one row's cells by column name, as JSON values
/// (the K4b parquet reader turns each arrow cell into one). Required columns:
/// `repo_owner`, `repo_name`, `base_sha`, `issue_title`, `issue_body`,
/// `changed_files`, `diff`, `id`.
pub(crate) fn lca_row_from_columns(
    subset: &str,
    index: usize,
    columns: &Map<String, Value>,
) -> Result<Row> {
    let text = |name: &str| -> Result<String> {
        match columns.get(name) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(Value::Number(n)) => Ok(n.to_string()),
            Some(Value::Null) => Ok(String::new()),
            Some(_) => bail!("LCA row {index}: column {name:?} is not text"),
            None => bail!("LCA row {index}: no column {name:?}"),
        }
    };
    let files = match columns.get("changed_files") {
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .with_context(|| format!("LCA row {index}: a changed_files entry is not text"))
            })
            .collect::<Result<Vec<_>>>()?,
        Some(Value::String(s)) => serde_json::from_str::<Vec<String>>(s)
            .with_context(|| format!("LCA row {index}: changed_files is not a list"))?,
        _ => bail!("LCA row {index}: no list column \"changed_files\""),
    };
    let id = text("id")?;
    if id.trim().is_empty() {
        bail!("LCA row {index} has an empty id");
    }
    let language = match subset {
        "py" => "python",
        "java" => "java",
        "kt" => "kotlin",
        other => other,
    };
    Ok(Row {
        row: index,
        id: format!("lca:{subset}:{id}"),
        repo: format!("{}/{}", text("repo_owner")?, text("repo_name")?),
        base_commit: text("base_sha")?,
        language: language.to_string(),
        question_parts: vec![text("issue_title")?, text("issue_body")?],
        patch: Some(text("diff")?),
        changed_files: Some(files),
        nodes: None,
        source_sha256: None,
    })
}

// ------------------------------------------------------------------ patches

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilePatch {
    /// The path at the base commit (a renamed file keeps its old path).
    pub path: String,
    pub created: bool,
}

/// The files of a git-format patch, in order of first appearance: each
/// `diff --git a/<p> b/<p>` header, `new file mode` marking a created file and
/// `rename from` giving the base path. A header git quoted (special
/// characters) is kept raw so the portability check rejects it.
pub(crate) fn patch_paths(patch: &str) -> Vec<FilePatch> {
    let mut files: Vec<FilePatch> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            files.push(FilePatch {
                path: header_path(rest),
                created: false,
            });
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(FilePatch {
                path: rest.to_string(),
                created: false,
            });
        } else if let Some(file) = files.last_mut() {
            if line.starts_with("new file mode") {
                file.created = true;
            } else if let Some(from) = line.strip_prefix("rename from ") {
                file.path = from.to_string();
            }
        }
    }
    files
}

/// `rest` is `<a path> b/<b path>`. A path may hold spaces, so take the split
/// where both halves are equal; for a rename (halves differ) the first split.
fn header_path(rest: &str) -> String {
    let splits: Vec<usize> = rest.match_indices(" b/").map(|(i, _)| i).collect();
    for &at in &splits {
        if rest[..at] == rest[at + 3..] {
            return rest[..at].to_string();
        }
    }
    match splits.first() {
        Some(&at) => rest[..at].to_string(),
        None => rest.to_string(),
    }
}

/// A repository-relative, `/`-separated path: not empty, not absolute, no
/// drive letter, no backslash, no quote, no `.` or `..` or empty segment.
pub(crate) fn portable_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', '"', '\0'])
        && !path.chars().nth(1).is_some_and(|c| c == ':')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// The test-file rule: a directory segment `test`, `tests`, `__tests__`,
/// `spec` or `testing` (any case), or a file named `*.test.*`, `*.spec.*`,
/// `*_test.*`, `test_*.py`, `*Test.java` or `*Tests.java`.
pub(crate) fn is_test_path(path: &str) -> bool {
    let mut segments: Vec<&str> = path.split('/').collect();
    let name = segments.pop().unwrap_or_default();
    if segments.iter().any(|s| {
        matches!(
            s.to_ascii_lowercase().as_str(),
            "test" | "tests" | "__tests__" | "spec" | "testing"
        )
    }) {
        return true;
    }
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    name.contains(".test.")
        || name.contains(".spec.")
        || stem.ends_with("_test")
        || (name.starts_with("test_") && name.ends_with(".py"))
        || name.ends_with("Test.java")
        || name.ends_with("Tests.java")
}

fn has_adapter(path: &str) -> bool {
    lci_core::language::for_path(Path::new(path)).is_some()
}

// -------------------------------------------------------------------- leak

fn split_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for piece in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut current = String::new();
        let mut previous_lower = false;
        for c in piece.chars() {
            if c.is_ascii_uppercase() && previous_lower && !current.is_empty() {
                out.push(std::mem::take(&mut current).to_ascii_lowercase());
            }
            previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
            current.push(c);
        }
        if !current.is_empty() {
            out.push(current.to_ascii_lowercase());
        }
    }
    out
}

/// Whether the question holds the member's stem under any spelling of its
/// tokens. A one-token stem is the strata rule (`holds_stem`: generic and short
/// stems never). A stem of several tokens (`user_service`, `UserService`) is
/// held when a question word has the same letters and digits once separators
/// are dropped (`UserService`, `user_service`) or the question holds its tokens
/// next to each other (`user service`); a stem whose tokens are all generic
/// never counts.
fn holds_stem_tokens(words: &[String], question_tokens: &[String], path: &str) -> bool {
    if holds_stem(words, path) {
        return true;
    }
    let tokens = split_tokens(file_stem(path));
    if tokens.len() < 2 || tokens.iter().all(|t| is_generic_token(t)) {
        return false;
    }
    let collapsed: String = tokens.concat();
    if collapsed.len() < 3 || GENERIC_STEMS.contains(&collapsed.as_str()) {
        return false;
    }
    words.iter().any(|w| w.replace('_', "") == collapsed)
        || question_tokens
            .windows(tokens.len())
            .any(|w| w == tokens.as_slice())
}

fn is_generic_token(token: &str) -> bool {
    GENERIC_STEMS.contains(&token) || GENERIC_DIR_TOKENS.contains(&token)
}

/// Whether the question gives a member away: it holds the member's stem (as a
/// whole word, or under another spelling of its tokens: `holds_stem_tokens`),
/// holds the member's path verbatim, or holds a lower-cased token of 4 or more
/// characters from the member's directories (split on `/`, `_`, `-`, `.` and
/// camelCase; the generic directory tokens, the repository name and the
/// repository owner never). Question words split on every non-alphanumeric
/// character for the directory tokens (so `file_utils` gives `file` and
/// `utils`).
pub(crate) fn leak_of(question: &str, members: &[String], repo: &str) -> bool {
    let question_tokens = split_tokens(question);
    let words: Vec<String> = question
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let parts: BTreeSet<String> = split_tokens(question)
        .into_iter()
        .chain(
            question
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_ascii_lowercase),
        )
        .collect();
    let (owner, repo_name) = repo.rsplit_once('/').unwrap_or(("", repo));
    let mut ignored: BTreeSet<String> = BTreeSet::new();
    for name in [owner, repo_name] {
        ignored.extend(split_tokens(name));
        ignored.insert(name.to_ascii_lowercase());
    }
    members.iter().any(|path| {
        if holds_stem_tokens(&words, &question_tokens, path) || question.contains(path.as_str()) {
            return true;
        }
        let dirs = path.rsplit_once('/').map_or("", |(dirs, _)| dirs);
        split_tokens(dirs).into_iter().any(|token| {
            token.len() >= MIN_DIR_TOKEN
                && !GENERIC_DIR_TOKENS.contains(&token.as_str())
                && !GENERIC_STEMS.contains(&token.as_str())
                && !ignored.contains(&token)
                && parts.contains(&token)
        })
    })
}

// --------------------------------------------------------------- converter

/// What the items and the report record about where the file came from.
pub(crate) struct Provenance {
    pub revision: Option<String>,
    pub sha256: String,
    /// File name to sha256, for the report.
    pub files: BTreeMap<String, String>,
}

pub(crate) struct Conversion {
    /// In `id` order.
    pub items: Vec<Value>,
    pub report: Value,
}

#[derive(Default)]
struct Excluded {
    test: usize,
    created: usize,
    unsupported_extension: usize,
}

impl Excluded {
    fn to_json(&self) -> Value {
        json!({
            "test": self.test,
            "created": self.created,
            "unsupported_extension": self.unsupported_extension,
        })
    }
}

enum Outcome {
    Kept(Box<Value>, usize, bool),
    NoQuestion,
    Portability,
    NoMember,
}

/// The question: the parts without empty ones, joined by a blank line, line
/// endings as LF, cut at `QUESTION_MAX_CHARS` characters. Returns the text,
/// the character count before the cut and whether it was cut.
fn question_of(parts: &[String]) -> (String, usize, bool) {
    let joined = parts
        .iter()
        .map(|p| p.replace("\r\n", "\n").replace('\r', "\n"))
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let chars = joined.chars().count();
    match joined.char_indices().nth(QUESTION_MAX_CHARS) {
        Some((cut, _)) => (joined[..cut].to_string(), chars, true),
        None => (joined, chars, false),
    }
}

/// `modified_nodes` as a list of node paths. The Verified file's exact shape
/// is read at K4b: a JSON list, a Python-style list literal or one string are
/// all accepted. Informational only: no scorer reads it.
fn symbols_of(nodes: &str) -> Vec<String> {
    let nodes = nodes.trim();
    if nodes.is_empty() {
        return Vec::new();
    }
    let list: Vec<String> = match serde_json::from_str::<Vec<String>>(nodes) {
        Ok(list) => list,
        Err(_) => match nodes.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
            Some(inner) => inner
                .split(',')
                .map(|s| s.trim().trim_matches(['\'', '"']).to_string())
                .collect(),
            None => vec![nodes.to_string()],
        },
    };
    let mut seen = BTreeSet::new();
    list.into_iter()
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

fn convert_row(
    source: Source,
    row: &Row,
    provenance: &Provenance,
    overlap: bool,
    excluded: &mut Excluded,
) -> Outcome {
    let (question, chars, truncated) = question_of(&row.question_parts);
    let patch = row.patch.as_deref().map(patch_paths).unwrap_or_default();
    let created: BTreeSet<&str> = patch
        .iter()
        .filter(|f| f.created)
        .map(|f| f.path.as_str())
        .collect();
    let candidates: Vec<&str> = match &row.changed_files {
        Some(list) => list.iter().map(String::as_str).collect(),
        None => patch.iter().map(|f| f.path.as_str()).collect(),
    };
    if candidates.iter().any(|p| !portable_path(p)) {
        return Outcome::Portability;
    }
    if question.trim().is_empty() {
        return Outcome::NoQuestion;
    }
    let mut members: Vec<String> = Vec::new();
    for path in candidates {
        if created.contains(path) {
            excluded.created += 1;
        } else if is_test_path(path) {
            excluded.test += 1;
        } else if !has_adapter(path) {
            excluded.unsupported_extension += 1;
        } else if !members.iter().any(|m| m == path) {
            members.push(path.to_string());
        }
    }
    if members.is_empty() {
        return Outcome::NoMember;
    }
    let leak = leak_of(&question, &members, &row.repo);
    let symbols = row.nodes.as_deref().map(symbols_of).unwrap_or_default();
    let item = json!({
        "id": row.id,
        "set": "where-to",
        "question": question,
        "gold": {
            "form": "exemplar",
            "granularity": "file",
            "members": members.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
            "symbols": symbols,
        },
        "accept": {},
        "stratum": if leak { "external/leak" } else { "external/clean" },
        "external": {
            "dataset": source.dataset(),
            "licence": source.licence(),
            "url": source.url(),
            "revision": provenance.revision,
            "sha256": row.source_sha256.as_deref().unwrap_or(&provenance.sha256),
            "row": row.row,
            "language": row.language,
            "repo": row.repo,
            "base_commit": row.base_commit,
            "question_chars": chars,
            "question_truncated": truncated,
            "excluded_files": excluded.to_json(),
            "overlap": overlap,
            "heldout": true,
        },
    });
    Outcome::Kept(Box::new(item), members.len(), leak)
}

/// Convert the rows of one source. Rows are taken in `id` order. A row is
/// dropped, and counted in one class only, when a file path is not portable,
/// when its question is empty, or when no member is left.
pub(crate) fn convert(
    source: Source,
    rows: &[Row],
    provenance: &Provenance,
    known_repos: &[String],
) -> Conversion {
    let known: BTreeSet<String> = known_repos.iter().map(|r| r.to_ascii_lowercase()).collect();
    let mut ordered: Vec<&Row> = rows.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));
    let mut items = Vec::new();
    let mut excluded = Excluded::default();
    let (mut no_member, mut portability, mut no_question) = (0usize, 0usize, 0usize);
    let (mut leak, mut overlap_items, mut truncated) = (0usize, 0usize, 0usize);
    let mut per_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_repo: BTreeMap<String, usize> = BTreeMap::new();
    let mut overlap_repos: BTreeMap<String, usize> = BTreeMap::new();
    let mut member_counts: Vec<usize> = Vec::new();
    for row in ordered {
        let overlap = known.contains(&row.repo.to_ascii_lowercase());
        let mut row_excluded = Excluded::default();
        match convert_row(source, row, provenance, overlap, &mut row_excluded) {
            Outcome::Kept(item, members, is_leak) => {
                excluded.test += row_excluded.test;
                excluded.created += row_excluded.created;
                excluded.unsupported_extension += row_excluded.unsupported_extension;
                leak += usize::from(is_leak);
                if overlap {
                    overlap_items += 1;
                    *overlap_repos.entry(row.repo.clone()).or_default() += 1;
                }
                truncated += usize::from(item["external"]["question_truncated"] == json!(true));
                *per_language.entry(row.language.clone()).or_default() += 1;
                *per_repo.entry(row.repo.clone()).or_default() += 1;
                member_counts.push(members);
                items.push(*item);
            }
            Outcome::NoMember => {
                excluded.test += row_excluded.test;
                excluded.created += row_excluded.created;
                excluded.unsupported_extension += row_excluded.unsupported_extension;
                no_member += 1;
            }
            Outcome::Portability => portability += 1,
            Outcome::NoQuestion => no_question += 1,
        }
    }
    member_counts.sort_unstable();
    let quantile = |p: f64| -> Value {
        if member_counts.is_empty() {
            return Value::Null;
        }
        let rank = ((p * member_counts.len() as f64).ceil() as usize).clamp(1, member_counts.len());
        json!(member_counts[rank - 1])
    };
    let report = json!({
        "source": source.name(),
        "dataset": source.dataset(),
        "licence": source.licence(),
        "url": source.url(),
        "revision": provenance.revision,
        "sha256": provenance.sha256,
        "files": provenance.files,
        "heldout": true,
        "rows": rows.len(),
        "items": items.len(),
        "dropped": {
            "no_member": no_member,
            "portability": portability,
            "no_question": no_question,
        },
        "per_language": per_language,
        "per_repo": per_repo,
        "leak": leak,
        "clean": items.len() - leak,
        "overlap": overlap_items,
        "overlap_repos": overlap_repos,
        "known_repos": known_repos,
        "truncated": truncated,
        "members_per_item": {
            "min": quantile(0.0),
            "p50": quantile(0.5),
            "p90": quantile(0.9),
            "max": quantile(1.0),
        },
        "excluded_files": excluded.to_json(),
        "note": "counts and hashes only; items are held-out gold, never used to tune or to choose a default",
    });
    Conversion { items, report }
}

/// One line for the terminal.
pub(crate) fn headline(report: &Value) -> String {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let d = &report["dropped"];
    format!(
        "external {}: {} rows, {} items (leak {}, clean {}, overlap {}, truncated {}); dropped: no_member {}, portability {}, no_question {}",
        report["source"].as_str().unwrap_or("?"),
        n(&report["rows"]),
        n(&report["items"]),
        n(&report["leak"]),
        n(&report["clean"]),
        n(&report["overlap"]),
        n(&report["truncated"]),
        n(&d["no_member"]),
        n(&d["portability"]),
        n(&d["no_question"]),
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
