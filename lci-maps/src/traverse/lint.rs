//! Steer lint: checks on a rule list and a stop policy that their types make
//! decidable before the run. Each finding is a sentence for the response's
//! `warnings`; none is an error, the run proceeds.
//!
//! Checked: a rule that prunes every node; a rule that cannot fire (its match
//! never holds, it names a map that is unknown or not among the call's maps,
//! or an earlier rule that always matches decides every node first); two
//! rules with one match and opposite actions; a stop trigger the pop budget
//! cannot reach (`stop_every` above the pops) and a stop count of zero.
//!
//! Not checked, because the answer depends on the walk or on data the types do
//! not hold: whether `on_disagreement` or `likely_complete` is ever reached,
//! whether a `path_glob`, `kind`, `witness` or `score` bound matches any node
//! a map offers, whether `redirect` ids exist.

use super::{
    handler::Action,
    rules::{Cmp, Predicate, Rule, contradicts},
    stop::StopPolicy,
};

/// What the lint knows about the call.
pub struct LintContext<'a> {
    /// The pop budget after the caps.
    pub pops: u32,
    /// The depth at which states are recorded, not expanded.
    pub max_depth: u32,
    /// Every map id the server knows.
    pub known_maps: &'a [&'a str],
    /// The maps this call uses.
    pub selected_maps: &'a [&'a str],
}

/// The findings, in rule order, then the stop findings.
pub fn lint(rules: &[Rule], stops: &StopPolicy, cx: &LintContext<'_>) -> Vec<String> {
    let mut found = Vec::new();
    // The first node rule whose match always holds: it decides every node
    // that reaches it, so a later node rule only takes part in contradictions.
    let first_always = rules
        .iter()
        .position(|r| r.then.is_node_action() && constant(&r.when) == Some(true));
    for (at, rule) in rules.iter().enumerate() {
        let earlier = &rules[..at];
        let node_rule = rule.then.is_node_action();
        let always = constant(&rule.when) == Some(true);
        if let Some(why) = cannot_match(&rule.when, cx) {
            found.push(format!("rule {at} cannot fire: {why}"));
        } else if let Some(why) = action_map_problem(&rule.then, cx) {
            found.push(format!("rule {at} cannot act: {why}"));
        } else if let Some(first) = first_always.filter(|first| node_rule && *first < at) {
            found.push(format!(
                "rule {at} cannot fire: rule {first} matches every node and decides first"
            ));
        }
        if node_rule && always && matches!(rule.then, Action::Prune) {
            let who = if earlier.iter().any(|e| e.then.is_node_action()) {
                "every node no earlier rule decides"
            } else {
                "every node"
            };
            found.push(format!("rule {at} prunes {who}: its match always holds"));
        }
        if node_rule {
            for (first, other) in earlier.iter().enumerate() {
                let overlap =
                    other.when == rule.when || constant(&other.when) == Some(true) || always;
                if other.then.is_node_action() && overlap && contradicts(&other.then, &rule.then) {
                    found.push(format!(
                        "rules {first} and {at} contradict each other: {} and {} on the same nodes",
                        other.then.name(),
                        rule.then.name()
                    ));
                }
            }
        }
    }
    if stops.is_active() {
        if stops.max_stops == 0 {
            found.push("max_stops is 0: no stop fires and the rules are never consulted".into());
        }
        if stops.every_pops > cx.pops {
            found.push(format!(
                "stop_every {} is above the pop budget {}: the periodic stop never fires",
                stops.every_pops, cx.pops
            ));
        }
    }
    found
}

/// Whether a predicate holds for every node (`Some(true)`) or for none
/// (`Some(false)`), when its type says so: a count against a bound no count
/// crosses. `budget_left` is a percent, 0 to 100.
fn constant(predicate: &Predicate) -> Option<bool> {
    let count = |cmp: Cmp, bound: u32, max: Option<u32>| match cmp {
        Cmp::Ge if bound == 0 => Some(true),
        Cmp::Lt if bound == 0 => Some(false),
        Cmp::Le if max.is_some_and(|max| bound >= max) => Some(true),
        Cmp::Gt if max.is_some_and(|max| bound >= max) => Some(false),
        _ => None,
    };
    match predicate {
        Predicate::Fanout(cmp, bound) | Predicate::Depth(cmp, bound) => count(*cmp, *bound, None),
        Predicate::BudgetLeft(cmp, bound) => count(*cmp, *bound, Some(100)),
        Predicate::All(list) => {
            let parts: Vec<_> = list.iter().map(constant).collect();
            if parts.contains(&Some(false)) {
                Some(false)
            } else {
                parts.iter().all(|p| *p == Some(true)).then_some(true)
            }
        }
        Predicate::Any(list) => {
            let parts: Vec<_> = list.iter().map(constant).collect();
            if parts.contains(&Some(true)) {
                Some(true)
            } else {
                parts.iter().all(|p| *p == Some(false)).then_some(false)
            }
        }
        Predicate::Not(inner) => constant(inner).map(|holds| !holds),
        _ => None,
    }
}

/// Why a predicate can never hold, if it can be told.
fn cannot_match(predicate: &Predicate, cx: &LintContext<'_>) -> Option<String> {
    if constant(predicate) == Some(false) {
        return Some("its match never holds".into());
    }
    match predicate {
        Predicate::Map(id) => map_problem(id, cx).map(|why| format!("map {id:?} {why}")),
        Predicate::Depth(Cmp::Gt, bound) if *bound >= cx.max_depth => {
            Some(format!("no node is deeper than max_depth {}", cx.max_depth))
        }
        Predicate::Depth(Cmp::Ge | Cmp::Eq, bound) if *bound > cx.max_depth => {
            Some(format!("no node is deeper than max_depth {}", cx.max_depth))
        }
        Predicate::All(list) => list.iter().find_map(|p| cannot_match(p, cx)),
        Predicate::Any(list) => {
            let reasons: Vec<_> = list.iter().map(|p| cannot_match(p, cx)).collect();
            if reasons.iter().all(Option::is_some) {
                reasons.into_iter().next().flatten()
            } else {
                None
            }
        }
        _ => None,
    }
}

fn map_problem(id: &str, cx: &LintContext<'_>) -> Option<&'static str> {
    if !cx.known_maps.contains(&id) {
        Some("is not a known map")
    } else if !cx.selected_maps.contains(&id) {
        Some("is not among the maps of this call")
    } else {
        None
    }
}

fn action_map_problem(action: &Action, cx: &LintContext<'_>) -> Option<String> {
    match action {
        Action::MapOn(id) | Action::MapOff(id) | Action::MapWeight(id, _) => {
            map_problem(id, cx).map(|why| format!("map {id:?} {why}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traverse::rules::RuleList;
    use serde_json::{Value, json};

    const MAPS: [&str; 3] = ["graph.scip", "structure.contains", "grep.live"];

    fn rules(value: Value) -> Vec<Rule> {
        RuleList::from_json(&value).unwrap().rules().to_vec()
    }

    fn lint_with(value: Value, stops: StopPolicy) -> Vec<String> {
        let cx = LintContext {
            pops: 100,
            max_depth: 6,
            known_maps: &MAPS,
            selected_maps: &MAPS[..2],
        };
        lint(&rules(value), &stops, &cx)
    }

    #[test]
    fn each_check_fires_on_a_minimal_bad_input() {
        let periodic = StopPolicy::every(10);
        let cases = [
            (
                json!([{"when": {"depth": {"ge": 0}}, "then": "prune"}]),
                periodic,
                "rule 0 prunes every node: its match always holds",
            ),
            (
                json!([
                    {"when": {"kind": "test"}, "then": "continue"},
                    {"when": {"not": {"fanout": {"lt": 0}}}, "then": "prune"},
                ]),
                periodic,
                "rule 1 prunes every node no earlier rule decides",
            ),
            (
                json!([{"when": {"map": "nope"}, "then": "prune"}]),
                periodic,
                "rule 0 cannot fire: map \"nope\" is not a known map",
            ),
            (
                json!([{"when": {"map": "grep.live"}, "then": "prune"}]),
                periodic,
                "rule 0 cannot fire: map \"grep.live\" is not among the maps of this call",
            ),
            (
                json!([{"when": {"depth": {"gt": 9}}, "then": "prune"}]),
                periodic,
                "rule 0 cannot fire: no node is deeper than max_depth 6",
            ),
            (
                json!([{"when": {"budget_left": {"gt": 100}}, "then": "stop"}]),
                periodic,
                "rule 0 cannot fire: its match never holds",
            ),
            (
                json!([{"when": {"kind": "test"}, "then": {"map_off": "nope"}}]),
                periodic,
                "rule 0 cannot act: map \"nope\" is not a known map",
            ),
            (
                json!([
                    {"when": {"budget_left": {"ge": 0}}, "then": "continue"},
                    {"when": {"kind": "test"}, "then": {"boost": 2.0}},
                ]),
                periodic,
                "rule 1 cannot fire: rule 0 matches every node and decides first",
            ),
            (
                json!([
                    {"when": {"kind": "test"}, "then": "prune"},
                    {"when": {"kind": "test"}, "then": "expand"},
                ]),
                periodic,
                "rules 0 and 1 contradict each other: prune and expand on the same nodes",
            ),
            (
                json!([]),
                StopPolicy::every(500),
                "stop_every 500 is above the pop budget 100: the periodic stop never fires",
            ),
            (
                json!([]),
                StopPolicy::every(10).max_stops(0),
                "max_stops is 0: no stop fires and the rules are never consulted",
            ),
        ];
        for (rules, stops, expected) in cases {
            let found = lint_with(rules.clone(), stops);
            assert!(
                found.iter().any(|line| line.starts_with(expected)),
                "{rules}: {found:?}"
            );
        }
    }

    #[test]
    fn sound_rules_and_stops_give_no_warning() {
        let sound = json!([
            {"when": {"kind": "test"}, "then": "prune"},
            {"when": {"all": [{"map": "graph.scip"}, {"depth": {"le": 2}}]}, "then": {"boost": 2.0}},
            {"when": {"any": [{"map": "grep.live"}, {"kind": "doc"}]}, "then": "expand"},
            {"when": {"budget_left": {"lt": 10}}, "then": "stop"},
            {"when": {"budget_left": {"ge": 0}}, "then": "continue"},
        ]);
        let stops = StopPolicy::every(20).max_stops(4).on_disagreement(5);
        assert_eq!(lint_with(sound, stops), Vec::<String>::new());
    }
}
