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

mod impl_units;

use self::impl_units::{ImplGrouper, is_impl_header};
use super::fit_priors::file_salt;
use super::trend::{self, NCOLS, TrendRow};
use crate::gold::{self, Answer, Set};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use lci_core::node::{NodeId, NodeKind, SymbolKind};
use lci_maps::traverse::priors::PriorTable;
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
    /// The trait-implementation set (private; not part of `all`).
    Traits,
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
            SetArg::Traits => vec![Set::Traits],
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
    /// Which gold set to run (all = the five frozen ones, in order; traits is
    /// run on its own).
    #[arg(long, value_enum)]
    set: SetArg,
    /// Directory holding impact.json, flow.json, where-to.json, history.json,
    /// instructions.json and traits.json (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The SCIP index of the frozen tree (not needed with --rescore).
    #[arg(long)]
    scip: Option<PathBuf>,
    /// The frozen tree the index describes (not needed with --rescore, where it
    /// is optional). For `--set traits` it is also the source root: Q1 method
    /// nodes of the SCIP graph carry no impl in their id, so their definition
    /// sites are read here to count the methods of one impl block as one
    /// answer. Without it they keep separate answer slots and the report warns.
    #[arg(long)]
    tree: Option<PathBuf>,
    /// Maps to use, comma separated (default: the maps the CLI turns on when
    /// it is not told, read from a probe call; with --rescore, name the maps
    /// the run used). Naming the opt-in `seeds.bm25` or `seeds.literal` turns
    /// its `[run_profile.seeds]` key on for the run, in a derived overlay under
    /// `<out-dir>/<wave>/overlay` (the `--config` file plus the key; its hash is in
    /// the row's note). The hash depends on the TOML serializer (keys
    /// re-sorted, comments dropped); `derived_from_sha256` in `score.json`
    /// keeps the provenance. A `--rescore` run does not derive one.
    #[arg(long, value_delimiter = ',')]
    maps: Vec<String>,
    /// A steering policy file for `lci traverse --policy`, or `default` for
    /// none.
    #[arg(long, default_value = "default")]
    policy: String,
    /// BENCH-ONLY, GOLD-AWARE ORACLE ARM. A directory of per-item steering
    /// policy files written by `maps oracle-policy` (`<slug of item id>.json`),
    /// one passed to `lci traverse --policy` for each item. The policies are
    /// built from the gold, so this violates the rule that the traversal never
    /// sees the gold: the row says ORACLE and the arm is a steering ceiling,
    /// not a result. An item with no file is skipped (cli_error). The
    /// `[traverse]` config must allow the policy's `max_stops` (the cap
    /// applies otherwise). Excludes --policy.
    #[arg(long)]
    policy_dir: Option<PathBuf>,
    /// Depth at which states are recorded, not expanded (`lci traverse
    /// --max-depth`; the deepest state expanded is at one less), up to the
    /// `[traverse]` cap. `--max-depth 1` expands the seeds once and returns
    /// their children (the one-hop arm; add `--include-seeds`). The row's note
    /// says `max-depth=N`.
    #[arg(long)]
    max_depth: Option<u32>,
    /// How results are ranked (`lci traverse --fusion`): `boost` is the CLI's
    /// own default and passes nothing; `log_odds` is capped log-odds fusion; `rrf` is the weighted reciprocal rank
    /// fusion. The row's note says when it is not `boost`.
    #[arg(long, default_value = "boost", value_parser = ["boost", "rrf", "log_odds"])]
    fusion: String,
    /// Pass through connective nodes (`lci traverse --pass-through`): files,
    /// commits and config keys are expanded without costing a pop. The row's
    /// note says when it is on.
    #[arg(long)]
    pass_through: bool,
    /// Rate the kept hops with the reranker (`lci traverse --rerank-frontier`;
    /// the floor is the config's `[traverse] rerank_floor`). The row's note
    /// says `rerank-frontier` and counts the reranker calls.
    #[arg(long)]
    rerank_frontier: bool,
    /// Switch the score bound off (`lci traverse --no-score-bound`). The row's
    /// note says `no-bound`.
    #[arg(long)]
    no_score_bound: bool,
    /// Write `pop` events into the CLI traces (`lci traverse --trajectory`;
    /// the input of `maps trajectories`). Evaluation runs only: no report file
    /// changes with it.
    #[arg(long)]
    trajectory: bool,
    /// A configuration overlay for every `lci traverse` call of the run
    /// (`lci --config`, the CLI's own file; no second format), so one arm can
    /// switch a `[run_profile]` key such as `[run_profile.walk] pricing` or
    /// `[run_profile.seeds] bm25`. The file must exist. The row's note says
    /// `config=<file stem>@<hash>` and `score.json` records the file's
    /// SHA-256, its path and the `profile_hash` the traversals reported, so
    /// two runs with different overlays never read as the same run.
    #[arg(long)]
    config: Option<PathBuf>,
    /// A table of fitted hop priors (`lci traverse --priors`, given
    /// `--question-type` the name of the set). The row's note says
    /// `priors=<file stem>`.
    #[arg(long)]
    priors: Option<PathBuf>,
    /// Run only the items in the table's hold-out list (impact: of the
    /// primary items). The row's note says `holdout-only`.
    #[arg(long, requires = "priors")]
    holdout_only: bool,
    /// The split salt the prior table was fitted with (`maps fit-priors
    /// --salt`). The hold-out list and the salt both come from the table; this
    /// only checks that they match, and a mismatch is an error. The row's note
    /// says `salt=<salt>` when the table has one.
    #[arg(long, requires = "holdout_only")]
    salt: Option<String>,
    /// A fitted tie-break file (`lci traverse --tie-break`) that orders results
    /// tied at the same rank. The row's note says `tie-break=<file stem>`.
    #[arg(long)]
    tie_break: Option<PathBuf>,
    /// Run only the first N items of each set (impact: of the primary items).
    #[arg(long)]
    limit: Option<usize>,
    /// Answers requested per item (`lci traverse --top-k`, at most 40).
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Score only the first N answers of each traversal (default: all of
    /// them), so `--top-k` can ask for more candidates, such as a pool to fit a
    /// tie-break on, while the metrics stay those of the first N.
    #[arg(long)]
    score_k: Option<usize>,
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
    /// The engine's first stage (`lci traverse --first-stage`): `off`, `append`
    /// or `interleave`. The answers are the engine's results in order, exact
    /// and seed cards included. The row's note says `first-stage=<mode>`.
    #[arg(long, value_parser = ["off", "append", "interleave"])]
    first_stage: Option<String>,
    /// `lci traverse --first-k` (0: no cap). The row's note says `first_k=N`.
    #[arg(long)]
    first_k: Option<u32>,
    /// `lci traverse --answer-seeds`: the engine answers with the start nodes
    /// after the exact cards (with a first stage other than `off`).
    #[arg(long)]
    answer_seeds: bool,
    /// `lci traverse --exact-seeds`: exact cards the graph holds start the walk.
    #[arg(long)]
    exact_seeds: bool,
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
    // Before any work: a missing overlay stops the run here.
    let overlay = args
        .config
        .as_deref()
        .map(ConfigOverlay::load)
        .transpose()?;
    let policy = match args.policy.as_str() {
        "default" => None,
        other => Some(other.to_string()),
    };
    if args.policy_dir.is_some() && policy.is_some() {
        bail!("--policy and --policy-dir exclude each other");
    }
    let trend_path = args
        .trend
        .clone()
        .unwrap_or_else(|| repo_root().join(DEFAULT_TREND));
    let sets = args.set.sets();
    let holdout = match (&args.priors, args.holdout_only) {
        (Some(path), true) => {
            file_salt(path, args.salt.as_deref())?;
            Some(
                PriorTable::load(path)
                    .with_context(|| format!("read the prior table {}", path.display()))?
                    .holdout,
            )
        }
        _ => None,
    };
    let mut gold_items = Vec::new();
    for set in &sets {
        gold_items.push(select_items(
            *set,
            gold::prepare(
                *set,
                gold::load_gold(&args.gold_dir.join(format!("{}.json", set_name(*set))))?,
            ),
            args.limit,
            holdout.as_ref(),
        ));
    }
    if let Some(dir) = &args.rescore {
        return rescore(&args, dir, sets[0], &gold_items[0], &trend_path, overlay);
    }

    let scip = args.scip.clone().context("--scip is required")?;
    let tree = args.tree.clone().context("--tree is required")?;
    let out_dir = args.out_dir.clone().context("--out-dir is required")?;
    // Naming an opt-in seed map turns its key on in this arm's overlay.
    let overlay = seed_map_overlay(
        &args.maps,
        overlay,
        &out_dir.join(slug(&args.wave)).join("overlay"),
    )?;
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
        config: overlay.as_ref().map(|o| o.path.clone()),
        policy_dir: args.policy_dir.clone(),
        max_depth: args.max_depth,
        fusion: fusion_arg(&args.fusion),
        pass_through: args.pass_through,
        rerank_frontier: args.rerank_frontier,
        no_score_bound: args.no_score_bound,
        trajectory: args.trajectory,
        priors: args
            .priors
            .clone()
            .map(|path| (path, set_name(sets[0]).to_owned())),
        tie_break: args.tie_break.clone(),
        first_stage: args.first_stage.clone(),
        first_k: args.first_k,
        answer_seeds: args.answer_seeds,
        exact_seeds: args.exact_seeds,
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
        policy_label: policy_label_of(&args),
        under_load: args.under_load,
        include_seeds: args.include_seeds,
        note: note_with_flags(
            args.note.as_deref(),
            &args.fusion,
            [args.pass_through, args.rerank_frontier],
            args.no_score_bound,
            extra_notes(&args, overlay.as_ref()),
        ),
        progress: true,
        rescored: false,
        score_k: args.score_k,
        tree: args.tree.clone(),
        overlay,
    };
    traverser.maps = config.available_maps();
    for (set, items) in sets.into_iter().zip(&gold_items) {
        // The table's column for this set is the set's name.
        if let Some((_, question_type)) = &mut traverser.priors {
            *question_type = set_name(set).to_owned();
        }
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
    overlay: Option<ConfigOverlay>,
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
        policy_label: policy_label_of(args),
        under_load: args.under_load,
        include_seeds: args.include_seeds,
        note: note_with_flags(
            args.note.as_deref(),
            &args.fusion,
            [args.pass_through, args.rerank_frontier],
            args.no_score_bound,
            extra_notes(args, overlay.as_ref()),
        ),
        progress: false,
        rescored: true,
        score_k: args.score_k,
        tree: args.tree.clone(),
        overlay,
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

pub(crate) fn default_lci() -> Result<PathBuf> {
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

/// The configuration overlay of a run (`--config`): the file the CLI reads
/// with its own `--config`, and its content hash, which is what makes the run's
/// identity differ from a run without it or with another file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOverlay {
    pub path: PathBuf,
    /// Lowercase hex SHA-256 of the file's bytes.
    pub sha256: String,
    /// The opt-in seed maps the bench turned on in this file because `--maps`
    /// names them (empty for a file the user wrote as it is).
    pub seed_maps_on: Vec<String>,
    /// SHA-256 of the `--config` file this one was derived from, if the user
    /// gave one.
    pub derived_from: Option<String>,
}

impl ConfigOverlay {
    /// Reads and hashes the file; a missing or unreadable one is an error, so
    /// the run stops before any work.
    pub fn load(path: &Path) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path)
            .with_context(|| format!("read the --config overlay {}", path.display()))?;
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            path: path.to_path_buf(),
            sha256,
            seed_maps_on: Vec::new(),
            derived_from: None,
        })
    }

    /// `config=<file stem>@<first 12 hex of the hash>`, for the row's note (the
    /// path is machine-specific and stays in `score.json`); a file the bench
    /// derived says which seed maps it turned on.
    fn note(&self) -> String {
        let stem = self
            .path
            .file_stem()
            .map_or_else(|| "overlay".into(), |s| s.to_string_lossy());
        let on = if self.seed_maps_on.is_empty() {
            String::new()
        } else {
            format!(" ({} on)", self.seed_maps_on.join(", "))
        };
        format!("config={stem}@{}{on}", &self.sha256[..12])
    }

    /// The `score.json` block: path, hash and the distinct `profile_hash`es
    /// the traversals reported (the effective run profile).
    fn record(&self, raw: &[(String, Value)]) -> Value {
        let profile_hashes: BTreeSet<&str> = raw
            .iter()
            .filter_map(|(_, value)| value["profile_hash"].as_str())
            .collect();
        let mut record = json!({
            "path": self.path.to_string_lossy(),
            "sha256": self.sha256,
            "profile_hashes": profile_hashes,
        });
        if !self.seed_maps_on.is_empty() {
            record["seed_maps_on"] = json!(self.seed_maps_on);
            record["derived_from_sha256"] = json!(self.derived_from);
        }
        record
    }
}

/// The opt-in seed maps `--maps` names, each with its `[run_profile.seeds]`
/// key.
fn named_opt_in_seed_maps(maps: &[String]) -> Vec<(&'static str, &'static str)> {
    OPT_IN_SEED_KEYS
        .iter()
        .filter(|(map, _)| maps.iter().any(|named| named == map))
        .copied()
        .collect()
}

/// `base` (the text of an overlay, or none) with `[run_profile.seeds] <key> =
/// true` for each key; the rest of the table stays. `None` when every key is
/// already true. Comments of `base` are not kept.
fn with_seed_keys_on(base: Option<&str>, keys: &[&str]) -> Result<Option<String>> {
    let mut table: toml::Table = match base {
        Some(text) => toml::from_str(text).context("parse the --config overlay as TOML")?,
        None => toml::Table::new(),
    };
    let profile = table
        .entry("run_profile")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("[run_profile] of the --config overlay is not a table")?;
    let seeds = profile
        .entry("seeds")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("[run_profile.seeds] of the --config overlay is not a table")?;
    if keys
        .iter()
        .all(|key| seeds.get(*key).and_then(toml::Value::as_bool) == Some(true))
    {
        return Ok(None);
    }
    for key in keys {
        seeds.insert((*key).to_owned(), toml::Value::Boolean(true));
    }
    Ok(Some(
        toml::to_string(&table).context("render the derived overlay")?,
    ))
}

/// Naming `seeds.bm25` or `seeds.literal` in `--maps` turns its
/// `[run_profile.seeds]` key on for the run (d461), since the map gives no
/// start nodes while the key is off. When the `--config` overlay (if any) does
/// not already have the key on, the effective overlay becomes a file written to
/// `dir`: the user's overlay with the key on, or only the key when there is
/// none. The row's `config=` note and `score.json` carry the hash of that file,
/// so the arm cannot read as a run without it. No other key and no default
/// changes; a run that names neither map gets the overlay it had.
fn seed_map_overlay(
    maps: &[String],
    overlay: Option<ConfigOverlay>,
    dir: &Path,
) -> Result<Option<ConfigOverlay>> {
    let named = named_opt_in_seed_maps(maps);
    if named.is_empty() {
        return Ok(overlay);
    }
    let base = match &overlay {
        Some(user) => Some(
            std::fs::read_to_string(&user.path)
                .with_context(|| format!("read the --config overlay {}", user.path.display()))?,
        ),
        None => None,
    };
    let keys: Vec<&str> = named.iter().map(|(_, key)| *key).collect();
    let Some(text) = with_seed_keys_on(base.as_deref(), &keys)? else {
        return Ok(overlay);
    };
    let name = match overlay.as_ref().and_then(|user| user.path.file_stem()) {
        Some(stem) => format!("{}+seeds-on.toml", stem.to_string_lossy()),
        None => "seeds-on.toml".to_owned(),
    };
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create the overlay directory {}", dir.display()))?;
    let path = dir.join(name);
    std::fs::write(&path, text)
        .with_context(|| format!("write the derived overlay {}", path.display()))?;
    let mut derived = ConfigOverlay::load(&path)?;
    derived.seed_maps_on = named.iter().map(|(map, _)| (*map).to_owned()).collect();
    derived.derived_from = overlay.map(|user| user.sha256);
    Ok(Some(derived))
}

/// The `--fusion` value to pass the CLI: none for its default.
fn fusion_arg(fusion: &str) -> Option<String> {
    (fusion != "boost").then(|| fusion.to_owned())
}

/// The note of the row, with the fusion when it is not the default and
/// `pass-through` and `rerank-frontier` when they are on.
fn note_with_flags(
    note: Option<&str>,
    fusion: &str,
    [pass_through, rerank_frontier]: [bool; 2],
    no_score_bound: bool,
    extras: Vec<Option<String>>,
) -> String {
    let flags = [
        fusion_arg(fusion).map(|fusion| format!("fusion {fusion}")),
        pass_through.then(|| "pass-through".to_owned()),
        rerank_frontier.then(|| "rerank-frontier".to_owned()),
        no_score_bound.then(|| "no-bound".to_owned()),
    ];
    let parts = note
        .filter(|note| !note.is_empty())
        .map(str::to_owned)
        .into_iter()
        .chain(flags.into_iter().chain(extras).flatten());
    parts.collect::<Vec<_>>().join("; ")
}

/// `priors=<file stem>`, `holdout-only` and the table's split salt (when it
/// has one), for the row's note.
fn priors_note(args: &BenchArgs) -> Option<String> {
    let path = args.priors.as_ref()?;
    let stem = path.file_stem()?.to_string_lossy();
    let holdout = if args.holdout_only {
        let salt = file_salt(path, None).unwrap_or_default();
        if salt.is_empty() {
            ", holdout-only".to_owned()
        } else {
            format!(", holdout-only, salt={salt}")
        }
    } else {
        String::new()
    };
    Some(format!("priors={stem}{holdout}"))
}

/// The note parts that come from the config overlay, the model files, the
/// depth limit and the oracle arm: `config=`, priors, tie-break, `max-depth=N` and the ORACLE marker.
fn extra_notes(args: &BenchArgs, overlay: Option<&ConfigOverlay>) -> Vec<Option<String>> {
    vec![
        overlay.map(ConfigOverlay::note),
        priors_note(args),
        tie_break_note(args),
        args.max_depth.map(|depth| format!("max-depth={depth}")),
        first_stage_note(args),
        args.answer_seeds.then(|| "answer-seeds".to_owned()),
        args.exact_seeds.then(|| "exact-seeds".to_owned()),
        args.policy_dir.as_ref().map(|_| {
            "ORACLE: per-item policies built from the gold (bench-only, gold-aware steering ceiling)"
                .to_owned()
        }),
    ]
}

/// The policy column of the row: the policy file's stem, `default`, or for
/// `--policy-dir` `oracle:<directory name>`.
fn policy_label_of(args: &BenchArgs) -> String {
    match &args.policy_dir {
        Some(dir) => format!(
            "oracle:{}",
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("dir")
        ),
        None => policy_label(&args.policy),
    }
}

/// `first-stage=<mode> first_k=N`, for the row's note, when a first-stage flag
/// was given (`first_k=config` when only the configuration's cap applies).
fn first_stage_note(args: &BenchArgs) -> Option<String> {
    if args.first_stage.is_none() && args.first_k.is_none() {
        return None;
    }
    let mode = args.first_stage.as_deref().unwrap_or("config");
    let k = args
        .first_k
        .map_or_else(|| "config".to_owned(), |k| k.to_string());
    Some(format!("first-stage={mode} first_k={k}"))
}

/// The engine's exact operators: descriptor-only maps whose cards are the first
/// stage, never walked and so never called.
const EXACT_MAPS: [&str; 2] = ["exact.closure", "exact.pickaxe"];

/// Items whose response says an exact operator answered (made cards), of the
/// items that returned one; `None` when the run selected no exact map.
fn exact_answered(maps: &[String], raws: &[&Value]) -> Option<String> {
    if !maps.iter().any(|m| EXACT_MAPS.contains(&m.as_str())) {
        return None;
    }
    let answered = raws
        .iter()
        .filter(|raw| {
            raw["exact"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(|op| op["answered"] == true))
        })
        .count();
    Some(format!("exact answered={answered}/{}", raws.len()))
}

/// `tie-break=<file stem>`, for the row's note.
fn tie_break_note(args: &BenchArgs) -> Option<String> {
    let stem = args.tie_break.as_ref()?.file_stem()?.to_string_lossy();
    Some(format!("tie-break={stem}"))
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

pub(crate) fn slug(text: &str) -> String {
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
        Set::Traits => "traits",
    }
}

/// The scored population of a set (impact keeps its `primary` items), limited
/// to the ids in `holdout` when given, cut to the first `limit`.
pub(super) fn select_items(
    set: Set,
    items: Vec<Value>,
    limit: Option<usize>,
    holdout: Option<&BTreeSet<String>>,
) -> Vec<Value> {
    let kept = items.into_iter().filter(|item| {
        let primary = match set {
            Set::Impact => item.get("primary").and_then(Value::as_bool) == Some(true),
            _ => true,
        };
        primary && holdout.is_none_or(|ids| item["id"].as_str().is_some_and(|id| ids.contains(id)))
    });
    match limit {
        Some(n) => kept.take(n).collect(),
        None => kept.collect(),
    }
}

// ------------------------------------------------------------ capabilities

/// The maps that only find start nodes; they are never expanded, so they
/// show up in a call by the seeds they gave, not by calls counted.
const SEED_MAPS: [&str; 4] = [
    "seeds.hybrid",
    "seeds.scip_names",
    "seeds.bm25",
    "seeds.literal",
];

/// The seed maps that are off until their `[run_profile.seeds]` key is on,
/// each with that key (d461).
const OPT_IN_SEED_KEYS: [(&str, &str); 2] = [("seeds.bm25", "bm25"), ("seeds.literal", "literal")];

/// The node kinds a known map emits (the `emits` of its descriptor); `None`
/// for a map this runner does not know (it is run and judged by its results).
fn map_emits(map: &str) -> Option<&'static [NodeKind]> {
    use NodeKind::{Chunk, Commit, ConfigKey, File, Symbol};
    match map {
        "seeds.scip_names" | "graph.scip" | "scip.impact" | "scip.implements"
        | "tests.tested_by" | "exact.closure" => Some(&[Symbol]),
        "exact.pickaxe" => Some(&[Commit]),
        "seeds.hybrid" => Some(&[Symbol, Chunk, File]),
        "seeds.bm25" => Some(&[Symbol]),
        "seeds.literal" => Some(&[Symbol, File]),
        "structure.contains" | "grep.live" | "grep.structural" | "probe.compile" => {
            Some(&[Symbol, File])
        }
        "git.history" => Some(&[Commit, File]),
        "docs.mentions" => Some(&[File]),
        "config.key" => Some(&[ConfigKey, Symbol]),
        "search.lexical" | "search.vector" => Some(&[Chunk]),
        _ => None,
    }
}

/// The node kinds from which a set's answers can be made.
fn needed_kinds(set: Set) -> &'static [NodeKind] {
    match set {
        Set::Impact | Set::Flow | Set::WhereTo | Set::Traits => &[NodeKind::Symbol, NodeKind::File],
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
    pub(super) fn from_json(value: &Value) -> Option<Site> {
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
    /// Pops of connective nodes that did not count toward `pops` (0 unless
    /// `--pass-through` was on); total expansions = `pops + pass_through_pops`.
    pub pass_through_pops: u64,
    pub elapsed_ms: f64,
    /// External calls the walk's allowance paid for (`totals.external_calls`) and
    /// the ones that physically ran (`totals.physical_external_calls`).
    pub external_calls: u32,
    pub physical_external_calls: u32,
    /// The reranker frontier's counts (0 when it was off).
    pub rerank_calls: u64,
    pub rerank_docs: u64,
    pub rerank_failures: u64,
}

impl Totals {
    /// Model calls of the traversal: the reranker frontier's.
    pub fn model_calls(&self) -> u64 {
        self.rerank_calls
    }
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
    /// Milliseconds each map took in the call (`totals.per_map.<id>.ms`); a map
    /// that is not there did not run, and the first traces kept a bare count.
    pub map_ms: BTreeMap<String, f64>,
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
                pass_through_pops: t["pass_through_pops"].as_u64().unwrap_or(0),
                elapsed_ms: t["elapsed_ms"].as_f64().unwrap_or(0.0),
                external_calls: count_u32(&t["external_calls"]),
                physical_external_calls: count_u32(&t["physical_external_calls"]),
                rerank_calls: t["rerank_calls"].as_u64().unwrap_or(0),
                rerank_docs: t["rerank_docs"].as_u64().unwrap_or(0),
                rerank_failures: t["rerank_failures"].as_u64().unwrap_or(0),
            });
        let map_ms = value["totals"]["per_map"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(id, stat)| Some((id.clone(), stat["ms"].as_f64()?)))
            .collect();
        Traversed {
            status: value["status"].as_str().unwrap_or("unknown").to_string(),
            seeds: strings(&value["seeds"]),
            results,
            sites,
            totals,
            map_ms,
            wall_ms,
        }
    }
}

/// A count of the response, saturated at `u32::MAX`; 0 when it is absent.
fn count_u32(value: &Value) -> u32 {
    value
        .as_u64()
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

/// The note a call carries once its external-call allowance was spent
/// (`src/app/traverse/response.rs`; the `external_spent` events behind it are
/// not in the response). A spent allowance never ends a walk: it goes on with
/// the maps that need no external call, so the note says which maps went
/// unasked, not why the call ended (`stop_reason` says that).
const EXTERNAL_SPENT_NOTE: &str = "the external-call allowance was spent";

/// Whether a reply's `notes` hold one that contains `needle`.
fn has_note(raw: &Value, needle: &str) -> bool {
    raw["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|note| note.contains(needle))
}

/// Which budget cut an item's walk (or its live language-server calls) short.
/// Such an item was not measured to its natural end: `maps compare` leaves it
/// out like a deadline stop unless `--keep-failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetHit {
    /// The call ended on the pop cap (`stop_reason = pops`).
    Pops,
    /// The call ended on the token budget (`stop_reason = budget`; the engine
    /// ends a walk on `budget` only for tokens or an unadopted result).
    Tokens,
    /// The external-call allowance was spent (the note in `notes`) in a walk that
    /// ended some other way than on a budget, the deadline or the pop cap: the
    /// maps that need an external call went unasked from then on.
    ExternalCalls,
    /// The call ended on its deadline (`stop_reason = deadline`).
    Deadline,
    /// The walk went on, but `lsp.live` ran out of the deadline on a node (its
    /// note is in `notes`): the map's edges are partial.
    LspDeadline,
}

impl BudgetHit {
    /// The value written to `items.json` (`budget_hit`) and the key of the
    /// per-stratum `budget_hits` counts.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pops => "pops",
            Self::Tokens => "tokens",
            Self::ExternalCalls => "external_calls",
            Self::Deadline => "deadline",
            Self::LspDeadline => "lsp_deadline",
        }
    }

    /// Reads the hit from a raw traversal reply. Precedence: the cause that
    /// ended the walk (`stop_reason` `pops`, `budget` or `deadline`), then a
    /// spent external-call allowance, then the `lsp.live` deadline note. Only one
    /// value is kept; [`external_spent`] tells whether the allowance was spent
    /// whatever else ended the walk.
    pub fn from_raw(raw: &Value) -> Option<Self> {
        match raw["stop_reason"].as_str() {
            Some("pops") => Some(Self::Pops),
            Some("budget") => Some(Self::Tokens),
            Some("deadline") => Some(Self::Deadline),
            _ if external_spent(raw) => Some(Self::ExternalCalls),
            _ => has_note(raw, lci_maps::lsp_live::DEADLINE_NOTE).then_some(Self::LspDeadline),
        }
    }
}

/// Whether the call's external-call allowance was spent, whatever ended the walk.
pub fn external_spent(raw: &Value) -> bool {
    has_note(raw, EXTERNAL_SPENT_NOTE)
}

/// What makes one item's result weak evidence: the walk was cut by the
/// deadline (machine load, not the algorithm) or another budget (`budget_hit`),
/// or calls it depended on failed.
/// Read from the raw traversal JSON; map failures need the checkpoint, which
/// only a run that returns one has.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ItemFlags {
    /// `budget_hit == Some(Deadline)`, kept as its own field for the callers that
    /// count deadline stops.
    pub deadline: bool,
    pub rerank_failures: u64,
    pub map_failures: u64,
    pub budget_hit: Option<BudgetHit>,
    /// The external-call allowance was spent, whatever ended the walk (it is
    /// `budget_hit` too only when nothing else ended the walk).
    pub external_spent: bool,
}

impl ItemFlags {
    pub fn from_raw(raw: &Value) -> Self {
        let map_failures = raw
            .pointer("/checkpoint/maps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|stat| stat["failures"].as_u64())
            .sum();
        let budget_hit = BudgetHit::from_raw(raw);
        Self {
            deadline: budget_hit == Some(BudgetHit::Deadline),
            // The reranker frontier's failed calls make a row unreliable.
            rerank_failures: raw["totals"]["rerank_failures"].as_u64().unwrap_or(0),
            map_failures,
            budget_hit,
            external_spent: external_spent(raw),
        }
    }

    pub fn any(&self) -> bool {
        self.deadline
            || self.budget_hit.is_some()
            || self.external_spent
            || self.rerank_failures > 0
            || self.map_failures > 0
    }

    /// The names of what flags the item, for the counts `maps compare` prints:
    /// the budget hit, `external_spent` (also when another cause ended the walk),
    /// `rerank_failures` and `map_failures`.
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> =
            self.budget_hit.map(BudgetHit::as_str).into_iter().collect();
        if self.deadline && self.budget_hit != Some(BudgetHit::Deadline) {
            kinds.push("deadline");
        }
        if self.external_spent && self.budget_hit != Some(BudgetHit::ExternalCalls) {
            kinds.push("external_calls");
        }
        if self.rerank_failures > 0 {
            kinds.push("rerank_failures");
        }
        if self.map_failures > 0 {
            kinds.push("map_failures");
        }
        kinds
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

/// The part of the note `seeds.literal` writes when the question names nothing
/// in code spans.
const LITERAL_NO_NAMES: &str = "names nothing in code spans";

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
        let wanted = match map {
            "seeds.hybrid" => "hybrid",
            "seeds.bm25" => "bm25",
            "seeds.literal" => "literal",
            _ => "name_tokens",
        };
        let source = response["seed_source"].as_str().unwrap_or("none");
        let n_seeds = response["seeds"].as_array().map_or(0, Vec::len);
        // `seeds.literal` searches the names a question puts in code spans: a
        // probe question with none gives no seeds although the map ran (a map
        // that is off says so in another note and still fails).
        let nothing_to_search = map == "seeds.literal"
            && n_seeds == 0
            && response["notes"].as_array().is_some_and(|notes| {
                notes
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|note| note.contains(LITERAL_NO_NAMES))
            });
        if nothing_to_search {
            return Ok(());
        }
        if source != wanted || n_seeds == 0 {
            let fallback = response["seed_fallback"]
                .as_str()
                .map(|why| format!(": {why}"))
                .unwrap_or_default();
            // The map's own note says why (off, no ripgrep, no names).
            let own = response["notes"]
                .as_array()
                .and_then(|notes| {
                    notes
                        .iter()
                        .filter_map(Value::as_str)
                        .find(|note| note.starts_with(map))
                })
                .map(|note| format!(" (note: {note})"))
                .unwrap_or_default();
            return Err(format!(
                "{map} gave no start nodes for the probe question (seed_source {source}, {n_seeds} seeds{fallback}){own}"
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
    // The exact operators are never walked: registered and on is the check.
    if EXACT_MAPS.contains(&map) {
        return Ok(());
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
    /// `--config` for the CLI: the run's configuration overlay. Nothing is
    /// passed without it (the CLI keeps its own lookup, `LCI_CONFIG` included).
    pub config: Option<PathBuf>,
    /// A directory of per-item policy files (`<slug of item id>.json`), the
    /// bench-only oracle arm; excludes `policy`.
    pub policy_dir: Option<PathBuf>,
    /// `--max-depth` for the CLI.
    pub max_depth: Option<u32>,
    /// `--fusion` for the CLI; `None` leaves its default.
    pub fusion: Option<String>,
    /// `--pass-through` for the CLI.
    pub pass_through: bool,
    /// `--rerank-frontier` for the CLI.
    pub rerank_frontier: bool,
    /// `--no-score-bound` for the CLI.
    pub no_score_bound: bool,
    /// `--trajectory` for the CLI: `pop` events in the trace. Not part of the
    /// run config: the report files do not change with it.
    pub trajectory: bool,
    /// `--priors` and `--question-type` for the CLI: the table file and the
    /// question type (the name of the set being run).
    pub priors: Option<(PathBuf, String)>,
    /// `--tie-break` for the CLI.
    pub tie_break: Option<PathBuf>,
    /// `--first-stage`, `--first-k`, `--answer-seeds` and `--exact-seeds` for
    /// the CLI; unset leaves the configuration's.
    pub first_stage: Option<String>,
    pub first_k: Option<u32>,
    pub answer_seeds: bool,
    pub exact_seeds: bool,
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
    /// The `lci traverse` command of one call. `maps` empty leaves the CLI to
    /// choose; `seeds` empty lets it find the start nodes from the question;
    /// `policy_file` (an item's oracle policy) wins over `--policy`.
    fn command(
        &self,
        maps: &[String],
        seeds: &[String],
        question: &str,
        policy_file: Option<&Path>,
    ) -> Command {
        let mut command = Command::new(&self.lci);
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
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
        match (policy_file, &self.policy) {
            (Some(file), _) => {
                command.arg("--policy").arg(file);
            }
            (None, Some(policy)) => {
                command.arg("--policy").arg(policy);
            }
            (None, None) => {}
        }
        if let Some(depth) = self.max_depth {
            command.arg(format!("--max-depth={depth}"));
        }
        if let Some(fusion) = &self.fusion {
            command.arg(format!("--fusion={fusion}"));
        }
        if self.pass_through {
            command.arg("--pass-through");
        }
        if self.rerank_frontier {
            command.arg("--rerank-frontier");
        }
        if self.no_score_bound {
            command.arg("--no-score-bound");
        }
        if self.trajectory {
            command.arg("--trajectory");
        }
        if let Some((table, question_type)) = &self.priors {
            command.arg("--priors").arg(table);
            command.arg(format!("--question-type={question_type}"));
        }
        if let Some(file) = &self.tie_break {
            command.arg("--tie-break").arg(file);
        }
        if let Some(mode) = &self.first_stage {
            command.arg(format!("--first-stage={mode}"));
        }
        if let Some(k) = self.first_k {
            command.arg(format!("--first-k={k}"));
        }
        if self.answer_seeds {
            command.arg("--answer-seeds");
        }
        if self.exact_seeds {
            command.arg("--exact-seeds");
        }
        command
    }

    /// One `lci traverse` call (see [`Self::command`]).
    fn invoke(
        &self,
        maps: &[String],
        seeds: &[String],
        question: &str,
        policy_file: Option<&Path>,
    ) -> Result<(Traversed, Value), TraverseError> {
        let mut command = self.command(maps, seeds, question, policy_file);
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
        let (traversed, _) = self.invoke(&maps, &[], question, None)?;
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
    fn traverse(&self, id: &str, question: &str) -> Result<(Traversed, Value), TraverseError> {
        let file = match &self.policy_dir {
            Some(dir) => {
                let file = dir.join(format!("{}.json", slug(id)));
                if !file.is_file() {
                    return Err(TraverseError::Cli(format!(
                        "no oracle policy file for {id}: {}",
                        file.display()
                    )));
                }
                Some(file)
            }
            None => None,
        };
        self.invoke(&self.maps, &[], question, file.as_deref())
    }

    fn default_maps(&self, question: &str) -> Result<Vec<String>, TraverseError> {
        let seeds = self.probe_seeds(question)?;
        let (_, raw) = self.invoke(&[], &seeds.nodes, question, None)?;
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
        let (_, raw) = self.invoke(&[map.to_string()], &seeds, question, None)?;
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
        Set::Traits => adapt_traits(item, nodes),
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

/// The answer unit of a Q1 node: a symbol inside an impl block stands for that
/// impl, named by its module path and container chain up to the innermost
/// `impl` header (the node id carries the container, so no span is needed);
/// any other node stands for itself.
fn impl_unit(id: &str) -> String {
    if let Some(NodeId::Symbol(key)) = parsed(id)
        && let Some(last) = key.container.iter().rposition(|s| is_impl_header(s))
    {
        return format!(
            "{}|{}",
            key.module_path.join("::"),
            key.container[..=last].join("/")
        );
    }
    id.to_string()
}

/// Traits: Q1 ("which types implement") offers every located symbol, because
/// the scorer credits an impl block or a type declaration. One answer per
/// distinct impl or type: a method (or other member) node counts as the impl
/// that contains it, read from the container chain of its node id, and the
/// duplicates collapse here, before the scorer's budget cut, keeping the best
/// ranked node's site. Impls of different types at one macro call site have
/// different headers, so they stay apart. A type node and the methods of its
/// impl are not merged (the id does not tie them). Q2 and Q3 want an
/// implementing member, so only functions and methods are offered, each kept.
/// Each answer carries its node id as `symbol`: the nodes of one macro call
/// site share a site, and the scorer needs them apart. The trait's own
/// declaration card is offered like any other; the scorer drops it.
///
/// This form reads only the node ids. A SCIP method node (default ids) has no
/// container in its id, so it stays its own unit here; [`adapt_traits_in`]
/// places it in its impl block through the source tree.
pub fn adapt_traits(item: &Value, nodes: &[ResultNode]) -> Vec<Value> {
    adapt_traits_in(item, nodes, None)
}

/// The unit of a Q1 node: the impl its id names, else (a method or function
/// node whose id names none) the impl block its definition site sits in, read
/// from the evaluated tree's source, else the node itself.
fn q1_unit(node: &ResultNode, decl: &Decl, grouper: Option<&mut ImplGrouper>) -> String {
    let unit = impl_unit(&node.id);
    if unit != node.id || !matches!(decl.kind, SymbolKind::Function | SymbolKind::Method) {
        return unit;
    }
    grouper
        .and_then(|grouper| grouper.unit(&decl.path, decl.line))
        .unwrap_or(unit)
}

/// [`adapt_traits`] with the source-tree grouping of Q1 method nodes whose ids
/// carry no impl (see [`ImplGrouper`]); without a grouper it is the same.
fn adapt_traits_in(
    item: &Value,
    nodes: &[ResultNode],
    mut grouper: Option<&mut ImplGrouper>,
) -> Vec<Value> {
    let members = matches!(item["set"].as_str(), Some("Q2" | "Q3"));
    let mut units = BTreeSet::new();
    dedup(
        nodes
            .iter()
            .filter_map(|n| {
                let decl = decl_of(n)?;
                if members {
                    if !matches!(decl.kind, SymbolKind::Function | SymbolKind::Method) {
                        return None;
                    }
                } else if !units.insert(q1_unit(n, &decl, grouper.as_deref_mut())) {
                    return None;
                }
                let mut a = decl_answer(&decl);
                a["symbol"] = json!(n.id);
                Some(a)
            })
            .collect(),
    )
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

/// Precision, F0.5 and the per-item record of `items.json` for one scored
/// item: the scorer's metrics of the item alone plus `precision`, `f0.5` and,
/// when known, `expansions`. `maps compare` rebuilds the record from `run.json`
/// with this same function for runs that predate `items.json`.
pub(super) fn item_record(
    set: Set,
    item: &Value,
    answers: &[Value],
    expansions: Option<u64>,
) -> Result<(f64, f64, Map<String, Value>)> {
    let metrics = item_metrics(set, item, answers)?;
    let (p, r) = item_precision_recall(set, item, answers, &metrics);
    let f05 = f_beta(p, r, 0.5);
    let mut record: Map<String, Value> = metrics
        .iter()
        .map(|(name, value)| (name.clone(), json!(value)))
        .collect();
    record.insert("precision".into(), json!(p));
    record.insert("f0.5".into(), json!(f05));
    if let Some(n) = expansions {
        record.insert("expansions".into(), json!(n));
    }
    Ok((p, f05, record))
}

/// What one scored item cost, read from its traversal reply: the numbers the
/// per-item record of `items.json` and the per-stratum means carry. All of it is
/// in the raw reply already; nothing here is measured again.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemCosts {
    /// Pops plus pass-through pops.
    pub expansions: u64,
    /// Reranker frontier calls.
    pub model_calls: u64,
    /// External calls the allowance paid for, and the ones that physically ran.
    pub external_calls: u32,
    pub physical_external_calls: u32,
    /// Milliseconds `lsp.live` took in the call (0 when the map did not run).
    pub lsp_ms: f64,
    /// Characters of the answer nodes the bench scored, divided by four and
    /// rounded up (the one computation behind the per-item number and the set total).
    pub answer_tokens: u64,
    /// The budget that cut the walk short, if one did.
    pub budget_hit: Option<BudgetHit>,
    /// The external-call allowance was spent, whatever ended the walk.
    pub external_spent: bool,
}

impl ItemCosts {
    pub fn of(traversed: &Traversed, flags: &ItemFlags, answer_tokens: u64) -> Self {
        let totals = traversed.totals.clone().unwrap_or_default();
        Self {
            expansions: totals.pops + totals.pass_through_pops,
            model_calls: totals.model_calls(),
            external_calls: totals.external_calls,
            physical_external_calls: totals.physical_external_calls,
            lsp_ms: traversed.map_ms.get(LSP_LIVE_MAP).copied().unwrap_or(0.0),
            answer_tokens,
            budget_hit: flags.budget_hit,
            external_spent: flags.external_spent,
        }
    }
}

/// The map whose time is the `lsp_ms` cost.
const LSP_LIVE_MAP: &str = "lsp.live";

/// [`item_record`] with the item's costs: `expansions` and `model_calls` as
/// before, plus `external_calls`, `physical_external_calls`, `lsp_ms`,
/// `answer_tokens` and, only when a budget cut the walk, `budget_hit` (a string,
/// so `maps compare` does not read it as a metric), and `external_spent: true`
/// when the external-call allowance was spent.
pub(super) fn item_record_with_costs(
    set: Set,
    item: &Value,
    answers: &[Value],
    costs: &ItemCosts,
) -> Result<(f64, f64, Map<String, Value>)> {
    let (p, f05, mut record) = item_record(set, item, answers, Some(costs.expansions))?;
    record.insert("model_calls".into(), json!(costs.model_calls));
    record.insert("external_calls".into(), json!(costs.external_calls));
    record.insert(
        "physical_external_calls".into(),
        json!(costs.physical_external_calls),
    );
    record.insert("lsp_ms".into(), json!(costs.lsp_ms));
    record.insert("answer_tokens".into(), json!(costs.answer_tokens));
    if let Some(hit) = costs.budget_hit {
        record.insert("budget_hit".into(), json!(hit.as_str()));
    }
    if costs.external_spent {
        record.insert("external_spent".into(), json!(true));
    }
    Ok((p, f05, record))
}

/// Means of the costs over a group of items and the budget hits among them.
#[derive(Default)]
struct CostCell {
    n: usize,
    expansions: f64,
    external_calls: f64,
    lsp_ms: f64,
    answer_tokens: f64,
    hits: BTreeMap<&'static str, usize>,
}

impl CostCell {
    fn add(&mut self, costs: &ItemCosts) {
        self.n += 1;
        self.expansions += costs.expansions as f64;
        self.external_calls += f64::from(costs.external_calls);
        self.lsp_ms += costs.lsp_ms;
        self.answer_tokens += costs.answer_tokens as f64;
        if let Some(hit) = costs.budget_hit {
            *self.hits.entry(hit.as_str()).or_default() += 1;
        }
    }

    fn to_json(&self) -> Value {
        let mean = |sum: f64| (sum / self.n.max(1) as f64 * 10_000.0).round() / 10_000.0;
        json!({
            "n": self.n,
            "mean": {
                "expansions": mean(self.expansions),
                "external_calls": mean(self.external_calls),
                "lsp_ms": mean(self.lsp_ms),
                "answer_tokens": mean(self.answer_tokens),
            },
            "budget_hits": self.hits,
        })
    }
}

/// The costs of the scored items of a set, overall and per stratum (the
/// scorer's own dimensions, [`gold::dims`]): `{"overall": cell, "strata":
/// {dimension: {value: cell}}}` with `cell = {n, mean: {expansions,
/// external_calls, lsp_ms, answer_tokens}, budget_hits: {kind: n}}`. Written
/// into `score.json` as `costs`; the scorer's own report is not touched.
pub fn costs_by_stratum(set: Set, scored: &[(&Value, &ItemCosts)]) -> Value {
    let mut overall = CostCell::default();
    let mut strata: BTreeMap<&'static str, BTreeMap<String, CostCell>> = BTreeMap::new();
    for (item, costs) in scored {
        overall.add(costs);
        for (dimension, value) in gold::dims(set, item) {
            strata
                .entry(dimension)
                .or_default()
                .entry(value)
                .or_default()
                .add(costs);
        }
    }
    let strata: Map<String, Value> = strata
        .into_iter()
        .map(|(dimension, cells)| {
            let cells: Map<String, Value> = cells
                .into_iter()
                .map(|(value, cell)| (value, cell.to_json()))
                .collect();
            (dimension.to_owned(), Value::Object(cells))
        })
        .collect();
    json!({ "overall": overall.to_json(), "strata": strata })
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
        Set::Traits => {
            if item["set"].as_str() == Some("Q2") {
                // One gold location: recall is the hit in the first ten, and
                // precision that hit over the scorer's counted answers (at most ten).
                let counted = m("counted@10");
                let hit = m("hit@10");
                (if counted == 0.0 { 0.0 } else { hit / counted }, hit)
            } else {
                (m("set_precision"), m("set_recall"))
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
    /// Score only this many of the first answers of each traversal.
    pub score_k: Option<usize>,
    /// The evaluated tree (`--tree`): the source root that places Q1 method
    /// nodes in their impl blocks. None: those nodes are not grouped, and the
    /// report warns.
    pub tree: Option<PathBuf>,
    /// The `--config` overlay: named in the row's note and recorded in
    /// `score.json`. None: no output differs from a run without the flag.
    pub overlay: Option<ConfigOverlay>,
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
    /// `{"set": name, "items": {"<id>": {metric: value}}}`: the scorer's
    /// metrics of every scored item alone, plus `precision`, `f0.5` and
    /// `expansions` (pops + pass-through pops). `maps compare` pairs on it.
    /// `Null` for a row that scored nothing.
    pub items: Value,
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
    pass_through_pops: u64,
    /// Items that stopped on the deadline, had reranker failures or had map
    /// call failures (an item can be in several).
    flagged: [usize; 3],
    /// Reranker frontier calls, nodes rated and calls that failed.
    rerank: [u64; 3],
    answer_chars_tokens: u64,
    engine_ms: Vec<f64>,
    wall_ms: Vec<f64>,
    non_empty: usize,
    no_seeds: usize,
    precision: f64,
    f05: f64,
    /// Scored items per `stop_reason` of their raw output.
    stop_reasons: BTreeMap<String, usize>,
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
pub(super) fn headline(set: Set) -> &'static [&'static str] {
    match set {
        Set::Impact => &["decl_recall", "decl_f1", "decl_ndcg@10", "file_f1"],
        Set::Flow => &["path_recall", "tau", "spurious"],
        Set::WhereTo => &["reg_hit@3", "reg_mrr", "set_f1"],
        Set::History => &["mrr", "hit@3", "hit@10"],
        Set::Instructions => &["i1_section_f1", "i2_hit@3_primary", "i2_required_recall@5"],
        Set::Traits => &["set_f1", "recall_explicit", "hit@10", "mrr"],
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
        items: Value::Null,
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
    // Traits only: places SCIP method nodes (ids without a container) in their
    // impl blocks through the source tree; one parse per file for the run.
    let mut grouper = (set == Set::Traits).then(|| ImplGrouper::new(config.tree.as_deref()));
    let mut raw = Vec::new();
    let mut sums = Sums::default();
    let mut skips = Skips::default();
    let mut scored: Vec<(Vec<Value>, &Value)> = Vec::new();
    // The costs of each scored item (expansions, model calls, external calls,
    // LSP ms, answer tokens, budget hit), for `items.json` and `score.json`.
    let mut costs: Vec<ItemCosts> = Vec::new();
    let started = Instant::now();
    for (index, item) in items.iter().enumerate() {
        let id = item["id"].as_str().context("gold item without an id")?;
        let question = item["question"].as_str().unwrap_or_default();
        match traverser.traverse(id, question) {
            Ok((traversed, raw_json)) => {
                if let Some(reason) = raw_json["stop_reason"].as_str() {
                    *sums.stop_reasons.entry(reason.to_owned()).or_default() += 1;
                }
                let flags = ItemFlags::from_raw(&raw_json);
                for (count, hit) in sums.flagged.iter_mut().zip([
                    flags.deadline,
                    flags.rerank_failures > 0,
                    flags.map_failures > 0,
                ]) {
                    *count += usize::from(hit);
                }
                raw.push((id.to_string(), raw_json));
                let mut nodes: Vec<ResultNode> = Vec::new();
                if config.include_seeds {
                    nodes.extend(traversed.seeds.iter().map(|s| ResultNode {
                        id: s.clone(),
                        text: s.clone(),
                        site: traversed.sites.get(s).cloned(),
                    }));
                }
                nodes.extend(
                    traversed
                        .results
                        .iter()
                        .take(config.score_k.unwrap_or(usize::MAX))
                        .cloned(),
                );
                let answers = match grouper.as_mut() {
                    Some(grouper) => adapt_traits_in(item, &nodes, Some(grouper)),
                    None => adapt(set, item, &nodes),
                };
                if traversed.status == "no_seeds" {
                    sums.no_seeds += 1;
                }
                if !answers.is_empty() {
                    sums.non_empty += 1;
                }
                let chars: usize = nodes.iter().map(|n| n.text.chars().count()).sum();
                let answer_tokens = chars.div_ceil(4) as u64;
                sums.answer_chars_tokens += answer_tokens;
                sums.wall_ms.extend(traversed.wall_ms);
                if let Some(t) = &traversed.totals {
                    sums.pops += t.pops;
                    sums.hops += t.hops;
                    sums.cross += t.cross_map_hops;
                    sums.stops += t.stops;
                    sums.pass_through_pops += t.pass_through_pops;
                    for (sum, n) in sums.rerank.iter_mut().zip([
                        t.rerank_calls,
                        t.rerank_docs,
                        t.rerank_failures,
                    ]) {
                        *sum += n;
                    }
                    sums.engine_ms.push(t.elapsed_ms);
                }
                run.insert(id.to_string(), Value::Array(answers.clone()));
                costs.push(ItemCosts::of(&traversed, &flags, answer_tokens));
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

    let mut item_records = Map::new();
    for ((answers, item), costs) in scored.iter().zip(&costs) {
        // The costs sit beside the metrics, so `maps compare` can pair on them.
        let (p, f05, record) = item_record_with_costs(set, item, answers, costs)?;
        sums.precision += p;
        sums.f05 += f05;
        if let Some(id) = item["id"].as_str() {
            item_records.insert(id.to_owned(), Value::Object(record));
        }
    }
    let n_scored = scored.len();
    let scored_items: Vec<Value> = scored.iter().map(|(_, item)| (*item).clone()).collect();
    let run_value = Value::Object(run);
    let mut report = if n_scored == 0 {
        Value::Null
    } else {
        let parsed_run = gold::parse_run(&run_value.to_string())?;
        gold::score_set(set, &scored_items, &parsed_run, false, false)?
    };
    if let Some(object) = report.as_object_mut() {
        let rows: Vec<(&Value, &ItemCosts)> = scored
            .iter()
            .map(|(_, item)| *item)
            .zip(costs.iter())
            .collect();
        object.insert("costs".into(), costs_by_stratum(set, &rows));
        if let Some(overlay) = &config.overlay {
            object.insert("config_overlay".into(), overlay.record(&raw));
        }
    }
    let grouping_warnings = grouper
        .as_ref()
        .map(ImplGrouper::warnings)
        .unwrap_or_default();
    for warning in &grouping_warnings {
        eprintln!("warning: {warning}");
    }
    if !grouping_warnings.is_empty()
        && let Some(object) = report.as_object_mut()
    {
        let warnings = object.entry("warnings").or_insert_with(|| json!([]));
        if let Some(list) = warnings.as_array_mut() {
            list.extend(grouping_warnings.iter().map(|w| json!(w)));
        }
    }

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
    if let Some(marker) = unreliable_marker(sums.flagged) {
        notes.push(marker);
    }
    if !config.note.is_empty() {
        notes.push(config.note.clone());
    }
    let returned: Vec<&Value> = raw
        .iter()
        .map(|(_, value)| value)
        .filter(|value| value.get("skipped").is_none())
        .collect();
    notes.extend(exact_answered(&config.maps, &returned));
    if sums.pass_through_pops > 0 {
        notes.push(format!(
            "expansions={} (pops {} + pass-through pops {})",
            sums.pops + sums.pass_through_pops,
            sums.pops,
            sums.pass_through_pops
        ));
    }
    if sums.no_seeds > 0 {
        notes.push(format!("{} scored items found no seeds", sums.no_seeds));
    }
    let [calls, docs, failures] = sums.rerank;
    if calls >= 1 {
        notes.push(format!(
            "rerank calls={calls} docs={docs} failed={failures}"
        ));
    }
    if calls > 0 {
        // Two components, never combined: no weight of a model call against an
        // expansion is assumed. Judge each against the F-append arm's own total
        // (1.5x rule per component); `items.json` carries both per item.
        notes.push(format!(
            "cost: expansions={} (hops {}) model calls={calls} (rerank {calls}); each component is judged apart against the F-append arm, 1.5x rule per component",
            sums.pops + sums.pass_through_pops,
            sums.hops,
        ));
    }
    if !sums.stop_reasons.is_empty() {
        let counts: Vec<String> = sums
            .stop_reasons
            .iter()
            .map(|(reason, n)| format!("{reason}={n}"))
            .collect();
        notes.push(format!("stops: {}", counts.join(" ")));
    }
    if let Some(marker) = unreliable_marker(sums.flagged) {
        eprintln!(
            "warning: {}: {marker}; machine load can cut walks and failed calls change answers",
            set_name(set)
        );
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
    notes.extend(grouping_warnings);
    notes.push("ans tok = chars/4".into());
    cells[20] = notes.join("; ");
    Ok(SetOutcome {
        run: run_value,
        report,
        row: TrendRow { cells },
        raw,
        items: json!({ "set": set_name(set), "items": item_records }),
    })
}

/// The row-note marker for a run whose items include a deadline stop, model
/// call failures (reranker) or map call failures (`flagged` counts the items of each kind);
/// such a row is not a clean measurement and should not decide anything.
fn unreliable_marker([deadline, rerank, map]: [usize; 3]) -> Option<String> {
    (deadline + rerank + map > 0).then(|| {
        format!(
            "UNRELIABLE, not decision-grade (items with: deadline stop={deadline}, model-call failures={rerank}, map call failures={map})"
        )
    })
}

/// The set a scored outcome belongs to, read from its scorer report.
fn outcome_set(outcome: &SetOutcome) -> Option<Set> {
    <Set as ValueEnum>::from_str(outcome.report["set"].as_str()?, true).ok()
}

/// Writes `run.json`, `score.json`, `items.json`, `row.md`, `supplement.md`
/// (extra tables and ceiling footers, see `supplement.rs`) and `raw/<id>.json`
/// under `dir`.
pub fn write_outputs(dir: &Path, outcome: &SetOutcome) -> Result<()> {
    let raw_dir = dir.join("raw");
    std::fs::create_dir_all(&raw_dir).with_context(|| format!("create {}", raw_dir.display()))?;
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
    std::fs::write(dir.join("run.json"), pretty(&outcome.run)?)?;
    if !outcome.report.is_null() {
        std::fs::write(dir.join("score.json"), pretty(&outcome.report)?)?;
    }
    std::fs::write(dir.join("row.md"), outcome.row.to_markdown() + "\n")?;
    if !outcome.items.is_null() {
        std::fs::write(dir.join("items.json"), pretty(&outcome.items)?)?;
    }
    if let Some(set) = outcome_set(outcome)
        && let Some(text) = super::supplement::render(set, &outcome.report, &outcome.items)
    {
        std::fs::write(dir.join("supplement.md"), text)?;
    }
    for (id, value) in &outcome.raw {
        std::fs::write(raw_dir.join(format!("{}.json", slug(id))), pretty(value)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_core::node::{SymbolKey, rust_module_path};

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
    fn traits_adapter_keeps_nodes_apart_and_members_only_for_q2_and_q3() {
        let nodes = located_nodes();
        let q1 = adapt_traits(&json!({"set": "Q1"}), &nodes);
        assert_eq!(q1.len(), nodes.iter().filter_map(decl_of).count());
        assert_eq!(q1[0]["path"], "src/app/search.rs");
        assert_eq!(q1[0]["line"], 12);
        assert_eq!(q1[0]["symbol"], json!(nodes[0].id));
        // two nodes at one site stay two answers
        let twin = located(METHOD, "src/app/search.rs", 12);
        let both = adapt_traits(&json!({"set": "Q1"}), &[nodes[0].clone(), twin]);
        assert_eq!(both.len(), 2);
        assert_ne!(both[0]["symbol"], both[1]["symbol"]);
        // members only: the type node is left out
        let members = adapt_traits(&json!({"set": "Q3"}), &nodes);
        assert_eq!(members.len(), 3);
        assert!(members.iter().all(|a| a["symbol"].is_string()));
    }

    #[test]
    fn traits_q2_precision_uses_only_counted_answers_up_to_ten() {
        let span =
            |file: &str, a: u64, b: u64| json!({"file": file, "begin_line": a, "end_line": b});
        let item = json!({
            "id": "q2", "set": "Q2",
            "trait_decl": span("tr.rs", 100, 140),
            "neutral": [span("neutral.rs", 1, 5)],
            "cfg_gated": [span("cfg.rs", 1, 5)],
            "gold": [{"location": span("m.rs", 5, 5)}]
        });
        let ignored = vec![
            json!({"path": "tr.rs", "line": 110}),
            json!({"path": "neutral.rs", "line": 1}),
            json!({"path": "cfg.rs", "line": 1}),
        ];
        let hit = json!({"path": "m.rs", "line": 5});
        let mut answers = vec![ignored[0].clone(), hit];
        for extra in [vec![], ignored.clone()] {
            answers.extend(extra);
            let metrics = item_metrics(Set::Traits, &item, &answers).unwrap();
            assert_eq!(metrics["counted@10"], Some(1.0));
            assert_eq!(
                item_precision_recall(Set::Traits, &item, &answers, &metrics),
                (1.0, 1.0)
            );
        }
        answers.extend((1..=12).map(|line| json!({"path": "miss.rs", "line": line})));
        let metrics = item_metrics(Set::Traits, &item, &answers).unwrap();
        assert_eq!(metrics["counted@10"], Some(10.0));
        assert_eq!(
            item_precision_recall(Set::Traits, &item, &answers, &metrics),
            (0.1, 1.0)
        );
        let metrics = item_metrics(Set::Traits, &item, &ignored).unwrap();
        assert_eq!(metrics["counted@10"], Some(0.0));
        assert_eq!(
            item_precision_recall(Set::Traits, &item, &ignored, &metrics),
            (0.0, 0.0)
        );
    }

    #[test]
    fn q1_groups_generic_and_unsafe_impl_members() {
        for header in [
            "impl<T> Tr for Foo<T>",
            "unsafe impl Tr for Foo",
            "unsafe impl<T> Tr for Foo<T>",
        ] {
            let nodes = vec![
                located(
                    &format!("sym:rust:app::x::{header}::a/method"),
                    "src/x.rs",
                    10,
                ),
                located(
                    &format!("sym:rust:app::x::{header}::b/method"),
                    "src/x.rs",
                    11,
                ),
                located("sym:rust:app::x::impl Tr for Bar::a/method", "src/x.rs", 20),
            ];
            let answers = adapt_traits(&json!({"set": "Q1"}), &nodes);
            assert_eq!(answers.len(), 2, "{header}");
            assert_eq!(answers[0]["line"], 10);
            assert_eq!(answers[1]["line"], 20);
            for template in ["Q2", "Q3"] {
                assert_eq!(adapt_traits(&json!({"set": template}), &nodes).len(), 3);
            }
        }
    }

    #[test]
    fn a_q1_walk_counts_an_impl_and_its_methods_as_one_answer() {
        const A1: &str = "sym:rust:app::x::impl Tr for Foo::a/method";
        const A2: &str = "sym:rust:app::x::impl Tr for Foo::b/method";
        const B1: &str = "sym:rust:app::x::impl Tr for Bar::a/method";
        let nodes = vec![
            located(A1, "src/x.rs", 10),
            located(A2, "src/x.rs", 11),
            located(B1, "src/x.rs", 20),
            located(TYPE, "src/x.rs", 1),
        ];
        let q1 = json!({"set": "Q1"});
        let answers = adapt_traits(&q1, &nodes);
        // Foo's two methods are one answer (the better ranked), Bar's is another,
        // the type node stands for itself.
        let lines: Vec<u64> = answers
            .iter()
            .map(|a| a["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, [10, 20, 1]);
        // Q3 keeps every member.
        assert_eq!(adapt_traits(&json!({"set": "Q3"}), &nodes).len(), 3);
        // One slot, one credit: two gold impls (k = 2); without the collapse the
        // budget would be spent on Foo's two methods and Bar would be cut.
        let span = |a: u64, b: u64| json!({"file": "src/x.rs", "begin_line": a, "end_line": b});
        let item = json!({
            "id": "q1", "set": "Q1",
            "gold": [
                {"impl_id": "i0", "stratum": "explicit", "impl": span(10, 15), "type_decl": null},
                {"impl_id": "i1", "stratum": "explicit", "impl": span(20, 25), "type_decl": null}
            ]
        });
        let m = item_metrics(Set::Traits, &item, &answers[..2]).unwrap();
        assert_eq!(m["set_recall"], Some(1.0));
        assert_eq!(m["set_precision"], Some(1.0));
    }

    // ---- Q1 grouping of SCIP method nodes through the evaluated tree's source.

    /// `src/x.rs` of the grouping tests; the lines are what the tests cite.
    const GROUPING_SOURCE: &str = "\
pub struct Foo;
pub struct Bar;

impl Foo {
    pub fn a(&self) {}
    pub fn b(&self) {}
}

impl Bar {
    pub fn a(&self) {}
}

impl<T> Wrap<T> {
    pub fn g(&self) {}
    pub fn h(&self) {}
}

unsafe impl Send for Foo {
    fn u1(&self) {}
    fn u2(&self) {}
}

pub fn free_one() {}
pub fn free_two() {}
";

    /// A temp source tree holding `src/x.rs`, removed on drop.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> TempTree {
            let dir = std::env::temp_dir().join(format!(
                "lci-bench-impl-units-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("src/x.rs"), GROUPING_SOURCE).unwrap();
            TempTree(dir)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The id `ScipMap::with_default_ids` gives a node: no container, the
    /// module path of its file, the bare name, a disambiguator counting equal
    /// keys.
    fn scip_id(path: &str, name: &str, kind: SymbolKind, disambiguator: u16) -> String {
        NodeId::Symbol(SymbolKey {
            language: "rust".into(),
            module_path: rust_module_path(path),
            container: Vec::new(),
            name: name.into(),
            kind,
            disambiguator,
        })
        .to_string()
    }

    fn method(name: &str, disambiguator: u16, line: u64) -> ResultNode {
        located(
            &scip_id("src/x.rs", name, SymbolKind::Method, disambiguator),
            "src/x.rs",
            line,
        )
    }

    fn lines_of(answers: &[Value]) -> Vec<u64> {
        answers
            .iter()
            .map(|a| a["line"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn scip_default_ids_carry_no_impl() {
        let id = scip_id("src/x.rs", "a", SymbolKind::Method, 0);
        assert_eq!(id, "sym:rust:x::a/method");
        assert_eq!(impl_unit(&id), id);
    }

    #[test]
    fn q1_groups_scip_method_nodes_by_impl_block_from_source() {
        let tree = TempTree::new("foo-bar");
        // Foo.a, Foo.b, Bar.a: ids in the with_default_ids shape (Bar.a is the
        // second `a`, so disambiguator 1).
        let nodes = vec![method("a", 0, 5), method("b", 0, 6), method("a", 1, 10)];
        let q1 = json!({"set": "Q1"});
        let plain = adapt_traits(&q1, &nodes);
        assert_eq!(lines_of(&plain), [5, 6, 10], "ids alone cannot group");
        let mut grouper = ImplGrouper::new(Some(&tree.0));
        let answers = adapt_traits_in(&q1, &nodes, Some(&mut grouper));
        assert_eq!(lines_of(&answers), [5, 10]);
        assert_eq!(grouper.files_seen(), 1, "one parse per file");
        assert!(grouper.warnings().is_empty());
        let span = |a: u64, b: u64| json!({"file": "src/x.rs", "begin_line": a, "end_line": b});
        let item = json!({
            "id": "q1", "set": "Q1",
            "gold": [
                {"impl_id": "i0", "stratum": "explicit", "impl": span(4, 7), "type_decl": null},
                {"impl_id": "i1", "stratum": "explicit", "impl": span(9, 11), "type_decl": null}
            ]
        });
        let recall = |answers: &[Value]| item_metrics(Set::Traits, &item, &answers[..2]).unwrap()["set_recall"];
        assert_eq!(recall(&answers), Some(1.0));
        // Without the grouping, k = 2 is spent on Foo's two methods.
        assert_eq!(recall(&plain), Some(0.5));
    }

    #[test]
    fn q1_source_grouping_handles_generic_unsafe_and_free_functions() {
        let tree = TempTree::new("kinds");
        let free = |name: &str, line: u64| {
            located(
                &scip_id("src/x.rs", name, SymbolKind::Function, 0),
                "src/x.rs",
                line,
            )
        };
        let nodes = vec![
            // impl<T> Wrap<T>: g and h are one answer.
            method("g", 0, 14),
            method("h", 0, 15),
            // unsafe impl Send for Foo: u1 and u2 are one answer.
            method("u1", 0, 19),
            method("u2", 0, 20),
            // Free functions are not in an impl: each is its own answer.
            free("free_one", 23),
            free("free_two", 24),
            // The same function again is the same node.
            free("free_one", 23),
        ];
        let mut grouper = ImplGrouper::new(Some(&tree.0));
        let answers = adapt_traits_in(&json!({"set": "Q1"}), &nodes, Some(&mut grouper));
        assert_eq!(lines_of(&answers), [14, 19, 23, 24]);
        assert!(grouper.warnings().is_empty());
        // Q2 and Q3 keep every member node whatever the tree says.
        let members = adapt_traits_in(&json!({"set": "Q3"}), &nodes, Some(&mut grouper));
        assert_eq!(members.len(), 6);
    }

    #[test]
    fn q1_without_a_tree_keeps_today_s_answers_and_warns() {
        let nodes = vec![method("a", 0, 5), method("b", 0, 6), method("a", 1, 10)];
        let mut grouper = ImplGrouper::new(None);
        let answers = adapt_traits_in(&json!({"set": "Q1"}), &nodes, Some(&mut grouper));
        assert_eq!(lines_of(&answers), [5, 6, 10]);
        let warnings = grouper.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no source root"), "{warnings:?}");
        // A node whose id names its impl needs no tree and no warning.
        let mut grouper = ImplGrouper::new(None);
        let named = vec![located(
            "sym:rust:x::impl Tr for Foo::a/method",
            "src/x.rs",
            5,
        )];
        adapt_traits_in(&json!({"set": "Q1"}), &named, Some(&mut grouper));
        assert!(grouper.warnings().is_empty());
    }

    #[test]
    fn q1_with_an_unreadable_file_keeps_the_nodes_apart_and_warns() {
        let tree = TempTree::new("unreadable");
        let missing = |name: &str, line: u64| {
            located(
                &scip_id("src/gone.rs", name, SymbolKind::Method, 0),
                "src/gone.rs",
                line,
            )
        };
        let nodes = vec![missing("a", 5), missing("b", 6)];
        let mut grouper = ImplGrouper::new(Some(&tree.0));
        let answers = adapt_traits_in(&json!({"set": "Q1"}), &nodes, Some(&mut grouper));
        assert_eq!(lines_of(&answers), [5, 6]);
        let warnings = grouper.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("could not be read"), "{warnings:?}");
    }

    #[test]
    fn bench_set_groups_traits_q1_through_the_tree_and_reports_the_missing_root() {
        let tree = TempTree::new("bench-set");
        let results: Vec<Value> = [("a", 0, 5), ("b", 0, 6), ("a", 1, 10)]
            .iter()
            .map(|(name, d, line)| {
                json!({
                    "node": scip_id("src/x.rs", name, SymbolKind::Method, *d),
                    "text": "t",
                    "site": {"path": "src/x.rs", "line": line, "col": 8},
                })
            })
            .collect();
        let raw = json!({"status": "complete", "seeds": [], "results": results});
        let span = |a: u64, b: u64| json!({"file": "src/x.rs", "begin_line": a, "end_line": b});
        let items = vec![json!({
            "id": "q1", "set": "Q1", "question": "q", "primary": true,
            "gold": [
                {"impl_id": "i0", "stratum": "explicit", "impl": span(4, 7), "type_decl": null},
                {"impl_id": "i1", "stratum": "explicit", "impl": span(9, 11), "type_decl": null}
            ]
        })];
        let mut answers = BTreeMap::new();
        answers.insert("q".to_string(), raw);
        let fixture = Fixture::new(answers);

        let mut cfg = config(&["graph.scip"]);
        cfg.tree = Some(tree.0.clone());
        let with_tree = bench_set(Set::Traits, &items, &fixture, &cfg).unwrap();
        assert_eq!(with_tree.run["q1"].as_array().unwrap().len(), 2);
        // (The scorer's own no-trait_decl warning may be there; grouping adds none.)
        assert!(
            !with_tree.report["warnings"]
                .to_string()
                .contains("impl grouping"),
            "{}",
            with_tree.report["warnings"]
        );
        assert_eq!(with_tree.report["overall"]["metrics"]["set_recall"], 1.0);

        cfg.tree = None;
        let without = bench_set(Set::Traits, &items, &fixture, &cfg).unwrap();
        assert_eq!(without.run["q1"].as_array().unwrap().len(), 3);
        let warnings = without.report["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap().contains("no source root")),
            "{warnings:?}"
        );
        assert!(without.row.cells[20].contains("no source root"));
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

    #[test]
    fn tests_tested_by_can_answer_the_impact_set() {
        let tested_by = vec!["tests.tested_by".to_string()];
        assert!(can_answer(Set::Impact, &tested_by));
        assert!(can_answer(Set::WhereTo, &tested_by));
        assert!(!can_answer(Set::History, &tested_by));
        assert!(!can_answer(Set::Instructions, &tested_by));
    }

    #[test]
    fn docs_mentions_can_answer_the_impact_set() {
        let docs = vec!["docs.mentions".to_string()];
        assert!(can_answer(Set::Impact, &docs));
        assert!(can_answer(Set::WhereTo, &docs));
        assert!(!can_answer(Set::History, &docs));
        // It emits files, never a section: the instructions set skips every item.
        assert!(!can_answer(Set::Instructions, &docs));
    }

    #[test]
    fn grep_structural_can_answer_the_traits_and_impact_sets() {
        let structural = vec!["grep.structural".to_string()];
        assert_eq!(
            map_emits("grep.structural"),
            Some(&[NodeKind::Symbol, NodeKind::File][..])
        );
        assert!(can_answer(Set::Traits, &structural));
        assert!(can_answer(Set::Impact, &structural));
        assert!(can_answer(Set::WhereTo, &structural));
        assert!(!can_answer(Set::History, &structural));
        assert!(!can_answer(Set::Instructions, &structural));
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
            "mode": "traverse", "status": "complete", "stop_reason": "bound", "seeds": seeds,
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
            score_k: None,
            tree: None,
            overlay: None,
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
        let mut two = traversal(&["x"], &[METHOD], 4);
        two["stop_reason"] = json!("deadline");
        two["totals"]["rerank_calls"] = json!(3);
        two["totals"]["rerank_docs"] = json!(7);
        two["totals"]["rerank_failures"] = json!(1);
        answers.insert("q two".into(), two);
        answers.insert(
            "q three".into(),
            json!({"mode": "traverse", "status": "no_seeds", "stop_reason": "no_seeds", "results": [], "seeds": []}),
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
        assert!(cells[20].contains("rerank calls=3 docs=7 failed=1"));
        assert!(cells[20].contains("stops: bound=1 deadline=1 no_seeds=1"));
        // The deadline stop and the reranker failure make the row unreliable.
        assert!(
            cells[20].starts_with(
                "UNRELIABLE, not decision-grade (items with: deadline stop=1, model-call failures=1, map call failures=0)"
            ),
            "{}",
            cells[20]
        );
        // The same item is in `items.json` with the per-item metrics.
        assert_eq!(outcome.items["set"], "impact");
        assert_eq!(outcome.items["items"]["i2"]["decl_recall"], 1.0);
        assert_eq!(outcome.items["items"]["i2"]["expansions"], 4);
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
        assert!(dir.join("out/items.json").exists());
        // The supplement is a new file; the frozen outputs above are unchanged.
        let supplement = std::fs::read_to_string(dir.join("out/supplement.md")).unwrap();
        assert!(supplement.contains("band | n | decl_recall | r2_decl_recall | tests_recall"));
        assert!(supplement.contains("Ceiling: first-ring declaration recall"));
        // Re-running the same wave and set is refused.
        assert!(trend::append(&trend_path, &outcome.row).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_run_has_no_marker_and_pass_through_pops_count_as_expansions() {
        let items = vec![impact_item(
            "i1",
            "q one",
            "src/app/search.rs",
            "search::rerank_document",
        )];
        let mut one = traversal(&["x"], &[SEARCH_FN], 6);
        one["totals"]["pass_through_pops"] = json!(5);
        let answers = BTreeMap::from([("q one".to_owned(), one)]);
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(answers),
            &config(&["graph.scip", "seeds.scip_names"]),
        )
        .unwrap();
        let note = &outcome.row.cells[20];
        assert!(!note.contains("UNRELIABLE"), "{note}");
        assert!(
            note.contains("expansions=11 (pops 6 + pass-through pops 5)"),
            "{note}"
        );
        assert_eq!(outcome.items["items"]["i1"]["expansions"], 11);
        // The map failures of a checkpoint are flags too.
        let raw = json!({"checkpoint": {"maps": [
            {"map": "graph.scip", "failures": 0}, {"map": "git.history", "failures": 3}]}});
        let flags = ItemFlags::from_raw(&raw);
        assert_eq!(
            (flags.map_failures, flags.deadline, flags.any()),
            (3, false, true)
        );
        assert!(!ItemFlags::from_raw(&json!({"stop_reason": "bound"})).any());
    }

    /// The costs and the budget hit a raw reply gives.
    fn costs_of(raw: &Value, answer_tokens: u64) -> (ItemCosts, ItemFlags) {
        let traversed = Traversed::from_json(raw, None);
        let flags = ItemFlags::from_raw(raw);
        (ItemCosts::of(&traversed, &flags, answer_tokens), flags)
    }

    #[test]
    fn an_item_record_carries_its_costs_and_the_budget_hit() {
        let items = vec![impact_item(
            "i1",
            "q one",
            "src/app/search.rs",
            "search::rerank_document",
        )];
        let mut raw = traversal(&["x"], &[SEARCH_FN], 6);
        raw["totals"]["external_calls"] = json!(7);
        raw["totals"]["physical_external_calls"] = json!(9);
        raw["totals"]["per_map"] = json!({
            "graph.scip": {"expands": 4, "ms": 1.5},
            "lsp.live": {"expands": 3, "ms": 120.5},
        });
        raw["stop_reason"] = json!("budget");
        raw["notes"] = json!([
            "the external-call allowance was spent: from then on only maps that need no external call were asked"
        ]);
        let answers = BTreeMap::from([("q one".to_owned(), raw.clone())]);
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(answers),
            &config(&["graph.scip", "seeds.scip_names"]),
        )
        .unwrap();
        let record = &outcome.items["items"]["i1"];
        assert_eq!(record["expansions"], 6);
        assert_eq!(record["external_calls"], 7);
        assert_eq!(record["physical_external_calls"], 9);
        assert_eq!(record["lsp_ms"], 120.5);
        // The walk ended on the token budget; the spent allowance is its own flag.
        assert_eq!(record["budget_hit"], "tokens");
        assert_eq!(record["external_spent"], true);
        // The per-item tokens and the set total (the `ans tok` cell) are one
        // computation: chars/4 of the result texts, over one item.
        let chars = format!("{SEARCH_FN} | graph.scip/compiler | p1.00 d1")
            .chars()
            .count();
        let tokens = chars.div_ceil(4) as u64;
        assert_eq!(record["answer_tokens"], tokens);
        assert_eq!(outcome.row.cells[14], tokens.to_string());
        // The flag is what `maps compare` excludes on; the row is not "deadline".
        let flags = ItemFlags::from_raw(&raw);
        assert!(flags.any() && !flags.deadline);

        // The budget that ended the call, from the reply alone.
        let hit = |reply: Value| BudgetHit::from_raw(&reply).map(BudgetHit::as_str);
        let spent = json!(["the external-call allowance was spent: from then on only ..."]);
        assert_eq!(
            hit(json!({"stop_reason": "budget"})),
            Some("tokens"),
            "no allowance note: the token budget"
        );
        // A spent allowance never ends a walk: with `budget` the ending cause
        // wins, and a walk that completed anyway is flagged by the note alone.
        assert_eq!(
            hit(json!({"stop_reason": "budget", "notes": spent})),
            Some("tokens")
        );
        let complete = json!({"stop_reason": "complete", "notes": spent});
        assert_eq!(hit(complete.clone()), Some("external_calls"));
        let flags = ItemFlags::from_raw(&complete);
        assert!(flags.external_spent && flags.any() && !flags.deadline);
        assert_eq!(flags.kinds(), vec!["external_calls"]);
        let cut = ItemFlags::from_raw(&json!({"stop_reason": "pops", "notes": spent}));
        assert_eq!(cut.kinds(), vec!["pops", "external_calls"]);
        assert!(!ItemFlags::from_raw(&json!({"stop_reason": "complete"})).any());
        assert_eq!(hit(json!({"stop_reason": "pops"})), Some("pops"));
        let (costs, flags) = costs_of(&json!({"stop_reason": "deadline"}), 0);
        assert_eq!(costs.budget_hit.map(BudgetHit::as_str), Some("deadline"));
        assert!(flags.deadline && flags.any());
        // The walk went on but `lsp.live` ran out of the deadline on a node.
        let lsp_note = format!(
            "lsp.live reported an error for a node: {}",
            lci_maps::lsp_live::DEADLINE_NOTE
        );
        let (costs, flags) = costs_of(&json!({"stop_reason": "complete", "notes": [lsp_note]}), 0);
        assert_eq!(
            costs.budget_hit.map(BudgetHit::as_str),
            Some("lsp_deadline")
        );
        assert!(!flags.deadline && flags.any());
        // No map time and no budget: `lsp_ms` is 0 and the key is absent.
        let calm = traversal(&["x"], &[SEARCH_FN], 2);
        let (costs, flags) = costs_of(&calm, 5);
        assert_eq!(
            (costs.lsp_ms, costs.budget_hit, flags.any()),
            (0.0, None, false)
        );
        let (_, _, record) = item_record_with_costs(Set::Impact, &items[0], &[], &costs).unwrap();
        assert_eq!(record["lsp_ms"], 0.0);
        assert_eq!(record["external_calls"], 0);
        assert_eq!(record["answer_tokens"], 5);
        assert!(!record.contains_key("budget_hit"));
        // An old reply with a bare per-map count has no map time.
        let old = json!({"totals": {"pops": 1, "per_map": {"lsp.live": 3}}});
        assert!(Traversed::from_json(&old, None).map_ms.is_empty());
    }

    #[test]
    fn costs_by_stratum_averages_per_dimension() {
        let mut unique = impact_item(
            "i1",
            "q one",
            "src/app/search.rs",
            "search::rerank_document",
        );
        unique["ambiguity"] = json!({"band": "unique"});
        let mut common = impact_item(
            "i2",
            "q two",
            "src/app/search.rs",
            "search::rerank_document",
        );
        common["ambiguity"] = json!({"band": "common"});
        let items = vec![unique, common];
        let mut one = traversal(&["x"], &[SEARCH_FN], 4);
        one["totals"]["external_calls"] = json!(1);
        let mut two = traversal(&["x"], &[SEARCH_FN], 8);
        two["totals"]["external_calls"] = json!(3);
        two["totals"]["per_map"] = json!({"lsp.live": {"expands": 2, "ms": 30.0}});
        two["stop_reason"] = json!("pops");
        let answers = BTreeMap::from([("q one".to_owned(), one), ("q two".to_owned(), two)]);
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(answers),
            &config(&["graph.scip", "seeds.scip_names"]),
        )
        .unwrap();
        let costs = &outcome.report["costs"];
        assert_eq!(costs["overall"]["n"], 2);
        let mean = |cell: &Value, name: &str| cell["mean"][name].as_f64().unwrap();
        assert_eq!(mean(&costs["overall"], "expansions"), 6.0);
        assert_eq!(mean(&costs["overall"], "external_calls"), 2.0);
        assert_eq!(mean(&costs["overall"], "lsp_ms"), 15.0);
        assert_eq!(costs["overall"]["budget_hits"], json!({"pops": 1}));
        let band = &costs["strata"]["band"];
        assert_eq!(band["unique"]["n"], 1);
        assert_eq!(mean(&band["unique"], "expansions"), 4.0);
        assert_eq!(mean(&band["unique"], "lsp_ms"), 0.0);
        assert_eq!(band["unique"]["budget_hits"], json!({}));
        assert_eq!(band["common"]["n"], 1);
        assert_eq!(mean(&band["common"], "expansions"), 8.0);
        assert_eq!(mean(&band["common"], "external_calls"), 3.0);
        assert_eq!(mean(&band["common"], "lsp_ms"), 30.0);
        assert_eq!(band["common"]["budget_hits"], json!({"pops": 1}));
        // The other impact dimensions are there too; the scorer's own report is untouched.
        assert_eq!(costs["strata"]["kind"]["sym-fn"]["n"], 2);
        assert!(outcome.report["strata"]["band"]["unique"].is_object());
        // A run with nothing scored has no report and so no costs block.
        let none = bench_set(
            Set::Impact,
            &[],
            &Fixture::new(BTreeMap::new()),
            &config(&["graph.scip", "seeds.scip_names"]),
        )
        .unwrap();
        assert!(none.report.is_null());
    }

    #[test]
    fn rerank_calls_are_summed_into_the_cost_note() {
        let items = vec![impact_item(
            "i1",
            "q one",
            "src/app/search.rs",
            "search::rerank_document",
        )];
        let mut one = traversal(&["x"], &[SEARCH_FN], 6);
        one["totals"]["rerank_calls"] = json!(2);
        let answers = BTreeMap::from([("q one".to_owned(), one)]);
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(answers),
            &config(&["graph.scip", "seeds.scip_names"]),
        )
        .unwrap();
        // The per-item record carries the model calls next to the expansions.
        assert_eq!(outcome.items["items"]["i1"]["model_calls"], 2);
        assert_eq!(outcome.items["items"]["i1"]["expansions"], 6);
        let note = &outcome.row.cells[20];
        assert!(
            note.contains("cost: expansions=6 (hops ")
                && note.contains("model calls=2 (rerank 2)")
                && note.contains("1.5x rule per component"),
            "{note}"
        );
        // A run without reranker calls has no model-call cost.
        let plain = BTreeMap::from([("q one".to_owned(), traversal(&["x"], &[SEARCH_FN], 6))]);
        let outcome = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(plain),
            &config(&["graph.scip", "seeds.scip_names"]),
        )
        .unwrap();
        assert!(!outcome.row.cells[20].contains("model calls"));
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
            ids(select_items(Set::Impact, items.clone(), None, None)),
            ["b", "c"]
        );
        assert_eq!(
            ids(select_items(Set::Impact, items.clone(), Some(1), None)),
            ["b"]
        );
        assert_eq!(
            ids(select_items(Set::Flow, items.clone(), Some(2), None)),
            ["a", "b"]
        );
        // The hold-out list keeps its ids, after the population and before the limit.
        let held: BTreeSet<String> = ["a", "c"].map(String::from).into();
        let held_ids = |set, limit| ids(select_items(set, items.clone(), limit, Some(&held)));
        assert_eq!(held_ids(Set::Impact, None), ["c"]);
        assert_eq!(held_ids(Set::Flow, None), ["a", "c"]);
        assert_eq!(held_ids(Set::Flow, Some(1)), ["a"]);
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

    // ---- d461: seeds.bm25 and seeds.literal in the bench.

    #[test]
    fn the_opt_in_seed_maps_are_known_seed_maps_with_their_emitted_kinds() {
        use NodeKind::{File, Symbol};
        for map in ["seeds.bm25", "seeds.literal"] {
            assert!(SEED_MAPS.contains(&map), "{map}");
        }
        // The descriptors in src/app/traverse/seeds.rs (`SeedMap::bm25` and
        // `SeedMap::literal`).
        assert_eq!(map_emits("seeds.bm25"), Some(&[Symbol][..]));
        assert_eq!(map_emits("seeds.literal"), Some(&[Symbol, File][..]));
        // Symbols and files answer impact, flow, where-to and traits; neither
        // map can answer history or instructions.
        for map in ["seeds.bm25", "seeds.literal"] {
            let maps = vec![map.to_string()];
            assert!(can_answer(Set::Impact, &maps), "{map}");
            assert!(can_answer(Set::Flow, &maps), "{map}");
            assert!(can_answer(Set::WhereTo, &maps), "{map}");
            assert!(!can_answer(Set::History, &maps), "{map}");
            assert!(!can_answer(Set::Instructions, &maps), "{map}");
        }
        // An unknown seeds.* name is no seed map: it is not judged by its
        // seeds, so its probe needs a start event, and the CLI refuses it.
        assert!(!SEED_MAPS.contains(&"seeds.nope"));
        assert_eq!(map_emits("seeds.nope"), None);
        let gave = json!({"seed_source": "bm25", "seeds": ["sym:rust:a::b/fn"]});
        assert!(check_probe("seeds.nope", &gave, None).is_err());
    }

    #[test]
    fn the_opt_in_seed_maps_are_checked_by_the_start_nodes_they_gave() {
        let bm25 = json!({"seed_source": "bm25", "seeds": ["sym:rust:a::b/fn"]});
        assert!(check_probe("seeds.bm25", &bm25, None).is_ok());
        let literal = json!({"seed_source": "literal", "seeds": ["sym:rust:a::b/fn"]});
        assert!(check_probe("seeds.literal", &literal, None).is_ok());
        // A map that gave nothing, or that another source answered for, fails.
        let off = json!({"seed_source": "none", "seeds": [],
                         "notes": ["seeds.bm25 is off ([run_profile.seeds] bm25 = false): the map gives no start nodes"]});
        assert!(check_probe("seeds.bm25", &off, None).is_err());
        let off = json!({"seed_source": "none", "seeds": [],
                         "notes": ["seeds.literal is off ([run_profile.seeds] literal = false): the map gives no start nodes"]});
        let error = check_probe("seeds.literal", &off, None).unwrap_err();
        assert!(error.contains("seeds.literal is off"), "{error}");
        // The CLI opens the application for a question only with seeds.hybrid
        // named, so a lone seeds.literal call says it has no ripgrep.
        let no_rg = json!({"seed_source": "none", "seeds": [],
                           "notes": ["seeds.literal needs the application's ripgrep (give --root with a question)"]});
        let error = check_probe("seeds.literal", &no_rg, None).unwrap_err();
        assert!(error.contains("needs the application's ripgrep"), "{error}");
        assert!(check_probe("seeds.bm25", &literal, None).is_err());
        assert!(check_probe("seeds.literal", &bm25, None).is_err());
        let names = json!({"seed_source": "name_tokens", "seeds": ["sym:rust:a::b/fn"]});
        assert!(check_probe("seeds.bm25", &names, None).is_err());
        // The probe question may name nothing in code spans: the map ran and
        // had nothing to search for, which is not an unavailable map.
        let nothing = json!({"seed_source": "none", "seeds": [],
                             "notes": ["seeds.literal: the question names nothing in code spans (`Name` or `a::Name`), so there is nothing to search for"]});
        assert!(check_probe("seeds.literal", &nothing, None).is_ok());
        // That exception is the literal map's alone.
        assert!(check_probe("seeds.bm25", &nothing, None).is_err());
    }

    /// A traverser whose preflight is `check_probe` on a canned probe answer
    /// per map, as the CLI's would be on a seed map (no probe start nodes).
    struct ProbeFixture {
        answers: BTreeMap<String, Value>,
    }

    impl Traverser for ProbeFixture {
        fn traverse(
            &self,
            _id: &str,
            _question: &str,
        ) -> Result<(Traversed, Value), TraverseError> {
            Err(TraverseError::Cli("not used".into()))
        }

        fn default_maps(&self, _question: &str) -> Result<Vec<String>, TraverseError> {
            Ok(Vec::new())
        }

        fn preflight(&self, map: &str, _question: &str) -> Result<(), TraverseError> {
            let none = json!({"seed_source": "none", "seeds": []});
            let response = self.answers.get(map).unwrap_or(&none);
            let plan = (!SEED_MAPS.contains(&map)).then_some(&[][..]);
            check_probe(map, response, plan).map_err(TraverseError::Cli)
        }
    }

    #[test]
    fn named_opt_in_seed_maps_pass_the_preflight_and_the_row_has_costs_and_the_overlay_hash() {
        let seeds = json!({"seed_source": "bm25", "seeds": ["x"]});
        let literal = json!({"seed_source": "literal", "seeds": ["x"]});
        let probe = ProbeFixture {
            answers: BTreeMap::from([
                ("seeds.bm25".to_owned(), seeds),
                ("seeds.literal".to_owned(), literal),
            ]),
        };
        let maps: Vec<String> = ["seeds.bm25", "seeds.literal", "graph.scip"]
            .map(String::from)
            .into();
        let unavailable = preflight_maps(&probe, &maps[..2], "q");
        assert!(unavailable.is_empty(), "{unavailable:?}");
        // The same traverser refuses a seeds.* name nothing knows.
        let nope = preflight_maps(&probe, &["seeds.nope".to_owned()], "q");
        assert_eq!(nope.len(), 1);

        // The arm's overlay: derived from the map names, hashed into the row.
        let dir = std::env::temp_dir().join(format!("xtask-seed-maps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let overlay = seed_map_overlay(&maps, None, &dir).unwrap().unwrap();
        assert_eq!(overlay.sha256.len(), 64);
        let mut raw = traversal(&["x"], &[SEARCH_FN], 6);
        raw["profile_hash"] = json!("abc123");
        raw["totals"]["external_calls"] = json!(2);
        let items = vec![impact_item(
            "i1",
            "q one",
            "src/app/search.rs",
            "search::rerank_document",
        )];
        let mut config = config(&["seeds.bm25", "seeds.literal", "graph.scip"]);
        config.note = note_with_flags(
            None,
            "boost",
            [false, false],
            false,
            vec![Some(overlay.note())],
        );
        config.overlay = Some(overlay.clone());
        let fixture = Fixture::new(BTreeMap::from([("q one".to_owned(), raw)]));
        let outcome = bench_set(Set::Impact, &items, &fixture, &config).unwrap();
        let cells = &outcome.row.cells;
        assert_eq!(cells[5], "none");
        assert_eq!(cells[18], "none", "no map unavailable");
        // Wall time and the cost columns are filled like any other arm.
        assert_ne!(cells[16], "-", "wall time");
        assert_ne!(cells[10], "-", "pops");
        assert_eq!(cells[17], "seeds.bm25,seeds.literal,graph.scip");
        assert!(
            cells[20].contains(&format!("config=seeds-on@{}", &overlay.sha256[..12])),
            "{}",
            cells[20]
        );
        assert!(
            cells[20].contains("seeds.bm25, seeds.literal on"),
            "{}",
            cells[20]
        );
        assert!(
            outcome.report["costs"].is_object(),
            "{}",
            outcome.report["costs"]
        );
        assert!(outcome.items["items"]["i1"]["expansions"].is_number());
        let record = &outcome.report["config_overlay"];
        assert_eq!(record["sha256"], overlay.sha256);
        assert_eq!(record["profile_hashes"], json!(["abc123"]));
        assert_eq!(
            record["seed_maps_on"],
            json!(["seeds.bm25", "seeds.literal"])
        );
        assert_eq!(record["derived_from_sha256"], Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn naming_an_opt_in_seed_map_turns_its_key_on_in_that_arms_overlay_only() {
        let dir = std::env::temp_dir().join(format!("xtask-seed-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let strings = |maps: &[&str]| maps.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let parse = |text: &str| -> lci_core::config::Config { toml::from_str(text).unwrap() };

        // The defaults stay off (the golden defaults file is not touched).
        let defaults = lci_core::config::Config::default();
        assert!(!defaults.run_profile.seeds.bm25 && !defaults.run_profile.seeds.literal);

        // Neither map named, or only maps that are not opt-in seed maps, or an
        // unknown seeds.* name: no file, the overlay is as it was.
        let out = dir.join("none");
        for maps in [
            strings(&[]),
            strings(&["seeds.hybrid", "seeds.scip_names", "graph.scip"]),
            strings(&["seeds.nope"]),
        ] {
            assert_eq!(seed_map_overlay(&maps, None, &out).unwrap(), None);
        }
        assert!(!out.exists());

        // bm25 named, no overlay: a file with that key only.
        let derived = seed_map_overlay(&strings(&["seeds.bm25", "graph.scip"]), None, &out)
            .unwrap()
            .unwrap();
        assert_eq!(derived.path, out.join("seeds-on.toml"));
        assert_eq!(derived.seed_maps_on, ["seeds.bm25"]);
        assert_eq!(derived.derived_from, None);
        let config = parse(&std::fs::read_to_string(&derived.path).unwrap());
        assert!(config.run_profile.seeds.bm25 && !config.run_profile.seeds.literal);
        config.validate().unwrap();

        // literal named next to a user overlay: its other keys stay, the
        // user's hash is recorded, the identity differs from the user's file.
        let user = dir.join("pricing.toml");
        std::fs::write(
            &user,
            "# an arm\n[run_profile.walk]\npricing = \"relative\"\n\n[channels]\nsemantic = false\nreranker = false\n",
        )
        .unwrap();
        let user = ConfigOverlay::load(&user).unwrap();
        let both = strings(&["seeds.literal", "seeds.bm25"]);
        let derived = seed_map_overlay(&both, Some(user.clone()), &dir.join("arm"))
            .unwrap()
            .unwrap();
        assert_eq!(derived.path, dir.join("arm").join("pricing+seeds-on.toml"));
        assert_eq!(derived.derived_from.as_deref(), Some(user.sha256.as_str()));
        assert_eq!(derived.seed_maps_on, ["seeds.bm25", "seeds.literal"]);
        assert_ne!(derived.sha256, user.sha256);
        assert!(derived.note().starts_with("config=pricing+seeds-on@"));
        let config = parse(&std::fs::read_to_string(&derived.path).unwrap());
        assert!(config.run_profile.seeds.bm25 && config.run_profile.seeds.literal);
        assert_eq!(
            config.run_profile.walk.pricing,
            lci_core::config::WalkPricing::Relative
        );
        assert!(!config.channels.semantic && !config.channels.reranker);
        // The same inputs give the same file content (the hash is a run identity).
        let again = seed_map_overlay(&both, Some(user.clone()), &dir.join("arm2"))
            .unwrap()
            .unwrap();
        assert_eq!(again.sha256, derived.sha256);

        // A user overlay that has the key on already is used as it is.
        let on = dir.join("on.toml");
        std::fs::write(&on, "[run_profile.seeds]\nbm25 = true\n").unwrap();
        let on = ConfigOverlay::load(&on).unwrap();
        let same =
            seed_map_overlay(&strings(&["seeds.bm25"]), Some(on.clone()), &dir.join("on")).unwrap();
        assert_eq!(same, Some(on.clone()));
        assert!(!dir.join("on").exists());
        // ... but not when it names a second map whose key is off.
        let more = seed_map_overlay(&both, Some(on), &dir.join("on2"))
            .unwrap()
            .unwrap();
        assert!(
            parse(&std::fs::read_to_string(&more.path).unwrap())
                .run_profile
                .seeds
                .literal
        );

        // An overlay that is not a table where the key goes is an error, not a
        // silent overwrite.
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[run_profile]\nseeds = 3\n").unwrap();
        let bad = ConfigOverlay::load(&bad).unwrap();
        let err =
            seed_map_overlay(&strings(&["seeds.bm25"]), Some(bad), &dir.join("bad")).unwrap_err();
        assert!(format!("{err:#}").contains("not a table"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn score_k_scores_the_first_answers_of_a_longer_list() {
        let dir = std::env::temp_dir().join(format!("xtask-score-k-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        let both = traversal(&["x"], &[METHOD, SEARCH_FN], 5);
        std::fs::write(dir.join("raw/i1.json"), both.to_string()).unwrap();
        let items = vec![impact_item("i1", "q1", "src/app/search.rs", "n")];
        let raw = RawOutputs {
            dir: dir.join("raw"),
        };
        let answers = |score_k| {
            let mut cfg = config(&["graph.scip"]);
            cfg.score_k = score_k;
            let outcome = bench_set(Set::Impact, &items, &raw, &cfg).unwrap();
            outcome.run["i1"].as_array().unwrap().len()
        };
        assert_eq!(
            (answers(None), answers(Some(1)), answers(Some(9))),
            (2, 1, 2)
        );
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

    // ---- BR3: --max-depth, --policy-dir

    fn cli_traverser() -> CliTraverser {
        CliTraverser {
            lci: PathBuf::from("lci"),
            scip: PathBuf::from("index.scip"),
            tree: PathBuf::from("tree"),
            maps: vec!["graph.scip".into()],
            policy: None,
            config: None,
            policy_dir: None,
            max_depth: None,
            fusion: None,
            pass_through: false,
            rerank_frontier: false,
            no_score_bound: false,
            trajectory: false,
            priors: None,
            tie_break: None,
            first_stage: None,
            first_k: None,
            answer_seeds: false,
            exact_seeds: false,
            top_k: 10,
            out: PathBuf::from("out"),
            timeout: Duration::from_secs(1),
            probe: std::cell::RefCell::new(None),
        }
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn max_depth_and_an_items_policy_file_reach_the_cli() {
        let mut t = cli_traverser();
        let plain = args_of(&t.command(&t.maps, &[], "q", None));
        assert!(!plain.iter().any(|a| a.starts_with("--max-depth")));
        assert!(!plain.contains(&"--policy".to_owned()));
        t.max_depth = Some(1);
        t.policy = Some("impact".into());
        let args = args_of(&t.command(&t.maps, &[], "q", None));
        assert!(args.contains(&"--max-depth=1".to_owned()));
        let at = args.iter().position(|a| a == "--policy").unwrap();
        assert_eq!(args[at + 1], "impact");
        // An item's file wins over --policy.
        let file = PathBuf::from("pol/item-1.json");
        let args = args_of(&t.command(&t.maps, &[], "q", Some(&file)));
        let at = args.iter().position(|a| a == "--policy").unwrap();
        assert_eq!(PathBuf::from(&args[at + 1]), file);
        assert_eq!(args.iter().filter(|a| *a == "--policy").count(), 1);
    }

    #[test]
    fn the_trajectory_flag_reaches_the_cli_only_when_given() {
        let mut t = cli_traverser();
        let plain = args_of(&t.command(&t.maps, &[], "q", None));
        assert!(!plain.contains(&"--trajectory".to_owned()));
        t.trajectory = true;
        let args = args_of(&t.command(&t.maps, &[], "q", None));
        assert_eq!(args.iter().filter(|a| *a == "--trajectory").count(), 1);
        // Nothing else changes: the flag is not a run config key.
        let without: Vec<&String> = args.iter().filter(|a| *a != "--trajectory").collect();
        assert_eq!(without, plain.iter().collect::<Vec<_>>());
    }

    #[test]
    fn the_config_overlay_reaches_every_cli_call_only_when_given() {
        let mut t = cli_traverser();
        let plain = args_of(&t.command(&t.maps, &[], "q", None));
        assert!(!plain.contains(&"--config".to_owned()));
        t.config = Some(PathBuf::from("arm/pricing-relative.toml"));
        // Every kind of call (the item call, a probe with seeds and no maps)
        // goes through `command`, so each carries it.
        for args in [
            args_of(&t.command(&t.maps, &[], "q", None)),
            args_of(&t.command(&[], &["sym:a".to_owned()], "q", None)),
        ] {
            assert_eq!(args.iter().filter(|a| *a == "--config").count(), 1);
            let at = args.iter().position(|a| a == "--config").unwrap();
            assert_eq!(args[at + 1], "arm/pricing-relative.toml");
            // A global flag of the CLI: before the subcommand.
            assert!(at < args.iter().position(|a| a == "traverse").unwrap());
        }
        // Nothing else changes.
        let args = args_of(&t.command(&t.maps, &[], "q", None));
        let without: Vec<&String> = args
            .iter()
            .filter(|a| *a != "--config" && *a != "arm/pricing-relative.toml")
            .collect();
        assert_eq!(without, plain.iter().collect::<Vec<_>>());
    }

    #[test]
    fn config_overlays_change_the_run_identity_and_no_overlay_changes_nothing() {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: BenchArgs,
        }
        let dir = std::env::temp_dir().join(format!("xtask-overlay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rank = dir.join("rank.toml");
        let relative = dir.join("relative.toml");
        std::fs::write(&rank, "[run_profile.walk]\npricing = \"rank\"\n").unwrap();
        std::fs::write(&relative, "[run_profile.walk]\npricing = \"relative\"\n").unwrap();
        let rank = ConfigOverlay::load(&rank).unwrap();
        let relative = ConfigOverlay::load(&relative).unwrap();
        assert_eq!(rank.sha256.len(), 64);
        assert_ne!(rank.sha256, relative.sha256);
        assert_ne!(rank.note(), relative.note());
        assert!(rank.note().starts_with("config=rank@"));
        // The same bytes under another name are the same overlay content.
        let copy = dir.join("copy.toml");
        std::fs::write(&copy, "[run_profile.walk]\npricing = \"rank\"\n").unwrap();
        assert_eq!(ConfigOverlay::load(&copy).unwrap().sha256, rank.sha256);

        // No overlay: no note part, as before.
        let plain = Wrapper::parse_from(["x", "--set", "impact", "--gold-dir", "g"]).args;
        assert!(plain.config.is_none());
        assert!(extra_notes(&plain, None).iter().all(Option::is_none));
        let given = Wrapper::parse_from([
            "x",
            "--set",
            "impact",
            "--gold-dir",
            "g",
            "--config",
            "c.toml",
        ])
        .args;
        assert_eq!(given.config, Some(PathBuf::from("c.toml")));
        let notes: Vec<String> = extra_notes(&given, Some(&rank))
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(notes, vec![rank.note()]);

        // The run: score.json gains the block only with an overlay, and it
        // carries the profile hash the traversals reported.
        let items = vec![impact_item(
            "i1",
            "q one",
            "src/app/search.rs",
            "search::rerank_document",
        )];
        let mut raw = traversal(&["x"], &[SEARCH_FN], 6);
        raw["profile_hash"] = json!("abc123");
        let answers = BTreeMap::from([("q one".to_owned(), raw)]);
        let run_with = |overlay: Option<&ConfigOverlay>| {
            let mut config = config(&["graph.scip", "seeds.scip_names"]);
            config.overlay = overlay.cloned();
            config.note = note_with_flags(
                None,
                "boost",
                [false, false],
                false,
                extra_notes(&plain, overlay),
            );
            bench_set(Set::Impact, &items, &Fixture::new(answers.clone()), &config).unwrap()
        };
        let none = run_with(None);
        let a = run_with(Some(&rank));
        let b = run_with(Some(&relative));
        assert!(none.report.get("config_overlay").is_none());
        assert!(!none.row.cells[20].contains("config="));
        assert_eq!(a.report["config_overlay"]["sha256"], rank.sha256);
        assert_eq!(
            a.report["config_overlay"]["profile_hashes"],
            json!(["abc123"])
        );
        assert_ne!(
            a.report["config_overlay"]["sha256"],
            b.report["config_overlay"]["sha256"]
        );
        assert_ne!(a.row.cells[20], b.row.cells[20]);
        assert_ne!(a.row.cells[20], none.row.cells[20]);
        // Everything but the overlay record and the note is the same.
        let mut stripped = a.report.clone();
        stripped.as_object_mut().unwrap().remove("config_overlay");
        assert_eq!(stripped, none.report);
        assert_eq!(a.run, none.run);
        assert_eq!(a.items, none.items);

        // A missing file stops the run before any work.
        let err = ConfigOverlay::load(&dir.join("missing.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("--config overlay"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_config_overlay_fails_run_before_any_gold_is_read() {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: BenchArgs,
        }
        // The gold dir does not exist either: the overlay is checked first.
        let args = Wrapper::parse_from([
            "x",
            "--set",
            "impact",
            "--gold-dir",
            "no-such-gold-dir",
            "--config",
            "no-such-overlay.toml",
        ])
        .args;
        let err = run(args).unwrap_err();
        assert!(
            format!("{err:#}").contains("no-such-overlay.toml"),
            "{err:#}"
        );
    }

    #[test]
    fn an_item_without_a_policy_file_is_skipped_not_run_without_steering() {
        let dir = std::env::temp_dir().join(format!("xtask-br3-pd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("impact-1.json"), "[]").unwrap();
        let mut t = cli_traverser();
        t.lci = dir.join("no-such-binary");
        t.policy_dir = Some(dir.clone());
        // The file of impact-1 exists, so the call gets as far as running the binary.
        let err = t.traverse("impact-1", "q").unwrap_err();
        assert!(err.to_string().contains("run the lci binary"), "{err}");
        // impact-2 has no file: refused before any run.
        let err = t.traverse("impact-2", "q").unwrap_err();
        assert!(
            err.to_string()
                .contains("no oracle policy file for impact-2"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_row_says_max_depth_and_oracle() {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: BenchArgs,
        }
        let plain = Wrapper::parse_from(["x", "--set", "impact", "--gold-dir", "g"]).args;
        assert!(extra_notes(&plain, None).iter().all(Option::is_none));
        assert_eq!(policy_label_of(&plain), "default");
        let oracle = Wrapper::parse_from([
            "x",
            "--set",
            "impact",
            "--gold-dir",
            "g",
            "--max-depth",
            "1",
            "--policy-dir",
            "work/oracle-impact",
        ])
        .args;
        let notes: Vec<String> = extra_notes(&oracle, None).into_iter().flatten().collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0], "max-depth=1");
        assert!(notes[1].starts_with("ORACLE"));
        assert_eq!(policy_label_of(&oracle), "oracle:oracle-impact");
        let note = note_with_flags(
            None,
            "boost",
            [false, false],
            false,
            extra_notes(&oracle, None),
        );
        assert!(note.contains("max-depth=1") && note.contains("ORACLE"));
    }

    // ---- BR4b: the engine's first stage

    #[test]
    fn the_first_stage_flags_reach_the_cli_and_the_row_names_them() {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: BenchArgs,
        }
        let mut t = cli_traverser();
        let plain = args_of(&t.command(&t.maps, &[], "q", None));
        assert!(!plain.iter().any(|a| a.contains("first")
            || a.contains("answer-seeds")
            || a.contains("exact-seeds")));
        t.first_stage = Some("interleave".into());
        t.first_k = Some(0);
        t.answer_seeds = true;
        t.exact_seeds = true;
        let args = args_of(&t.command(&t.maps, &[], "q", None));
        for flag in [
            "--first-stage=interleave",
            "--first-k=0",
            "--answer-seeds",
            "--exact-seeds",
        ] {
            assert!(args.contains(&flag.to_owned()), "{flag} in {args:?}");
        }
        let none = Wrapper::parse_from(["x", "--set", "impact", "--gold-dir", "g"]).args;
        assert_eq!(first_stage_note(&none), None);
        let given = Wrapper::parse_from([
            "x",
            "--set",
            "impact",
            "--gold-dir",
            "g",
            "--first-stage",
            "append",
            "--first-k",
            "10",
            "--answer-seeds",
        ])
        .args;
        let notes: Vec<String> = extra_notes(&given, None).into_iter().flatten().collect();
        assert_eq!(notes, ["first-stage=append first_k=10", "answer-seeds"]);
        let off = Wrapper::parse_from([
            "x",
            "--set",
            "impact",
            "--gold-dir",
            "g",
            "--first-stage",
            "off",
        ])
        .args;
        assert_eq!(
            first_stage_note(&off).as_deref(),
            Some("first-stage=off first_k=config")
        );
    }

    #[test]
    fn the_exact_maps_are_checked_by_registration_alone() {
        // Descriptor-only: never walked, so no per-map call count exists.
        let response = json!({"totals": {"per_map": {}}});
        let on = plan(&[("exact.closure", true)]);
        assert!(check_probe("exact.closure", &response, Some(&on)).is_ok());
        let off = plan(&[("exact.closure", false)]);
        assert!(check_probe("exact.closure", &response, Some(&off)).is_err());
        let left_out = plan(&[("graph.scip", true)]);
        assert!(check_probe("exact.pickaxe", &response, Some(&left_out)).is_err());
        let maps = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
        assert!(can_answer(Set::History, &maps(&["exact.pickaxe"])));
        assert!(!can_answer(Set::History, &maps(&["exact.closure"])));
        assert!(can_answer(Set::Impact, &maps(&["exact.closure"])));
    }

    #[test]
    fn exact_cards_are_scored_by_their_site_in_the_engines_order_and_the_row_counts_them() {
        // The engine's results: an exact card first (a declaration the graph
        // does not hold, located by its site), then a walk card.
        let mut raw = traversal(&["x"], &[TYPE, SEARCH_FN], 1);
        raw["results"][0]["origin"] = json!("exact");
        raw["results"][0]["site"] = json!({"path": "src/app/search.rs", "line": 12, "col": 4});
        raw["results"][1]["origin"] = json!("walk");
        raw["exact"] = json!([{"map": "exact.closure", "answered": true, "cards": 1}]);
        let mut abstained = traversal(&["x"], &[SEARCH_FN], 1);
        abstained["exact"] = json!([{"map": "exact.closure", "answered": false, "cards": 0}]);
        let answers = BTreeMap::from([("q1".to_owned(), raw), ("q2".to_owned(), abstained)]);
        let items = vec![
            impact_item("i1", "q1", "src/app/search.rs", "search::rerank_document"),
            impact_item("i2", "q2", "src/app/search.rs", "search::rerank_document"),
        ];
        let mut cfg = config(&["exact.closure", "graph.scip"]);
        cfg.note = "first-stage=append first_k=10".into();
        let outcome = bench_set(Set::Impact, &items, &Fixture::new(answers), &cfg).unwrap();
        let note = &outcome.row.cells[20];
        assert!(note.contains("first-stage=append first_k=10"), "{note}");
        assert!(note.contains("exact answered=1/2"), "{note}");
        // The exact card is the first answer, scored by its own site.
        let first = &outcome.run["i1"][0];
        assert_eq!(first["path"], "src/app/search.rs");
        assert_eq!(first["line"], 12);
        // No exact map selected: no such note.
        let plain = bench_set(
            Set::Impact,
            &items,
            &Fixture::new(BTreeMap::from([
                ("q1".to_owned(), traversal(&["x"], &[SEARCH_FN], 1)),
                ("q2".to_owned(), traversal(&["x"], &[SEARCH_FN], 1)),
            ])),
            &config(&["graph.scip"]),
        )
        .unwrap();
        assert!(!plain.row.cells[20].contains("exact answered"));
    }
}
