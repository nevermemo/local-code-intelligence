//! `cargo xtask maps baseline serena`: the Serena baseline arm for the
//! `traits`, `where-to` and `impact` sets (K5b).
//!
//! Serena (a coding-agent toolkit whose symbol tools are backed by language
//! servers; GPL-3.0-or-later application, MIT SolidLSP) runs as a separate
//! process that this arm talks to over MCP Streamable HTTP on loopback, the way
//! the rg arm runs `rg`: nothing of it is linked, vendored or copied here. It
//! is a baseline instrument beside the seed arms, never a gain claim.
//!
//! Serena writes a `.serena/` directory into the project it activates, so the
//! arm never points it at the frozen tree: it copies the tree (without `.git`
//! directories or files, `node_modules` and cargo `target` directories) and
//! Serena serves the copy. The frozen tree's file-list hash is taken before and after the run and
//! recorded in `meta.json`; a tree that already holds a `.serena` directory is
//! refused.
//!
//! Needles are the rg arm's (`rg::needles_of`: the last segment of each
//! backticked name, at most four, else `lexical::terms`), from the question
//! only. Two rules, named in `meta.json["rule"]`:
//!
//! * `symbol` (default): `find_symbol` per needle (exact name, no body, depth
//!   0); `traits` adds the implementations of the first type-like symbol found
//!   (`find_implementations` for a trait, else the type/impl-kind entries of
//!   `find_referencing_symbols`); `where-to` falls back to `search_for_pattern`
//!   when no symbol matched; `impact` answers the declarations that reference
//!   the first symbol of the first needle.
//! * `pattern` (traits, where-to): `search_for_pattern` with the needles as an
//!   escaped whole-word alternation, one answer per matched line: the rg arm
//!   through Serena, so the two arms pair line for line and the difference
//!   measures Serena's symbol layer.
//!
//! Answers are `{path, line}` with workspace-relative `/` paths and 1-based
//! lines (Serena counts from 0), ordered and cut by the rg arm's own
//! `answers_for`. Only the tools in [`ALLOWED_TOOLS`] are ever called. Serena's
//! file-editing and shell tools stay unused even where the server offers them.

use super::rg::{self, NeedleSource, RgLine};
use super::{say, set_name, slug, write_arm_run};
use crate::acceptance::{ManagedServer, McpSession, mcp_envelopes};
use crate::gold::Set;
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

/// The environment variable that names the Serena executable (or the URL of a
/// running server) when `--serena` is not given.
pub const SERENA_ENV: &str = "LCI_BASELINE_SERENA";

/// The tools the server must offer; without them the arm refuses to run.
const REQUIRED_TOOLS: &[&str] = &[
    "find_symbol",
    "find_referencing_symbols",
    "search_for_pattern",
];

/// The only tools the arm ever calls. A `tools/call` of anything else is a bug.
const ALLOWED_TOOLS: &[&str] = &[
    "find_symbol",
    "find_referencing_symbols",
    "find_implementations",
    "search_for_pattern",
];

/// Symbol kinds (Serena's names for LSP kinds) whose implementations/referencing
/// types the `traits` set expands.
const TYPE_KINDS: &[&str] = &["Interface", "Class", "Struct", "Enum"];

/// Kinds of referencing symbols kept as "implementations" of a type.
const REF_KEEP: &[&str] = &["Object", "Class", "Struct", "Enum", "Interface"];

/// Written into a tree copy so that a later run of the same wave may reuse it.
const COPY_MARKER: &str = ".lci-baseline-copy";

/// Consecutive failed items after which the run stops: a dead server would
/// otherwise cost a call timeout per remaining item.
const MAX_FAILURE_STREAK: usize = 5;

#[derive(Clone, Copy, ValueEnum)]
pub enum SerenaSet {
    Traits,
    WhereTo,
    Impact,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SerenaRule {
    Symbol,
    Pattern,
}

impl SerenaRule {
    fn name(self) -> &'static str {
        match self {
            SerenaRule::Symbol => "symbol",
            SerenaRule::Pattern => "pattern",
        }
    }
}

#[derive(Args)]
pub struct SerenaArgs {
    /// The set to answer: `traits`, `where-to` or `impact`.
    #[arg(long, value_enum)]
    set: SerenaSet,
    /// Directory holding the set's gold file (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The frozen tree. Only read: Serena runs against a copy.
    #[arg(long)]
    tree: PathBuf,
    /// The Serena executable (the arm starts it on a loopback port against the
    /// tree copy and stops it at the end), or `http://127.0.0.1:<port>` of a
    /// server you started on the copy given by `--tree-copy`. Falls back to
    /// the environment variable LCI_BASELINE_SERENA.
    #[arg(long)]
    serena: Option<String>,
    /// Answers per item, the bench's `--top-k`; 0 keeps every answer.
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/<set>`.
    /// Default `baseline-serena`, or `baseline-serena-pattern` for the pattern
    /// rule.
    #[arg(long)]
    wave: Option<String>,
    /// Run only the first N items.
    #[arg(long)]
    limit: Option<usize>,
    /// The rule: `symbol` (default) or `pattern` (traits and where-to only).
    #[arg(long, value_enum, default_value_t = SerenaRule::Symbol)]
    rule: SerenaRule,
    /// The tree copy Serena serves: reused when it holds this arm's marker for
    /// this tree, created there when absent. Default `<out-dir>/<wave>/tree`
    /// (so use one wave per tree, or a `--tree-copy` per tree).
    /// Required with a URL in `--serena`.
    #[arg(long)]
    tree_copy: Option<PathBuf>,
    /// Longest to wait for a started Serena to answer `initialize`, seconds.
    #[arg(long, default_value_t = 600)]
    start_timeout_s: u64,
    /// Longest one MCP request may take, seconds; a slower call fails its item.
    #[arg(long, default_value_t = 300)]
    call_timeout_s: u64,
    /// `max_answer_chars` sent with every call (Serena shortens longer answers,
    /// which the arm then counts as an unknown shape).
    #[arg(long, default_value_t = 1_000_000)]
    max_answer_chars: i64,
    /// An extra argument for `serena start-mcp-server` (repeatable), appended
    /// after the arm's own so that it wins.
    #[arg(long = "serena-arg", allow_hyphen_values = true)]
    serena_args: Vec<String>,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

// ------------------------------------------------------- the MCP peer

/// What the arm needs of a server: its tool names and a tool call that
/// returns the result text. The real peer is [`HttpPeer`]; tests use a stub.
pub(super) trait Peer {
    async fn tool_names(&mut self) -> Result<Vec<String>>;
    async fn call(&mut self, name: &str, arguments: Value) -> Result<String>;
}

struct HttpPeer(McpSession);

impl Peer for HttpPeer {
    async fn tool_names(&mut self) -> Result<Vec<String>> {
        self.0.tools_list().await
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Result<String> {
        let body = self.0.call_tool(name, arguments).await?;
        result_text(&body)
    }
}

/// The text of a `tools/call` response body (event stream or plain JSON): the
/// `content` text items joined by newlines.
pub(super) fn result_text(body: &str) -> Result<String> {
    for envelope in mcp_envelopes(body) {
        if let Some(error) = envelope.get("error") {
            bail!("MCP error: {error}");
        }
        let Some(content) = envelope
            .pointer("/result/content")
            .and_then(Value::as_array)
        else {
            continue;
        };
        let texts: Vec<&str> = content.iter().filter_map(|c| c["text"].as_str()).collect();
        return Ok(texts.join("\n"));
    }
    bail!("no tool result in the MCP response: {body}");
}

/// A peer behind the allow-list, with the arm's call and shape counters.
struct Tools<P: Peer> {
    peer: P,
    offered: Vec<String>,
    max_answer_chars: i64,
    calls: u32,
    unknown: u32,
    used: BTreeMap<String, u32>,
}

impl<P: Peer> Tools<P> {
    fn offers(&self, name: &str) -> bool {
        self.offered.iter().any(|t| t == name)
    }

    /// Calls `name` with `arguments` plus the arm's `max_answer_chars`. A tool
    /// off the allow-list, or one the server does not offer, is an error and
    /// never reaches the peer.
    async fn call(&mut self, name: &str, mut arguments: Value) -> Result<String> {
        if !ALLOWED_TOOLS.contains(&name) {
            bail!("the tool {name} is not on the Serena arm's allow-list");
        }
        if !self.offers(name) {
            bail!("the server does not offer the tool {name}");
        }
        arguments["max_answer_chars"] = json!(self.max_answer_chars);
        self.calls += 1;
        *self.used.entry(name.to_owned()).or_default() += 1;
        self.peer.call(name, arguments).await
    }
}

// ------------------------------------------------- reading Serena's answers

/// One symbol location Serena reported. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Place {
    /// Relative to the project, `/` separators.
    pub path: String,
    /// The path as Serena wrote it (sent back in follow-up calls).
    pub raw_path: String,
    pub line: u64,
    pub kind: String,
    pub name_path: String,
}

/// Places read from one answer, and the entries that had no path or line (or an
/// answer that was not the expected shape at all).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Parsed {
    pub places: Vec<Place>,
    pub unknown: u32,
}

fn norm_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_owned();
    }
    path
}

/// A symbol entry: `name_path`, `body_location.start_line` (0-based) and the
/// file, which the caller supplies when the entry does not carry it.
fn place_of(entry: &Value, file: Option<&str>, kind: Option<&str>) -> Option<Place> {
    let raw = file.or_else(|| entry["relative_path"].as_str())?;
    let start = entry.pointer("/body_location/start_line")?.as_u64()?;
    Some(Place {
        path: norm_path(raw),
        raw_path: raw.to_owned(),
        line: start + 1,
        kind: kind
            .or_else(|| entry["kind"].as_str())
            .unwrap_or("")
            .to_owned(),
        name_path: entry["name_path"].as_str().unwrap_or("").to_owned(),
    })
}

/// `find_symbol` / `find_implementations`: a JSON array of symbol entries.
pub(super) fn parse_symbols(text: &str) -> Parsed {
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(text) else {
        return Parsed {
            places: Vec::new(),
            unknown: 1,
        };
    };
    let mut parsed = Parsed::default();
    for entry in &entries {
        match place_of(entry, None, None) {
            Some(place) => parsed.places.push(place),
            None => parsed.unknown += 1,
        }
    }
    parsed
}

/// `find_referencing_symbols`: `{file: {kind: [entry, ..]}}`. Only the kinds in
/// `keep` are read (an empty list keeps all).
pub(super) fn parse_references(text: &str, keep: &[&str]) -> Parsed {
    let Ok(Value::Object(files)) = serde_json::from_str::<Value>(text) else {
        return Parsed {
            places: Vec::new(),
            unknown: 1,
        };
    };
    let mut parsed = Parsed::default();
    for (file, kinds) in &files {
        let Some(kinds) = kinds.as_object() else {
            parsed.unknown += 1;
            continue;
        };
        for (kind, entries) in kinds {
            if !keep.is_empty() && !keep.contains(&kind.as_str()) {
                continue;
            }
            for entry in entries.as_array().into_iter().flatten() {
                match place_of(entry, Some(file.as_str()), Some(kind.as_str())) {
                    Some(place) => parsed.places.push(place),
                    None => parsed.unknown += 1,
                }
            }
        }
    }
    parsed
}

/// One line `search_for_pattern` matched: 1-based line and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PatternLine {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// `search_for_pattern`: `{file: ["  >  15:text", ..]}` with 0-based line
/// numbers; the `>` marks a match, `...` a context line (skipped).
pub(super) fn parse_pattern(text: &str) -> (Vec<PatternLine>, u32) {
    let Ok(Value::Object(files)) = serde_json::from_str::<Value>(text) else {
        return (Vec::new(), 1);
    };
    let (mut lines, mut unknown) = (Vec::new(), 0);
    for (file, entries) in &files {
        let Some(entries) = entries.as_array() else {
            unknown += 1;
            continue;
        };
        for entry in entries {
            let Some(raw) = entry.as_str() else {
                unknown += 1;
                continue;
            };
            let raw = raw.trim_start();
            if raw.starts_with("...") {
                continue;
            }
            let parsed = raw.strip_prefix('>').and_then(|rest| {
                let rest = rest.trim_start();
                let (number, text) = rest.split_once(':')?;
                Some((number.trim().parse::<u64>().ok()?, text))
            });
            match parsed {
                Some((number, text)) => lines.push(PatternLine {
                    path: norm_path(file),
                    line: number + 1,
                    text: text.to_owned(),
                }),
                None => unknown += 1,
            }
        }
    }
    (lines, unknown)
}

// ---------------------------------------------------------------- the rules

/// `RgLine`s from places: a place's needle count is the distinct needles that
/// queried it plus those that occur as a word in its name path.
fn symbol_lines(needles: &[String], hits: &[(usize, Place)]) -> Vec<RgLine> {
    let mut by_place: BTreeMap<(String, u64), BTreeSet<usize>> = BTreeMap::new();
    for (index, place) in hits {
        let set = by_place
            .entry((place.path.clone(), place.line))
            .or_default();
        set.insert(*index);
        for (other, needle) in needles.iter().enumerate() {
            if rg::has_word(&place.name_path, needle) {
                set.insert(other);
            }
        }
    }
    by_place
        .into_iter()
        .map(|((path, line), set)| RgLine {
            path,
            line,
            needles: set.len().max(1) as u32,
            decl: false,
        })
        .collect()
}

/// `RgLine`s from pattern lines, with the rg arm's distinct-needle count read
/// from the line text (lowercase compare unless `case_sensitive`).
fn pattern_rg_lines(
    lines: &[PatternLine],
    needles: &[String],
    case_sensitive: bool,
) -> Vec<RgLine> {
    let key = |s: &str| {
        if case_sensitive {
            s.to_owned()
        } else {
            s.to_lowercase()
        }
    };
    let wanted: Vec<String> = needles.iter().map(|n| key(n)).collect();
    lines
        .iter()
        .map(|l| {
            let text = key(&l.text);
            let held = wanted.iter().filter(|n| rg::has_word(&text, n)).count();
            RgLine {
                path: l.path.clone(),
                line: l.line,
                needles: held.max(1) as u32,
                decl: false,
            }
        })
        .collect()
}

/// The regex of the pattern rule: the needles escaped, as one whole-word
/// alternation (`(?i)` first when the match is case-insensitive).
pub(super) fn pattern_regex(needles: &[String], case_sensitive: bool) -> String {
    let alternatives: Vec<String> = needles.iter().map(|n| rg::regex_escape(n)).collect();
    format!(
        "{}\\b(?:{})\\b",
        if case_sensitive { "" } else { "(?i)" },
        alternatives.join("|")
    )
}

/// The file name matches one of the language adapters' globs (`*.ext`), the
/// files the rg arm searches.
fn in_adapter_globs(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    rg::globs().iter().any(|g| {
        g.strip_prefix('*')
            .is_some_and(|suffix| name.ends_with(suffix))
    })
}

async fn pattern_lines<P: Peer>(
    tools: &mut Tools<P>,
    needles: &[String],
    case_sensitive: bool,
) -> Result<Vec<RgLine>> {
    let text = tools
        .call(
            "search_for_pattern",
            json!({
                "substring_pattern": pattern_regex(needles, case_sensitive),
                "multiline": false,
                "restrict_search_to_code_files": false,
                "context_lines_before": 0,
                "context_lines_after": 0,
            }),
        )
        .await?;
    let (mut lines, unknown) = parse_pattern(&text);
    tools.unknown += unknown;
    // Serena has no multi-extension glob (`{*.rs,*.md}` matches nothing, `*.rs`
    // only the root): the whole tree is searched and the files are kept by the
    // rg arm's globs, so the two arms see the same files.
    lines.retain(|l| in_adapter_globs(&l.path));
    Ok(pattern_rg_lines(&lines, needles, case_sensitive))
}

async fn find_symbols<P: Peer>(
    tools: &mut Tools<P>,
    needles: &[String],
) -> Result<Vec<(usize, Place)>> {
    let mut hits = Vec::new();
    for (index, needle) in needles.iter().enumerate() {
        let text = tools
            .call(
                "find_symbol",
                json!({
                    "name_path_pattern": needle,
                    "depth": 0,
                    "include_body": false,
                    "substring_matching": false,
                }),
            )
            .await?;
        let parsed = parse_symbols(&text);
        tools.unknown += parsed.unknown;
        hits.extend(parsed.places.into_iter().map(|p| (index, p)));
    }
    Ok(hits)
}

/// The implementations of a type-like symbol: `find_implementations` for a
/// trait/interface when the server offers it, else the type/impl-kind entries
/// of `find_referencing_symbols`.
async fn expand_type<P: Peer>(tools: &mut Tools<P>, target: &Place) -> Result<Vec<Place>> {
    let arguments = json!({ "name_path": target.name_path, "relative_path": target.raw_path });
    if target.kind == "Interface" && tools.offers("find_implementations") {
        let text = tools.call("find_implementations", arguments).await?;
        let parsed = parse_symbols(&text);
        tools.unknown += parsed.unknown;
        return Ok(parsed.places);
    }
    let text = tools.call("find_referencing_symbols", arguments).await?;
    let parsed = parse_references(&text, REF_KEEP);
    tools.unknown += parsed.unknown;
    Ok(parsed.places)
}

/// What the arm made of one question.
struct ItemAnswer {
    answers: Vec<Value>,
    source: NeedleSource,
    /// where-to: no symbol matched, `search_for_pattern` answered instead.
    pattern_fallback: bool,
    /// A follow-up call (implementations, references) failed; its places are missing.
    expansion_failure: Option<String>,
}

async fn answer_item<P: Peer>(
    tools: &mut Tools<P>,
    set: Set,
    rule: SerenaRule,
    question: &str,
    top_k: usize,
) -> Result<ItemAnswer> {
    let (needles, source) = rg::needles_of(question);
    let mut item = ItemAnswer {
        answers: Vec::new(),
        source,
        pattern_fallback: false,
        expansion_failure: None,
    };
    if source == NeedleSource::None {
        return Ok(item);
    }
    let case_sensitive = source == NeedleSource::Backticked;
    let lines: Vec<RgLine> = match (rule, set) {
        (SerenaRule::Pattern, _) => pattern_lines(tools, &needles, case_sensitive).await?,
        (SerenaRule::Symbol, Set::Impact) => {
            let hits = find_symbols(tools, &needles[..1]).await?;
            let mut places: Vec<Place> = hits.into_iter().map(|(_, p)| p).collect();
            places.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
            let mut found = Vec::new();
            if let Some(target) = places.first() {
                let arguments =
                    json!({ "name_path": target.name_path, "relative_path": target.raw_path });
                match tools.call("find_referencing_symbols", arguments).await {
                    Ok(text) => {
                        let parsed = parse_references(&text, &[]);
                        tools.unknown += parsed.unknown;
                        found = parsed.places;
                    }
                    Err(e) => item.expansion_failure = Some(format!("{e:#}")),
                }
            }
            symbol_lines(
                &needles[..1],
                &found.into_iter().map(|p| (0, p)).collect::<Vec<_>>(),
            )
        }
        (SerenaRule::Symbol, _) => {
            let mut hits = find_symbols(tools, &needles).await?;
            if set == Set::Traits {
                let target = hits
                    .iter()
                    .find(|(_, p)| TYPE_KINDS.contains(&p.kind.as_str()))
                    .cloned();
                if let Some((index, target)) = target {
                    match expand_type(tools, &target).await {
                        Ok(places) => hits.extend(places.into_iter().map(|p| (index, p))),
                        Err(e) => item.expansion_failure = Some(format!("{e:#}")),
                    }
                }
            }
            let mut lines = symbol_lines(&needles, &hits);
            if lines.is_empty() && set == Set::WhereTo {
                item.pattern_fallback = true;
                lines = pattern_lines(tools, &needles, case_sensitive).await?;
            }
            lines
        }
    };
    item.answers = rg::answers_for(set, &lines, top_k, false);
    Ok(item)
}

// ----------------------------------------------------- the tree and its copy

/// The directories left out of a tree listing, hash and copy: `.git` and
/// `node_modules` anywhere, a cargo `target` next to a `Cargo.toml`.
fn skip_dir(parent: &Path, name: &str) -> bool {
    match name {
        ".git" | "node_modules" => true,
        "target" => parent.join("Cargo.toml").is_file(),
        _ => false,
    }
}

/// One file of a tree listing: path relative to the root with `/`, size,
/// modification time in nanoseconds since the epoch.
type Entry = (String, u64, u128);

/// Every regular file under `root` outside the skipped directories, sorted by
/// path, and the number of symbolic links not followed.
fn list_tree(root: &Path) -> Result<(Vec<Entry>, usize)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>, links: &mut usize) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
            let kind = entry
                .file_type()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if kind.is_symlink() {
                *links += 1;
            } else if kind.is_dir() {
                if !skip_dir(dir, &name) {
                    walk(root, &path, out, links)?;
                }
            } else if kind.is_file() && name != ".git" {
                let meta = entry
                    .metadata()
                    .with_context(|| format!("stat {}", path.display()))?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, meta.len(), mtime));
            }
        }
        Ok(())
    }
    let (mut files, mut links) = (Vec::new(), 0);
    walk(root, root, &mut files, &mut links)?;
    files.sort();
    Ok((files, links))
}

/// The hash of a file listing (path, size and modification time of every file)
/// and the number of files: "unchanged by hash" for a tree that is not read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeHash {
    sha256: String,
    files: usize,
}

fn hash_entries(files: &[Entry]) -> TreeHash {
    let mut hasher = Sha256::new();
    for (path, size, mtime) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(size.to_le_bytes());
        hasher.update(mtime.to_le_bytes());
    }
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    TreeHash {
        sha256,
        files: files.len(),
    }
}

fn tree_hash(root: &Path) -> Result<TreeHash> {
    Ok(hash_entries(&list_tree(root)?.0))
}

/// Refuses a tree that already holds a `.serena` directory (Serena was run in
/// place on it).
fn check_tree(tree: &Path) -> Result<()> {
    if !tree.is_dir() {
        bail!("--tree {} is not a directory", tree.display());
    }
    if tree.join(".serena").exists() {
        bail!(
            "--tree {} already holds a .serena directory: Serena was run in place on it; \
             use a clean checkout (the arm runs Serena on a copy)",
            tree.display()
        );
    }
    Ok(())
}

/// The tree copy Serena serves.
#[derive(Debug)]
struct CopyInfo {
    dir: PathBuf,
    /// Made by this run (false: an earlier run of the arm made it).
    created: bool,
    files: usize,
    links_skipped: usize,
    /// The copy's hash when this run started.
    hash: TreeHash,
}

/// Makes (or reuses) the copy of `tree` at `dir`. A `dir` that exists is reused
/// only when its marker names this tree's hash; nothing is ever deleted.
fn prepare_copy(tree: &Path, dir: &Path, source: &TreeHash) -> Result<CopyInfo> {
    let tree_abs = std::fs::canonicalize(tree)?;
    std::fs::create_dir_all(dir)?;
    let dir_abs = std::fs::canonicalize(dir)?;
    if dir_abs.starts_with(&tree_abs) || tree_abs.starts_with(&dir_abs) {
        bail!("the tree copy and --tree may not contain one another");
    }
    let marker = dir.join(COPY_MARKER);
    let occupied = std::fs::read_dir(dir)?.next().is_some();
    if occupied {
        let text = std::fs::read_to_string(&marker).ok();
        let recorded = text
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["source_sha256"].as_str().map(str::to_owned));
        if recorded.as_deref() != Some(source.sha256.as_str()) {
            bail!(
                "{} exists and is not a copy of this tree made by the Serena arm; \
                 pass another --tree-copy or remove it yourself",
                dir.display()
            );
        }
        let (files, links) = list_tree(dir)?;
        return Ok(CopyInfo {
            dir: dir.to_owned(),
            created: false,
            files: files.len(),
            links_skipped: links,
            hash: hash_entries(&files),
        });
    }
    let (files, links) = list_tree(tree)?;
    for (rel, _, _) in &files {
        let to = dir.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(tree.join(rel), &to).with_context(|| format!("copy {rel}"))?;
    }
    std::fs::write(
        &marker,
        serde_json::to_string_pretty(&json!({
            "source_sha256": source.sha256,
            "source_files": source.files,
        }))?,
    )?;
    let (copied, _) = list_tree(dir)?;
    Ok(CopyInfo {
        dir: dir.to_owned(),
        created: true,
        files: files.len(),
        links_skipped: links,
        hash: hash_entries(&copied),
    })
}

// ----------------------------------------------------------- the transport

/// Where the server is: one the arm starts, or one the owner started.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Launch(PathBuf),
    Attach(String),
}

fn port_is_reserved(port: u16) -> bool {
    (8765..=8769).contains(&port)
}

/// `http://127.0.0.1:<port>[/mcp]` -> the base URL. Loopback hosts only; the
/// ports of the model services and LCI are refused.
fn parse_loopback_url(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("http://")
        .with_context(|| format!("{url}: only http:// URLs on loopback are accepted"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix("/mcp").unwrap_or(rest);
    let (host, port) = rest
        .rsplit_once(':')
        .with_context(|| format!("{url}: no port"))?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        bail!("{url}: the host must be 127.0.0.1, localhost or [::1]");
    }
    let port: u16 = port.parse().with_context(|| format!("{url}: bad port"))?;
    if port_is_reserved(port) {
        bail!("{url}: ports 8765-8769 are never contacted");
    }
    Ok(format!("http://{host}:{port}"))
}

fn target_of(value: &str) -> Result<Target> {
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(Target::Attach(parse_loopback_url(value)?))
    } else {
        Ok(Target::Launch(PathBuf::from(value)))
    }
}

/// A free loopback port outside 8765-8769.
fn free_port() -> Result<u16> {
    for _ in 0..16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        if !port_is_reserved(port) {
            return Ok(port);
        }
    }
    bail!("no free loopback port outside 8765-8769 after 16 tries");
}

/// The arguments of `serena start-mcp-server` for a tree copy: Streamable HTTP
/// on loopback, no dashboard or log window, one tool per operation, the `ide`
/// context (which leaves out Serena's file-reading and shell tools).
fn launch_args(copy: &Path, port: u16, extra: &[String]) -> Vec<String> {
    let mut args: Vec<String> = [
        "start-mcp-server",
        "--project",
        &copy.display().to_string(),
        "--transport",
        "streamable-http",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--enable-web-dashboard",
        "false",
        "--open-web-dashboard",
        "false",
        "--enable-gui-log-window",
        "false",
        "--agent-interface",
        "tools",
        "--context",
        "ide",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.extend(extra.iter().cloned());
    args
}

/// A Serena process the arm started.
struct ServerChild {
    child: Child,
    pid: u32,
}

/// What stopping the server left behind.
struct Stopped {
    exited: bool,
    leftover: usize,
}

impl ServerChild {
    /// Kills every descendant, then the process; reports whether it exited and
    /// how many descendants are still alive.
    async fn kill_tree(&mut self) -> Stopped {
        let mut system = System::new_all();
        system.refresh_all();
        let descendants = ManagedServer::descendants_of(&system, self.pid);
        for pid in &descendants {
            ManagedServer::kill_process(*pid);
        }
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(10), self.child.wait()).await;
        let mut system = System::new_all();
        system.refresh_all();
        let leftover = descendants
            .iter()
            .filter(|pid| system.process(Pid::from_u32(**pid)).is_some())
            .count();
        Stopped {
            exited: matches!(self.child.try_wait(), Ok(Some(_))),
            leftover,
        }
    }
}

async fn wait_ready(
    child: &mut Child,
    base: &str,
    call_timeout: Duration,
    start_timeout: Duration,
    stderr_log: &Path,
) -> Result<McpSession> {
    let deadline = Instant::now() + start_timeout;
    let mut last: String;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "Serena exited before it answered ({status}); see {}",
                stderr_log.display()
            );
        }
        match McpSession::connect_with(base, call_timeout).await {
            Ok(session) => return Ok(session),
            Err(e) => last = format!("{e:#}"),
        }
        if Instant::now() > deadline {
            bail!(
                "Serena did not answer initialize within {start_timeout:?}: {last}; see {}",
                stderr_log.display()
            );
        }
        // Readiness poll, bounded by `deadline`, like `wait_http_ok`.
        sleep(Duration::from_millis(500)).await;
    }
}

// ------------------------------------------------------------------- the run

/// Everything decided before the server is reached.
struct Plan {
    set: Set,
    rule: SerenaRule,
    wave: String,
    items: Vec<Value>,
    tree_before: TreeHash,
    copy: CopyInfo,
}

/// What the items produced.
struct Driven {
    version: String,
    server: Value,
    offered: Vec<String>,
    used: BTreeMap<String, u32>,
    answers: Vec<Vec<Value>>,
    calls: Vec<u32>,
    wall_ms: Vec<u64>,
    backticked: usize,
    fallback: usize,
    none: usize,
    pattern_fallbacks: usize,
    unknown: u32,
    failures: usize,
    first_failure: Option<String>,
    expansion_failures: usize,
    first_expansion_failure: Option<String>,
    empty: usize,
}

fn set_of(set: SerenaSet) -> Set {
    match set {
        SerenaSet::Traits => Set::Traits,
        SerenaSet::WhereTo => Set::WhereTo,
        SerenaSet::Impact => Set::Impact,
    }
}

fn plan(args: &SerenaArgs, target: &Target) -> Result<Plan> {
    let set = set_of(args.set);
    if args.rule == SerenaRule::Pattern && set == Set::Impact {
        bail!("--rule pattern answers the traits and where-to sets only");
    }
    check_tree(&args.tree)?;
    let tree_abs = std::fs::canonicalize(&args.tree)?;
    std::fs::create_dir_all(&args.out_dir)?;
    if std::fs::canonicalize(&args.out_dir)?.starts_with(&tree_abs) {
        bail!("--out-dir may not lie inside --tree (the frozen tree stays unchanged)");
    }
    let wave = args.wave.clone().unwrap_or_else(|| {
        match args.rule {
            SerenaRule::Symbol => "baseline-serena",
            SerenaRule::Pattern => "baseline-serena-pattern",
        }
        .to_owned()
    });
    let items = rg::load_items(set, &args.gold_dir, args.limit)?;
    let tree_before = tree_hash(&args.tree)?;
    let copy = match target {
        Target::Launch(_) => {
            let dir = args
                .tree_copy
                .clone()
                .unwrap_or_else(|| args.out_dir.join(slug(&wave)).join("tree"));
            prepare_copy(&args.tree, &dir, &tree_before)?
        }
        Target::Attach(_) => {
            let dir = args
                .tree_copy
                .clone()
                .context("a URL in --serena needs --tree-copy: the copy that server serves")?;
            if !dir.is_dir() {
                bail!("--tree-copy {} is not a directory", dir.display());
            }
            let (dir_abs, tree_abs) = (std::fs::canonicalize(&dir)?, tree_abs);
            if dir_abs.starts_with(&tree_abs) || tree_abs.starts_with(&dir_abs) {
                bail!("the tree copy and --tree may not contain one another");
            }
            let (files, links) = list_tree(&dir)?;
            CopyInfo {
                dir,
                created: false,
                files: files.len(),
                links_skipped: links,
                hash: hash_entries(&files),
            }
        }
    };
    Ok(Plan {
        set,
        rule: args.rule,
        wave,
        items,
        tree_before,
        copy,
    })
}

async fn drive<P: Peer>(
    plan: &Plan,
    args: &SerenaArgs,
    version: String,
    server: Value,
    mut peer: P,
) -> Result<Driven> {
    let offered = peer.tool_names().await?;
    let missing: Vec<&str> = REQUIRED_TOOLS
        .iter()
        .copied()
        .filter(|t| !offered.iter().any(|o| o == t))
        .collect();
    if !missing.is_empty() {
        bail!(
            "the server does not offer {missing:?}; it offers {offered:?}. \
             Refusing to run (the arm never guesses tool names)"
        );
    }
    let mut tools = Tools {
        peer,
        offered: offered.clone(),
        max_answer_chars: args.max_answer_chars,
        calls: 0,
        unknown: 0,
        used: BTreeMap::new(),
    };
    let mut out = Driven {
        version,
        server,
        offered,
        used: BTreeMap::new(),
        answers: Vec::with_capacity(plan.items.len()),
        calls: Vec::new(),
        wall_ms: Vec::new(),
        backticked: 0,
        fallback: 0,
        none: 0,
        pattern_fallbacks: 0,
        unknown: 0,
        failures: 0,
        first_failure: None,
        expansion_failures: 0,
        first_expansion_failure: None,
        empty: 0,
    };
    let mut streak = 0usize;
    for (index, item) in plan.items.iter().enumerate() {
        let question = item["question"].as_str().unwrap_or_default();
        let (calls_before, started) = (tools.calls, Instant::now());
        let outcome = answer_item(&mut tools, plan.set, plan.rule, question, args.top_k).await;
        let picked = match outcome {
            Ok(done) => {
                streak = 0;
                match done.source {
                    NeedleSource::Backticked => out.backticked += 1,
                    NeedleSource::Fallback => out.fallback += 1,
                    NeedleSource::None => out.none += 1,
                }
                out.pattern_fallbacks += usize::from(done.pattern_fallback);
                if let Some(message) = done.expansion_failure {
                    out.expansion_failures += 1;
                    out.first_expansion_failure.get_or_insert(message);
                }
                done.answers
            }
            Err(e) => {
                streak += 1;
                out.failures += 1;
                out.first_failure.get_or_insert_with(|| format!("{e:#}"));
                if streak >= MAX_FAILURE_STREAK {
                    bail!(
                        "{streak} items in a row failed; stopping (first failure: {})",
                        out.first_failure.as_deref().unwrap_or_default()
                    );
                }
                Vec::new()
            }
        };
        out.empty += usize::from(picked.is_empty());
        out.answers.push(picked);
        out.calls.push(tools.calls - calls_before);
        out.wall_ms.push(started.elapsed().as_millis() as u64);
        if (index + 1) % 10 == 0 {
            eprintln!("serena: {}/{} done", index + 1, plan.items.len());
        }
    }
    out.unknown = tools.unknown;
    out.used = tools.used;
    Ok(out)
}

/// The order text of `meta.json`.
fn order_text(set: Set, rule: SerenaRule) -> String {
    let how = match (rule, set) {
        (SerenaRule::Pattern, _) => {
            "search_for_pattern lines, distinct needles hit desc, then path, then line"
        }
        (SerenaRule::Symbol, Set::Impact) => {
            "declarations referencing the first symbol of the first needle (find_referencing_symbols), by path, then line"
        }
        (SerenaRule::Symbol, _) => {
            "find_symbol locations (plus implementations of the first type-like symbol for traits), distinct needles hit (queried or in the name path) desc, then path, then line"
        }
    };
    match set {
        Set::WhereTo => format!(
            "one answer per file (its best line); files by matched lines desc, then max needles, then path; lines from {how}"
        ),
        _ => format!("one answer per location: {how}"),
    }
}

struct Closing {
    transport: &'static str,
    launch: &'static str,
    stopped: Option<Stopped>,
    tree_after: TreeHash,
}

fn finish(plan: &Plan, args: &SerenaArgs, driven: Driven, closing: Closing) -> Result<()> {
    let scored: Vec<(&Value, Vec<Value>)> = plan.items.iter().zip(driven.answers).collect();
    let dir = args.out_dir.join(slug(&plan.wave)).join(set_name(plan.set));
    let cap = if args.top_k == 0 {
        "uncapped".to_owned()
    } else {
        format!("top {}", args.top_k)
    };
    let unchanged = closing.tree_after == plan.tree_before;
    let used: Vec<&String> = driven.used.keys().collect();
    let meta = json!({
        "arm": format!("serena baseline ({})", plan.rule.name()),
        "rule": plan.rule.name(),
        "gold_aware": false,
        "answers": cap,
        "order": order_text(plan.set, plan.rule),
        "serena_version": driven.version,
        "serena_server": driven.server,
        "transport": closing.transport,
        "launch": closing.launch,
        "tools": { "offered": driven.offered, "used": used, "calls_by_tool": driven.used },
        "tree_copy": {
            "dir": plan.copy.dir.display().to_string(),
            "created": plan.copy.created,
            "files": plan.copy.files,
            "links_skipped": plan.copy.links_skipped,
            "sha256_at_start": plan.copy.hash.sha256,
            "left_out": [".git (directory or file)", "node_modules", "target (beside a Cargo.toml)"],
        },
        "tree_hash_before": plan.tree_before.sha256,
        "tree_hash_after": closing.tree_after.sha256,
        "tree_files": plan.tree_before.files,
        "tree_unchanged": unchanged,
        "needles": { "backticked": driven.backticked, "fallback": driven.fallback, "none": driven.none },
        "calls": { "total": driven.calls.iter().map(|c| u64::from(*c)).sum::<u64>(), "per_item": driven.calls },
        "unknown_fields": driven.unknown,
        "pattern_fallback_items": driven.pattern_fallbacks,
        "failures": driven.failures,
        "first_failure": driven.first_failure,
        "expansion_failures": driven.expansion_failures,
        "first_expansion_failure": driven.first_expansion_failure,
        "items_with_empty_answer": driven.empty,
        "items": plan.items.len(),
        "wall_ms": driven.wall_ms,
        "serena_exited": closing.stopped.as_ref().map(|s| s.exited),
        "serena_leftover_descendants": closing.stopped.as_ref().map(|s| s.leftover),
        "note": "needles come from the question only; Serena runs on a copy of the tree; wall_ms is informational (the arm is judged by its answers); lines are 1-based (Serena counts from 0); a symbol's line is the start of its range, which can precede the name line",
    });
    let zeros = vec![0u64; scored.len()];
    let summary = write_arm_run(&dir, plan.set, &scored, &zeros, &zeros, &meta)?;
    say(
        &format!(
            "baseline serena ({}, {})",
            plan.rule.name(),
            set_name(plan.set)
        ),
        &dir,
        &summary,
    );
    eprintln!(
        "needles: backticked {}, fallback {}, none {}; empty answers {}; tool calls {}",
        driven.backticked,
        driven.fallback,
        driven.none,
        driven.empty,
        driven.calls.iter().map(|c| u64::from(*c)).sum::<u64>()
    );
    if driven.failures > 0 {
        eprintln!(
            "warning: {} items failed (first: {})",
            driven.failures,
            driven.first_failure.as_deref().unwrap_or_default()
        );
    }
    if !unchanged {
        bail!(
            "the frozen tree's file-list hash changed during the run ({} -> {}); the run dir is written but the tree is no longer the frozen one",
            plan.tree_before.sha256,
            closing.tree_after.sha256
        );
    }
    Ok(())
}

pub(super) fn run_serena_arm(args: SerenaArgs) -> Result<()> {
    // `xtask`'s main already runs a multi-thread runtime; `maps::run` is sync.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(run_async(args)))
}

async fn run_async(args: SerenaArgs) -> Result<()> {
    let value = args
        .serena
        .clone()
        .or_else(|| std::env::var(SERENA_ENV).ok().filter(|v| !v.is_empty()))
        .with_context(|| {
            format!("pass --serena <executable or http://127.0.0.1:port> or set {SERENA_ENV}")
        })?;
    let target = target_of(&value)?;
    let plan = plan(&args, &target)?;
    let (call_timeout, start_timeout) = (
        Duration::from_secs(args.call_timeout_s),
        Duration::from_secs(args.start_timeout_s),
    );
    let (session, mut child, launch) = match &target {
        Target::Attach(base) => (
            McpSession::connect_with(base, call_timeout).await?,
            None,
            "attached",
        ),
        Target::Launch(exe) => {
            let port = free_port()?;
            let wave_dir = args.out_dir.join(slug(&plan.wave));
            std::fs::create_dir_all(&wave_dir)
                .with_context(|| format!("create {}", wave_dir.display()))?;
            let log =
                |suffix: &str| wave_dir.join(format!("serena-{}.{suffix}.log", set_name(plan.set)));
            let (stdout_log, stderr_log) = (log("stdout"), log("stderr"));
            let mut command = Command::new(exe);
            command
                .args(launch_args(&plan.copy.dir, port, &args.serena_args))
                .stdin(Stdio::null())
                .stdout(Stdio::from(std::fs::File::create(&stdout_log)?))
                .stderr(Stdio::from(std::fs::File::create(&stderr_log)?))
                .kill_on_drop(true);
            let spawned = command
                .spawn()
                .with_context(|| format!("start Serena at {}", exe.display()))?;
            let pid = spawned.id().context("Serena has no pid")?;
            let mut child = ServerChild {
                child: spawned,
                pid,
            };
            let base = format!("http://127.0.0.1:{port}");
            match wait_ready(
                &mut child.child,
                &base,
                call_timeout,
                start_timeout,
                &stderr_log,
            )
            .await
            {
                Ok(session) => (session, Some(child), "spawned"),
                Err(e) => {
                    let stopped = child.kill_tree().await;
                    return Err(e.context(format!(
                        "Serena stopped: exited {}, descendants left {}",
                        stopped.exited, stopped.leftover
                    )));
                }
            }
        }
    };
    let info = session.server_info().clone();
    let version = info["version"].as_str().unwrap_or("unknown").to_owned();
    let driven = drive(&plan, &args, version, info, HttpPeer(session)).await;
    let stopped = match child.as_mut() {
        Some(c) => Some(c.kill_tree().await),
        None => None,
    };
    let driven = driven?;
    let tree_after = tree_hash(&args.tree)?;
    finish(
        &plan,
        &args,
        driven,
        Closing {
            transport: "streamable-http",
            launch,
            stopped,
            tree_after,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A canned peer: each tool name has a queue of reply texts.
    struct Stub {
        names: Vec<String>,
        replies: BTreeMap<String, VecDeque<String>>,
        log: Vec<(String, Value)>,
    }

    impl Stub {
        fn new(names: &[&str], replies: &[(&str, &[&str])]) -> Self {
            Self {
                names: names.iter().map(|s| (*s).to_owned()).collect(),
                replies: replies
                    .iter()
                    .map(|(n, r)| ((*n).to_owned(), r.iter().map(|s| (*s).to_owned()).collect()))
                    .collect(),
                log: Vec::new(),
            }
        }
    }

    impl Peer for Stub {
        async fn tool_names(&mut self) -> Result<Vec<String>> {
            Ok(self.names.clone())
        }

        async fn call(&mut self, name: &str, arguments: Value) -> Result<String> {
            self.log.push((name.to_owned(), arguments));
            self.replies
                .get_mut(name)
                .and_then(VecDeque::pop_front)
                .with_context(|| format!("no canned reply for {name}"))
        }
    }

    const ALL_TOOLS: &[&str] = &[
        "find_symbol",
        "find_referencing_symbols",
        "find_implementations",
        "search_for_pattern",
        "replace_content",
    ];

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-k5b-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn places(answers: &[Value]) -> Vec<(String, u64)> {
        answers
            .iter()
            .map(|a| {
                (
                    a["path"].as_str().unwrap().to_owned(),
                    a["line"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    fn at(path: &str, line: u64) -> (String, u64) {
        (path.to_owned(), line)
    }

    fn tools_of(stub: Stub) -> Tools<Stub> {
        Tools {
            offered: stub.names.clone(),
            peer: stub,
            max_answer_chars: 1000,
            calls: 0,
            unknown: 0,
            used: BTreeMap::new(),
        }
    }

    #[test]
    fn serena_symbol_answers_map_to_path_and_line_and_unknown_fields_are_counted() {
        // As the 2.1.0 smoke recorded them: Windows separators, 0-based lines.
        let text = r#"[{"name_path": "Shape", "kind": "Interface", "relative_path": "src\\lib.rs", "body_location": {"start_line": 0, "end_line": 5}},
            {"name_path": "area", "kind": "Method", "relative_path": "src\\lib.rs"},
            {"name_path": "impl Shape for Circle/area", "kind": "Method", "relative_path": "src\\lib.rs", "body_location": {"start_line": 16, "end_line": 18}}]"#;
        let parsed = parse_symbols(text);
        assert_eq!(parsed.unknown, 1, "the entry without a line is counted");
        let found: Vec<(&str, u64, &str)> = parsed
            .places
            .iter()
            .map(|p| (p.path.as_str(), p.line, p.kind.as_str()))
            .collect();
        assert_eq!(
            found,
            [("src/lib.rs", 1, "Interface"), ("src/lib.rs", 17, "Method")]
        );
        assert_eq!(parsed.places[0].raw_path, "src\\lib.rs");
        assert_eq!(parse_symbols("not json").unknown, 1);
        assert_eq!(parse_symbols("[]"), Parsed::default());

        let refs = r#"{"src\\lib.rs": {"Object": [{"name_path": "impl Shape for Circle", "body_location": {"start_line": 15, "end_line": 19}, "content_around_reference": "..."}],
            "Function": [{"name_path": "total", "body_location": {"start_line": 27, "end_line": 29}}, {"name_path": "broken"}]}}"#;
        let all = parse_references(refs, &[]);
        assert_eq!((all.places.len(), all.unknown), (2, 1));
        let impls = parse_references(refs, REF_KEEP);
        assert_eq!(impls.places.len(), 1);
        assert_eq!(
            (
                impls.places[0].path.as_str(),
                impls.places[0].line,
                impls.places[0].kind.as_str()
            ),
            ("src/lib.rs", 16, "Object")
        );
        assert_eq!(parse_references("[]", &[]).unknown, 1);

        let pattern = r#"{"src\\lib.rs": ["  >   0:pub trait Shape {", "...  14:", "  >  15:impl Shape for Circle {", "garbage"]}"#;
        let (lines, unknown) = parse_pattern(pattern);
        assert_eq!(unknown, 1);
        assert_eq!(
            lines,
            [
                PatternLine {
                    path: "src/lib.rs".into(),
                    line: 1,
                    text: "pub trait Shape {".into()
                },
                PatternLine {
                    path: "src/lib.rs".into(),
                    line: 16,
                    text: "impl Shape for Circle {".into()
                },
            ]
        );
        assert_eq!(parse_pattern("{}"), (Vec::new(), 0));
    }

    #[test]
    fn result_text_reads_event_streams_and_plain_json_and_errors() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"text\":\"[]\",\"type\":\"text\"}],\"isError\":false}}\n\n";
        assert_eq!(result_text(sse).unwrap(), "[]");
        let plain =
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"text":"{}","type":"text"}]}}"#;
        assert_eq!(result_text(plain).unwrap(), "{}");
        let error = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"bad"}}"#;
        assert!(result_text(error).is_err());
        assert!(result_text("event: ping\n").is_err());
    }

    #[test]
    fn the_pattern_rule_pairs_with_the_rg_arm_on_the_same_lines() {
        let needles = vec!["Shape".to_owned(), "Circle".to_owned()];
        // The same three matches, as Serena (0-based, Windows paths) and as rg --json.
        let serena = r#"{"src\\b.rs": ["  >   8:let s: Shape = Circle::new();"],
            "src\\a.rs": ["  >  14:impl Shape for Circle {", "  >   2:use Shape;"]}"#;
        let (found, unknown) = parse_pattern(serena);
        assert_eq!(unknown, 0);
        let via_serena = rg::answers_for(
            Set::Traits,
            &pattern_rg_lines(&found, &needles, true),
            10,
            false,
        );
        let rg_json = r#"{"type":"match","data":{"path":{"text":"./src\\b.rs"},"lines":{"text":"let s: Shape = Circle::new();\n"},"line_number":9,"absolute_offset":0,"submatches":[{"match":{"text":"Shape"},"start":7,"end":12},{"match":{"text":"Circle"},"start":15,"end":21}]}}
{"type":"match","data":{"path":{"text":"./src\\a.rs"},"lines":{"text":"impl Shape for Circle {\n"},"line_number":15,"absolute_offset":0,"submatches":[{"match":{"text":"Shape"},"start":5,"end":10},{"match":{"text":"Circle"},"start":15,"end":21}]}}
{"type":"match","data":{"path":{"text":"./src\\a.rs"},"lines":{"text":"use Shape;\n"},"line_number":3,"absolute_offset":0,"submatches":[{"match":{"text":"Shape"},"start":4,"end":9}]}}
"#;
        let via_rg = rg::answers_for(
            Set::Traits,
            &rg::parse_rg_json(rg_json.as_bytes(), &needles, true).unwrap(),
            10,
            false,
        );
        assert_eq!(places(&via_serena), places(&via_rg));
        assert_eq!(
            places(&via_serena),
            [at("src/a.rs", 15), at("src/b.rs", 9), at("src/a.rs", 3)]
        );
        let where_to = rg::answers_for(
            Set::WhereTo,
            &pattern_rg_lines(&found, &needles, true),
            10,
            false,
        );
        assert_eq!(places(&where_to), [at("src/a.rs", 15), at("src/b.rs", 9)]);
        // the regex: escaped whole-word alternation, (?i) only when insensitive
        assert_eq!(
            pattern_regex(&["Shape".into(), "a.b".into()], true),
            r"\b(?:Shape|a\.b)\b"
        );
        assert_eq!(
            pattern_regex(&["shape".into()], false),
            r"(?i)\b(?:shape)\b"
        );
    }

    #[test]
    fn a_tree_with_a_serena_directory_is_refused_and_the_copy_is_used() {
        let root = temp_dir("copy");
        let tree = root.join("tree");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::create_dir_all(tree.join(".git")).unwrap();
        std::fs::create_dir_all(tree.join("target")).unwrap();
        std::fs::write(tree.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(tree.join("src/lib.rs"), "pub trait Shape {}\n").unwrap();
        std::fs::write(tree.join(".git/HEAD"), "ref\n").unwrap();
        // a worktree or submodule keeps `.git` as a file that points at the real repository
        std::fs::write(tree.join("src/.git"), "gitdir: elsewhere\n").unwrap();
        std::fs::write(tree.join("target/junk"), "x\n").unwrap();

        // a clean tree passes; one with .serena is refused with the reason
        check_tree(&tree).unwrap();
        let before = tree_hash(&tree).unwrap();
        assert_eq!(before.files, 2, ".git and the cargo target are left out");
        std::fs::create_dir_all(tree.join(".serena")).unwrap();
        let refusal = check_tree(&tree).unwrap_err().to_string();
        assert!(
            refusal.contains(".serena") && refusal.contains("in place"),
            "{refusal}"
        );
        std::fs::remove_dir_all(tree.join(".serena")).unwrap();

        // the copy holds the files, not .git or target, and leaves the tree alone
        let copy_dir = root.join("copy");
        let copy = prepare_copy(&tree, &copy_dir, &before).unwrap();
        assert!(copy.created);
        assert_eq!(copy.files, 2);
        assert!(copy_dir.join("src/lib.rs").is_file());
        assert!(!copy_dir.join(".git").exists() && !copy_dir.join("target").exists());
        assert!(!copy_dir.join("src/.git").exists());
        assert!(copy_dir.join(COPY_MARKER).is_file());
        assert_eq!(tree_hash(&tree).unwrap(), before);
        // a second run reuses it; a stranger directory or a tree inside is refused
        assert!(!prepare_copy(&tree, &copy_dir, &before).unwrap().created);
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("mine.txt"), "keep\n").unwrap();
        let refused = prepare_copy(&tree, &other, &before)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("not a copy of this tree"), "{refused}");
        assert!(other.join("mine.txt").is_file(), "nothing deleted");
        assert!(prepare_copy(&tree, &tree.join("src"), &before).is_err());
        // a changed tree hashes differently
        std::fs::write(tree.join("src/lib.rs"), "pub trait Shape { fn a(); }\n").unwrap();
        assert_ne!(tree_hash(&tree).unwrap(), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn urls_are_loopback_only_and_launch_args_are_streamable_http_on_loopback() {
        assert_eq!(
            parse_loopback_url("http://127.0.0.1:9000/mcp").unwrap(),
            "http://127.0.0.1:9000"
        );
        assert_eq!(
            parse_loopback_url("http://localhost:9000/").unwrap(),
            "http://localhost:9000"
        );
        for bad in [
            "http://192.168.1.4:9000",
            "http://0.0.0.0:9000",
            "https://127.0.0.1:9000",
            "http://127.0.0.1:8766",
            "http://127.0.0.1:8768/mcp",
            "http://127.0.0.1",
        ] {
            assert!(parse_loopback_url(bad).is_err(), "{bad}");
        }
        assert_eq!(
            target_of("http://127.0.0.1:9000").unwrap(),
            Target::Attach("http://127.0.0.1:9000".into())
        );
        assert_eq!(
            target_of("serena").unwrap(),
            Target::Launch(PathBuf::from("serena"))
        );
        let args = launch_args(
            Path::new("copy"),
            9123,
            &["--log-level".into(), "DEBUG".into()],
        );
        let joined = args.join(" ");
        assert!(
            joined.starts_with("start-mcp-server --project copy"),
            "{joined}"
        );
        assert!(joined.contains("--transport streamable-http --host 127.0.0.1 --port 9123"));
        assert!(joined.contains("--enable-web-dashboard false --open-web-dashboard false"));
        assert!(joined.ends_with("--log-level DEBUG"));
        assert!(!port_is_reserved(free_port().unwrap()));
    }

    #[tokio::test]
    async fn tools_list_without_the_symbol_tools_fails_before_any_item() {
        let root = temp_dir("no-tools");
        let (args, plan) = fixture(&root, "Which types implement `Shape`?");
        let stub = Stub::new(&["read_file"], &[]);
        let error = drive(&plan, &args, "x".into(), Value::Null, stub)
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("find_symbol") && error.contains("Refusing to run"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_tool_off_the_allow_list_never_reaches_the_peer() {
        let mut tools = tools_of(Stub::new(ALL_TOOLS, &[("replace_content", &["done"])]));
        let error = tools
            .call("replace_content", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("allow-list"), "{error}");
        let error = tools
            .call("read_file", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("allow-list"), "{error}");
        assert!(tools.peer.log.is_empty() && tools.calls == 0);
        // an allowed tool the server does not offer is refused too
        let mut bare = tools_of(Stub::new(&["find_symbol"], &[]));
        assert!(bare.call("find_implementations", json!({})).await.is_err());
        assert!(bare.peer.log.is_empty());
    }

    #[tokio::test]
    async fn the_symbol_rule_expands_a_trait_through_its_implementations() {
        let shape = r#"[{"name_path": "Shape", "kind": "Interface", "relative_path": "src\\lib.rs", "body_location": {"start_line": 0, "end_line": 5}}]"#;
        let impls = r#"[{"name_path": "impl Shape for Circle", "kind": "Object", "relative_path": "src\\lib.rs", "body_location": {"start_line": 15, "end_line": 19}},
            {"name_path": "impl Shape for Square", "kind": "Object", "relative_path": "src\\lib.rs", "body_location": {"start_line": 21, "end_line": 25}}]"#;
        let mut tools = tools_of(Stub::new(
            ALL_TOOLS,
            &[
                ("find_symbol", &[shape]),
                ("find_implementations", &[impls]),
            ],
        ));
        let done = answer_item(
            &mut tools,
            Set::Traits,
            SerenaRule::Symbol,
            "Which types implement `shapes::Shape`?",
            10,
        )
        .await
        .unwrap();
        assert_eq!(done.source, NeedleSource::Backticked);
        assert_eq!(
            places(&done.answers),
            [
                at("src/lib.rs", 1),
                at("src/lib.rs", 16),
                at("src/lib.rs", 22)
            ]
        );
        assert_eq!(tools.calls, 2);
        let names: Vec<&str> = tools.peer.log.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["find_symbol", "find_implementations"]);
        // follow-up calls send the path as Serena wrote it; every call carries the cap
        assert_eq!(tools.peer.log[1].1["relative_path"], "src\\lib.rs");
        assert_eq!(tools.peer.log[1].1["max_answer_chars"], 1000);
        assert_eq!(tools.peer.log[0].1["substring_matching"], false);

        // without find_implementations the referencing symbols of impl kind answer
        let refs = r#"{"src\\lib.rs": {"Object": [{"name_path": "impl Shape for Circle", "body_location": {"start_line": 15, "end_line": 19}}],
            "Function": [{"name_path": "total", "body_location": {"start_line": 27, "end_line": 29}}]}}"#;
        let mut tools = tools_of(Stub::new(
            &[
                "find_symbol",
                "find_referencing_symbols",
                "search_for_pattern",
            ],
            &[
                ("find_symbol", &[shape]),
                ("find_referencing_symbols", &[refs]),
            ],
        ));
        let done = answer_item(
            &mut tools,
            Set::Traits,
            SerenaRule::Symbol,
            "Which types implement `Shape`?",
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            places(&done.answers),
            [at("src/lib.rs", 1), at("src/lib.rs", 16)]
        );
    }

    #[tokio::test]
    async fn impact_answers_the_declarations_that_reference_the_first_symbol() {
        let foo = r#"[{"name_path": "Foo", "kind": "Struct", "relative_path": "a.rs", "body_location": {"start_line": 0, "end_line": 0}}]"#;
        let refs = r#"{"a.rs": {"Function": [{"name_path": "beta", "body_location": {"start_line": 2, "end_line": 3}}, {"name_path": "alpha", "body_location": {"start_line": 1, "end_line": 1}}]}}"#;
        let mut tools = tools_of(Stub::new(
            ALL_TOOLS,
            &[
                ("find_symbol", &[foo]),
                ("find_referencing_symbols", &[refs]),
            ],
        ));
        let done = answer_item(
            &mut tools,
            Set::Impact,
            SerenaRule::Symbol,
            "What breaks if `Foo` changes?",
            10,
        )
        .await
        .unwrap();
        assert_eq!(places(&done.answers), [at("a.rs", 2), at("a.rs", 3)]);
        // a prose question has no backticked name: lexical terms find no symbol
        let mut tools = tools_of(Stub::new(
            ALL_TOOLS,
            &[("find_symbol", &["[]", "[]", "[]"])],
        ));
        let done = answer_item(
            &mut tools,
            Set::Impact,
            SerenaRule::Symbol,
            "what breaks",
            10,
        )
        .await
        .unwrap();
        assert!(done.answers.is_empty());
        // a pattern rule on impact is not offered
        let root = temp_dir("impact-pattern");
        let (mut args, _) = fixture(&root, "q?");
        args.set = SerenaSet::Impact;
        args.rule = SerenaRule::Pattern;
        let target = Target::Launch(PathBuf::from("serena"));
        assert!(plan(&args, &target).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn where_to_falls_back_to_the_pattern_when_no_symbol_matches() {
        let pattern = r#"{"src\\cfg.rs": ["  >   4:fn read_config() {}", "  >  10:fn config_path() {}"], "src\\main.rs": ["  >   2:read_config();"]}"#;
        let mut tools = tools_of(Stub::new(
            ALL_TOOLS,
            &[
                ("find_symbol", &["[]", "[]", "[]", "[]"]),
                ("search_for_pattern", &[pattern]),
            ],
        ));
        let done = answer_item(
            &mut tools,
            Set::WhereTo,
            SerenaRule::Symbol,
            "where do config readers register",
            10,
        )
        .await
        .unwrap();
        assert_eq!(done.source, NeedleSource::Fallback);
        assert!(done.pattern_fallback);
        assert_eq!(
            places(&done.answers),
            [at("src/cfg.rs", 5), at("src/main.rs", 3)]
        );
        let regex = tools.peer.log.last().unwrap().1["substring_pattern"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(regex.starts_with("(?i)\\b(?:"), "{regex}");
    }

    #[tokio::test]
    async fn the_pattern_rule_keeps_the_files_the_rg_arm_searches() {
        assert!(in_adapter_globs("crates/a/src/lib.rs"));
        assert!(in_adapter_globs("CHANGELOG.md"));
        assert!(in_adapter_globs("web/app.tsx"));
        assert!(!in_adapter_globs("Cargo.toml"));
        assert!(!in_adapter_globs("pnpm-lock.yaml"));
        assert!(!in_adapter_globs("notes.rs.txt"));
        let pattern = r#"{"Cargo.toml": ["  >   3:Shape = 1"], "docs\\a.md": ["  >   4:Shape docs"], "src\\a.rs": ["  >   0:trait Shape {"]}"#;
        let mut tools = tools_of(Stub::new(ALL_TOOLS, &[("search_for_pattern", &[pattern])]));
        let lines = pattern_lines(&mut tools, &["Shape".to_owned()], true)
            .await
            .unwrap();
        let kept: Vec<(&str, u64)> = lines.iter().map(|l| (l.path.as_str(), l.line)).collect();
        assert_eq!(kept, [("docs/a.md", 5), ("src/a.rs", 1)]);
        let sent = &tools.peer.log[0].1;
        assert_eq!(sent["restrict_search_to_code_files"], false);
        assert_eq!(sent["multiline"], false);
    }

    fn traits_item() -> Value {
        let span =
            |file: &str, a: i64, b: i64| json!({"file": file, "begin_line": a, "end_line": b});
        json!({
            "id": "t-Q2-0", "set": "Q2", "stratum": "explicit",
            "question": "Where is `Shape::area` implemented for `Circle`?",
            "trait": "x::Shape", "trait_decl": span("src/lib.rs", 1, 6),
            "gold": [{"impl_id": "i", "stratum": "explicit",
                      "location": span("src/lib.rs", 17, 19), "impl": span("src/lib.rs", 16, 20)}]
        })
    }

    /// A tree, a gold dir with one traits item, and the args/plan for a launch.
    fn fixture(root: &Path, question: &str) -> (SerenaArgs, Plan) {
        let tree = root.join("tree");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(tree.join("src/lib.rs"), "pub trait Shape {}\n").unwrap();
        let gold = root.join("gold");
        std::fs::create_dir_all(&gold).unwrap();
        let mut item = traits_item();
        item["question"] = json!(question);
        std::fs::write(gold.join("traits.json"), json!([item]).to_string()).unwrap();
        let args = SerenaArgs {
            set: SerenaSet::Traits,
            gold_dir: gold,
            tree,
            serena: Some("serena".into()),
            top_k: 10,
            wave: Some("w".into()),
            limit: None,
            rule: SerenaRule::Symbol,
            tree_copy: None,
            start_timeout_s: 1,
            call_timeout_s: 1,
            max_answer_chars: 1000,
            serena_args: Vec::new(),
            out_dir: root.join("out"),
        };
        let plan = plan(&args, &Target::Launch(PathBuf::from("serena"))).unwrap();
        (args, plan)
    }

    #[tokio::test]
    async fn serena_arm_run_dir_is_read_by_compare_and_the_scorer() {
        let root = temp_dir("run-dir");
        let (args, plan) = fixture(&root, "Where is `Shape::area` implemented for `Circle`?");
        // the copy path is recorded and Serena would serve it, not the tree
        assert_eq!(plan.copy.dir, args.out_dir.join("w").join("tree"));
        assert!(plan.copy.dir.join("src/lib.rs").is_file());
        let area = r#"[{"name_path": "impl Shape for Square/area", "kind": "Method", "relative_path": "src\\lib.rs", "body_location": {"start_line": 22, "end_line": 24}},
            {"name_path": "impl Shape for Circle/area", "kind": "Method", "relative_path": "src\\lib.rs", "body_location": {"start_line": 16, "end_line": 18}}]"#;
        let circle = r#"[{"name_path": "Circle", "kind": "Struct", "relative_path": "src\\lib.rs", "body_location": {"start_line": 7, "end_line": 9}}]"#;
        let refs = r#"{"src\\lib.rs": {"Object": [{"name_path": "impl Shape for Circle", "body_location": {"start_line": 15, "end_line": 19}}]}}"#;
        let stub = Stub::new(
            ALL_TOOLS,
            &[
                ("find_symbol", &[area, circle]),
                ("find_referencing_symbols", &[refs]),
            ],
        );
        let server = json!({"name": "Serena", "version": "2.1.0"});
        let driven = drive(&plan, &args, "2.1.0".into(), server, stub)
            .await
            .unwrap();
        assert_eq!(
            (driven.failures, driven.empty, driven.backticked),
            (0, 0, 1)
        );
        assert_eq!(driven.calls, [3]);
        let tree_after = tree_hash(&args.tree).unwrap();
        finish(
            &plan,
            &args,
            driven,
            Closing {
                transport: "streamable-http",
                launch: "spawned",
                stopped: None,
                tree_after,
            },
        )
        .unwrap();
        let dir = args.out_dir.join("w").join("traits");
        for file in ["run.json", "items.json", "score.json", "meta.json"] {
            assert!(dir.join(file).is_file(), "{file}");
        }
        let data = crate::maps::compare::load_run(&dir, None).unwrap();
        assert_eq!(data.set, "traits");
        let record = &data.items["t-Q2-0"];
        assert_eq!(record["hit@10"], Some(1.0));
        assert_eq!(record["expansions"], Some(0.0));
        assert_eq!(record["model_calls"], Some(0.0));
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["arm"], "serena baseline (symbol)");
        assert_eq!(meta["gold_aware"], false);
        assert_eq!(meta["serena_version"], "2.1.0");
        assert_eq!(meta["tree_unchanged"], true);
        assert_eq!(meta["calls"]["total"], 3);
        assert!(meta["tree_copy"]["dir"].as_str().unwrap().ends_with("tree"));
        assert_eq!(
            meta["tools"]["used"],
            json!(["find_referencing_symbols", "find_symbol"])
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A one-connection-at-a-time MCP server on 127.0.0.1:0 that speaks the
    /// Streamable HTTP shape Serena 2.1.0 answered with (event-stream bodies, a
    /// session id header), and records the tool names it was asked to call.
    async fn fake_mcp(
        tools: Vec<&'static str>,
        reply: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let called = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = called.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut data = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
                    let Ok(n) = stream.read(&mut chunk).await else {
                        break String::new();
                    };
                    if n == 0 {
                        break String::new();
                    }
                    data.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&data).into_owned();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let want = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if rest.len() >= want {
                            break rest[..want].to_owned();
                        }
                    }
                };
                let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let id = request["id"].clone();
                let envelope = match request["method"].as_str() {
                    Some("initialize") => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"serverInfo": {"name": "Serena", "version": "9.9.9"}, "capabilities": {}}})
                    }
                    Some("tools/list") => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools.iter().map(|t| json!({"name": t})).collect::<Vec<_>>()}})
                    }
                    Some("tools/call") => {
                        seen.lock()
                            .unwrap()
                            .push(request["params"]["name"].as_str().unwrap_or("").to_owned());
                        json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": reply}], "isError": false}})
                    }
                    _ => Value::Null,
                };
                let response = if envelope.is_null() {
                    "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_owned()
                } else {
                    let payload = format!("event: message\r\ndata: {envelope}\r\n\r\n");
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nmcp-session-id: s1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (base, called)
    }

    #[tokio::test]
    async fn the_http_peer_talks_to_a_fake_mcp_server_on_loopback() {
        let (base, called) = fake_mcp(
            vec!["find_symbol", "find_referencing_symbols", "search_for_pattern", "replace_content"],
            r#"[{"name_path": "Circle", "kind": "Struct", "relative_path": "src\\lib.rs", "body_location": {"start_line": 7, "end_line": 9}}]"#,
        )
        .await;
        let session = McpSession::connect_with(&base, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(session.server_info()["version"], "9.9.9");
        let mut peer = HttpPeer(session);
        let names = peer.tool_names().await.unwrap();
        assert!(names.contains(&"find_symbol".to_owned()));
        let offered = names.clone();
        let mut tools = Tools {
            peer,
            offered,
            max_answer_chars: 1000,
            calls: 0,
            unknown: 0,
            used: BTreeMap::new(),
        };
        let done = answer_item(
            &mut tools,
            Set::WhereTo,
            SerenaRule::Symbol,
            "Where is `Circle` declared?",
            10,
        )
        .await
        .unwrap();
        assert_eq!(places(&done.answers), [at("src/lib.rs", 8)]);
        // an off-list call is refused client-side and never reaches the server
        assert!(tools.call("replace_content", json!({})).await.is_err());
        let seen = called.lock().unwrap().clone();
        assert_eq!(seen, ["find_symbol"]);
    }
}
