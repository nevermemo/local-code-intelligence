//! `cargo xtask maps baseline`: simple, non-walk arms written as run dirs that
//! the bench scorer and `maps compare` read (`run.json`, `items.json`,
//! `score.json`, `meta.json` under `<out>/<wave>/<set>`; no trend row).
//!
//! * `impact`: M1.4 arm N. The anchor comes from the backticked names of the
//!   question (no gold), the answer is the exact one-hop reverse closure of
//!   the anchor symbol, ordered by how many references of the anchor each
//!   declaration holds (most first), then by path and line, and cut to the
//!   bench's `--top-k`. Everything is `lci_maps::impact`'s public API.
//! * `history`: `git log -G/-S/--grep` over the frozen eval tree, from the
//!   identifiers of the question, in the fixed order documented on
//!   [`HistoryArgs`].
//!
//! Both write the answers as the bench's adapters do (impact `{path, line}`,
//! history `{commit}`) and score them with [`super::bench::item_record`], so a
//! baseline run pairs with any bench run in `maps compare`. Neither arm
//! expands anything: `expansions` is 0.

use super::bench::{self, item_record, select_items, set_name, slug};
use crate::gold::{self, Set};
use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use lci_maps::impact::{ImpactIndex, code_names, dir_source};
use lci_maps::scip_graph::SourceText;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Subcommand)]
pub enum BaselineCommand {
    /// M1.4 arm N on the impact set: anchor from the question's backticked
    /// names, then the exact one-hop reverse closure of the anchor, ordered by
    /// the anchor's reference count per declaration, cut to --top-k. Not given
    /// the gold. Writes a run dir for `maps compare`.
    Impact(ImpactArgs),
    /// `git log -G/-S/--grep` on the frozen eval tree, from the identifiers of
    /// each history question (H1 first creation, H2 last modification, H3
    /// free text), top --top-k commits in a fixed order. Writes a run dir for
    /// `maps compare`.
    History(HistoryArgs),
}

pub fn run(command: BaselineCommand) -> Result<()> {
    match command {
        BaselineCommand::Impact(args) => run_impact(args),
        BaselineCommand::History(args) => run_history(args),
    }
}

// ------------------------------------------------------------ the run dir

/// What a written arm looks like in one line.
pub(super) struct ArmSummary {
    pub n: usize,
    pub precision: f64,
    pub f05: f64,
    pub headline: String,
}

/// Writes `run.json`, `items.json`, `score.json` and `meta.json` under `dir`
/// for the scored `(item, answers)` pairs, with `expansions` per pair.
pub(super) fn write_arm_run(
    dir: &Path,
    set: Set,
    scored: &[(&Value, Vec<Value>)],
    expansions: &[u64],
    meta: &Value,
) -> Result<ArmSummary> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut run = Map::new();
    let mut records = Map::new();
    let (mut precision, mut f05) = (0.0, 0.0);
    for ((item, answers), work) in scored.iter().zip(expansions) {
        let id = item["id"].as_str().context("gold item without an id")?;
        let (p, f, record) = item_record(set, item, answers, Some(*work))?;
        precision += p;
        f05 += f;
        run.insert(id.to_owned(), Value::Array(answers.clone()));
        records.insert(id.to_owned(), Value::Object(record));
    }
    let run = Value::Object(run);
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
    std::fs::write(dir.join("run.json"), pretty(&run)?)?;
    std::fs::write(
        dir.join("items.json"),
        pretty(&json!({ "set": set_name(set), "items": records }))?,
    )?;
    std::fs::write(dir.join("meta.json"), pretty(meta)?)?;
    let n = scored.len();
    let mut headline = String::from("nothing scored");
    if n > 0 {
        let items: Vec<Value> = scored.iter().map(|(item, _)| (*item).clone()).collect();
        let parsed = gold::parse_run(&run.to_string())?;
        let report = gold::score_set(set, &items, &parsed, false, false)?;
        std::fs::write(dir.join("score.json"), pretty(&report)?)?;
        headline = bench::headline(set)
            .iter()
            .filter_map(|name| {
                let v = report["overall"]["metrics"][*name].as_f64()?;
                Some(format!("{name}={v:.3}"))
            })
            .collect::<Vec<_>>()
            .join("; ");
    }
    let denominator = n.max(1) as f64;
    Ok(ArmSummary {
        n,
        precision: precision / denominator,
        f05: f05 / denominator,
        headline,
    })
}

fn say(label: &str, dir: &Path, summary: &ArmSummary) {
    eprintln!(
        "{label}: {} items scored, precision {:.3}, f0.5 {:.3}; {}\nrun dir: {}",
        summary.n,
        summary.precision,
        summary.f05,
        summary.headline,
        dir.display()
    );
}

// ----------------------------------------------------------------- impact

#[derive(Args)]
pub struct ImpactArgs {
    /// Directory holding impact.json (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The SCIP index of the frozen tree.
    #[arg(long)]
    scip: PathBuf,
    /// The frozen tree the index describes (read for the index's
    /// macro-expansion repair).
    #[arg(long)]
    tree: PathBuf,
    /// Answers per item, the bench's `--top-k`; 0 keeps the whole closure
    /// (arm N-full).
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/impact`.
    #[arg(long, default_value = "baseline-n")]
    wave: String,
    /// Run only the first N primary items.
    #[arg(long)]
    limit: Option<usize>,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

/// What arm N made of one question.
#[derive(Debug, Clone, PartialEq)]
pub struct ImpactPick {
    pub answers: Vec<Value>,
    /// Backticked names the question gave.
    pub names: usize,
    pub anchored: bool,
    /// Declarations in the closure, before the cut.
    pub closure: usize,
}

/// Arm N for one question: anchor (`anchor_n`), closure, order by the anchor's
/// reference count per declaration (descending), then path, line, symbol; cut
/// to `top_k` (0 = no cut). Answers are `{path, line}` of each declaration's
/// name, the form the bench's impact adapter offers.
pub fn pick_impact(ix: &ImpactIndex, question: &str, top_k: usize) -> ImpactPick {
    let (names, _) = code_names(question);
    let none = |names: usize| ImpactPick {
        answers: Vec::new(),
        names,
        anchored: false,
        closure: 0,
    };
    let Some(anchor) = ix.anchor_n(&names) else {
        return none(names.len());
    };
    let symbol = &ix.defs[anchor.def].symbol;
    let mut held: HashMap<usize, usize> = HashMap::new();
    for (path, line, col) in ix.occurrences(symbol) {
        if let Some(unit) = ix.unit_at(&path, line, col) {
            *held.entry(unit).or_default() += 1;
        }
    }
    let mut decls: Vec<(usize, lci_maps::impact::Decl)> = ix
        .closure(symbol)
        .into_iter()
        .map(|def| (held.get(&def).copied().unwrap_or(0), ix.decl(def)))
        .collect();
    decls.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            (&a.1.path, a.1.line, &a.1.symbol).cmp(&(&b.1.path, b.1.line, &b.1.symbol))
        })
    });
    let closure = decls.len();
    let keep = if top_k == 0 {
        closure
    } else {
        top_k.min(closure)
    };
    let answers = decls
        .into_iter()
        .take(keep)
        .map(|(_, d)| json!({ "path": d.path, "line": d.line }))
        .collect();
    ImpactPick {
        answers,
        names: names.len(),
        anchored: true,
        closure,
    }
}

fn run_impact(args: ImpactArgs) -> Result<()> {
    let items = select_items(
        Set::Impact,
        gold::load_gold(&args.gold_dir.join("impact.json"))?,
        args.limit,
        None,
    );
    let source = dir_source(&args.tree);
    let ix = ImpactIndex::load(&args.scip, Some(&source as &dyn SourceText))
        .map_err(|e| anyhow!("load the SCIP index: {e}"))?;
    let (mut no_names, mut no_anchor, mut empty) = (0usize, 0usize, 0usize);
    let mut scored: Vec<(&Value, Vec<Value>)> = Vec::new();
    for item in &items {
        let question = item["question"].as_str().unwrap_or_default();
        let pick = pick_impact(&ix, question, args.top_k);
        no_names += usize::from(pick.names == 0);
        no_anchor += usize::from(!pick.anchored);
        empty += usize::from(pick.answers.is_empty());
        scored.push((item, pick.answers));
    }
    let dir = args.out_dir.join(slug(&args.wave)).join("impact");
    let cap = if args.top_k == 0 {
        "uncapped (N-full)".to_owned()
    } else {
        format!("top {}", args.top_k)
    };
    let meta = json!({
        "arm": "M1.4 arm N (baseline impact)",
        "gold_aware": false,
        "answers": cap,
        "order": "anchor reference count per declaration desc, then path, line, symbol",
        "items": items.len(),
        "items_without_backticked_names": no_names,
        "items_without_anchor": no_anchor,
        "items_with_empty_answer": empty,
        "note": "the anchor reads the backticked names of the question; the engine's hybrid seeds do not parse backticks; the closure comes from the same SCIP index as the gold's R1",
    });
    let expansions = vec![0u64; scored.len()];
    let summary = write_arm_run(&dir, Set::Impact, &scored, &expansions, &meta)?;
    say("baseline impact (arm N)", &dir, &summary);
    eprintln!("without names {no_names}, without anchor {no_anchor}, empty {empty}");
    Ok(())
}

// ---------------------------------------------------------------- history

#[derive(Args)]
pub struct HistoryArgs {
    /// Directory holding history.json (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The frozen eval tree: a local git checkout. Only read with `git log`.
    #[arg(long)]
    tree: PathBuf,
    /// Commits per item, the bench's `--top-k`.
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/history`.
    #[arg(long, default_value = "baseline-git")]
    wave: String,
    /// Run only the first N items.
    #[arg(long)]
    limit: Option<usize>,
    /// Longest one `git log` call may run, in seconds; a slower call is killed
    /// and counts as a failure (its commits are left out).
    #[arg(long, default_value_t = 60)]
    git_timeout_s: u64,
    /// The git binary.
    #[arg(long, default_value = "git")]
    git: PathBuf,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

/// The history question families this arm tells apart by wording (not by the
/// gold's `stratum`). The patterns come from the wording described in the R3
/// design and are not checked against the private gold here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryQuery {
    /// H1, "first created `X`": commits that introduced a declaration of X,
    /// oldest first.
    Creation { idents: Vec<String> },
    /// H2, "most recent commit that modified `X` (path)", with the sha the
    /// history is cut at: commits that changed the count of X in the path,
    /// newest first; the plain file log when there are none.
    LastModified {
        idents: Vec<String>,
        path: Option<String>,
        sha: Option<String>,
    },
    /// H3, free "why" text: commits matching up to five tokens by message
    /// (`--grep -i`) or by diff (`-S`).
    Why { tokens: Vec<String> },
}

impl HistoryQuery {
    pub fn family(&self) -> &'static str {
        match self {
            Self::Creation { .. } => "H1",
            Self::LastModified { .. } => "H2",
            Self::Why { .. } => "H3",
        }
    }
}

/// The most tokens of an H3 query.
const MAX_TOKENS: usize = 5;
/// Words of six letters or more that say nothing about the code.
const STOP_WORDS: [&str; 22] = [
    "because", "should", "would", "could", "which", "where", "there", "their", "these", "those",
    "before", "after", "being", "using", "commit", "commits", "change", "changed", "reason",
    "happened", "behind", "decide",
];
const PATH_EXTENSIONS: [&str; 8] = [".rs", ".toml", ".md", ".json", ".py", ".ts", ".js", ".yml"];

fn is_ident(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(|c: char| c.is_ascii_digit())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The identifiers in the backticks of a question: the last `::` segment of a
/// span, `()` and generic arguments dropped; spans that are not paths of
/// identifiers (code, file names) are skipped. First occurrence order.
pub fn backtick_idents(question: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for span in question.split('`').skip(1).step_by(2) {
        let mut depth = 0u32;
        let text: String = span
            .trim()
            .chars()
            .filter(|&c| match c {
                '<' => {
                    depth += 1;
                    false
                }
                '>' => {
                    depth = depth.saturating_sub(1);
                    false
                }
                _ => depth == 0,
            })
            .collect();
        let text = text.trim().strip_suffix("()").unwrap_or(text.trim());
        let Some(last) = text.rsplit("::").next() else {
            continue;
        };
        if text.split("::").all(is_ident) && !found.iter().any(|f| f == last) {
            found.push(last.to_owned());
        }
    }
    found
}

fn trim_token(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '`' | '(' | ')' | ',' | ';' | ':' | '\'' | '"' | '?' | '.'
        )
    })
}

/// The first token that looks like a file path: it has a `/`, or a known
/// extension, and does not start with `-`.
fn path_token(question: &str) -> Option<String> {
    question
        .split_whitespace()
        .map(trim_token)
        .find(|t| {
            !t.starts_with('-')
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
                && (t.contains('/') || PATH_EXTENSIONS.iter().any(|e| t.ends_with(e)))
                && t.chars().any(|c| c.is_ascii_alphabetic())
        })
        .map(str::to_owned)
}

/// The first token that looks like an abbreviated or full commit sha: seven to
/// forty hex digits with at least one decimal digit.
fn sha_token(question: &str) -> Option<String> {
    question
        .split_whitespace()
        .map(trim_token)
        .find(|t| {
            (7..=40).contains(&t.len())
                && t.chars().all(|c| c.is_ascii_hexdigit())
                && t.chars().any(|c| c.is_ascii_digit())
        })
        .map(|t| t.to_ascii_lowercase())
}

/// Up to [`MAX_TOKENS`] tokens for an H3 query: the backticked identifiers,
/// then words of six or more letters (lower case, not in [`STOP_WORDS`]), in
/// the order of the question, each once.
pub fn why_tokens(question: &str) -> Vec<String> {
    let mut tokens: Vec<String> = backtick_idents(question);
    let outside: String = question.split('`').step_by(2).collect::<Vec<_>>().join(" ");
    for word in outside.split(|c: char| !c.is_ascii_alphabetic()) {
        let word = word.to_ascii_lowercase();
        if word.len() >= 6
            && !STOP_WORDS.contains(&word.as_str())
            && !tokens.iter().any(|t| t.eq_ignore_ascii_case(&word))
        {
            tokens.push(word);
        }
    }
    tokens.truncate(MAX_TOKENS);
    tokens
}

/// Tells the family of a question by its wording.
pub fn classify(question: &str) -> HistoryQuery {
    let lower = question.to_ascii_lowercase();
    let idents = backtick_idents(question);
    let created = [
        "first created",
        "first introduced",
        "first added",
        "was created",
        "first appeared",
    ]
    .iter()
    .any(|p| lower.contains(p));
    let modified = [
        "most recent",
        "last modified",
        "latest commit",
        "last changed",
        "last touched",
    ]
    .iter()
    .any(|p| lower.contains(p));
    if modified && (path_token(question).is_some() || !idents.is_empty()) {
        return HistoryQuery::LastModified {
            idents,
            path: path_token(question),
            sha: sha_token(question),
        };
    }
    if created && !idents.is_empty() {
        return HistoryQuery::Creation { idents };
    }
    HistoryQuery::Why {
        tokens: why_tokens(question),
    }
}

/// `git log` over a tree: the lines of its output, or why it failed.
pub trait GitLog {
    fn log(&self, args: &[String]) -> Result<Vec<String>, String>;
}

pub struct RealGit {
    pub git: PathBuf,
    pub tree: PathBuf,
    pub timeout: Duration,
}

/// Runs `command`, killing it after `timeout`; the stdout text, or the error.
fn run_capture(mut command: Command, timeout: Duration) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command.spawn().map_err(|e| format!("run git: {e}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = (stdout.join(), stderr.join());
                return Err(format!("timeout after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("wait for git: {e}")),
        }
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if !status.success() {
        let text = String::from_utf8_lossy(&err);
        return Err(format!("git exited with {status}: {}", text.trim()));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

impl GitLog for RealGit {
    fn log(&self, args: &[String]) -> Result<Vec<String>, String> {
        let mut command = Command::new(&self.git);
        command.arg("-C").arg(&self.tree).arg("log").args(args);
        let text = run_capture(command, self.timeout)?;
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// Calls that failed in a run (a timeout, an unknown revision), with the
/// first reason.
#[derive(Debug, Default)]
pub struct Tally {
    pub failures: usize,
    pub first_failure: Option<String>,
}

fn ask(git: &dyn GitLog, args: &[&str], tally: &mut Tally) -> Vec<String> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    git.log(&args).unwrap_or_else(|why| {
        tally.failures += 1;
        tally.first_failure.get_or_insert(why);
        Vec::new()
    })
}

/// Like [`ask`] on `rev`, and on `HEAD` when `rev` fails (a sha the tree does
/// not have).
fn ask_at(git: &dyn GitLog, rev: &str, rest: &[String], tally: &mut Tally) -> Vec<String> {
    let mut args = vec![rev.to_owned()];
    args.extend(rest.iter().cloned());
    match git.log(&args) {
        Ok(lines) => lines,
        Err(_) if rev != "HEAD" => {
            args[0] = "HEAD".to_owned();
            git.log(&args).unwrap_or_else(|why| {
                tally.failures += 1;
                tally.first_failure.get_or_insert(why);
                Vec::new()
            })
        }
        Err(why) => {
            tally.failures += 1;
            tally.first_failure.get_or_insert(why);
            Vec::new()
        }
    }
}

/// One commit of each list in turn, first list first, each commit once, up to
/// `k`.
pub fn interleave(lists: &[Vec<String>], k: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for at in 0..longest {
        for list in lists {
            if let Some(sha) = list.get(at)
                && seen.insert(sha.clone())
            {
                out.push(sha.clone());
            }
        }
    }
    out.truncate(k);
    out
}

/// The H3 order: commits matched by more tokens first, then newer first (a
/// lower index in `recency`, the tree's `git log` order; unknown last), then
/// by hash.
pub fn rank_by_tokens(
    matches: &[(String, usize)],
    recency: &HashMap<String, usize>,
    k: usize,
) -> Vec<String> {
    let mut ranked: Vec<&(String, usize)> = matches.iter().collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| {
                let at = |sha: &String| recency.get(sha).copied().unwrap_or(usize::MAX);
                at(&a.0).cmp(&at(&b.0))
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .take(k)
        .map(|(sha, _)| sha.clone())
        .collect()
}

/// The commits of the baseline for one question.
///
/// Order, fixed:
/// * H1: per identifier `git log --reverse -E -G '(fn|struct|enum|type|trait|
///   const|static|mod|union)\s+X\b' -- '*.rs'` (oldest first; `-S X` when the
///   regex finds nothing); several identifiers are interleaved first
///   identifier first.
/// * H2: per identifier `git log <sha|HEAD> -S X -- <path>` (newest first);
///   when no identifier finds anything, `git log <sha|HEAD> -- <path>`;
///   several identifiers interleaved.
/// * H3: the union of `--grep t -i` and `-S t` over the tokens, ranked by the
///   number of tokens a commit matches, then newest first (the order of
///   `git log` on HEAD), then hash.
pub fn history_commits(
    git: &dyn GitLog,
    recency: &RefCell<Option<HashMap<String, usize>>>,
    query: &HistoryQuery,
    k: usize,
    tally: &mut Tally,
) -> Vec<String> {
    match query {
        HistoryQuery::Creation { idents } => {
            let lists: Vec<Vec<String>> = idents
                .iter()
                .map(|x| {
                    let regex = format!(
                        "(fn|struct|enum|type|trait|const|static|mod|union)[[:space:]]+{x}([^A-Za-z0-9_]|$)"
                    );
                    let by_decl = ask(
                        git,
                        &["--reverse", "--format=%H", "-E", &format!("-G{regex}"), "--", "*.rs"],
                        tally,
                    );
                    if by_decl.is_empty() {
                        ask(
                            git,
                            &["--reverse", "--format=%H", &format!("-S{x}"), "--", "*.rs"],
                            tally,
                        )
                    } else {
                        by_decl
                    }
                })
                .collect();
            interleave(&lists, k)
        }
        HistoryQuery::LastModified { idents, path, sha } => {
            let rev = sha.as_deref().unwrap_or("HEAD");
            let tail: Vec<String> = match path {
                Some(p) => vec!["--".to_owned(), p.clone()],
                None => Vec::new(),
            };
            let with = |extra: &[String]| -> Vec<String> {
                let mut v: Vec<String> = vec!["--format=%H".to_owned()];
                v.extend(extra.iter().cloned());
                v.extend(tail.iter().cloned());
                v
            };
            let lists: Vec<Vec<String>> = idents
                .iter()
                .map(|x| ask_at(git, rev, &with(&[format!("-S{x}")]), tally))
                .collect();
            let found = interleave(&lists, k);
            if !found.is_empty() || path.is_none() {
                return found;
            }
            let mut plain = ask_at(git, rev, &with(&[]), tally);
            plain.truncate(k);
            plain
        }
        HistoryQuery::Why { tokens } => {
            if tokens.is_empty() {
                return Vec::new();
            }
            let mut counts: Vec<(String, usize)> = Vec::new();
            let mut at: HashMap<String, usize> = HashMap::new();
            for token in tokens {
                let mut hit: BTreeSet<String> = BTreeSet::new();
                hit.extend(ask(
                    git,
                    &["--format=%H", "-i", &format!("--grep={token}")],
                    tally,
                ));
                hit.extend(ask(git, &["--format=%H", &format!("-S{token}")], tally));
                for sha in hit {
                    match at.get(&sha) {
                        Some(&i) => counts[i].1 += 1,
                        None => {
                            at.insert(sha.clone(), counts.len());
                            counts.push((sha, 1));
                        }
                    }
                }
            }
            if recency.borrow().is_none() {
                let order = ask(git, &["--format=%H"], tally)
                    .into_iter()
                    .enumerate()
                    .map(|(i, sha)| (sha, i))
                    .collect();
                *recency.borrow_mut() = Some(order);
            }
            let guard = recency.borrow();
            let empty = HashMap::new();
            rank_by_tokens(&counts, guard.as_ref().unwrap_or(&empty), k)
        }
    }
}

fn run_history(args: HistoryArgs) -> Result<()> {
    let items = select_items(
        Set::History,
        gold::load_gold(&args.gold_dir.join("history.json"))?,
        args.limit,
        None,
    );
    let git = RealGit {
        git: args.git.clone(),
        tree: args.tree.clone(),
        timeout: Duration::from_secs(args.git_timeout_s),
    };
    let recency = RefCell::new(None);
    let mut tally = Tally::default();
    let mut families: HashMap<&'static str, usize> = HashMap::new();
    let mut scored: Vec<(&Value, Vec<Value>)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let question = item["question"].as_str().unwrap_or_default();
        let query = classify(question);
        *families.entry(query.family()).or_default() += 1;
        let commits = history_commits(&git, &recency, &query, args.top_k, &mut tally);
        scored.push((
            item,
            commits.iter().map(|c| json!({ "commit": c })).collect(),
        ));
        if (index + 1) % 10 == 0 {
            eprintln!("history: {}/{} done", index + 1, items.len());
        }
    }
    let dir = args.out_dir.join(slug(&args.wave)).join("history");
    let meta = json!({
        "arm": "git log baseline (baseline history)",
        "gold_aware": false,
        "answers": format!("top {}", args.top_k),
        "order": "H1 oldest first from -G declaration regex (else -S); H2 newest first from -S in the question's path at its sha (else the file log); H3 by tokens matched then newest first; see history_commits",
        "items": items.len(),
        "families_by_wording": families,
        "git_failures": tally.failures,
        "first_git_failure": tally.first_failure,
        "note": "the engine's git.history reads the tree HEAD only; this arm honours the sha in an H2 question, a small edge on those items",
    });
    let expansions = vec![0u64; scored.len()];
    let summary = write_arm_run(&dir, Set::History, &scored, &expansions, &meta)?;
    say("baseline history (git log)", &dir, &summary);
    if tally.failures > 0 {
        eprintln!(
            "warning: {} git calls failed (first: {})",
            tally.failures,
            tally.first_failure.as_deref().unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_maps::scip_graph::{RawDocument, RawOccurrence};
    use scip::types::symbol_information::Kind;

    fn sym(path: &str) -> String {
        format!("rust-analyzer cargo demo 0.1.0 {path}")
    }

    fn occ(
        range: (u32, u32, u32, u32),
        symbol: &str,
        roles: i32,
        enclosing: Option<(u32, u32, u32, u32)>,
    ) -> RawOccurrence {
        RawOccurrence {
            range,
            symbol: symbol.to_owned(),
            roles,
            enclosing,
        }
    }

    /// a.rs: `struct Foo` (line 1), `fn alpha` uses Foo once (line 2),
    /// `fn beta` twice (line 3), `fn gamma` once (line 4), `fn Foo_user`
    /// does not exist; all in one file.
    fn demo() -> ImpactIndex {
        let (foo, alpha, beta, gamma) = (
            sym("a/Foo#"),
            sym("a/alpha()."),
            sym("a/beta()."),
            sym("a/gamma()."),
        );
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 7, 0, 10), &foo, 1, Some((0, 0, 0, 14))),
                occ((1, 3, 1, 8), &alpha, 1, Some((1, 0, 1, 30))),
                occ((1, 12, 1, 15), &foo, 0, None),
                occ((2, 3, 2, 7), &beta, 1, Some((2, 0, 2, 40))),
                occ((2, 12, 2, 15), &foo, 0, None),
                occ((2, 20, 2, 23), &foo, 0, None),
                occ((3, 3, 3, 8), &gamma, 1, Some((3, 0, 3, 30))),
                occ((3, 12, 3, 15), &foo, 0, None),
            ],
        }];
        let kinds: HashMap<String, Kind> = [
            (foo, Kind::Struct),
            (alpha, Kind::Function),
            (beta, Kind::Function),
            (gamma, Kind::Function),
        ]
        .into_iter()
        .collect();
        ImpactIndex::build(docs, &kinds, None)
    }

    #[test]
    fn arm_n_orders_the_closure_by_references_held_and_cuts_to_k() {
        let ix = demo();
        let full = pick_impact(&ix, "What breaks if `Foo` changes?", 0);
        assert!(full.anchored);
        assert_eq!((full.names, full.closure), (1, 3));
        // beta holds two references, alpha and gamma one each (path, line order).
        let lines: Vec<u64> = full
            .answers
            .iter()
            .map(|a| a["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, [3, 2, 4]);
        assert!(full.answers.iter().all(|a| a["path"] == "a.rs"));
        let capped = pick_impact(&ix, "What breaks if `Foo` changes?", 2);
        assert_eq!(capped.closure, 3);
        let lines: Vec<u64> = capped
            .answers
            .iter()
            .map(|a| a["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, [3, 2]);
    }

    #[test]
    fn arm_n_without_a_name_or_an_anchor_answers_nothing() {
        let ix = demo();
        let none = pick_impact(&ix, "What breaks if the parser changes?", 10);
        assert_eq!(
            (none.names, none.anchored, none.answers.len()),
            (0, false, 0)
        );
        let unknown = pick_impact(&ix, "What breaks if `Nowhere` changes?", 10);
        assert_eq!((unknown.names, unknown.anchored), (1, false));
        assert!(unknown.answers.is_empty());
    }

    #[test]
    fn arm_n_run_dir_is_read_by_compare_and_the_scorer() {
        let ix = demo();
        let item = json!({
            "id": "impact-t1", "primary": true,
            "question": "What breaks if `Foo` changes?",
            "stratum": "sym-fn/deg2-4/tests-near",
            "ambiguity": {"band": "some"},
            "seed": {"path": "a.rs", "line": 1, "span": [1, 1], "name": "Foo", "symbol": "sym:foo"},
            "accept": {"neutral_decls": [], "neutral_files": [], "decoys": []},
            "gold": {"r1": {
                "decls": [
                    {"path": "a.rs", "line": 2, "span": [2, 2], "name": "alpha", "bare": "alpha", "symbol": "sym:alpha"},
                    {"path": "a.rs", "line": 3, "span": [3, 3], "name": "beta", "bare": "beta", "symbol": "sym:beta"}
                ],
                "files": ["a.rs"]
            }, "r2": {"decls": []}, "tests": []}
        });
        let pick = pick_impact(&ix, item["question"].as_str().unwrap(), 10);
        let dir = temp_dir("arm-n");
        let scored = vec![(&item, pick.answers)];
        let summary =
            write_arm_run(&dir, Set::Impact, &scored, &[0], &json!({"arm": "test"})).unwrap();
        assert_eq!(summary.n, 1);
        for file in ["run.json", "items.json", "score.json", "meta.json"] {
            assert!(dir.join(file).is_file(), "{file}");
        }
        let data = super::super::compare::load_run(&dir, None).unwrap();
        assert_eq!(data.set, "impact");
        let record = &data.items["impact-t1"];
        assert_eq!(record["decl_recall"], Some(1.0));
        assert_eq!(record["expansions"], Some(0.0));
        assert!(record["f0.5"].is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-br3-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- history

    #[test]
    fn questions_are_told_apart_by_their_wording() {
        let h1 = classify("Which commit first created `RuleList::from_json`?");
        assert_eq!(
            h1,
            HistoryQuery::Creation {
                idents: vec!["from_json".into()]
            }
        );
        let h2 = classify(
            "What is the most recent commit that modified `glob_match` (lci-maps/src/traverse/rules.rs) as of 1a2b3c4d5e?",
        );
        assert_eq!(
            h2,
            HistoryQuery::LastModified {
                idents: vec!["glob_match".into()],
                path: Some("lci-maps/src/traverse/rules.rs".into()),
                sha: Some("1a2b3c4d5e".into()),
            }
        );
        let h3 = classify(
            "Why does the traverse budget reject a larger `max_pops` than the configured cap?",
        );
        let HistoryQuery::Why { tokens } = h3 else {
            panic!("H3 expected");
        };
        assert_eq!(tokens[0], "max_pops");
        assert!(tokens.len() <= MAX_TOKENS);
        assert!(tokens.contains(&"traverse".to_owned()));
        assert!(!tokens.contains(&"because".to_owned()));
        // "first created" with no identifier is free text.
        assert_eq!(classify("When was the cache first created?").family(), "H3");
    }

    #[test]
    fn identifiers_come_from_paths_of_names_only() {
        assert_eq!(
            backtick_idents("`crate::a::Foo<T>::run()` and `x + y` and `Foo` and `src/a.rs`"),
            ["run", "Foo"]
        );
        assert_eq!(sha_token("at deadbeef"), None);
        assert_eq!(sha_token("at cafe1234 now"), Some("cafe1234".into()));
        assert_eq!(path_token("see -rf/x and a/b.rs"), Some("a/b.rs".into()));
    }

    #[test]
    fn interleave_and_rank_are_fixed_orders() {
        let a = vec!["1".to_owned(), "2".to_owned(), "3".to_owned()];
        let b = vec!["2".to_owned(), "9".to_owned()];
        assert_eq!(interleave(&[a, b], 4), ["1", "2", "9", "3"]);
        let recency: HashMap<String, usize> = [("n".to_owned(), 0), ("o".to_owned(), 5)]
            .into_iter()
            .collect();
        let hits = vec![
            ("o".to_owned(), 2),
            ("n".to_owned(), 1),
            ("z".to_owned(), 1),
            ("m".to_owned(), 2),
        ];
        // 2 tokens first (known newer before unknown), then 1 token (newer first).
        assert_eq!(rank_by_tokens(&hits, &recency, 10), ["o", "m", "n", "z"]);
        assert_eq!(rank_by_tokens(&hits, &recency, 2), ["o", "m"]);
    }

    struct Fake(Vec<(&'static str, Vec<&'static str>)>);

    impl GitLog for Fake {
        fn log(&self, args: &[String]) -> Result<Vec<String>, String> {
            let key = args.join(" ");
            self.0
                .iter()
                .find(|(k, _)| key.starts_with(k))
                .map(|(_, v)| v.iter().map(|s| (*s).to_owned()).collect())
                .ok_or_else(|| format!("unexpected call: {key}"))
        }
    }

    #[test]
    fn h2_falls_back_to_the_file_log_and_an_unknown_sha_to_head() {
        let git = Fake(vec![
            ("HEAD --format=%H -Sglob_match -- a/b.rs", vec![]),
            ("HEAD --format=%H -- a/b.rs", vec!["c3", "c2", "c1"]),
        ]);
        let query = HistoryQuery::LastModified {
            idents: vec!["glob_match".into()],
            path: Some("a/b.rs".into()),
            sha: None,
        };
        let mut tally = Tally::default();
        let recency = RefCell::new(None);
        let commits = history_commits(&git, &recency, &query, 2, &mut tally);
        assert_eq!(commits, ["c3", "c2"]);
        assert_eq!(tally.failures, 0);
        // A sha the tree lacks: the call on it fails, HEAD answers.
        let git = Fake(vec![("HEAD --format=%H -Sx -- p.rs", vec!["h1"])]);
        let query = HistoryQuery::LastModified {
            idents: vec!["x".into()],
            path: Some("p.rs".into()),
            sha: Some("deadbee1".into()),
        };
        let commits = history_commits(&git, &recency, &query, 10, &mut tally);
        assert_eq!(commits, ["h1"]);
    }

    #[test]
    fn a_failed_git_call_counts_and_leaves_that_list_empty() {
        let git = Fake(vec![]);
        let query = HistoryQuery::Creation {
            idents: vec!["Foo".into()],
        };
        let mut tally = Tally::default();
        let commits = history_commits(&git, &RefCell::new(None), &query, 10, &mut tally);
        assert!(commits.is_empty());
        // The -G call and the -S fallback both failed.
        assert_eq!(tally.failures, 2);
        assert!(tally.first_failure.unwrap().contains("unexpected call"));
    }

    /// Runs git in `dir` (hooks and signing off), panicking on failure.
    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git on PATH");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn real_git_orders_h1_oldest_first_and_h2_newest_first() {
        let dir = temp_dir("hist");
        git_in(&dir, &["init", "-q"]);
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn alpha() {}\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "add alpha"]);
        std::fs::write(&file, "fn alpha() {}\nfn beta() {}\n").unwrap();
        git_in(
            &dir,
            &["commit", "-q", "-am", "add beta, the reason is speed"],
        );
        std::fs::write(&file, "fn alpha() { alpha_helper(); }\nfn beta() {}\n").unwrap();
        git_in(&dir, &["commit", "-q", "-am", "touch alpha"]);
        let git = RealGit {
            git: PathBuf::from("git"),
            tree: dir.clone(),
            timeout: Duration::from_secs(30),
        };
        let recency = RefCell::new(None);
        let mut tally = Tally::default();
        let all = git.log(&["--format=%H".to_owned()]).unwrap();
        assert_eq!(all.len(), 3);
        let (newest, middle, oldest) = (&all[0], &all[1], &all[2]);

        // H1: `alpha` was created by the oldest commit (the newest rewrites its
        // line, which -G also matches, so it comes second); `beta` by the middle one.
        let q = classify("Which commit first created `alpha`?");
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(&c, &[oldest.clone(), newest.clone()]);
        let q = classify("Which commit first created `beta`?");
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(c, [middle.as_str()]);
        // H2: the count of "alpha" changed in the oldest and the newest commit; newest first.
        let q = classify("What is the most recent commit that modified `alpha` (a.rs)?");
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(&c, &[newest.clone(), oldest.clone()]);
        // H2 cut at the middle commit's sha: only the oldest is before it.
        let q = HistoryQuery::LastModified {
            idents: vec!["alpha".into()],
            path: Some("a.rs".into()),
            sha: Some(middle.clone()),
        };
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(c, [oldest.as_str()]);
        // H3: a message token finds the middle commit.
        let q = HistoryQuery::Why {
            tokens: vec!["reason".into()],
        };
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(c, [middle.as_str()]);
        assert_eq!(tally.failures, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
