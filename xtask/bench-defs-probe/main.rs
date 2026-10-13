//! The spike bench's `sym.defs` probe (`docs/spike/multimap/bench.md`,
//! section 2.1). Not a workspace member: `cargo xtask bench freeze-defs`
//! builds it against the `lci-core` and `lci-maps` of the tree being
//! frozen, because no build exposes `sym.defs` through a CLI or REST
//! surface (the map is registered for tests and the contract kit only).
//!
//! Usage: `defs-probe <checkout> <bundle store dir> <limit>`, one name per
//! line on stdin. Scans the checkout with the core scanner, builds
//! `sym.defs` from the declaration bundles (reusing the store's bundles of
//! unchanged files) and answers each name with the map's own ranking. Prints
//! one JSON object: scan and build cost, and per name the paths of the
//! chunks the map returned, in rank order.

use lci_core::{bundle::BundleStore, chunk::scan_incremental, node::chunk_ids};
use lci_maps::{
    SnapshotRef, SnapshotView,
    fresh::Target,
    symdefs::{SymDefs, answer},
};
use serde_json::json;
use std::{collections::HashMap, io::Read, path::PathBuf, time::Instant};

/// Bundle store key: a fixed workspace id, one store per checkout.
const STORE_KEY: &str = "bench";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [checkout, store, limit] = args.as_slice() else {
        return Err("usage: defs-probe <checkout> <bundle store dir> <limit>".into());
    };
    let (checkout, store) = (PathBuf::from(checkout), PathBuf::from(store));
    let limit: usize = limit.parse()?;
    let mut names = String::new();
    std::io::stdin().read_to_string(&mut names)?;

    let started = Instant::now();
    let scan = scan_incremental(&checkout, &HashMap::new()).map_err(|e| format!("{e:#}"))?;
    let scan_ms = started.elapsed().as_secs_f64() * 1000.0;
    let snapshot = SnapshotRef {
        indexed_at: "bench".into(),
        fingerprint: scan.fingerprint.clone(),
    };
    let mut map = SymDefs::new(BundleStore::new(&store, STORE_KEY));
    let started = Instant::now();
    map.build(&Target {
        snapshot: &snapshot,
        root: &checkout,
        files: &scan.files,
    })?;
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let data = map.data().ok_or("sym.defs built no data")?;
    let (reused, extracted) = map.last_sync.unwrap_or_default();

    let paths: HashMap<_, &str> = chunk_ids(&scan.chunks)
        .into_iter()
        .zip(scan.chunks.iter().map(|c| c.relative_file_path.as_str()))
        .collect();
    let view = SnapshotView::new(snapshot.clone(), scan.chunks.iter().collect());
    let answers: Vec<_> = names
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let started = Instant::now();
            let hits = answer(data, name, limit, &view);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let files: Vec<&str> = hits
                .iter()
                .map(|hit| paths.get(&hit.node).copied().unwrap_or(""))
                .collect();
            json!({"name": name, "files": files, "ms": ms})
        })
        .collect();
    let report = json!({
        "files": scan.files.len(),
        "chunks": scan.chunks.len(),
        "declarations": data.entries().len(),
        "scan_ms": scan_ms,
        "build_ms": build_ms,
        "reused": reused,
        "extracted": extracted,
        "answers": answers,
    });
    println!("{report}");
    Ok(())
}
