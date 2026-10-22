//! BENCH-ONLY, GOLD-AWARE arms for the steering ceiling. They read the gold,
//! which the bench rule says is never given to a traversal, so a row from
//! either is an upper-bound probe, never a result.
//!
//! * `maps oracle-policy` writes one steering policy file per gold item: the
//!   rule list prunes every frontier node outside the item's gold regions
//!   (`maps bench --policy-dir` passes each item its file).
//! * `maps oracle-filter` is the control: it takes the results of a plain
//!   walk (a bench run dir with `raw/`) and keeps only the nodes inside the
//!   same regions, by the same test the rules apply, so the gain of the
//!   policy can be split into steering and result filtering.
//!
//! Regions are files. The gold's own files for the item (the files
//! `gold to-evaluate` expects), plus the seed's file for impact items and the
//! entry and target files for flow items. A file or chunk node is inside when
//! its file is a region file. A symbol node is inside when its module path
//! (`lci_core::node::rust_module_path` of a region file, joined with `/`, which
//! is what a `path_glob` sees of a symbol) equals a region file's module or is
//! below it (`<module>/**`: inline modules, but also child modules of a
//! `mod.rs`, `lib.rs` or `x.rs` with an `x/` directory, so the region is
//! slightly generous there). Other node kinds are never pruned. A symbol at
//! the crate root (`src/lib.rs`, empty module path) cannot be named by a glob;
//! it is kept exactly when `src/lib.rs`-style root files are in the regions.

use super::baseline::write_arm_run;
use super::bench::{ResultNode, Totals, Traversed, adapt, select_items, set_name, slug};
use crate::gold::{self, Set};
use anyhow::{Context, Result, bail};
use clap::Args;
use lci_core::node::{NodeId, rust_module_path};
use lci_maps::traverse::rules::{MAX_CHILDREN, MAX_GLOB, MAX_RULES, glob_match, node_path};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// The most globs one region can use: `MAX_CHILDREN` groups of `MAX_CHILDREN`.
const MAX_GLOBS: usize = MAX_CHILDREN * MAX_CHILDREN;

/// Where an item's answers may lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    files: BTreeSet<String>,
    /// Globs on a symbol's module path, sorted.
    module_globs: Vec<String>,
    /// A region file is a crate root (empty module path).
    root_module: bool,
}

impl Region {
    pub fn new(files: &[String]) -> Result<Region> {
        let files: BTreeSet<String> = files
            .iter()
            .map(|f| f.replace('\\', "/"))
            .filter(|f| !f.is_empty())
            .collect();
        let mut globs = BTreeSet::new();
        let mut root_module = false;
        for file in &files {
            if file.len() > MAX_GLOB {
                bail!("region file path longer than {MAX_GLOB} characters: {file}");
            }
            if !lci_core::path::has_extension_ignore_case(file, "rs") {
                continue;
            }
            let module = rust_module_path(file).join("/");
            if module.is_empty() {
                root_module = true;
                continue;
            }
            if module.contains(['*', '?']) || file.contains(['*', '?']) {
                bail!("a glob character in a region path: {file}");
            }
            globs.insert(format!("{module}/**"));
            globs.insert(module);
        }
        if globs.len() > MAX_GLOBS || files.len() > MAX_GLOBS {
            bail!(
                "region too large for the rule grammar ({} files); at most {MAX_GLOBS} globs per group",
                files.len()
            );
        }
        Ok(Region {
            files,
            module_globs: globs.into_iter().collect(),
            root_module,
        })
    }

    pub fn files(&self) -> &BTreeSet<String> {
        &self.files
    }

    /// Whether the rules keep `node`: the one test both arms use.
    pub fn keeps(&self, node: &NodeId) -> bool {
        match node {
            NodeId::Symbol(_) => {
                let module = node_path(node).unwrap_or_default();
                if module.is_empty() {
                    self.root_module
                } else {
                    self.module_globs.iter().any(|g| glob_match(g, &module))
                }
            }
            NodeId::File { path } | NodeId::Chunk { path, .. } => self.files.contains(path),
            _ => true,
        }
    }

    /// The rule list: prune what `keeps` refuses, then `continue` for all.
    pub fn rules(&self) -> Result<Value> {
        let sym = json!({"kind": "sym"});
        let sym_rule = match (self.module_globs.is_empty(), self.root_module) {
            (true, false) => sym,
            (true, true) => json!({"all": [sym, {"path_glob": "?**"}]}),
            (false, false) => json!({"all": [sym, {"not": any_of(&self.module_globs)?}]}),
            (false, true) => {
                json!({"all": [sym, {"path_glob": "?**"}, {"not": any_of(&self.module_globs)?}]})
            }
        };
        let files: Vec<String> = self.files.iter().cloned().collect();
        let file_rule = json!({"all": [
            {"any": [{"kind": "file"}, {"kind": "chunk"}]},
            {"not": any_of(&files)?},
        ]});
        let rules = json!([
            {"when": sym_rule, "then": "prune"},
            {"when": file_rule, "then": "prune"},
            {"when": {"budget_left": {"ge": 0}}, "then": "continue"},
        ]);
        debug_assert!(rules.as_array().is_some_and(|r| r.len() <= MAX_RULES));
        Ok(rules)
    }
}

/// `{"any": [globs]}`, nested in groups when there are more than
/// `MAX_CHILDREN`.
fn any_of(globs: &[String]) -> Result<Value> {
    if globs.is_empty() {
        bail!("a region has no globs for this node kind");
    }
    if globs.len() <= MAX_CHILDREN {
        let leaves: Vec<Value> = globs.iter().map(|g| json!({"path_glob": g})).collect();
        return Ok(json!({"any": leaves}));
    }
    let groups: Vec<Value> = globs
        .chunks(MAX_CHILDREN)
        .map(|chunk| {
            let leaves: Vec<Value> = chunk.iter().map(|g| json!({"path_glob": g})).collect();
            json!({"any": leaves})
        })
        .collect();
    if groups.len() > MAX_CHILDREN {
        bail!("{} globs do not fit the rule grammar", globs.len());
    }
    Ok(json!({"any": groups}))
}

/// The policy object `lci traverse --policy` reads. The ladder must end with
/// `agent` (a stop no rung can decide would otherwise be refused); the last
/// rule decides every stop, so the agent rung never runs.
pub fn policy(region: &Region, max_stops: u32, stop_every: u32, tau: f64) -> Result<Value> {
    if stop_every == 0 || max_stops == 0 {
        bail!("--stop-every and --max-stops must be at least 1");
    }
    if !(0.0..=1.0).contains(&tau) {
        bail!("--tau is a number from 0 to 1");
    }
    Ok(json!({
        "rules": region.rules()?,
        "ladder": ["rules", "agent"],
        "max_stops": max_stops,
        "tau": tau,
        "stop_every": stop_every,
    }))
}

fn text_at(item: &Value, pointer: &str) -> Option<String> {
    item.pointer(pointer)
        .and_then(Value::as_str)
        .map(|s| s.replace('\\', "/"))
}

/// The files an item's answers may lie in (see the module docs).
pub fn region_files(set: Set, item: &Value) -> Result<Vec<String>, &'static str> {
    let mut files = gold::expected_files(set, item)?;
    let extra: Vec<Option<String>> = match set {
        Set::Impact => vec![text_at(item, "/seed/path")],
        Set::Flow => vec![
            text_at(item, "/gold/entry/file"),
            text_at(item, "/gold/target/file"),
        ],
        _ => Vec::new(),
    };
    files.extend(extra.into_iter().flatten());
    files.sort();
    files.dedup();
    Ok(files)
}

fn supported(set: Set) -> Result<()> {
    match set {
        Set::Impact | Set::Flow | Set::WhereTo => Ok(()),
        _ => bail!(
            "the oracle regions are files: they fit impact, flow and where-to, not history or instructions"
        ),
    }
}

// ----------------------------------------------------------- oracle-policy

#[derive(Args)]
pub struct OraclePolicyArgs {
    /// Which gold set (impact, flow or where-to).
    #[arg(long, value_enum)]
    set: Set,
    /// Directory holding the set's gold file (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// Where the per-item policy files are written (`<slug of item id>.json`,
    /// plus `_index.json`), for `maps bench --policy-dir`.
    #[arg(long)]
    out_dir: PathBuf,
    /// Only the first N items of the bench population.
    #[arg(long)]
    limit: Option<usize>,
    /// `max_stops` of every policy. The walk is steered only at stops, so the
    /// first `max_stops` pops are; the run's `[traverse] max_stops` config
    /// caps it, so raise that cap to match.
    #[arg(long, default_value_t = 32)]
    max_stops: u32,
    /// A stop every this many pops (1 steers every pop until `max_stops`).
    #[arg(long, default_value_t = 1)]
    stop_every: u32,
    /// Confidence a rung needs to decide a stop.
    #[arg(long, default_value_t = 0.5)]
    tau: f64,
}

pub fn run_policy(args: OraclePolicyArgs) -> Result<()> {
    supported(args.set)?;
    let items = select_items(
        args.set,
        gold::load_gold(&args.gold_dir.join(format!("{}.json", set_name(args.set))))?,
        args.limit,
        None,
    );
    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create {}", args.out_dir.display()))?;
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
    let mut written = Vec::new();
    let mut regions = serde_json::Map::new();
    let mut skipped = serde_json::Map::new();
    for item in &items {
        let id = item["id"].as_str().context("gold item without an id")?;
        let files = match region_files(args.set, item) {
            Ok(files) => files,
            Err(why) => {
                skipped.insert(id.to_owned(), json!(why));
                continue;
            }
        };
        let region = Region::new(&files).with_context(|| format!("region of {id}"))?;
        let file = policy(&region, args.max_stops, args.stop_every, args.tau)
            .with_context(|| format!("policy of {id}"))?;
        std::fs::write(
            args.out_dir.join(format!("{}.json", slug(id))),
            pretty(&file)?,
        )?;
        regions.insert(id.to_owned(), json!(region.files().len()));
        written.push(id.to_owned());
    }
    std::fs::write(
        args.out_dir.join("_index.json"),
        pretty(&json!({
            "bench_only_gold_aware": true,
            "set": set_name(args.set),
            "written": written,
            "region_files_per_item": regions,
            "skipped": skipped,
        }))?,
    )?;
    eprintln!(
        "oracle-policy: BENCH-ONLY, GOLD-AWARE. {} policies written, {} items skipped, to {}",
        written.len(),
        skipped.len(),
        args.out_dir.display()
    );
    Ok(())
}

// ----------------------------------------------------------- oracle-filter

#[derive(Args)]
pub struct OracleFilterArgs {
    /// The plain walk's run dir (`<out>/<wave>/<set>` of a `maps bench` run;
    /// its `raw/` is read). Run it with `--top-k 40` so the filter has a pool.
    run_dir: PathBuf,
    /// Which gold set (impact, flow or where-to).
    #[arg(long, value_enum)]
    set: Set,
    /// Directory holding the set's gold file (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// Results kept per item after the filter (the bench's `--top-k`).
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Put the walk's seed nodes ahead of the results, as `maps bench
    /// --include-seeds` does (seeds are not filtered, as the policy leaves
    /// them alone).
    #[arg(long)]
    include_seeds: bool,
    /// Label of the output; the run dir is `<out-dir>/<wave>/<set>`.
    #[arg(long, default_value = "oracle-filter")]
    wave: String,
    /// Where the filtered run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

pub fn run_filter(args: OracleFilterArgs) -> Result<()> {
    supported(args.set)?;
    let set = args.set;
    let items = select_items(
        set,
        gold::load_gold(&args.gold_dir.join(format!("{}.json", set_name(set))))?,
        None,
        None,
    );
    let raw_dir = args.run_dir.join("raw");
    let dir = args.out_dir.join(slug(&args.wave)).join(set_name(set));
    std::fs::create_dir_all(dir.join("raw"))?;
    let (mut missing, mut no_region, mut short) = (0usize, 0usize, 0usize);
    let mut scored: Vec<(&Value, Vec<Value>)> = Vec::new();
    let mut expansions: Vec<u64> = Vec::new();
    let mut model_calls: Vec<u64> = Vec::new();
    for item in &items {
        let id = item["id"].as_str().context("gold item without an id")?;
        let file = format!("{}.json", slug(id));
        let raw: Option<Value> = std::fs::read_to_string(raw_dir.join(&file))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .filter(|raw: &Value| raw.get("skipped").is_none());
        let Some(raw) = raw else {
            missing += 1;
            continue;
        };
        let Ok(files) = region_files(set, item) else {
            no_region += 1;
            continue;
        };
        let region = Region::new(&files).with_context(|| format!("region of {id}"))?;
        let traversed = Traversed::from_json(&raw, None);
        let mut nodes: Vec<ResultNode> = Vec::new();
        if args.include_seeds {
            nodes.extend(traversed.seeds.iter().map(|s| ResultNode {
                id: s.clone(),
                text: s.clone(),
                site: traversed.sites.get(s).cloned(),
            }));
        }
        let kept: Vec<ResultNode> = traversed
            .results
            .iter()
            .filter(|n| {
                n.id.parse::<NodeId>()
                    .ok()
                    .is_none_or(|id| region.keeps(&id))
            })
            .take(args.top_k)
            .cloned()
            .collect();
        short += usize::from(kept.len() < args.top_k);
        nodes.extend(kept);
        scored.push((item, adapt(set, item, &nodes)));
        expansions.push(
            traversed
                .totals
                .as_ref()
                .map_or(0, |t| t.pops + t.pass_through_pops),
        );
        // The filter is free, but the walk it filters paid for these calls.
        model_calls.push(traversed.totals.as_ref().map_or(0, Totals::model_calls));
        std::fs::copy(raw_dir.join(&file), dir.join("raw").join(&file))
            .with_context(|| format!("copy the raw output of {id}"))?;
    }
    let meta = json!({
        "arm": "oracle post-filter control (maps oracle-filter)",
        "gold_aware": true,
        "bench_only": true,
        "source_run": args.run_dir.display().to_string(),
        "kept_per_item": args.top_k,
        "include_seeds": args.include_seeds,
        "items_without_raw_output": missing,
        "items_without_region": no_region,
        "items_with_fewer_than_k_after_filter": short,
        "note": "the plain walk's results restricted to the gold regions, then cut to k; compare with the oracle-policy arm to split steering from filtering; expansions and model calls are the source walk's",
    });
    let summary = write_arm_run(&dir, set, &scored, &expansions, &model_calls, &meta)?;
    eprintln!(
        "oracle-filter: BENCH-ONLY, GOLD-AWARE. {} items scored, {missing} without a raw output, {no_region} without a region, {short} left with fewer than {} results\nprecision {:.3}, f0.5 {:.3}; {}\nrun dir: {}",
        summary.n,
        args.top_k,
        summary.precision,
        summary.f05,
        summary.headline,
        dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_core::node::{SymbolKey, SymbolKind};
    use lci_maps::traverse::rules::RuleList;

    fn symbol(module: &[&str], name: &str) -> NodeId {
        NodeId::Symbol(SymbolKey {
            language: "rust".into(),
            module_path: module.iter().map(|m| (*m).to_owned()).collect(),
            container: Vec::new(),
            name: name.into(),
            kind: SymbolKind::Function,
            disambiguator: 0,
        })
    }

    fn file(path: &str) -> NodeId {
        NodeId::File { path: path.into() }
    }

    fn region(files: &[&str]) -> Region {
        Region::new(&files.iter().map(|f| (*f).to_owned()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn region_globs_follow_the_module_paths() {
        let r = region(&["src/app/search.rs", "lci-maps/src/impact.rs", "docs/a.md"]);
        assert_eq!(
            r.module_globs,
            [
                "app/search",
                "app/search/**",
                "lci-maps/impact",
                "lci-maps/impact/**"
            ]
        );
        assert!(!r.root_module);
        assert!(r.keeps(&symbol(&["app", "search"], "f")));
        // An inline module of the file is inside; a sibling module is not.
        assert!(r.keeps(&symbol(&["app", "search", "tests"], "f")));
        assert!(!r.keeps(&symbol(&["app", "other"], "f")));
        assert!(!r.keeps(&symbol(&["app", "searching"], "f")));
        assert!(r.keeps(&symbol(&["lci-maps", "impact"], "f")));
        // Files and chunks go by file path; other kinds are never pruned.
        assert!(r.keeps(&file("docs/a.md")));
        assert!(!r.keeps(&file("src/app/other.rs")));
        assert!(r.keeps(&NodeId::Commit {
            sha: "0123456789abcdef0123456789abcdef01234567".into()
        }));
    }

    #[test]
    fn a_crate_root_symbol_is_kept_only_when_a_root_file_is_a_region() {
        let with = region(&["src/lib.rs", "src/a.rs"]);
        assert!(with.root_module);
        assert!(with.keeps(&symbol(&[], "root_fn")));
        let without = region(&["src/a.rs"]);
        assert!(!without.keeps(&symbol(&[], "root_fn")));
        for r in [&with, &without] {
            RuleList::from_json(&r.rules().unwrap()).unwrap();
        }
    }

    #[test]
    fn the_policy_parses_with_the_rule_grammar_and_the_cli_keys() {
        let r = region(&["src/app/search.rs", "src/b.rs"]);
        let p = policy(&r, 32, 1, 0.5).unwrap();
        // `SteerArgs` refuses unknown keys.
        let allowed = [
            "rules",
            "ladder",
            "max_stops",
            "tau",
            "stop_every",
            "on_disagreement",
            "likely_complete",
        ];
        for key in p.as_object().unwrap().keys() {
            assert!(allowed.contains(&key.as_str()), "unknown key {key}");
        }
        assert_eq!(p["ladder"], json!(["rules", "agent"]));
        assert_eq!(p["max_stops"], 32);
        assert_eq!(p["stop_every"], 1);
        let rules = p["rules"].as_array().unwrap();
        assert_eq!(rules.last().unwrap()["then"], "continue");
        assert!(
            rules
                .iter()
                .all(|r| r["when"].is_object() && r["then"].is_string())
        );
        // The real parser accepts it (bounds on rules, nesting and children).
        RuleList::from_json(&p["rules"]).unwrap();
        assert!(policy(&r, 0, 1, 0.5).is_err());
        assert!(policy(&r, 32, 1, 1.5).is_err());
    }

    #[test]
    fn many_files_nest_the_any_lists_and_too_many_are_refused() {
        let files: Vec<String> = (0..20).map(|i| format!("src/m{i}.rs")).collect();
        let r = Region::new(&files).unwrap();
        // 40 module globs: three groups of at most 16.
        assert_eq!(r.module_globs.len(), 40);
        let rules = r.rules().unwrap();
        RuleList::from_json(&rules).unwrap();
        assert!(r.keeps(&symbol(&["m19"], "f")));
        assert!(!r.keeps(&symbol(&["m20"], "f")));
        let many: Vec<String> = (0..300).map(|i| format!("src/m{i}.rs")).collect();
        assert!(Region::new(&many).is_err());
        let long = format!("src/{}.rs", "a".repeat(MAX_GLOB));
        assert!(Region::new(&[long]).is_err());
    }

    #[test]
    fn regions_are_the_gold_files_plus_the_seed_and_flow_ends() {
        let impact = json!({
            "id": "i-1", "primary": true,
            "seed": {"path": "s.rs"},
            "gold": {"r1": {"files": ["a.rs", "b.rs"]}}
        });
        assert_eq!(
            region_files(Set::Impact, &impact).unwrap(),
            ["a.rs", "b.rs", "s.rs"]
        );
        let not_primary =
            json!({"id": "i-2", "primary": false, "gold": {"r1": {"files": ["a.rs"]}}});
        assert!(region_files(Set::Impact, &not_primary).is_err());
        let flow = json!({
            "id": "f-1", "stratum": "named/len4",
            "gold": {
                "entry": {"name": "A", "file": "a.rs", "line": 1},
                "target": {"name": "D", "file": "d.rs", "line": 4},
                "hops": [],
                "intermediates": [{"name": "B", "file": "b.rs", "line": 2}]
            }
        });
        assert_eq!(
            region_files(Set::Flow, &flow).unwrap(),
            ["a.rs", "b.rs", "d.rs"]
        );
        assert!(supported(Set::History).is_err());
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-br3-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn impact_gold() -> Value {
        json!([{
            "id": "impact-t1", "primary": true,
            "question": "What breaks if `seed` changes?",
            "stratum": "sym-fn/deg2-4/tests-near",
            "ambiguity": {"band": "some"},
            "seed": {"path": "a.rs", "line": 1, "span": [1, 1], "name": "seed", "symbol": "sym:seed"},
            "accept": {"neutral_decls": [], "neutral_files": [], "decoys": []},
            "gold": {"r1": {
                "decls": [{"path": "a.rs", "line": 2, "span": [2, 2], "name": "inside", "bare": "inside", "symbol": "sym:inside"}],
                "files": ["a.rs"]
            }, "r2": {"decls": []}, "tests": []}
        }])
    }

    #[test]
    fn the_filter_keeps_the_results_inside_the_regions_and_writes_a_run_dir() {
        let root = temp_dir("filter");
        let gold_dir = root.join("gold");
        std::fs::create_dir_all(&gold_dir).unwrap();
        std::fs::write(gold_dir.join("impact.json"), impact_gold().to_string()).unwrap();
        // A plain walk's raw output: one result inside a.rs, one outside.
        let inside = symbol(&["a"], "inside").to_string();
        let outside = symbol(&["z"], "outside").to_string();
        let raw = json!({
            "status": "complete", "stop_reason": "bound", "seeds": [],
            "results": [
                {"node": outside, "text": "outside", "site": {"path": "z.rs", "line": 5}},
                {"node": inside, "text": "inside", "site": {"path": "a.rs", "line": 2}},
            ],
            "totals": {
                "pops": 7, "hops": 9, "elapsed_ms": 1.0,
                "rerank_calls": 2
            }
        });
        let run_dir = root.join("run");
        std::fs::create_dir_all(run_dir.join("raw")).unwrap();
        std::fs::write(run_dir.join("raw").join("impact-t1.json"), raw.to_string()).unwrap();
        let out = root.join("out");
        run_filter(OracleFilterArgs {
            run_dir: run_dir.clone(),
            set: Set::Impact,
            gold_dir,
            top_k: 10,
            include_seeds: false,
            wave: "w".into(),
            out_dir: out.clone(),
        })
        .unwrap();
        let dir = out.join("w").join("impact");
        let run: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(run["impact-t1"], json!([{"path": "a.rs", "line": 2}]));
        let data = super::super::compare::load_run(&dir, None).unwrap();
        assert_eq!(data.items["impact-t1"]["decl_recall"], Some(1.0));
        assert_eq!(data.items["impact-t1"]["expansions"], Some(7.0));
        // The arm carries its source's model calls (rerank 2), the
        // same sum the bench stamps on the source run's own items.
        let source = Traversed::from_json(&raw, None).totals.unwrap();
        assert_eq!(source.model_calls(), 2);
        assert_eq!(data.items["impact-t1"]["model_calls"], Some(2.0));
        // The raw output travelled with it, so compare can read its flags.
        assert!(dir.join("raw").join("impact-t1.json").is_file());
        assert!(data.flags["impact-t1"].is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oracle_policy_writes_one_file_per_item_and_an_index() {
        let root = temp_dir("policy");
        let gold_dir = root.join("gold");
        std::fs::create_dir_all(&gold_dir).unwrap();
        std::fs::write(gold_dir.join("impact.json"), impact_gold().to_string()).unwrap();
        let out = root.join("policies");
        run_policy(OraclePolicyArgs {
            set: Set::Impact,
            gold_dir,
            out_dir: out.clone(),
            limit: None,
            max_stops: 32,
            stop_every: 1,
            tau: 0.5,
        })
        .unwrap();
        let text = std::fs::read_to_string(out.join("impact-t1.json")).unwrap();
        let policy: Value = serde_json::from_str(&text).unwrap();
        RuleList::from_json(&policy["rules"]).unwrap();
        let index: Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("_index.json")).unwrap())
                .unwrap();
        assert_eq!(index["written"], json!(["impact-t1"]));
        assert_eq!(index["bench_only_gold_aware"], true);
        let _ = std::fs::remove_dir_all(&root);
    }
}
