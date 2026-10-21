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
//! * No data in the repository. The converter reads one dataset file given on
//!   the command line and writes the gold under a directory given on the
//!   command line; the owner's local paths live only in local configs and
//!   scripts. The report holds counts and hashes, never question or patch
//!   text.
//! * The converter reads only the row: no tree, no network, no clone.
//!
//! K4a (this file) holds the row type, the SWE-PolyBench CSV reader, the
//! LongCodeArena column adapter (the parquet reader that feeds it is K4b), the
//! patch parser, the test-file rule, the leak check and the converter. Fetching
//! the files is K4b.

use super::strata::{GENERIC_STEMS, holds_stem};
use super::write_file;
use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The question goes on a command line in the bench; Windows caps a command
/// line near 32k characters.
pub(crate) const QUESTION_MAX_CHARS: usize = 8000;

/// Directory tokens that say nothing when a question holds them.
const GENERIC_DIR_TOKENS: [&str; 6] = ["src", "lib", "main", "app", "core", "test"];

/// Shortest directory token that counts for the leak check.
const MIN_DIR_TOKEN: usize = 4;

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

    /// The file of the source that `convert` reads, inside `--in`.
    pub(crate) fn data_file(self) -> &'static str {
        match self {
            Source::SwePolybenchVerified => "test.csv",
            Source::LcaBugloc => "test-00000-of-00001.parquet",
        }
    }
}

#[derive(Subcommand)]
pub enum ExternalCommand {
    /// Convert one downloaded dataset file to a where-to gold file
    /// (`external-<source>.json`, a JSON array in id order) and a counts
    /// report. Reads the file and nothing else. Held out: never use the
    /// output to tune or to choose a default.
    Convert {
        #[arg(long, value_enum)]
        source: Source,
        /// Directory holding the source's data file (`test.csv` for
        /// swe-polybench-verified).
        #[arg(long = "in")]
        input: PathBuf,
        /// Gold directory (outside the repository) that receives the files.
        #[arg(long)]
        out: PathBuf,
        /// A repository (`owner/name`) of the owner's local gold trees;
        /// matching items get `external.overlap: true`. Repeatable.
        #[arg(long = "known-repo", value_name = "OWNER/NAME")]
        known_repo: Vec<String>,
        /// Revision of the dataset the file came from, recorded in every
        /// item and in the report.
        #[arg(long)]
        revision: Option<String>,
        /// Report JSON path (default: `<out>/external-<source>.report.json`).
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

pub fn run(command: ExternalCommand) -> Result<()> {
    match command {
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
    if source == Source::LcaBugloc {
        bail!(
            "lca-bugloc needs the parquet reader, which is not built yet (K4b); \
             `lca_row_from_columns` is its adapter"
        );
    }
    let file = input.join(source.data_file());
    let bytes =
        std::fs::read(&file).with_context(|| format!("failed to read {}", file.display()))?;
    let rows = read_polybench_csv(&bytes)?;
    let provenance = Provenance {
        revision: revision.map(str::to_string),
        sha256: hex(&Sha256::digest(&bytes)),
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
        });
    }
    Ok(rows)
}

/// The LongCodeArena adapter: one row's cells by column name, as JSON values
/// (the K4b parquet reader turns each arrow cell into one). Required columns:
/// `repo_owner`, `repo_name`, `base_sha`, `issue_title`, `issue_body`,
/// `changed_files`, `diff`, `id`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the K4b parquet reader is its only caller")
)]
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

/// Whether the question gives a member away: it holds the member's stem as a
/// whole word (the strata rule, `holds_stem`: generic and short stems never),
/// holds the member's path verbatim, or holds a lower-cased token of 4 or more
/// characters from the member's directories (split on `/`, `_`, `-`, `.` and
/// camelCase; `src`, `lib`, `main`, `app`, `core`, `test` and the repository
/// name never). Question words split on every non-alphanumeric character
/// for the directory tokens (so `file_utils` gives `file` and `utils`).
pub(crate) fn leak_of(question: &str, members: &[String], repo: &str) -> bool {
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
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
    let mut ignored: BTreeSet<String> = split_tokens(repo_name).into_iter().collect();
    ignored.insert(repo_name.to_ascii_lowercase());
    members.iter().any(|path| {
        if holds_stem(&words, path) || question.contains(path.as_str()) {
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
            "sha256": provenance.sha256,
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
