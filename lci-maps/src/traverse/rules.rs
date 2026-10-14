//! The declarative rule list: rung 1 of the steering ladder (spine brief
//! 1.5).
//!
//! An agent writes rules as JSON; [`RuleList::from_json`] parses them into a
//! closed AST (no eval, no scripts). Bounds: at most [`MAX_RULES`] rules,
//! predicates nested at most [`MAX_NESTING`] deep, and an evaluation time of
//! at most [`EVAL_CAP`] per stop (the list stops looking at nodes when the
//! time is up and says so). The only actions are those of [`Action`]; none
//! raises a budget, and the engine clamps what they ask for.
//!
//! ```json
//! [
//!   {"when": {"path_glob": "tests/**"}, "then": "prune"},
//!   {"when": {"all": [{"witness": ["compiler"]}, {"depth": {"le": 2}}]},
//!    "then": {"boost": 2.0}},
//!   {"when": {"budget_left": {"lt": 10}}, "then": "stop"}
//! ]
//! ```
//!
//! Predicates (one key each): `kind` (`file`, `chunk`, `sym`, `commit`,
//! `test`, `cfg`, `doc`, `str`), `path_glob`, `witness` (list of classes),
//! `map`, `score`, `fanout`, `depth`, `budget_left` (percent of the pop
//! budget; each takes `{"lt"|"le"|"eq"|"ge"|"gt": n}`), `seen_before`
//! (`true`), `all`, `any`, `not`. Actions: `continue`, `prune`, `expand`,
//! `stop` (strings) and `{"boost": f}`, `{"map_on": id}`, `{"map_off": id}`,
//! `{"map_weight": {"id": id, "weight": w}}`, `{"rerank": "witness"|"depth"|
//! "fanout"}`, `{"redirect": [node ids]}`, `{"emit": note}`.
//!
//! First match per frontier node wins among the node actions (`continue`,
//! `prune`, `expand`, `boost`). The other actions are checkpoint rules: each
//! fires once when its predicate holds for some frontier node (or, with an
//! empty frontier, for the checkpoint alone), in list order.
//!
//! As rung 1 of the ladder the list is confident (1.0) when some rule matched
//! and uncertain (0.0) when none did, when it ran out of time, or when two
//! rules matching one node contradict each other (one prunes it, the other
//! expands it or boosts it above 1); the first match is still the one it
//! proposes. There is no rule for `budget`: a rule list cannot raise a cap.

use crate::edge::WitnessClass;
use crate::traverse::handler::{
    Action, BOOST_MAX, Checkpoint, Decision, Directive, EMIT_MAX, FrontierItem, Handler,
    REDIRECT_MAX, RerankBy, StopBudget,
};
use lci_core::node::{NodeId, NodeKind};
use serde_json::Value;
use std::{
    str::FromStr,
    time::{Duration, Instant},
};

pub const MAX_RULES: usize = 64;
pub const MAX_NESTING: usize = 8;
/// The most children of one `all` or `any`.
pub const MAX_CHILDREN: usize = 16;
pub const MAX_GLOB: usize = 128;
/// The evaluation time of one stop.
pub const EVAL_CAP: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl Cmp {
    fn holds<T: PartialOrd>(self, value: T, bound: T) -> bool {
        match self {
            Self::Lt => value < bound,
            Self::Le => value <= bound,
            Self::Eq => value == bound,
            Self::Ge => value >= bound,
            Self::Gt => value > bound,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Kind(NodeKind),
    PathGlob(String),
    Witness(Vec<WitnessClass>),
    Map(String),
    Score(Cmp, f64),
    Fanout(Cmp, u32),
    Depth(Cmp, u32),
    BudgetLeft(Cmp, u32),
    SeenBefore,
    All(Vec<Predicate>),
    Any(Vec<Predicate>),
    Not(Box<Predicate>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub when: Predicate,
    pub then: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    NotAnArray,
    /// More than [`MAX_RULES`] rules.
    TooManyRules(usize),
    UnknownPredicate {
        rule: usize,
        name: String,
    },
    UnknownAction {
        rule: usize,
        name: String,
    },
    Invalid {
        rule: usize,
        reason: String,
    },
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnArray => write!(f, "the rules are not a JSON array"),
            Self::TooManyRules(n) => write!(f, "{n} rules; at most {MAX_RULES} are allowed"),
            Self::UnknownPredicate { rule, name } => {
                write!(f, "rule {rule}: unknown predicate {name:?}")
            }
            Self::UnknownAction { rule, name } => write!(f, "rule {rule}: unknown action {name:?}"),
            Self::Invalid { rule, reason } => write!(f, "rule {rule}: {reason}"),
        }
    }
}

impl std::error::Error for RuleError {}

/// A parse failure before the rule number is known.
enum Fail {
    Predicate(String),
    Action(String),
    Invalid(String),
}

impl Fail {
    fn at(self, rule: usize) -> RuleError {
        match self {
            Self::Predicate(name) => RuleError::UnknownPredicate { rule, name },
            Self::Action(name) => RuleError::UnknownAction { rule, name },
            Self::Invalid(reason) => RuleError::Invalid { rule, reason },
        }
    }
}

fn invalid<T>(reason: impl Into<String>) -> Result<T, Fail> {
    Err(Fail::Invalid(reason.into()))
}

/// The one `(key, value)` of a single-key object.
fn single(value: &Value) -> Result<(&str, &Value), Fail> {
    match value.as_object() {
        Some(map) if map.len() == 1 => {
            let (key, value) = map.iter().next().expect("one entry");
            Ok((key, value))
        }
        _ => invalid("expected an object with exactly one key"),
    }
}

fn parse_cmp(value: &Value) -> Result<(Cmp, &Value), Fail> {
    let (op, bound) = single(value)?;
    let cmp = match op {
        "lt" => Cmp::Lt,
        "le" => Cmp::Le,
        "eq" => Cmp::Eq,
        "ge" => Cmp::Ge,
        "gt" => Cmp::Gt,
        other => return invalid(format!("unknown comparison {other:?}")),
    };
    Ok((cmp, bound))
}

fn parse_count(value: &Value, max: u32) -> Result<(Cmp, u32), Fail> {
    let (cmp, bound) = parse_cmp(value)?;
    match bound.as_u64().and_then(|n| u32::try_from(n).ok()) {
        Some(n) if n <= max => Ok((cmp, n)),
        _ => invalid(format!("expected a whole number up to {max}")),
    }
}

fn parse_witness(name: &str) -> Option<WitnessClass> {
    [
        WitnessClass::Compiler,
        WitnessClass::Lsp,
        WitnessClass::Exact,
        WitnessClass::Text,
        WitnessClass::Vector,
        WitnessClass::Heuristic,
        WitnessClass::Agent,
    ]
    .into_iter()
    .find(|class| class.as_str() == name)
}

fn parse_predicate(value: &Value, depth: usize) -> Result<Predicate, Fail> {
    if depth > MAX_NESTING {
        return invalid(format!("predicates nest deeper than {MAX_NESTING}"));
    }
    let (name, body) = single(value)?;
    let children = |body: &Value| -> Result<Vec<Predicate>, Fail> {
        let list = body
            .as_array()
            .filter(|list| !list.is_empty() && list.len() <= MAX_CHILDREN);
        let Some(list) = list else {
            return invalid(format!("expected 1 to {MAX_CHILDREN} predicates"));
        };
        list.iter().map(|p| parse_predicate(p, depth + 1)).collect()
    };
    Ok(match name {
        "kind" => {
            let text = body.as_str().unwrap_or_default();
            match NodeKind::ALL.into_iter().find(|k| k.prefix() == text) {
                Some(kind) => Predicate::Kind(kind),
                None => return invalid(format!("unknown node kind {text:?}")),
            }
        }
        "path_glob" => match body.as_str() {
            Some(glob) if !glob.is_empty() && glob.len() <= MAX_GLOB => {
                Predicate::PathGlob(glob.to_string())
            }
            _ => return invalid(format!("expected a glob of 1 to {MAX_GLOB} characters")),
        },
        "witness" => {
            let list = body.as_array().filter(|list| !list.is_empty());
            let Some(list) = list else {
                return invalid("expected a non-empty list of witness classes");
            };
            let classes: Option<Vec<_>> = list
                .iter()
                .map(|name| name.as_str().and_then(parse_witness))
                .collect();
            match classes {
                Some(classes) => Predicate::Witness(classes),
                None => return invalid("unknown witness class"),
            }
        }
        "map" => match body.as_str() {
            Some(id) if !id.is_empty() => Predicate::Map(id.to_string()),
            _ => return invalid("expected a map id"),
        },
        "score" => {
            let (cmp, bound) = parse_cmp(body)?;
            match bound.as_f64().filter(|n| n.is_finite()) {
                Some(n) => Predicate::Score(cmp, n),
                None => return invalid("expected a finite number"),
            }
        }
        "fanout" => {
            let (cmp, n) = parse_count(body, u32::MAX)?;
            Predicate::Fanout(cmp, n)
        }
        "depth" => {
            let (cmp, n) = parse_count(body, u32::MAX)?;
            Predicate::Depth(cmp, n)
        }
        "budget_left" => {
            let (cmp, n) = parse_count(body, 100)?;
            Predicate::BudgetLeft(cmp, n)
        }
        "seen_before" => match body.as_bool() {
            Some(true) => Predicate::SeenBefore,
            _ => return invalid("seen_before takes true; use not for the opposite"),
        },
        "all" => Predicate::All(children(body)?),
        "any" => Predicate::Any(children(body)?),
        "not" => Predicate::Not(Box::new(parse_predicate(body, depth + 1)?)),
        other => return Err(Fail::Predicate(other.to_string())),
    })
}

fn parse_action(value: &Value) -> Result<Action, Fail> {
    if let Some(name) = value.as_str() {
        return match name {
            "continue" => Ok(Action::Continue),
            "prune" => Ok(Action::Prune),
            "expand" => Ok(Action::Expand),
            "stop" => Ok(Action::Stop),
            other => Err(Fail::Action(other.to_string())),
        };
    }
    let (name, body) =
        single(value).or_else(|_| invalid("an action is a string or a one-key object"))?;
    let id = |body: &Value| match body.as_str() {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => invalid("expected a map id"),
    };
    Ok(match name {
        "boost" => match body.as_f64() {
            Some(f) if f.is_finite() && (0.0..=BOOST_MAX).contains(&f) => Action::Boost(f),
            _ => return invalid(format!("boost takes a number from 0 to {BOOST_MAX}")),
        },
        "map_on" => Action::MapOn(id(body)?),
        "map_off" => Action::MapOff(id(body)?),
        "map_weight" => {
            let weight = body["weight"].as_f64();
            match (id(&body["id"]), weight) {
                (Ok(id), Some(w)) if (0.0..=1.0).contains(&w) => Action::MapWeight(id, w),
                _ => return invalid("map_weight takes an id and a weight from 0 to 1"),
            }
        }
        "rerank" => Action::Rerank(match body.as_str() {
            Some("witness") => RerankBy::Witness,
            Some("depth") => RerankBy::Depth,
            Some("fanout") => RerankBy::Fanout,
            _ => return invalid("rerank takes witness, depth or fanout"),
        }),
        "redirect" => {
            let list = body
                .as_array()
                .filter(|list| !list.is_empty() && list.len() <= REDIRECT_MAX);
            let Some(list) = list else {
                return invalid(format!("redirect takes 1 to {REDIRECT_MAX} node ids"));
            };
            let nodes: Result<Vec<NodeId>, _> = list
                .iter()
                .map(|n| NodeId::from_str(n.as_str().unwrap_or_default()))
                .collect();
            match nodes {
                Ok(nodes) => Action::Redirect(nodes),
                Err(error) => return invalid(error.to_string()),
            }
        }
        "emit" => match body.as_str() {
            Some(note) if note.chars().count() <= EMIT_MAX => Action::Emit(note.to_string()),
            _ => return invalid(format!("emit takes a note of up to {EMIT_MAX} characters")),
        },
        other => return Err(Fail::Action(other.to_string())),
    })
}

/// Parses one rule-list action (`"prune"`, `{"boost": 2.0}`, ..).
pub fn parse_action_json(value: &Value) -> Result<Action, String> {
    parse_action(value).map_err(|fail| match fail {
        Fail::Predicate(name) | Fail::Action(name) => format!("unknown action {name:?}"),
        Fail::Invalid(reason) => reason,
    })
}

/// Whether `text` matches `pattern`: `?` is one character other than `/`,
/// `*` any run of characters other than `/`, `**` any run, `**/` any run
/// ending in `/` or nothing. Everything else matches itself. Linear in the
/// product of the lengths.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Tok {
        Lit(char),
        One,
        Star,
        Deep,
        DeepSlash,
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '?' => tokens.push(Tok::One),
            '*' => {
                let mut j = i;
                while chars.get(j) == Some(&'*') {
                    j += 1;
                }
                if j - i == 1 {
                    tokens.push(Tok::Star);
                } else if chars.get(j) == Some(&'/') {
                    tokens.push(Tok::DeepSlash);
                    j += 1;
                } else {
                    tokens.push(Tok::Deep);
                }
                i = j;
                continue;
            }
            c => tokens.push(Tok::Lit(c)),
        }
        i += 1;
    }
    let text: Vec<char> = text.chars().collect();
    // `now[j]`: the tokens so far match `text[..j]`.
    let mut now = vec![false; text.len() + 1];
    now[0] = true;
    for token in tokens {
        let mut next = vec![false; text.len() + 1];
        match token {
            Tok::Lit(c) => {
                for j in 1..=text.len() {
                    next[j] = now[j - 1] && text[j - 1] == c;
                }
            }
            Tok::One => {
                for j in 1..=text.len() {
                    next[j] = now[j - 1] && text[j - 1] != '/';
                }
            }
            Tok::Star => {
                for j in 0..=text.len() {
                    next[j] = now[j] || (j > 0 && next[j - 1] && text[j - 1] != '/');
                }
            }
            Tok::Deep => {
                for j in 0..=text.len() {
                    next[j] = now[j] || (j > 0 && next[j - 1]);
                }
            }
            Tok::DeepSlash => {
                // Nothing, or anything up to and including a `/`.
                let mut any_before = false;
                for j in 0..=text.len() {
                    if j > 0 {
                        any_before |= now[j - 1];
                    }
                    next[j] = now[j] || (j > 0 && text[j - 1] == '/' && any_before);
                }
            }
        }
        now = next;
    }
    now[text.len()]
}

/// The path a glob is matched against: the file path of a file, chunk or
/// doc section, a symbol's module path joined with `/`; none for the rest.
pub fn node_path(node: &NodeId) -> Option<String> {
    match node {
        NodeId::Symbol(key) => Some(key.module_path.join("/")),
        other => other.path().map(str::to_string),
    }
}

struct Cx<'a> {
    checkpoint: &'a Checkpoint,
    item: Option<&'a FrontierItem>,
}

fn eval(predicate: &Predicate, cx: &Cx) -> bool {
    let on_item = |test: &dyn Fn(&FrontierItem) -> bool| cx.item.is_some_and(test);
    match predicate {
        Predicate::Kind(kind) => on_item(&|i| i.node.kind() == *kind),
        Predicate::PathGlob(glob) => {
            on_item(&|i| node_path(&i.node).is_some_and(|p| glob_match(glob, &p)))
        }
        Predicate::Witness(classes) => {
            on_item(&|i| i.witness.is_some_and(|w| classes.contains(&w)))
        }
        Predicate::Map(id) => on_item(&|i| i.via == Some(id.as_str())),
        Predicate::Score(cmp, bound) => on_item(&|i| cmp.holds(i.score, *bound)),
        Predicate::Fanout(cmp, bound) => on_item(&|i| cmp.holds(i.fanout, *bound)),
        Predicate::Depth(cmp, bound) => on_item(&|i| cmp.holds(i.depth, *bound)),
        Predicate::BudgetLeft(cmp, bound) => cmp.holds(cx.checkpoint.budget_left_pct, *bound),
        Predicate::SeenBefore => on_item(&|i| i.seen_before),
        Predicate::All(list) => list.iter().all(|p| eval(p, cx)),
        Predicate::Any(list) => list.iter().any(|p| eval(p, cx)),
        Predicate::Not(inner) => !eval(inner, cx),
    }
}

/// Whether two node actions on one node pull opposite ways: one drops it,
/// the other expands it or raises its priority.
fn contradicts(a: &Action, b: &Action) -> bool {
    let drops = |x: &Action| matches!(x, Action::Prune);
    let wants = |x: &Action| match x {
        Action::Expand => true,
        Action::Boost(factor) => *factor > 1.0,
        _ => false,
    };
    (drops(a) && wants(b)) || (wants(a) && drops(b))
}

/// Rung 1 of the ladder: a bounded list of rules.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleList {
    rules: Vec<Rule>,
    eval_cap: Duration,
}

impl RuleList {
    pub fn new(rules: Vec<Rule>) -> Result<Self, RuleError> {
        if rules.len() > MAX_RULES {
            return Err(RuleError::TooManyRules(rules.len()));
        }
        Ok(Self {
            rules,
            eval_cap: EVAL_CAP,
        })
    }

    /// Parses `[{"when": .., "then": ..}, ..]`.
    pub fn from_json(value: &Value) -> Result<Self, RuleError> {
        let list = value.as_array().ok_or(RuleError::NotAnArray)?;
        if list.len() > MAX_RULES {
            return Err(RuleError::TooManyRules(list.len()));
        }
        let mut rules = Vec::with_capacity(list.len());
        for (at, raw) in list.iter().enumerate() {
            let rule = (|| {
                let Some(object) = raw.as_object() else {
                    return invalid("a rule is an object");
                };
                if let Some(extra) = object.keys().find(|k| *k != "when" && *k != "then") {
                    return invalid(format!("unknown rule key {extra:?}"));
                }
                let (Some(when), Some(then)) = (object.get("when"), object.get("then")) else {
                    return invalid("a rule needs when and then");
                };
                Ok(Rule {
                    when: parse_predicate(when, 1)?,
                    then: parse_action(then)?,
                })
            })();
            rules.push(rule.map_err(|fail: Fail| fail.at(at))?);
        }
        Self::new(rules)
    }

    /// A shorter evaluation time than [`EVAL_CAP`]; a longer one is ignored.
    pub fn with_eval_cap(mut self, cap: Duration) -> Self {
        self.eval_cap = cap.min(EVAL_CAP);
        self
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

impl Handler for RuleList {
    fn id(&self) -> &str {
        "rules"
    }

    fn decide(&self, checkpoint: &Checkpoint, budget: &StopBudget) -> Decision {
        let started = Instant::now();
        let cap = self.eval_cap.min(budget.max_eval).min(EVAL_CAP);
        let expired = || started.elapsed() >= cap;
        let mut decision = Decision::default();
        let mut matched = false;
        let mut conflicts = 0u32;

        'nodes: for item in &checkpoint.frontier {
            if expired() {
                decision.truncated = true;
                break;
            }
            let cx = Cx {
                checkpoint,
                item: Some(item),
            };
            // The first match decides; a later match that contradicts it
            // makes the whole list uncertain (it still applies the first).
            let mut first: Option<&Action> = None;
            for rule in self.rules.iter().filter(|r| r.then.is_node_action()) {
                if !eval(&rule.when, &cx) {
                    continue;
                }
                match first {
                    None => {
                        matched = true;
                        first = Some(&rule.then);
                        decision.actions.push(Directive {
                            node: Some(item.node.clone()),
                            action: rule.then.clone(),
                        });
                    }
                    Some(decided) if contradicts(decided, &rule.then) => {
                        conflicts += 1;
                        continue 'nodes;
                    }
                    Some(_) => {}
                }
            }
        }

        for rule in self.rules.iter().filter(|r| !r.then.is_node_action()) {
            let hit = if checkpoint.frontier.is_empty() {
                eval(
                    &rule.when,
                    &Cx {
                        checkpoint,
                        item: None,
                    },
                )
            } else {
                let mut hit = false;
                for item in &checkpoint.frontier {
                    if expired() {
                        break;
                    }
                    let cx = Cx {
                        checkpoint,
                        item: Some(item),
                    };
                    if eval(&rule.when, &cx) {
                        hit = true;
                        break;
                    }
                }
                hit
            };
            if expired() && !hit {
                decision.truncated = true;
                break;
            }
            if hit {
                matched = true;
                decision.actions.push(Directive {
                    node: None,
                    action: rule.then.clone(),
                });
            }
        }

        decision.confidence = if matched && !decision.truncated && conflicts == 0 {
            1.0
        } else {
            0.0
        };
        decision.cost = started.elapsed();
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(path: &str) -> NodeId {
        NodeId::file(path).unwrap()
    }

    fn item(path: &str, depth: u32) -> FrontierItem {
        FrontierItem {
            node: file(path),
            via: Some("scip"),
            witness: Some(WitnessClass::Compiler),
            score: 0.5,
            potential: 0.5,
            depth,
            fanout: 3,
            seen_before: false,
            est_cost: 0,
        }
    }

    fn checkpoint(items: Vec<FrontierItem>) -> Checkpoint {
        Checkpoint {
            pops: 4,
            stop: 1,
            budget_left_pct: 60,
            tokens_left: 1000,
            frontier: items,
            visited: 4,
            cleared: 0,
            ..Checkpoint::default()
        }
    }

    fn parse(value: Value) -> RuleList {
        RuleList::from_json(&value).expect("parses")
    }

    fn decide(list: &RuleList, cp: &Checkpoint) -> Decision {
        list.decide(cp, &StopBudget::default())
    }

    fn actions(decision: &Decision) -> Vec<(Option<String>, &'static str)> {
        decision
            .actions
            .iter()
            .map(|d| (d.node.as_ref().map(|n| n.to_string()), d.action.name()))
            .collect()
    }

    #[test]
    fn glob_cases() {
        assert!(glob_match("tests/**", "tests/a/b.rs"));
        assert!(glob_match("tests/**", "tests/"));
        assert!(!glob_match("tests/**", "src/tests/a.rs"));
        assert!(glob_match("**/tests/**", "src/tests/a.rs"));
        assert!(glob_match("**/tests/**", "tests/a.rs"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/a/lib.rs"));
        assert!(glob_match("src/**.rs", "src/a/lib.rs"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("a", ""));
    }

    #[test]
    fn glob_is_not_exponential() {
        let text = "a".repeat(2000);
        assert!(!glob_match("*a*a*a*a*a*a*a*a*b", &text));
    }

    #[test]
    fn parse_rejects_unknown_predicates_and_actions() {
        let err = RuleList::from_json(&json!([{"when": {"exec": "ls"}, "then": "prune"}]));
        assert_eq!(
            err.unwrap_err(),
            RuleError::UnknownPredicate {
                rule: 0,
                name: "exec".into()
            }
        );
        let err = RuleList::from_json(&json!([
            {"when": {"depth": {"gt": 1}}, "then": "prune"},
            {"when": {"depth": {"gt": 1}}, "then": {"budget": 100}}
        ]));
        assert_eq!(
            err.unwrap_err(),
            RuleError::UnknownAction {
                rule: 1,
                name: "budget".into()
            }
        );
        // A nested unknown predicate is found too.
        let err = RuleList::from_json(&json!([{"when": {"not": {"shell": 1}}, "then": "stop"}]));
        assert!(matches!(
            err.unwrap_err(),
            RuleError::UnknownPredicate { .. }
        ));
    }

    #[test]
    fn parse_rejects_malformed_rules() {
        for bad in [
            json!({"when": {"seen_before": true}, "then": "stop"}),
            json!([{"when": {"seen_before": true}}]),
            json!([{"when": {"seen_before": true}, "then": "stop", "extra": 1}]),
            json!([{"when": {"depth": {"gt": -1}}, "then": "stop"}]),
            json!([{"when": {"depth": {"gt": 1, "lt": 5}}, "then": "stop"}]),
            json!([{"when": {"budget_left": {"gt": 101}}, "then": "stop"}]),
            json!([{"when": {"kind": "banana"}, "then": "stop"}]),
            json!([{"when": {"all": []}, "then": "stop"}]),
            json!([{"when": {"seen_before": true}, "then": {"boost": 9.0}}]),
            json!([{"when": {"seen_before": true}, "then": {"map_weight": {"id": "x", "weight": 1.5}}}]),
            json!([{"when": {"seen_before": true}, "then": {"redirect": ["not a node id"]}}]),
        ] {
            assert!(RuleList::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parse_bounds_nesting() {
        let mut deep = json!({"seen_before": true});
        for _ in 0..MAX_NESTING {
            deep = json!({"not": deep});
        }
        assert!(RuleList::from_json(&json!([{"when": deep, "then": "stop"}])).is_err());
        let mut ok = json!({"seen_before": true});
        for _ in 0..MAX_NESTING - 2 {
            ok = json!({"not": ok});
        }
        assert!(RuleList::from_json(&json!([{"when": ok, "then": "stop"}])).is_ok());
    }

    #[test]
    fn the_65th_rule_is_refused() {
        let rule = json!({"when": {"seen_before": true}, "then": "continue"});
        let sixty_four: Vec<Value> = (0..MAX_RULES).map(|_| rule.clone()).collect();
        assert_eq!(
            RuleList::from_json(&Value::Array(sixty_four.clone()))
                .unwrap()
                .rules()
                .len(),
            64
        );
        let mut sixty_five = sixty_four;
        sixty_five.push(rule);
        assert_eq!(
            RuleList::from_json(&Value::Array(sixty_five)).unwrap_err(),
            RuleError::TooManyRules(65)
        );
        let rules = vec![
            Rule {
                when: Predicate::SeenBefore,
                then: Action::Continue,
            };
            65
        ];
        assert_eq!(
            RuleList::new(rules).unwrap_err(),
            RuleError::TooManyRules(65)
        );
    }

    #[test]
    fn each_node_action_applies_to_the_matching_nodes_only() {
        let cp = checkpoint(vec![
            item("tests/a.rs", 1),
            item("src/b.rs", 1),
            item("src/c.rs", 4),
        ]);
        let list = parse(json!([
            {"when": {"path_glob": "tests/**"}, "then": "prune"},
            {"when": {"depth": {"gt": 3}}, "then": {"boost": 2.0}},
            {"when": {"path_glob": "src/b.rs"}, "then": "expand"}
        ]));
        let decision = decide(&list, &cp);
        assert_eq!(
            actions(&decision),
            [
                (Some("file:tests/a.rs".into()), "prune"),
                (Some("file:src/b.rs".into()), "expand"),
                (Some("file:src/c.rs".into()), "boost"),
            ]
        );
        assert_eq!(decision.confidence, 1.0);
        assert!(!decision.truncated);
    }

    #[test]
    fn first_matching_rule_wins_per_node() {
        let cp = checkpoint(vec![item("src/a.rs", 1)]);
        let list = parse(json!([
            {"when": {"depth": {"ge": 1}}, "then": "continue"},
            {"when": {"depth": {"ge": 1}}, "then": "prune"}
        ]));
        assert_eq!(
            actions(&decide(&list, &cp)),
            [(Some("file:src/a.rs".into()), "continue")]
        );
    }

    #[test]
    fn checkpoint_rules_fire_once_in_list_order() {
        let cp = checkpoint(vec![item("src/a.rs", 1), item("src/b.rs", 2)]);
        let list = parse(json!([
            {"when": {"depth": {"ge": 1}}, "then": {"rerank": "depth"}},
            {"when": {"budget_left": {"lt": 90}}, "then": {"map_off": "lexical"}},
            {"when": {"budget_left": {"lt": 10}}, "then": "stop"},
            {"when": {"seen_before": true}, "then": {"emit": "never"}}
        ]));
        let decision = decide(&list, &cp);
        assert_eq!(actions(&decision), [(None, "rerank"), (None, "map_off")]);
    }

    #[test]
    fn predicates_combine_and_read_the_item() {
        let mut seen = item("src/a.rs", 2);
        seen.seen_before = true;
        seen.via = Some("lexical");
        seen.witness = Some(WitnessClass::Text);
        let cp = checkpoint(vec![seen, item("src/b.rs", 2)]);
        let list = parse(json!([
            {"when": {"all": [{"seen_before": true}, {"map": "lexical"}, {"witness": ["text", "vector"]}]},
             "then": "prune"},
            {"when": {"any": [{"kind": "file"}, {"kind": "sym"}]}, "then": {"boost": 0.5}},
            {"when": {"not": {"score": {"gt": 0.9}}}, "then": "continue"}
        ]));
        assert_eq!(
            actions(&decide(&list, &cp)),
            [
                (Some("file:src/a.rs".into()), "prune"),
                (Some("file:src/b.rs".into()), "boost"),
            ]
        );
    }

    #[test]
    fn a_list_that_matches_nothing_is_not_confident() {
        let cp = checkpoint(vec![item("src/a.rs", 1)]);
        let list = parse(json!([{"when": {"path_glob": "docs/**"}, "then": "prune"}]));
        let decision = decide(&list, &cp);
        assert!(decision.actions.is_empty());
        assert_eq!(decision.confidence, 0.0);
    }

    #[test]
    fn no_rule_can_raise_the_budget() {
        // There is no budget action to parse ...
        for name in ["budget", "add_pops", "raise", "extend", "max_pops"] {
            let rule = json!([{"when": {"seen_before": true}, "then": {name: 100}}]);
            assert!(
                matches!(
                    RuleList::from_json(&rule),
                    Err(RuleError::UnknownAction { .. })
                ),
                "{name}"
            );
        }
        // ... and the actions that parse never carry a value above their cap.
        let rule = json!([{"when": {"seen_before": true}, "then": {"boost": 1e9}}]);
        assert!(RuleList::from_json(&rule).is_err());
    }

    #[test]
    fn the_evaluation_cap_is_enforced() {
        let items: Vec<FrontierItem> = (0..500).map(|i| item(&format!("src/{i}.rs"), 1)).collect();
        let cp = checkpoint(items);
        let list = parse(json!([{"when": {"depth": {"ge": 0}}, "then": "continue"}]));
        // A zero cap evaluates nothing and says so.
        let zero = StopBudget {
            max_eval: Duration::ZERO,
        };
        let decision = list.decide(&cp, &zero);
        assert!(decision.truncated);
        assert!(decision.actions.is_empty());
        assert_eq!(decision.confidence, 0.0);
        let decision = list
            .clone()
            .with_eval_cap(Duration::ZERO)
            .decide(&cp, &StopBudget::default());
        assert!(decision.truncated);
        // A cap above the 5 ms bound is ignored.
        let long = list.with_eval_cap(Duration::from_secs(60));
        assert_eq!(long.eval_cap, EVAL_CAP);
        let long_budget = StopBudget {
            max_eval: Duration::from_secs(60),
        };
        let decision = long.decide(&cp, &long_budget);
        assert!(decision.cost <= EVAL_CAP + Duration::from_millis(50));
    }

    #[test]
    fn redirect_parses_node_ids() {
        let list = parse(json!([
            {"when": {"budget_left": {"lt": 50}}, "then": {"redirect": ["file:src/a.rs"]}}
        ]));
        assert_eq!(
            list.rules()[0].then,
            Action::Redirect(vec![file("src/a.rs")])
        );
    }
}
