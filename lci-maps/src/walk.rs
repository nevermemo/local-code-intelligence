//! The scoring primitives for walks over a [`ScipGraph`]: the question
//! tokenizer and name index, the hop values and a reference best-first walk
//! (History: slice M0.4 of the multimap spike v2; pre-registration in
//! `docs/spike/multimap/v2/m0.4-prereg.md`).
//!
//! [`hop`] is the single step. A node with at most `gate` distinct callees
//! is followed whole: every callee gets the value 1 and no scorer runs. Above
//! the gate a [`Scorer`] orders the callees, the best `beam` are kept, and
//! the callee at rank `r` (0-based) gets the value `1 / (1 + r)`. An
//! unscored arm (BFS) follows every callee in source order whatever the gate
//! and the beam.
//!
//! [`walk`] is best-first search over path states. A state's priority is the
//! product of the hop values along its path; ties pop in discovery order.
//! One pop expands one state. The walk stops at `pop_budget` pops, when the
//! frontier is empty, or at `max_depth`; a state at `max_depth` is recorded
//! but not expanded.
//!
//! Only the question text feeds [`Query`]; the walk never sees a target.

use crate::scip_graph::{GraphEdgeKind, ScipGraph};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
};

/// Words that carry no signal in a question: English function words and the
/// boilerplate of the gold set's question templates.
pub const STOPWORDS: &[&str] = &[
    // English function words.
    "a",
    "after",
    "also",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "been",
    "before",
    "by",
    "can",
    "do",
    "does",
    "each",
    "every",
    "for",
    "from",
    "has",
    "have",
    "how",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "of",
    "on",
    "once",
    "one",
    "only",
    "or",
    "per",
    "so",
    "than",
    "that",
    "the",
    "then",
    "there",
    "this",
    "through",
    "to",
    "was",
    "were",
    "what",
    "when",
    "which",
    "while",
    "whose",
    "with",
    // Boilerplate of the gold set's question templates.
    "between",
    "binary",
    "call",
    "called",
    "calls",
    "chain",
    "concrete",
    "containing",
    "contains",
    "down",
    "end",
    "ends",
    "function",
    "functions",
    "implementing",
    "intermediate",
    "invoking",
    "lead",
    "leads",
    "lie",
    "links",
    "list",
    "method",
    "methods",
    "name",
    "next",
    "order",
    "ordered",
    "passed",
    "path",
    "reach",
    "reaches",
    "sequence",
    "start",
    "starting",
    "step",
    "steps",
    "them",
    "trace",
    "way",
];

/// Splits text into lowercase word tokens: on non-alphanumerics, at
/// lower-to-upper case changes (`findReferences`), before the last capital of
/// an acronym run (`HTTPServer` is `http`, `server`) and between letters and
/// digits.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current: Vec<char> = Vec::new();
    let flush = |current: &mut Vec<char>, tokens: &mut Vec<String>| {
        if !current.is_empty() {
            tokens.push(current.iter().collect::<String>().to_lowercase());
            current.clear();
        }
    };
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            flush(&mut current, &mut tokens);
            continue;
        }
        if let Some(&prev) = current.last() {
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_uppercase() && c.is_uppercase() && next_lower)
                || (prev.is_alphabetic() != c.is_alphabetic());
            if boundary {
                flush(&mut current, &mut tokens);
            }
        }
        current.push(c);
    }
    flush(&mut current, &mut tokens);
    tokens
}

/// Query tokens of a question: [`tokenize`], minus stopwords and one-letter
/// tokens, deduplicated in order of first use.
pub fn question_tokens(question: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokenize(question)
        .into_iter()
        .filter(|t| t.chars().count() > 1 && !STOPWORDS.contains(&t.as_str()))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// Per-graph statistics the rules scorer needs: name tokens per node, token
/// document frequencies and the in-degree idf.
pub struct NameIndex {
    tokens: Vec<HashSet<String>>,
    token_idf: HashMap<String, f64>,
    indegree_idf: Vec<f64>,
}

impl NameIndex {
    pub fn new(graph: &ScipGraph) -> Self {
        let n = graph.nodes.len() as f64;
        let tokens: Vec<HashSet<String>> = graph
            .nodes
            .iter()
            .map(|node| tokenize(&node.name).into_iter().collect())
            .collect();
        let mut df: HashMap<String, f64> = HashMap::new();
        for set in &tokens {
            for t in set {
                *df.entry(t.clone()).or_default() += 1.0;
            }
        }
        let token_idf = df
            .into_iter()
            .map(|(t, d)| (t, ((n + 1.0) / (d + 1.0)).ln()))
            .collect();
        let norm = (n + 1.0).ln().max(f64::MIN_POSITIVE);
        let indegree_idf = graph
            .indegree
            .iter()
            .map(|&d| ((n + 1.0) / (f64::from(d) + 1.0)).ln() / norm)
            .collect();
        Self {
            tokens,
            token_idf,
            indegree_idf,
        }
    }

    pub fn name_tokens(&self, node: usize) -> &HashSet<String> {
        &self.tokens[node]
    }
}

/// The only input from the question.
#[derive(Debug, Clone)]
pub struct Query {
    pub tokens: Vec<String>,
}

impl Query {
    pub fn new(question: &str) -> Self {
        Self {
            tokens: question_tokens(question),
        }
    }

    /// Idf-weighted share of the query tokens (those that occur in some
    /// symbol name) that the node's name tokens contain; 0 to 1.
    pub fn overlap(&self, names: &NameIndex, node: usize) -> f64 {
        let (mut shared, mut total) = (0.0, 0.0);
        for token in &self.tokens {
            if let Some(&idf) = names.token_idf.get(token) {
                total += idf;
                if names.tokens[node].contains(token) {
                    shared += idf;
                }
            }
        }
        if total > 0.0 { shared / total } else { 0.0 }
    }
}

/// One distinct callee of a node, with the strongest edge towards it.
#[derive(Debug, Clone, Copy)]
pub struct Cand {
    pub target: usize,
    pub kind: GraphEdgeKind,
    pub conf: f32,
}

/// Prior of an edge kind in the rules scorer.
pub fn kind_prior(kind: GraphEdgeKind) -> f64 {
    match kind {
        GraphEdgeKind::CallsOrRefs => 1.0,
        GraphEdgeKind::Contains => 0.5,
    }
}

/// Distinct callees of `from` in source order of their first edge; for a
/// callee reached by several edges the one with the highest prior times conf
/// (the first on ties).
pub fn candidates(graph: &ScipGraph, from: usize) -> Vec<Cand> {
    let mut out: Vec<Cand> = Vec::new();
    let mut position: HashMap<usize, usize> = HashMap::new();
    for edge in &graph.out[from] {
        let cand = Cand {
            target: edge.to,
            kind: edge.kind,
            conf: edge.conf,
        };
        match position.get(&edge.to) {
            None => {
                position.insert(edge.to, out.len());
                out.push(cand);
            }
            Some(&p) => {
                let strength = |c: &Cand| kind_prior(c.kind) * f64::from(c.conf);
                if strength(&cand) > strength(&out[p]) {
                    out[p] = cand;
                }
            }
        }
    }
    out
}

pub struct ScoreCtx<'a> {
    pub graph: &'a ScipGraph,
    pub names: &'a NameIndex,
    pub query: &'a Query,
}

/// An ordering arm. Scores are higher-is-better; ties keep source order.
pub trait Scorer {
    fn label(&self) -> String;
    /// True for an arm that follows every callee in source order and ignores
    /// the gate and the beam.
    fn unscored(&self) -> bool {
        false
    }
    fn score(&mut self, ctx: &ScoreCtx, from: usize, cands: &[Cand]) -> Vec<f64>;
}

/// Weights of the rules scorer, fixed in the pre-registration and not tuned.
pub const W_BASE: f64 = 0.1;
pub const W_INDEGREE_IDF: f64 = 0.3;
pub const W_OVERLAP: f64 = 0.6;

/// `prior(kind) * conf * (W_BASE + W_INDEGREE_IDF * idf + W_OVERLAP * overlap)`.
/// With `use_overlap` false the overlap term is 0 (the name-free ablation).
pub struct RulesScorer {
    pub use_overlap: bool,
}

impl Scorer for RulesScorer {
    fn label(&self) -> String {
        if self.use_overlap {
            "rules"
        } else {
            "rules-noname"
        }
        .to_string()
    }

    fn score(&mut self, ctx: &ScoreCtx, _from: usize, cands: &[Cand]) -> Vec<f64> {
        cands
            .iter()
            .map(|c| {
                let overlap = if self.use_overlap {
                    ctx.query.overlap(ctx.names, c.target)
                } else {
                    0.0
                };
                kind_prior(c.kind)
                    * f64::from(c.conf)
                    * (W_BASE
                        + W_INDEGREE_IDF * ctx.names.indegree_idf[c.target]
                        + W_OVERLAP * overlap)
            })
            .collect()
    }
}

/// Splitmix64: a small seeded generator, so runs need no `rand` crate.
#[derive(Debug, Clone)]
pub struct SplitMix(pub u64);

impl SplitMix {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Control: a seeded random order of the callees.
pub struct RandomScorer {
    pub rng: SplitMix,
}

impl RandomScorer {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: SplitMix(seed),
        }
    }
}

impl Scorer for RandomScorer {
    fn label(&self) -> String {
        "random".to_string()
    }

    fn score(&mut self, _ctx: &ScoreCtx, _from: usize, cands: &[Cand]) -> Vec<f64> {
        cands.iter().map(|_| self.rng.next_f64()).collect()
    }
}

/// Control: unscored first-in-first-out, callees in source order.
pub struct BfsScorer;

impl Scorer for BfsScorer {
    fn label(&self) -> String {
        "bfs".to_string()
    }

    fn unscored(&self) -> bool {
        true
    }

    fn score(&mut self, _ctx: &ScoreCtx, _from: usize, cands: &[Cand]) -> Vec<f64> {
        cands.iter().map(|_| 0.0).collect()
    }
}

/// Candidate positions best first. Ties keep source order; an unscored arm
/// keeps source order outright.
pub fn rank_order(
    scorer: &mut dyn Scorer,
    ctx: &ScoreCtx,
    from: usize,
    cands: &[Cand],
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..cands.len()).collect();
    if !scorer.unscored() {
        let scores = scorer.score(ctx, from, cands);
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    }
    order
}

/// Walk parameters.
#[derive(Debug, Clone, Copy)]
pub struct WalkConfig {
    /// Nodes with at most this many distinct callees are followed unscored.
    pub gate: usize,
    /// Callees kept per above-gate expansion.
    pub beam: usize,
    pub pop_budget: usize,
    /// Edges from the seed; a state at this depth is not expanded.
    pub max_depth: usize,
}

/// One step: the callees to follow and their hop values.
pub fn hop(
    scorer: &mut dyn Scorer,
    ctx: &ScoreCtx,
    from: usize,
    config: &WalkConfig,
) -> Vec<(usize, f64)> {
    let cands = candidates(ctx.graph, from);
    if scorer.unscored() || cands.len() <= config.gate {
        return cands.iter().map(|c| (c.target, 1.0)).collect();
    }
    rank_order(scorer, ctx, from, &cands)
        .into_iter()
        .take(config.beam)
        .enumerate()
        .map(|(rank, i)| (cands[i].target, 1.0 / (1.0 + rank as f64)))
        .collect()
}

/// A discovered path state.
#[derive(Debug, Clone, Copy)]
pub struct State {
    pub node: usize,
    pub parent: Option<usize>,
    pub depth: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WalkResult {
    pub states: Vec<State>,
    pub pops: usize,
}

struct Entry {
    priority: f64,
    seq: usize,
    state: usize,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .total_cmp(&other.priority)
            .then(other.seq.cmp(&self.seq))
    }
}

pub fn walk(
    scorer: &mut dyn Scorer,
    ctx: &ScoreCtx,
    seed: usize,
    config: &WalkConfig,
) -> WalkResult {
    let mut result = WalkResult {
        states: vec![State {
            node: seed,
            parent: None,
            depth: 0,
        }],
        pops: 0,
    };
    let mut heap = BinaryHeap::new();
    let mut priorities = vec![1.0f64];
    let mut seq = 0usize;
    if config.max_depth > 0 {
        heap.push(Entry {
            priority: 1.0,
            seq,
            state: 0,
        });
    }
    while result.pops < config.pop_budget {
        let Some(entry) = heap.pop() else { break };
        result.pops += 1;
        let state = result.states[entry.state];
        let mut on_path = HashSet::new();
        let mut cursor = Some(entry.state);
        while let Some(i) = cursor {
            on_path.insert(result.states[i].node);
            cursor = result.states[i].parent;
        }
        for (target, value) in hop(scorer, ctx, state.node, config) {
            if on_path.contains(&target) {
                continue;
            }
            let priority = priorities[entry.state] * value;
            let child = result.states.len();
            result.states.push(State {
                node: target,
                parent: Some(entry.state),
                depth: state.depth + 1,
            });
            priorities.push(priority);
            if state.depth + 1 < config.max_depth {
                seq += 1;
                heap.push(Entry {
                    priority,
                    seq,
                    state: child,
                });
            }
        }
    }
    result
}

impl WalkResult {
    /// True when some discovered state spells `path` from the seed.
    pub fn found_path(&self, path: &[usize]) -> bool {
        let Some((&last, _)) = path.split_last() else {
            return false;
        };
        self.states.iter().enumerate().any(|(i, s)| {
            if s.node != last || s.depth + 1 != path.len() {
                return false;
            }
            let mut cursor = Some(i);
            for &want in path.iter().rev() {
                match cursor {
                    Some(c) if self.states[c].node == want => cursor = self.states[c].parent,
                    _ => return false,
                }
            }
            true
        })
    }

    /// True when `node` was discovered by any path.
    pub fn reached(&self, node: usize) -> bool {
        self.states.iter().any(|s| s.node == node)
    }
}

/// Wilson 95% interval of `k` successes in `n` trials.
pub fn wilson(k: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let (k, n) = (k as f64, n as f64);
    let z = 1.959_963_984_540_054f64;
    let p = k / n;
    let denom = 1.0 + z * z / n;
    let centre = (p + z * z / (2.0 * n)) / denom;
    let half = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / denom;
    let lo = if k == 0.0 {
        0.0
    } else {
        (centre - half).max(0.0)
    };
    let hi = if k == n {
        1.0
    } else {
        (centre + half).min(1.0)
    };
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scip_graph::{RawDocument, RawOccurrence};

    fn sym(name: &str) -> String {
        format!("rust-analyzer cargo demo 0.1.0 m/{name}().")
    }

    /// Builds a graph from `(caller, callee)` pairs over functions `name`.
    /// Each function gets a one-line range at its own line; calls are
    /// referenced on that line.
    fn graph(edges: &[(&str, &str)]) -> ScipGraph {
        let mut names: Vec<&str> = Vec::new();
        for (a, b) in edges {
            for n in [a, b] {
                if !names.contains(n) {
                    names.push(n);
                }
            }
        }
        let mut occurrences = Vec::new();
        for (line, name) in names.iter().enumerate() {
            let line = line as u32;
            occurrences.push(RawOccurrence {
                range: (line, 3, line, 6),
                symbol: sym(name),
                roles: 1,
                enclosing: Some((line, 0, line, 200)),
            });
        }
        for (a, b) in edges {
            let line = names.iter().position(|n| n == a).unwrap() as u32;
            let n = occurrences.iter().filter(|o| o.range.0 == line).count() as u32;
            occurrences.push(RawOccurrence {
                range: (line, 10 + n * 5, line, 14 + n * 5),
                symbol: sym(b),
                roles: 0,
                enclosing: None,
            });
        }
        ScipGraph::build(
            &[RawDocument {
                path: "m.rs".into(),
                occurrences,
            }],
            None,
        )
    }

    fn idx(g: &ScipGraph, name: &str) -> usize {
        g.node_index(&sym(name)).unwrap()
    }

    fn config(gate: usize, beam: usize, budget: usize) -> WalkConfig {
        WalkConfig {
            gate,
            beam,
            pop_budget: budget,
            max_depth: 6,
        }
    }

    #[test]
    fn tokens_split_snake_camel_and_acronyms() {
        assert_eq!(
            tokenize("McpServer::find_references"),
            ["mcp", "server", "find", "references"]
        );
        assert_eq!(tokenize("HTTPServer2go"), ["http", "server", "2", "go"]);
        assert_eq!(
            question_tokens("How does `A::run_all` reach the Store?"),
            ["run", "all", "store"]
        );
    }

    #[test]
    fn below_the_gate_every_callee_is_followed_with_value_one() {
        let g = graph(&[("a", "b"), ("a", "c"), ("a", "d")]);
        let names = NameIndex::new(&g);
        let query = Query::new("");
        let ctx = ScoreCtx {
            graph: &g,
            names: &names,
            query: &query,
        };
        let steps = hop(
            &mut RulesScorer { use_overlap: true },
            &ctx,
            idx(&g, "a"),
            &config(3, 1, 10),
        );
        assert_eq!(steps.len(), 3);
        assert!(steps.iter().all(|&(_, v)| v == 1.0));
        // Above the gate the beam applies and values fall with rank.
        let steps = hop(
            &mut RulesScorer { use_overlap: true },
            &ctx,
            idx(&g, "a"),
            &config(2, 2, 10),
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].1, 0.5);
    }

    #[test]
    fn rules_prefer_the_callee_whose_name_overlaps_the_question() {
        let g = graph(&[("a", "parse_config"), ("a", "send_reply"), ("a", "flush")]);
        let names = NameIndex::new(&g);
        let query = Query::new("where is the reply sent? send_reply");
        let ctx = ScoreCtx {
            graph: &g,
            names: &names,
            query: &query,
        };
        let steps = hop(
            &mut RulesScorer { use_overlap: true },
            &ctx,
            idx(&g, "a"),
            &config(1, 1, 10),
        );
        assert_eq!(steps, [(idx(&g, "send_reply"), 1.0)]);
        // Without the name term the three callees tie and source order rules.
        let steps = hop(
            &mut RulesScorer { use_overlap: false },
            &ctx,
            idx(&g, "a"),
            &config(1, 1, 10),
        );
        assert_eq!(steps[0].0, idx(&g, "parse_config"));
    }

    #[test]
    fn bfs_ignores_the_gate_and_follows_source_order() {
        let g = graph(&[("a", "z"), ("a", "y"), ("a", "x")]);
        let names = NameIndex::new(&g);
        let query = Query::new("");
        let ctx = ScoreCtx {
            graph: &g,
            names: &names,
            query: &query,
        };
        let steps = hop(&mut BfsScorer, &ctx, idx(&g, "a"), &config(0, 1, 10));
        let order: Vec<usize> = steps.iter().map(|s| s.0).collect();
        assert_eq!(order, [idx(&g, "z"), idx(&g, "y"), idx(&g, "x")]);
    }

    #[test]
    fn random_is_deterministic_per_seed() {
        let g = graph(&[("a", "b"), ("a", "c"), ("a", "d"), ("a", "e"), ("a", "f")]);
        let names = NameIndex::new(&g);
        let query = Query::new("");
        let ctx = ScoreCtx {
            graph: &g,
            names: &names,
            query: &query,
        };
        let run = |seed| {
            hop(
                &mut RandomScorer {
                    rng: SplitMix(seed),
                },
                &ctx,
                idx(&g, "a"),
                &config(0, 5, 10),
            )
        };
        assert_eq!(run(7), run(7));
        assert!((1..20).any(|s| run(s) != run(7)));
    }

    #[test]
    fn walk_finds_a_path_within_budget_and_stops_at_it() {
        // a -> b -> c -> d, plus noise under b and a.
        let g = graph(&[("a", "b"), ("a", "n1"), ("b", "c"), ("b", "n2"), ("c", "d")]);
        let names = NameIndex::new(&g);
        let query = Query::new("");
        let ctx = ScoreCtx {
            graph: &g,
            names: &names,
            query: &query,
        };
        let path = [idx(&g, "a"), idx(&g, "b"), idx(&g, "c"), idx(&g, "d")];
        let found = walk(&mut BfsScorer, &ctx, path[0], &config(8, 1, 200));
        assert!(found.found_path(&path));
        assert!(found.reached(path[3]));
        // Frontier order a, b, n1, c, n2: d is discovered when c is popped,
        // the fourth pop; a budget of three pops misses it.
        let short = walk(&mut BfsScorer, &ctx, path[0], &config(8, 1, 3));
        assert_eq!(short.pops, 3);
        assert!(!short.found_path(&path));
        assert!(walk(&mut BfsScorer, &ctx, path[0], &config(8, 1, 4)).found_path(&path));
    }

    #[test]
    fn walk_does_not_revisit_nodes_on_its_own_path_and_caps_depth() {
        let g = graph(&[("a", "b"), ("b", "a"), ("b", "c"), ("c", "d"), ("d", "e")]);
        let names = NameIndex::new(&g);
        let query = Query::new("");
        let ctx = ScoreCtx {
            graph: &g,
            names: &names,
            query: &query,
        };
        let cfg = WalkConfig {
            max_depth: 3,
            ..config(8, 1, 200)
        };
        let w = walk(&mut BfsScorer, &ctx, idx(&g, "a"), &cfg);
        assert!(w.reached(idx(&g, "d")));
        assert!(!w.reached(idx(&g, "e")));
        assert!(w.states.iter().all(|s| s.depth <= 3));
        assert_eq!(
            w.states.iter().filter(|s| s.node == idx(&g, "a")).count(),
            1
        );
    }

    #[test]
    fn wilson_matches_known_values() {
        let (lo, hi) = wilson(50, 100);
        assert!((lo - 0.4038).abs() < 1e-3 && (hi - 0.5962).abs() < 1e-3);
        assert_eq!(wilson(0, 0), (0.0, 1.0));
        assert!(wilson(10, 10).1 <= 1.0);
    }
}
