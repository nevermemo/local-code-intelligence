//! Personalised PageRank over the SCIP graph (`seeds.repo_map`, slice K18a).
//!
//! The ranking behind Aider's repo map, on the graph this process already
//! holds: a random surfer follows a call or containment edge with probability
//! [`DAMPING`] and otherwise jumps to a node drawn from the personalisation, the
//! nodes whose name, path or documentation hold the question's words
//! ([`Bm25Seeds`] scores). A function that many of those nodes call collects
//! their mass even when its own name holds none of the question's words.
//!
//! Pure and deterministic: the edge list is built once per index stamp, a rank
//! is a fixed number of power iterations over it (no randomness, no clock, the
//! same summation order every time), and equal scores are ordered by node id.
//! Edges are the graph's out-edges except `implements` (the walk's kind prior
//! treats that relation apart, and following it would make a trait method the
//! hub of its implementations), counted once per (caller, callee) pair, without
//! self loops. Only the graph's function and method nodes exist here: types,
//! traits and modules are never ranked.

// Wired by the next commit (K18a, commit 2).
#![allow(dead_code)]

use super::seeds::Bm25Seeds;
use lci_core::node::NodeId;
use lci_maps::scip_graph::{GraphEdgeKind, ScipMap};
use std::collections::HashMap;

/// Probability of following an edge (networkx's and Aider's default); the rest
/// is the jump to the personalisation. Not fitted.
pub(super) const DAMPING: f64 = 0.85;
/// The most power iterations of one rank. Not fitted.
pub(super) const ITERATIONS: usize = 30;
/// A rank stops early when one iteration moves the scores by less than this
/// (the L1 distance between two successive score vectors).
pub(super) const TOLERANCE: f64 = 1e-9;
/// How many of the question's best BM25 matches carry the personalisation
/// (Aider's repo map personalises on a comparably small set of mentioned
/// identifiers and files). Not fitted.
pub(super) const PERSONALISE_K: usize = 64;

/// What one rank did, for the response and the measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stats {
    /// Power iterations run (at most [`ITERATIONS`]); 0 when nothing was ranked.
    pub(super) iterations: usize,
    /// How many nodes carried the personalisation.
    pub(super) personalised: usize,
}

/// The graph's edges as plain index lists, for repeated ranks.
pub(super) struct RepoMap {
    ids: Vec<NodeId>,
    /// Per node: the graph's `Owner::name`, for the seed's details.
    names: Vec<String>,
    /// Per node: its distinct callees and children, sorted, without itself.
    out: Vec<Vec<u32>>,
    index: HashMap<NodeId, usize>,
}

impl RepoMap {
    /// The edge lists of the SCIP graph, every kind of edge but `implements`.
    pub(super) fn new(scip: &ScipMap) -> Self {
        let graph = scip.graph();
        let n = graph.nodes.len();
        let ids: Vec<NodeId> = (0..n).map(|i| scip.node_id(i).clone()).collect();
        let names = graph.nodes.iter().map(|node| node.name.clone()).collect();
        let out = (0..n)
            .map(|from| {
                let mut targets: Vec<u32> = graph.out[from]
                    .iter()
                    .filter(|edge| edge.kind != GraphEdgeKind::Implements && edge.to != from)
                    .filter_map(|edge| u32::try_from(edge.to).ok())
                    .collect();
                targets.sort_unstable();
                targets.dedup();
                targets
            })
            .collect();
        let index = ids.iter().cloned().zip(0..).collect();
        Self {
            ids,
            names,
            out,
            index,
        }
    }

    /// How many nodes the graph has.
    pub(super) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// How many distinct edges the ranking follows.
    #[cfg(test)]
    pub(super) fn edge_count(&self) -> usize {
        self.out.iter().map(Vec::len).sum()
    }

    /// The personalised PageRank of every node, summing to 1, and how many
    /// iterations it took. `personal` is `(node index, weight)`: the weights
    /// are normalised to sum 1 (a non-positive weight and an index outside the
    /// graph count for nothing); with none left the jump is uniform. The scores
    /// start at the personalisation; the mass of a node with no out-edge goes
    /// back to the personalisation, so the total is conserved. Stops after
    /// `iterations` or when one iteration moves the scores by less than
    /// `tolerance` (L1).
    pub(super) fn pagerank(
        &self,
        personal: &[(usize, f64)],
        damping: f64,
        iterations: usize,
        tolerance: f64,
    ) -> (Vec<f64>, usize) {
        let n = self.out.len();
        if n == 0 {
            return (Vec::new(), 0);
        }
        let mut jump = vec![0.0f64; n];
        for &(node, weight) in personal {
            if node < n && weight.is_finite() && weight > 0.0 {
                jump[node] += weight;
            }
        }
        let total: f64 = jump.iter().sum();
        if total > 0.0 {
            jump.iter_mut().for_each(|p| *p /= total);
        } else {
            jump.fill(1.0 / n as f64);
        }
        let mut scores = jump.clone();
        let mut done = 0;
        for _ in 0..iterations {
            let mut dangling = 0.0f64;
            let mut next = vec![0.0f64; n];
            for (from, targets) in self.out.iter().enumerate() {
                if targets.is_empty() {
                    dangling += scores[from];
                    continue;
                }
                let share = damping * scores[from] / targets.len() as f64;
                for &to in targets {
                    next[to as usize] += share;
                }
            }
            let teleport = (1.0 - damping) + damping * dangling;
            for (score, p) in next.iter_mut().zip(&jump) {
                *score += teleport * p;
            }
            let moved: f64 = next.iter().zip(&scores).map(|(a, b)| (a - b).abs()).sum();
            scores = next;
            done += 1;
            if moved < tolerance {
                break;
            }
        }
        (scores, done)
    }

    /// The best `limit` nodes for the question: the BM25 entity index's best
    /// [`PERSONALISE_K`] matches are the personalisation, the rank runs with the
    /// constants above, and the nodes that gained any mass come back best first
    /// (equal scores by node id) with their `Owner::name` and score. No BM25
    /// match, no personalisation and no nodes: a global hub answers no question.
    pub(super) fn matches(
        &self,
        bm25: &Bm25Seeds,
        question: &str,
        limit: usize,
    ) -> (Vec<(NodeId, String, f64)>, Stats) {
        let personal: Vec<(usize, f64)> = bm25
            .matches(question, PERSONALISE_K)
            .into_iter()
            .filter_map(|(node, _, score)| Some((*self.index.get(&node)?, score)))
            .collect();
        let none = Stats {
            iterations: 0,
            personalised: 0,
        };
        if !personal.iter().any(|(_, weight)| *weight > 0.0) {
            return (Vec::new(), none);
        }
        let personalised = personal.len();
        let (scores, iterations) = self.pagerank(&personal, DAMPING, ITERATIONS, TOLERANCE);
        let mut order: Vec<usize> = (0..scores.len()).filter(|&i| scores[i] > 0.0).collect();
        order.sort_by(|&a, &b| {
            scores[b]
                .total_cmp(&scores[a])
                .then_with(|| self.ids[a].cmp(&self.ids[b]))
        });
        let ranked = order
            .into_iter()
            .take(limit)
            .map(|i| (self.ids[i].clone(), self.names[i].clone(), scores[i]))
            .collect();
        (
            ranked,
            Stats {
                iterations,
                personalised,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_maps::scip_graph::{RawDocument, RawOccurrence, ScipGraph, SourceText};

    fn sym(name: &str) -> String {
        format!("rust-analyzer cargo demo 0.1.0 m/{name}().")
    }

    /// A graph of functions named by the `(caller, callee)` pairs, one per
    /// line of `m.rs`, each calling its callees on its own line.
    fn graph(edges: &[(&str, &str)]) -> ScipMap {
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
        let documents = [RawDocument {
            path: "m.rs".into(),
            occurrences,
        }];
        ScipMap::with_default_ids(ScipGraph::build(&documents, None))
    }

    fn at(scip: &ScipMap, name: &str) -> usize {
        scip.graph().node_index(&sym(name)).unwrap()
    }

    /// The rank of the pairs' graph, uniform unless `personal` names nodes.
    fn rank(edges: &[(&str, &str)], personal: &[(&str, f64)]) -> (ScipMap, Vec<f64>, usize) {
        let scip = graph(edges);
        let personal: Vec<(usize, f64)> = personal
            .iter()
            .map(|(name, weight)| (at(&scip, name), *weight))
            .collect();
        let (scores, iterations) =
            RepoMap::new(&scip).pagerank(&personal, DAMPING, ITERATIONS, TOLERANCE);
        (scip, scores, iterations)
    }

    /// The function names (without the module `m::`) best first by score,
    /// equal scores by node id.
    fn order(scip: &ScipMap, scores: &[f64]) -> Vec<String> {
        let mut nodes: Vec<usize> = (0..scores.len()).collect();
        nodes.sort_by(|&a, &b| {
            scores[b]
                .total_cmp(&scores[a])
                .then_with(|| scip.node_id(a).cmp(scip.node_id(b)))
        });
        nodes
            .into_iter()
            .map(|i| {
                scip.graph().nodes[i]
                    .name
                    .trim_start_matches("m::")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn pagerank_on_a_star_puts_the_hub_first() {
        let (scip, scores, _) = rank(&[("a", "h"), ("b", "h"), ("c", "h")], &[]);
        let ranked = order(&scip, &scores);
        assert_eq!(ranked[0], "h", "{ranked:?}");
        // The three callers tie, and the tie is broken by node id.
        let (a, b, c) = (at(&scip, "a"), at(&scip, "b"), at(&scip, "c"));
        assert!(
            scores[a] == scores[b] && scores[b] == scores[c],
            "{scores:?}"
        );
        assert!(scores[at(&scip, "h")] > scores[a]);
        assert_eq!(&ranked[1..], ["a", "b", "c"]);
    }

    #[test]
    fn personalisation_moves_the_mass_to_the_question_s_component() {
        // Two disconnected components: a hub `h` called by three, and a pair
        // `x -> y`. The jump lands on `x` alone.
        let edges = [("a", "h"), ("b", "h"), ("c", "h"), ("x", "y")];
        let (scip, uniform, _) = rank(&edges, &[]);
        assert_eq!(order(&scip, &uniform)[0], "h", "uniform: the hub");
        let (scip, scores, _) = rank(&edges, &[("x", 1.0)]);
        let h = scores[at(&scip, "h")];
        assert!(
            scores[at(&scip, "x")] > h && scores[at(&scip, "y")] > h,
            "{scores:?}"
        );
        // The first component holds no mass at all.
        for name in ["a", "b", "c", "h"] {
            assert_eq!(scores[at(&scip, name)], 0.0, "{name}");
        }
        assert_eq!(order(&scip, &scores)[..2], ["x", "y"]);
    }

    #[test]
    fn mass_is_conserved_with_dangling_nodes() {
        // A chain whose last node (and the isolated `lone`) has no out-edge.
        let scip = graph(&[("a", "b"), ("b", "c"), ("c", "d"), ("lone", "lone")]);
        let repo = RepoMap::new(&scip);
        let personal = [(at(&scip, "a"), 3.0), (at(&scip, "c"), 1.0)];
        for iterations in [1, 5, 30] {
            let (scores, done) = repo.pagerank(&personal, DAMPING, iterations, 0.0);
            let sum: f64 = scores.iter().sum();
            assert!((sum - 1.0).abs() < 1e-9, "{iterations}: {sum}");
            assert_eq!(done, iterations);
            assert!(scores.iter().all(|s| *s >= 0.0));
        }
        // The weights are normalised: 3:1 is the same jump as 0.75:0.25.
        let scaled = [(at(&scip, "a"), 0.75), (at(&scip, "c"), 0.25)];
        assert_eq!(
            repo.pagerank(&personal, DAMPING, 30, 0.0),
            repo.pagerank(&scaled, DAMPING, 30, 0.0)
        );
    }

    /// `a.rs`: a trait `Tr` with a method `m`, three impls of it, and a
    /// function `caller` that calls `callee`.
    fn trait_graph() -> ScipMap {
        const SRC: &str = "pub trait Tr {\n    fn m(&self);\n}\nimpl Tr for A {\n    fn m(&self) {}\n}\nimpl Tr for B {\n    fn m(&self) {}\n}\nimpl Tr for C {\n    fn m(&self) {}\n}\nfn caller() { callee(); }\nfn callee() {}\n";
        struct One(Vec<u8>);
        impl SourceText for One {
            fn read(&self, path: &str) -> Option<Vec<u8>> {
                (path == "a.rs").then(|| self.0.clone())
            }
        }
        let sym = |path: &str| format!("rust-analyzer cargo demo 0.1.0 {path}");
        let def = |range, symbol: &str, enclosing| RawOccurrence {
            range,
            symbol: sym(symbol),
            roles: 1,
            enclosing: Some(enclosing),
        };
        let reference = |range, symbol: &str| RawOccurrence {
            range,
            symbol: sym(symbol),
            roles: 0,
            enclosing: None,
        };
        let documents = [RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                def((0, 10, 0, 12), "a/Tr#", (0, 0, 2, 1)),
                def((1, 7, 1, 8), "a/Tr#m().", (1, 4, 1, 16)),
                reference((3, 5, 3, 7), "a/Tr#"),
                reference((3, 12, 3, 13), "a/A#"),
                def((4, 7, 4, 8), "a/impl#[A][Tr]m().", (4, 4, 4, 18)),
                reference((6, 5, 6, 7), "a/Tr#"),
                reference((6, 12, 6, 13), "a/B#"),
                def((7, 7, 7, 8), "a/impl#[B][Tr]m().", (7, 4, 7, 18)),
                reference((9, 5, 9, 7), "a/Tr#"),
                reference((9, 12, 9, 13), "a/C#"),
                def((10, 7, 10, 8), "a/impl#[C][Tr]m().", (10, 4, 10, 18)),
                def((12, 3, 12, 9), "a/caller().", (12, 0, 12, 25)),
                reference((12, 14, 12, 20), "a/callee()."),
                def((13, 3, 13, 9), "a/callee().", (13, 0, 13, 14)),
            ],
        }];
        ScipMap::with_default_ids(ScipGraph::build(
            &documents,
            Some(&One(SRC.as_bytes().to_vec()) as &dyn SourceText),
        ))
    }

    #[test]
    fn implements_edges_are_not_followed() {
        let scip = trait_graph();
        let graph = scip.graph();
        // The fixture has the three `implements` edges into the declaration.
        assert_eq!(graph.edge_count(GraphEdgeKind::Implements), 3);
        let decl = graph
            .node_index("rust-analyzer cargo demo 0.1.0 a/Tr#m().")
            .unwrap();
        let callee = graph
            .node_index("rust-analyzer cargo demo 0.1.0 a/callee().")
            .unwrap();
        let repo = RepoMap::new(&scip);
        // Only the call is followed: no edge into or out of the declaration.
        assert_eq!(repo.edge_count(), 1);
        let (scores, _) = repo.pagerank(&[], DAMPING, ITERATIONS, TOLERANCE);
        assert!(scores[callee] > scores[decl], "{scores:?}");
        // Were the three edges followed the declaration would lead.
        let mut followed = RepoMap::new(&scip);
        for node in 0..graph.nodes.len() {
            for edge in &graph.out[node] {
                if edge.kind == GraphEdgeKind::Implements {
                    followed.out[node].push(edge.to as u32);
                }
            }
        }
        let (scores, _) = followed.pagerank(&[], DAMPING, ITERATIONS, TOLERANCE);
        assert!(scores[decl] > scores[callee], "{scores:?}");
    }

    #[test]
    fn a_node_that_calls_itself_or_twice_is_one_edge_to_each_callee() {
        let scip = graph(&[("a", "a"), ("a", "b"), ("a", "b"), ("a", "c")]);
        let repo = RepoMap::new(&scip);
        assert_eq!(repo.edge_count(), 2);
        assert_eq!(repo.len(), 3);
        assert!(!repo.is_empty());
    }

    #[test]
    fn the_ranking_is_deterministic() {
        let edges = [
            ("a", "h"),
            ("b", "h"),
            ("c", "h"),
            ("h", "t"),
            ("c", "u"),
            ("u", "a"),
        ];
        let scip = graph(&edges);
        let repo = RepoMap::new(&scip);
        let personal = [
            (at(&scip, "a"), 2.0),
            (at(&scip, "b"), 2.0),
            (at(&scip, "u"), 1.0),
        ];
        let first = repo.pagerank(&personal, DAMPING, ITERATIONS, TOLERANCE);
        let second = RepoMap::new(&scip).pagerank(&personal, DAMPING, ITERATIONS, TOLERANCE);
        assert_eq!(first.1, second.1);
        let bits = |scores: &[f64]| scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&first.0), bits(&second.0), "bitwise");
        // Equal scores order by node id: `a` and `b` tie under a symmetric jump.
        let (scip, tied, _) = rank(&[("b", "h"), ("a", "h")], &[("a", 1.0), ("b", 1.0)]);
        assert_eq!(tied[at(&scip, "a")], tied[at(&scip, "b")]);
        let ranked = order(&scip, &tied);
        assert_eq!(ranked, ["h", "a", "b"]);
    }

    #[test]
    fn convergence_stops_early_on_a_small_graph_and_reports_iterations() {
        // A ring of five with the uniform jump starts at its fixed point: one
        // iteration confirms it. (Most graphs converge geometrically, at a rate
        // of the damping, and run the whole `ITERATIONS`.)
        let ring = [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e"), ("e", "a")];
        let (_, scores, iterations) = rank(&ring, &[]);
        assert!(iterations < ITERATIONS, "{iterations}");
        assert_eq!(iterations, 1);
        assert!(scores.iter().all(|s| (s - 0.2).abs() < 1e-12), "{scores:?}");
        // A star under a jump on one leaf is not at its fixed point: it runs on,
        // and the cap holds.
        let scip = graph(&[("a", "h"), ("b", "h"), ("c", "h")]);
        let repo = RepoMap::new(&scip);
        let personal = [(at(&scip, "a"), 1.0)];
        let (_, capped) = repo.pagerank(&personal, DAMPING, 5, 0.0);
        assert_eq!(capped, 5);
        let (_, ran) = repo.pagerank(&personal, DAMPING, 3, TOLERANCE);
        assert_eq!(ran, 3);
        let (_, none) = repo.pagerank(&personal, DAMPING, 0, TOLERANCE);
        assert_eq!(none, 0);
    }

    #[test]
    fn matches_personalise_on_bm25_and_give_nothing_without_a_match() {
        // `read_frame` is called by the three functions the question names and
        // holds none of its words; ten other functions fill the index.
        let scip = graph(&[
            ("parse_alpha", "read_frame"),
            ("parse_beta", "read_frame"),
            ("parse_gamma", "read_frame"),
            ("zeta_one", "zeta_two"),
            ("kilo_three", "lima_four"),
            ("mike_five", "nova_six"),
            ("oscar_seven", "papa_eight"),
            ("quebec_nine", "romeo_ten"),
        ]);
        let repo = RepoMap::new(&scip);
        let bm25 = Bm25Seeds::new(&scip, &Default::default());
        let (ranked, stats) = repo.matches(&bm25, "parse alpha beta gamma", 8);
        let names: Vec<&str> = ranked.iter().map(|(_, name, _)| name.as_str()).collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert!(names[0].ends_with("read_frame"), "the hub first: {names:?}");
        assert!(ranked.windows(2).all(|pair| pair[0].2 >= pair[1].2));
        // Nothing outside the callers' component gained mass.
        assert!(!names.iter().any(|name| name.contains("zeta")), "{names:?}");
        assert_eq!(stats.personalised, 3);
        assert!((1..=ITERATIONS).contains(&stats.iterations));
        assert_eq!(repo.matches(&bm25, "parse alpha beta gamma", 2).0.len(), 2);
        // The hub is no BM25 match: the question alone would not find it.
        assert!(
            bm25.matches("parse alpha beta gamma", 8)
                .iter()
                .all(|(_, name, _)| !name.ends_with("read_frame"))
        );
        // No BM25 match, no personalisation, no nodes (a uniform jump would
        // rank the global hub first).
        let (none, stats) = repo.matches(&bm25, "zebra", 8);
        assert!(none.is_empty());
        assert_eq!((stats.iterations, stats.personalised), (0, 0));
        assert!(repo.matches(&bm25, "", 8).0.is_empty());
    }

    #[test]
    fn an_empty_graph_ranks_nothing() {
        let scip = ScipMap::with_default_ids(ScipGraph::empty());
        let repo = RepoMap::new(&scip);
        assert!(repo.is_empty());
        assert_eq!(
            repo.pagerank(&[(0, 1.0)], DAMPING, ITERATIONS, TOLERANCE),
            (Vec::new(), 0)
        );
    }
}
