//! `flow` gold: multi-hop call paths. Primary metric is path recall (the
//! fraction of gold intermediates named; multi-path items take the best
//! alternate; config-flow items use the field plus its readers). Secondary:
//! Kendall tau over the gold nodes the answer names, exact-order rate,
//! spurious nodes per answer, and fan-out recall for dispatch items.
//!
//! An answer matches a gold node by its `Owner::name` / `module::name`, by
//! its bare name when that is unambiguous inside the item, or by
//! `file:line` of the definition (fan-out implementations too); case-insensitive,
//! backticks and a trailing `()` ignored. Nodes in `accept.ignorable_nodes`
//! are neither hits nor spurious.

use super::metrics::kendall_tau;
use super::{Answer, Converted, Dropped, ItemScore, arr_of, dedup_keep_order, str_of};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

struct Node {
    name: String,
    symbol: String,
    keys_file_line: String,
    bare_lower: String,
    file: String,
}

fn norm(s: &str) -> String {
    let s = s.trim().trim_matches('`').trim();
    s.strip_suffix("()").unwrap_or(s).to_lowercase()
}

fn node(v: &Value) -> Node {
    let name = str_of(v, "name").unwrap_or_default().to_string();
    let file = str_of(v, "file").unwrap_or_default().to_string();
    let line = v.get("line").and_then(Value::as_i64).unwrap_or(0);
    Node {
        bare_lower: name.rsplit("::").next().unwrap_or(&name).to_lowercase(),
        symbol: str_of(v, "symbol").unwrap_or_default().to_string(),
        keys_file_line: norm(&format!("{file}:{line}")),
        name,
        file,
    }
}

fn nodes(v: &Value, key: &str) -> Vec<Node> {
    arr_of(v, key).iter().map(node).collect()
}

fn keys(n: &Node, ambiguous: &BTreeSet<String>) -> BTreeSet<String> {
    let mut ks = BTreeSet::from([norm(&n.name), n.keys_file_line.clone()]);
    if !ambiguous.contains(&n.bare_lower) {
        ks.insert(norm(&n.bare_lower));
    }
    ks
}

/// The strings an answer offers for matching, normalised.
fn candidates(a: &Answer) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(n) = &a.name {
        out.push(norm(n));
    }
    if let (Some(p), Some(l)) = (&a.path, a.line) {
        out.push(norm(&format!("{p}:{l}")));
    }
    out
}

fn is_path_item(item: &Value) -> bool {
    item["gold"].get("hops").is_some()
}

pub fn score_item(item: &Value, answers: &[Answer]) -> ItemScore {
    let g = &item["gold"];
    let path_item = is_path_item(item);
    let (paths, ends): (Vec<Vec<Node>>, Vec<Node>) = if path_item {
        let mut paths = vec![nodes(g, "intermediates")];
        for alt in arr_of(&item["accept"], "alternates") {
            paths.push(nodes(alt, "intermediates"));
        }
        let ends = vec![node(&g["entry"]), node(&g["target"])];
        (paths, ends)
    } else {
        let mut p = vec![node(&g["field"])];
        p.extend(nodes(g, "readers"));
        (vec![p], Vec::new())
    };
    let everyone: Vec<&Node> = paths.iter().flatten().chain(ends.iter()).collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for n in &everyone {
        *counts.entry(n.bare_lower.as_str()).or_default() += 1;
    }
    let ambiguous: BTreeSet<String> = counts
        .into_iter()
        .filter(|(_, c)| *c > 1)
        .map(|(k, _)| k.to_string())
        .collect();
    let all_keys: Vec<(&Node, BTreeSet<String>)> =
        everyone.iter().map(|n| (*n, keys(n, &ambiguous))).collect();
    let ignorable: BTreeSet<String> = arr_of(&item["accept"], "ignorable_nodes")
        .iter()
        .map(node)
        .flat_map(|n| keys(&n, &BTreeSet::new()))
        .collect();

    let cands: Vec<Vec<String>> = answers.iter().map(candidates).collect();
    let matched: Vec<Option<&str>> = cands
        .iter()
        .map(|cs| {
            all_keys
                .iter()
                .find(|(_, ks)| cs.iter().any(|c| ks.contains(c)))
                .map(|(n, _)| n.symbol.as_str())
        })
        .collect();

    let mut best: Option<(f64, Option<f64>, bool)> = None;
    for p in &paths {
        let syms: Vec<&str> = p.iter().map(|n| n.symbol.as_str()).collect();
        let mut present: Vec<&str> = Vec::new();
        for m in matched.iter().flatten() {
            if syms.contains(m) && !present.contains(m) {
                present.push(m);
            }
        }
        let rec = if syms.is_empty() {
            1.0
        } else {
            present.len() as f64 / syms.len() as f64
        };
        let in_gold_order: Vec<&str> = syms
            .iter()
            .copied()
            .filter(|s| present.contains(s))
            .collect();
        // `present` is already in answer order and de-duplicated.
        let tau = if path_item {
            kendall_tau(&in_gold_order, &present)
        } else {
            None
        };
        let exact = path_item && rec >= 1.0 && present == syms;
        if best.as_ref().is_none_or(|b| rec > b.0) {
            best = Some((rec, tau, exact));
        }
    }
    let (path_recall, tau, exact) = best.unwrap_or((0.0, None, false));

    let mut spurious = cands
        .iter()
        .zip(&matched)
        .filter(|(cs, m)| m.is_none() && !cs.iter().any(|c| ignorable.contains(c)))
        .count();
    let mut fanout_recall = None;
    if let Some(fan) = g.get("fan_out").and_then(Value::as_array) {
        let fkeys: Vec<(String, BTreeSet<String>)> = fan
            .iter()
            .map(|f| {
                let name = str_of(f, "name").unwrap_or_default();
                let bare = name.rsplit("::").next().unwrap_or(name);
                let impl_type = str_of(f, "impl_type").unwrap_or_default();
                // The location of the definition too: a bench answer is a
                // path and line, as for the other gold nodes.
                (
                    str_of(f, "symbol").unwrap_or_default().to_string(),
                    BTreeSet::from([
                        norm(name),
                        norm(&format!("{impl_type}::{bare}")),
                        node(f).keys_file_line,
                    ]),
                )
            })
            .collect();
        let got: BTreeSet<&str> = fkeys
            .iter()
            .filter(|(_, ks)| cands.iter().flatten().any(|c| ks.contains(c)))
            .map(|(s, _)| s.as_str())
            .collect();
        fanout_recall = Some(if fkeys.is_empty() {
            1.0
        } else {
            got.len() as f64 / fkeys.len() as f64
        });
        spurious = cands
            .iter()
            .zip(&matched)
            .filter(|(cs, m)| {
                m.is_none()
                    && !cs.iter().any(|c| ignorable.contains(c))
                    && !cs
                        .iter()
                        .any(|c| fkeys.iter().any(|(_, ks)| ks.contains(c)))
            })
            .count();
    }

    let stratum = str_of(item, "stratum").unwrap_or_default();
    let headline = item["accept"]["analysis"]["headline"]
        .as_bool()
        .unwrap_or(false);
    ItemScore {
        metrics: vec![
            ("path_recall", Some(path_recall)),
            ("tau", tau),
            (
                "exact_order",
                path_item.then_some(if exact { 1.0 } else { 0.0 }),
            ),
            ("spurious", Some(spurious as f64)),
            ("fanout_recall", fanout_recall),
        ],
        dims: vec![
            (
                "stratum",
                stratum.split('/').next().unwrap_or("unknown").to_string(),
            ),
            ("headline", headline.to_string()),
        ],
    }
}

pub fn oracle(item: &Value) -> Vec<Value> {
    let g = &item["gold"];
    let mut names: Vec<String> = if is_path_item(item) {
        nodes(g, "intermediates")
            .into_iter()
            .map(|n| n.name)
            .collect()
    } else {
        let mut v = vec![node(&g["field"]).name];
        v.extend(nodes(g, "readers").into_iter().map(|n| n.name));
        v
    };
    names.extend(nodes(g, "fan_out").into_iter().map(|n| n.name));
    names.into_iter().map(|n| json!({ "name": n })).collect()
}

pub fn to_query(item: &Value) -> Result<Converted, Dropped> {
    let g = &item["gold"];
    let mut files: Vec<String> = Vec::new();
    if is_path_item(item) {
        files.extend(nodes(g, "intermediates").into_iter().map(|n| n.file));
        files.extend(nodes(g, "fan_out").into_iter().map(|n| n.file));
    } else {
        files.push(node(&g["field"]).file);
        files.extend(nodes(g, "readers").into_iter().map(|n| n.file));
    }
    let expected = dedup_keep_order(files.into_iter().filter(|f| !f.is_empty()));
    if expected.is_empty() {
        return Err(Dropped("no intermediate files"));
    }
    let stratum = str_of(item, "stratum").unwrap_or_default();
    let headline = item["accept"]["analysis"]["headline"]
        .as_bool()
        .unwrap_or(false);
    let mut tags = vec![format!(
        "stratum:{}",
        stratum.split('/').next().unwrap_or("unknown")
    )];
    if headline {
        tags.push("headline".to_string());
    }
    Ok(Converted {
        expected,
        preferred: Vec::new(),
        disfavored: Vec::new(),
        tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, file: &str, line: i64) -> Value {
        json!({"name": name, "file": file, "line": line, "symbol": format!("sym:{name}")})
    }

    fn path_item() -> Value {
        json!({
            "id": "flow-t1", "stratum": "named/len4",
            "accept": {"alternates": [], "analysis": {"headline": true}},
            "gold": {
                "entry": n("A::start", "a.rs", 1), "target": n("D::end", "d.rs", 4),
                "hops": [],
                "intermediates": [n("B::one", "b.rs", 2), n("C::two", "c.rs", 3)]
            }
        })
    }

    fn named(names: &[&str]) -> Vec<Answer> {
        names.iter().map(|n| Answer::parse_shorthand(n)).collect()
    }

    fn metric(s: &ItemScore, name: &str) -> Option<f64> {
        s.metrics.iter().find(|(m, _)| *m == name).unwrap().1
    }

    #[test]
    fn path_recall_counts_named_intermediates() {
        let item = path_item();
        let s = score_item(&item, &named(&["B::one", "`two()`"]));
        assert_eq!(metric(&s, "path_recall"), Some(1.0));
        assert_eq!(
            metric(&score_item(&item, &named(&["b.rs:2"])), "path_recall"),
            Some(0.5)
        );
        assert_eq!(metric(&score_item(&item, &[]), "path_recall"), Some(0.0));
    }

    #[test]
    fn order_metrics_and_spurious_nodes() {
        let item = path_item();
        let right = score_item(&item, &named(&["B::one", "C::two"]));
        assert_eq!(metric(&right, "tau"), Some(1.0));
        assert_eq!(metric(&right, "exact_order"), Some(1.0));
        let reversed = score_item(&item, &named(&["C::two", "B::one", "Nope::x"]));
        assert_eq!(metric(&reversed, "tau"), Some(-1.0));
        assert_eq!(metric(&reversed, "exact_order"), Some(0.0));
        assert_eq!(metric(&reversed, "spurious"), Some(1.0));
        // endpoints are known nodes: neither a recall hit nor spurious
        let ends = score_item(&item, &named(&["A::start", "D::end"]));
        assert_eq!(metric(&ends, "spurious"), Some(0.0));
        assert_eq!(metric(&ends, "path_recall"), Some(0.0));
    }

    #[test]
    fn config_and_dispatch_items_use_their_own_lists() {
        let config = json!({
            "id": "flow-t2", "stratum": "config-flow", "accept": {},
            "gold": {"field": n("Cfg::key", "cfg.rs", 1), "readers": [n("R::read", "r.rs", 2)]}
        });
        let s = score_item(&config, &named(&["Cfg::key", "R::read"]));
        assert_eq!(metric(&s, "path_recall"), Some(1.0));
        assert_eq!(metric(&s, "tau"), None);
        let mut dispatch = path_item();
        dispatch["gold"]["fan_out"] = json!([
            {"name": "X::go", "impl_type": "X", "symbol": "sym:x", "file": "x.rs"},
            {"name": "Y::go", "impl_type": "Y", "symbol": "sym:y", "file": "y.rs"}
        ]);
        let d = score_item(&dispatch, &named(&["X::go", "B::one"]));
        assert_eq!(metric(&d, "fanout_recall"), Some(0.5));
        assert_eq!(metric(&d, "spurious"), Some(0.0));
    }

    #[test]
    fn a_fan_out_impl_answered_by_its_location_counts_and_is_not_spurious() {
        // The bench answers by path and line of the definition, never by name.
        let mut dispatch = path_item();
        dispatch["gold"]["fan_out"] = json!([
            {"name": "X::go", "impl_type": "X", "symbol": "sym:x", "file": "x.rs", "line": 7},
            {"name": "Y::go", "impl_type": "Y", "symbol": "sym:y", "file": "y.rs", "line": 9}
        ]);
        let located =
            |path: &str, line: u64| Answer::from_value(&json!({"path": path, "line": line}));
        let run = vec![located("x.rs", 7).unwrap(), located("b.rs", 2).unwrap()];
        let d = score_item(&dispatch, &run);
        assert_eq!(metric(&d, "fanout_recall"), Some(0.5));
        assert_eq!(metric(&d, "spurious"), Some(0.0));
        // A location that is no gold node at all is still spurious.
        let stray = vec![located("z.rs", 1).unwrap()];
        assert_eq!(
            metric(&score_item(&dispatch, &stray), "spurious"),
            Some(1.0)
        );
    }

    #[test]
    fn ignorable_nodes_are_not_spurious_and_oracle_is_perfect() {
        let mut item = path_item();
        item["accept"]["ignorable_nodes"] = json!([n("H::helper", "h.rs", 9)]);
        let s = score_item(&item, &named(&["H::helper"]));
        assert_eq!(metric(&s, "spurious"), Some(0.0));
        let run: Vec<Answer> = oracle(&item)
            .iter()
            .map(|v| Answer::from_value(v).unwrap())
            .collect();
        let perfect = score_item(&item, &run);
        assert_eq!(metric(&perfect, "path_recall"), Some(1.0));
        assert_eq!(metric(&perfect, "exact_order"), Some(1.0));
        assert_eq!(to_query(&item).ok().unwrap().expected, vec!["b.rs", "c.rs"]);
    }
}
