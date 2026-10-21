//! `cargo xtask maps trajectories`: join the `pop` events of `lci traverse
//! --trajectory` runs to the gold outcome (K33a-2).
//!
//! A bench out dir made with `maps bench --trajectory` holds, per item, a trace
//! with one `pop` event per counted pop (the state at the choice and the best
//! queued candidates). This command writes one JSON row per pop to
//! `<wave>/<set>/trajectories.jsonl` and a summary to `trajectories.md` beside
//! it. It is a separate command, so a bench run's files are not touched.
//!
//! Gold labels come from [`is_gold`] only (the set's own scorer on the answer
//! the bench adapter offers for the node). A symbol node no card and no SCIP
//! index located has no label: `null`, never `false`.
//!
//! Pops are counted from the trace's `end.totals` (`pops` plus
//! `pass_through_pops`): the first that many `pop` events are `counted`. The
//! bench never resumes, so only the last pop event can be a pop that was put
//! back (its `expand` was dropped for the budget, or cut with nothing adopted):
//! the next call pops the node again and reuses the number, so the last event
//! can carry a `pop` one above `end.totals.pops` and is written with
//! `counted: false`. A put-back pop whose call timed out stays in
//! `end.totals.pops` (the engine keeps it counted) and so stays `counted`.
//! Only a trace with no `end` falls back to the `expand` event. The summary
//! and `gold_before` use counted rows only. The engine's `pop` number repeats
//! the previous paid number for a free pop under `pass_through`: order rows by
//! `event`, which the summary's ordinals (first gold pop) follow.
//!
//! Size: one row is at most about 2.3 KB with 100-character node ids and the
//! default 8 candidates (`ROW_BYTES_BOUND`, asserted in a test). Rows = pops,
//! so a 100-item set at the 200-pop cap is at most about 46 MB, typically a few
//! MB. The files go under the bench out dir (`test-results/`), never into git.

use super::bench::{self, ResultNode, Site, Traversed};
use super::fit_priors::{is_gold, scip_sites, set_runs};
use crate::gold::{self, Set};
use anyhow::{Context, Result};
use clap::Args;
use lci_maps::traverse::trace::read_trace;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The keys of a row, as JSON pointers; `[]` stands for every element of an
/// array. `outcome/item` also carries the headline metrics of the set.
#[cfg(test)]
pub const SCHEMA: &[&str] = &[
    "set",
    "wave",
    "item",
    "trace",
    "event",
    "pop",
    "free",
    "counted",
    "state",
    "state/depth",
    "state/frontier",
    "state/discovered",
    "state/tokens_left",
    "state/calls_left",
    "choice",
    "choice/node",
    "choice/priority",
    "choice/via",
    "choice/parent",
    "choice/gold",
    "candidates",
    "candidates[]/node",
    "candidates[]/priority",
    "candidates[]/depth",
    "candidates[]/via",
    "candidates[]/gold",
    "expand",
    "expand/offered",
    "expand/adopted",
    "expand/cut",
    "expand/dropped",
    "outcome",
    "outcome/gold_candidate_skipped",
    "outcome/gold_before",
    "outcome/item",
    "outcome/item/f0.5",
];

/// The most bytes a row takes with 100-character node ids and the default 8
/// candidates.
#[cfg(test)]
pub const ROW_BYTES_BOUND: usize = 2400;

#[derive(Args)]
pub struct TrajectoriesArgs {
    /// Bench out dirs (the `--out-dir` of `maps bench --trajectory`), or a
    /// directory of them.
    #[arg(long, required = true, num_args = 1..)]
    runs: Vec<PathBuf>,
    /// Directory holding the gold files (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The SCIP index the bench ran with: gives the symbol nodes no card
    /// located their site, so they get a label instead of `null`.
    #[arg(long)]
    scip: Option<PathBuf>,
    /// Only this set.
    #[arg(long, value_enum)]
    set: Option<Set>,
}

/// The `pop` event of a trace (`lci_maps::traverse` module doc).
#[derive(Deserialize)]
struct PopEvent {
    pop: u64,
    #[serde(default)]
    free: bool,
    node: String,
    priority: f64,
    depth: u64,
    #[serde(default)]
    via: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    frontier: u64,
    discovered: u64,
    tokens_left: u64,
    calls_left: u64,
    #[serde(default)]
    candidates: Vec<CandidateEvent>,
}

#[derive(Deserialize)]
struct CandidateEvent {
    node: String,
    priority: f64,
    depth: u64,
    #[serde(default)]
    via: Option<String>,
}

/// What the `expand` event of a pop says; also the row's `expand` object.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
struct Expand {
    #[serde(default)]
    offered: u64,
    #[serde(default)]
    adopted: u64,
    #[serde(default)]
    cut: bool,
    #[serde(default)]
    dropped: Option<String>,
}

/// Whether the pop stands in `end.totals`, judged from its `expand` event:
/// only the fallback for a trace with no `end`. A pop whose expansion was
/// dropped for the budget, or cut with nothing adopted, is put back; the engine
/// keeps a put-back pop counted when its call timed out, which the `expand`
/// event does not say.
fn is_counted(expand: Option<&Expand>) -> bool {
    expand.is_none_or(|e| e.dropped.is_none() && !(e.cut && e.adopted == 0))
}

#[derive(Serialize)]
struct RowState {
    depth: u64,
    frontier: u64,
    discovered: u64,
    tokens_left: u64,
    calls_left: u64,
}

#[derive(Serialize)]
struct Choice {
    node: String,
    priority: f64,
    via: Option<String>,
    parent: Option<String>,
    gold: Option<bool>,
}

#[derive(Serialize)]
struct RowCandidate {
    node: String,
    priority: f64,
    depth: u64,
    via: Option<String>,
    gold: Option<bool>,
}

#[derive(Serialize)]
struct Outcome {
    /// A candidate is gold and the choice is not. `null` when a candidate is
    /// gold and the choice has no label, or when no candidate is known gold
    /// and one has no label; `false` only when every candidate is labelled and
    /// none is gold, or the choice is gold.
    gold_candidate_skipped: Option<bool>,
    /// Counted gold choices so far in this item.
    gold_before: u64,
    /// `f0.5` and the headline metrics of the set, from the item's record in
    /// `items.json` (`null` for one the record lacks).
    item: BTreeMap<String, Option<f64>>,
}

#[derive(Serialize)]
struct Row<'a> {
    set: &'a str,
    wave: &'a str,
    item: &'a str,
    trace: &'a str,
    /// Index of the `pop` event among the trace's events.
    event: usize,
    pop: u64,
    free: bool,
    /// False for a pop that was put back (see the module doc).
    counted: bool,
    state: RowState,
    choice: Choice,
    candidates: Vec<RowCandidate>,
    expand: Option<Expand>,
    outcome: Outcome,
}

/// What the summary needs of one row.
struct Fact {
    /// 1-based position of the row among the item's `pop` events (the
    /// engine's `pop` number repeats under `pass_through`).
    ordinal: u64,
    counted: bool,
    gold: Option<bool>,
    skipped: Option<bool>,
}

/// One item's rows (JSON lines), their facts and the pops its `end` event says.
struct ItemJoin {
    lines: Vec<String>,
    facts: Vec<Fact>,
    end_pops: Option<u64>,
}

/// Where a gold label comes from: the sites the raw output's cards gave, the
/// SCIP index's, and the labels already asked for this item.
struct Labels<'a> {
    set: Set,
    item: &'a Value,
    sites: &'a BTreeMap<String, Site>,
    scip: &'a HashMap<String, Site>,
    memo: HashMap<String, Option<bool>>,
}

impl Labels<'_> {
    /// Whether the node is gold; `None` when it is a symbol nobody located.
    fn of(&mut self, node: &str) -> Result<Option<bool>> {
        if let Some(label) = self.memo.get(node) {
            return Ok(*label);
        }
        let site = self
            .sites
            .get(node)
            .or_else(|| self.scip.get(node))
            .cloned();
        let label = if site.is_none() && node.starts_with("sym:") {
            None
        } else {
            let result = ResultNode {
                id: node.to_owned(),
                text: node.to_owned(),
                site,
            };
            Some(is_gold(self.set, self.item, result)?)
        };
        self.memo.insert(node.to_owned(), label);
        Ok(label)
    }
}

/// The `expand` event of the pop at `at`: the next one for the same node
/// before the next `pop` event.
fn expand_of(events: &[Value], at: usize, node: &str) -> Result<Option<Expand>> {
    for event in &events[at + 1..] {
        match event["ev"].as_str() {
            Some("pop") => return Ok(None),
            Some("expand") if event["node"].as_str() == Some(node) => {
                return Ok(Some(serde_json::from_value(event.clone())?));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// The pops an `end` event counts: `pops` plus `pass_through_pops`.
fn end_pops(events: &[Value]) -> Option<u64> {
    let totals = &events.iter().rfind(|e| e["ev"] == "end")?["totals"];
    Some(totals["pops"].as_u64()? + totals["pass_through_pops"].as_u64().unwrap_or(0))
}

/// One row per `pop` event of a trace.
#[allow(clippy::too_many_arguments)]
fn join_item(
    set: Set,
    wave: &str,
    id: &str,
    trace_file: &str,
    events: &[Value],
    labels: &mut Labels,
    record: &Value,
) -> Result<ItemJoin> {
    let set_label = bench::set_name(set);
    let mut metrics = BTreeMap::new();
    for name in std::iter::once("f0.5").chain(bench::headline(set).iter().copied()) {
        metrics.insert(name.to_owned(), record[name].as_f64());
    }
    let mut join = ItemJoin {
        lines: Vec::new(),
        facts: Vec::new(),
        end_pops: end_pops(events),
    };
    let mut gold_before = 0;
    let pop_events = events.iter().enumerate().filter(|(_, e)| e["ev"] == "pop");
    for (k, (at, event)) in pop_events.enumerate() {
        let pop: PopEvent = serde_json::from_value(event.clone())
            .with_context(|| format!("pop event {at} of the trace of {id}"))?;
        let expand = expand_of(events, at, &pop.node)?;
        // With an `end`, the pops it counts are the first `end_pops` pop
        // events (the bench never resumes, so only the last can be put back,
        // and a put-back that timed out stays counted). Without one, the
        // `expand` event says.
        let counted = match join.end_pops {
            Some(end_pops) => (k as u64) < end_pops,
            None => is_counted(expand.as_ref()),
        };
        let choice_gold = labels.of(&pop.node)?;
        let mut candidates = Vec::new();
        for c in pop.candidates {
            candidates.push(RowCandidate {
                gold: labels.of(&c.node)?,
                node: c.node,
                priority: c.priority,
                depth: c.depth,
                via: c.via,
            });
        }
        let skipped = if candidates.iter().any(|c| c.gold == Some(true)) {
            choice_gold.map(|gold| !gold)
        } else if candidates.iter().any(|c| c.gold.is_none()) {
            // An unlabelled candidate might be the gold one.
            None
        } else {
            Some(false)
        };
        let row = Row {
            set: set_label,
            wave,
            item: id,
            trace: trace_file,
            event: at,
            pop: pop.pop,
            free: pop.free,
            counted,
            state: RowState {
                depth: pop.depth,
                frontier: pop.frontier,
                discovered: pop.discovered,
                tokens_left: pop.tokens_left,
                calls_left: pop.calls_left,
            },
            choice: Choice {
                node: pop.node,
                priority: pop.priority,
                via: pop.via,
                parent: pop.parent,
                gold: choice_gold,
            },
            candidates,
            expand,
            outcome: Outcome {
                gold_candidate_skipped: skipped,
                gold_before,
                item: metrics.clone(),
            },
        };
        join.lines.push(serde_json::to_string(&row)?);
        join.facts.push(Fact {
            ordinal: k as u64 + 1,
            counted,
            gold: choice_gold,
            skipped,
        });
        if counted && choice_gold == Some(true) {
            gold_before += 1;
        }
    }
    Ok(join)
}

/// What one (wave, set) run contained.
#[derive(Debug, Default, PartialEq)]
struct SetStats {
    /// Items whose trace has `pop` events.
    items: usize,
    /// Items with no raw output, or one that names no trace.
    no_trace: usize,
    /// Items the scorer leaves out of its population.
    unscored: usize,
    /// Ids of items with a trace and no `pop` event.
    no_pops: Vec<String>,
    /// Items whose trace has no `end` event (pops then counted from the rows).
    no_end: usize,
    /// Pops, from `end.totals`.
    pops: u64,
    rows: usize,
    /// Rows of pops that were put back.
    uncounted: usize,
    /// Counted rows by the label of the choice.
    gold_choices: usize,
    known_choices: usize,
    /// Counted rows by `gold_candidate_skipped`.
    skipped: usize,
    skip_known: usize,
    /// Per item with a gold choice, the pop of the first one.
    first_gold: Vec<u64>,
}

impl SetStats {
    fn add(&mut self, join: &ItemJoin) {
        self.items += 1;
        self.rows += join.facts.len();
        let mut counted = 0;
        let mut first = None;
        for fact in join.facts.iter().filter(|f| f.counted) {
            counted += 1;
            self.known_choices += usize::from(fact.gold.is_some());
            if fact.gold == Some(true) {
                self.gold_choices += 1;
                first.get_or_insert(fact.ordinal);
            }
            self.skip_known += usize::from(fact.skipped.is_some());
            self.skipped += usize::from(fact.skipped == Some(true));
        }
        self.uncounted += join.facts.len() - counted;
        match join.end_pops {
            Some(pops) => self.pops += pops,
            None => {
                self.no_end += 1;
                self.pops += counted as u64;
            }
        }
        self.first_gold.extend(first);
    }

    fn first_gold_p50(&self) -> Option<f64> {
        let mut pops = self.first_gold.clone();
        pops.sort_unstable();
        let n = pops.len();
        match n {
            0 => None,
            _ if n % 2 == 1 => Some(pops[n / 2] as f64),
            _ => Some((pops[n / 2 - 1] + pops[n / 2]) as f64 / 2.0),
        }
    }
}

fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "n/a (0)".into();
    }
    format!(
        "{:.1}% ({part}/{whole})",
        100.0 * part as f64 / whole as f64
    )
}

/// How many ids of items without `pop` events the summary names.
const NAMED_ITEMS: usize = 50;

fn render_md(wave: &str, set: &str, stats: &SetStats) -> String {
    let mut text = format!("# Trajectories: {wave}/{set}\n\n");
    text.push_str("| quantity | value |\n| --- | --- |\n");
    let mut line = |name: &str, value: String| text.push_str(&format!("| {name} | {value} |\n"));
    line("items with pop events", stats.items.to_string());
    line("items without pop events", stats.no_pops.len().to_string());
    line("items without a trace", stats.no_trace.to_string());
    line("items the scorer leaves out", stats.unscored.to_string());
    line("pops (end.totals)", stats.pops.to_string());
    line("rows", stats.rows.to_string());
    line(
        "rows of put-back pops (counted: false)",
        stats.uncounted.to_string(),
    );
    let counted = stats.rows - stats.uncounted;
    line(
        "gold choices (counted rows with a label)",
        percent(stats.gold_choices, stats.known_choices),
    );
    line(
        "choices without a label (null)",
        (counted - stats.known_choices).to_string(),
    );
    line(
        "pops with a gold candidate skipped",
        percent(stats.skipped, stats.skip_known),
    );
    line(
        "first gold pop p50 (items with a gold choice)",
        match stats.first_gold_p50() {
            Some(p50) => format!("{p50} ({} items)", stats.first_gold.len()),
            None => "n/a (0)".into(),
        },
    );
    if !stats.no_pops.is_empty() {
        text.push_str(
            "\nItems with a trace and no `pop` event (the run was made without `--trajectory`; not an error):\n\n",
        );
        for id in stats.no_pops.iter().take(NAMED_ITEMS) {
            text.push_str(&format!("- {id}\n"));
        }
        if stats.no_pops.len() > NAMED_ITEMS {
            text.push_str(&format!(
                "- and {} more\n",
                stats.no_pops.len() - NAMED_ITEMS
            ));
        }
    }
    if stats.no_end > 0 {
        text.push_str(&format!(
            "\n{} trace(s) have no `end` event: their pops are the counted rows.\n",
            stats.no_end
        ));
    }
    text
}

/// One written (wave, set).
pub struct Written {
    pub wave: String,
    pub set: String,
    pub dir: PathBuf,
    stats: SetStats,
}

/// Writes `trajectories.jsonl` and `trajectories.md` in every (wave, set) dir
/// of the out dirs under `runs` (only `only` when given).
fn write_runs(
    runs: &[PathBuf],
    gold_dir: &Path,
    scip: &HashMap<String, Site>,
    only: Option<Set>,
) -> Result<Vec<Written>> {
    let mut golds: HashMap<&'static str, Vec<Value>> = HashMap::new();
    let mut written = Vec::new();
    for root in runs {
        for (out_dir, set, raw_dir) in set_runs(root)? {
            if only.is_some_and(|only| only != set) {
                continue;
            }
            let name = bench::set_name(set);
            if !golds.contains_key(name) {
                let items = gold::load_gold(&gold_dir.join(format!("{name}.json")))?;
                golds.insert(name, items);
            }
            let dir = raw_dir.parent().context("a raw dir has a parent")?;
            let wave = dir
                .parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            let items_json: Value = match std::fs::read_to_string(dir.join("items.json")) {
                Ok(text) => serde_json::from_str(&text)
                    .with_context(|| format!("{} is not JSON", dir.join("items.json").display()))?,
                Err(_) => Value::Null,
            };
            let mut stats = SetStats::default();
            let mut lines = Vec::new();
            for item in &golds[name] {
                let id = item["id"].as_str().unwrap_or_default();
                let Ok(text) =
                    std::fs::read_to_string(raw_dir.join(format!("{}.json", bench::slug(id))))
                else {
                    stats.no_trace += 1;
                    continue;
                };
                let raw: Value = serde_json::from_str(&text)
                    .with_context(|| format!("raw output of {id} is not JSON"))?;
                let Some(file) = raw["trace"]["file"].as_str() else {
                    stats.no_trace += 1;
                    continue;
                };
                let empty = gold::Run::new();
                let population =
                    gold::score_set(set, std::slice::from_ref(item), &empty, false, false)?;
                if population["scored_items"] == 0 {
                    stats.unscored += 1;
                    continue;
                }
                let events = read_trace(out_dir.join("cli").join("traces").join(file))
                    .with_context(|| format!("trace of {id}"))?;
                let sites = Traversed::from_json(&raw, None).sites;
                let mut labels = Labels {
                    set,
                    item,
                    sites: &sites,
                    scip,
                    memo: HashMap::new(),
                };
                let join = join_item(
                    set,
                    &wave,
                    id,
                    file,
                    &events,
                    &mut labels,
                    &items_json["items"][id],
                )?;
                if join.facts.is_empty() {
                    stats.no_pops.push(id.to_owned());
                    continue;
                }
                stats.add(&join);
                lines.extend(join.lines);
            }
            let jsonl = dir.join("trajectories.jsonl");
            if lines.is_empty() {
                // A stale file of an earlier run would pass for this run's.
                if jsonl.exists() {
                    std::fs::remove_file(&jsonl)
                        .with_context(|| format!("remove {}", jsonl.display()))?;
                }
            } else {
                let mut text = lines.join("\n");
                text.push('\n');
                std::fs::write(&jsonl, text)
                    .with_context(|| format!("write {}", jsonl.display()))?;
            }
            std::fs::write(dir.join("trajectories.md"), render_md(&wave, name, &stats))?;
            written.push(Written {
                wave,
                set: name.to_owned(),
                dir: dir.to_path_buf(),
                stats,
            });
        }
    }
    Ok(written)
}

pub fn run(args: TrajectoriesArgs) -> Result<()> {
    let scip = match &args.scip {
        Some(index) => scip_sites(index)?,
        None => HashMap::new(),
    };
    let written = write_runs(&args.runs, &args.gold_dir, &scip, args.set)?;
    if written.is_empty() {
        println!("no bench run found under the given --runs");
    }
    for w in &written {
        let s = &w.stats;
        println!(
            "{}/{}: {} items with pop events ({} without, {} without a trace), {} rows ({} put back); \
             gold choices {}, gold candidate skipped {}; {}",
            w.wave,
            w.set,
            s.items,
            s.no_pops.len(),
            s.no_trace,
            s.rows,
            s.uncounted,
            percent(s.gold_choices, s.known_choices),
            percent(s.skipped, s.skip_known),
            w.dir.join("trajectories.md").display(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SEED: &str = "sym:rust:m::seed/fn";
    const MID: &str = "sym:rust:m::mid/fn";
    const OTHER: &str = "sym:rust:m::other/fn";
    const UNKNOWN: &str = "sym:rust:m::unknown/fn";

    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("trajectories-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// An out dir with one flow item `flow-0` (gold: entry e:1, intermediate
    /// mid:5, target t:9) whose trace is `events` and whose raw output has
    /// cards for the seed (line 77), mid (5) and other (50), not for unknown.
    /// Returns (root, out dir, gold dir).
    fn synthetic(name: &str, events: &[Value]) -> (PathBuf, PathBuf, PathBuf) {
        let root = temp(name);
        let out = root.join("out");
        let set_dir = out.join("w").join("flow");
        std::fs::create_dir_all(set_dir.join("raw")).unwrap();
        std::fs::create_dir_all(out.join("cli").join("traces")).unwrap();
        let node = |name: &str, line: u64| json!({"name": name, "file": "src/a.rs", "line": line, "symbol": name});
        let item = json!({
            "id": "flow-0", "question": "q",
            "gold": {"hops": 2, "entry": node("e", 1), "target": node("t", 9),
                     "intermediates": [node("mid", 5)]},
        });
        let trace: String = events.iter().map(|e| format!("{e}\n")).collect();
        std::fs::write(out.join("cli/traces/flow-0.jsonl"), trace).unwrap();
        let card =
            |id: &str, line: u64| json!({"node": id, "site": {"path": "src/a.rs", "line": line}});
        let raw = json!({
            "trace": {"file": "flow-0.jsonl"},
            "checkpoint": {"answer": [card(SEED, 77), card(MID, 5), card(OTHER, 50)]},
        });
        std::fs::write(set_dir.join("raw/flow-0.json"), raw.to_string()).unwrap();
        let gold_dir = root.join("gold");
        std::fs::create_dir_all(&gold_dir).unwrap();
        std::fs::write(gold_dir.join("flow.json"), json!([item]).to_string()).unwrap();
        (root, out, gold_dir)
    }

    fn pop(pop: u64, node: &str, candidates: &[&str]) -> Value {
        let candidates: Vec<Value> = candidates
            .iter()
            .enumerate()
            .map(|(i, n)| json!({"node": n, "priority": 0.5 - i as f64 / 10.0, "depth": 1, "via": "graph.scip"}))
            .collect();
        json!({
            "ev": "pop", "pop": pop, "free": false, "node": node, "priority": 0.9,
            "depth": 0, "via": null, "parent": null, "frontier": 3, "discovered": 4,
            "tokens_left": 900, "calls_left": 2, "candidates": candidates,
        })
    }

    fn expand(pop: u64, node: &str, adopted: u64, extra: Value) -> Value {
        let mut event = json!({
            "ev": "expand", "pop": pop, "node": node, "offered": 2, "adopted": adopted,
            "corroborated": 0, "calls": [], "tokens": 10, "cut": false,
        });
        for (k, v) in extra.as_object().unwrap() {
            event[k.as_str()] = v.clone();
        }
        event
    }

    fn end(pops: u64) -> Value {
        json!({"ev": "end", "status": "complete", "totals": {"pops": pops, "hops": 2}})
    }

    fn rows(set_dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(set_dir.join("trajectories.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Whether `pointer` (`[]` = every array element) is a key of `row`.
    fn has_key(row: &Value, pointer: &str) -> bool {
        let mut at = vec![row];
        for part in pointer.split('/') {
            let (key, each) = match part.strip_suffix("[]") {
                Some(key) => (key, true),
                None => (part, false),
            };
            at = at
                .into_iter()
                .flat_map(|v| {
                    let next = &v[key];
                    if each {
                        next.as_array()
                            .map(|a| a.iter().collect())
                            .unwrap_or_default()
                    } else if v.as_object().is_some_and(|o| o.contains_key(key)) {
                        vec![next]
                    } else {
                        Vec::new()
                    }
                })
                .collect();
            if at.is_empty() {
                return false;
            }
        }
        true
    }

    #[test]
    fn trajectory_rows_have_the_schema_and_the_gold_labels() {
        let events = [
            json!({"ev": "start", "seeds": [SEED]}),
            pop(1, SEED, &[MID, OTHER, UNKNOWN]),
            expand(1, SEED, 2, json!({})),
            json!({"ev": "hop", "src": SEED, "dst": MID, "kind": "calls_or_refs", "map": "graph.scip"}),
            json!({"ev": "hop", "src": SEED, "dst": OTHER, "kind": "contains", "map": "graph.scip"}),
            end(1),
        ];
        let (root, out, gold_dir) = synthetic("schema", &events);
        let items = json!({"set": "flow", "items": {"flow-0": {"f0.5": 0.5, "path_recall": 1.0}}});
        std::fs::write(out.join("w/flow/items.json"), items.to_string()).unwrap();

        let written =
            write_runs(std::slice::from_ref(&out), &gold_dir, &HashMap::new(), None).unwrap();
        assert_eq!(written.len(), 1);
        let set_dir = out.join("w").join("flow");
        let rows = rows(&set_dir);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        for key in SCHEMA {
            assert!(has_key(row, key), "missing key {key} in {row}");
        }
        assert_eq!(row["set"], "flow");
        assert_eq!(row["wave"], "w");
        assert_eq!(row["item"], "flow-0");
        assert_eq!(row["trace"], "flow-0.jsonl");
        assert_eq!(row["event"], 1);
        assert_eq!(
            (row["pop"].as_u64(), row["free"].as_bool()),
            (Some(1), Some(false))
        );
        assert_eq!(row["counted"], true);
        assert_eq!(
            row["state"],
            json!({"depth": 0, "frontier": 3, "discovered": 4, "tokens_left": 900, "calls_left": 2})
        );
        // The seed's card says line 77: a location, not gold.
        assert_eq!(row["choice"]["node"], SEED);
        assert_eq!(row["choice"]["gold"], false);
        // mid is the intermediate, other is elsewhere, unknown has no card.
        let golds: Vec<&Value> = row["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| &c["gold"])
            .collect();
        assert_eq!(golds, [&json!(true), &json!(false), &Value::Null]);
        assert_eq!(
            row["expand"],
            json!({"offered": 2, "adopted": 2, "cut": false, "dropped": null})
        );
        assert_eq!(row["outcome"]["gold_candidate_skipped"], true);
        assert_eq!(row["outcome"]["gold_before"], 0);
        assert_eq!(row["outcome"]["item"]["f0.5"], 0.5);
        assert_eq!(row["outcome"]["item"]["path_recall"], 1.0);
        // A headline metric the record lacks is null, not absent.
        assert_eq!(row["outcome"]["item"]["tau"], Value::Null);

        let md = std::fs::read_to_string(set_dir.join("trajectories.md")).unwrap();
        assert!(md.contains("| items with pop events | 1 |"), "{md}");
        assert!(md.contains("| pops (end.totals) | 1 |"), "{md}");
        assert!(md.contains("| rows | 1 |"), "{md}");
        assert!(md.contains("0.0% (0/1)"), "{md}");
        assert!(md.contains("choices without a label (null) | 0 |"), "{md}");
        assert!(
            md.contains("pops with a gold candidate skipped | 100.0% (1/1)"),
            "{md}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_gold_choice_is_counted_and_an_unlocated_symbol_is_null() {
        let events = [
            json!({"ev": "start", "seeds": [SEED]}),
            pop(1, UNKNOWN, &[MID]),
            expand(1, UNKNOWN, 1, json!({})),
            pop(2, MID, &[OTHER]),
            expand(2, MID, 1, json!({})),
            end(2),
        ];
        let (root, out, gold_dir) = synthetic("gold", &events);
        write_runs(std::slice::from_ref(&out), &gold_dir, &HashMap::new(), None).unwrap();
        let rows = rows(&out.join("w/flow"));
        assert_eq!(rows.len(), 2);
        // An unlocated choice is unknown, and the skipped flag with it.
        assert_eq!(rows[0]["choice"]["gold"], Value::Null);
        assert_eq!(rows[0]["outcome"]["gold_candidate_skipped"], Value::Null);
        assert_eq!(rows[0]["outcome"]["gold_before"], 0);
        assert_eq!(rows[1]["choice"]["gold"], true);
        assert_eq!(rows[1]["outcome"]["gold_candidate_skipped"], false);
        assert_eq!(rows[1]["outcome"]["gold_before"], 0);
        let md = std::fs::read_to_string(out.join("w/flow/trajectories.md")).unwrap();
        assert!(
            md.contains("| gold choices (counted rows with a label) | 100.0% (1/1) |"),
            "{md}"
        );
        assert!(
            md.contains("| choices without a label (null) | 1 |"),
            "{md}"
        );
        assert!(
            md.contains("| first gold pop p50 (items with a gold choice) | 2 (1 items) |"),
            "{md}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_run_without_pop_events_writes_no_rows_and_says_so() {
        let events = [
            json!({"ev": "start", "seeds": [SEED]}),
            expand(1, SEED, 1, json!({})),
            json!({"ev": "hop", "src": SEED, "dst": MID, "kind": "calls_or_refs", "map": "graph.scip"}),
            end(1),
        ];
        let (root, out, gold_dir) = synthetic("nopops", &events);
        let set_dir = out.join("w/flow");
        // A file of an earlier run must not pass for this one's.
        std::fs::write(set_dir.join("trajectories.jsonl"), "stale\n").unwrap();
        let written =
            write_runs(std::slice::from_ref(&out), &gold_dir, &HashMap::new(), None).unwrap();
        assert_eq!(written.len(), 1);
        assert!(!set_dir.join("trajectories.jsonl").exists());
        let md = std::fs::read_to_string(set_dir.join("trajectories.md")).unwrap();
        assert!(md.contains("| items with pop events | 0 |"), "{md}");
        assert!(md.contains("| items without pop events | 1 |"), "{md}");
        assert!(md.contains("without `--trajectory`"), "{md}");
        assert!(md.contains("- flow-0"), "{md}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn put_back_pops_are_not_counted_and_pops_come_from_the_end_totals() {
        // Pop 2 was dropped for the budget and put back: its number is above
        // `end.totals.pops`, and the next call reuses it.
        let events = [
            json!({"ev": "start", "seeds": [SEED]}),
            pop(1, SEED, &[MID]),
            expand(1, SEED, 1, json!({})),
            pop(2, MID, &[OTHER]),
            expand(2, MID, 0, json!({"dropped": "budget"})),
            end(1),
        ];
        let (root, out, gold_dir) = synthetic("putback", &events);
        write_runs(std::slice::from_ref(&out), &gold_dir, &HashMap::new(), None).unwrap();
        let rows = rows(&out.join("w/flow"));
        let counted: Vec<_> = rows
            .iter()
            .map(|r| r["counted"].as_bool().unwrap())
            .collect();
        assert_eq!(counted, [true, false]);
        assert_eq!(rows[1]["pop"], 2);
        assert_eq!(rows[1]["expand"]["dropped"], "budget");
        // The put-back pop's gold choice (mid) is written but not counted.
        assert_eq!(rows[1]["choice"]["gold"], true);
        let md = std::fs::read_to_string(out.join("w/flow/trajectories.md")).unwrap();
        assert!(md.contains("| pops (end.totals) | 1 |"), "{md}");
        assert!(md.contains("| rows | 2 |"), "{md}");
        assert!(
            md.contains("| rows of put-back pops (counted: false) | 1 |"),
            "{md}"
        );
        assert!(
            md.contains("| gold choices (counted rows with a label) | 0.0% (0/1) |"),
            "{md}"
        );
        assert!(
            md.contains("| first gold pop p50 (items with a gold choice) | n/a (0) |"),
            "{md}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The `counted` flags of the rows of a trace of one item.
    fn counted_flags(name: &str, events: &[Value]) -> Vec<bool> {
        let (root, out, gold_dir) = synthetic(name, events);
        write_runs(std::slice::from_ref(&out), &gold_dir, &HashMap::new(), None).unwrap();
        let flags = rows(&out.join("w/flow"))
            .iter()
            .map(|r| r["counted"].as_bool().unwrap())
            .collect();
        std::fs::remove_dir_all(&root).unwrap();
        flags
    }

    #[test]
    fn a_cut_pop_is_counted_by_the_end_totals_not_by_its_adopted_hops() {
        let cut = json!({"cut": true});
        let trace = |pops: Option<u64>| {
            let mut events = vec![
                json!({"ev": "start", "seeds": [SEED]}),
                pop(1, SEED, &[MID]),
                // Cut with something adopted: counted.
                expand(1, SEED, 1, cut.clone()),
                pop(2, MID, &[OTHER]),
                // Cut with nothing adopted.
                expand(2, MID, 0, cut.clone()),
            ];
            events.extend(pops.map(end));
            events
        };
        // The call stopped on the budget: the pop was put back and uncounted.
        assert_eq!(counted_flags("cut-back", &trace(Some(1))), [true, false]);
        // The call timed out: the engine keeps that pop in `end.totals.pops`.
        assert_eq!(counted_flags("cut-timeout", &trace(Some(2))), [true, true]);
        // No `end`: the `expand` event is all there is.
        assert_eq!(counted_flags("cut-no-end", &trace(None)), [true, false]);
    }

    #[test]
    fn a_scip_site_labels_a_symbol_no_card_located() {
        let events = [
            json!({"ev": "start", "seeds": [SEED]}),
            pop(1, SEED, &[MID, UNKNOWN]),
            expand(1, SEED, 1, json!({})),
            end(1),
        ];
        let (root, out, gold_dir) = synthetic("scip", &events);
        let candidate_golds = |scip: &HashMap<String, Site>| -> (Value, Value) {
            write_runs(std::slice::from_ref(&out), &gold_dir, scip, None).unwrap();
            let row = rows(&out.join("w/flow")).remove(0);
            (
                row["candidates"][1]["gold"].clone(),
                row["outcome"]["gold_candidate_skipped"].clone(),
            )
        };
        // No card, no index: unknown, and a gold candidate (mid) was skipped.
        assert_eq!(candidate_golds(&HashMap::new()), (Value::Null, json!(true)));
        // The index puts it at the intermediate's line: now it is gold.
        let site = Site {
            path: "src/a.rs".into(),
            line: 5,
        };
        let scip = HashMap::from([(UNKNOWN.to_owned(), site)]);
        assert_eq!(candidate_golds(&scip), (json!(true), json!(true)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unlabelled_candidate_makes_the_skipped_flag_unknown_unless_one_is_gold() {
        // No candidate is gold (other is a miss) and one is unlabelled.
        let events = [
            json!({"ev": "start", "seeds": [SEED]}),
            pop(1, SEED, &[OTHER, UNKNOWN]),
            expand(1, SEED, 1, json!({})),
            pop(2, SEED, &[OTHER]),
            expand(2, SEED, 1, json!({})),
            end(2),
        ];
        let (root, out, gold_dir) = synthetic("skipped-null", &events);
        write_runs(std::slice::from_ref(&out), &gold_dir, &HashMap::new(), None).unwrap();
        let rows = rows(&out.join("w/flow"));
        assert_eq!(rows[0]["outcome"]["gold_candidate_skipped"], Value::Null);
        // Every candidate labelled, none gold: false.
        assert_eq!(rows[1]["outcome"]["gold_candidate_skipped"], false);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_counted_rule_covers_dropped_cut_and_missing_expansions() {
        let expand = |adopted, cut, dropped: Option<&str>| Expand {
            offered: 3,
            adopted,
            cut,
            dropped: dropped.map(str::to_owned),
        };
        assert!(is_counted(None));
        assert!(is_counted(Some(&expand(2, false, None))));
        assert!(is_counted(Some(&expand(0, false, None))));
        // Cut with something adopted: the adopted hops stay charged.
        assert!(is_counted(Some(&expand(2, true, None))));
        assert!(!is_counted(Some(&expand(0, true, None))));
        assert!(!is_counted(Some(&expand(0, false, Some("budget")))));
    }

    #[test]
    fn a_row_stays_under_the_documented_size_bound() {
        let id = |tag: &str| format!("sym:rust:{tag}{}", "x".repeat(100 - 9 - tag.len()));
        assert_eq!(id("a").len(), 100);
        let candidate = |i: usize| RowCandidate {
            node: id(&format!("c{i}")),
            priority: 0.123456789012345,
            depth: 12,
            via: Some("graph.scip".into()),
            gold: Some(false),
        };
        let row = Row {
            set: "where-to",
            wave: "wave-0123456789",
            item: "where-to-registration-0123",
            trace: "0123456789abcdef0123456789abcdef.jsonl",
            event: 199,
            pop: 200,
            free: false,
            counted: true,
            state: RowState {
                depth: 12,
                frontier: 4000,
                discovered: 9000,
                tokens_left: 123456,
                calls_left: 40,
            },
            choice: Choice {
                node: id("p"),
                priority: 0.123456789012345,
                via: Some("graph.scip".into()),
                parent: Some(id("q")),
                gold: Some(false),
            },
            candidates: (0..8).map(candidate).collect(),
            expand: Some(Expand {
                offered: 100,
                adopted: 20,
                cut: false,
                dropped: None,
            }),
            outcome: Outcome {
                gold_candidate_skipped: Some(true),
                gold_before: 3,
                item: bench::headline(Set::WhereTo)
                    .iter()
                    .chain(&["f0.5"])
                    .map(|n| (n.to_string(), Some(0.123456789012345)))
                    .collect(),
            },
        };
        let bytes = serde_json::to_string(&row).unwrap().len();
        assert!(bytes <= ROW_BYTES_BOUND, "{bytes}");
        // The bound is tight enough to mean something.
        assert!(bytes * 2 > ROW_BYTES_BOUND, "{bytes}");
    }
}
