//! The compiler probe (L5) of slice M1.3 (pre-registration
//! `docs/spike/multimap/v2/m1.3-prereg.md`): apply a patch to a scratch
//! checkout, run `cargo check`, revert, and return the error sites as
//! `compile_error_at` edges.
//!
//! ```text
//! compile_probe --tree TREE --target-dir DIR --out probe.json \
//!               [--patch FILE] [--patch-id ID] [--scip INDEX.scip]
//! ```
//!
//! Without `--patch` the unmutated tree is checked (baseline and re-warm).
//! Without `--scip` sites are not mapped to nodes. The SCIP index is loaded
//! before the patch is applied, so the timed span (`cargo_ms`) is the cargo
//! process alone. The tree must be a scratch checkout: it is changed (and
//! restored with `git checkout -- .`) in place and must be clean before and
//! after.

use lci_maps::{
    impact::{ImpactIndex, dir_source},
    probe::{Site, edge_json, parse_cargo_json},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

fn flags() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        map.insert(flag, value);
    }
    map
}

fn need<'a>(m: &'a BTreeMap<String, String>, k: &str) -> &'a str {
    m.get(k).unwrap_or_else(|| panic!("{k} is required"))
}

fn git(tree: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(args)
        .output()
        .expect("run git");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn clean(tree: &Path) -> bool {
    let (ok, text) = git(tree, &["status", "--porcelain", "--untracked-files=no"]);
    ok && text.trim().is_empty()
}

fn ms(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

fn main() {
    let m = flags();
    let tree = PathBuf::from(need(&m, "--tree"));
    let target_dir = PathBuf::from(need(&m, "--target-dir"));
    let out_path = need(&m, "--out").to_string();
    let patch = m.get("--patch").map(PathBuf::from);
    let patch_id = m.get("--patch-id").cloned().unwrap_or_else(|| {
        patch
            .as_ref()
            .and_then(|p| p.file_stem())
            .map_or("none".into(), |s| s.to_string_lossy().into_owned())
    });
    let root = tree.to_string_lossy().into_owned();

    let clean_before = clean(&tree);
    assert!(clean_before, "the tree must be clean before the probe");
    let t = Instant::now();
    let index = m.get("--scip").map(|p| {
        let src = dir_source(&tree);
        ImpactIndex::load(&PathBuf::from(p), Some(&src)).expect("load index")
    });
    let scip_load_ms = ms(t);

    let t = Instant::now();
    let mut apply_ok = true;
    if let Some(p) = &patch {
        let p = p.to_string_lossy().into_owned();
        let (ok, text) = git(&tree, &["apply", "--unidiff-zero", &p]);
        assert!(ok, "git apply failed: {text}");
        apply_ok = ok;
    }
    let apply_ms = ms(t);

    let t = Instant::now();
    let output = Command::new("cargo")
        .args([
            "check",
            "--workspace",
            "--all-targets",
            "--keep-going",
            "--message-format=json",
        ])
        .current_dir(&tree)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("run cargo");
    let cargo_ms = ms(t);

    let t = Instant::now();
    let mut revert_ok = true;
    if patch.is_some() {
        // `git apply -R` leaves a pure deletion as a line-ending change in a
        // CRLF checkout, so the revert restores the files from the index.
        let (ok, text) = git(&tree, &["checkout", "--", "."]);
        if !ok {
            eprintln!("revert failed: {text}");
        }
        revert_ok = ok;
    }
    let revert_ms = ms(t);
    let clean_after = clean(&tree);

    let t = Instant::now();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_cargo_json(&stdout, &root);
    let edges: Vec<Value> = parsed
        .sites
        .iter()
        .map(|s| edge_json(index.as_ref(), &patch_id, s))
        .collect();
    let map_ms = ms(t);
    let mut codes: BTreeMap<String, usize> = BTreeMap::new();
    for Site { code, .. } in &parsed.sites {
        *codes
            .entry(code.clone().unwrap_or_else(|| "none".into()))
            .or_default() += 1;
    }
    let report = json!({
        "patch_id": patch_id,
        "tree_commit": git(&tree, &["rev-parse", "HEAD"]).1.trim(),
        "clean_before": clean_before,
        "clean_after": clean_after,
        "apply_ok": apply_ok,
        "revert_ok": revert_ok,
        "cargo_rc": output.status.code(),
        "build_success": parsed.build_success,
        "cargo_ms": cargo_ms,
        "apply_ms": apply_ms,
        "revert_ms": revert_ms,
        "map_ms": map_ms,
        "scip_load_ms": scip_load_ms,
        "error_messages": parsed.error_messages,
        "n_sites": parsed.sites.len(),
        "external_spans_skipped": parsed.external_spans_skipped,
        "codes": codes,
        "reached_units": parsed.reached,
        "edges": edges,
    });
    let text = serde_json::to_string_pretty(&report).expect("serialize");
    std::fs::write(&out_path, text + "\n").unwrap_or_else(|e| panic!("write {out_path}: {e}"));
    println!(
        "{patch_id}: cargo {cargo_ms} ms, rc {:?}, {} sites",
        output.status.code(),
        parsed.sites.len()
    );
    if !clean_after || !revert_ok {
        std::process::exit(3);
    }
}
