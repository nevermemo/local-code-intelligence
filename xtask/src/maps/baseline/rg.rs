//! `cargo xtask maps baseline rg`: the rg-only baseline arm for the `traits`
//! and `where-to` sets.
//!
//! Per item: needles from the question alone (no gold) -> one `rg` run over
//! the frozen tree, fixed strings and whole words -> matched lines -> answers
//! `{path, line}` in a fixed order -> [`write_arm_run`]. No walk, no SCIP, no
//! index, no server, no model service: `expansions` and `model_calls` are 0.
//!
//! Needles are the last path segment of every backticked name of the question
//! (at least two characters, at most [`MAX_NEEDLES`], question order, matched
//! case-sensitively). A question with no usable backticked name falls back to
//! `lexical::terms(question)` cut to the same number (matched
//! case-insensitively); neither -> no answer, counted in `meta.json`.
//!
//! Order (also written to `meta.json["order"]`):
//!
//! * `traits`: one answer per matched line, by distinct needles hit (most
//!   first), then path, then line. `--decl-first` puts lines with a
//!   declaration keyword before the first needle ahead of the rest.
//! * `where-to`: one answer per file (the file's best line by the same order),
//!   files by matched lines (most first), then the most needles any one of
//!   its lines holds, then path.
//!
//! `--rule regex` (traits only) reproduces the rule of the earlier `rg_arm.py`
//! driver (d392) instead: one `rg --sort path --glob *.rs` call per question
//! with an impl-header regex and a derive regex built from the trait name the
//! question gives (templates Q1/Q2/Q3, read from the question text), Q1 by
//! line, Q2/Q3 by the first `fn member` at depth 1 of each impl block, Q2
//! ranked by the type named after `for`. `meta.json["rule"]` names the wave's
//! rule: `fixed`, `fixed-decl-first` or `regex`. rg has no total-output cap
//! or timeout beyond the fixed rule's per-file `--max-count`.
//!
//! A baseline record has no K3a-1 cost keys (`external_calls`, `lsp_ms`,
//! `answer_tokens`, `budget_hit`): the arm has no walk, so `maps compare`
//! pairs no item on them, which is right.

use super::{say, select_items, set_name, slug, write_arm_run};
use crate::gold::{self, Set};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use lci_core::{language, lexical};
use lci_maps::impact::code_names;
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Needles per question: the same rule as the search channel's literal pass.
pub(super) const MAX_NEEDLES: usize = 4;

/// Words that, before the first needle on a line, mark it as a declaration.
const DECL_KEYWORDS: &[&str] = &[
    "fn",
    "trait",
    "struct",
    "enum",
    "impl",
    "type",
    "mod",
    "class",
    "interface",
    "def",
    "func",
    "union",
    "record",
    "protocol",
];

#[derive(Clone, Copy, ValueEnum)]
pub enum RgSet {
    Traits,
    WhereTo,
}

#[derive(Args)]
pub struct RgArgs {
    /// The set to answer: `traits` or `where-to`.
    #[arg(long, value_enum)]
    set: RgSet,
    /// Directory holding traits.json / where-to.json (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The frozen eval tree rg searches (read only).
    #[arg(long)]
    tree: PathBuf,
    /// Answers per item, the bench's `--top-k`; 0 keeps every answer.
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/<set>`.
    /// Default `baseline-rg`, or `baseline-rg-decl` with `--decl-first`.
    #[arg(long)]
    wave: Option<String>,
    /// Run only the first N items.
    #[arg(long)]
    limit: Option<usize>,
    /// The rg binary (resolved like the lexical channel's `ripgrep_path`).
    #[arg(long, default_value = "rg")]
    rg: String,
    /// Put lines with a declaration keyword before the first needle ahead of
    /// the other lines (rg as an agent reads it); the plain order is rg as
    /// typed. Run both as two waves.
    #[arg(long)]
    decl_first: bool,
    /// rg `--max-count`: matched lines kept per file.
    #[arg(long, default_value_t = 50)]
    max_per_file: usize,
    /// The rule: `fixed` (fixed-string whole-word needles, the default) or
    /// `regex` (traits only: one call with an impl-header regex and a derive
    /// regex built from the trait name, answers by the three trait question
    /// templates; the rule of the earlier `rg_arm.py` driver, d392).
    #[arg(long, value_enum, default_value_t = RgRule::Fixed)]
    rule: RgRule,
    /// `--rule regex` only: answers per Q1/Q3 item (Q2 is cut to --top-k).
    #[arg(long, default_value_t = 40)]
    max_answers: usize,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RgRule {
    Fixed,
    Regex,
}

/// Where an item's needles came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NeedleSource {
    Backticked,
    Fallback,
    None,
}

/// One line rg matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RgLine {
    /// Relative to the tree, `/` separators.
    pub path: String,
    pub line: u64,
    /// Distinct needles the line holds.
    pub needles: u32,
    /// A declaration keyword occurs before the first needle.
    pub decl: bool,
}

/// The needles of a question, from the question only.
pub(super) fn needles_of(question: &str) -> (Vec<String>, NeedleSource) {
    let (names, _) = code_names(question);
    let mut needles: Vec<String> = Vec::new();
    for path in &names {
        if let Some(last) = path.last().filter(|s| s.chars().count() >= 2)
            && !needles.contains(last)
        {
            needles.push(last.clone());
        }
    }
    needles.truncate(MAX_NEEDLES);
    if !needles.is_empty() {
        return (needles, NeedleSource::Backticked);
    }
    let mut terms = lexical::terms(question);
    terms.truncate(MAX_NEEDLES);
    if terms.is_empty() {
        (terms, NeedleSource::None)
    } else {
        (terms, NeedleSource::Fallback)
    }
}

/// The file globs of every language adapter, once each.
pub(super) fn globs() -> Vec<&'static str> {
    let mut globs: Vec<&'static str> = language::ADAPTERS.iter().map(|a| a.glob()).collect();
    globs.sort_unstable();
    globs.dedup();
    globs
}

/// One rg run over `tree`: fixed strings, whole words. Exit 0 and 1 are
/// answers; anything else is an error carrying rg's stderr.
pub(super) fn run_rg(
    rg: &Path,
    tree: &Path,
    needles: &[String],
    case_sensitive: bool,
    max_per_file: usize,
) -> Result<Vec<RgLine>> {
    let mut command = Command::new(rg);
    command
        .current_dir(tree)
        .args([
            "--json",
            "--line-number",
            "--fixed-strings",
            "--word-regexp",
            "--max-count",
        ])
        .arg(max_per_file.max(1).to_string());
    if !case_sensitive {
        command.arg("--ignore-case");
    }
    for glob in globs() {
        command.arg("--glob").arg(glob);
    }
    for needle in needles {
        command.arg("-e").arg(needle);
    }
    command.arg("--").arg(".");
    let output = command
        .output()
        .with_context(|| format!("start ripgrep at {}", rg.display()))?;
    match output.status.code() {
        Some(0 | 1) => {}
        Some(code) => {
            let tail = &output.stderr[output.stderr.len().saturating_sub(512)..];
            bail!(
                "ripgrep exited with code {code}: {}",
                String::from_utf8_lossy(tail).trim()
            );
        }
        None => bail!("ripgrep terminated without an exit code: {}", output.status),
    }
    parse_rg_json(&output.stdout, needles, case_sensitive)
}

/// Whether a declaration keyword is a whole word of `prefix`.
fn has_decl_keyword(prefix: &str) -> bool {
    prefix
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| DECL_KEYWORDS.contains(&word))
}

/// The `match` events of `rg --json` output as [`RgLine`]s. Other events
/// (`begin`, `end`, `context`, `summary`) are ignored. A line's `needles` is
/// the number of distinct needles among its submatches (compared lowercase
/// unless `case_sensitive`).
pub(super) fn parse_rg_json(
    stdout: &[u8],
    needles: &[String],
    case_sensitive: bool,
) -> Result<Vec<RgLine>> {
    let key = |s: &str| {
        if case_sensitive {
            s.to_owned()
        } else {
            s.to_lowercase()
        }
    };
    let wanted: Vec<String> = needles.iter().map(|n| key(n)).collect();
    let mut lines = Vec::new();
    for raw in stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
    {
        let event: Value = serde_json::from_slice(raw).context("invalid ripgrep JSON")?;
        if event["type"] != "match" {
            continue;
        }
        let data = &event["data"];
        // A line that is not valid UTF-8 arrives as `bytes` only: skipped.
        let (Some(path), Some(line), Some(text)) = (
            data["path"]["text"].as_str(),
            data["line_number"].as_u64(),
            data["lines"]["text"].as_str(),
        ) else {
            continue;
        };
        let mut hit = BTreeSet::new();
        let mut first = usize::MAX;
        for sub in data["submatches"].as_array().into_iter().flatten() {
            let Some(matched) = sub["match"]["text"].as_str() else {
                continue;
            };
            let matched = key(matched);
            if let Some(index) = wanted.iter().position(|w| *w == matched) {
                hit.insert(index);
            }
            let start = sub["start"].as_u64().map_or(0, |s| s as usize);
            first = first.min(start);
        }
        let first = if first == usize::MAX { 0 } else { first };
        let mut path = path.replace('\\', "/");
        while let Some(rest) = path.strip_prefix("./") {
            path = rest.to_owned();
        }
        lines.push(RgLine {
            path,
            line,
            // rg matched the line for some needle even when the text compare
            // above could not tell which (case folding corner cases).
            needles: hit.len().max(1) as u32,
            decl: text.get(..first).is_some_and(has_decl_keyword),
        });
    }
    Ok(lines)
}

/// The fixed line order: declarations first when asked, then distinct needles
/// (most first), then path, then line.
fn line_order(a: &RgLine, b: &RgLine, decl_first: bool) -> Ordering {
    let decl = if decl_first {
        b.decl.cmp(&a.decl)
    } else {
        Ordering::Equal
    };
    decl.then(b.needles.cmp(&a.needles))
        .then_with(|| a.path.cmp(&b.path))
        .then(a.line.cmp(&b.line))
}

fn answer(line: &RgLine) -> Value {
    json!({ "path": line.path, "line": line.line })
}

fn cut<T>(mut v: Vec<T>, top_k: usize) -> Vec<T> {
    if top_k > 0 {
        v.truncate(top_k);
    }
    v
}

/// `traits`: one answer per line in [`line_order`], cut to `top_k` (0 = no cut).
pub(super) fn rank_traits(lines: &[RgLine], top_k: usize, decl_first: bool) -> Vec<Value> {
    let mut sorted: Vec<&RgLine> = lines.iter().collect();
    sorted.sort_by(|a, b| line_order(a, b, decl_first));
    sorted.dedup_by(|a, b| a.path == b.path && a.line == b.line);
    cut(sorted.into_iter().map(answer).collect(), top_k)
}

/// `where-to`: one answer per file, the file's best line by [`line_order`];
/// files by matched lines (most first), then the most needles on any of its
/// lines, then path. Cut to `top_k` (0 = no cut).
pub(super) fn rank_where_to(lines: &[RgLine], top_k: usize, decl_first: bool) -> Vec<Value> {
    let mut files: BTreeMap<&str, Vec<&RgLine>> = BTreeMap::new();
    for line in lines {
        files.entry(line.path.as_str()).or_default().push(line);
    }
    let mut ranked: Vec<(usize, u32, &str, &RgLine)> = files
        .into_iter()
        .filter_map(|(path, group)| {
            let most = group.iter().map(|l| l.needles).max()?;
            let best = group
                .iter()
                .min_by(|a, b| line_order(a, b, decl_first))
                .copied()?;
            Some((group.len(), most, path, best))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    cut(ranked.into_iter().map(|r| answer(r.3)).collect(), top_k)
}

pub(super) fn answers_for(
    set: Set,
    lines: &[RgLine],
    top_k: usize,
    decl_first: bool,
) -> Vec<Value> {
    match set {
        Set::WhereTo => rank_where_to(lines, top_k, decl_first),
        _ => rank_traits(lines, top_k, decl_first),
    }
}

fn order_text(set: Set, decl_first: bool) -> String {
    let lines = "distinct needles hit desc, then path, then line";
    let lines = if decl_first {
        format!("declaration lines first, then {lines}")
    } else {
        lines.to_owned()
    };
    match set {
        Set::WhereTo => format!(
            "one answer per file (its best line: {lines}); files by matched lines desc, then max needles on a line desc, then path"
        ),
        _ => format!("one answer per line: {lines}"),
    }
}

// ------------------------------------------------- the regex rule (traits)

/// Which trait question template a question is (read from its text, never
/// from the gold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Template {
    /// "Which types implement `Trait`?"
    Q1,
    /// "Where is `Trait::member` implemented for `Type`?"
    Q2,
    /// "What must change if `Trait::member` changes?"
    Q3,
}

/// The names a trait question gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RegexQuery {
    pub template: Template,
    pub trait_name: String,
    pub member: Option<String>,
    pub ty: Option<String>,
}

/// Reads the three trait question templates from the question text (backticked
/// names via `code_names`); `None` when the text matches none of them.
pub(super) fn regex_query(question: &str) -> Option<RegexQuery> {
    let text = question.trim();
    if !text.ends_with('?') {
        return None;
    }
    let (names, _) = code_names(text);
    let member_of = |path: &Vec<String>| -> Option<(String, String)> {
        let [.., t, m] = path.as_slice() else {
            return None;
        };
        Some((t.clone(), m.clone()))
    };
    if text.starts_with("Which types implement") && names.len() == 1 {
        return Some(RegexQuery {
            template: Template::Q1,
            trait_name: names[0].last()?.clone(),
            member: None,
            ty: None,
        });
    }
    if text.starts_with("Where is") && text.contains(" implemented for ") && names.len() == 2 {
        let (trait_name, member) = member_of(&names[0])?;
        return Some(RegexQuery {
            template: Template::Q2,
            trait_name,
            member: Some(member),
            ty: Some(names[1].last()?.clone()),
        });
    }
    if text.starts_with("What must change if") && names.len() == 1 {
        let (trait_name, member) = member_of(&names[0])?;
        return Some(RegexQuery {
            template: Template::Q3,
            trait_name,
            member: Some(member),
            ty: None,
        });
    }
    None
}

/// Escapes a name for a regex: every character that is not a word character
/// gets a backslash.
pub(super) fn regex_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if !(c.is_alphanumeric() || c == '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The two patterns of the rule, built from the trait name only: an impl
/// header on one line, and a derive list naming the trait.
pub(super) fn regex_patterns(trait_name: &str) -> (String, String) {
    let t = regex_escape(trait_name);
    (
        format!(r"\bimpl\b[^{{;]*\b{t}\b[^{{;]*\bfor\b"),
        format!(r"#\[derive\([^)]*\b{t}\b"),
    )
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `\bword\b` occurs in `hay`.
pub(super) fn has_word(hay: &str, word: &str) -> bool {
    hay.match_indices(word).any(|(at, _)| {
        !hay[..at].chars().next_back().is_some_and(is_word)
            && !hay[at + word.len()..].chars().next().is_some_and(is_word)
    })
}

/// `#\[derive\([^)]*\bTRAIT\b` matches somewhere in `text`.
pub(super) fn is_derive_line(text: &str, trait_name: &str) -> bool {
    text.match_indices("#[derive(").any(|(at, m)| {
        let rest = &text[at + m.len()..];
        has_word(rest.split(')').next().unwrap_or_default(), trait_name)
    })
}

/// `\bfor\b[^{]*\bTYPE\b` matches in an impl header line.
fn type_after_for(text: &str, ty: &str) -> bool {
    text.match_indices("for").any(|(at, m)| {
        let before = text[..at].chars().next_back();
        let after = text[at + m.len()..].chars().next();
        if before.is_some_and(is_word) || after.is_some_and(is_word) {
            return false;
        }
        has_word(
            text[at + m.len()..].split('{').next().unwrap_or_default(),
            ty,
        )
    })
}

/// `\bfn\s+MEMBER\b` matches in `line`.
fn has_fn_member(line: &str, member: &str) -> bool {
    line.match_indices("fn").any(|(at, m)| {
        if line[..at].chars().next_back().is_some_and(is_word) {
            return false;
        }
        let rest = &line[at + m.len()..];
        let trimmed = rest.trim_start();
        trimmed.len() < rest.len()
            && trimmed.starts_with(member)
            && !trimmed[member.len()..].chars().next().is_some_and(is_word)
    })
}

/// 1-based line of `fn member` at brace depth 1 of the block opened at or
/// after the 0-based `header0`, or `None`; looks at most `budget` lines.
pub(super) fn member_line(
    lines: &[String],
    header0: usize,
    member: &str,
    budget: usize,
) -> Option<u64> {
    let (mut depth, mut opened) = (0i64, false);
    for (i, text) in lines.iter().enumerate().skip(header0).take(budget) {
        if opened && depth == 1 && has_fn_member(text, member) {
            return Some(i as u64 + 1);
        }
        for c in text.chars() {
            match c {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return None;
        }
    }
    None
}

/// One line the header or derive pattern matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HeaderHit {
    pub path: String,
    pub line: u64,
    pub text: String,
    pub derive: bool,
}

/// The `match` events of `rg --json` output as [`HeaderHit`]s.
pub(super) fn parse_header_hits(stdout: &[u8], trait_name: &str) -> Result<Vec<HeaderHit>> {
    let mut hits = Vec::new();
    for raw in stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
    {
        let event: Value = serde_json::from_slice(raw).context("invalid ripgrep JSON")?;
        if event["type"] != "match" {
            continue;
        }
        let data = &event["data"];
        let (Some(path), Some(line), Some(text)) = (
            data["path"]["text"].as_str(),
            data["line_number"].as_u64(),
            data["lines"]["text"].as_str(),
        ) else {
            continue;
        };
        let mut path = path.replace('\\', "/");
        while let Some(rest) = path.strip_prefix("./") {
            path = rest.to_owned();
        }
        hits.push(HeaderHit {
            path,
            line,
            text: text.trim_end_matches(['\r', '\n']).to_owned(),
            derive: is_derive_line(text, trait_name),
        });
    }
    Ok(hits)
}

/// One rg call over `tree` for the rule's two patterns (`*.rs` only).
pub(super) fn run_rg_headers(rg: &Path, tree: &Path, trait_name: &str) -> Result<Vec<HeaderHit>> {
    let (header, derive) = regex_patterns(trait_name);
    let output = Command::new(rg)
        .current_dir(tree)
        .args([
            "--json",
            "--line-number",
            "--sort",
            "path",
            "--glob",
            "*.rs",
        ])
        .arg("-e")
        .arg(&header)
        .arg("-e")
        .arg(&derive)
        .arg("--")
        .arg(".")
        .output()
        .with_context(|| format!("start ripgrep at {}", rg.display()))?;
    match output.status.code() {
        Some(0 | 1) => {}
        Some(code) => {
            let tail = &output.stderr[output.stderr.len().saturating_sub(512)..];
            bail!(
                "ripgrep exited with code {code}: {}",
                String::from_utf8_lossy(tail).trim()
            );
        }
        None => bail!("ripgrep terminated without an exit code: {}", output.status),
    }
    parse_header_hits(&output.stdout, trait_name)
}

/// Finds the member line of the impl block that opens at a header line:
/// `(path, header_line, member)`.
pub(super) type MemberAt<'a> = dyn FnMut(&str, u64, &str) -> Result<Option<u64>> + 'a;

const REGEX_ORDER: &str = "Q1: every header or derive line by path, line; Q3: the first `fn member` at brace depth 1 of each impl block (derive lines and headers without it give nothing) by path, line; Q2: as Q3, headers naming the type after `for` first, then path, line";

/// Lines a member search may scan below an impl header.
const MEMBER_BUDGET: usize = 2000;

/// The rule's answers. Q1: every matched line (header or derive) in path/line
/// order, cut to `max_answers`. Q3: the first `fn member` at depth 1 of each
/// header's block (derive lines and headers without it give nothing), in
/// path/line order, cut to `max_answers`. Q2: as Q3, headers whose text names
/// the type after `for` first, then path/line, cut to `q2_cut`.
/// `member_at(path, header_line, member)` finds the member line of a block
/// (the driver reads the file; tests pass an in-memory tree).
pub(super) fn rank_regex(
    query: &RegexQuery,
    hits: &[HeaderHit],
    member_at: &mut MemberAt,
    max_answers: usize,
    q2_cut: usize,
) -> Result<Vec<Value>> {
    let mut sorted: Vec<&HeaderHit> = hits.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    let place = |path: &str, line: u64| json!({ "path": path, "line": line });
    if query.template == Template::Q1 {
        return Ok(cut(
            sorted.iter().map(|h| place(&h.path, h.line)).collect(),
            max_answers,
        ));
    }
    let member = query.member.as_deref().unwrap_or_default();
    let mut candidates: Vec<(u8, &str, u64)> = Vec::new();
    for hit in sorted.iter().filter(|h| !h.derive) {
        if let Some(line) = member_at(&hit.path, hit.line, member)? {
            let names_type = query
                .ty
                .as_deref()
                .is_some_and(|ty| type_after_for(&hit.text, ty));
            candidates.push((u8::from(!names_type), hit.path.as_str(), line));
        }
    }
    let (keep, cap) = if query.template == Template::Q2 {
        candidates.sort();
        (candidates, q2_cut)
    } else {
        candidates.sort_by(|a, b| a.1.cmp(b.1).then(a.2.cmp(&b.2)));
        (candidates, max_answers)
    };
    Ok(cut(
        keep.iter()
            .map(|(_, path, line)| place(path, *line))
            .collect(),
        cap,
    ))
}

/// `rg --version`, first line.
fn rg_version(rg: &Path) -> Result<String> {
    let output = Command::new(rg)
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "start ripgrep at {}; install rg, add it to PATH, or pass --rg",
                rg.display()
            )
        })?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned())
}

/// The set's gold items as the bench reads them: prepared (traits gets its
/// `dominant_site`, so `nodom_*` scores exist) before the `--limit` cut.
pub(super) fn load_items(set: Set, gold_dir: &Path, limit: Option<usize>) -> Result<Vec<Value>> {
    let items = gold::load_gold(&gold_dir.join(format!("{}.json", set_name(set))))?;
    Ok(select_items(set, gold::prepare(set, items), limit, None))
}

pub(super) fn run_rg_arm(args: RgArgs) -> Result<()> {
    let set = match args.set {
        RgSet::Traits => Set::Traits,
        RgSet::WhereTo => Set::WhereTo,
    };
    if args.rule == RgRule::Regex && (set != Set::Traits || args.decl_first) {
        bail!("--rule regex answers the traits set only and takes no --decl-first");
    }
    let rule = match (args.rule, args.decl_first) {
        (RgRule::Regex, _) => "regex",
        (RgRule::Fixed, true) => "fixed-decl-first",
        (RgRule::Fixed, false) => "fixed",
    };
    if !args.tree.is_dir() {
        bail!("--tree {} is not a directory", args.tree.display());
    }
    let rg = lexical::resolve_ripgrep_path(&args.rg);
    let version = rg_version(&rg)?;
    let items = load_items(set, &args.gold_dir, args.limit)?;
    let (mut backticked, mut fallback, mut none) = (0usize, 0usize, 0usize);
    let (mut failures, mut first_failure, mut empty) = (0usize, None::<String>, 0usize);
    let mut answers: Vec<Vec<Value>> = Vec::with_capacity(items.len());
    let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        let question = item["question"].as_str().unwrap_or_default();
        let outcome: Result<Option<Vec<Value>>> = if args.rule == RgRule::Regex {
            // Needles are the question's trait name (and member, type).
            match regex_query(question) {
                None => {
                    none += 1;
                    Ok(None)
                }
                Some(query) => {
                    backticked += 1;
                    run_rg_headers(&rg, &args.tree, &query.trait_name).and_then(|hits| {
                        let mut member_at =
                            |path: &str, header: u64, member: &str| -> Result<Option<u64>> {
                                if !file_cache.contains_key(path) {
                                    let text = std::fs::read(args.tree.join(path))
                                        .with_context(|| format!("read {path}"))?;
                                    let lines = String::from_utf8_lossy(&text)
                                        .lines()
                                        .map(str::to_owned)
                                        .collect();
                                    file_cache.insert(path.to_owned(), lines);
                                }
                                Ok(member_line(
                                    &file_cache[path],
                                    (header as usize).saturating_sub(1),
                                    member,
                                    MEMBER_BUDGET,
                                ))
                            };
                        rank_regex(&query, &hits, &mut member_at, args.max_answers, args.top_k)
                            .map(Some)
                    })
                }
            }
        } else {
            let (needles, source) = needles_of(question);
            match source {
                NeedleSource::Backticked => backticked += 1,
                NeedleSource::Fallback => fallback += 1,
                NeedleSource::None => none += 1,
            }
            if source == NeedleSource::None {
                Ok(None)
            } else {
                let case_sensitive = source == NeedleSource::Backticked;
                run_rg(&rg, &args.tree, &needles, case_sensitive, args.max_per_file)
                    .map(|lines| Some(answers_for(set, &lines, args.top_k, args.decl_first)))
            }
        };
        let picked = match outcome {
            Ok(found) => found.unwrap_or_default(),
            Err(e) => {
                failures += 1;
                first_failure.get_or_insert_with(|| format!("{e:#}"));
                Vec::new()
            }
        };
        empty += usize::from(picked.is_empty());
        answers.push(picked);
        if (index + 1) % 10 == 0 {
            eprintln!("rg: {}/{} done", index + 1, items.len());
        }
    }
    let scored: Vec<(&Value, Vec<Value>)> = items.iter().zip(answers).collect();
    let wave = args.wave.clone().unwrap_or_else(|| {
        match rule {
            "regex" => "baseline-rg-regex",
            "fixed-decl-first" => "baseline-rg-decl",
            _ => "baseline-rg",
        }
        .to_owned()
    });
    let dir = args.out_dir.join(slug(&wave)).join(set_name(set));
    let cap = if args.top_k == 0 {
        "uncapped".to_owned()
    } else {
        format!("top {}", args.top_k)
    };
    let regex = args.rule == RgRule::Regex;
    let (arm, order, answers_text) = if regex {
        (
            "rg baseline (regex rule of rg_arm.py)",
            REGEX_ORDER.to_owned(),
            format!("Q1/Q3 up to {}, Q2 top {}", args.max_answers, args.top_k),
        )
    } else {
        (
            "rg baseline (fixed strings, whole word)",
            order_text(set, args.decl_first),
            cap,
        )
    };
    let meta = json!({
        "arm": arm,
        "rule": rule,
        "gold_aware": false,
        "answers": answers_text,
        "order": order,
        "needles": { "backticked": backticked, "fallback": fallback, "none": none },
        "rg": rg.display().to_string(),
        "rg_version": version,
        "rg_failures": failures,
        "first_rg_failure": first_failure,
        "items_with_empty_answer": empty,
        "max_per_file": if regex { Value::Null } else { json!(args.max_per_file) },
        "decl_first": args.decl_first,
        "items": items.len(),
        "note": if regex {
            "names come from the question's template only (trait, member, type); no index, no SCIP, no model service"
        } else {
            "needles come from the question only (backticked names, else lexical terms); no index, no SCIP, no model service"
        },
    });
    let zeros = vec![0u64; scored.len()];
    let summary = write_arm_run(&dir, set, &scored, &zeros, &zeros, &meta)?;
    say(&format!("baseline rg ({})", set_name(set)), &dir, &summary);
    eprintln!(
        "needles: backticked {backticked}, fallback {fallback}, none {none}; empty answers {empty}"
    );
    if failures > 0 {
        eprintln!(
            "warning: {failures} rg runs failed (first: {})",
            first_failure.as_deref().unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(path: &str, line: u64, needles: u32, decl: bool) -> RgLine {
        RgLine {
            path: path.to_owned(),
            line,
            needles,
            decl,
        }
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

    #[test]
    fn needles_prefer_backticked_names_then_fall_back_to_terms() {
        let (needles, source) = needles_of("Which types implement `shapes::Shape`?");
        assert_eq!(
            (needles, source),
            (vec!["Shape".into()], NeedleSource::Backticked)
        );

        let (needles, source) = needles_of("where do config readers register");
        assert_eq!(source, NeedleSource::Fallback);
        assert!(
            !needles.is_empty() && needles.len() <= MAX_NEEDLES,
            "{needles:?}"
        );
        assert!(needles.iter().all(|n| *n == n.to_lowercase()));

        let (needles, source) = needles_of("`a`");
        assert_eq!((needles.len(), source), (0, NeedleSource::None));

        // deduped by last segment, question order, at most four
        let (needles, source) = needles_of("`a::Foo` `b::Foo` `Bar` `Baz` `Qux` `Quux`");
        assert_eq!(source, NeedleSource::Backticked);
        assert_eq!(needles, ["Foo", "Bar", "Baz", "Qux"]);
    }

    #[test]
    fn parse_rg_json_counts_distinct_needles_and_marks_declarations() {
        let json = r#"{"type":"begin","data":{"path":{"text":"./src\\a.rs"}}}
{"type":"match","data":{"path":{"text":"./src\\a.rs"},"lines":{"text":"impl Shape for Circle {\n"},"line_number":3,"absolute_offset":10,"submatches":[{"match":{"text":"Shape"},"start":5,"end":10},{"match":{"text":"Circle"},"start":15,"end":21}]}}
{"type":"context","data":{"path":{"text":"./src\\a.rs"},"lines":{"text":"    // Shape\n"},"line_number":4,"absolute_offset":40,"submatches":[]}}
{"type":"match","data":{"path":{"text":"./src\\a.rs"},"lines":{"text":"    let s = Shape::new();\n"},"line_number":9,"absolute_offset":60,"submatches":[{"match":{"text":"Shape"},"start":12,"end":17}]}}
{"type":"end","data":{"path":{"text":"./src\\a.rs"},"binary_offset":null,"stats":{}}}
{"type":"summary","data":{"elapsed_total":{"human":"0.001s"},"stats":{}}}
"#;
        let needles = vec!["Shape".to_owned(), "Circle".to_owned()];
        let lines = parse_rg_json(json.as_bytes(), &needles, true).unwrap();
        assert_eq!(
            lines,
            [line("src/a.rs", 3, 2, true), line("src/a.rs", 9, 1, false)]
        );
        // case-insensitive runs compare lowercase
        let lower = vec!["shape".to_owned()];
        let lines = parse_rg_json(json.as_bytes(), &lower, false).unwrap();
        assert_eq!(lines[1].needles, 1);
        assert!(parse_rg_json(b"not json\n", &needles, true).is_err());
    }

    #[test]
    fn traits_answers_rank_by_needles_then_path_and_decl_first_moves_declarations() {
        let lines = vec![
            line("b.rs", 4, 1, false),
            line("a.rs", 9, 1, false),
            line("a.rs", 2, 1, false),
            line("z.rs", 1, 2, false),
            line("c.rs", 7, 1, true),
        ];
        let plain = rank_traits(&lines, 10, false);
        assert_eq!(
            places(&plain),
            [
                at("z.rs", 1),
                at("a.rs", 2),
                at("a.rs", 9),
                at("b.rs", 4),
                at("c.rs", 7)
            ]
        );
        let decl = rank_traits(&lines, 10, true);
        assert_eq!(
            places(&decl),
            [
                at("c.rs", 7),
                at("z.rs", 1),
                at("a.rs", 2),
                at("a.rs", 9),
                at("b.rs", 4)
            ]
        );
        assert_eq!(rank_traits(&lines, 2, false).len(), 2);
        assert_eq!(rank_traits(&lines, 0, false).len(), 5);
        assert!(rank_traits(&[], 10, false).is_empty());
    }

    #[test]
    fn where_to_answers_are_one_per_file_by_matched_lines() {
        let lines = vec![
            line("b.rs", 5, 1, false),
            line("a.rs", 1, 1, false),
            line("a.rs", 8, 2, false),
            line("c.rs", 3, 1, false),
            line("c.rs", 4, 1, false),
            line("c.rs", 6, 1, true),
            line("d.rs", 2, 2, false),
        ];
        // c has 3 lines, a 2, d 1 (max needles 2), b 1 (max needles 1)
        let plain = rank_where_to(&lines, 10, false);
        assert_eq!(
            places(&plain),
            [at("c.rs", 3), at("a.rs", 8), at("d.rs", 2), at("b.rs", 5)]
        );
        // --decl-first picks the declaration line as the file's answer
        let decl = rank_where_to(&lines, 10, true);
        assert_eq!(places(&decl)[0], at("c.rs", 6));
        assert_eq!(rank_where_to(&lines, 2, false).len(), 2);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-k5a-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rg_arm_run_dir_is_read_by_compare_and_the_scorer() {
        let span =
            |file: &str, a: i64, b: i64| json!({"file": file, "begin_line": a, "end_line": b});
        let item = json!({
            "id": "t-Q2-0", "set": "Q2", "stratum": "explicit",
            "question": "Where is `Tr::go` implemented for `Circle`?",
            "trait": "x::Tr", "trait_decl": span("tr.rs", 100, 140),
            "gold": [{"impl_id": "i", "stratum": "explicit",
                      "location": span("m.rs", 10, 14), "impl": span("m.rs", 8, 20)}]
        });
        let lines = vec![line("m.rs", 12, 1, false), line("other.rs", 3, 2, false)];
        let answers = rank_traits(&lines, 10, false);
        assert_eq!(places(&answers), [at("other.rs", 3), at("m.rs", 12)]);
        let dir = temp_dir("rg-arm");
        let scored = vec![(&item, answers)];
        let summary = write_arm_run(
            &dir,
            Set::Traits,
            &scored,
            &[0],
            &[0],
            &json!({"arm": "test"}),
        )
        .unwrap();
        assert_eq!(summary.n, 1);
        for file in ["run.json", "items.json", "score.json", "meta.json"] {
            assert!(dir.join(file).is_file(), "{file}");
        }
        let data = crate::maps::compare::load_run(&dir, None).unwrap();
        assert_eq!(data.set, "traits");
        let record = &data.items["t-Q2-0"];
        assert_eq!(record["hit@10"], Some(1.0));
        assert_eq!(record["hit@1"], Some(0.0));
        assert_eq!(record["expansions"], Some(0.0));
        assert_eq!(record["model_calls"], Some(0.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn regex_rule_reads_the_templates_and_builds_the_patterns_from_the_trait_name() {
        let q1 = regex_query("Which types implement `shapes::Shape`?").unwrap();
        assert_eq!(
            (q1.template, q1.trait_name.as_str(), q1.member, q1.ty),
            (Template::Q1, "Shape", None, None)
        );
        let q2 = regex_query("Where is `shapes::Shape::area` implemented for `Circle`?").unwrap();
        assert_eq!(
            (
                q2.template,
                q2.trait_name.as_str(),
                q2.member.as_deref(),
                q2.ty.as_deref()
            ),
            (Template::Q2, "Shape", Some("area"), Some("Circle"))
        );
        let q3 = regex_query("What must change if `Shape::area` changes?").unwrap();
        assert_eq!(
            (q3.template, q3.trait_name.as_str(), q3.member.as_deref()),
            (Template::Q3, "Shape", Some("area"))
        );
        // a member needs a trait before it; other wordings are not templates
        assert!(regex_query("What must change if `area` changes?").is_none());
        assert!(regex_query("where do config readers register").is_none());

        let (header, derive) = regex_patterns("Shape");
        assert_eq!(header, r"\bimpl\b[^{;]*\bShape\b[^{;]*\bfor\b");
        assert_eq!(derive, r"#\[derive\([^)]*\bShape\b");
        assert_eq!(
            regex_patterns("a.b").0,
            r"\bimpl\b[^{;]*\ba\.b\b[^{;]*\bfor\b"
        );

        // the derive test is the derive pattern; whole words only
        assert!(is_derive_line("#[derive(Debug, Shape)]", "Shape"));
        assert!(!is_derive_line("#[derive(Debug, ShapeExt)]", "Shape"));
        assert!(!is_derive_line("#[derive(Debug)] // Shape", "Shape"));
        assert!(!is_derive_line("impl Shape for Circle {", "Shape"));
        assert!(type_after_for("impl Shape for Circle {", "Circle"));
        assert!(!type_after_for(
            "impl<T: Circle> Shape for Square {",
            "Circle"
        ));
        assert!(!type_after_for("impl Shape for CircleX {", "Circle"));
    }

    fn hit(path: &str, line: u64, text: &str, derive: bool) -> HeaderHit {
        HeaderHit {
            path: path.to_owned(),
            line,
            text: text.to_owned(),
            derive,
        }
    }

    #[test]
    fn regex_rule_member_search_and_ranking() {
        let source: Vec<String> = [
            "impl Shape for Square {",
            "    fn name(&self) -> &str { \"sq\" }",
            "    fn area(&self) -> f64 {",
            "        1.0",
            "    }",
            "}",
            "impl Shape for Circle {",
            "    fn side(&self) {}",
            "}",
            "impl Shape for Disc {",
            "    fn area(&self) -> f64 {",
            "        fn area() {}",
            "    }",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // depth 1 only; the header line itself opens the block
        assert_eq!(member_line(&source, 0, "area", 2000), Some(3));
        assert_eq!(member_line(&source, 6, "area", 2000), None);
        assert_eq!(member_line(&source, 9, "area", 2000), Some(11));
        assert_eq!(member_line(&source, 0, "area", 2), None);
        assert!(has_fn_member("    pub fn area(&self)", "area"));
        assert!(!has_fn_member("    fn areas(&self)", "area"));
        assert!(!has_fn_member("    fnarea", "area"));

        let hits = vec![
            hit("b.rs", 1, "impl Shape for Square {", false),
            hit("a.rs", 1, "impl Shape for Circle {", false),
            hit("a.rs", 4, "impl Shape for Disc {", false),
            hit("a.rs", 9, "#[derive(Shape)]", true),
        ];
        // b.rs holds Square's block; a.rs holds Circle (no area) and Disc
        let mut member_at = |path: &str, header: u64, member: &str| -> Result<Option<u64>> {
            let lines = if path == "b.rs" {
                &source[0..6]
            } else {
                &source[6..]
            };
            let start = (header as usize).saturating_sub(1);
            Ok(member_line(lines, start, member, 2000))
        };
        let q = |template, ty: Option<&str>| RegexQuery {
            template,
            trait_name: "Shape".into(),
            member: Some("area".into()),
            ty: ty.map(String::from),
        };
        // Q1: every header or derive line, path then line
        let q1 = RegexQuery {
            member: None,
            ..q(Template::Q1, None)
        };
        let all = rank_regex(&q1, &hits, &mut member_at, 40, 10).unwrap();
        assert_eq!(
            places(&all),
            [at("a.rs", 1), at("a.rs", 4), at("a.rs", 9), at("b.rs", 1)]
        );
        assert_eq!(
            rank_regex(&q1, &hits, &mut member_at, 2, 10).unwrap().len(),
            2
        );
        // Q3: headers with the member, derives skipped, path then line
        let q3 = rank_regex(&q(Template::Q3, None), &hits, &mut member_at, 40, 10).unwrap();
        assert_eq!(places(&q3), [at("a.rs", 5), at("b.rs", 3)]);
        // Q2: the header naming the type first, then path/line; cut by q2_cut
        let q2 = rank_regex(
            &q(Template::Q2, Some("Square")),
            &hits,
            &mut member_at,
            40,
            10,
        )
        .unwrap();
        assert_eq!(places(&q2), [at("b.rs", 3), at("a.rs", 5)]);
        let q2 = rank_regex(
            &q(Template::Q2, Some("Square")),
            &hits,
            &mut member_at,
            40,
            1,
        )
        .unwrap();
        assert_eq!(places(&q2), [at("b.rs", 3)]);
    }

    #[test]
    fn regex_rule_parses_rg_json_hits_and_marks_derives() {
        let json = r##"{"type":"begin","data":{"path":{"text":"./src\\a.rs"}}}
{"type":"match","data":{"path":{"text":"./src\\a.rs"},"lines":{"text":"impl Shape for Circle {\n"},"line_number":3,"absolute_offset":10,"submatches":[]}}
{"type":"match","data":{"path":{"text":"./src\\a.rs"},"lines":{"text":"#[derive(Clone, Shape)]\n"},"line_number":9,"absolute_offset":60,"submatches":[]}}
{"type":"summary","data":{"stats":{}}}
"##;
        let hits = parse_header_hits(json.as_bytes(), "Shape").unwrap();
        assert_eq!(
            hits,
            [
                hit("src/a.rs", 3, "impl Shape for Circle {", false),
                hit("src/a.rs", 9, "#[derive(Clone, Shape)]", true)
            ]
        );
    }

    #[test]
    fn traits_gold_is_prepared_so_nodom_metrics_exist() {
        let span =
            |file: &str, a: i64, b: i64| json!({"file": file, "begin_line": a, "end_line": b});
        let entry = |id: &str, stratum: &str, file: &str, a: i64, b: i64| json!({"impl_id": id, "stratum": stratum, "impl": span(file, a, b), "type_decl": null});
        let gold = json!([{
            "id": "t-Q1-0", "set": "Q1", "stratum": "macro",
            "question": "Which types implement `x::Tr`?", "trait": "x::Tr",
            "gold": [
                entry("a", "macro", "m.rs", 5, 5),
                entry("b", "macro", "m.rs", 5, 5),
                entry("c", "explicit", "e.rs", 20, 30)
            ]
        }]);
        let dir = temp_dir("rg-prepare");
        std::fs::write(dir.join("traits.json"), gold.to_string()).unwrap();
        let items = load_items(Set::Traits, &dir, None).unwrap();
        assert_eq!(items[0]["dominant_site"]["file"], "m.rs");
        let answers = rank_traits(&[line("e.rs", 22, 1, false)], 10, false);
        let scored = vec![(&items[0], answers)];
        let out = dir.join("out");
        write_arm_run(&out, Set::Traits, &scored, &[0], &[0], &json!({})).unwrap();
        let data = crate::maps::compare::load_run(&out, None).unwrap();
        let record = &data.items["t-Q1-0"];
        assert_eq!(record["nodom_recall"], Some(1.0));
        assert!(record["nodom_f1"].is_some());
        let score = std::fs::read_to_string(out.join("score.json")).unwrap();
        let score: Value = serde_json::from_str(&score).unwrap();
        assert!(!score.to_string().contains("\"dominant_site\":null"));
        assert_eq!(score["traits"]["dominant_site"]["file"], "m.rs");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
