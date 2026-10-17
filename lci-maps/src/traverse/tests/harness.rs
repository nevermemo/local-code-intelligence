//! A seeded, model-based check of the engine. Each seed makes a small random
//! graph (one to three maps, cycles and duplicates allowed), a random budget,
//! price, scorer, score bound and handler, and runs it. Invariants: no budget
//! is exceeded, no state is expanded twice, a seed gives identical results and
//! events, a paused run resumed equals the straight run, the bound never fires
//! with fewer than `k` results, and every end names its stop reason.
//!
//! One seed in three ranks by `rrf` fusion instead of `boost`: the same
//! invariants hold, the score bound does not apply (it is off), and every
//! result has a positive fused score that a resume reproduces.
//!
//! One seed in four prices hops with a random fitted table instead of the
//! guess: the same invariants hold.
//!
//! Every seed runs twice, with pass-through off and on. On, the odd nodes are
//! symbols and the even ones files, so some pops are free: counted pops stay
//! within the pop cap, free pops stay within it too, and the token budget
//! holds; a resume still equals the straight run.
//!
//! 512 seeds run by default. To replay one:
//! `LCI_TRAVERSE_SEED=<n> cargo test -p lci-maps harness`.

use super::*;
use crate::{
    traverse::{handle::Snapshot, priors::HopPriors},
    walk::SplitMix,
};

const DEFAULT_SEEDS: u64 = 512;
const HUGE_TOKENS: u64 = u64::MAX / 2;
const HUGE_CALLS: u32 = u32::MAX / 2;

macro_rules! ensure {
    ($cond:expr, $($message:tt)+) => {
        if !$cond {
            return Err(format!($($message)+));
        }
    };
}

fn below(rng: &mut SplitMix, n: usize) -> usize {
    (rng.next_u64() % n.max(1) as u64) as usize
}

fn pick<T: Copy>(rng: &mut SplitMix, options: &[T]) -> T {
    options[below(rng, options.len())]
}

#[derive(Debug)]
struct MapSpec {
    id: MapId,
    edges: Vec<(String, String)>,
    conf: u8,
    witness: WitnessClass,
    dependent: bool,
    cost: (u64, u32),
}

/// Everything a seed decides.
#[derive(Debug)]
struct Case {
    seed: u64,
    maps: Vec<MapSpec>,
    seeds: Vec<(usize, bool)>,
    pops: u32,
    depth: u32,
    tokens: u64,
    calls: u32,
    price: (usize, usize),
    /// 0 rules, 1 unscored, 2 random.
    scorer: usize,
    top_k: Option<usize>,
    every: Option<u32>,
    pause_at: u32,
    fusion: Fusion,
    pass_through: bool,
    /// One seed in four prices hops with a random fitted table.
    priors: bool,
}

impl Case {
    /// The node `n<i>`: a file, or under pass-through a symbol when `i` is odd.
    fn name(pass_through: bool, i: usize) -> String {
        if pass_through && i % 2 == 1 {
            format!("sym/n{i}")
        } else {
            format!("n{i}")
        }
    }

    fn new(seed: u64, pass_through: bool) -> Self {
        let mut rng = SplitMix(seed);
        let nodes = 3 + below(&mut rng, 8);
        let maps = (0..=below(&mut rng, 3))
            .map(|i| MapSpec {
                id: ["m0", "m1", "m2"][i],
                edges: (0..below(&mut rng, 3 * nodes))
                    .map(|_| {
                        let (a, b) = (below(&mut rng, nodes), below(&mut rng, nodes));
                        (Self::name(pass_through, a), Self::name(pass_through, b))
                    })
                    .collect(),
                conf: 20 + below(&mut rng, 76) as u8,
                witness: pick(
                    &mut rng,
                    &[
                        WitnessClass::Compiler,
                        WitnessClass::Text,
                        WitnessClass::Vector,
                    ],
                ),
                dependent: i > 0 && below(&mut rng, 3) == 0,
                cost: (
                    pick(&mut rng, &[0, 0, 3, 10, 25]),
                    pick(&mut rng, &[0, 0, 0, 1, 2]),
                ),
            })
            .collect();
        let first = below(&mut rng, nodes);
        let mut seeds = vec![(first, below(&mut rng, 3) == 0)];
        if below(&mut rng, 2) == 0 {
            seeds.push(((first + 1 + below(&mut rng, nodes - 1)) % nodes, false));
        }
        let pops = pick(&mut rng, &[0, 1, 3, 6, 10, 20, 40]);
        Self {
            seed,
            maps,
            seeds,
            pops,
            depth: pick(&mut rng, &[1, 2, 3, 6, 20]),
            tokens: pick(
                &mut rng,
                &[HUGE_TOKENS, HUGE_TOKENS, HUGE_TOKENS, 0, 20, 60, 150, 400],
            ),
            calls: pick(&mut rng, &[HUGE_CALLS, HUGE_CALLS, HUGE_CALLS, 0, 1, 3, 6]),
            price: (
                pick(&mut rng, &[0, 1, 2, 8]),
                pick(&mut rng, &[1, 2, 4, 16]),
            ),
            scorer: below(&mut rng, 3),
            top_k: (below(&mut rng, 2) == 0).then(|| 1 + below(&mut rng, 4)),
            every: (below(&mut rng, 3) == 0).then(|| 1 + below(&mut rng, 4) as u32),
            pause_at: (1 + below(&mut rng, pops as usize) as u32).min(pops),
            // Not drawn from the generator, so the other choices of a seed
            // stay what they were.
            fusion: if seed % 3 == 2 {
                Fusion::Rrf
            } else {
                Fusion::Boost
            },
            pass_through,
            priors: seed % 4 == 1,
        }
    }

    /// The guess, or a table with a random prior for each map's calls.
    fn hop_priors(&self) -> HopPriors {
        if !self.priors {
            return HopPriors::default();
        }
        let mut rng = SplitMix(self.seed);
        let by_map: Vec<String> = ["m0", "m1", "m2"]
            .iter()
            .map(|id| {
                let prior = (1 + below(&mut rng, 500)) as f64 / 1000.0;
                format!(r#""{id}": {{"calls_or_refs": {prior}}}"#)
            })
            .collect();
        let text = format!(
            r#"{{"version": 1, "priors": {{"flow": {{{}}}}}}}"#,
            by_map.join(", ")
        );
        HopPriors::from_text(&text, "flow").expect("a valid table")
    }

    fn budget(&self, pops: u32) -> TraversalBudget {
        TraversalBudget {
            pops,
            max_depth: self.depth,
            deadline: Instant::now() + Duration::from_secs(60),
            est_tokens: self.tokens,
            external_calls: self.calls,
        }
    }
}

/// The calls the maps made (they keep to the allowance they are given).
type Runs = Vec<Arc<AtomicU32>>;

/// How often the engine asked each map to expand a node.
type Invoked = BTreeMap<MapId, u32>;

/// What a call may change from the case: the maps registered (the first
/// `maps`), the result limit of the score bound, and the budget (`roomy`: no
/// cap on tokens or external calls).
#[derive(Clone, Copy)]
struct Setup {
    maps: usize,
    top_k: Option<usize>,
    roomy: bool,
}

impl Setup {
    fn of(case: &Case) -> Self {
        Self {
            maps: case.maps.len(),
            top_k: case.top_k,
            roomy: false,
        }
    }
}

/// One call of the case: fresh, or a resume of `snapshot`.
async fn run(
    case: &Case,
    pops: u32,
    snapshot: Option<Snapshot>,
) -> (TraversalResult, u32, Invoked) {
    run_with(case, pops, snapshot, Setup::of(case)).await
}

async fn run_with(
    case: &Case,
    pops: u32,
    snapshot: Option<Snapshot>,
    setup: Setup,
) -> (TraversalResult, u32, Invoked) {
    let specs = &case.maps[..setup.maps];
    let runs: Runs = specs.iter().map(|_| Arc::default()).collect();
    let maps: Vec<GraphMap> = specs
        .iter()
        .zip(&runs)
        .map(|(spec, count)| {
            let mut map = GraphMap::new(spec.id, &[])
                .conf(spec.conf)
                .witness(spec.witness)
                .cost(spec.cost.0, spec.cost.1)
                .counted(count)
                .accepts(vec![NodeKind::File, NodeKind::Symbol]);
            map.edges = spec.edges.clone();
            map.dependent = spec.dependent;
            map
        })
        .collect();
    let invoked: Vec<(MapId, Arc<AtomicU32>)> = maps
        .iter()
        .map(|m| (m.descriptor.id, m.invoked.clone()))
        .collect();
    let reg = registry(maps);
    let view = view();
    let source = OnTheFly::new(&reg, &view);
    let mut budget = case.budget(pops);
    if setup.roomy {
        budget.est_tokens = HUGE_TOKENS;
        budget.external_calls = HUGE_CALLS;
    }
    let mut traversal = match snapshot {
        Some(snapshot) => Traversal::resumed(source, snapshot, budget),
        None => Traversal::new(
            source,
            case.seeds
                .iter()
                .map(|&(at, from_map)| Seed {
                    node: node(&Case::name(case.pass_through, at)),
                    map: from_map.then_some("seeds.x"),
                })
                .collect(),
            budget,
        ),
    }
    .price(HopPrice {
        gate: case.price.0,
        beam: case.price.1,
    })
    .fusion(case.fusion)
    .pass_through(case.pass_through)
    .priors(case.hop_priors());
    traversal = match case.scorer {
        1 => traversal.scorer(BfsScorer),
        2 => traversal.scorer(RandomScorer::new(case.seed)),
        _ => traversal,
    };
    if let Some(k) = setup.top_k {
        traversal = traversal.top_k(k, reg.independent_expanders());
    }
    if let Some(n) = case.every {
        traversal = traversal.handler(scripted(prune_last), every(n));
    }
    let result = traversal.run().await;
    let made = runs.iter().map(|count| count.load(Ordering::Relaxed)).sum();
    let invoked = invoked
        .into_iter()
        .map(|(id, count)| (id, count.load(Ordering::Relaxed)))
        .collect();
    (result, made, invoked)
}

fn results(result: &TraversalResult) -> usize {
    result
        .states
        .iter()
        .filter(|s| s.parent.is_some() && !result.cleared.contains(&s.node))
        .count()
}

/// The events of the walk itself: no call markers, no hashes of checkpoints
/// (they show the budget of their own call).
fn walk_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|e| {
            !["start", "end", "resume", "stop", "return"].contains(&e["ev"].as_str().unwrap())
        })
        .cloned()
        .map(|mut e| {
            e.as_object_mut().map(|o| o.remove("inputs_hash"));
            e
        })
        .collect()
}

/// What two runs that did the same work agree on.
fn same_walk(a: &TraversalResult, b: &TraversalResult) -> bool {
    let totals = |r: &TraversalResult| Totals {
        elapsed_ms: 0.0,
        ..r.totals.clone()
    };
    a.status == b.status
        && a.stop_reason == b.stop_reason
        && a.states == b.states
        && a.popped == b.popped
        && a.cleared == b.cleared
        && a.fusion == b.fusion
        && a.fused == b.fused
        && a.ranked() == b.ranked()
        && a.maps == b.maps
        && totals(a) == totals(b)
}

/// Each map's count of calls is the number of times the engine asked it,
/// whatever came of the expansion (cut, over the budget, retried), and a map
/// that has time has a count.
fn counts_match(result: &TraversalResult, invoked: &Invoked) -> Result<(), String> {
    let ids: BTreeSet<MapId> = result.maps.keys().chain(invoked.keys()).copied().collect();
    for id in ids {
        let counted = result.maps.get(id).map_or(0, |stat| stat.calls);
        let asked = invoked.get(id).copied().unwrap_or(0);
        ensure!(
            counted == asked,
            "{id}: {counted} calls counted, {asked} made"
        );
    }
    ensure!(
        result.map_ms.keys().all(|id| result.maps.contains_key(id)),
        "a map has time and no count: {:?} / {:?}",
        result.map_ms,
        result.maps
    );
    Ok(())
}

fn plus(a: &Invoked, b: &Invoked) -> Invoked {
    let mut sum = a.clone();
    for (id, n) in b {
        *sum.entry(id).or_default() += n;
    }
    sum
}

fn invariants(
    case: &Case,
    pops: u32,
    result: &TraversalResult,
    made: u32,
    invoked: &Invoked,
) -> Result<(), String> {
    counts_match(result, invoked)?;
    let totals = &result.totals;
    // Counted pops and free pops are each within the pop cap; without
    // pass-through there are no free pops.
    ensure!(
        totals.pops <= pops
            && totals.pass_through_pops <= pops
            && result.popped.len() == (totals.pops + totals.pass_through_pops) as usize,
        "pops {} and free pops {} over {pops}",
        totals.pops,
        totals.pass_through_pops
    );
    ensure!(
        case.pass_through || totals.pass_through_pops == 0,
        "free pops without pass-through"
    );
    ensure!(
        totals.tokens <= case.tokens,
        "tokens {} over {}",
        totals.tokens,
        case.tokens
    );
    ensure!(
        totals.external_calls <= case.calls,
        "external calls {} over {}",
        totals.external_calls,
        case.calls
    );
    ensure!(
        made == totals.external_calls,
        "{made} calls ran, {} charged",
        totals.external_calls
    );

    // Visit once: a node is one state, and a state is popped at most once.
    let mut nodes: Vec<_> = result.states.iter().map(|s| &s.node).collect();
    nodes.sort();
    nodes.dedup();
    ensure!(nodes.len() == result.states.len(), "a node has two states");
    let mut popped = result.popped.clone();
    popped.sort_unstable();
    popped.dedup();
    ensure!(
        popped.len() == result.popped.len(),
        "a state was popped twice: {:?}",
        result.popped
    );
    let expanded: Vec<&str> = result
        .events
        .iter()
        .filter(|e| e["ev"] == "expand")
        .map(|e| e["node"].as_str().unwrap())
        .collect();
    let mut once = expanded.clone();
    once.sort_unstable();
    once.dedup();
    ensure!(
        once.len() == expanded.len(),
        "a node was expanded twice: {expanded:?}"
    );
    ensure!(
        result
            .popped
            .iter()
            .all(|&i| result.states[i].depth < case.depth),
        "a state at max_depth was expanded"
    );

    // Every end names its reason, and the reason fits the status.
    let end = result.events.last().ok_or("no events")?;
    ensure!(
        end["ev"] == "end" && end["stop_reason"] == result.stop_reason.as_str(),
        "end event {end}"
    );
    use stop::StopReason::{Bound, Budget, Complete, Deadline, Pops};
    ensure!(
        result.status != TraversalStatus::Stopped
            && result.checkpoint.is_some() == result.snapshot.is_some(),
        "status {:?}",
        result.status
    );
    let fits = match result.status {
        TraversalStatus::Complete => {
            matches!(result.stop_reason, Complete | Bound) && result.snapshot.is_none()
        }
        TraversalStatus::BudgetExhausted => {
            matches!(result.stop_reason, Pops | Budget | Deadline) && result.snapshot.is_some()
        }
        _ => false,
    };
    ensure!(
        fits,
        "status {:?} with reason {:?}",
        result.status,
        result.stop_reason
    );
    ensure!(
        result.stop_reason != Pops || totals.pops >= pops || totals.pass_through_pops >= pops,
        "pops reason at {} (free {}) of {pops}",
        totals.pops,
        totals.pass_through_pops
    );

    // The bound holds only with a limit, enough results, and a frontier that
    // cannot beat the k-th.
    let bounds: Vec<&Value> = result
        .events
        .iter()
        .filter(|e| e["ev"] == "bound")
        .collect();
    ensure!(
        bounds.len() == usize::from(result.stop_reason == Bound),
        "bound events {bounds:?}"
    );
    if case.fusion == Fusion::Rrf {
        // A fused score has no bound on a frontier state: the bound is off.
        ensure!(bounds.is_empty(), "a bound under rrf: {bounds:?}");
        let ranked = result.ranked();
        ensure!(ranked.len() == results(result), "ranked() misses results");
        for &i in &ranked {
            let score = result.ranked_potential(&result.states[i]);
            ensure!(score.is_finite() && score > 0.0, "fused score {score}");
        }
        ensure!(
            ranked.windows(2).all(|w| {
                let (a, b) = (&result.states[w[0]], &result.states[w[1]]);
                let (x, y) = (result.ranked_potential(a), result.ranked_potential(b));
                x > y || (x == y && a.node < b.node)
            }),
            "results out of order"
        );
    } else if result.fusion != Fusion::Boost || !result.fused.is_empty() {
        return Err("a boost run holds fused scores".into());
    }
    if let Some(bound) = bounds.first() {
        let k = case.top_k.ok_or("a bound without a limit")?;
        ensure!(
            results(result) >= k,
            "bound with {} of {k} results",
            results(result)
        );
        let ceiling =
            bound["frontier_best"].as_f64().unwrap() * bound["max_factor"].as_f64().unwrap();
        let beaten = ceiling > bound["kth"].as_f64().unwrap();
        ensure!(!beaten, "{bound}");
    }
    Ok(())
}

async fn check(case: &Case, tally: &mut BTreeMap<&'static str, usize>) -> Result<(), String> {
    let (straight, made, seen) = run(case, case.pops, None).await;
    invariants(case, case.pops, &straight, made, &seen)?;

    // The same seed gives the same walk, event for event.
    let (again, ..) = run(case, case.pops, None).await;
    ensure!(
        same_walk(&straight, &again) && straight.events == again.events,
        "two runs differ"
    );

    // A call cut by the budget, resumed with room, is counted as often as it
    // was made.
    if let Some(snapshot) = straight
        .snapshot
        .clone()
        .filter(|_| straight.stop_reason == stop::StopReason::Budget)
    {
        let roomy = Setup {
            roomy: true,
            ..Setup::of(case)
        };
        let (more, _, seen_more) = run_with(case, case.pops, Some(snapshot), roomy).await;
        counts_match(&more, &plus(&seen, &seen_more))?;
        *tally.entry("retried").or_default() += 1;
    }

    // A walk paused and resumed through its JSON handle equals the straight
    // one (the random scorer keeps its state outside the handle).
    if case.scorer != 2 {
        let (first, made, seen_first) = run(case, case.pause_at, None).await;
        invariants(case, case.pause_at, &first, made, &seen_first)?;
        let mut events = walk_events(&first.events);
        let paused = first
            .snapshot
            .clone()
            .filter(|_| first.stop_reason == stop::StopReason::Pops && case.pause_at < case.pops);
        let last = if let Some(snapshot) = paused {
            let back = Snapshot::from_json(&snapshot.to_json())?;
            ensure!(back == snapshot, "the handle does not read back");
            *tally.entry("resumed").or_default() += 1;

            // Resumed with fewer maps, the score bound still counts the
            // evidence the states kept from the maps that left.
            if case.maps.len() > 1 {
                let fewer = Setup {
                    maps: case.maps.len() - 1,
                    top_k: Some(case.top_k.unwrap_or(1)),
                    roomy: false,
                };
                let (rest, ..) = run_with(case, case.pops, Some(back.clone()), fewer).await;
                if let Some(bound) = rest.events.iter().find(|e| e["ev"] == "bound") {
                    *tally.entry("bound_fewer_maps").or_default() += 1;
                    let kept = rest.states.iter().map(State::n_maps).max().unwrap_or(0);
                    ensure!(
                        bound["maps"].as_u64().unwrap() as usize >= kept,
                        "a bound over {} maps, states hold {kept}: {bound}",
                        bound["maps"]
                    );
                }
            }

            let (second, made, seen_second) = run(case, case.pops, Some(back)).await;
            invariants(
                case,
                case.pops,
                &second,
                first.totals.external_calls + made,
                &plus(&seen_first, &seen_second),
            )?;
            events.extend(walk_events(&second.events));
            second
        } else {
            first
        };
        ensure!(
            same_walk(&straight, &last),
            "resume differs from the straight run"
        );
        ensure!(
            walk_events(&straight.events) == events,
            "resume events differ from the straight run"
        );
    }

    let mut count = |what: &'static str, hit: bool| {
        if hit {
            *tally.entry(what).or_default() += 1;
        }
    };
    let has = |ev: &str| straight.events.iter().any(|e| e["ev"] == ev);
    count("bound", straight.stop_reason == stop::StopReason::Bound);
    count("pops", straight.stop_reason == stop::StopReason::Pops);
    count("budget", straight.stop_reason == stop::StopReason::Budget);
    count(
        "complete",
        straight.stop_reason == stop::StopReason::Complete,
    );
    count("free", straight.totals.pass_through_pops > 0);
    count("relax", has("relax"));
    count("alt_path", has("alt_path"));
    count("steer", has("steer"));
    Ok(())
}

#[tokio::test]
async fn harness_random_walks_keep_the_engine_invariants() {
    let replay = std::env::var("LCI_TRAVERSE_SEED")
        .ok()
        .map(|v| v.parse::<u64>().expect("LCI_TRAVERSE_SEED is a number"));
    let seeds: Vec<u64> = replay.map_or_else(|| (0..DEFAULT_SEEDS).collect(), |seed| vec![seed]);
    let mut tally = BTreeMap::new();
    for seed in seeds {
        for pass_through in [false, true] {
            let case = Case::new(seed, pass_through);
            if let Err(why) = check(&case, &mut tally).await {
                panic!(
                    "{why}\n  replay: LCI_TRAVERSE_SEED={seed} cargo test -p lci-maps harness\n  {case:?}"
                );
            }
        }
    }
    if replay.is_none() {
        // The seeds reach the cases the invariants are about.
        for what in [
            "bound",
            "pops",
            "budget",
            "complete",
            "free",
            "relax",
            "alt_path",
            "steer",
            "resumed",
            "retried",
            "bound_fewer_maps",
        ] {
            assert!(
                tally.get(what).copied().unwrap_or(0) > 0,
                "no seed reached `{what}`: {tally:?}"
            );
        }
    }
}
