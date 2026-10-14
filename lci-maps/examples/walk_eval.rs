//! The evaluation driver of the scored-walk harness (slice M0.4 of the
//! multimap spike v2; pre-registration in
//! `docs/spike/multimap/v2/m0.4-prereg.md`).
//!
//! Loads a SCIP index into a [`ScipGraph`], reads the `flow` gold set and,
//! for every path item, measures each arm per hop (the next gold node among
//! the out-edges of the current one) and as a beam walk under a pop budget.
//! Only the question text feeds the query; the gold path is read only to
//! seed the walk at the entry and to score it.
//!
//! ```text
//! cargo run --release -p lci-maps --example walk_eval -- \
//!     --scip INDEX.scip --gold flow.json --out result.json \
//!     [--root TREE] [--gate 8] [--seed 1] [--pops 200] [--max-depth 6] \
//!     [--only ID,ID] [--limit N]
//! ```
//!
//! `--root` is a checkout of the indexed commit; it enables the
//! enclosing-range repair of macro-expanded impl members. `--seed` is the
//! first of five random-arm seeds.

use lci_maps::{
    scip_graph::{DirSource, GraphEdgeKind, ScipGraph},
    walk::{
        BfsScorer, NameIndex, Query, RandomScorer, RulesScorer, ScoreCtx, Scorer, SplitMix, W_BASE,
        W_INDEGREE_IDF, W_OVERLAP, WalkConfig, candidates, rank_order, walk, wilson,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

const BEAMS: [usize; 2] = [1, 3];
const RANDOM_SEEDS: u64 = 5;
const SHORTCUT_FLAG: &str = "name-matching-shortcut";

struct Args {
    scip: PathBuf,
    gold: PathBuf,
    out: PathBuf,
    root: Option<PathBuf>,
    gate: usize,
    seed: u64,
    pops: usize,
    max_depth: usize,
    only: Option<Vec<String>>,
    limit: Option<usize>,
}

fn parse_args() -> Args {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        map.insert(flag, value);
    }
    let path = |k: &str| PathBuf::from(map.get(k).unwrap_or_else(|| panic!("{k} is required")));
    let num = |k: &str, d: u64| {
        map.get(k).map_or(d, |v| {
            v.parse().unwrap_or_else(|_| panic!("{k} is a number"))
        })
    };
    Args {
        scip: path("--scip"),
        gold: path("--gold"),
        out: path("--out"),
        root: map.get("--root").map(PathBuf::from),
        gate: num("--gate", 8) as usize,
        seed: num("--seed", 1),
        pops: num("--pops", 200) as usize,
        max_depth: num("--max-depth", 6) as usize,
        only: map
            .get("--only")
            .map(|v| v.split(',').map(str::to_string).collect()),
        limit: map
            .get("--limit")
            .map(|v| v.parse().expect("--limit is a number")),
    }
}

struct Item {
    id: String,
    stratum: String,
    question: String,
    cluster: String,
    flags: Vec<String>,
    headline: bool,
    /// Graph indices of entry, intermediates, target; None if any is absent.
    path: Option<Vec<usize>>,
    alternates: Vec<Vec<usize>>,
    /// Hops (from, to) with both nodes in the graph; None when one is not.
    hops: Vec<Option<(usize, usize)>>,
    edge_present: Vec<bool>,
}

impl Item {
    fn shortcut(&self) -> bool {
        self.flags.iter().any(|f| f == SHORTCUT_FLAG)
    }
    fn len(&self) -> usize {
        self.hops.len()
    }
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn load_items(gold: &Value, graph: &ScipGraph, args: &Args) -> (Vec<Item>, usize) {
    let mut items = Vec::new();
    let mut skipped = 0;
    for raw in gold.as_array().expect("gold is an array") {
        let hops = raw["gold"]["hops"].as_array().cloned().unwrap_or_default();
        if hops.is_empty() {
            skipped += 1;
            continue;
        }
        let id = str_of(&raw["id"]);
        if args.only.as_ref().is_some_and(|only| !only.contains(&id)) {
            continue;
        }
        let mut symbols = vec![str_of(&raw["gold"]["entry"]["symbol"])];
        for node in raw["gold"]["intermediates"]
            .as_array()
            .into_iter()
            .flatten()
        {
            symbols.push(str_of(&node["symbol"]));
        }
        symbols.push(str_of(&raw["gold"]["target"]["symbol"]));
        let indices: Vec<Option<usize>> = symbols.iter().map(|s| graph.node_index(s)).collect();
        let path = indices.iter().copied().collect::<Option<Vec<usize>>>();
        let mut alternates = Vec::new();
        for alt in raw["accept"]["alternates"].as_array().into_iter().flatten() {
            let hops = alt["hops"].as_array().cloned().unwrap_or_default();
            let mut syms = hops
                .first()
                .map(|h| vec![str_of(&h["caller_symbol"])])
                .unwrap_or_default();
            syms.extend(hops.iter().map(|h| str_of(&h["callee_symbol"])));
            if let Some(nodes) = syms
                .iter()
                .map(|s| graph.node_index(s))
                .collect::<Option<Vec<usize>>>()
            {
                alternates.push(nodes);
            }
        }
        let hop_pairs: Vec<Option<(usize, usize)>> =
            indices.windows(2).map(|w| Some((w[0]?, w[1]?))).collect();
        let edge_present = hop_pairs
            .iter()
            .map(|h| h.is_some_and(|(a, b)| graph.has_edge(a, b, GraphEdgeKind::CallsOrRefs)))
            .collect();
        let analysis = &raw["accept"]["analysis"];
        items.push(Item {
            id,
            stratum: str_of(&raw["stratum"]),
            question: str_of(&raw["question"]),
            cluster: str_of(&analysis["cluster"]["prefix_cluster"]),
            flags: analysis["flags"]
                .as_array()
                .into_iter()
                .flatten()
                .map(str_of)
                .collect(),
            headline: analysis["headline"].as_bool().unwrap_or(false),
            path,
            alternates,
            hops: hop_pairs,
            edge_present,
        });
        if args.limit.is_some_and(|n| items.len() >= n) {
            break;
        }
    }
    (items, skipped)
}

#[derive(Clone, Copy)]
enum Arm {
    Rules,
    RulesNoName,
    Bfs,
    Random(u64),
}

impl Arm {
    fn label(self) -> String {
        match self {
            Self::Rules => "rules".into(),
            Self::RulesNoName => "rules-noname".into(),
            Self::Bfs => "bfs".into(),
            Self::Random(seed) => format!("random-{seed}"),
        }
    }

    /// A fresh scorer; `purpose` keeps the random streams of the per-hop
    /// ranking and of each walk independent of one another.
    fn scorer(self, item: usize, purpose: u64) -> Box<dyn Scorer> {
        match self {
            Self::Rules => Box::new(RulesScorer { use_overlap: true }),
            Self::RulesNoName => Box::new(RulesScorer { use_overlap: false }),
            Self::Bfs => Box::new(BfsScorer),
            Self::Random(seed) => {
                let mix = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    ^ (item as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
                    ^ purpose.wrapping_mul(0x1656_67B1_9E37_79F9);
                let mut rng = SplitMix(mix);
                rng.next_u64();
                Box::new(RandomScorer { rng })
            }
        }
    }
}

#[derive(Default, Clone)]
struct ItemRun {
    /// Rank (1-based) of the gold next node among all callees, per hop.
    ranks: Vec<Option<usize>>,
    /// Out-degree (distinct callees) at the start of each hop.
    degrees: Vec<usize>,
    found: [bool; 2],
    reached: [bool; 2],
    pops: [usize; 2],
}

struct Setup<'a> {
    graph: &'a ScipGraph,
    names: &'a NameIndex,
    gate: usize,
    pops: usize,
    max_depth: usize,
}

fn run_item(setup: &Setup, arm: Arm, index: usize, item: &Item) -> ItemRun {
    let query = Query::new(&item.question);
    let ctx = ScoreCtx {
        graph: setup.graph,
        names: setup.names,
        query: &query,
    };
    let mut run = ItemRun::default();
    let mut scorer = arm.scorer(index, 0);
    for hop in &item.hops {
        match hop {
            Some((from, to)) => {
                let cands = candidates(setup.graph, *from);
                run.degrees.push(cands.len());
                let order = rank_order(scorer.as_mut(), &ctx, *from, &cands);
                run.ranks.push(
                    order
                        .iter()
                        .position(|&i| cands[i].target == *to)
                        .map(|p| p + 1),
                );
            }
            None => {
                run.degrees.push(0);
                run.ranks.push(None);
            }
        }
    }
    if let Some(path) = &item.path {
        for (b, &beam) in BEAMS.iter().enumerate() {
            let config = WalkConfig {
                gate: setup.gate,
                beam,
                pop_budget: setup.pops,
                max_depth: setup.max_depth,
            };
            let mut scorer = arm.scorer(index, 1 + b as u64);
            let result = walk(scorer.as_mut(), &ctx, path[0], &config);
            run.found[b] =
                result.found_path(path) || item.alternates.iter().any(|a| result.found_path(a));
            run.reached[b] = result.reached(*path.last().expect("a path has a target"));
            run.pops[b] = result.pops;
        }
    }
    run
}

/// Hop outcome under the walk's policy at beam `b`: followed whole below the
/// gate, otherwise the gold callee must rank within the beam. An unscored
/// arm follows every callee.
fn policy_ok(run: &ItemRun, hop: usize, beam: usize, gate: usize, unscored: bool) -> bool {
    match run.ranks[hop] {
        None => false,
        Some(rank) => unscored || run.degrees[hop] <= gate || rank <= beam,
    }
}

fn rate(k: usize, n: usize) -> Value {
    let (lo, hi) = wilson(k, n);
    json!({"k": k, "n": n, "rate": if n > 0 { k as f64 / n as f64 } else { f64::NAN }, "lo": lo, "hi": hi})
}

type Group<'a> = (&'a str, fn(&Item) -> bool);

const GROUPS: [Group; 6] = [
    ("all", |_| true),
    ("headline", |i| i.headline),
    ("non_headline", |i| !i.headline),
    ("shortcut", Item::shortcut),
    ("no_shortcut", |i| !i.shortcut()),
    ("headline_no_shortcut", |i| i.headline && !i.shortcut()),
];

#[derive(Default)]
struct Counts {
    items: usize,
    hops: usize,
    hit1: usize,
    hit3: usize,
    policy: [usize; 2],
    all_hit1: usize,
    all_hit3: usize,
    ceiling: [usize; 2],
    found: [usize; 2],
    reached: [usize; 2],
    pops: [usize; 2],
    missing_edges: usize,
}

fn count<'a>(
    pairs: impl Iterator<Item = (&'a Item, &'a ItemRun)>,
    gate: usize,
    unscored: bool,
) -> Counts {
    let mut c = Counts::default();
    for (_, run) in pairs {
        c.items += 1;
        c.hops += run.ranks.len();
        let mut all1 = true;
        let mut all3 = true;
        let mut ceiling = [true; 2];
        for h in 0..run.ranks.len() {
            let rank = run.ranks[h];
            c.missing_edges += usize::from(rank.is_none());
            c.hit1 += usize::from(rank == Some(1));
            c.hit3 += usize::from(rank.is_some_and(|r| r <= 3));
            all1 &= rank == Some(1);
            all3 &= rank.is_some_and(|r| r <= 3);
            for (b, &beam) in BEAMS.iter().enumerate() {
                let ok = policy_ok(run, h, beam, gate, unscored);
                c.policy[b] += usize::from(ok);
                ceiling[b] &= ok;
            }
        }
        c.all_hit1 += usize::from(all1);
        c.all_hit3 += usize::from(all3);
        for (b, ok) in ceiling.into_iter().enumerate() {
            c.ceiling[b] += usize::from(ok);
            c.found[b] += usize::from(run.found[b]);
            c.reached[b] += usize::from(run.reached[b]);
            c.pops[b] += run.pops[b];
        }
    }
    c
}

fn counts_json(c: &Counts) -> Value {
    let mean_pops = |b: usize| {
        if c.items > 0 {
            c.pops[b] as f64 / c.items as f64
        } else {
            0.0
        }
    };
    json!({
        "items": c.items,
        "hops": c.hops,
        "gold_edge_missing_hops": c.missing_edges,
        "hop_hit1": rate(c.hit1, c.hops),
        "hop_hit3": rate(c.hit3, c.hops),
        "item_all_hops_hit1": rate(c.all_hit1, c.items),
        "item_all_hops_hit3": rate(c.all_hit3, c.items),
        "policy_hop_ok_beam1": rate(c.policy[0], c.hops),
        "policy_hop_ok_beam3": rate(c.policy[1], c.hops),
        "ceiling_all_hops_ok_beam1": rate(c.ceiling[0], c.items),
        "ceiling_all_hops_ok_beam3": rate(c.ceiling[1], c.items),
        "path_recall_beam1": rate(c.found[0], c.items),
        "path_recall_beam3": rate(c.found[1], c.items),
        "target_reached_beam1": rate(c.reached[0], c.items),
        "target_reached_beam3": rate(c.reached[1], c.items),
        "mean_pops_beam1": mean_pops(0),
        "mean_pops_beam3": mean_pops(1),
    })
}

/// The headline numbers of one arm instance, keyed `group/metric`, for the
/// random-seed spread.
fn headline_rates(c: &Counts, group: &str, out: &mut BTreeMap<String, f64>) {
    let r = |k: usize, n: usize| if n > 0 { k as f64 / n as f64 } else { f64::NAN };
    out.insert(format!("{group}/hop_hit1"), r(c.hit1, c.hops));
    out.insert(format!("{group}/hop_hit3"), r(c.hit3, c.hops));
    out.insert(format!("{group}/path_recall_beam1"), r(c.found[0], c.items));
    out.insert(format!("{group}/path_recall_beam3"), r(c.found[1], c.items));
}

fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn degree_summary(mut degrees: Vec<usize>, gates: &[usize]) -> Value {
    degrees.sort_unstable();
    let n = degrees.len();
    let mean = if n > 0 {
        degrees.iter().sum::<usize>() as f64 / n as f64
    } else {
        0.0
    };
    let above: BTreeMap<String, Value> = gates
        .iter()
        .map(|&g| {
            let k = degrees.iter().filter(|&&d| d > g).count();
            (format!("gt_{g}"), rate(k, n))
        })
        .collect();
    json!({
        "n": n, "mean": mean,
        "p10": percentile(&degrees, 10.0), "p25": percentile(&degrees, 25.0),
        "p50": percentile(&degrees, 50.0), "p75": percentile(&degrees, 75.0),
        "p90": percentile(&degrees, 90.0), "p95": percentile(&degrees, 95.0),
        "p99": percentile(&degrees, 99.0), "max": degrees.last().copied().unwrap_or(0),
        "share_above_gate": above,
    })
}

fn arm_json(
    arm: &str,
    unscored: bool,
    items: &[Item],
    runs: &[ItemRun],
    setup: &Setup,
) -> (Value, BTreeMap<String, f64>) {
    let mut groups = serde_json::Map::new();
    let mut headline = BTreeMap::new();
    for (name, member) in GROUPS {
        let pairs = || items.iter().zip(runs).filter(|(i, _)| member(i));
        let c = count(pairs(), setup.gate, unscored);
        headline_rates(&c, name, &mut headline);
        let mut g = counts_json(&c);
        // The compounding curve: per-hop success p to the power k against
        // the observed path recall, by path length k (edges).
        let lengths: BTreeSet<usize> = pairs().map(|(i, _)| i.len()).collect();
        let by_length: Vec<Value> = lengths
            .into_iter()
            .map(|k| {
                let ck = count(pairs().filter(|(i, _)| i.len() == k), setup.gate, unscored);
                let p = |b: usize| if ck.hops > 0 { ck.policy[b] as f64 / ck.hops as f64 } else { f64::NAN };
                let obs = |b: usize| if ck.items > 0 { ck.found[b] as f64 / ck.items as f64 } else { f64::NAN };
                let ceil = |b: usize| if ck.items > 0 { ck.ceiling[b] as f64 / ck.items as f64 } else { f64::NAN };
                json!({
                    "k": k, "items": ck.items,
                    "beam1": {"p": p(0), "p_pow_k": p(0).powi(k as i32), "ceiling": ceil(0), "observed": obs(0)},
                    "beam3": {"p": p(1), "p_pow_k": p(1).powi(k as i32), "ceiling": ceil(1), "observed": obs(1)},
                })
            })
            .collect();
        g["by_length"] = json!(by_length);
        groups.insert(name.to_string(), g);
    }
    let clusters: BTreeSet<&str> = items.iter().map(|i| i.cluster.as_str()).collect();
    let by_cluster: BTreeMap<String, Value> = clusters
        .into_iter()
        .map(|cl| {
            let c = count(
                items.iter().zip(runs).filter(|(i, _)| i.cluster == cl),
                setup.gate,
                unscored,
            );
            (
                cl.to_string(),
                json!({
                    "items": c.items, "hops": c.hops,
                    "hop_hit1": rate(c.hit1, c.hops), "hop_hit3": rate(c.hit3, c.hops),
                    "path_recall_beam1": rate(c.found[0], c.items),
                    "path_recall_beam3": rate(c.found[1], c.items),
                }),
            )
        })
        .collect();
    (
        json!({"arm": arm, "unscored": unscored, "groups": groups, "by_cluster": by_cluster}),
        headline,
    )
}

fn spread(values: &[f64]) -> Value {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = if values.len() > 1 {
        values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    json!({
        "mean": mean, "sd": var.sqrt(),
        "min": values.iter().copied().fold(f64::INFINITY, f64::min),
        "max": values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    })
}

fn main() {
    let args = parse_args();
    let source = args.root.clone().map(DirSource);
    let graph = ScipGraph::load(
        &args.scip,
        source
            .as_ref()
            .map(|s| s as &dyn lci_maps::scip_graph::SourceText),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let gold: Value =
        serde_json::from_str(&std::fs::read_to_string(&args.gold).expect("read gold"))
            .expect("parse gold");
    let (items, skipped) = load_items(&gold, &graph, &args);
    let names = NameIndex::new(&graph);
    let setup = Setup {
        graph: &graph,
        names: &names,
        gate: args.gate,
        pops: args.pops,
        max_depth: args.max_depth,
    };

    let mut arms = vec![Arm::Rules, Arm::RulesNoName, Arm::Bfs];
    arms.extend((0..RANDOM_SEEDS).map(|s| Arm::Random(args.seed + s)));
    let mut arm_values = Vec::new();
    let mut per_item_arms: Vec<(String, Vec<ItemRun>)> = Vec::new();
    let mut random_rates: Vec<BTreeMap<String, f64>> = Vec::new();
    let mut table = Vec::new();
    for arm in &arms {
        let runs: Vec<ItemRun> = items
            .iter()
            .enumerate()
            .map(|(i, item)| run_item(&setup, *arm, i, item))
            .collect();
        let unscored = matches!(arm, Arm::Bfs);
        let (value, headline) = arm_json(&arm.label(), unscored, &items, &runs, &setup);
        table.push(format!(
            "{:<14} hit@1 {:.3} hit@3 {:.3} recall b1 {:.3} b3 {:.3}",
            arm.label(),
            headline["all/hop_hit1"],
            headline["all/hop_hit3"],
            headline["all/path_recall_beam1"],
            headline["all/path_recall_beam3"],
        ));
        if matches!(arm, Arm::Random(_)) {
            random_rates.push(headline);
        }
        arm_values.push(value);
        per_item_arms.push((arm.label(), runs));
    }
    let random_summary: BTreeMap<String, Value> = random_rates
        .first()
        .map(|first| {
            first
                .keys()
                .map(|k| {
                    let values: Vec<f64> = random_rates.iter().map(|r| r[k]).collect();
                    (k.clone(), spread(&values))
                })
                .collect()
        })
        .unwrap_or_default();

    // Out-degree at the gold hops' start nodes, from the arm-independent
    // candidate counts of the first arm.
    let degrees: Vec<usize> = per_item_arms
        .first()
        .map(|(_, runs)| {
            items
                .iter()
                .zip(runs)
                .flat_map(|(item, run)| {
                    item.hops
                        .iter()
                        .zip(&run.degrees)
                        .filter(|(h, _)| h.is_some())
                        .map(|(_, &d)| d)
                })
                .collect()
        })
        .unwrap_or_default();
    let graph_degrees: Vec<usize> = (0..graph.nodes.len())
        .map(|n| candidates(&graph, n).len())
        .filter(|&d| d > 0)
        .collect();
    let gates = [0, 2, 4, 6, 8, 12, 16, 24, 32];

    let hops_total: usize = items.iter().map(Item::len).sum();
    let hops_present: usize = items
        .iter()
        .map(|i| i.edge_present.iter().filter(|&&p| p).count())
        .sum();
    let per_item: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(n, item)| {
            let arms_json: BTreeMap<String, Value> = per_item_arms
                .iter()
                .map(|(label, runs)| {
                    let r = &runs[n];
                    (
                        label.clone(),
                        json!({"ranks": r.ranks, "found_beam1": r.found[0], "found_beam3": r.found[1],
                               "reached_beam1": r.reached[0], "reached_beam3": r.reached[1],
                               "pops": r.pops}),
                    )
                })
                .collect();
            json!({
                "id": item.id, "stratum": item.stratum, "cluster": item.cluster,
                "length": item.len(), "flags": item.flags, "headline": item.headline,
                "path_in_graph": item.path.is_some(), "edge_present": item.edge_present,
                "degrees": per_item_arms.first().map(|(_, r)| r[n].degrees.clone()),
                "query_tokens": Query::new(&item.question).tokens,
                "arms": arms_json,
            })
        })
        .collect();

    let result = json!({
        "config": {
            "scip": args.scip.file_name().map(|n| n.to_string_lossy().to_string()),
            "repair_source": args.root.is_some(),
            "gate": args.gate, "pop_budget": args.pops, "max_depth": args.max_depth,
            "beams": BEAMS, "random_seeds": (0..RANDOM_SEEDS).map(|s| args.seed + s).collect::<Vec<_>>(),
            "rules_weights": {"base": W_BASE, "indegree_idf": W_INDEGREE_IDF, "overlap": W_OVERLAP},
            "only": args.only, "limit": args.limit,
        },
        "graph": {
            "nodes": graph.nodes.len(),
            "edges_calls_or_refs": graph.edge_count(GraphEdgeKind::CallsOrRefs),
            "edges_contains": graph.edge_count(GraphEdgeKind::Contains),
            "report": {
                "documents": graph.report.documents,
                "definitions": graph.report.definitions,
                "function_definitions": graph.report.function_definitions,
                "repaired": graph.report.repaired,
                "repair_skipped": graph.report.repair_skipped,
                "attributed_sites": graph.report.attributed_sites,
                "unattributed_sites": graph.report.unattributed_sites,
            },
            "out_degree_all_nodes_with_callees": degree_summary(graph_degrees, &gates),
        },
        "gold": {
            "path_items": items.len(),
            "items_without_path_skipped": skipped,
            "items_with_all_nodes_in_graph": items.iter().filter(|i| i.path.is_some()).count(),
            "hops": hops_total,
            "hops_with_calls_or_refs_edge": hops_present,
            "items_with_all_edges": items.iter().filter(|i| i.edge_present.iter().all(|&p| p)).count(),
            "out_degree_at_gold_hop_starts": degree_summary(degrees, &gates),
        },
        "arms": arm_values,
        "random_summary": random_summary,
        "items": per_item,
    });
    std::fs::write(
        &args.out,
        serde_json::to_string_pretty(&result).expect("serialize"),
    )
    .expect("write output");

    println!(
        "graph: {} nodes, {} calls_or_refs, {} contains (repaired {}, repair skipped {})",
        graph.nodes.len(),
        graph.edge_count(GraphEdgeKind::CallsOrRefs),
        graph.edge_count(GraphEdgeKind::Contains),
        graph.report.repaired,
        graph.report.repair_skipped,
    );
    println!(
        "gold: {} path items ({} without path skipped), {} hops, {} with a graph edge",
        items.len(),
        skipped,
        hops_total,
        hops_present
    );
    println!(
        "gold-hop out-degree: {}",
        result["gold"]["out_degree_at_gold_hop_starts"]
    );
    for line in table {
        println!("{line}");
    }
}
