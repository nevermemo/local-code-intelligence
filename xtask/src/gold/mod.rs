//! `cargo xtask gold`: scorers for the five local gold sets (impact, flow,
//! where-to, history, instructions) and the trait-implementation set
//! (traits), a validator for the structured-config set (structured), a
//! shipped-baseline reproducer and a converter to the
//! `lci evaluate` definition format. The gold files live
//! outside the repository; nothing here embeds or names their contents.
//! See `docs/spike/multimap/v2/gold-scorers.md` for the run schema and the
//! metric definitions.

mod flow;
mod history;
mod impact;
mod instructions;
pub mod metrics;
pub(crate) mod structured;
mod traits;
mod where_to;

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use lci_core::evaluation::{EvalQuery, EvalWorkspace, EvaluationFile, validate_definition};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Set {
    Impact,
    Flow,
    WhereTo,
    History,
    Instructions,
    Traits,
}

impl Set {
    fn name(self) -> &'static str {
        match self {
            Set::Impact => "impact",
            Set::Flow => "flow",
            Set::WhereTo => "where-to",
            Set::History => "history",
            Set::Instructions => "instructions",
            Set::Traits => "traits",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BaselineKind {
    /// The baseline the gold set ships (its `grep_baseline` field, or the
    /// shortcut baselines its documentation names), as far as the gold file
    /// alone can reproduce it.
    Shipped,
    /// Every item answered with its own gold: a scorer self-check (all
    /// metrics 1.0).
    Oracle,
    /// History only: one constant commit per family (the commit that is
    /// gold for the most items).
    Constant,
}

#[derive(Subcommand)]
pub enum GoldCommand {
    /// Score a run file against a gold set; prints metrics JSON (overall and
    /// per stratum, with n per cell).
    Score {
        #[arg(long, value_enum)]
        set: Set,
        /// Gold file for the set (a JSON array of items).
        #[arg(long)]
        gold: PathBuf,
        /// Run file: {"<item id>": [ranked answers]}.
        #[arg(long)]
        run: PathBuf,
        /// Write the metrics JSON here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Score only items present in the run (default: a missing item is
        /// an empty answer and scores 0).
        #[arg(long)]
        partial: bool,
        /// where-to only: do not treat preferred, documentation, definition
        /// and related sites as neutral.
        #[arg(long)]
        strict: bool,
    },
    /// Build a baseline from the gold file's own fields. Run-shaped
    /// baselines can be fed to `score`; impact's `grep_baseline` stores only
    /// metrics, so it yields a metrics report in the same layout instead.
    Baseline {
        #[arg(long, value_enum)]
        set: Set,
        #[arg(long)]
        gold: PathBuf,
        #[arg(long, value_enum, default_value = "shipped")]
        kind: BaselineKind,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Convert a gold set to an `lci evaluate` definition (TOML, file-level
    /// expected paths). Prints what could not be converted to stderr.
    ToEvaluate {
        #[arg(long, value_enum)]
        set: Set,
        #[arg(long)]
        gold: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Workspace key written into the definition.
        #[arg(long, default_value = "self")]
        workspace: String,
        /// Result window written into every query.
        #[arg(long, default_value_t = 10)]
        top_k: usize,
    },
    /// Validate the structured-config gold files `<gold-dir>/<repo>.json`:
    /// schema, counts, spans against the checked-out trees, key leaks, the
    /// seeded dev/heldout split and portability. Exits non-zero on any
    /// failure. See the `structured` module for the split algorithm.
    ValidateStructured {
        /// Directory holding one `<repo>.json` per repository.
        #[arg(long)]
        gold_dir: PathBuf,
        /// Checkout of a repository as `<repo>=<dir>`; repeat per repository.
        #[arg(long = "tree", value_name = "REPO=DIR")]
        tree: Vec<String>,
        /// Write the report JSON here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

pub fn run(command: GoldCommand) -> Result<()> {
    match command {
        GoldCommand::Score {
            set,
            gold,
            run,
            out,
            partial,
            strict,
        } => {
            let items = load_gold(&gold)?;
            let run = load_run(&run)?;
            let report = score_set(set, &items, &run, partial, strict)?;
            emit(&report, out.as_deref())
        }
        GoldCommand::Baseline {
            set,
            gold,
            kind,
            out,
        } => {
            let items = load_gold(&gold)?;
            let (output, note) = baseline(set, &items, kind)?;
            eprintln!("{note}");
            emit(&output, out.as_deref())
        }
        GoldCommand::ToEvaluate {
            set,
            gold,
            out,
            workspace,
            top_k,
        } => {
            let items = load_gold(&gold)?;
            let (toml_text, summary) = to_evaluate(set, &items, &workspace, top_k)?;
            write_file(&out, &toml_text)?;
            eprintln!("{summary}");
            Ok(())
        }
        GoldCommand::ValidateStructured {
            gold_dir,
            tree,
            out,
        } => structured::run(&gold_dir, &tree, out.as_deref()),
    }
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

fn emit(value: &Value, out: Option<&Path>) -> Result<()> {
    let text = serde_json::to_string_pretty(value)? + "\n";
    match out {
        Some(path) => write_file(path, &text),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ loading

/// A gold file is a JSON array of items. A file of one JSON item per line (the
/// `traits` question files are) is read as well: when the text is not one JSON
/// value, or the extension is `.jsonl`, every non-empty line is an item.
pub fn load_gold(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read gold file {}", path.display()))?;
    let lines = || -> Result<Value> {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array)
            .map_err(Into::into)
    };
    let value: Value = if path.extension().is_some_and(|e| e == "jsonl") {
        lines().with_context(|| format!("gold file {} is not valid JSON lines", path.display()))?
    } else {
        match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => lines()
                .map_err(|_| error)
                .with_context(|| format!("gold file {} is not valid JSON", path.display()))?,
        }
    };
    let Value::Array(items) = value else {
        bail!("gold file {} must be a JSON array of items", path.display());
    };
    for (i, item) in items.iter().enumerate() {
        if item.get("id").and_then(Value::as_str).is_none() {
            bail!("gold item {i} has no string `id`");
        }
    }
    Ok(items)
}

/// The items of a set as the scorers and the bench want them. Only `traits`
/// changes: its Q1 items get the set's dominant macro call site, which needs the
/// whole gold (see `traits::annotate`). Call it before any item is dropped.
pub fn prepare(set: Set, items: Vec<Value>) -> Vec<Value> {
    match set {
        Set::Traits => traits::annotate(items),
        _ => items,
    }
}

/// Run file: item id -> ranked answers.
pub type Run = BTreeMap<String, Vec<Answer>>;

pub fn load_run(path: &Path) -> Result<Run> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read run file {}", path.display()))?;
    parse_run(&text).with_context(|| format!("invalid run file {}", path.display()))
}

pub fn parse_run(text: &str) -> Result<Run> {
    let value: Value = serde_json::from_str(text).context("run is not valid JSON")?;
    let Value::Object(map) = value else {
        bail!("a run must be a JSON object mapping item id to a ranked array of answers");
    };
    let mut run = Run::new();
    for (id, answers) in map {
        let Value::Array(list) = answers else {
            bail!("run entry {id:?} must be an array");
        };
        let parsed = list
            .iter()
            .enumerate()
            .map(|(i, v)| Answer::from_value(v).with_context(|| format!("run entry {id:?}[{i}]")))
            .collect::<Result<Vec<_>>>()?;
        run.insert(id, parsed);
    }
    Ok(run)
}

/// One ranked answer. Every field is optional; each set reads the ones its
/// gold uses. A bare string is shorthand: `path`, `path:line`,
/// `path:start-end`, a symbol or function name, or (history) a commit hash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub path: Option<String>,
    pub line: Option<i64>,
    pub end_line: Option<i64>,
    pub name: Option<String>,
    pub symbol: Option<String>,
    pub commit: Option<String>,
    /// History only: files the answer claims the commit touched.
    pub files: Vec<String>,
}

impl Answer {
    pub fn from_value(value: &Value) -> Result<Answer> {
        match value {
            Value::String(s) => Ok(Answer::parse_shorthand(s)),
            Value::Object(map) => {
                let text = |keys: &[&str]| {
                    keys.iter()
                        .find_map(|k| map.get(*k).and_then(Value::as_str))
                        .map(str::to_string)
                };
                let number = |key: &str| map.get(key).and_then(Value::as_i64);
                let line = number("line").or_else(|| number("start_line"));
                Ok(Answer {
                    path: text(&["path", "doc", "file"]),
                    line,
                    end_line: number("end_line").or(line),
                    name: text(&["name"]),
                    symbol: text(&["symbol"]),
                    commit: text(&["commit"]),
                    files: map
                        .get("files")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            }
            other => bail!("an answer must be a string or an object, got {other}"),
        }
    }

    pub fn parse_shorthand(s: &str) -> Answer {
        let s = s.trim();
        let mut answer = Answer::default();
        if (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()) {
            answer.commit = Some(s.to_string());
        }
        if let Some((head, tail)) = s.rsplit_once(':') {
            let range = tail.split_once('-').unwrap_or((tail, tail));
            if let (Ok(a), Ok(b)) = (range.0.parse::<i64>(), range.1.parse::<i64>())
                && !head.is_empty()
            {
                answer.path = Some(head.to_string());
                answer.line = Some(a);
                answer.end_line = Some(b);
                return answer;
            }
        }
        let path_like = s.contains('/')
            || (s.contains('.')
                && !s.contains("::")
                && !s.contains(char::is_whitespace)
                && !s.contains('('));
        if path_like {
            answer.path = Some(s.to_string());
        } else {
            answer.name = Some(s.to_string());
        }
        answer
    }
}

// ------------------------------------------------------------ json helpers

pub(crate) fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn arr_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub(crate) fn strings_of(v: &Value, key: &str) -> Vec<String> {
    arr_of(v, key)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

pub(crate) fn span_of(v: &Value) -> Option<(i64, i64)> {
    let span = v.get("span").and_then(Value::as_array)?;
    Some((span.first()?.as_i64()?, span.get(1)?.as_i64()?))
}

pub(crate) fn dedup_keep_order(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    items
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

// ----------------------------------------------------------------- scoring

/// Scores for one item: metric -> value (`None` = not defined for this item)
/// and the strata the item belongs to (dimension -> value).
pub struct ItemScore {
    pub metrics: Vec<(&'static str, Option<f64>)>,
    pub dims: Vec<(&'static str, String)>,
}

#[derive(Default)]
struct Cell {
    n: usize,
    sums: BTreeMap<&'static str, (f64, usize)>,
}

impl Cell {
    fn add(&mut self, item: &ItemScore) {
        self.n += 1;
        for (name, value) in &item.metrics {
            let entry = self.sums.entry(*name).or_insert((0.0, 0));
            if let Some(v) = value {
                entry.0 += v;
                entry.1 += 1;
            }
        }
    }

    /// `{"n": items, "metrics": {name: mean}, "n_defined": {name: k}}`;
    /// `n_defined` lists only metrics defined on fewer items than `n`.
    fn to_json(&self) -> Value {
        let mut metrics = Map::new();
        let mut short = Map::new();
        for (name, (sum, k)) in &self.sums {
            if *k > 0 {
                metrics.insert((*name).to_string(), json!(round4(sum / *k as f64)));
            } else {
                metrics.insert((*name).to_string(), Value::Null);
            }
            if *k != self.n {
                short.insert((*name).to_string(), json!(k));
            }
        }
        let mut cell = Map::new();
        cell.insert("n".into(), json!(self.n));
        cell.insert("metrics".into(), Value::Object(metrics));
        if !short.is_empty() {
            cell.insert("n_defined".into(), Value::Object(short));
        }
        Value::Object(cell)
    }
}

pub(crate) struct Aggregate {
    overall: Cell,
    strata: BTreeMap<&'static str, BTreeMap<String, Cell>>,
}

impl Aggregate {
    pub(crate) fn new() -> Self {
        Aggregate {
            overall: Cell::default(),
            strata: BTreeMap::new(),
        }
    }

    pub(crate) fn add(&mut self, item: &ItemScore) {
        self.overall.add(item);
        for (dim, value) in &item.dims {
            self.strata
                .entry(*dim)
                .or_default()
                .entry(value.clone())
                .or_default()
                .add(item);
        }
    }

    pub(crate) fn n(&self) -> usize {
        self.overall.n
    }

    pub(crate) fn to_json(&self) -> (Value, Value) {
        let strata = self
            .strata
            .iter()
            .map(|(dim, cells)| {
                let cells: Map<String, Value> = cells
                    .iter()
                    .map(|(k, c)| (k.clone(), c.to_json()))
                    .collect();
                ((*dim).to_string(), Value::Object(cells))
            })
            .collect::<Map<_, _>>();
        (self.overall.to_json(), Value::Object(strata))
    }
}

pub struct Options {
    pub strict: bool,
}

fn score_item(set: Set, item: &Value, answers: &[Answer], opts: &Options) -> ItemScore {
    match set {
        Set::Impact => impact::score_item(item, answers),
        Set::Flow => flow::score_item(item, answers),
        Set::WhereTo => where_to::score_item(item, answers, opts),
        Set::History => history::score_item(item, answers),
        Set::Instructions => instructions::score_item(item, answers),
        Set::Traits => traits::score_item(item, answers),
    }
}

/// The strata (dimension -> value) of one item of `set`, the ones its scorer
/// reports under `strata`. They come from the item alone, so they do not depend
/// on the answers; the bench groups costs by them.
pub fn dims(set: Set, item: &Value) -> Vec<(&'static str, String)> {
    score_item(set, item, &[], &Options { strict: false }).dims
}

/// Items a set leaves out of its scored population (impact keeps `primary`).
fn is_scored(set: Set, item: &Value) -> bool {
    match set {
        Set::Impact => impact::is_primary(item),
        _ => true,
    }
}

const TRAITS_NO_DECL: &str = "no trait_decl spans: the trait declaration card counts as a miss";

pub fn score_set(
    set: Set,
    items: &[Value],
    run: &Run,
    partial: bool,
    strict: bool,
) -> Result<Value> {
    let opts = Options { strict };
    let prepared;
    let items = if set == Set::Traits {
        prepared = prepare(set, items.to_vec());
        prepared.as_slice()
    } else {
        items
    };
    let ids: std::collections::BTreeSet<&str> = items
        .iter()
        .filter_map(|i| i.get("id").and_then(Value::as_str))
        .collect();
    let unknown: Vec<&String> = run.keys().filter(|k| !ids.contains(k.as_str())).collect();
    let mut aggregate = Aggregate::new();
    let (mut missing, mut excluded) = (0usize, 0usize);
    for item in items {
        if !is_scored(set, item) {
            excluded += 1;
            continue;
        }
        let id = str_of(item, "id").unwrap_or_default();
        let answers = match run.get(id) {
            Some(a) => a.as_slice(),
            None => {
                missing += 1;
                if partial {
                    continue;
                }
                &[]
            }
        };
        aggregate.add(&score_item(set, item, answers, &opts));
    }
    let (overall, strata) = aggregate.to_json();
    let mut report = json!({
        "set": set.name(),
        "gold_items": items.len(),
        "excluded_from_population": excluded,
        "scored_items": aggregate.n(),
        "items_missing_from_run": missing,
        "missing_scored_as_empty": !partial,
        "run_ids_not_in_gold": unknown,
        "strict": strict,
        "overall": overall,
        "strata": strata,
    });
    if set == Set::Traits {
        let notes = traits::report_notes(items);
        if notes["items_with_trait_decl"] == 0 {
            // once per process: the bench scores item by item
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| eprintln!("warning: {TRAITS_NO_DECL}"));
            report["warnings"] = json!([TRAITS_NO_DECL]);
        }
        report["traits"] = notes;
    }
    Ok(report)
}

// --------------------------------------------------------------- baselines

/// Returns the baseline (a run, or for impact a metrics report) and a note
/// saying what it is and what it cannot reproduce.
pub fn baseline(set: Set, items: &[Value], kind: BaselineKind) -> Result<(Value, String)> {
    if kind == BaselineKind::Oracle {
        let mut run = Map::new();
        for item in items {
            let id = str_of(item, "id").unwrap_or_default().to_string();
            run.insert(id, Value::Array(oracle_answers(set, item)));
        }
        return Ok((
            Value::Object(run),
            format!(
                "{}: oracle run (every item answered with its own gold); score it to self-check the scorer",
                set.name()
            ),
        ));
    }
    match set {
        Set::Impact => impact::stored_grep_baseline(items),
        Set::History => history::baseline(items, kind),
        Set::Instructions => instructions::baseline(items),
        Set::Flow | Set::WhereTo | Set::Traits => bail!(
            "the {} gold ships no baseline field; use --kind oracle for a scorer self-check",
            set.name()
        ),
    }
}

fn oracle_answers(set: Set, item: &Value) -> Vec<Value> {
    match set {
        Set::Impact => impact::oracle(item),
        Set::Flow => flow::oracle(item),
        Set::WhereTo => where_to::oracle(item),
        Set::History => history::oracle(item),
        Set::Instructions => instructions::oracle(item),
        Set::Traits => traits::oracle(item),
    }
}

// ------------------------------------------------------------- to-evaluate

/// File-level expectations for one `lci evaluate` query.
pub struct Converted {
    pub expected: Vec<String>,
    pub preferred: Vec<String>,
    pub disfavored: Vec<String>,
    pub tags: Vec<String>,
}

/// Why an item cannot become an evaluate query.
pub struct Dropped(pub &'static str);

/// The files an item's gold expects (what `to-evaluate` writes as
/// `expected_paths`), with the reason when the item has none. For the
/// bench-only, gold-aware arms of `maps` (`oracle-policy`, `oracle-filter`).
pub fn expected_files(set: Set, item: &Value) -> Result<Vec<String>, &'static str> {
    let converted = match set {
        Set::Impact => impact::to_query(item),
        Set::Flow => flow::to_query(item),
        Set::WhereTo => where_to::to_query(item),
        Set::History => history::to_query(item),
        Set::Instructions => instructions::to_query(item),
        Set::Traits => traits::to_query(item),
    };
    converted.map(|c| c.expected).map_err(|Dropped(why)| why)
}

pub fn to_evaluate(
    set: Set,
    items: &[Value],
    workspace: &str,
    top_k: usize,
) -> Result<(String, String)> {
    let mut queries = Vec::new();
    let mut dropped: BTreeMap<&'static str, usize> = BTreeMap::new();
    for item in items {
        let converted = match set {
            Set::Impact => impact::to_query(item),
            Set::Flow => flow::to_query(item),
            Set::WhereTo => where_to::to_query(item),
            Set::History => history::to_query(item),
            Set::Instructions => instructions::to_query(item),
            Set::Traits => traits::to_query(item),
        };
        match converted {
            Ok(c) => {
                let mut tags = vec![set.name().to_string()];
                tags.extend(c.tags);
                // Instruction gold names rule documents, which the search
                // returns in its separate `docs` list; score that list.
                if matches!(set, Set::Instructions) {
                    tags.push(lci_core::evaluation::EvalLane::DOCS_TAG.to_string());
                }
                queries.push(EvalQuery {
                    query: str_of(item, "question").unwrap_or_default().to_string(),
                    workspace: workspace.to_string(),
                    top_k: Some(top_k),
                    filters: None,
                    expected_paths: c.expected,
                    preferred_paths: c.preferred,
                    disfavored_paths: c.disfavored,
                    expected_roles: Vec::new(),
                    required_snippet: None,
                    tags: dedup_keep_order(tags),
                });
            }
            Err(Dropped(reason)) => *dropped.entry(reason).or_default() += 1,
        }
    }
    let definition = EvaluationFile {
        workspaces: vec![EvalWorkspace {
            key: workspace.to_string(),
            required: true,
        }],
        queries,
    };
    validate_definition(&definition).context("converted definition is invalid")?;
    let header = format!(
        "# `lci evaluate` definition converted from the `{}` gold set by `cargo xtask gold to-evaluate`.\n\
         # File-level expectations only: ordering, commits, sections, symbols and as-of\n\
         # history are not expressible here. See docs/spike/multimap/v2/gold-scorers.md.\n\n",
        set.name()
    );
    let body = toml::to_string_pretty(&definition).context("failed to render TOML")?;
    let mut summary = format!(
        "{}: converted {} of {} items",
        set.name(),
        definition.queries.len(),
        items.len()
    );
    for (reason, n) in &dropped {
        summary.push_str(&format!("\n  dropped {n}: {reason}"));
    }
    summary.push_str(&format!("\n  note: {}", note_for(set)));
    Ok((header + &body, summary))
}

fn note_for(set: Set) -> &'static str {
    match set {
        Set::Impact => {
            "expected_paths = R1 files; disfavored = planted decoy files; declaration-level gold, R2, tests and docs are not converted"
        }
        Set::Flow => {
            "expected_paths = files of the gold intermediates (config-flow: field and readers, dispatch: plus fan-out); hop order, tau and spurious nodes are lost"
        }
        Set::WhereTo => {
            "expected_paths = expected registration files, exemplar member files or convention implementation files; declaration spans, neutral sites and precision are lost"
        }
        Set::History => {
            "expected_paths = gold.files; the commit (the real target) and the as-of cut-off cannot be expressed, and evaluate searches the checked-out tree"
        }
        Set::Instructions => {
            "expected_paths = rule documents; sections and units are lost; tagged lane:docs, so the query is scored on the search's separate docs list (Markdown documents only)"
        }
        Set::Traits => {
            "expected_paths = files of the impl blocks and implementing type declarations (Q1) or of the member locations (Q2, Q3); spans, kinds, the budget and the trait declaration are lost"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_strings_parse_by_shape() {
        let a = Answer::parse_shorthand("src/a.rs:12");
        assert_eq!(
            (a.path.as_deref(), a.line, a.end_line),
            (Some("src/a.rs"), Some(12), Some(12))
        );
        let a = Answer::parse_shorthand("AGENTS.md:5-9");
        assert_eq!(
            (a.path.as_deref(), a.line, a.end_line),
            (Some("AGENTS.md"), Some(5), Some(9))
        );
        let a = Answer::parse_shorthand("App::references");
        assert_eq!(a.name.as_deref(), Some("App::references"));
        assert!(a.path.is_none());
        let a = Answer::parse_shorthand("0123abcdef");
        assert_eq!(a.commit.as_deref(), Some("0123abcdef"));
        assert_eq!(
            Answer::parse_shorthand("Cargo.toml").path.as_deref(),
            Some("Cargo.toml")
        );
    }

    #[test]
    fn run_files_parse_strings_and_objects() {
        let run =
            parse_run(r#"{"x-1": ["a/b.rs:3", {"path": "c.rs", "name": "f", "files": ["z.rs"]}]}"#)
                .unwrap();
        let answers = &run["x-1"];
        assert_eq!(answers[0].line, Some(3));
        assert_eq!(answers[1].name.as_deref(), Some("f"));
        assert_eq!(answers[1].files, vec!["z.rs".to_string()]);
        assert!(parse_run("[1]").is_err());
        assert!(parse_run(r#"{"x": 3}"#).is_err());
        assert!(parse_run(r#"{"x": [3]}"#).is_err());
    }

    #[test]
    fn cells_report_n_and_partially_defined_metrics() {
        let mut agg = Aggregate::new();
        agg.add(&ItemScore {
            metrics: vec![("a", Some(1.0)), ("b", None)],
            dims: vec![("d", "x".into())],
        });
        agg.add(&ItemScore {
            metrics: vec![("a", Some(0.0)), ("b", Some(0.5))],
            dims: vec![("d", "x".into())],
        });
        let (overall, strata) = agg.to_json();
        assert_eq!(overall["n"], 2);
        assert_eq!(overall["metrics"]["a"], 0.5);
        assert_eq!(overall["metrics"]["b"], 0.5);
        assert_eq!(overall["n_defined"]["b"], 1);
        assert_eq!(strata["d"]["x"]["n"], 2);
    }

    #[test]
    fn missing_items_score_as_empty_unless_partial() {
        let items = vec![json!({
            "id": "h-1", "stratum": "H1/x",
            "gold": {"commit": "abcdef1234", "files": []}, "accept": {}
        })];
        let run = Run::new();
        let full = score_set(Set::History, &items, &run, false, false).unwrap();
        assert_eq!(full["scored_items"], 1);
        assert_eq!(full["overall"]["metrics"]["mrr"], 0.0);
        let partial = score_set(Set::History, &items, &run, true, false).unwrap();
        assert_eq!(partial["scored_items"], 0);
        assert_eq!(partial["items_missing_from_run"], 1);
    }

    #[test]
    fn gold_files_may_be_json_lines() {
        let dir = std::env::temp_dir().join(format!("lci-gold-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lines = dir.join("traits.json");
        std::fs::write(
            &lines,
            "{\"id\": \"a\"}

{\"id\": \"b\"}
",
        )
        .unwrap();
        assert_eq!(load_gold(&lines).unwrap().len(), 2);
        let array = dir.join("array.json");
        std::fs::write(&array, "[{\"id\": \"a\"}]").unwrap();
        assert_eq!(load_gold(&array).unwrap().len(), 1);
        let one = dir.join("one.jsonl");
        std::fs::write(
            &one,
            "{\"id\": \"a\"}
",
        )
        .unwrap();
        assert_eq!(load_gold(&one).unwrap().len(), 1);
        let bad = dir.join("bad.json");
        std::fs::write(
            &bad,
            "{\"id\": \"a\"}
not json
",
        )
        .unwrap();
        assert!(load_gold(&bad).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn traits_q2_means_exclude_items_with_only_cfg_gated_gold() {
        let location = json!({"file": "a.rs", "begin_line": 1, "end_line": 3});
        let items = vec![
            json!({"id": "active", "set": "Q2", "gold": [{"location": location}]}),
            json!({"id": "gated", "set": "Q2", "cfg_gated": [location],
                   "gold": [{"location": location}]}),
        ];
        let run = parse_run(r#"{"active": ["a.rs:1"], "gated": ["a.rs:1"]}"#).unwrap();
        let report = score_set(Set::Traits, &items, &run, false, false).unwrap();
        for cell in [&report["overall"], &report["strata"]["template"]["Q2"]] {
            assert_eq!(cell["n"], 2);
            for name in ["hit@1", "hit@10", "mrr"] {
                assert_eq!(cell["metrics"][name], 1.0, "{name}");
                assert_eq!(cell["n_defined"][name], 1, "{name}");
            }
        }
    }

    #[test]
    fn traits_report_carries_the_dominant_site_and_the_optional_fields() {
        let span = |f: &str, a: i64, b: i64| json!({"file": f, "begin_line": a, "end_line": b});
        let entry = |id: &str, kind: &str, f: &str| json!({"impl_id": id, "stratum": kind, "impl": span(f, 1, 2), "type_decl": null});
        let items = vec![json!({
            "id": "q1", "set": "Q1", "trait_decl": span("tr.rs", 9, 30),
            "gold": [entry("i0", "macro", "m.rs"), entry("i1", "macro", "m.rs"), entry("i2", "explicit", "e.rs")]
        })];
        let run = parse_run(
            r#"{"q1": [{"path": "m.rs", "line": 1, "symbol": "n0"}, "e.rs:1", "tr.rs:10"]}"#,
        )
        .unwrap();
        let report = score_set(Set::Traits, &items, &run, false, false).unwrap();
        assert_eq!(report["set"], "traits");
        assert_eq!(report["strata"]["template"]["Q1"]["n"], 1);
        assert_eq!(report["traits"]["dominant_site"]["file"], "m.rs");
        assert_eq!(report["traits"]["items_with_trait_decl"], 1);
        assert_eq!(report["traits"]["items_with_cfg_gated"], 0);
        assert!(report.get("warnings").is_none());
        // no trait_decl anywhere: the report says the declaration card counts as a miss
        let mut bare = items.clone();
        bare[0].as_object_mut().unwrap().remove("trait_decl");
        let report = score_set(Set::Traits, &bare, &run, false, false).unwrap();
        assert_eq!(report["warnings"], json!([TRAITS_NO_DECL]));
        // the dominant site's two gold entries leave one: k = 1, found by "e.rs:1"
        assert_eq!(report["overall"]["metrics"]["nodom_recall"], 1.0);
        // the oracle run scores 1.0
        let (oracle, _) = baseline(Set::Traits, &items, BaselineKind::Oracle).unwrap();
        let run = parse_run(&oracle.to_string()).unwrap();
        let report = score_set(Set::Traits, &items, &run, false, false).unwrap();
        assert_eq!(report["overall"]["metrics"]["set_f1"], 1.0);
        assert!(baseline(Set::Traits, &items, BaselineKind::Shipped).is_err());
    }

    #[test]
    fn converted_definition_loads_as_an_evaluation_file() {
        let items = vec![
            json!({
                "id": "w-1", "question": "where?", "stratum": "registration/clean/single-file",
                "accept": {"decoys": [{"path": "d.rs"}]},
                "gold": {"form": "registration", "expected_files": ["r.rs"]}
            }),
            json!({
                "id": "w-2", "question": "nothing", "stratum": "exemplar/clean",
                "accept": {}, "gold": {"form": "exemplar", "members": []}
            }),
        ];
        let (text, summary) = to_evaluate(Set::WhereTo, &items, "self", 7).unwrap();
        let parsed: EvaluationFile = toml::from_str(&text).unwrap();
        validate_definition(&parsed).unwrap();
        assert_eq!(parsed.queries.len(), 1);
        assert_eq!(parsed.queries[0].top_k, Some(7));
        assert_eq!(parsed.queries[0].expected_paths, vec!["r.rs"]);
        assert!(summary.contains("dropped 1: no gold files"));
    }
}
