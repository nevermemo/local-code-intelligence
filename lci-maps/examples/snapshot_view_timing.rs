//! Times `SnapshotView::new` (merge condition M4,
//! `docs/spike/multimap/engine.md`, "Seams and invariants"): the per-search pass that
//! derives a Chunk NodeId for every chunk of the filtered universe. The
//! universe here is a scan of `--root` (default: this repository), the
//! whole tree unfiltered, as a search without filters sees it.
//! `--copies N` repeats it under `copy<i>/` path prefixes to stand in for a
//! larger repository. Prints the chunk count and p50, p95 and max over
//! `--runs` constructions, in milliseconds.
//!
//! ```text
//! cargo run --release -p lci-maps --example snapshot_view_timing -- [--root DIR] [--copies N] [--runs N]
//! ```

use lci_core::chunk::{Chunk, scan};
use lci_maps::{SnapshotRef, SnapshotView};
use std::{path::PathBuf, time::Instant};

fn main() {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut copies = 1usize;
    let mut runs = 50usize;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--root" => root = PathBuf::from(value),
            "--copies" => copies = value.parse().expect("--copies is a number"),
            "--runs" => runs = value.parse().expect("--runs is a number"),
            other => panic!("unknown flag {other}"),
        }
    }
    assert!(
        copies >= 1 && runs >= 1,
        "--copies and --runs are at least 1"
    );
    let scanned = scan(&root).expect("scan the root");
    let chunks: Vec<Chunk> = (0..copies)
        .flat_map(|copy| {
            scanned.iter().map(move |chunk| Chunk {
                relative_file_path: if copies == 1 {
                    chunk.relative_file_path.clone()
                } else {
                    format!("copy{copy}/{}", chunk.relative_file_path)
                },
                ..chunk.clone()
            })
        })
        .collect();
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(view.len(), chunks.len());
    }
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
    println!(
        "chunks {} ({} scanned x {copies}), runs {runs}: p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms",
        chunks.len(),
        scanned.len(),
        at(0.5),
        at(0.95),
        times[times.len() - 1]
    );
}
