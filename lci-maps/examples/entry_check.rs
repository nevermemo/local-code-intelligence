//! Entry-point check for the spike v2 E (error) gold set.
//!
//! Loads a frozen SCIP index into a [`ScipGraph`] and, for every item of the
//! error gold set, maps the raise sites and the candidate entry handlers to
//! graph nodes, then reports over `calls_or_refs` edges only:
//!
//! * the shortest forward path from each candidate handler to the nearest
//!   raise site of the item (length and chain);
//! * per raise site, every node of `src/main.rs`, `src/server.rs` and
//!   `src/rest.rs` that reaches it, with its distance (the caller classifies
//!   which of them are real handlers);
//! * per raise site, the reverse closure sizes at a few depths (the control:
//!   an approximation of a call-hierarchy walk from the raise site).
//!
//! ```text
//! cargo run --release -p lci-maps --example entry_check -- \
//!     --scip INDEX.scip --root TREE --gold error.json --out result.json
//! ```

use lci_maps::scip_graph::{DirSource, GraphEdgeKind, ScipGraph};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
};

const HANDLER_FILES: [&str; 3] = ["src/main.rs", "src/server.rs", "src/rest.rs"];

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// Backward BFS over calls_or_refs from `target`: distance and the next node
/// on the shortest path toward `target` (parent pointers).
fn reverse_bfs(rev: &[Vec<usize>], target: usize) -> (Vec<u32>, Vec<usize>) {
    let n = rev.len();
    let mut dist = vec![u32::MAX; n];
    let mut next = vec![usize::MAX; n];
    let mut queue = VecDeque::new();
    dist[target] = 0;
    queue.push_back(target);
    while let Some(v) = queue.pop_front() {
        for &u in &rev[v] {
            if dist[u] == u32::MAX {
                dist[u] = dist[v] + 1;
                next[u] = v;
                queue.push_back(u);
            }
        }
    }
    (dist, next)
}

fn main() {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        map.insert(flag, value);
    }
    let get = |k: &str| PathBuf::from(map.get(k).unwrap_or_else(|| panic!("{k} is required")));
    let (scip, root, gold, out) = (get("--scip"), get("--root"), get("--gold"), get("--out"));

    let source = DirSource(root);
    let graph = ScipGraph::load(&scip, Some(&source)).expect("load scip");
    let n = graph.nodes.len();
    let mut rev: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (from, edges) in graph.out.iter().enumerate() {
        for e in edges
            .iter()
            .filter(|e| e.kind == GraphEdgeKind::CallsOrRefs)
        {
            rev[e.to].push(from);
        }
    }
    // (file, 1-based line of the name) -> node.
    let mut by_pos: HashMap<(String, u32), usize> = HashMap::new();
    for (i, node) in graph.nodes.iter().enumerate() {
        by_pos.insert((node.file.clone(), node.line + 1), i);
    }
    let label = |i: usize| {
        let node = &graph.nodes[i];
        json!({"name": node.name, "file": node.file, "line": node.line + 1})
    };

    let items: Value = serde_json::from_str(&std::fs::read_to_string(&gold).expect("read gold"))
        .expect("parse gold");
    let mut results = Vec::new();
    for raw in items.as_array().expect("gold is an array") {
        let id = str_of(&raw["id"]);
        // Raise sites -> nodes.
        let mut sites = Vec::new();
        let mut site_nodes: Vec<Option<usize>> = Vec::new();
        for s in raw["gold"]["sites"].as_array().into_iter().flatten() {
            let path = str_of(&s["path"]);
            let fn_line = s["fn_line"].as_u64().unwrap_or(0) as u32;
            let node = by_pos.get(&(path.clone(), fn_line)).copied();
            site_nodes.push(node);
            sites.push(json!({
                "path": path, "fn": s["fn"], "fn_line": fn_line, "line": s["line"],
                "role": s["role"],
                "node": node.map(label),
                "indegree": node.map(|i| graph.indegree[i]),
            }));
        }
        // Typed-display items: the construction site of the variant is only
        // recorded as the `from_site` of the entry-point chains ("path:line
        // Owner::fn"); add each one that is not already a gold site.
        for e in raw["entry_point"].as_array().into_iter().flatten() {
            let from = str_of(&e["from_site"]);
            let Some((loc, _)) = from.split_once(' ') else {
                continue;
            };
            let Some((path, line)) = loc.rsplit_once(':') else {
                continue;
            };
            let line: u32 = line.parse().unwrap_or(0);
            let node = by_pos.get(&(path.to_string(), line)).copied();
            if node.is_some() && site_nodes.contains(&node) {
                continue;
            }
            site_nodes.push(node);
            sites.push(json!({
                "path": path, "fn": from, "fn_line": line, "role": "from_site",
                "node": node.map(label),
                "indegree": node.map(|i| graph.indegree[i]),
            }));
        }
        let bfs: Vec<Option<(Vec<u32>, Vec<usize>)>> = site_nodes
            .iter()
            .map(|n| n.map(|i| reverse_bfs(&rev, i)))
            .collect();

        // Candidate entry handlers.
        let mut entries = Vec::new();
        for e in raw["entry_point"].as_array().into_iter().flatten() {
            let handler = str_of(&e["handler"]);
            // "path:line name"
            let (loc, hname) = handler.split_once(' ').unwrap_or((handler.as_str(), ""));
            let (hpath, hline) = loc.rsplit_once(':').unwrap_or((loc, "0"));
            let hline: u32 = hline.parse().unwrap_or(0);
            let node = by_pos.get(&(hpath.to_string(), hline)).copied();
            let mut best: Option<(u32, usize, Vec<Value>)> = None;
            if let Some(h) = node {
                for (si, b) in bfs.iter().enumerate() {
                    let Some((dist, next)) = b else { continue };
                    if dist[h] != u32::MAX && best.as_ref().is_none_or(|(d, _, _)| dist[h] < *d) {
                        let mut chain = vec![label(h)];
                        let mut cur = h;
                        while next[cur] != usize::MAX {
                            cur = next[cur];
                            chain.push(label(cur));
                        }
                        best = Some((dist[h], si, chain));
                    }
                }
            }
            entries.push(json!({
                "handler": handler, "kind": e["kind"], "hname": hname,
                "node": node.map(label),
                "reaches": best.is_some(),
                "len": best.as_ref().map(|b| b.0),
                "site_index": best.as_ref().map(|b| b.1),
                "chain": best.map(|b| b.2),
                "name_chain_ambiguous": e["via_ambiguous_name"],
            }));
        }

        // Handler-file nodes reaching each site, and reverse closure sizes.
        let mut per_site = Vec::new();
        for (si, b) in bfs.iter().enumerate() {
            let Some((dist, next)) = b else {
                per_site.push(json!({"site_index": si, "mapped": false}));
                continue;
            };
            let mut reaching = Vec::new();
            for (i, node) in graph.nodes.iter().enumerate() {
                if dist[i] != u32::MAX && HANDLER_FILES.contains(&node.file.as_str()) {
                    let mut chain = vec![label(i)];
                    let mut cur = i;
                    while next[cur] != usize::MAX {
                        cur = next[cur];
                        chain.push(label(cur));
                    }
                    reaching.push(json!({"node": label(i), "dist": dist[i], "chain": chain}));
                }
            }
            let closure = |d: u32| dist.iter().filter(|&&x| x != u32::MAX && x <= d).count();
            // Direct callees of `main` that reach the site: which CLI arms can.
            let mut first_hops = Vec::new();
            for (i, node) in graph.nodes.iter().enumerate() {
                if node.file == "src/main.rs" && node.name == "main" {
                    for e in graph.out[i]
                        .iter()
                        .filter(|e| e.kind == GraphEdgeKind::CallsOrRefs)
                    {
                        if dist[e.to] != u32::MAX {
                            let mut chain = vec![label(e.to)];
                            let mut cur = e.to;
                            while next[cur] != usize::MAX {
                                cur = next[cur];
                                chain.push(label(cur));
                            }
                            first_hops.push(json!({
                                "hop": label(e.to), "dist": dist[e.to], "chain": chain,
                            }));
                        }
                    }
                }
            }
            per_site.push(json!({
                "site_index": si, "mapped": true,
                "main_first_hops": first_hops,
                "handler_file_nodes_reaching": reaching,
                "closure_d3": closure(3), "closure_d5": closure(5), "closure_d8": closure(8),
                "closure_all": closure(u32::MAX - 1),
            }));
        }
        results.push(json!({"id": id, "sites": sites, "entries": entries, "per_site": per_site}));
    }
    let report = json!({
        "graph": {
            "nodes": n,
            "calls_or_refs_edges": graph.edge_count(GraphEdgeKind::CallsOrRefs),
            "repaired": graph.report.repaired,
            "repair_skipped": graph.report.repair_skipped,
        },
        "items": results,
    });
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).expect("write");
    eprintln!("wrote {} ({} items)", out.display(), results.len());
}
