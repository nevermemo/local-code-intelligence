//! The first stage of a traversal call: answers that are not the walk's.
//!
//! Two exact operators (descriptor-only maps, so they sit in the maps list, the
//! selectors and the trace; the walk never queries them) answer a question that
//! names a symbol or asks about history without walking:
//!
//! * `exact.closure`: [`ImpactIndex::ranked_closure`], the declarations that
//!   hold a reference to the symbol the question names, most references first.
//!   A card per declaration: `node` is the id `scip.impact` gives the symbol
//!   (the SCIP graph's when the graph holds it), `site` is where it is defined
//!   (always, so a declaration the graph does not hold can still be located).
//! * `exact.pickaxe`: [`classify`] and [`history_commits`], the `git log
//!   -G/-S/--grep` calls that answer a history question, one card per commit
//!   (`commit:<sha>`). Every git call is counted as a physical external call and
//!   runs under a timeout.
//!
//! Each abstains (no cards) when nothing the question names resolves, and says
//! so in a note. The first-stage list is the operators' cards in the order of
//! the maps list, then, with `answer_seeds`, the start nodes ([`seed_cards`]);
//! [`merge`] joins it with the walk's ranked cards by `[traverse] first_stage`.
//!
//! A continuation (a call with a `handle`) does not run the first stage again
//! and returns the walk's results alone: the first response already carried the
//! first-stage list, and a handle that kept it would have to store the list in
//! the snapshot. A note says so when the setting is on.

use crate::config::TraverseFirstStage;
use lci_core::node::NodeId;
use lci_maps::{
    MapId,
    exact_history::{GitLog, Tally, classify, history_commits, run_capture},
    scip_impact::ImpactMap,
    traverse::{Seed, stop::Card, trace},
};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    cell::RefCell,
    collections::HashSet,
    path::PathBuf,
    process::Command,
    str::FromStr,
    time::{Duration, Instant},
};

/// The exact operator over the SCIP impact index.
pub const EXACT_CLOSURE_ID: MapId = "exact.closure";
/// The exact operator over the git history.
pub const EXACT_PICKAXE_ID: MapId = "exact.pickaxe";

/// One card of an exact operator, with what the engine needs besides the JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct ExactCard {
    pub node: NodeId,
    /// The SCIP graph holds the node (it can start a walk).
    pub in_graph: bool,
    /// The operator that made the card.
    pub op: MapId,
    /// References to the anchor the declaration holds (`exact.closure`).
    pub held: usize,
    /// The card as the response lists it (with `origin: "exact"`).
    pub card: Value,
}

/// A card of the first stage: the fields every card of the response has.
fn base_card(node: &NodeId, via: Option<MapId>, maps: Vec<MapId>, origin: &str) -> Value {
    let card = Card {
        node: node.clone(),
        via,
        witness: None,
        potential: 1.0,
        ranked_potential: 1.0,
        depth: 0,
        n_maps: maps.len(),
        maps,
    };
    let mut value = trace::card_json(&card);
    if let Some(object) = value.as_object_mut() {
        object.insert("origin".into(), json!(origin));
    }
    value
}

/// What `exact.closure` made of a question, with the note for the response.
pub struct Closure {
    pub cards: Vec<ExactCard>,
    pub note: String,
}

/// `exact.closure` over the index of a `scip.impact` map. `in_graph` says
/// whether the SCIP graph holds a symbol.
pub fn closure_cards(map: &ImpactMap, in_graph: &dyn Fn(&str) -> bool, question: &str) -> Closure {
    let ix = map.index();
    let ranked = ix.ranked_closure(question);
    if !ranked.anchored {
        return Closure {
            cards: Vec::new(),
            note: format!("{EXACT_CLOSURE_ID}: names {}, anchored no", ranked.names),
        };
    }
    let mut cards = Vec::new();
    for entry in &ranked.decls {
        let Some(node) = map.node_of(&entry.decl.symbol) else {
            continue;
        };
        let col = ix.defs[entry.def].name_range.1 + 1;
        let mut card = base_card(
            node,
            Some(EXACT_CLOSURE_ID),
            vec![EXACT_CLOSURE_ID],
            "exact",
        );
        if let Some(object) = card.as_object_mut() {
            object.insert(
                "site".into(),
                json!({
                    "path": entry.decl.path.replace('\\', "/"),
                    "line": entry.decl.line,
                    "col": col,
                }),
            );
            object.insert("held".into(), json!(entry.held));
            object.insert("name".into(), json!(entry.decl.name));
        }
        cards.push(ExactCard {
            node: node.clone(),
            in_graph: in_graph(&entry.decl.symbol),
            op: EXACT_CLOSURE_ID,
            held: entry.held,
            card,
        });
    }
    Closure {
        note: format!(
            "{EXACT_CLOSURE_ID}: names {}, anchored yes, closure {}, kept {}",
            ranked.names,
            ranked.decls.len(),
            cards.len()
        ),
        cards,
    }
}

/// The error [`BudgetedGit`] gives once the first stage's time is spent.
pub const STAGE_TIME_SPENT: &str = "first-stage time spent";

/// `git log` over a tree under one budget for all the calls: each call may run
/// as long as the least of `per_call` and the time left until `end`; once `end
/// has passed, a call is refused ([`STAGE_TIME_SPENT`]) without running git.
pub struct BudgetedGit {
    pub git: PathBuf,
    pub tree: PathBuf,
    pub per_call: Duration,
    pub end: Instant,
}

impl GitLog for BudgetedGit {
    fn log(&self, args: &[String]) -> Result<Vec<String>, String> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(STAGE_TIME_SPENT.to_owned());
        }
        let mut command = Command::new(&self.git);
        command.arg("-C").arg(&self.tree).arg("log").args(args);
        let text = run_capture(command, self.per_call.min(left))?;
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// A [`GitLog`] that counts the calls made through it (a call refused for
/// [`STAGE_TIME_SPENT`] is not a call) and notes the refusals.
pub struct CountingGit<'a> {
    inner: &'a dyn GitLog,
    calls: Cell<u32>,
    refused: Cell<u32>,
}

impl<'a> CountingGit<'a> {
    pub fn new(inner: &'a dyn GitLog) -> Self {
        Self {
            inner,
            calls: Cell::new(0),
            refused: Cell::new(0),
        }
    }

    /// Calls made so far, failed ones included.
    pub fn calls(&self) -> u32 {
        self.calls.get()
    }

    /// Calls refused because the first stage's time was spent.
    pub fn refused(&self) -> u32 {
        self.refused.get()
    }
}

impl GitLog for CountingGit<'_> {
    fn log(&self, args: &[String]) -> Result<Vec<String>, String> {
        let result = self.inner.log(args);
        if matches!(&result, Err(why) if why == STAGE_TIME_SPENT) {
            self.refused.set(self.refused.get() + 1);
        } else {
            self.calls.set(self.calls.get() + 1);
        }
        result
    }
}

/// What `exact.pickaxe` made of a question.
pub struct Pickaxe {
    pub cards: Vec<ExactCard>,
    /// Git calls made (each one a physical external call).
    pub git_calls: u32,
    pub note: String,
}

/// `exact.pickaxe`: the commits the history baseline finds for the question,
/// at most `k`, as cards.
pub fn pickaxe_cards(git: &dyn GitLog, question: &str, k: usize) -> Pickaxe {
    let query = classify(question);
    let counted = CountingGit::new(git);
    let recency = RefCell::new(None);
    let mut tally = Tally::default();
    let shas = history_commits(&counted, &recency, &query, k, &mut tally);
    let mut cards = Vec::new();
    for sha in shas {
        let Ok(node) = NodeId::from_str(&format!("commit:{sha}")) else {
            continue;
        };
        cards.push(ExactCard {
            card: base_card(
                &node,
                Some(EXACT_PICKAXE_ID),
                vec![EXACT_PICKAXE_ID],
                "exact",
            ),
            node,
            in_graph: false,
            op: EXACT_PICKAXE_ID,
            held: 0,
        });
    }
    let mut note = if cards.is_empty() {
        format!("{EXACT_PICKAXE_ID}: class {}, no commits", query.family())
    } else {
        format!(
            "{EXACT_PICKAXE_ID}: class {}, {} commits",
            query.family(),
            cards.len()
        )
    };
    note.push_str(&format!(" (git calls {}", counted.calls()));
    let failed = tally.failures.saturating_sub(counted.refused() as usize);
    if let (true, Some(first)) = (failed > 0, &tally.first_failure) {
        note.push_str(&format!(", failed {failed}: {first}"));
    }
    if counted.refused() > 0 {
        note.push_str(
            ", stopped: the first stage's time ran out, the commits found so far are kept",
        );
    }
    note.push(')');
    Pickaxe {
        cards,
        git_calls: counted.calls(),
        note,
    }
}

/// The start nodes as cards (`origin: "seed"`), in start order, with the rank
/// the hybrid search gave the hit when the seed has one.
pub fn seed_cards(seeds: &[Seed], details: &[Value]) -> Vec<Value> {
    seeds
        .iter()
        .enumerate()
        .map(|(i, seed)| {
            let mut card = base_card(&seed.node, seed.map, Vec::new(), "seed");
            let rank = details
                .iter()
                .find(|d| d["node"].as_str() == Some(seed.node.to_string().as_str()))
                .and_then(|d| d["hit_rank"].as_u64());
            if let Some(object) = card.as_object_mut() {
                object.insert("seed_rank".into(), json!(rank.unwrap_or(i as u64 + 1)));
            }
            card
        })
        .collect()
}

/// What the response needs of the first stage.
#[derive(Debug, Clone, PartialEq)]
pub struct FirstStage {
    pub mode: TraverseFirstStage,
    /// Cap on the first-stage list, 0 for none.
    pub first_k: usize,
    /// The first-stage cards: the exact operators', then the seeds'.
    pub cards: Vec<Value>,
    /// Git calls the exact operators made, to add to the physical external
    /// calls of the totals.
    pub git_calls: u32,
    /// The policy asks for the first stage alone: the walk's cards are not
    /// returned (the walk took one pop; a handle continues it).
    pub walk_off: bool,
}

impl FirstStage {
    /// No first stage: the results are the walk's alone.
    pub fn off() -> Self {
        Self {
            mode: TraverseFirstStage::Off,
            first_k: 0,
            cards: Vec::new(),
            git_calls: 0,
            walk_off: false,
        }
    }

    /// Whether the results are merged (the setting is not `off`).
    pub fn active(&self) -> bool {
        self.mode != TraverseFirstStage::Off
    }
}

fn node_of(card: &Value) -> Option<&str> {
    card["node"].as_str()
}

/// The cards of `list` not seen before, marking each as seen. A card with no
/// node is kept (nothing says it repeats).
fn unseen<'a>(list: &'a [Value], seen: &mut HashSet<&'a str>) -> Vec<&'a Value> {
    list.iter()
        .filter(|card| node_of(card).is_none_or(|node| seen.insert(node)))
        .collect()
}

/// Joins the first-stage list with the walk's cards by `mode`; pure.
///
/// * The first-stage list is cut to `first_k` after repeats are dropped
///   (`first_k` 0: no cut); a node listed twice keeps its first place.
/// * `Off`: the walk's first `top_k` cards, as they are.
/// * `Append`: the first-stage cards, then at most `top_k` walk cards that the
///   first stage does not list (so up to `first_k + top_k` cards).
/// * `Interleave`: first-stage and walk cards alternately, a first-stage card
///   first, skipping a card whose node is already listed, until both lists are
///   used up or `top_k` cards are listed; once one list is used up the other
///   goes on.
///
/// Walk cards get `origin: "walk"` (unless `Off`). Give the walk's cards in
/// rank order, at least `top_k` plus the number of first-stage cards when they
/// exist, so that repeats can be skipped.
pub fn merge(
    mode: TraverseFirstStage,
    first: &[Value],
    walk: &[Value],
    first_k: usize,
    top_k: usize,
) -> Vec<Value> {
    if mode == TraverseFirstStage::Off {
        return walk.iter().take(top_k).cloned().collect();
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut first_list = unseen(first, &mut seen);
    seen.clear();
    if first_k > 0 {
        first_list.truncate(first_k);
    }
    let in_first: HashSet<&str> = first_list.iter().filter_map(|card| node_of(card)).collect();
    let walk_list: Vec<&Value> = walk
        .iter()
        .filter(|card| node_of(card).is_none_or(|node| !in_first.contains(node)))
        .collect();
    // Repeats inside the walk's own list cannot occur (one card per state).
    let walk_card = |card: &Value| {
        let mut card = card.clone();
        if let Some(object) = card.as_object_mut() {
            object.entry("origin").or_insert_with(|| json!("walk"));
        }
        card
    };
    match mode {
        TraverseFirstStage::Off => unreachable!("handled above"),
        TraverseFirstStage::Append => first_list
            .iter()
            .map(|card| (*card).clone())
            .chain(walk_list.iter().take(top_k).map(|card| walk_card(card)))
            .collect(),
        TraverseFirstStage::Interleave => {
            let mut out: Vec<Value> = Vec::new();
            let (mut i, mut j) = (0, 0);
            let mut first_turn = true;
            while out.len() < top_k && (i < first_list.len() || j < walk_list.len()) {
                if first_turn {
                    if let Some(card) = first_list.get(i) {
                        out.push((*card).clone());
                        i += 1;
                    }
                } else if let Some(card) = walk_list.get(j) {
                    out.push(walk_card(card));
                    j += 1;
                }
                first_turn = !first_turn;
            }
            out
        }
    }
}

/// `{"exact": n, "seeds": n, "walk": n}`: the cards by origin.
pub fn origin_counts(cards: &[Value]) -> Value {
    let count = |origin: &str| cards.iter().filter(|c| c["origin"] == origin).count();
    json!({"exact": count("exact"), "seeds": count("seed"), "walk": count("walk")})
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_maps::{
        impact::ImpactIndex,
        scip_graph::{RawDocument, RawOccurrence},
        scip_impact::ImpactOptions,
    };
    use scip::types::symbol_information::Kind;
    use std::collections::HashMap;

    fn card(node: &str, origin: &str) -> Value {
        json!({"node": node, "origin": origin})
    }

    fn walk_card(node: &str) -> Value {
        json!({"node": node})
    }

    fn nodes(cards: &[Value]) -> Vec<String> {
        cards
            .iter()
            .map(|c| {
                format!(
                    "{}:{}",
                    &c["origin"].as_str().unwrap_or("-")[..1],
                    node_of(c).unwrap()
                )
            })
            .collect()
    }

    fn exact(names: &[&str]) -> Vec<Value> {
        names.iter().map(|n| card(n, "exact")).collect()
    }

    fn walk(names: &[&str]) -> Vec<Value> {
        names.iter().map(|n| walk_card(n)).collect()
    }

    #[test]
    fn off_returns_the_walk_alone_untouched() {
        let w = walk(&["a", "b", "c"]);
        let out = merge(TraverseFirstStage::Off, &exact(&["x"]), &w, 10, 2);
        assert_eq!(out, w[..2].to_vec());
        assert!(out.iter().all(|c| c.get("origin").is_none()));
    }

    #[test]
    fn append_lists_the_first_stage_then_the_walk_without_repeats() {
        let out = merge(
            TraverseFirstStage::Append,
            &exact(&["x", "y", "x", "z"]),
            &walk(&["y", "a", "b", "c"]),
            10,
            2,
        );
        // x, y, z once each (first place wins); the walk's y is skipped and the
        // walk adds top_k = 2 further cards.
        assert_eq!(nodes(&out), ["e:x", "e:y", "e:z", "w:a", "w:b"]);
    }

    #[test]
    fn append_cuts_the_first_stage_to_first_k_and_zero_is_no_cut() {
        let list = exact(&["x", "y", "z"]);
        let w = walk(&["a"]);
        let cut = merge(TraverseFirstStage::Append, &list, &w, 2, 5);
        assert_eq!(nodes(&cut), ["e:x", "e:y", "w:a"]);
        let all = merge(TraverseFirstStage::Append, &list, &w, 0, 5);
        assert_eq!(nodes(&all), ["e:x", "e:y", "e:z", "w:a"]);
        // A node past the cut may come back as the walk's own card.
        let back = merge(TraverseFirstStage::Append, &list, &walk(&["z"]), 2, 5);
        assert_eq!(nodes(&back), ["e:x", "e:y", "w:z"]);
    }

    #[test]
    fn interleave_alternates_from_the_first_stage_and_cuts_to_top_k() {
        let out = merge(
            TraverseFirstStage::Interleave,
            &exact(&["x", "y", "z"]),
            &walk(&["a", "b", "c", "d"]),
            10,
            5,
        );
        assert_eq!(nodes(&out), ["e:x", "w:a", "e:y", "w:b", "e:z"]);
        let all = merge(
            TraverseFirstStage::Interleave,
            &exact(&["x"]),
            &walk(&["a", "b", "c"]),
            10,
            40,
        );
        // Once the first stage is used up the walk goes on.
        assert_eq!(nodes(&all), ["e:x", "w:a", "w:b", "w:c"]);
        let lone = merge(
            TraverseFirstStage::Interleave,
            &[],
            &walk(&["a", "b"]),
            10,
            40,
        );
        assert_eq!(nodes(&lone), ["w:a", "w:b"]);
    }

    #[test]
    fn interleave_lists_a_node_once_and_honours_first_k() {
        let out = merge(
            TraverseFirstStage::Interleave,
            &exact(&["x", "y", "z"]),
            &walk(&["x", "a", "y", "b"]),
            2,
            10,
        );
        // The walk's x and y are the first stage's; z is past first_k.
        assert_eq!(nodes(&out), ["e:x", "w:a", "e:y", "w:b"]);
    }

    #[test]
    fn origins_are_counted_over_the_cards_returned() {
        let out = merge(
            TraverseFirstStage::Append,
            &[card("x", "exact"), card("s", "seed")],
            &walk(&["a", "b"]),
            10,
            1,
        );
        assert_eq!(
            origin_counts(&out),
            json!({"exact": 1, "seeds": 1, "walk": 1})
        );
    }

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
            symbol: symbol.to_string(),
            roles,
            enclosing,
        }
    }

    /// a.rs: `struct Foo` (line 1), `fn alpha` uses Foo once (line 2), `fn beta`
    /// twice (line 3).
    fn impact() -> ImpactMap {
        let (foo, alpha, beta) = (sym("a/Foo#"), sym("a/alpha()."), sym("a/beta()."));
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 7, 0, 10), &foo, 1, Some((0, 0, 0, 14))),
                occ((1, 3, 1, 8), &alpha, 1, Some((1, 0, 1, 30))),
                occ((1, 12, 1, 15), &foo, 0, None),
                occ((2, 3, 2, 7), &beta, 1, Some((2, 0, 2, 40))),
                occ((2, 12, 2, 15), &foo, 0, None),
                occ((2, 20, 2, 23), &foo, 0, None),
            ],
        }];
        let kinds: HashMap<String, Kind> = [
            (foo, Kind::Struct),
            (alpha, Kind::Function),
            (beta, Kind::Function),
        ]
        .into_iter()
        .collect();
        ImpactMap::new(
            ImpactIndex::build(docs, &kinds, None),
            &|_| None,
            ImpactOptions::default(),
        )
    }

    #[test]
    fn exact_closure_abstains_with_a_note_when_nothing_named_resolves() {
        let map = impact();
        let none = closure_cards(&map, &|_| false, "What breaks if the parser changes?");
        assert!(none.cards.is_empty());
        assert_eq!(none.note, "exact.closure: names 0, anchored no");
        let unknown = closure_cards(&map, &|_| false, "What breaks if `Nowhere` changes?");
        assert!(unknown.cards.is_empty());
        assert_eq!(unknown.note, "exact.closure: names 1, anchored no");
    }

    #[test]
    fn exact_closure_cards_carry_a_site_and_merge_ahead_of_the_walk() {
        let map = impact();
        let found = closure_cards(
            &map,
            &|s| s.ends_with("beta()."),
            "What breaks if `Foo` changes?",
        );
        assert_eq!(
            found.note,
            "exact.closure: names 1, anchored yes, closure 2, kept 2"
        );
        // beta holds two references, alpha one.
        let lines: Vec<u64> = found
            .cards
            .iter()
            .map(|c| c.card["site"]["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, [3, 2]);
        assert_eq!(found.cards[0].card["site"]["path"], "a.rs");
        assert_eq!(found.cards[0].card["site"]["col"], 4);
        assert_eq!(found.cards[0].card["held"], 2);
        assert_eq!(found.cards[0].card["origin"], "exact");
        assert_eq!(found.cards[0].card["via"], EXACT_CLOSURE_ID);
        assert!(found.cards[0].in_graph && !found.cards[1].in_graph);
        // The node is the id scip.impact gives the symbol.
        assert_eq!(
            found.cards[0].node,
            *map.node_of(&sym("a/beta().")).unwrap()
        );

        // The merge: exact cards first, then the walk's cards, none twice.
        let first: Vec<Value> = found.cards.iter().map(|c| c.card.clone()).collect();
        let walk_nodes = [
            found.cards[1].node.to_string(),
            "file:src/other.rs".to_owned(),
        ];
        let walk: Vec<Value> = walk_nodes.iter().map(|n| walk_card(n)).collect();
        let out = merge(TraverseFirstStage::Append, &first, &walk, 10, 5);
        let listed: Vec<&str> = out.iter().map(|c| node_of(c).unwrap()).collect();
        assert_eq!(
            listed,
            [
                found.cards[0].node.to_string().as_str(),
                found.cards[1].node.to_string().as_str(),
                "file:src/other.rs"
            ]
        );
        assert_eq!(
            origin_counts(&out),
            json!({"exact": 2, "seeds": 0, "walk": 1})
        );
        assert_eq!(out[0]["site"]["line"], 3);
    }

    /// Answers `git log` calls from a script.
    struct Fake(Vec<Result<Vec<String>, String>>, Cell<usize>);

    impl GitLog for Fake {
        fn log(&self, _args: &[String]) -> Result<Vec<String>, String> {
            let at = self.1.get();
            self.1.set(at + 1);
            self.0.get(at).cloned().unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    #[test]
    fn exact_pickaxe_counts_git_calls_and_abstains_when_nothing_is_found() {
        let sha = "a".repeat(40);
        let git = Fake(vec![Ok(vec![sha.clone()])], Cell::new(0));
        let found = pickaxe_cards(&git, "Which commit first created `Foo`?", 10);
        assert_eq!(found.cards.len(), 1);
        assert_eq!(found.cards[0].node.to_string(), format!("commit:{sha}"));
        assert_eq!(found.cards[0].card["origin"], "exact");
        assert_eq!(found.git_calls, 1);
        assert_eq!(
            found.note,
            "exact.pickaxe: class H1, 1 commits (git calls 1)"
        );

        let empty = pickaxe_cards(
            &Fake(Vec::new(), Cell::new(0)),
            "Which commit first created `Foo`?",
            10,
        );
        assert!(empty.cards.is_empty());
        // The -G call finds nothing, then the -S call is made.
        assert_eq!(empty.git_calls, 2);
        assert!(
            empty
                .note
                .starts_with("exact.pickaxe: class H1, no commits"),
            "{}",
            empty.note
        );

        let failing = Fake(vec![Err("timeout after 20s".into())], Cell::new(0));
        let failed = pickaxe_cards(&failing, "Which commit first created `Foo`?", 10);
        assert!(
            failed.note.contains("failed 1: timeout after 20s"),
            "{}",
            failed.note
        );

        // Out of time: the refused calls are not git calls, and the note says so.
        let late = Fake(
            vec![Ok(Vec::new()), Err(STAGE_TIME_SPENT.into())],
            Cell::new(0),
        );
        let stopped = pickaxe_cards(&late, "Which commit first created `Foo`?", 10);
        assert_eq!(stopped.git_calls, 1);
        assert!(stopped.cards.is_empty());
        assert!(
            stopped
                .note
                .contains("stopped: the first stage's time ran out")
                && !stopped.note.contains("failed"),
            "{}",
            stopped.note
        );

        let nothing = pickaxe_cards(&Fake(Vec::new(), Cell::new(0)), "?", 10);
        assert_eq!(nothing.git_calls, 0);
        assert_eq!(
            nothing.note,
            "exact.pickaxe: class H3, no commits (git calls 0)"
        );
    }

    #[test]
    fn a_budgeted_git_refuses_a_call_once_its_time_has_passed() {
        let git = BudgetedGit {
            git: PathBuf::from("git"),
            tree: PathBuf::from("."),
            per_call: Duration::from_secs(20),
            end: Instant::now(),
        };
        assert_eq!(git.log(&[]), Err(STAGE_TIME_SPENT.to_owned()));
    }

    #[test]
    fn seeds_become_cards_with_their_origin_and_rank() {
        let node = NodeId::file("src/a.rs").unwrap();
        let seeds = [Seed {
            node: node.clone(),
            map: Some("seeds.hybrid"),
        }];
        let details = [json!({"node": node.to_string(), "hit_rank": 3})];
        let cards = seed_cards(&seeds, &details);
        assert_eq!(cards[0]["origin"], "seed");
        assert_eq!(cards[0]["seed_rank"], 3);
        assert_eq!(cards[0]["via"], "seeds.hybrid");
    }
}
