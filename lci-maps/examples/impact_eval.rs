//! The driver of slice M1.4 of the multimap spike v2: `impact(x)` as a search
//! anchor plus an exact one-hop closure (pre-registration in
//! `docs/spike/multimap/v2/m1.4-prereg.md`).
//!
//! ```text
//! impact_eval make-input --report FR-report.json --gold impact.json --out anchor-input.json
//! impact_eval anchors    --scip INDEX.scip --root TREE --input anchor-input.json \
//!                        --rule C|N [--k 5] --out anchors.json
//! impact_eval label      --anchors anchors.json --gold impact.json --out labels.json
//! impact_eval run        --arm O|OD|C|N|V|B|R|CD --scip INDEX.scip --root TREE \
//!                        [--gold impact.json (O, OD only)] [--anchors anchors.json] \
//!                        [--size-from C-run.json (B, R)] [--seed 1] --out run.json
//! impact_eval kinds      --scip INDEX.scip --root TREE
//! ```
//!
//! `make-input` is the only step that reads the gold before the anchors are
//! frozen, and only for the item ids and the questions (an alignment check).
//! `anchors` takes the stripped input file and the index and has no gold
//! option. `run` reads the gold only for the arms seeded from it (O, OD).

use lci_maps::{
    impact::{
        Chunk, Decl, ImpactIndex, SourceCache, arm_b, arm_r, arm_v, code_names, dir_source,
        reverse_reach,
    },
    scip_graph::ScipGraph,
    walk::wilson,
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, path::PathBuf};

const SAFETY_LIMIT: usize = 2000;
const MAX_DEPTH: usize = 12;

fn flags() -> (String, BTreeMap<String, String>) {
    let mut it = std::env::args().skip(1);
    let mode = it.next().expect("a mode is required");
    let mut map = BTreeMap::new();
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        map.insert(flag, value);
    }
    (mode, map)
}

fn read_json(path: &str) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn write_json(path: &str, value: &Value) {
    let text = serde_json::to_string_pretty(value).expect("serialize");
    std::fs::write(path, text + "\n").unwrap_or_else(|e| panic!("write {path}: {e}"));
}

fn need<'a>(m: &'a BTreeMap<String, String>, k: &str) -> &'a str {
    m.get(k).unwrap_or_else(|| panic!("{k} is required"))
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn primary_items(gold: &Value) -> Vec<&Value> {
    gold.as_array()
        .expect("gold is an array")
        .iter()
        .filter(|x| x["primary"].as_bool() == Some(true))
        .collect()
}

fn decl_json(d: &Decl) -> Value {
    json!({"path": d.path, "line": d.line, "name": d.name, "symbol": d.symbol})
}

fn main() {
    let (mode, m) = flags();
    match mode.as_str() {
        "make-input" => make_input(&m),
        "anchors" => anchors(&m),
        "label" => label(&m),
        "run" => run(&m),
        "kinds" => kinds(&m),
        other => panic!("unknown mode {other}"),
    }
}

fn make_input(m: &BTreeMap<String, String>) {
    let report = read_json(need(m, "--report"));
    let gold = read_json(need(m, "--gold"));
    let items = primary_items(&gold);
    let queries = report["queries"].as_array().expect("report.queries");
    assert_eq!(
        queries.len(),
        items.len(),
        "report and gold item counts differ"
    );
    let mut out = Vec::new();
    let mut mismatched = 0;
    for (q, item) in queries.iter().zip(&items) {
        // Alignment check only: the gold question is compared, never used.
        if s(&q["query"]["query"]) != s(&item["question"]) {
            mismatched += 1;
        }
        let mut evidence = Vec::new();
        for e in q["evidence"].as_array().expect("evidence") {
            evidence.push(json!({
                "rank": e["rank"],
                "path": e["relative_file_path"],
                "start_line": e["start_line"],
                "end_line": e["end_line"],
            }));
        }
        out.push(json!({"id": item["id"], "query": q["query"]["query"], "evidence": evidence}));
    }
    assert_eq!(
        mismatched, 0,
        "{mismatched} report queries differ from the gold question"
    );
    write_json(need(m, "--out"), &Value::Array(out));
    eprintln!(
        "make-input: {} items, query text equals gold question for all",
        items.len()
    );
}

fn load_index(m: &BTreeMap<String, String>) -> ImpactIndex {
    let root = PathBuf::from(need(m, "--root"));
    let src = dir_source(&root);
    ImpactIndex::load(&PathBuf::from(need(m, "--scip")), Some(&src)).expect("load index")
}

fn anchors(m: &BTreeMap<String, String>) {
    assert!(
        !m.contains_key("--gold"),
        "the anchor step has no gold option"
    );
    let ix = load_index(m);
    let input = read_json(need(m, "--input"));
    let rule = need(m, "--rule");
    let k: u32 = m
        .get("--k")
        .map_or(5, |v| v.parse().expect("--k is a number"));
    let mut items = Map::new();
    let (mut found, mut none, mut no_names, mut rejected_total) = (0, 0, 0, 0);
    for it in input.as_array().expect("input array") {
        let id = s(&it["id"]);
        let (names, rejected) = code_names(&s(&it["query"]));
        rejected_total += rejected;
        let evidence: Vec<Chunk> = it["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| Chunk {
                rank: e["rank"].as_u64().unwrap_or(0) as u32,
                path: s(&e["path"]),
                start_line: e["start_line"].as_u64().unwrap_or(0) as u32,
                end_line: e["end_line"].as_u64().unwrap_or(0) as u32,
            })
            .collect();
        if names.is_empty() {
            no_names += 1;
        }
        let a = match rule {
            "C" => ix.anchor_c(&names, &evidence, k),
            "N" => ix.anchor_n(&names),
            other => panic!("unknown rule {other}"),
        };
        let name_strs: Vec<String> = names.iter().map(|n| n.join("::")).collect();
        let entry = match a {
            Some(a) => {
                found += 1;
                json!({"symbol": ix.defs[a.def].symbol, "chunk_rank": a.chunk_rank,
                       "segments": a.segments, "names": name_strs})
            }
            None => {
                none += 1;
                json!({"symbol": null, "names": name_strs})
            }
        };
        items.insert(id, entry);
    }
    write_json(
        need(m, "--out"),
        &json!({"rule": rule, "k": if rule == "C" { json!(k) } else { Value::Null }, "items": items}),
    );
    eprintln!(
        "anchors {rule} k={k}: {found} anchored, {none} none ({no_names} without a name), {rejected_total} code spans rejected"
    );
}

fn label(m: &BTreeMap<String, String>) {
    let anchors = read_json(need(m, "--anchors"));
    let gold = read_json(need(m, "--gold"));
    let mut rows = Map::new();
    let mut tally: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    for item in primary_items(&gold) {
        let id = s(&item["id"]);
        let band = s(&item["ambiguity"]["band"]);
        let anchor = &anchors["items"][&id]["symbol"];
        let label = match anchor.as_str() {
            None => "none",
            Some(sym) if sym == s(&item["seed"]["symbol"]) => "correct",
            Some(_) => "wrong",
        };
        let idx = match label {
            "correct" => 0,
            "wrong" => 1,
            _ => 2,
        };
        for key in ["all".to_string(), band.clone()] {
            tally.entry(key).or_default()[idx] += 1;
        }
        rows.insert(id, json!({"band": band, "label": label}));
    }
    let mut summary = Map::new();
    for (key, [c, w, n]) in &tally {
        let total = c + w + n;
        let (lo, hi) = wilson(*c, total);
        eprintln!(
            "{key}: n {total}  correct {c} ({:.3} [{lo:.3}, {hi:.3}])  wrong {w}  none {n}",
            *c as f64 / total as f64
        );
        summary.insert(
            key.clone(),
            json!({"n": total, "correct": c, "wrong": w, "none": n, "share": *c as f64 / total as f64, "lo": lo, "hi": hi}),
        );
    }
    write_json(
        need(m, "--out"),
        &json!({"summary": summary, "items": rows}),
    );
}

fn kinds(m: &BTreeMap<String, String>) {
    let ix = load_index(m);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (i, d) in ix.defs.iter().enumerate() {
        let k = ix.kinds[i].map_or("none".to_string(), |k| format!("{k:?}"));
        *counts
            .entry(format!("{k} unit={}", ix.unit[i] && d.enclosing.is_some()))
            .or_default() += 1;
    }
    for (k, c) in &counts {
        eprintln!("{c:6} {k}");
    }
    // Decl-to-decl edges and attributed sites, a build check against the gold's record.
    let mut edges = std::collections::BTreeSet::new();
    let mut sites = 0;
    for u in 0..ix.defs.len() {
        for c in ix.closure(&ix.defs[u].symbol) {
            edges.insert((c, u));
        }
    }
    for (di, doc) in ix.docs.iter().enumerate() {
        let _ = di;
        for occ in &doc.occurrences {
            if occ.roles & 1 == 0
                && !occ.symbol.starts_with("local ")
                && ix.def_index(&occ.symbol).is_some()
                && ix.unit_at(&doc.path, occ.range.0, occ.range.1).is_some()
            {
                sites += 1;
            }
        }
    }
    eprintln!(
        "units {}  decl->decl edges {}  attributed sites to unit symbols {}  repaired {}  unparsed {}",
        ix.unit_defs().len(),
        edges.len(),
        sites,
        ix.repaired,
        ix.unparsed_symbols
    );
}

fn run(m: &BTreeMap<String, String>) {
    let arm = need(m, "--arm");
    let seeded = matches!(arm, "O" | "OD");
    assert!(
        seeded || !m.contains_key("--gold"),
        "arm {arm} must not be given the gold"
    );
    let ix = load_index(m);
    let root = PathBuf::from(need(m, "--root"));
    let src = dir_source(&root);
    let mut cache = SourceCache::new(&src);
    let needs_graph = matches!(arm, "B" | "OD" | "CD");
    let graph = needs_graph.then(|| {
        ScipGraph::load(&PathBuf::from(need(m, "--scip")), Some(&src)).expect("load graph")
    });
    let seed: u64 = m
        .get("--seed")
        .map_or(1, |v| v.parse().expect("--seed is a number"));

    // (item id, the symbol this arm starts from)
    let mut work: Vec<(String, Option<String>)> = Vec::new();
    if seeded {
        let gold = read_json(need(m, "--gold"));
        for item in primary_items(&gold) {
            work.push((s(&item["id"]), Some(s(&item["seed"]["symbol"]))));
        }
    } else {
        let anchors = read_json(need(m, "--anchors"));
        for (id, a) in anchors["items"].as_object().expect("anchors.items") {
            work.push((id.clone(), a["symbol"].as_str().map(str::to_string)));
        }
    }
    let sizes: BTreeMap<String, usize> = if matches!(arm, "B" | "R") {
        let c = read_json(need(m, "--size-from"));
        c.as_object()
            .expect("size-from run")
            .iter()
            .map(|(k, v)| (k.clone(), v.as_array().map_or(0, Vec::len)))
            .collect()
    } else {
        BTreeMap::new()
    };
    let units = ix.unit_defs();

    let mut out = Map::new();
    let (mut empty, mut total, mut max, mut defects, mut no_start) = (0, 0, 0, 0, 0);
    for (idx, (id, symbol)) in work.iter().enumerate() {
        let decls: Vec<Decl> = match (arm, symbol) {
            ("R", _) => {
                let n = sizes.get(id).copied().unwrap_or(0);
                arm_r(&units, n, seed * 1_000_003 + idx as u64)
                    .into_iter()
                    .map(|d| ix.decl(d))
                    .collect()
            }
            (_, None) => Vec::new(),
            ("O" | "C" | "N", Some(sym)) => {
                ix.closure(sym).into_iter().map(|d| ix.decl(d)).collect()
            }
            ("V", Some(sym)) => match ix.def_index(sym) {
                Some(a) => arm_v(&ix, &root, &mut cache, a)
                    .expect("rg")
                    .into_iter()
                    .map(|d| ix.decl(d))
                    .collect(),
                None => Vec::new(),
            },
            ("B", Some(sym)) => {
                let n = sizes.get(id).copied().unwrap_or(0);
                let start = ix.def_index(sym).and_then(|a| {
                    let d = &ix.defs[a];
                    ix.unit_at(&d.path, d.name_range.0, d.name_range.1)
                });
                match start {
                    Some(u) => arm_b(graph.as_ref().expect("graph"), &ix.defs[u].symbol, n)
                        .map(|v| {
                            v.iter()
                                .filter_map(|s| ix.def_index(s))
                                .map(|d| ix.decl(d))
                                .collect()
                        })
                        .unwrap_or_else(|| {
                            no_start += 1;
                            Vec::new()
                        }),
                    None => {
                        no_start += 1;
                        Vec::new()
                    }
                }
            }
            ("OD" | "CD", Some(sym)) => {
                let hop1: Vec<String> = ix
                    .closure(sym)
                    .into_iter()
                    .map(|d| ix.defs[d].symbol.clone())
                    .collect();
                reverse_reach(graph.as_ref().expect("graph"), &hop1, MAX_DEPTH)
                    .iter()
                    .filter_map(|s| ix.def_index(s))
                    .filter(|&d| cache.is_test(&ix.defs[d]))
                    .map(|d| ix.decl(d))
                    .collect()
            }
            (other, _) => panic!("unknown arm {other}"),
        };
        let mut decls = decls;
        decls.sort();
        decls.dedup();
        if decls.len() > SAFETY_LIMIT {
            eprintln!(
                "DEFECT {id}: {} declarations exceed the safety limit",
                decls.len()
            );
            defects += 1;
            decls.clear();
        }
        if decls.is_empty() {
            empty += 1;
        }
        total += decls.len();
        max = max.max(decls.len());
        out.insert(
            id.clone(),
            Value::Array(decls.iter().map(decl_json).collect()),
        );
    }
    write_json(need(m, "--out"), &Value::Object(out));
    eprintln!(
        "arm {arm}: {} items, {empty} empty, {total} declarations, max {max}, defects {defects}, start missing {no_start}",
        work.len()
    );
}
