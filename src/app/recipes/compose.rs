//! `lci_compose`: the thin baseline arm of the N3 A/B. Up to six operator
//! steps in one call, `$sN.field` references between them, no model call and
//! no recipe. It exists so a recipe cannot win only by wrapping several calls
//! in one.
//!
//! Operators (each reads the SCIP index or the config-key map, nothing else):
//!
//! | op | inputs | outputs |
//! | --- | --- | --- |
//! | `seeds.scip_names` | `names` (or `name`) | `symbols` |
//! | `exact.closure` | `symbols` (or `symbol`) | `symbols`: declarations referencing any input, one hop |
//! | `scip.impact` | `symbols`, `depth` 1-3 | `symbols`: declarations reaching an input within `depth` hops, nearest first, with `depths` |
//! | `graph.scip` | `symbols`, `depth` 1-4 | `symbols`: functions called within `depth` hops, nearest first, with `depths` |
//! | `config.key` | `keys` (or `key`) or `symbols` | `symbols` (readers of the keys) or `keys` (keys the symbols touch) |
//! | `set.union`, `set.intersect`, `set.minus` | `of`: references | the operand type; `minus` is the first minus the rest |
//! | `pick` | `from`, `role` (`prod`/`test`), `path_glob`, `name_glob`, `top` | `symbols` |
//!
//! A reference is `$sN.symbols`, `$sN.keys` or a bare `$sN` (the step's main
//! output). A name that matches several declarations stops the call with
//! `ambiguous_anchor` and its candidates. `counts.operator_calls` follows the
//! rule in the header of the parent module (one per node an operator expands).

use super::*;

/// Most steps in one call.
const MAX_STEPS: usize = 6;
/// Most nodes one step may hold.
const MAX_SET: usize = 2000;
/// Nodes of a step a response lists.
const SHOWN: usize = 40;

/// `lci_compose`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComposeArgs {
    /// The workspace the SCIP index and `config.example.toml` belong to.
    pub workspace_path: String,
    /// 1 to 6 steps, run in order. A step reads only earlier steps.
    pub steps: Vec<StepArgs>,
}

/// One step of `lci_compose`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StepArgs {
    /// The step's name: letters, digits and `_` (for example `s1`).
    pub id: String,
    /// `seeds.scip_names`, `exact.closure`, `scip.impact`, `graph.scip`,
    /// `config.key`, `set.union`, `set.intersect`, `set.minus` or `pick`.
    pub op: String,
    /// The operator's inputs: literals, or references such as `$s1.symbols`.
    #[serde(rename = "in", default)]
    pub inputs: serde_json::Map<String, Value>,
}

impl ComposeArgs {
    pub(super) fn uses_keys(&self) -> bool {
        self.steps.iter().any(|step| step.op == "config.key")
    }
}

/// The inputs each operator accepts.
fn allowed(op: &str) -> Option<&'static [&'static str]> {
    Some(match op {
        "seeds.scip_names" => &["names", "name"],
        "exact.closure" => &["symbols", "symbol"],
        "scip.impact" | "graph.scip" => &["symbols", "symbol", "depth"],
        "config.key" => &["keys", "key", "symbols", "symbol"],
        "set.union" | "set.intersect" | "set.minus" => &["of"],
        "pick" => &["from", "role", "path_glob", "name_glob", "top"],
        _ => return None,
    })
}

enum Fail {
    /// The request is wrong: the caller gets an error.
    Bad(anyhow::Error),
    /// The data ended the call: the caller gets a stop.
    Stop(Box<Halt>),
}

impl From<anyhow::Error> for Fail {
    fn from(error: anyhow::Error) -> Self {
        Self::Bad(error)
    }
}

type R<T> = std::result::Result<T, Fail>;

fn bad<T>(message: String) -> R<T> {
    Err(Fail::Bad(anyhow!(message)))
}

/// What a step produced.
#[derive(Clone, Default)]
struct Out {
    symbols: Vec<usize>,
    keys: Vec<String>,
    /// Hops from the inputs, parallel to `symbols`, when the operator has them.
    depths: Vec<u32>,
}

impl Out {
    fn size(&self) -> usize {
        self.symbols.len() + self.keys.len()
    }
}

enum Items {
    Symbols(Vec<usize>),
    Keys(Vec<String>),
}

struct Env<'a> {
    sources: &'a Sources,
    done: Vec<(String, Out)>,
}

impl Env<'_> {
    /// `$sN`, `$sN.symbols` or `$sN.keys`.
    fn reference(&self, text: &str) -> R<Items> {
        let body = &text[1..];
        let (id, field) = match body.split_once('.') {
            Some((id, field)) => (id, Some(field)),
            None => (body, None),
        };
        let Some((_, out)) = self.done.iter().find(|(done, _)| done == id) else {
            return bad(format!(
                "reference {text}: no earlier step is called {id:?}; the earlier steps are {}",
                self.done
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        match field {
            Some("symbols") => Ok(Items::Symbols(out.symbols.clone())),
            Some("keys") => Ok(Items::Keys(out.keys.clone())),
            None if out.symbols.is_empty() && !out.keys.is_empty() => {
                Ok(Items::Keys(out.keys.clone()))
            }
            None => Ok(Items::Symbols(out.symbols.clone())),
            Some(other) => bad(format!(
                "reference {text}: a step has the fields symbols and keys, not {other:?}"
            )),
        }
    }

    /// Declarations from a name, a reference, or a list of either.
    fn symbols(&self, step: &str, value: &Value) -> R<Vec<usize>> {
        let mut out = Vec::new();
        let list: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for item in list {
            let Some(text) = item.as_str() else {
                return bad(format!(
                    "step {step}: symbols are names or references, not {item}"
                ));
            };
            if text.starts_with('$') {
                match self.reference(text)? {
                    Items::Symbols(found) => out.extend(found),
                    Items::Keys(_) => {
                        return bad(format!(
                            "step {step}: {text} holds keys, symbols are needed"
                        ));
                    }
                }
            } else {
                let (def, _) = self.sources.anchor(text, step).map_err(Fail::Stop)?;
                out.push(def);
            }
        }
        Ok(dedupe(out))
    }

    /// Keys from a key name, a reference, or a list of either.
    fn keys(&self, step: &str, value: &Value) -> R<Vec<String>> {
        let mut out = Vec::new();
        let list: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for item in list {
            let Some(text) = item.as_str() else {
                return bad(format!(
                    "step {step}: keys are names or references, not {item}"
                ));
            };
            if text.starts_with('$') {
                match self.reference(text)? {
                    Items::Keys(found) => out.extend(found),
                    Items::Symbols(_) => {
                        return bad(format!(
                            "step {step}: {text} holds symbols, keys are needed"
                        ));
                    }
                }
                continue;
            }
            match self.sources.resolve_key(text) {
                KeyResolution::One(key, _) => out.push(key),
                KeyResolution::Many(ids) => {
                    return Err(Fail::Stop(Box::new(Halt {
                        anchor: json!({"given": text, "resolved": null, "candidates": ids}),
                        stop: Stop::at(step, "ambiguous_anchor", true),
                    })));
                }
                KeyResolution::Missing(like) => {
                    return Err(Fail::Stop(Box::new(Halt {
                        anchor: json!({"given": text, "resolved": null, "candidates": like,
                                       "why": format!("no key of {KEY_FILE} has that name")}),
                        stop: Stop::at(step, "empty", false),
                    })));
                }
            }
        }
        let mut seen = HashSet::new();
        out.retain(|key| seen.insert(key.clone()));
        Ok(out)
    }
}

fn dedupe(mut list: Vec<usize>) -> Vec<usize> {
    let mut seen = HashSet::new();
    list.retain(|item| seen.insert(*item));
    list
}

/// `*` matches any run of characters, `?` one character.
pub(super) fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

fn depth_of(step: &StepArgs, default: u32, max: u32) -> R<u32> {
    match step.inputs.get("depth") {
        None => Ok(default),
        Some(value) => match value.as_u64() {
            Some(depth) if (1..=u64::from(max)).contains(&depth) => Ok(depth as u32),
            _ => bad(format!("step {}: depth must be 1 through {max}", step.id)),
        },
    }
}

fn capped(step: &str, what: &str, held: usize) -> Fail {
    Fail::Stop(Box::new(Halt {
        anchor: Value::Null,
        stop: Stop::at(step, "fanout_cap", true).unexplored(
            json!({what: format!("more than {MAX_SET} nodes; the step was not completed"),
                               "held": held}),
        ),
    }))
}

/// Declarations reaching `start` within `depth` hops, nearest first.
fn impact_reach(
    sources: &Sources,
    step: &str,
    start: &[usize],
    depth: u32,
    calls: &mut u64,
) -> R<Out> {
    let ix = sources.ix();
    let mut seen: HashSet<usize> = start.iter().copied().collect();
    let mut frontier: Vec<usize> = start.to_vec();
    let mut out = Out::default();
    for hop in 1..=depth {
        let mut next: BTreeMap<(String, u32, usize), usize> = BTreeMap::new();
        for &def in &frontier {
            *calls += 1;
            for user in ix.closure(&ix.defs[def].symbol) {
                if seen.insert(user) {
                    let d = &ix.defs[user];
                    next.insert((d.path.clone(), d.name_range.0, user), user);
                }
            }
        }
        frontier = next.into_values().collect();
        out.depths.extend(std::iter::repeat_n(hop, frontier.len()));
        out.symbols.extend(frontier.iter().copied());
        if out.symbols.len() > MAX_SET {
            return Err(capped(step, "scip.impact", out.symbols.len()));
        }
    }
    Ok(out)
}

/// Functions called within `depth` hops of `start`, nearest first, and the
/// number of nodes expanded (the breadth-first pops below `depth`).
fn callee_reach(sources: &Sources, start: &[usize], depth: u32) -> (Out, u64) {
    let graph = sources.scip.graph();
    let roots: Vec<usize> = start
        .iter()
        .filter_map(|&def| sources.graph_node(def))
        .collect();
    let mut dist: HashMap<usize, u32> = roots.iter().map(|&n| (n, 0)).collect();
    let mut queue: VecDeque<usize> = roots.iter().copied().collect();
    let mut pops = 0;
    while let Some(node) = queue.pop_front() {
        let here = dist[&node];
        if here == depth {
            continue;
        }
        pops += 1;
        for edge in &graph.out[node] {
            dist.entry(edge.to).or_insert_with(|| {
                queue.push_back(edge.to);
                here + 1
            });
        }
    }
    let mut reached: Vec<(u32, String, usize)> = dist
        .into_iter()
        .filter(|(_, d)| *d > 0)
        .map(|(node, d)| (d, graph.nodes[node].name.clone(), node))
        .collect();
    reached.sort();
    let mut out = Out::default();
    for (d, _, node) in reached {
        if let Some(def) = sources.def_of_graph(node) {
            out.symbols.push(def);
            out.depths.push(d);
        }
    }
    (out, pops)
}

async fn execute(env: &Env<'_>, step: &StepArgs, calls: &mut u64) -> R<(Out, usize)> {
    let sources = env.sources;
    let id = step.id.as_str();
    let input = |names: &[&str]| names.iter().find_map(|name| step.inputs.get(*name));
    match step.op.as_str() {
        "seeds.scip_names" => {
            let Some(names) = input(&["names", "name"]) else {
                return bad(format!("step {id}: seeds.scip_names needs `names`"));
            };
            let symbols = env.symbols(id, names)?;
            *calls += 1;
            Ok((
                Out {
                    symbols: symbols.clone(),
                    ..Out::default()
                },
                symbols.len(),
            ))
        }
        "exact.closure" => {
            let Some(value) = input(&["symbols", "symbol"]) else {
                return bad(format!("step {id}: exact.closure needs `symbols`"));
            };
            let start = env.symbols(id, value)?;
            let ix = sources.ix();
            let mut found: BTreeMap<(String, u32, usize), usize> = BTreeMap::new();
            for &def in &start {
                *calls += 1;
                for user in ix.closure(&ix.defs[def].symbol) {
                    let d = &ix.defs[user];
                    found.insert((d.path.clone(), d.name_range.0, user), user);
                }
            }
            if found.len() > MAX_SET {
                return Err(capped(id, "exact.closure", found.len()));
            }
            Ok((
                Out {
                    symbols: found.into_values().collect(),
                    ..Out::default()
                },
                start.len(),
            ))
        }
        "scip.impact" => {
            let Some(value) = input(&["symbols", "symbol"]) else {
                return bad(format!("step {id}: scip.impact needs `symbols`"));
            };
            let start = env.symbols(id, value)?;
            let depth = depth_of(step, 1, 3)?;
            let out = impact_reach(sources, id, &start, depth, calls)?;
            Ok((out, start.len()))
        }
        "graph.scip" => {
            let Some(value) = input(&["symbols", "symbol"]) else {
                return bad(format!("step {id}: graph.scip needs `symbols`"));
            };
            let start = env.symbols(id, value)?;
            let depth = depth_of(step, 1, 4)?;
            let (out, pops) = callee_reach(sources, &start, depth);
            *calls += pops;
            if out.symbols.len() > MAX_SET {
                return Err(capped(id, "graph.scip", out.symbols.len()));
            }
            Ok((out, start.len()))
        }
        "config.key" => {
            if let Some(value) = input(&["keys", "key"]) {
                let keys = env.keys(id, value)?;
                let mut seen = HashSet::new();
                let mut symbols = Vec::new();
                for key in &keys {
                    *calls += 1;
                    for reader in sources.key_readers(key).await? {
                        if let Some(def) = reader.def
                            && seen.insert(def)
                        {
                            symbols.push(def);
                        }
                    }
                }
                Ok((
                    Out {
                        symbols,
                        ..Out::default()
                    },
                    keys.len(),
                ))
            } else if let Some(value) = input(&["symbols", "symbol"]) {
                let start = env.symbols(id, value)?;
                let mut seen = HashSet::new();
                let mut keys = Vec::new();
                for &def in &start {
                    *calls += 1;
                    for key in keys_of_def(sources, def).await? {
                        if seen.insert(key.clone()) {
                            keys.push(key);
                        }
                    }
                }
                Ok((
                    Out {
                        keys,
                        ..Out::default()
                    },
                    start.len(),
                ))
            } else {
                bad(format!("step {id}: config.key needs `keys` or `symbols`"))
            }
        }
        op @ ("set.union" | "set.intersect" | "set.minus") => {
            let Some(Value::Array(operands)) = input(&["of"]) else {
                return bad(format!("step {id}: {op} needs `of`, a list of references"));
            };
            let need = if op == "set.union" { 1 } else { 2 };
            if operands.len() < need {
                return bad(format!(
                    "step {id}: {op} needs at least {need} operands in `of`"
                ));
            }
            let mut sets = Vec::new();
            for operand in operands {
                let Some(text) = operand.as_str().filter(|text| text.starts_with('$')) else {
                    return bad(format!(
                        "step {id}: the operands of {op} are references such as $s1.symbols"
                    ));
                };
                sets.push(env.reference(text)?);
            }
            let n_in = operands.len();
            let combined = match &sets[0] {
                Items::Symbols(_) => {
                    let lists = sets
                        .iter()
                        .map(|set| match set {
                            Items::Symbols(list) => Ok(list.clone()),
                            Items::Keys(_) => {
                                bad(format!("step {id}: operands mix symbols and keys"))
                            }
                        })
                        .collect::<R<Vec<_>>>()?;
                    Out {
                        symbols: combine(op, &lists),
                        ..Out::default()
                    }
                }
                Items::Keys(_) => {
                    let lists = sets
                        .iter()
                        .map(|set| match set {
                            Items::Keys(list) => Ok(list.clone()),
                            Items::Symbols(_) => {
                                bad(format!("step {id}: operands mix symbols and keys"))
                            }
                        })
                        .collect::<R<Vec<_>>>()?;
                    Out {
                        keys: combine(op, &lists),
                        ..Out::default()
                    }
                }
            };
            Ok((combined, n_in))
        }
        "pick" => {
            let Some(from) = input(&["from"])
                .and_then(Value::as_str)
                .filter(|t| t.starts_with('$'))
            else {
                return bad(format!(
                    "step {id}: pick needs `from`, a reference such as $s1.symbols"
                ));
            };
            let Items::Symbols(symbols) = env.reference(from)? else {
                return bad(format!(
                    "step {id}: pick filters symbols, {from} holds keys"
                ));
            };
            let role = match step.inputs.get("role").map(|v| v.as_str()) {
                None => None,
                Some(Some(r @ ("prod" | "test"))) => Some(r),
                Some(_) => return bad(format!("step {id}: role is \"prod\" or \"test\"")),
            };
            let path_glob = step.inputs.get("path_glob").and_then(Value::as_str);
            let name_glob = step.inputs.get("name_glob").and_then(Value::as_str);
            let top = match step.inputs.get("top") {
                None => usize::MAX,
                Some(v) => match v.as_u64() {
                    Some(n) if n >= 1 => n as usize,
                    _ => return bad(format!("step {id}: top is a positive integer")),
                },
            };
            let ix = sources.ix();
            let n_in = symbols.len();
            let kept: Vec<usize> = symbols
                .into_iter()
                .filter(|&def| {
                    let decl = ix.decl(def);
                    role.is_none_or(|r| sources.role(def) == r)
                        && path_glob.is_none_or(|g| glob_match(g, &decl.path))
                        && name_glob.is_none_or(|g| glob_match(g, &decl.name))
                })
                .take(top)
                .collect();
            Ok((
                Out {
                    symbols: kept,
                    ..Out::default()
                },
                n_in,
            ))
        }
        other => bad(format!("step {id}: unknown op {other:?}")),
    }
}

fn combine<T: Clone + Eq + std::hash::Hash>(op: &str, lists: &[Vec<T>]) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    match op {
        "set.union" => {
            let mut seen = HashSet::new();
            for item in lists.iter().flatten() {
                if seen.insert(item.clone()) {
                    out.push(item.clone());
                }
            }
        }
        "set.intersect" => {
            let others: Vec<HashSet<&T>> = lists[1..].iter().map(|l| l.iter().collect()).collect();
            let mut seen = HashSet::new();
            for item in &lists[0] {
                if others.iter().all(|set| set.contains(item)) && seen.insert(item.clone()) {
                    out.push(item.clone());
                }
            }
        }
        _ => {
            let removed: HashSet<&T> = lists[1..].iter().flatten().collect();
            let mut seen = HashSet::new();
            for item in &lists[0] {
                if !removed.contains(item) && seen.insert(item.clone()) {
                    out.push(item.clone());
                }
            }
        }
    }
    out
}

fn rows_of(sources: &Sources, step: &str, out: &Out, cap: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = out
        .symbols
        .iter()
        .enumerate()
        .take(cap)
        .map(|(i, &def)| {
            let mut row = sources.row(def, step, &[step]);
            row.depth = out.depths.get(i).copied();
            row
        })
        .collect();
    rows.extend(
        out.keys
            .iter()
            .take(cap.saturating_sub(rows.len()))
            .map(|key| key_row(sources, key, step, &[step])),
    );
    rows
}

fn check(args: &ComposeArgs) -> anyhow::Result<()> {
    if args.steps.is_empty() || args.steps.len() > MAX_STEPS {
        bail!("steps must hold 1 through {MAX_STEPS} steps");
    }
    let mut ids = HashSet::new();
    for step in &args.steps {
        let valid = !step.id.is_empty()
            && step
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            bail!("step id {:?}: letters, digits and _ only", step.id);
        }
        if !ids.insert(step.id.as_str()) {
            bail!("step id {:?} is used twice", step.id);
        }
        let Some(inputs) = allowed(&step.op) else {
            bail!(
                "step {}: unknown op {:?}; the ops are seeds.scip_names, exact.closure, scip.impact, graph.scip, config.key, set.union, set.intersect, set.minus, pick",
                step.id,
                step.op
            );
        };
        if let Some(extra) = step
            .inputs
            .keys()
            .find(|key| !inputs.contains(&key.as_str()))
        {
            bail!(
                "step {}: {} has no input {extra:?}; its inputs are {}",
                step.id,
                step.op,
                inputs.join(", ")
            );
        }
    }
    Ok(())
}

pub(super) async fn run(
    sources: &Sources,
    args: &ComposeArgs,
    started: Instant,
) -> anyhow::Result<Value> {
    check(args)?;
    let mut run = Run::default();
    let mut env = Env {
        sources,
        done: Vec::new(),
    };
    let mut stop = Stop::complete();
    let mut anchor = Value::Null;
    for step in &args.steps {
        let at = Instant::now();
        match execute(&env, step, &mut run.calls).await {
            Ok((out, n_in)) => {
                run.seen += out.size() as u64;
                run.step(
                    &step.id,
                    &step.op,
                    Value::Object(step.inputs.clone()),
                    n_in,
                    out.size(),
                    at,
                );
                env.done.push((step.id.clone(), out));
            }
            Err(Fail::Bad(error)) => return Err(error),
            Err(Fail::Stop(halted)) => {
                anchor = halted.anchor;
                stop = halted.stop;
                break;
            }
        }
    }
    let resolved: serde_json::Map<String, Value> = env
        .done
        .iter()
        .map(|(id, out)| {
            let rows = rows_of(sources, id, out, SHOWN);
            let listed: Vec<Value> = rows
                .iter()
                .map(|row| json!(format!("{} {}:{}", row.name, row.path, row.line)))
                .collect();
            (id.clone(), json!({"n": out.size(), "listed": listed}))
        })
        .collect();
    if let Some((id, last)) = env.done.last()
        && last.size() > SHOWN
    {
        stop.cap(id, json!({"results_not_listed": last.size() - SHOWN}));
    }
    if let Some((id, last)) = env.done.last() {
        run.rows = rows_of(sources, id, last, SHOWN);
    }
    Ok(finish(
        "lci_compose",
        anchor,
        run,
        stop,
        started,
        Some(("resolved", Value::Object(resolved))),
    ))
}
