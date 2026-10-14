//! The extractor driver of slice M1.2 of the multimap spike v2 (pre-registration
//! in `docs/spike/multimap/v2/m1.2-prereg.md`).
//!
//! ```text
//! reads_key_eval reads-key  --scip INDEX.scip --root TREE --out edges.json
//! reads_key_eval check-flow --edges edges.json --gold flow.json [--out check.json]
//! ```
//!
//! `reads-key` reads the tree and the index only and has no gold option: it
//! writes the key table and every edge, and prints the counts of the freeze.
//! `check-flow` is the per-edge check against the `config-flow` items of the
//! flow gold; it is run after the edge file is frozen.

use lci_maps::{
    impact::{ImpactIndex, SourceCache, dir_source},
    reads_key::{
        CONFIG_RS, Class, build_key_table, extract, in_cfg_test_module, rs_files, test_module_files,
    },
    walk::wilson,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

fn flags() -> (String, BTreeMap<String, String>) {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode");
    let mut m = BTreeMap::new();
    while let Some(k) = args.next() {
        m.insert(k, args.next().expect("flag value"));
    }
    (mode, m)
}

fn need<'a>(m: &'a BTreeMap<String, String>, k: &str) -> &'a str {
    m.get(k).unwrap_or_else(|| panic!("missing {k}"))
}

fn read_json(path: &str) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json")
}

fn write_json(path: &str, v: &Value) {
    std::fs::write(path, serde_json::to_string_pretty(v).expect("ser") + "\n").expect("write");
}

fn main() {
    let (mode, m) = flags();
    match mode.as_str() {
        "reads-key" => reads_key(&m),
        "check-flow" => check_flow(&m),
        other => panic!("unknown mode {other}"),
    }
}

fn is_test_path(path: &str) -> bool {
    path.split('/').any(|c| c == "tests")
}

fn reads_key(m: &BTreeMap<String, String>) {
    assert!(
        !m.contains_key("--gold"),
        "the extractor has no gold option"
    );
    let root = PathBuf::from(need(m, "--root"));
    let src = dir_source(&root);
    let ix = ImpactIndex::load(&PathBuf::from(need(m, "--scip")), Some(&src)).expect("index");
    let table = build_key_table(&ix, &src);
    let edges = extract(&ix, &root, &src, &table).expect("extract");
    let mut cache = SourceCache::new(&src);
    let test_files = test_module_files(&src, &rs_files(&root).expect("files"));

    let keys: Vec<Value> = table
        .iter()
        .map(|e| {
            json!({"id": e.id(), "section": e.section, "key": e.key, "toml_line": e.toml_line,
                   "field_symbol": e.field_symbol, "owner": e.owner, "renamed": e.renamed})
        })
        .collect();
    let mut by_class: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_role: BTreeMap<&str, usize> = BTreeMap::new();
    let mut per_key: BTreeMap<String, usize> = BTreeMap::new();
    let mut out_edges = Vec::new();
    for e in &edges {
        *by_class.entry(e.class().name()).or_default() += 1;
        for r in &e.roles {
            *by_role.entry(r.name()).or_default() += 1;
        }
        *per_key.entry(e.key.clone()).or_default() += 1;
        let test = is_test_path(&e.decl.path)
            || test_files.contains(&e.decl.path)
            || in_cfg_test_module(&mut cache, &e.decl.path, e.decl.line as usize - 1);
        out_edges.push(json!({
            "key": e.key,
            "path": e.decl.path, "line": e.decl.line, "name": e.decl.name, "symbol": e.decl.symbol,
            "class": e.class().name(),
            "witnesses": {"scip": e.w1, "rg": e.w2, "L": e.l},
            "roles": e.roles.iter().map(|r| r.name()).collect::<Vec<_>>(),
            "in_def_file": e.in_def_file, "outside_scip": e.outside_scip, "test": test,
        }));
    }
    let mapped = table.iter().filter(|e| e.field_symbol.is_some()).count();
    let mut counts: Vec<usize> = table
        .iter()
        .filter(|e| e.field_symbol.is_some())
        .map(|e| per_key.get(&e.id()).copied().unwrap_or(0))
        .collect();
    counts.sort_unstable();
    let median = counts.get(counts.len() / 2).copied().unwrap_or(0);
    let bare: BTreeMap<&str, usize> = table.iter().fold(BTreeMap::new(), |mut a, e| {
        *a.entry(e.key.as_str()).or_default() += 1;
        a
    });
    let ambiguous: Vec<&str> = bare
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, _)| *k)
        .collect();
    write_json(
        need(m, "--out"),
        &json!({"config_file": CONFIG_RS, "keys": keys, "edges": out_edges}),
    );
    eprintln!(
        "keys {} mapped {mapped} unmapped {} renamed {} ambiguous bare keys {:?}",
        table.len(),
        table.len() - mapped,
        table.iter().filter(|e| e.renamed).count(),
        ambiguous
    );
    eprintln!(
        "edges {} by class {by_class:?} by role {by_role:?}",
        edges.len()
    );
    eprintln!(
        "edges per mapped key: min {} median {median} max {}",
        counts.first().copied().unwrap_or(0),
        counts.last().copied().unwrap_or(0)
    );
    let _ = Class::Confirmed;
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// Precision and recall of an extracted set against a gold set, 1.0 for an
/// empty denominator.
fn pr(extracted: &BTreeSet<(String, u64)>, gold: &BTreeSet<(String, u64)>) -> (f64, f64, usize) {
    let hit = extracted.intersection(gold).count();
    let p = if extracted.is_empty() {
        1.0
    } else {
        hit as f64 / extracted.len() as f64
    };
    let r = if gold.is_empty() {
        1.0
    } else {
        hit as f64 / gold.len() as f64
    };
    (p, r, hit)
}

fn check_flow(m: &BTreeMap<String, String>) {
    let edges = read_json(need(m, "--edges"));
    let gold = read_json(need(m, "--gold"));
    let items: Vec<&Value> = gold["items"]
        .as_array()
        .or_else(|| gold.as_array())
        .expect("gold items")
        .iter()
        .filter(|i| i["stratum"] == "config-flow")
        .collect();
    let keys = edges["keys"].as_array().expect("keys");
    let all_edges = edges["edges"].as_array().expect("edges");
    let mut rows = Map::new();
    let (mut field_ok, mut hits, mut ex_total, mut gold_total) = (0usize, 0usize, 0usize, 0usize);
    let (mut sum_p, mut sum_r) = (0.0, 0.0);
    for it in &items {
        let g = &it["gold"];
        let section = g["toml_section"].as_str().filter(|x| !x.is_empty());
        let key = s(&g["toml_key"]);
        let id = match section {
            Some(sec) => format!("{sec}.{key}"),
            None => key.clone(),
        };
        let entry = keys.iter().find(|k| s(&k["id"]) == id);
        let field_symbol = entry.map(|k| s(&k["field_symbol"]));
        let ok = field_symbol.as_deref() == Some(s(&g["field"]["symbol"]).as_str());
        field_ok += usize::from(ok);
        let gold_set: BTreeSet<(String, u64)> = g["readers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| s(&r["file"]) != CONFIG_RS)
            .map(|r| (s(&r["file"]), r["line"].as_u64().unwrap_or(0)))
            .collect();
        let ex_set: BTreeSet<(String, u64)> = all_edges
            .iter()
            .filter(|e| s(&e["key"]) == id)
            .filter(|e| {
                e["roles"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|x| x == "read"))
            })
            .filter(|e| s(&e["path"]) != CONFIG_RS && e["test"] != true)
            .map(|e| (s(&e["path"]), e["line"].as_u64().unwrap_or(0)))
            .collect();
        let (p, r, hit) = pr(&ex_set, &gold_set);
        sum_p += p;
        sum_r += r;
        hits += hit;
        ex_total += ex_set.len();
        gold_total += gold_set.len();
        rows.insert(
            s(&it["id"]),
            json!({"key": id, "field_ok": ok, "extracted": ex_set.len(), "gold": gold_set.len(),
                   "hit": hit, "precision": p, "recall": r,
                   "missed": gold_set.difference(&ex_set).collect::<Vec<_>>(),
                   "extra": ex_set.difference(&gold_set).collect::<Vec<_>>()}),
        );
    }
    let n = items.len();
    let (lo_p, hi_p) = wilson(hits, ex_total);
    let (lo_r, hi_r) = wilson(hits, gold_total);
    let summary = json!({
        "items": n, "field_ok": field_ok,
        "mean_precision": sum_p / n.max(1) as f64, "mean_recall": sum_r / n.max(1) as f64,
        "pooled": {"extracted": ex_total, "gold": gold_total, "hit": hits,
                   "precision": [hits as f64 / ex_total.max(1) as f64, lo_p, hi_p],
                   "recall": [hits as f64 / gold_total.max(1) as f64, lo_r, hi_r]},
    });
    eprintln!("{summary}");
    if let Some(out) = m.get("--out") {
        write_json(out, &json!({"summary": summary, "items": rows}));
    }
}
