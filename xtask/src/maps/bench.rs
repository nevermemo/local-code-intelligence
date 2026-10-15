//! `cargo xtask maps bench`: run `lci traverse` once per gold question, score
//! the answers with the gold scorers (in-process) and append one trend row per
//! (wave, set).
//!
//! The runner only calls the CLI, so whatever seeds the traversal today (the
//! name-token stand-in, later the hybrid search of S6b) is what is measured.
//! The gold items are read for their `question` only; the seed, the answer and
//! every other gold field are never given to the traversal.
//!
//! A traversal answers with graph nodes (`sym:...` ids). Each set scores a
//! different answer type, so a small adapter per set turns the nodes into the
//! run-file answers the scorer reads (declarations by path and line, files,
//! commits, document paths). A symbol id carries its module path but neither
//! its file nor its impl type, so a declaration is identified by the `site`
//! (path and line of the definition) the result card carries, which is what
//! the scorers match on (a line inside the gold span, or the gold's own
//! `file:line`). A symbol with no site is not offered: its id names a module
//! path, not a file, and the cards of every symbol result carry a site. When no selected map can emit the node kind a set needs (commits for
//! history, document sections for instructions), the row says so
//! (`structural-zero`) instead of reporting zeros.
//!
//! `--rescore <dir>` scores the raw outputs of an earlier run again (no
//! traversal, no CLI) with the adapters as they are now.

use super::trend::{self, NCOLS, TrendRow};
use crate::gold::{self, Answer, Set};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use lci_core::node::{NodeId, NodeKind, SymbolKind};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where the trend file lives, relative to the repository root.
const DEFAULT_TREND: &str = "docs/spike/multimap/v2/trend.md";
const NO_MAP: &str = "no map yields this answer type";
/// The row note of a set that no selected map can answer.
const STRUCTURAL_ZERO: &str = "structural-zero";
/// A progress line goes to stderr after this many items.
const PROGRESS_EVERY: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SetArg {
    Impact,
    Flow,
    WhereTo,
    History,
    Instructions,
    All,
}

impl SetArg {
    fn sets(self) -> Vec<Set> {
        match self {
            SetArg::Impact => vec![Set::Impact],
            SetArg::Flow => vec![Set::Flow],
            SetArg::WhereTo => vec![Set::WhereTo],
            SetArg::History => vec![Set::History],
            SetArg::Instructions => vec![Set::Instructions],
            SetArg::All => vec![
                Set::Impact,
                Set::Flow,
                Set::WhereTo,
                Set::History,
                Set::Instructions,
            ],
        }
    }
}

#[derive(Args)]
pub struct BenchArgs {
    /// Which gold set to run (all = the five, in order).
    #[arg(long, value_enum)]
    set: SetArg,
    /// Directory holding impact.json, flow.json, where-to.json, history.json
    /// and instructions.json (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The SCIP index of the frozen tree (not needed with --rescore).
    #[arg(long)]
    scip: Option<PathBuf>,
    /// The frozen tree the index describes (not needed with --rescore).
    #[arg(long)]
    tree: Option<PathBuf>,
    /// Maps to use, comma separated (default: the maps the CLI turns on when
    /// it is not told, read from a probe call; with --rescore, name the maps
    /// the run used).
    #[arg(long, value_delimiter = ',')]
    maps: Vec<String>,
    /// A steering policy file for `lci traverse --policy`, or `default` for
    /// none.
    #[arg(long, default_value = "default")]
    policy: String,
    /// How results are ranked (`lci traverse --fusion`): `boost` is the CLI's
    /// own default and passes nothing; `rrf` is the weighted reciprocal rank
    /// fusion. The row's note says when it is `rrf`.
    #[arg(long, default_value = "boost", value_parser = ["boost", "rrf"])]
    fusion: String,
    /// Pass through connective nodes (`lci traverse --pass-through`): files,
    /// commits and config keys are expanded without costing a pop. The row's
    /// note says when it is on.
    #[arg(long)]
    pass_through: bool,
    /// Switch the score bound off (`lci traverse --no-score-bound`). The row's
    /// note says `no-bound`.
    #[arg(long)]
    no_score_bound: bool,
    /// Run only the first N items of each set (impact: of the primary items).
    #[arg(long)]
    limit: Option<usize>,
    /// Answers requested per item (`lci traverse --top-k`, at most 40).
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Label of this measurement; the trend keeps one row per (wave, set).
    #[arg(long, default_value = "unlabelled")]
    wave: String,
    /// Longest one `lci traverse` call may run, in seconds; a slower item is
    /// killed and skipped with the cause `timeout`.
    #[arg(long, default_value_t = 60)]
    timeout_s: u64,
    /// Mark the latencies as measured while other work ran on the machine.
    #[arg(long)]
    under_load: bool,
    /// Also answer with the traversal's seed nodes, ahead of what it reached.
    #[arg(long)]
    include_seeds: bool,
    /// Free text for the row's note column (build profile, tree, seeding).
    #[arg(long)]
    note: Option<String>,
    /// The `lci` binary (default: target/release or target/debug of this
    /// repository, whichever exists and is newer).
    #[arg(long)]
    lci: Option<PathBuf>,
    /// Where run files, scores and raw traversal outputs are written (with
    /// --rescore: where the run file and score are written, optional).
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// Trend file to append to (default: the repository's trend.md).
    #[arg(long)]
    trend: Option<PathBuf>,
    /// Score the raw outputs of an earlier run again instead of running
    /// traversals: the directory that holds that run's `raw/` (one set, so
    /// not `--set all`). Needs --maps; the row says it was rescored.
    #[arg(long)]
    rescore: Option<PathBuf>,
}

pub fn run(args: BenchArgs) -> Result<()> {
    let policy = match args.policy.as_str() {
        "default" => None,
        other => Some(other.to_string()),
    };
    let trend_path = args
        .trend
        .clone()
        .unwrap_or_else(|| repo_root().join(DEFAULT_TREND));
    let sets = args.set.sets();
    let mut gold_items = Vec::new();
    for set in &sets {
        gold_items.push(select_items(
            *set,
            gold::load_gold(&args.gold_dir.join(format!("{}.json", set_name(*set))))?,
            args.limit,
        ));
    }
    if let Some(dir) = &args.rescore {
        return rescore(&args, dir, sets[0], &gold_items[0], &trend_path);
    }

    let scip = args.scip.clone().context("--scip is required")?;
    let tree = args.tree.clone().context("--tree is required")?;
    let out_dir = args.out_dir.clone().context("--out-dir is required")?;
    let lci = match &args.lci {
        Some(path) => path.clone(),
        None => default_lci()?,
    };
    let mut traverser = CliTraverser {
        lci,
        scip,
        tree,
        maps: args.maps.clone(),
        policy,
        fusion: fusion_arg(&args.fusion),
        pass_through: args.pass_through,
        no_score_bound: args.no_score_bound,
        top_k: args.top_k,
        out: out_dir.join("cli"),
        timeout: Duration::from_secs(args.timeout_s),
        probe: std::cell::RefCell::new(None),
    };
    // The probes use the first question of the run: it is one the start-node
    // search is known to be asked.
    let probe_question = gold_items
        .iter()
        .flatten()
        .find_map(|item| item["question"].as_str())
        .unwrap_or("probe")
        .to_string();
    let maps = effective_maps(&args.maps, &traverser, &probe_question)?;
    // One cheap call per map, once for the whole run: a map that fails is
    // marked unavailable and is not tried again per item.
    let maps_unavailable = preflight_maps(&traverser, &maps, &probe_question);
    for (map, reason) in &maps_unavailable {
        eprintln!(
            "map unavailable: {map}: {}",
            reason.lines().next().unwrap_or_default()
        );
    }
    let config = RunConfig {
        wave: args.wave.clone(),
        maps: maps.clone(),
        maps_unavailable,
        policy_label: policy_label(&args.policy),
        under_load: args.under_load,
        include_seeds: args.include_seeds,
        note: note_with_flags(
            args.note.as_deref(),
            &args.fusion,
            args.pass_through,
            args.no_score_bound,
        ),
        progress: true,
        rescored: false,
    };
    traverser.maps = config.available_maps();
    for (set, items) in sets.into_iter().zip(&gold_items) {
        eprintln!("{}: {} items", set_name(set), items.len());
        let outcome = bench_set(set, items, &traverser, &config)?;
        write_outputs(
            &out_dir.join(slug(&args.wave)).join(set_name(set)),
            &outcome,
        )?;
        trend::append(&trend_path, &outcome.row)?;
        eprintln!("{}", outcome.row.to_markdown());
    }
    Ok(())
}

/// `--rescore`: the same scoring and the same row, with the raw outputs of an
/// earlier run in place of traversals. Totals and engine latency come from the
/// raw JSON; the wall latency was not stored, so it stays `-`.
fn rescore(
    args: &BenchArgs,
    dir: &Path,
    set: Set,
    items: &[Value],
    trend_path: &Path,
) -> Result<()> {
    if args.set == SetArg::All {
        bail!("--rescore takes one --set: the directory holds the raw outputs of one set");
    }
    if args.maps.is_empty() {
        bail!("--rescore needs --maps: the maps the earlier run used");
    }
    let config = RunConfig {
        wave: args.wave.clone(),
        maps: args.maps.clone(),
        maps_unavailable: Vec::new(),
        policy_label: policy_label(&args.policy),
        under_load: args.under_load,
        include_seeds: args.include_seeds,
        note: note_with_flags(
            args.note.as_deref(),
            &args.fusion,
            args.pass_through,
            args.no_score_bound,
        ),
        progress: false,
        rescored: true,
    };
    let raw = RawOutputs {
        dir: dir.join("raw"),
    };
    eprintln!(
        "{}: {} items (rescoring {})",
        set_name(set),
        items.len(),
        dir.display()
    );
    let mut outcome = bench_set(set, items, &raw, &config)?;
    // The raw outputs stay where they are.
    outcome.raw.clear();
    if let Some(out_dir) = &args.out_dir {
        write_outputs(
            &out_dir.join(slug(&args.wave)).join(set_name(set)),
            &outcome,
        )?;
    }
    trend::append(trend_path, &outcome.row)?;
    eprintln!("{}", outcome.row.to_markdown());
    Ok(())
}

/// `crates/xtask/..` is the repository root; the manifest dir is `xtask/`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn default_lci() -> Result<PathBuf> {
    let exe = if cfg!(windows) {
        "local-code-intelligence.exe"
    } else {
        "local-code-intelligence"
    };
    let found = ["release", "debug"]
        .iter()
        .map(|profile| repo_root().join("target").join(profile).join(exe))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by_key(|(time, _)| *time)
        .map(|(_, p)| p);
    found.context("no built lci binary under target/release or target/debug; pass --lci <path>")
}

/// The maps of the run: the ones asked for, else the ones the CLI turns on
/// when it is not told (read from the CLI, so there is one list, its own).
pub fn effective_maps(
    requested: &[String],
    traverser: &dyn Traverser,
    question: &str,
) -> Result<Vec<String>> {
    if !requested.is_empty() {
        return Ok(requested.to_vec());
    }
    let maps = traverser
        .default_maps(question)
        .context("could not read the CLI's default maps; pass --maps")?;
    if maps.is_empty() {
        bail!("the CLI reported no default maps; pass --maps");
    }
    Ok(maps)
}

/// The `--fusion` value to pass the CLI: none for its default.
fn fusion_arg(fusion: &str) -> Option<String> {
    (fusion != "boost").then(|| fusion.to_owned())
}

/// The note of the row, with the fusion when it is not the default and
/// `pass-through` when it is on.
fn note_with_flags(
    note: Option<&str>,
    fusion: &str,
    pass_through: bool,
    no_score_bound: bool,
) -> String {
    let flags = [
        fusion_arg(fusion).map(|fusion| format!("fusion {fusion}")),
        pass_through.then(|| "pass-through".to_owned()),
        no_score_bound.then(|| "no-bound".to_owned()),
    ];
    let parts = note
        .filter(|note| !note.is_empty())
        .map(str::to_owned)
        .into_iter()
        .chain(flags.into_iter().flatten());
    parts.collect::<Vec<_>>().join("; ")
}

fn policy_label(policy: &str) -> String {
    if policy == "default" {
        return "default".into();
    }
    Path::new(policy)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("custom")
        .to_string()
}

fn slug(text: &str) -> String {
    let s: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if s.is_empty() { "wave".into() } else { s }
}

pub fn set_name(set: Set) -> &'static str {
    match set {
        Set::Impact => "impact",
        Set::Flow => "flow",
        Set::WhereTo => "where-to",
        Set::History => "history",
        Set::Instructions => "instructions",
    }
}

/// The scored population of a set (impact keeps its `primary` items), cut to
/// the first `limit`.
fn select_items(set: Set, items: Vec<Value>, limit: Option<usize>) -> Vec<Value> {
    let kept = items.into_iter().filter(|item| match set {
        Set::Impact => item.get("primary").and_then(Value::as_bool) == Some(true),
        _ => true,
    });
    match limit {
        Some(n) => kept.take(n).collect(),
        None => kept.collect(),
    }
}

// ------------------------------------------------------------ capabilities

/// The two maps that only find start nodes; they are never expanded, so they
/// show up in a call by the seeds they gave, not by calls counted.
const SEED_MAPS: [&str; 2] = ["seeds.hybrid", "seeds.scip_names"];

/// The node kinds a known map emits (the `emits` of its descriptor); `None`
/// for a map this runner does not know (it is run and judged by its results).
fn map_emits(map: &str) -> Option<&'static [NodeKind]> {
    use NodeKind::{Chunk, Commit, ConfigKey, File, Symbol};
    match map {
        "seeds.scip_names" | "graph.scip" | "scip.impact" => Some(&[Symbol]),
        "seeds.hybrid" => Some(&[Symbol, Chunk, File]),
        "structure.contains" | "grep.live" | "probe.compile" => Some(&[Symbol, File]),
        "git.history" => Some(&[Commit, File]),
        "config.key" => Some(&[ConfigKey, Symbol]),
        "search.lexical" | "search.vector" => Some(&[Chunk]),
        _ => None,
    }
}

/// The node kinds from which a set's answers can be made.
fn needed_kinds(set: Set) -> &'static [NodeKind] {
    match set {
        Set::Impact | Set::Flow | Set::WhereTo => &[NodeKind::Symbol, NodeKind::File],
        Set::History => &[NodeKind::Commit],
        Set::Instructions => &[NodeKind::DocSection],
    }
}

/// Whether any selected map can emit a node kind the set needs.
pub fn can_answer(set: Set, maps: &[String]) -> bool {
    maps.iter().any(|m| match map_emits(m) {
        Some(kinds) => kinds.iter().any(|k| needed_kinds(set).contains(k)),
        None => true,
    })
}

// -------------------------------------------------------------- traversal

/// Where a result card says its node is: the repository-relative path (with
/// `/`) and the 1-based line of the definition (of a symbol, the line of its
/// name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub path: String,
    pub line: u64,
}

impl Site {
    fn from_json(value: &Value) -> Option<Site> {
        let line = value["line"].as_u64().filter(|line| *line >= 1)?;
        Some(Site {
            path: value["path"].as_str()?.replace('\\', "/"),
            line,
        })
    }
}

/// One returned node: its canonical id, the one-line text an agent reads and
/// the site the card gave it, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultNode {
    pub id: String,
    pub text: String,
    pub site: Option<Site>,
}

/// The totals of one traversal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub pops: u64,
    pub hops: u64,
    pub cross_map_hops: u64,
    pub stops: u64,
    pub elapsed_ms: f64,
}

/// What one `lci traverse` call returned, reduced to what the bench uses.
#[derive(Debug, Clone, Default)]
pub struct Traversed {
    pub status: String,
    pub seeds: Vec<String>,
    pub results: Vec<ResultNode>,
    /// The site of every node a card of the response located (results,
    /// checkpoint cards and `seed_sites`), to give the seeds theirs.
    pub sites: BTreeMap<String, Site>,
    pub totals: Option<Totals>,
    /// The whole process, when it was measured (a rescored run has none).
    pub wall_ms: Option<f64>,
}

/// The card lists of a response that carry nodes and their sites.
const CARD_LISTS: [&str; 4] = [
    "/results",
    "/checkpoint/answer",
    "/checkpoint/visited_cards",
    "/checkpoint/frontier",
];

impl Traversed {
    pub fn from_json(value: &Value, wall_ms: Option<f64>) -> Traversed {
        let strings = |v: &Value| -> Vec<String> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let results = value["results"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| {
                        Some(ResultNode {
                            id: r["node"].as_str()?.to_string(),
                            text: r["text"].as_str().unwrap_or_default().to_string(),
                            site: Site::from_json(&r["site"]),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut sites = BTreeMap::new();
        for list in CARD_LISTS {
            for card in value
                .pointer(list)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(node), Some(site)) =
                    (card["node"].as_str(), Site::from_json(&card["site"]))
                {
                    sites.entry(node.to_string()).or_insert(site);
                }
            }
        }
        // The seeds' own sites: a run that returns no checkpoint still has them.
        for (node, site) in value["seed_sites"].as_object().into_iter().flatten() {
            if let Some(site) = Site::from_json(site) {
                sites.entry(node.clone()).or_insert(site);
            }
        }
        let totals = value
            .get("totals")
            .filter(|t| t.is_object())
            .map(|t| Totals {
                pops: t["pops"].as_u64().unwrap_or(0),
                hops: t["hops"].as_u64().unwrap_or(0),
                cross_map_hops: t["cross_map_hops"].as_u64().unwrap_or(0),
                stops: t["stops"].as_u64().unwrap_or(0),
                elapsed_ms: t["elapsed_ms"].as_f64().unwrap_or(0.0),
            });
        Traversed {
            status: value["status"].as_str().unwrap_or("unknown").to_string(),
            seeds: strings(&value["seeds"]),
            results,
            sites,
            totals,
            wall_ms,
        }
    }
}

/// Why a traversal call produced no answer. Both causes skip the item; they
/// are counted apart in the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraverseError {
    /// The call ran past the per-item timeout and was killed.
    Timeout(u64),
    /// The CLI could not run, exited non-zero or printed no JSON.
    Cli(String),
}

impl std::fmt::Display for TraverseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraverseError::Timeout(secs) => write!(f, "timeout after {secs}s"),
            TraverseError::Cli(message) => write!(f, "cli error: {message}"),
        }
    }
}

impl std::error::Error for TraverseError {}

/// Something that answers a question with a traversal. The real one runs the
/// CLI, the rescoring one reads the raw outputs of an earlier run; the tests
/// use a fixture.
pub trait Traverser {
    /// The parsed answer and the raw JSON it came from, for the gold item
    /// `id` and its question.
    fn traverse(&self, id: &str, question: &str) -> Result<(Traversed, Value), TraverseError>;

    /// The maps the CLI turns on when `--maps` is not given.
    fn default_maps(&self, question: &str) -> Result<Vec<String>, TraverseError>;

    /// One cheap call that must exercise `map`: it is given start nodes the
    /// map can expand (or, for a seeds map, the question) and has to show that
    /// the map was registered and called. An error means the map is
    /// unavailable on this tree.
    fn preflight(&self, map: &str, question: &str) -> Result<(), TraverseError>;
}

/// Runs the preflight once per map. Returns the reason for each map that
/// failed; those maps are not used for the run and are not retried per item.
pub fn preflight_maps(
    traverser: &dyn Traverser,
    maps: &[String],
    question: &str,
) -> Vec<(String, String)> {
    maps.iter()
        .filter_map(|map| {
            traverser
                .preflight(map, question)
                .err()
                .map(|error| (map.clone(), error.to_string()))
        })
        .collect()
}

/// The maps a probe call lists in the start event of its trace, each with
/// whether the call had it on. `None` when the text is no trace.
pub fn parse_start_maps(trace: &str) -> Option<Vec<(String, bool)>> {
    let start: Value = serde_json::from_str(trace.lines().next()?).ok()?;
    if start["ev"] != "start" {
        return None;
    }
    start["maps"]
        .as_array()?
        .iter()
        .map(|m| Some((m["id"].as_str()?.to_string(), m["enabled"].as_bool()?)))
        .collect()
}

/// What a probe call on `map` must show. A seeds map is judged by the start
/// nodes it gave (it is never expanded); any other map by the start event (it
/// was registered and switched on) and by the per-map totals (it was called at
/// least once on the explicit start nodes). `plan` is the start event's map
/// list, `None` for a seeds map.
pub fn check_probe(
    map: &str,
    response: &Value,
    plan: Option<&[(String, bool)]>,
) -> Result<(), String> {
    let note = || {
        response["notes"]
            .as_array()
            .and_then(|notes| notes.iter().filter_map(Value::as_str).next_back())
            .map(|note| format!(" (last note: {note})"))
            .unwrap_or_default()
    };
    if SEED_MAPS.contains(&map) {
        let wanted = if map == "seeds.hybrid" {
            "hybrid"
        } else {
            "name_tokens"
        };
        let source = response["seed_source"].as_str().unwrap_or("none");
        let n_seeds = response["seeds"].as_array().map_or(0, Vec::len);
        if source != wanted || n_seeds == 0 {
            let fallback = response["seed_fallback"]
                .as_str()
                .map(|why| format!(": {why}"))
                .unwrap_or_default();
            return Err(format!(
                "{map} gave no start nodes for the probe question (seed_source {source}, {n_seeds} seeds{fallback})"
            ));
        }
        if let Some(why) = response["seed_fallback"].as_str() {
            return Err(format!("{map} fell back to another source: {why}"));
        }
        return Ok(());
    }
    let Some(plan) = plan else {
        return Err(format!("{map}: the probe call left no start event to read"));
    };
    match plan.iter().find(|(id, _)| id == map) {
        None => {
            return Err(format!(
                "{map} was not registered: the CLI left it out of the call{}",
                note()
            ));
        }
        Some((_, false)) => return Err(format!("{map} was registered but switched off")),
        Some(_) => {}
    }
    // `{"expands": n, "ms": t}`; the first traces kept a bare count.
    let stat = &response["totals"]["per_map"][map];
    let calls = stat["expands"].as_u64().or(stat.as_u64()).unwrap_or(0);
    if calls == 0 {
        return Err(format!(
            "{map} was never called on the probe start nodes (per-map calls: {}){}",
            response["totals"]["per_map"],
            note()
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct CliTraverser {
    pub lci: PathBuf,
    pub scip: PathBuf,
    pub tree: PathBuf,
    pub maps: Vec<String>,
    pub policy: Option<String>,
    /// `--fusion` for the CLI; `None` leaves its default.
    pub fusion: Option<String>,
    /// `--pass-through` for the CLI.
    pub pass_through: bool,
    /// `--no-score-bound` for the CLI.
    pub no_score_bound: bool,
    pub top_k: usize,
    pub out: PathBuf,
    pub timeout: Duration,
    /// The start nodes the probes expand, found once.
    pub probe: std::cell::RefCell<Option<ProbeSeeds>>,
}

/// Explicit start nodes for the preflight: a symbol of the SCIP graph and
/// the file it is in, so that a map that expands either kind has something
/// to expand.
#[derive(Debug, Clone)]
pub struct ProbeSeeds {
    pub nodes: Vec<String>,
}

impl CliTraverser {
    /// One `lci traverse` call. `maps` empty leaves the CLI to choose;
    /// `seeds` empty lets it find the start nodes from the question.
    fn invoke(
        &self,
        maps: &[String],
        seeds: &[String],
        question: &str,
    ) -> Result<(Traversed, Value), TraverseError> {
        let mut command = Command::new(&self.lci);
        command
            .arg("traverse")
            .arg("--scip")
            .arg(&self.scip)
            .arg("--root")
            .arg(&self.tree)
            .arg(format!("--question={question}"))
            .arg(format!("--top-k={}", self.top_k))
            .arg("--out")
            .arg(&self.out);
        if !maps.is_empty() {
            command.arg(format!("--maps={}", maps.join(",")));
        }
        if !seeds.is_empty() {
            command.arg(format!("--seeds={}", seeds.join(",")));
        }
        if let Some(policy) = &self.policy {
            command.arg("--policy").arg(policy);
        }
        if let Some(fusion) = &self.fusion {
            command.arg(format!("--fusion={fusion}"));
        }
        if self.pass_through {
            command.arg("--pass-through");
        }
        if self.no_score_bound {
            command.arg("--no-score-bound");
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|e| TraverseError::Cli(format!("run the lci binary: {e}")))?;
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
                Ok(None) if started.elapsed() >= self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = (stdout.join(), stderr.join());
                    return Err(TraverseError::Timeout(self.timeout.as_secs()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(TraverseError::Cli(format!("wait for lci: {e}"))),
            }
        };
        let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
        let out = stdout.join().unwrap_or_default();
        let err = stderr.join().unwrap_or_default();
        if !status.success() {
            let stderr = String::from_utf8_lossy(&err);
            let tail: String = stderr
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(TraverseError::Cli(format!(
                "lci traverse exited with {status}: {}",
                tail.trim()
            )));
        }
        let value: Value = serde_json::from_slice(&out)
            .map_err(|e| TraverseError::Cli(format!("lci traverse printed no JSON: {e}")))?;
        Ok((Traversed::from_json(&value, Some(wall_ms)), value))
    }

    /// The start nodes the probes expand, found once with the name-token
    /// stand-in (it needs no search service): the first symbol it finds for
    /// the question and the file of the first result that has a site.
    fn probe_seeds(&self, question: &str) -> Result<ProbeSeeds, TraverseError> {
        if let Some(found) = self.probe.borrow().clone() {
            return Ok(found);
        }
        let maps = ["seeds.scip_names".to_string(), "graph.scip".to_string()];
        let (traversed, _) = self.invoke(&maps, &[], question)?;
        let Some(symbol) = traversed.seeds.iter().find(|s| s.starts_with("sym:")) else {
            return Err(TraverseError::Cli(format!(
                "no probe start node: the name-token seeds found no symbol for the probe question (status {})",
                traversed.status
            )));
        };
        let mut nodes = vec![symbol.clone()];
        if let Some(site) = traversed.results.iter().find_map(|r| r.site.as_ref()) {
            nodes.push(
                NodeId::File {
                    path: site.path.clone(),
                }
                .to_string(),
            );
        }
        let found = ProbeSeeds { nodes };
        *self.probe.borrow_mut() = Some(found.clone());
        Ok(found)
    }

    /// The maps the start event of the trace a call wrote lists.
    fn start_maps(&self, response: &Value) -> Result<Vec<(String, bool)>, TraverseError> {
        let file = response["trace"]["file"]
            .as_str()
            .ok_or_else(|| TraverseError::Cli("the probe call wrote no trace".into()))?;
        let path = self.out.join("traces").join(file);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| TraverseError::Cli(format!("read the probe trace: {e}")))?;
        parse_start_maps(&text)
            .ok_or_else(|| TraverseError::Cli("the probe trace has no start event".into()))
    }
}

impl Traverser for CliTraverser {
    fn traverse(&self, _id: &str, question: &str) -> Result<(Traversed, Value), TraverseError> {
        self.invoke(&self.maps, &[], question)
    }

    fn default_maps(&self, question: &str) -> Result<Vec<String>, TraverseError> {
        let seeds = self.probe_seeds(question)?;
        let (_, raw) = self.invoke(&[], &seeds.nodes, question)?;
        Ok(self
            .start_maps(&raw)?
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(id, _)| id)
            .collect())
    }

    fn preflight(&self, map: &str, question: &str) -> Result<(), TraverseError> {
        let seeds = if SEED_MAPS.contains(&map) {
            Vec::new()
        } else {
            self.probe_seeds(question)?.nodes
        };
        let (_, raw) = self.invoke(&[map.to_string()], &seeds, question)?;
        let plan = if SEED_MAPS.contains(&map) {
            None
        } else {
            Some(self.start_maps(&raw)?)
        };
        check_probe(map, &raw, plan.as_deref()).map_err(TraverseError::Cli)
    }
}

/// The raw outputs of an earlier run (`raw/<slug of item id>.json`), served as
/// if the traversals ran again. A skipped item stays skipped for the same
/// cause.
pub struct RawOutputs {
    pub dir: PathBuf,
}

impl Traverser for RawOutputs {
    fn traverse(&self, id: &str, _question: &str) -> Result<(Traversed, Value), TraverseError> {
        let path = self.dir.join(format!("{}.json", slug(id)));
        let text = std::fs::read_to_string(&path)
            .map_err(|e| TraverseError::Cli(format!("no raw output for {id}: {e}")))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| TraverseError::Cli(format!("raw output of {id} is not JSON: {e}")))?;
        if let Some(why) = value["skipped"].as_str() {
            return Err(match why.strip_prefix("timeout after ") {
                Some(rest) => {
                    TraverseError::Timeout(rest.trim_end_matches('s').parse().unwrap_or_default())
                }
                None => TraverseError::Cli(why.trim_start_matches("cli error: ").to_string()),
            });
        }
        Ok((Traversed::from_json(&value, None), value))
    }

    fn default_maps(&self, _question: &str) -> Result<Vec<String>, TraverseError> {
        Err(TraverseError::Cli(
            "raw outputs do not say which maps ran: pass --maps".into(),
        ))
    }

    fn preflight(&self, _map: &str, _question: &str) -> Result<(), TraverseError> {
        Ok(())
    }
}

// ------------------------------------------------------------- adapters

/// A symbol node as the scorers see a declaration: where it is defined. The
/// node id has neither the file nor the impl type, so there is no name to
/// offer; the scorers match on the location (a line inside the gold span, or
/// the gold's own `file:line`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decl {
    pub path: String,
    /// The line of the definition.
    pub line: u64,
    pub kind: SymbolKind,
}

/// The declaration a symbol result stands for: the site of its card (none
/// for a symbol the card could not place).
pub fn decl_of(node: &ResultNode) -> Option<Decl> {
    let NodeId::Symbol(key) = parsed(&node.id)? else {
        return None;
    };
    let site = node.site.as_ref()?;
    Some(Decl {
        path: site.path.clone(),
        line: site.line,
        kind: key.kind,
    })
}

fn parsed(id: &str) -> Option<NodeId> {
    id.parse::<NodeId>().ok()
}

/// The file a node stands for: a file, chunk or doc node names it, a symbol
/// is placed by its site.
fn path_of(node: &ResultNode) -> Option<String> {
    match parsed(&node.id)? {
        NodeId::Symbol(_) => Some(decl_of(node)?.path),
        other => other.path().map(str::to_string),
    }
}

fn answer(path: Option<&str>, line: Option<u64>) -> Value {
    let mut map = Map::new();
    if let Some(path) = path {
        map.insert("path".into(), json!(path));
    }
    if let Some(line) = line {
        map.insert("line".into(), json!(line));
    }
    Value::Object(map)
}

fn decl_answer(decl: &Decl) -> Value {
    answer(Some(&decl.path), Some(decl.line))
}

fn dedup(answers: Vec<Value>) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    answers
        .into_iter()
        .filter(|a| seen.insert(a.to_string()))
        .collect()
}

fn paths_only(nodes: &[ResultNode]) -> Vec<Value> {
    dedup(
        nodes
            .iter()
            .filter_map(path_of)
            .map(|p| answer(Some(&p), None))
            .collect(),
    )
}

/// The function and method declarations among `nodes`, as located answers.
fn function_decls(nodes: &[ResultNode]) -> Vec<Value> {
    dedup(
        nodes
            .iter()
            .filter_map(decl_of)
            .filter(|d| matches!(d.kind, SymbolKind::Function | SymbolKind::Method))
            .map(|d| decl_answer(&d))
            .collect(),
    )
}

/// Turns the nodes a traversal returned, best first, into the run-file answers
/// the set's scorer reads.
pub fn adapt(set: Set, item: &Value, nodes: &[ResultNode]) -> Vec<Value> {
    match set {
        Set::Impact => adapt_impact(nodes),
        Set::Flow => adapt_flow(nodes),
        Set::WhereTo => adapt_where_to(item, nodes),
        Set::History => adapt_history(nodes),
        Set::Instructions => adapt_instructions(nodes),
    }
}

/// Impact: every symbol is a declaration (path and line of its definition), a
/// file node is a file.
pub fn adapt_impact(nodes: &[ResultNode]) -> Vec<Value> {
    dedup(
        nodes
            .iter()
            .filter_map(|n| match parsed(&n.id)? {
                NodeId::Symbol(_) => Some(decl_answer(&decl_of(n)?)),
                other => other.path().map(|p| answer(Some(p), None)),
            })
            .collect(),
    )
}

/// Flow: the path is made of functions and methods, each by the path and line
/// of its definition (the gold names a node by `file:line` too); other nodes
/// are not offered (an offered node that is no known node would count as
/// spurious).
pub fn adapt_flow(nodes: &[ResultNode]) -> Vec<Value> {
    function_decls(nodes)
}

/// Where-to: registration items want files, ranked; convention items want an
/// implementation per file (a path-only answer matches when one implementation
/// lives in the file); exemplar items want members by location, or files when
/// the item is file-granular.
pub fn adapt_where_to(item: &Value, nodes: &[ResultNode]) -> Vec<Value> {
    let gold = &item["gold"];
    let by_member =
        gold["form"].as_str() == Some("exemplar") && gold["granularity"].as_str() != Some("file");
    if !by_member {
        return paths_only(nodes);
    }
    function_decls(nodes)
}

/// History: commit nodes, by hash.
pub fn adapt_history(nodes: &[ResultNode]) -> Vec<Value> {
    dedup(
        nodes
            .iter()
            .filter_map(|n| match parsed(&n.id)? {
                NodeId::Commit { sha } => Some(json!({ "commit": sha })),
                _ => None,
            })
            .collect(),
    )
}

/// Instructions: document-section nodes, as the whole document (the id holds
/// the heading path, not a line range).
pub fn adapt_instructions(nodes: &[ResultNode]) -> Vec<Value> {
    dedup(
        nodes
            .iter()
            .filter_map(|n| match parsed(&n.id)? {
                NodeId::DocSection { path, .. } => Some(json!({ "path": path })),
                _ => None,
            })
            .collect(),
    )
}

// ----------------------------------------------------------------- scoring

/// F-beta: `(1 + b^2) p r / (b^2 p + r)`, 0 when both are 0.
pub fn f_beta(precision: f64, recall: f64, beta: f64) -> f64 {
    let b2 = beta * beta;
    let denominator = b2 * precision + recall;
    if denominator <= 0.0 {
        0.0
    } else {
        (1.0 + b2) * precision * recall / denominator
    }
}

fn answers_of(values: &[Value]) -> Result<Vec<Answer>> {
    values.iter().map(Answer::from_value).collect()
}

/// The scorer's metrics for one item scored alone.
fn item_metrics(
    set: Set,
    item: &Value,
    answers: &[Value],
) -> Result<BTreeMap<String, Option<f64>>> {
    let id = item["id"].as_str().context("gold item without an id")?;
    let mut run = gold::Run::new();
    run.insert(id.to_string(), answers_of(answers)?);
    let report = gold::score_set(set, std::slice::from_ref(item), &run, false, false)?;
    Ok(report["overall"]["metrics"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_f64())).collect())
        .unwrap_or_default())
}

/// The answer-set precision and recall of one item, from the scorer's own
/// definitions (see the trend preface).
pub fn item_precision_recall(
    set: Set,
    item: &Value,
    answers: &[Value],
    metrics: &BTreeMap<String, Option<f64>>,
) -> (f64, f64) {
    let m = |name: &str| metrics.get(name).copied().flatten().unwrap_or(0.0);
    match set {
        Set::Impact => (m("decl_precision"), m("decl_recall")),
        Set::Flow => {
            let returned = answers.len() as f64;
            let precision = if returned == 0.0 {
                0.0
            } else {
                ((returned - m("spurious")) / returned).clamp(0.0, 1.0)
            };
            (precision, m("path_recall"))
        }
        Set::WhereTo => {
            if item["gold"]["form"].as_str() == Some("registration") {
                registration_precision_recall(item, answers)
            } else {
                (m("set_precision"), m("set_recall"))
            }
        }
        Set::History => history_precision_recall(item, answers),
        Set::Instructions => {
            if item["subtype"].as_str() == Some("I1") {
                (m("i1_section_precision"), m("i1_section_recall"))
            } else {
                (m("i2_required_precision@5"), m("i2_required_recall"))
            }
        }
    }
}

fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Registration: distinct returned files, minus the files the gold treats as
/// neutral, against the expected files.
fn registration_precision_recall(item: &Value, answers: &[Value]) -> (f64, f64) {
    let expected: BTreeSet<String> = string_list(&item["gold"]["expected_files"])
        .into_iter()
        .collect();
    let accept = &item["accept"];
    let mut neutral: BTreeSet<String> = BTreeSet::new();
    for key in [
        "preferred",
        "documentation",
        "neutral_sites",
        "definition_sites",
    ] {
        neutral.extend(
            accept[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["path"].as_str().map(str::to_string)),
        );
    }
    for related in accept["related_sites"].as_array().into_iter().flatten() {
        neutral.extend(string_list(&related["files"]));
    }
    neutral.retain(|f| !expected.contains(f));
    let returned: BTreeSet<&str> = answers
        .iter()
        .filter_map(|a| a["path"].as_str())
        .filter(|p| !neutral.contains(*p))
        .collect();
    let hits = returned.iter().filter(|p| expected.contains(**p)).count();
    let precision = if returned.is_empty() {
        0.0
    } else {
        hits as f64 / returned.len() as f64
    };
    let recall = if expected.is_empty() {
        1.0
    } else {
        hits as f64 / expected.len() as f64
    };
    (precision, recall)
}

/// History: accepted commits among the distinct commits returned; recall is 1
/// when an accepted commit was returned (every item has one answer to find).
fn history_precision_recall(item: &Value, answers: &[Value]) -> (f64, f64) {
    let mut accepted = string_list(&item["accept"]["mrr_commits"]);
    if accepted.is_empty() {
        accepted.extend(item["gold"]["commit"].as_str().map(str::to_string));
    }
    let same = |a: &str, b: &str| {
        let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
        a.len() >= 7 && b.len() >= 7 && (a.starts_with(&b) || b.starts_with(&a))
    };
    let returned: BTreeSet<&str> = answers
        .iter()
        .filter_map(|a| a["commit"].as_str())
        .collect();
    let hits = returned
        .iter()
        .filter(|c| accepted.iter().any(|g| same(c, g)))
        .count();
    if returned.is_empty() {
        return (0.0, 0.0);
    }
    (hits as f64 / returned.len() as f64, f64::from(hits > 0))
}

// -------------------------------------------------------------- the bench

pub struct RunConfig {
    pub wave: String,
    /// The maps asked for (the CLI's own default list when none were given).
    pub maps: Vec<String>,
    /// Maps that failed the preflight, with the reason; they are not used.
    pub maps_unavailable: Vec<(String, String)>,
    pub policy_label: String,
    pub under_load: bool,
    pub include_seeds: bool,
    pub note: String,
    /// Print a progress line to stderr every [`PROGRESS_EVERY`] items.
    pub progress: bool,
    /// The answers come from the raw outputs of an earlier run.
    pub rescored: bool,
}

impl RunConfig {
    /// The requested maps that passed the preflight.
    fn available_maps(&self) -> Vec<String> {
        self.maps
            .iter()
            .filter(|m| !self.maps_unavailable.iter().any(|(u, _)| u == *m))
            .cloned()
            .collect()
    }
}

/// Everything one (wave, set) produced.
pub struct SetOutcome {
    /// `{"<item id>": [answers]}`, the gold scorer's run file (scored items
    /// only).
    pub run: Value,
    /// The gold scorer's report (`Null` when nothing was scored).
    pub report: Value,
    pub row: TrendRow,
    /// Per attempted item: the raw traversal JSON, or the error text.
    pub raw: Vec<(String, Value)>,
}

/// Items that were not scored, by cause. Nothing is dropped without a count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Skips {
    pub timeout: usize,
    pub cli_error: usize,
    pub no_answer_type: usize,
    pub map_unavailable: usize,
}

impl Skips {
    pub fn total(&self) -> usize {
        self.timeout + self.cli_error + self.no_answer_type + self.map_unavailable
    }

    /// `timeout=2; cli_error=1` (only the causes that occurred), or `none`.
    pub fn describe(&self) -> String {
        let parts: Vec<String> = [
            ("timeout", self.timeout),
            ("cli_error", self.cli_error),
            ("no_answer_type", self.no_answer_type),
            ("map_unavailable", self.map_unavailable),
        ]
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(cause, n)| format!("{cause}={n}"))
        .collect();
        if parts.is_empty() {
            "none".into()
        } else {
            parts.join("; ")
        }
    }
}

#[derive(Default)]
struct Sums {
    pops: u64,
    hops: u64,
    cross: u64,
    stops: u64,
    answer_chars_tokens: u64,
    engine_ms: Vec<f64>,
    wall_ms: Vec<f64>,
    non_empty: usize,
    no_seeds: usize,
    precision: f64,
    f05: f64,
}

/// Nearest-rank percentile of an unsorted sample.
pub fn percentile(sample: &[f64], p: f64) -> Option<f64> {
    if sample.is_empty() {
        return None;
    }
    let mut sorted = sample.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

fn ms_pair(sample: &[f64], under_load: bool) -> String {
    match (percentile(sample, 0.5), percentile(sample, 0.95)) {
        (Some(a), Some(b)) => format!(
            "{a:.0}/{b:.0}{}",
            if under_load { " (under load)" } else { "" }
        ),
        _ => "-".into(),
    }
}

/// The metrics a row shows for each set.
fn headline(set: Set) -> &'static [&'static str] {
    match set {
        Set::Impact => &["decl_recall", "decl_f1", "decl_ndcg@10", "file_f1"],
        Set::Flow => &["path_recall", "tau", "spurious"],
        Set::WhereTo => &["reg_hit@3", "reg_mrr", "set_f1"],
        Set::History => &["mrr", "hit@3", "hit@10"],
        Set::Instructions => &["i1_section_f1", "i2_hit@3_primary", "i2_required_recall@5"],
    }
}

fn metrics_cell(set: Set, report: &Value) -> String {
    let overall = &report["overall"];
    headline(set)
        .iter()
        .filter(|name| overall["metrics"].get(**name).is_some())
        .map(|name| {
            let value = match overall["metrics"][*name].as_f64() {
                Some(v) => format!("{v:.3}"),
                None => "n/a".into(),
            };
            match overall["n_defined"][*name].as_u64() {
                Some(k) => format!("{name}={value} ({k})"),
                None => format!("{name}={value}"),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The row with the cells every outcome has; the rest stay `-`.
fn base_cells(config: &RunConfig, set: Set, n_gold: usize) -> [String; NCOLS] {
    let mut cells: [String; NCOLS] = std::array::from_fn(|_| "-".to_string());
    cells[0] = config.wave.clone();
    cells[1] = set_name(set).to_string();
    cells[2] = n_gold.to_string();
    cells[17] = config.maps.join(",");
    cells[18] = if config.maps_unavailable.is_empty() {
        "none".into()
    } else {
        config
            .maps_unavailable
            .iter()
            .map(|(map, reason)| {
                let reason: String = reason
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(60)
                    .collect();
                format!("{map} ({reason})")
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    cells[19] = config.policy_label.clone();
    cells[20] = config.note.clone();
    cells
}

/// A row for a set none of the items of which could be attempted.
fn skipped_outcome(
    config: &RunConfig,
    set: Set,
    n_gold: usize,
    skips: Skips,
    why: &str,
    structural: bool,
) -> SetOutcome {
    let mut cells = base_cells(config, set, n_gold);
    cells[3] = "0".into();
    cells[4] = "0".into();
    cells[5] = skips.describe();
    cells[7] = why.to_string();
    if structural {
        // The zero is the maps' shape, not a result: it is not a score to
        // average with the sets the maps can answer.
        cells[20] = if config.note.is_empty() {
            STRUCTURAL_ZERO.to_string()
        } else {
            format!("{}; {STRUCTURAL_ZERO}", config.note)
        };
    }
    SetOutcome {
        run: Value::Object(Map::new()),
        report: Value::Null,
        row: TrendRow { cells },
        raw: Vec::new(),
    }
}

pub fn bench_set(
    set: Set,
    items: &[Value],
    traverser: &dyn Traverser,
    config: &RunConfig,
) -> Result<SetOutcome> {
    let n_gold = items.len();
    if !can_answer(set, &config.maps) {
        let skips = Skips {
            no_answer_type: n_gold,
            ..Skips::default()
        };
        return Ok(skipped_outcome(config, set, n_gold, skips, NO_MAP, true));
    }
    if !can_answer(set, &config.available_maps()) {
        let skips = Skips {
            map_unavailable: n_gold,
            ..Skips::default()
        };
        return Ok(skipped_outcome(
            config,
            set,
            n_gold,
            skips,
            "map unavailable: no usable map can answer this set",
            false,
        ));
    }

    let mut run = Map::new();
    let mut raw = Vec::new();
    let mut sums = Sums::default();
    let mut skips = Skips::default();
    let mut scored: Vec<(Vec<Value>, &Value)> = Vec::new();
    let started = Instant::now();
    for (index, item) in items.iter().enumerate() {
        let id = item["id"].as_str().context("gold item without an id")?;
        let question = item["question"].as_str().unwrap_or_default();
        match traverser.traverse(id, question) {
            Ok((traversed, raw_json)) => {
                raw.push((id.to_string(), raw_json));
                let mut nodes: Vec<ResultNode> = Vec::new();
                if config.include_seeds {
                    nodes.extend(traversed.seeds.iter().map(|s| ResultNode {
                        id: s.clone(),
                        text: s.clone(),
                        site: traversed.sites.get(s).cloned(),
                    }));
                }
                nodes.extend(traversed.results.iter().cloned());
                let answers = adapt(set, item, &nodes);
                if traversed.status == "no_seeds" {
                    sums.no_seeds += 1;
                }
                if !answers.is_empty() {
                    sums.non_empty += 1;
                }
                let chars: usize = nodes.iter().map(|n| n.text.chars().count()).sum();
                sums.answer_chars_tokens += chars.div_ceil(4) as u64;
                sums.wall_ms.extend(traversed.wall_ms);
                if let Some(t) = &traversed.totals {
                    sums.pops += t.pops;
                    sums.hops += t.hops;
                    sums.cross += t.cross_map_hops;
                    sums.stops += t.stops;
                    sums.engine_ms.push(t.elapsed_ms);
                }
                run.insert(id.to_string(), Value::Array(answers.clone()));
                scored.push((answers, item));
            }
            Err(error) => {
                match error {
                    TraverseError::Timeout(_) => skips.timeout += 1,
                    TraverseError::Cli(_) => skips.cli_error += 1,
                }
                raw.push((id.to_string(), json!({ "skipped": error.to_string() })));
            }
        }
        let done = index + 1;
        if config.progress && (done % PROGRESS_EVERY == 0 || done == n_gold) {
            eprintln!(
                "{}: {done}/{n_gold} done, skipped {}, elapsed {:.0}s, ETA {:.0}s",
                set_name(set),
                skips.describe(),
                started.elapsed().as_secs_f64(),
                started.elapsed().as_secs_f64() / done as f64 * (n_gold - done) as f64,
            );
        }
    }

    for (answers, item) in &scored {
        let metrics = item_metrics(set, item, answers)?;
        let (p, r) = item_precision_recall(set, item, answers, &metrics);
        sums.precision += p;
        sums.f05 += f_beta(p, r, 0.5);
    }
    let n_scored = scored.len();
    let scored_items: Vec<Value> = scored.iter().map(|(_, item)| (*item).clone()).collect();
    let run_value = Value::Object(run);
    let report = if n_scored == 0 {
        Value::Null
    } else {
        let parsed_run = gold::parse_run(&run_value.to_string())?;
        gold::score_set(set, &scored_items, &parsed_run, false, false)?
    };

    let mut cells = base_cells(config, set, n_gold);
    cells[3] = n_gold.to_string();
    cells[4] = n_scored.to_string();
    cells[5] = skips.describe();
    if n_scored == 0 {
        cells[7] = "none scored".into();
    } else {
        let n = n_scored as f64;
        cells[6] = format!("{}/{n_scored}", sums.non_empty);
        cells[7] = format!("{} (over {n_scored})", metrics_cell(set, &report));
        cells[8] = format!("{:.3}", sums.precision / n);
        cells[9] = format!("{:.3}", sums.f05 / n);
        cells[10] = sums.pops.to_string();
        cells[11] = sums.hops.to_string();
        cells[12] = sums.cross.to_string();
        cells[13] = sums.stops.to_string();
        cells[14] = sums.answer_chars_tokens.to_string();
        cells[15] = ms_pair(&sums.engine_ms, config.under_load);
        cells[16] = ms_pair(&sums.wall_ms, config.under_load);
    }
    let mut notes: Vec<String> = Vec::new();
    if !config.note.is_empty() {
        notes.push(config.note.clone());
    }
    if sums.no_seeds > 0 {
        notes.push(format!("{} scored items found no seeds", sums.no_seeds));
    }
    if skips.total() > 0 {
        notes.push("skipped items are not in any metric; reasons in raw/".into());
    }
    if config.include_seeds {
        notes.push("seeds included in answers".into());
    }
    if config.under_load {
        notes.push("latency measured under load: not comparable with a clean run".into());
    }
    if config.rescored {
        notes.push(
            "rescored from the raw outputs of an earlier run, no traversal; wall ms was not recorded"
                .into(),
        );
    }
    notes.push("ans tok = chars/4".into());
    cells[20] = notes.join("; ");
    Ok(SetOutcome {
        run: run_value,
        report,
        row: TrendRow { cells },
        raw,
    })
}

/// Writes `run.json`, `score.json`, `row.md` and `raw/<id>.json` under `dir`.
pub fn write_outputs(dir: &Path, outcome: &SetOutcome) -> Result<()> {
    let raw_dir = dir.join("raw");
    std::fs::create_dir_all(&raw_dir).with_context(|| format!("create {}", raw_dir.display()))?;
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
    std::fs::write(dir.join("run.json"), pretty(&outcome.run)?)?;
    if !outcome.report.is_null() {
        std::fs::write(dir.join("score.json"), pretty(&outcome.report)?)?;
    }
    std::fs::write(dir.join("row.md"), outcome.row.to_markdown() + "\n")?;
    for (id, value) in &outcome.raw {
        std::fs::write(raw_dir.join(format!("{}.json", slug(id))), pretty(value)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A result with no site (the card did not locate it).
    fn node(id: &str) -> ResultNode {
        ResultNode {
            id: id.to_string(),
            text: format!("{id} | graph.scip/compiler | p1.00 d1"),
            site: None,
        }
    }

    /// A result whose card gave its definition site.
    fn located(id: &str, path: &str, line: u64) -> ResultNode {
        ResultNode {
            site: Some(Site {
                path: path.to_string(),
                line,
            }),
            ..node(id)
        }
    }

    const SEARCH_FN: &str = "sym:rust:app::search::rerank_document/fn";
    const METHOD: &str = "sym:rust:lci-core::chunk::split/method";
    const INLINE_FN: &str = "sym:rust:app::search::inner::helper/fn";
    const TYPE: &str = "sym:rust:app::search::Hit/struct";
    const COMMIT: &str = "commit:0123456789abcdef0123456789abcdef01234567";
    const DOC: &str = "doc:AGENTS.md#hard-rules";

    /// The three symbols above, located.
    fn located_nodes() -> Vec<ResultNode> {
        vec![
            located(SEARCH_FN, "src/app/search.rs", 12),
            located(METHOD, "lci-core/src/chunk.rs", 30),
            located(INLINE_FN, "src/app/search.rs", 80),
            located(TYPE, "src/app/search.rs", 5),
        ]
    }

    #[test]
    fn symbols_become_declarations_at_their_site_and_without_one_are_not_offered() {
        let d = decl_of(&located(SEARCH_FN, "src/app/other.rs", 12)).unwrap();
        assert_eq!((d.path.as_str(), d.line), ("src/app/other.rs", 12));
        let m = decl_of(&located(METHOD, "lci-core/src/chunk.rs", 30)).unwrap();
        assert_eq!(m.kind, SymbolKind::Method);
        assert!(decl_of(&node(COMMIT)).is_none());
        // A symbol without a site is not offered.
        assert!(decl_of(&node(SEARCH_FN)).is_none());
        assert!(adapt_impact(&[node(SEARCH_FN)]).is_empty());
        assert!(adapt_flow(&[node(SEARCH_FN)]).is_empty());
    }

    #[test]
    fn impact_adapter_offers_declarations_and_dedups() {
        let mut nodes = located_nodes();
        nodes.push(located(SEARCH_FN, "src/app/search.rs", 12));
        nodes.push(node(COMMIT));
        let answers = adapt_impact(&nodes);
        assert_eq!(
            answers,
            vec![
                json!({"path": "src/app/search.rs", "line": 12}),
                json!({"path": "lci-core/src/chunk.rs", "line": 30}),
                json!({"path": "src/app/search.rs", "line": 80}),
                json!({"path": "src/app/search.rs", "line": 5}),
            ]
        );
    }

    #[test]
    fn flow_adapter_keeps_functions_and_methods_only() {
        let answers = adapt_flow(&located_nodes());
        let lines: Vec<(&str, u64)> = answers
            .iter()
            .map(|a| (a["path"].as_str().unwrap(), a["line"].as_u64().unwrap()))
            .collect();
        assert_eq!(
            lines,
            [
                ("src/app/search.rs", 12),
                ("lci-core/src/chunk.rs", 30),
                ("src/app/search.rs", 80)
            ]
        );
    }

    #[test]
    fn where_to_adapter_follows_the_item_form() {
        let nodes = located_nodes();
        let registration = json!({"gold": {"form": "registration"}});
        assert_eq!(
            adapt_where_to(&registration, &nodes),
            vec![
                json!({"path": "src/app/search.rs"}),
                json!({"path": "lci-core/src/chunk.rs"})
            ]
        );
        let convention = json!({"gold": {"form": "convention"}});
        assert_eq!(adapt_where_to(&convention, &nodes).len(), 2);
        let exemplar = json!({"gold": {"form": "exemplar", "granularity": "member"}});
        let by_member = adapt_where_to(&exemplar, &nodes);
        assert_eq!(by_member.len(), 3);
        assert_eq!(
            by_member[0],
            json!({"path": "src/app/search.rs", "line": 12})
        );
        let file_level = json!({"gold": {"form": "exemplar", "granularity": "file"}});
        assert_eq!(adapt_where_to(&file_level, &nodes).len(), 2);
    }

    #[test]
    fn history_and_instruction_adapters_read_their_node_kinds() {
        assert_eq!(
            adapt_history(&[node(SEARCH_FN), node(COMMIT), node(COMMIT)]),
            vec![json!({"commit": "0123456789abcdef0123456789abcdef01234567"})]
        );
        assert!(adapt_history(&[node(SEARCH_FN)]).is_empty());
        assert_eq!(
            adapt_instructions(&[node(DOC), node(SEARCH_FN)]),
            vec![json!({"path": "AGENTS.md"})]
        );
        assert!(adapt_instructions(&[node(SEARCH_FN)]).is_empty());
    }

    #[test]
    fn sites_are_read_from_the_cards_and_given_to_the_seeds() {
        let raw = json!({
            "status": "complete", "seeds": [SEARCH_FN],
            "results": [{"node": METHOD, "text": "t",
                         "site": {"path": "lci-core\\src\\chunk.rs", "line": 30, "col": 8}},
                        {"node": TYPE, "text": "t"}],
            "checkpoint": {"visited_cards": [{"node": SEARCH_FN, "text": "t",
                           "site": {"path": "src/app/search.rs", "line": 12, "col": 4}}]},
        });
        let traversed = Traversed::from_json(&raw, None);
        assert_eq!(
            traversed.results[0].site,
            Some(Site {
                path: "lci-core/src/chunk.rs".into(),
                line: 30
            })
        );
        assert_eq!(traversed.results[1].site, None);
        assert_eq!(traversed.sites[SEARCH_FN].line, 12);
        let mut cfg = config(&["graph.scip"]);
        cfg.include_seeds = true;
        let mut answers = BTreeMap::new();
        answers.insert("q".into(), raw);
        let items = vec![impact_item("i1", "q", "src/app/search.rs", "x")];
        let outcome = bench_set(Set::Impact, &items, &Fixture::new(answers), &cfg).unwrap();
        // The seed leads, located by its checkpoint card.
        assert_eq!(
            outcome.run["i1"][0],
            json!({"path": "src/app/search.rs", "line": 12})
        );
    }

    #[test]
    fn a_seed_keeps_its_site_on_a_run_that_returns_no_checkpoint() {
        // A completed run has no checkpoint: the seed's site comes from
        // `seed_sites`.
        let raw = json!({
            "status": "complete", "seeds": [SEARCH_FN],
            "seed_sites": {SEARCH_FN: {"path": "src\\app\\search.rs", "line": 12, "col": 4}},
            "results": [{"node": METHOD, "text": "t"}],
        });
        let traversed = Traversed::from_json(&raw, None);
        assert_eq!(traversed.sites[SEARCH_FN].line, 12);
        let mut cfg = config(&["graph.scip"]);
        cfg.include_seeds = true;
        let mut answers = BTreeMap::new();
        answers.insert("q".into(), raw);
        let items = vec![impact_item("i1", "q", "src/app/search.rs", "x")];
        let outcome = bench_set(Set::Impact, &items, &Fixture::new(answers), &cfg).unwrap();
        assert_eq!(
            outcome.run["i1"][0],
            json!({"path": "src/app/search.rs", "line": 12})
        );
    }

    #[test]
    fn precision_recall_and_f_half() {
        // p = 0.5, r = 1.0: F0.5 = 1.25 * 0.5 / (0.125 + 1.0)
        assert!((f_beta(0.5, 1.0, 0.5) - 0.625 / 1.125).abs() < 1e-12);
        assert!((f_beta(1.0, 0.5, 0.5) - 0.625 / 0.75).abs() < 1e-12);
        assert_eq!(f_beta(0.0, 0.0, 0.5), 0.0);
        assert_eq!(f_beta(0.0, 1.0, 0.5), 0.0);
        assert!((f_beta(1.0, 1.0, 0.5) - 1.0).abs() < 1e-12);
        // Precision weighs more than recall.
        assert!(f_beta(0.9, 0.3, 0.5) > f_beta(0.3, 0.9, 0.5));

        // Registration precision skips the neutral files.
        let item = json!({
            "gold": {"form": "registration", "expected_files": ["a.rs", "b.rs"]},
            "accept": {"preferred": [{"path": "t.rs"}], "related_sites": [{"files": ["r.rs"]}]}
        });
        let answers = [
            json!({"path": "a.rs"}),
            json!({"path": "t.rs"}),
            json!({"path": "x.rs"}),
            json!({"path": "r.rs"}),
        ];
        let (p, r) = registration_precision_recall(&item, &answers);
        assert!((p - 0.5).abs() < 1e-12 && (r - 0.5).abs() < 1e-12);
        assert_eq!(registration_precision_recall(&item, &[]), (0.0, 0.0));

        // History precision counts the accepted commits.
        let item = json!({"gold": {"commit": "0123456789abcdef"}, "accept": {}});
        let hit = json!({"commit": "0123456789abcdef0123456789abcdef01234567"});
        let miss = json!({"commit": "ffffffffffffffffffffffffffffffffffffffff"});
        assert_eq!(
            history_precision_recall(&item, &[hit.clone(), miss]),
            (0.5, 1.0)
        );
        assert_eq!(history_precision_recall(&item, &[hit]), (1.0, 1.0));
        assert_eq!(history_precision_recall(&item, &[]), (0.0, 0.0));
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sample = [10.0, 40.0, 20.0, 30.0];
        assert_eq!(percentile(&sample, 0.5), Some(20.0));
        assert_eq!(percentile(&sample, 0.95), Some(40.0));
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[7.0], 0.95), Some(7.0));
    }

    #[test]
    fn capabilities_follow_the_maps() {
        let strings = |maps: &[&str]| maps.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let scip = strings(&["seeds.scip_names", "graph.scip"]);
        assert!(can_answer(Set::Impact, &scip));
        assert!(can_answer(Set::WhereTo, &scip));
        assert!(!can_answer(Set::History, &scip));
        assert!(!can_answer(Set::Instructions, &scip));
        // The four maps the CLI turns on by default emit no commit and no
        // document section: history and instructions are structural zeros.
        let four = strings(&[
            "seeds.hybrid",
            "seeds.scip_names",
            "graph.scip",
            "structure.contains",
        ]);
        assert!(can_answer(Set::Impact, &four));
        assert!(!can_answer(Set::History, &four));
        assert!(!can_answer(Set::Instructions, &four));
        let mut with_git = four.clone();
        with_git.push("git.history".into());
        assert!(can_answer(Set::History, &with_git));
        // A map this runner does not know is run and judged by its results.
        assert!(can_answer(Set::History, &strings(&["git.log"])));
    }

    // ---- synthetic end-to-end run: tiny gold, fixture traversals, temp trend.

    /// Answers by question; a missing question is a CLI error, `"timeout"`
    /// as the value is a timeout. Maps in `down` fail the preflight; `defaults`
    /// is what the CLI would turn on. `calls` counts traversal calls (not
    /// preflights).
    struct Fixture {
        answers: BTreeMap<String, Value>,
        down: Vec<String>,
        defaults: Vec<String>,
        calls: std::cell::Cell<usize>,
    }

    impl Fixture {
        fn new(answers: BTreeMap<String, Value>) -> Fixture {
            Fixture {
                answers,
                down: Vec::new(),
                defaults: Vec::new(),
                calls: std::cell::Cell::new(0),
            }
        }
    }

    impl Traverser for Fixture {
        fn traverse(&self, _id: &str, question: &str) -> Result<(Traversed, Value), TraverseError> {
            self.calls.set(self.calls.get() + 1);
            match self.answers.get(question) {
                Some(v) if v == "timeout" => Err(TraverseError::Timeout(60)),
                Some(raw) => Ok((Traversed::from_json(raw, Some(12.0)), raw.clone())),
                None => Err(TraverseError::Cli("no fixture".into())),
            }
        }

        fn default_maps(&self, _question: &str) -> Result<Vec<String>, TraverseError> {
            Ok(self.defaults.clone())
        }

        fn preflight(&self, map: &str, _question: &str) -> Result<(), TraverseError> {
            if self.down.iter().any(|d| d == map) {
                Err(TraverseError::Cli(format!("{map} is down")))
            } else {
                Ok(())
            }
        }
    }

    /// Where the fixture symbols are defined: the gold declarations of the
    /// tests below span lines 10 to 20, the type is somewhere else.
    fn fixture_site(id: &str) -> Value {
        match id {
            SEARCH_FN => json!({"path": "src/app/search.rs", "line": 12, "col": 4}),
            METHOD => json!({"path": "lci-core/src/chunk.rs", "line": 12, "col": 4}),
            TYPE => json!({"path": "src/app/search.rs", "line": 300, "col": 4}),
            _ => Value::Null,
        }
    }

    fn traversal(seeds: &[&str], results: &[&str], pops: u64) -> Value {
        json!({
            "mode": "traverse", "status": "complete", "seeds": seeds,
            "results": results.iter().map(|id| json!({
                "node": id, "text": format!("{id} | graph.scip/compiler | p1.00 d1"),
                "via": "graph.scip", "witness": "compiler", "potential": 1.0, "depth": 1,
                "site": fixture_site(id),
            })).collect::<Vec<_>>(),
            "totals": {"pops": pops, "hops": 2, "cross_map_hops": 1, "stops": 1,
                       "tokens": 9, "steers": 0, "elapsed_ms": 3.5},
        })
    }

    fn impact_item(id: &str, question: &str, path: &str, name: &str) -> Value {
        json!({
            "id": id, "question": question, "primary": true,
            "stratum": "sym-fn/x/covered", "accept": {},
            "seed": {"path": "seed.rs", "name": "seed", "line": 1, "span": [1, 2], "symbol": "s"},
            "gold": {"r1": {"decls": [{"path": path, "name": name, "line": 10,
                                       "span": [10, 20], "symbol": "g"}],
                            "files": [path]}},
        })
    }

    fn config(maps: &[&str]) -> RunConfig {
        RunConfig {
            wave: "test-wave".into(),
            maps: maps.iter().map(|s| s.to_string()).collect(),
            maps_unavailable: Vec::new(),
            policy_label: "default".into(),
            under_load: false,
            include_seeds: false,
            note: String::new(),
            progress: false,
            rescored: false,
        }
    }

    fn counts(cells: &[String]) -> (&str, &str, &str) {
        (cells[2].as_str(), cells[3].as_str(), cells[4].as_str())
    }

    #[test]
    fn three_item_run_scores_and_appends_a_trend_row() {
        let items = vec![
            impact_item(
                "i1",
                "q one",
                "src/app/search.rs",
                "search::rerank_document",
            ),
            impact_item("i2", "q two", "lci-core/src/chunk.rs", "Splitter::split"),
            impact_item("i3", "q three", "src/lib.rs", "nothing"),
        ];
        let mut answers = BTreeMap::new();
        // i1: the one gold declaration plus one wrong one; i2: exactly right;
        // i3: no seeds (a scored empty answer, not a skip).
        answers.insert("q one".into(), traversal(&["x"], &[SEARCH_FN, TYPE], 6));
        answers.insert("q two".into(), traversal(&["x"], &[METHOD], 4));
        answers.insert(
            "q three".into(),
            json!({"mode": "traverse", "status": "no_seeds", "results": [], "seeds": []}),
        );
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(answers),
            &config(&["graph.scip", "seeds.scip_names"]),
        )
        .unwrap();

        // Run file: the gold scorer's format.
        assert_eq!(outcome.run["i1"].as_array().unwrap().len(), 2);
        assert_eq!(outcome.run["i3"], json!([]));
        // The in-process scorer saw all three items.
        assert_eq!(outcome.report["scored_items"], 3);
        assert_eq!(outcome.report["items_missing_from_run"], 0);
        // decl_recall: 1, 1, 0 -> 2/3.
        let recall = outcome.report["overall"]["metrics"]["decl_recall"]
            .as_f64()
            .unwrap();
        assert!((recall - 0.6667).abs() < 1e-4, "{recall}");

        let cells = &outcome.row.cells;
        assert_eq!(counts(cells), ("3", "3", "3"));
        assert_eq!(cells[5], "none");
        assert_eq!(cells[6], "2/3");
        // Precision: 0.5, 1.0, 0 -> 0.5. F0.5: 1.25*.5*1/(.125+1)=0.5556, 1.0, 0.
        assert_eq!(cells[8], "0.500");
        assert_eq!(cells[9], format!("{:.3}", (0.625 / 1.125 + 1.0) / 3.0));
        // Totals are sums over the items that had a traversal.
        assert_eq!((cells[10].as_str(), cells[11].as_str()), ("10", "4"));
        assert_eq!((cells[12].as_str(), cells[13].as_str()), ("2", "2"));
        assert_eq!(cells[15], "4/4");
        assert!(cells[20].contains("1 scored items found no seeds"));
        assert!(cells[7].contains("decl_recall=0.667") && cells[7].contains("(over 3)"));
        assert_eq!(cells[18], "none");

        let dir = std::env::temp_dir().join(format!("xtask-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let trend_path = dir.join("trend.md");
        trend::append(&trend_path, &outcome.row).unwrap();
        write_outputs(&dir.join("out"), &outcome).unwrap();
        let text = std::fs::read_to_string(&trend_path).unwrap();
        assert!(text.contains("| test-wave | impact | 3 | 3 | 3 | none | 2/3 |"));
        assert!(dir.join("out/run.json").exists() && dir.join("out/score.json").exists());
        assert!(dir.join("out/raw/i1.json").exists());
        // Re-running the same wave and set is refused.
        assert!(trend::append(&trend_path, &outcome.row).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skipped_items_are_counted_by_cause_and_stay_out_of_the_metrics() {
        let items = vec![
            impact_item("i1", "q ok", "src/app/search.rs", "search::rerank_document"),
            impact_item("i2", "q slow", "a.rs", "a"),
            impact_item("i3", "q broken", "b.rs", "b"),
            impact_item("i4", "q ok2", "lci-core/src/chunk.rs", "Splitter::split"),
        ];
        let mut answers = BTreeMap::new();
        answers.insert("q ok".into(), traversal(&["x"], &[SEARCH_FN], 5));
        answers.insert("q slow".into(), json!("timeout"));
        answers.insert("q ok2".into(), traversal(&["x"], &[METHOD], 5));
        let mut progress_config = config(&["graph.scip"]);
        progress_config.progress = true; // only prints to stderr
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(answers),
            &progress_config,
        )
        .unwrap();
        let cells = &outcome.row.cells;
        assert_eq!(counts(cells), ("4", "4", "2"));
        assert_eq!(cells[5], "timeout=1; cli_error=1");
        // Both scored items are perfect; the skipped ones do not pull the means down.
        assert!(cells[7].contains("decl_recall=1.000") && cells[7].contains("(over 2)"));
        assert_eq!(cells[8], "1.000");
        assert_eq!(cells[10], "10");
        assert_eq!(outcome.run.as_object().unwrap().len(), 2);
        assert_eq!(outcome.report["scored_items"], 2);
        // The reasons are kept in the raw output.
        let raw: BTreeMap<_, _> = outcome.raw.iter().cloned().collect();
        assert!(raw["i2"]["skipped"].as_str().unwrap().contains("timeout"));
        assert!(raw["i3"]["skipped"].as_str().unwrap().contains("cli error"));
        assert!(cells[20].contains("skipped items are not in any metric"));
        // A skip description names only the causes that occurred.
        assert_eq!(Skips::default().describe(), "none");
        let skips = Skips {
            timeout: 2,
            map_unavailable: 1,
            ..Skips::default()
        };
        assert_eq!(skips.describe(), "timeout=2; map_unavailable=1");
        assert_eq!(skips.total(), 3);
    }

    #[test]
    fn when_every_call_fails_the_row_says_nothing_was_scored() {
        let items = vec![impact_item("i1", "missing", "a.rs", "a")];
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(BTreeMap::new()),
            &config(&["graph.scip"]),
        )
        .unwrap();
        let cells = &outcome.row.cells;
        assert_eq!(counts(cells), ("1", "1", "0"));
        assert_eq!(cells[5], "cli_error=1");
        assert_eq!(cells[7], "none scored");
        assert_eq!(cells[8], "-");
        assert!(outcome.report.is_null());
    }

    #[test]
    fn a_set_no_map_can_answer_gets_the_no_map_row() {
        let items = vec![json!({"id": "h1", "question": "q", "gold": {"commit": "abc"}}); 2];
        let fixture = Fixture::new(BTreeMap::new());
        let outcome = bench_set(Set::History, &items, &fixture, &config(&["graph.scip"])).unwrap();
        let cells = &outcome.row.cells;
        assert_eq!(counts(cells), ("2", "0", "0"));
        assert_eq!(cells[5], "no_answer_type=2");
        assert_eq!(cells[7], NO_MAP);
        assert_eq!(cells[8], "-");
        assert_eq!(fixture.calls.get(), 0);
        assert!(outcome.report.is_null());
        // R13: the zero is flagged, not left to be averaged.
        assert!(cells[20].contains("structural-zero"), "{}", cells[20]);
    }

    #[test]
    fn preflight_marks_a_failing_map_once_and_it_is_not_used() {
        let mut fixture = Fixture::new(BTreeMap::new());
        fixture.down = vec!["graph.scip".into()];
        let maps: Vec<String> = ["seeds.scip_names", "graph.scip"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let unavailable = preflight_maps(&fixture, &maps, "q");
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].0, "graph.scip");
        assert!(unavailable[0].1.contains("is down"));

        // The seeds map still emits symbols, so the run goes ahead with it
        // and the row names the map that was not used.
        let mut cfg = config(&["seeds.scip_names", "graph.scip"]);
        cfg.maps_unavailable = unavailable;
        assert_eq!(cfg.available_maps(), ["seeds.scip_names"]);
        let mut answers = BTreeMap::new();
        answers.insert("q".into(), traversal(&["x"], &[SEARCH_FN], 1));
        let fixture = Fixture::new(answers);
        let items = vec![impact_item(
            "i1",
            "q",
            "src/app/search.rs",
            "search::rerank_document",
        )];
        let outcome = bench_set(Set::Impact, &items, &fixture, &cfg).unwrap();
        assert!(
            outcome.row.cells[18].starts_with("graph.scip (cli error: graph.scip is down"),
            "{}",
            outcome.row.cells[18]
        );
        assert_eq!(outcome.row.cells[4], "1");

        // With every map down no traversal is attempted: all items are skipped
        // with the cause map_unavailable.
        cfg.maps_unavailable = cfg
            .maps
            .iter()
            .map(|m| (m.clone(), "down".into()))
            .collect();
        let outcome = bench_set(Set::Impact, &items, &fixture, &cfg).unwrap();
        assert_eq!(fixture.calls.get(), 1);
        assert_eq!(outcome.row.cells[3], "0");
        assert_eq!(outcome.row.cells[5], "map_unavailable=1");
        assert!(outcome.row.cells[7].contains("map unavailable"));
    }

    #[test]
    fn under_load_puts_a_reminder_in_the_row() {
        let mut answers = BTreeMap::new();
        answers.insert("q".into(), traversal(&["x"], &[SEARCH_FN], 1));
        let mut cfg = config(&["graph.scip"]);
        cfg.under_load = true;
        let items = vec![impact_item(
            "i1",
            "q",
            "src/app/search.rs",
            "search::rerank_document",
        )];
        let outcome = bench_set(Set::Impact, &items, &Fixture::new(answers), &cfg).unwrap();
        assert!(outcome.row.cells[15].ends_with("(under load)"));
        assert!(outcome.row.cells[20].contains("under load"));
    }

    #[test]
    fn limit_and_population_select_items() {
        let items = vec![
            json!({"id": "a", "primary": false}),
            json!({"id": "b", "primary": true}),
            json!({"id": "c", "primary": true}),
        ];
        let ids = |v: Vec<Value>| {
            v.iter()
                .map(|i| i["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(select_items(Set::Impact, items.clone(), None)),
            ["b", "c"]
        );
        assert_eq!(
            ids(select_items(Set::Impact, items.clone(), Some(1))),
            ["b"]
        );
        assert_eq!(ids(select_items(Set::Flow, items, Some(2))), ["a", "b"]);
    }

    // ---- R11: declarations are identified by where the result says they are.

    fn card(id: &str, path: &str, line: u64) -> Value {
        json!({
            "node": id, "text": format!("{id} | graph.scip/compiler | p1.00 d1"),
            "site": {"path": path, "line": line, "col": 8},
        })
    }

    fn flow_item(id: &str, question: &str, name: &str, file: &str, line: u64) -> Value {
        json!({
            "id": id, "question": question, "accept": {},
            "gold": {
                "hops": [],
                "entry": {"name": "entry_fn", "file": "src/entry.rs", "line": 1, "symbol": "s-entry"},
                "target": {"name": "target_fn", "file": "src/target.rs", "line": 1, "symbol": "s-target"},
                "intermediates": [{"name": name, "file": file, "line": line, "symbol": "s-mid"}],
            },
        })
    }

    /// Two methods of one name in two impls of one file: ids that differ only
    /// by the disambiguator, sites that differ by line.
    const SPLIT_A: &str = "sym:rust:app::search::split/method";
    const SPLIT_B: &str = "sym:rust:app::search::split/method~1";

    fn results_of(cards: Vec<Value>) -> Vec<ResultNode> {
        Traversed::from_json(&json!({ "results": cards }), None).results
    }

    #[test]
    fn same_name_methods_in_different_containers_stay_distinct() {
        let nodes = results_of(vec![
            card(SPLIT_A, "src/app/search.rs", 10),
            card(SPLIT_B, "src/app/search.rs", 40),
        ]);
        assert_eq!(adapt_impact(&nodes).len(), 2);
        assert_eq!(adapt_flow(&nodes).len(), 2);
    }

    #[test]
    fn a_correct_flow_answer_is_not_a_miss_because_of_an_invented_prefix() {
        // The gold names the method by its impl type; the node id carries only
        // the module path, so the old adapter offered `search::split`.
        let items = vec![flow_item(
            "f1",
            "q flow",
            "Splitter::split",
            "src/app/search.rs",
            10,
        )];
        let mut answers = BTreeMap::new();
        answers.insert(
            "q flow".into(),
            json!({"mode": "traverse", "status": "complete", "seeds": ["x"],
                   "results": [card(SPLIT_A, "src/app/search.rs", 10)]}),
        );
        let outcome = bench_set(
            Set::Flow,
            &items,
            &Fixture::new(answers),
            &config(&["graph.scip"]),
        )
        .unwrap();
        let metrics = &outcome.report["overall"]["metrics"];
        assert_eq!(metrics["path_recall"].as_f64(), Some(1.0), "{metrics}");
        assert_eq!(metrics["spurious"].as_f64(), Some(0.0), "{metrics}");
    }

    #[test]
    fn a_declaration_is_found_in_the_file_the_result_names() {
        // `app::search` is both `search.rs` and `search/mod.rs`; the module
        // path cannot tell them apart, the site can.
        let items = vec![impact_item(
            "i1",
            "q",
            "src/app/search/mod.rs",
            "Splitter::split",
        )];
        let mut answers = BTreeMap::new();
        answers.insert(
            "q".into(),
            json!({"mode": "traverse", "status": "complete", "seeds": ["x"],
                   "results": [card(SPLIT_A, "src/app/search/mod.rs", 10)]}),
        );
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(answers),
            &config(&["graph.scip"]),
        )
        .unwrap();
        let metrics = &outcome.report["overall"]["metrics"];
        assert_eq!(metrics["decl_recall"].as_f64(), Some(1.0), "{metrics}");
        assert_eq!(metrics["file_f1"].as_f64(), Some(1.0), "{metrics}");
    }

    #[test]
    fn history_on_the_default_four_maps_is_a_structural_zero_not_a_scored_zero() {
        // The last bench ran history on these four maps and scored 0/110.
        let items = vec![json!({"id": "h1", "question": "q", "gold": {"commit": "abc"}}); 2];
        let fixture = Fixture::new(BTreeMap::new());
        let four = [
            "seeds.hybrid",
            "seeds.scip_names",
            "graph.scip",
            "structure.contains",
        ];
        let mut cfg = config(&four);
        cfg.note = "tree x".into();
        let outcome = bench_set(Set::History, &items, &fixture, &cfg).unwrap();
        assert_eq!(outcome.row.cells[7], NO_MAP);
        assert_eq!(outcome.row.cells[20], "tree x; structural-zero");
        assert_eq!(fixture.calls.get(), 0);
        // A set the maps can answer carries no such note.
        let mut answers = BTreeMap::new();
        answers.insert("q".into(), traversal(&["x"], &[SEARCH_FN], 1));
        let impact = vec![impact_item("i1", "q", "src/app/search.rs", "x")];
        let outcome =
            bench_set(Set::Impact, &impact, &Fixture::new(answers), &config(&four)).unwrap();
        assert!(!outcome.row.cells[20].contains("structural-zero"));
    }

    // ---- R12: the default maps are the CLI's own, and a preflight must show a call.

    #[test]
    fn without_maps_the_run_uses_the_list_the_cli_reports() {
        let mut fixture = Fixture::new(BTreeMap::new());
        fixture.defaults = vec!["seeds.hybrid".into(), "graph.scip".into()];
        assert_eq!(
            effective_maps(&[], &fixture, "q").unwrap(),
            ["seeds.hybrid", "graph.scip"]
        );
        // Maps asked for are used as given, without asking the CLI.
        assert_eq!(
            effective_maps(&["git.history".to_string()], &fixture, "q").unwrap(),
            ["git.history"]
        );
        // A CLI that reports nothing is an error, not an empty run.
        fixture.defaults.clear();
        assert!(effective_maps(&[], &fixture, "q").is_err());
    }

    #[test]
    fn the_start_event_lists_the_maps_a_call_had_on() {
        let trace = concat!(
            r#"{"ev":"start","maps":[{"enabled":true,"id":"seeds.hybrid","version":1},"#,
            r#"{"enabled":false,"id":"git.history","version":1}]}"#,
            "\n",
            r#"{"ev":"expand"}"#,
            "\n"
        );
        assert_eq!(
            parse_start_maps(trace),
            Some(vec![
                ("seeds.hybrid".to_string(), true),
                ("git.history".to_string(), false)
            ])
        );
        assert_eq!(parse_start_maps(r#"{"ev":"expand"}"#), None);
        assert_eq!(parse_start_maps(""), None);
    }

    fn plan(maps: &[(&str, bool)]) -> Vec<(String, bool)> {
        maps.iter().map(|(m, on)| (m.to_string(), *on)).collect()
    }

    #[test]
    fn a_map_the_call_never_ran_fails_the_probe() {
        let called = json!({"totals": {"per_map": {"graph.scip": {"expands": 3, "ms": 1.5}}}});
        let uncalled = json!({"totals": {"per_map": {"graph.scip": {"expands": 3, "ms": 1.5}}},
                              "notes": ["git.history is unavailable: no repository"]});
        let on = plan(&[("graph.scip", true), ("git.history", true)]);
        assert!(check_probe("graph.scip", &called, Some(&on)).is_ok());
        // A trace from before the count had a time beside it.
        let bare = json!({"totals": {"per_map": {"graph.scip": 3}}});
        assert!(check_probe("graph.scip", &bare, Some(&on)).is_ok());
        // Registered and on, but the start nodes never reached it.
        let error = check_probe("git.history", &uncalled, Some(&on)).unwrap_err();
        assert!(error.contains("never called"), "{error}");
        // Left out of the plan: the old preflight passed this (exit 0, a note).
        let left_out = plan(&[("graph.scip", true)]);
        let error = check_probe("git.history", &uncalled, Some(&left_out)).unwrap_err();
        assert!(
            error.contains("not registered") && error.contains("no repository"),
            "{error}"
        );
        let off = plan(&[("git.history", false)]);
        assert!(check_probe("git.history", &uncalled, Some(&off)).is_err());
        assert!(check_probe("git.history", &uncalled, None).is_err());
    }

    #[test]
    fn a_seeds_map_is_checked_by_the_start_nodes_it_gave() {
        let hybrid = json!({"seed_source": "hybrid", "seeds": ["chunk:a@b#0"]});
        assert!(check_probe("seeds.hybrid", &hybrid, None).is_ok());
        let fell_back = json!({"seed_source": "name_tokens", "seeds": ["sym:rust:a::b/fn"],
                               "seed_fallback": "the hybrid search failed"});
        let error = check_probe("seeds.hybrid", &fell_back, None).unwrap_err();
        assert!(error.contains("no start nodes"), "{error}");
        let none = json!({"seed_source": "name_tokens", "seeds": []});
        assert!(check_probe("seeds.scip_names", &none, None).is_err());
        let names = json!({"seed_source": "name_tokens", "seeds": ["sym:rust:a::b/fn"]});
        assert!(check_probe("seeds.scip_names", &names, None).is_ok());
    }

    #[test]
    fn rescoring_reads_the_raw_outputs_and_keeps_skips_as_skips() {
        let dir = std::env::temp_dir().join(format!("xtask-rescore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        let write = |name: &str, value: &Value| {
            std::fs::write(dir.join("raw").join(name), value.to_string()).unwrap();
        };
        write("i1.json", &traversal(&["x"], &[SEARCH_FN], 5));
        write("i2.json", &json!({"skipped": "timeout after 60s"}));
        write("i3.json", &json!({"skipped": "cli error: boom"}));
        let items = vec![
            impact_item("i1", "q1", "src/app/search.rs", "n"),
            impact_item("i2", "q2", "a.rs", "n"),
            impact_item("i3", "q3", "a.rs", "n"),
            impact_item("i4", "q4", "a.rs", "n"),
        ];
        let mut cfg = config(&["graph.scip"]);
        cfg.rescored = true;
        let raw = RawOutputs {
            dir: dir.join("raw"),
        };
        let outcome = bench_set(Set::Impact, &items, &raw, &cfg).unwrap();
        let cells = &outcome.row.cells;
        assert_eq!(counts(cells), ("4", "4", "1"));
        // The missing raw file is a cli error like the stored one.
        assert_eq!(cells[5], "timeout=1; cli_error=2");
        assert!(cells[7].contains("decl_recall=1.000"), "{}", cells[7]);
        assert_eq!(cells[10], "5", "totals come from the raw output");
        assert_eq!(cells[16], "-", "wall latency was not recorded");
        assert!(cells[20].contains("rescored"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- R14: the policy file the bench can pass to `lci traverse --policy`.

    #[test]
    fn the_ladder_policy_file_uses_only_the_keys_the_cli_accepts() {
        let text = include_str!("../../../docs/spike/multimap/v2/policies/ladder-rules-only.json");
        let policy: Value = serde_json::from_str(text).unwrap();
        // `SteerArgs` refuses unknown keys, so a stray one would fail every call.
        let allowed = [
            "rules",
            "ladder",
            "max_stops",
            "tau",
            "stop_every",
            "on_disagreement",
            "likely_complete",
        ];
        for key in policy.as_object().unwrap().keys() {
            assert!(allowed.contains(&key.as_str()), "unknown key {key}");
        }
        let ladder: Vec<&str> = policy["ladder"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(ladder, ["rules", "agent"]);
        assert!(policy["stop_every"].as_u64().unwrap() >= 1);
        assert!(policy["on_disagreement"].as_u64().unwrap() >= 1);
        assert!((1..=100).contains(&policy["likely_complete"].as_u64().unwrap()));
        let rules = policy["rules"].as_array().unwrap();
        assert!(rules.len() <= 64);
        assert!(
            rules
                .iter()
                .all(|r| r["when"].is_object() && r["then"].is_string())
        );
        // Tests and generated files are pruned; a catch-all keeps the run going
        // (a stop no rung can decide would end a CLI run).
        let pruned: Vec<&str> = rules
            .iter()
            .filter(|r| r["then"] == "prune")
            .filter_map(|r| r["when"]["path_glob"].as_str())
            .collect();
        assert!(pruned.iter().any(|g| g.contains("tests")));
        assert!(pruned.iter().any(|g| g.contains("generated")));
        assert_eq!(rules.last().unwrap()["then"], "continue");
        // No machine path in a committed policy.
        assert!(!text.contains(":\\") && !text.contains("/Users/"));
    }
}
