//! `cargo xtask maps fit-priors`: fit the per-question-type hop priors
//! (`lci_maps::traverse::priors`) from the traces `maps bench` wrote.
//!
//! A bench out dir holds `cli/traces/<handle>.jsonl` and, per wave and set,
//! `<wave>/<set>/raw/<item>.json`, which names the trace of the item and the
//! site of every node its cards located. A node is gold when the set's own
//! scorer credits the answer the bench adapter offers for it (the definition
//! site, as the bench scores it); the scorers are reused, not copied.
//!
//! Items split by a hash into a fit part and a hold-out part. Only the fit part
//! is read; the hold-out ids go into the table so an evaluation can use only
//! them. With no salt the hash is of the item id (the original split). With a
//! salt it is of `<salt>:<group key>` ([`group_key`]), so items that share a
//! source commit fall on the same side and a new salt draws a new split; the
//! salt is written into the table as `holdout_salt`. The split does not change
//! when runs are added.

use super::bench::{self, ResultNode, Site, Traversed};
use crate::gold::{self, Answer, Set};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use lci_maps::scip_graph::{ScipGraph, ScipMap};
use lci_maps::traverse::{
    frontier::kind_prior,
    priors::{PriorFit, PriorTable, label_hops},
    trace::{StartEvent, read_trace},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct FitPriorsArgs {
    /// Bench out dirs (the `--out-dir` of `maps bench`), or a directory of them.
    #[arg(long, required = true, num_args = 1..)]
    runs: Vec<PathBuf>,
    /// Directory holding the gold files (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// Where the table is written. It is derived from private gold: keep it
    /// out of the repository.
    #[arg(long)]
    out: PathBuf,
    /// Share of items (by hash) kept out of the fit.
    #[arg(long, default_value_t = 30)]
    holdout_percent: u64,
    /// Split salt: the hash is of `<salt>:<group key>` (items of one source
    /// commit stay together) instead of the item id. Empty keeps the original
    /// id split. Written into the table as `holdout_salt`.
    #[arg(long, default_value = "")]
    salt: String,
    /// Pseudo-observations the guessed prior counts for.
    #[arg(long, default_value_t = 10.0)]
    smoothing: f64,
    /// The SCIP index the bench ran with. A raw output carries the site of only
    /// the nodes in its answer, visited and frontier cards (a minority of the
    /// symbols a trace names); the index gives the rest the same site the CLI
    /// would have given them.
    #[arg(long)]
    scip: Option<PathBuf>,
}

/// What one question type's traces contained.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TypeStats {
    pub items: usize,
    pub no_trace: usize,
    pub unscored: usize,
    /// Symbol nodes of the traces, and those no card gave a site.
    pub symbols: usize,
    pub symbols_without_site: usize,
}

/// A bench out dir's set runs: (out dir, set, the `raw` dir).
fn set_runs(root: &Path) -> Result<Vec<(PathBuf, Set, PathBuf)>> {
    let subdirs = |dir: &Path| -> Result<Vec<PathBuf>> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("read {}", dir.display()))?
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        Ok(dirs)
    };
    let out_dirs = if root.join("cli").is_dir() {
        vec![root.to_path_buf()]
    } else {
        subdirs(root)?
            .into_iter()
            .filter(|dir| dir.join("cli").is_dir())
            .collect()
    };
    let mut runs = Vec::new();
    for out in out_dirs {
        for wave in subdirs(&out)?.into_iter().filter(|d| !d.ends_with("cli")) {
            for dir in subdirs(&wave)? {
                let set = dir.file_name().and_then(|n| n.to_str());
                if let Some(set) = set.and_then(|n| Set::from_str(n, true).ok())
                    && dir.join("raw").is_dir()
                {
                    runs.push((out.clone(), set, dir.join("raw")));
                }
            }
        }
    }
    Ok(runs)
}

/// Whether the item id goes to the hold-out part (the unsalted split).
pub fn is_holdout(item_id: &str, percent: u64) -> bool {
    let digest = Sha256::digest(item_id.as_bytes());
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head) % 100 < percent
}

/// What a salted split keeps together: the first `commit:` entry of the item's
/// `provenance` (the history gold names the commit its question is about, and
/// several questions share one), else the item id. Gold items carry no other
/// field that clusters related queries.
pub fn group_key(item: &Value) -> String {
    let commit = item["provenance"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|entry| entry.strip_prefix("commit:"));
    match commit {
        Some(commit) => format!("commit:{commit}"),
        None => item["id"].as_str().unwrap_or_default().to_owned(),
    }
}

/// Whether the item goes to the hold-out part under `salt`. An empty salt is
/// exactly [`is_holdout`] of the item id, so tables fitted before salts exist
/// stay valid.
pub fn in_holdout(item: &Value, salt: &str, percent: u64) -> bool {
    if salt.is_empty() {
        is_holdout(item["id"].as_str().unwrap_or_default(), percent)
    } else {
        is_holdout(&format!("{salt}:{}", group_key(item)), percent)
    }
}

/// The `holdout_salt` of a prior table or tie-break file (empty when the file
/// has none: the unsalted split). `wanted` is a salt the caller expects; a
/// different one is an error, since the hold-out list was drawn with the
/// file's.
pub fn file_salt(path: &Path, wanted: Option<&str>) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the hold-out file {}", path.display()))?;
    let doc: Value = serde_json::from_str(&text).context("the hold-out file is not JSON")?;
    let salt = doc["holdout_salt"].as_str().unwrap_or_default().to_owned();
    if let Some(wanted) = wanted
        && wanted != salt
    {
        bail!(
            "--salt {wanted:?} does not match the holdout_salt {salt:?} of {}",
            path.display()
        );
    }
    Ok(salt)
}

/// Writes `holdout_salt` into the JSON file at `path` (the table writers in
/// `lci-maps` do not know the field; they ignore it when reading).
pub fn stamp_salt(path: &Path, salt: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&text).context("the file is not JSON")?;
    doc.as_object_mut()
        .context("the file is not a JSON object")?
        .insert("holdout_salt".into(), Value::from(salt));
    let mut out = serde_json::to_string_pretty(&doc)?;
    out.push('\n');
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))
}

/// The file name `maps bench` gives the raw output of an item.
fn raw_name(id: &str) -> String {
    let slug: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{slug}.json")
}

/// The scorer metrics that say one answer, scored alone, is gold.
fn hit_metrics(set: Set, answer: &Value) -> &'static [&'static str] {
    match set {
        Set::Flow => &["path_recall", "fanout_recall"],
        Set::Impact if answer["line"].is_null() => &["file_precision"],
        Set::Impact => &["decl_precision"],
        Set::WhereTo => &["reg_hit@3", "set_precision"],
        Set::History => &["hit@1"],
        Set::Instructions => &["i1_section_precision", "i2_required_precision@5"],
        Set::Traits => &["set_precision", "hit@1"],
    }
}

/// Whether the set's scorer credits the answer the bench would offer for the
/// node when it is the only answer.
pub(super) fn is_gold(set: Set, item: &Value, node: ResultNode) -> Result<bool> {
    let Some(answer) = bench::adapt(set, item, &[node]).into_iter().next() else {
        return Ok(false);
    };
    let id = item["id"].as_str().unwrap_or_default().to_owned();
    let run = gold::Run::from([(id, vec![Answer::from_value(&answer)?])]);
    let report = gold::score_set(set, std::slice::from_ref(item), &run, false, false)?;
    let metrics = &report["overall"]["metrics"];
    Ok(hit_metrics(set, &answer)
        .iter()
        .any(|name| metrics[*name].as_f64().is_some_and(|v| v > 0.0)))
}

/// The nodes a trace names: its seeds and the ends of its hops.
fn trace_nodes<'a>(events: &'a [Value], seeds: &'a [String]) -> BTreeSet<&'a str> {
    let ends = events
        .iter()
        .filter(|e| matches!(e["ev"].as_str(), Some("hop" | "relax")))
        .flat_map(|e| [e["src"].as_str(), e["dst"].as_str()])
        .flatten();
    ends.chain(seeds.iter().map(String::as_str)).collect()
}

pub struct Fitted {
    pub fit: PriorFit,
    pub holdout: BTreeSet<String>,
    pub stats: BTreeMap<String, TypeStats>,
}

/// `scip_sites` locate the symbol nodes no card of a raw output located.
pub fn fit_runs(
    runs: &[PathBuf],
    gold_dir: &Path,
    holdout_percent: u64,
    salt: &str,
    scip_sites: &HashMap<String, Site>,
) -> Result<Fitted> {
    let mut out = Fitted {
        fit: PriorFit::default(),
        holdout: BTreeSet::new(),
        stats: BTreeMap::new(),
    };
    let mut golds: HashMap<String, Vec<Value>> = HashMap::new();
    for root in runs {
        for (out_dir, set, raw_dir) in set_runs(root)? {
            let name = bench::set_name(set);
            if !golds.contains_key(name) {
                let items = gold::load_gold(&gold_dir.join(format!("{name}.json")))?;
                golds.insert(name.to_owned(), items);
            }
            let stats = out.stats.entry(name.to_owned()).or_default();
            for item in &golds[name] {
                let id = item["id"].as_str().unwrap_or_default();
                if in_holdout(item, salt, holdout_percent) {
                    out.holdout.insert(id.to_owned());
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(raw_dir.join(raw_name(id))) else {
                    stats.no_trace += 1;
                    continue;
                };
                let raw: Value = serde_json::from_str(&text)
                    .with_context(|| format!("raw output of {id} is not JSON"))?;
                let Some(file) = raw["trace"]["file"].as_str() else {
                    stats.no_trace += 1;
                    continue;
                };
                let events = read_trace(out_dir.join("cli").join("traces").join(file))
                    .with_context(|| format!("trace of {id}"))?;
                let seeds = events
                    .first()
                    .and_then(StartEvent::from_json)
                    .map(|start| start.seeds)
                    .unwrap_or_default();
                // An item the scorer leaves out of its population (impact keeps
                // the primary ones) has no gold to fit against.
                let empty = gold::Run::new();
                let population =
                    gold::score_set(set, std::slice::from_ref(item), &empty, false, false)?;
                if population["scored_items"] == 0 {
                    stats.unscored += 1;
                    continue;
                }
                let sites = Traversed::from_json(&raw, None).sites;
                let mut gold_nodes = BTreeSet::new();
                for node in trace_nodes(&events, &seeds) {
                    let site = sites.get(node).or_else(|| scip_sites.get(node)).cloned();
                    if node.starts_with("sym:") {
                        stats.symbols += 1;
                        stats.symbols_without_site += usize::from(site.is_none());
                    }
                    let result = ResultNode {
                        id: node.to_owned(),
                        text: String::new(),
                        site,
                    };
                    if is_gold(set, item, result)? {
                        gold_nodes.insert(node);
                    }
                }
                let hops = label_hops(&events, &seeds, |node| gold_nodes.contains(node));
                out.fit.add(name, &hops);
                stats.items += 1;
            }
        }
    }
    Ok(out)
}

/// The site of every symbol of a SCIP index, by node id, as the CLI's cards
/// give it (the line of the name, counted from 1).
fn scip_sites(index: &Path) -> Result<HashMap<String, Site>> {
    let graph = ScipGraph::load(index, None).map_err(|e| anyhow!(e))?;
    let map = ScipMap::with_default_ids(graph);
    Ok(map
        .graph()
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let site = Site {
                path: node.file.replace('\\', "/"),
                line: u64::from(node.line) + 1,
            };
            (map.node_id(i).to_string(), site)
        })
        .collect())
}

pub fn run(args: FitPriorsArgs) -> Result<()> {
    if args.holdout_percent > 100 || args.smoothing.is_nan() || args.smoothing < 0.0 {
        bail!("--holdout-percent must be 0..=100 and --smoothing at least 0");
    }
    let scip_sites = match &args.scip {
        Some(index) => scip_sites(index)?,
        None => HashMap::new(),
    };
    let fitted = fit_runs(
        &args.runs,
        &args.gold_dir,
        args.holdout_percent,
        &args.salt,
        &scip_sites,
    )?;
    let table: PriorTable = fitted.fit.fit(args.smoothing, fitted.holdout.clone());
    table.save(&args.out)?;
    stamp_salt(&args.out, &args.salt)?;
    let normalised = table.normalised();
    println!(
        "wrote {} ({} hold-out items, salt {:?})",
        args.out.display(),
        table.holdout.len(),
        args.salt,
    );
    for (question_type, pairs) in fitted.fit.counts() {
        let stats = &fitted.stats[question_type];
        let hops: u64 = pairs.values().map(|c| c.n).sum();
        let positives: u64 = pairs.values().map(|c| c.positives).sum();
        println!(
            "{question_type}: {} items fitted ({} without a trace, {} unscored), {hops} hops, {positives} positive; \
             {}/{} symbol nodes without a site{}",
            stats.items,
            stats.no_trace,
            stats.unscored,
            stats.symbols_without_site,
            stats.symbols,
            if table.types.contains_key(question_type) {
                ""
            } else {
                "; no positive hop: left to the guess"
            },
        );
        let mut changes: Vec<_> = pairs
            .iter()
            .map(|((map, kind), count)| {
                let prior = table.lookup(question_type, map, *kind);
                let norm = normalised.lookup(question_type, map, *kind);
                (prior - kind_prior(*kind), map, *kind, *count, prior, norm)
            })
            .collect();
        changes.sort_by(|a, b| b.0.abs().total_cmp(&a.0.abs()));
        for (_, map, kind, count, prior, norm) in changes {
            println!(
                "  {map} {}: n={} positive={} prior={prior:.4} normalised={norm:.3} guess={:.2}",
                kind.as_str(),
                count.n,
                count.positives,
                kind_prior(kind),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_split_depends_on_the_id_alone_and_partitions_the_items() {
        let ids: Vec<String> = (0..1000).map(|i| format!("flow-{i:04}")).collect();
        let held = |percent| ids.iter().filter(|id| is_holdout(id, percent)).count();
        assert_eq!(held(30), held(30));
        assert!((250..=350).contains(&held(30)), "{}", held(30));
        assert_eq!((held(0), held(100)), (0, 1000));
        // The hold-out part of a smaller share is inside the bigger one.
        assert!(
            ids.iter()
                .all(|id| !is_holdout(id, 10) || is_holdout(id, 30))
        );
    }

    #[test]
    fn an_empty_salt_is_the_original_id_split() {
        for i in 0..200 {
            let id = format!("history-h1-{i:04}");
            // Whatever the provenance says, no salt hashes the id alone.
            let item = json!({"id": id, "provenance": [format!("commit:c{}", i % 7)]});
            for percent in [0, 10, 30, 100] {
                assert_eq!(in_holdout(&item, "", percent), is_holdout(&id, percent));
            }
        }
        // Pinned buckets (first 8 bytes of SHA-256 of the id, mod 100): these
        // ids must keep their side, or tables fitted before salts break.
        for (id, bucket) in [
            ("flow-0000", 92),
            ("flow-0001", 6),
            ("flow-0002", 71),
            ("flow-0003", 21),
        ] {
            let item = json!({"id": id});
            assert!(!in_holdout(&item, "", bucket) && in_holdout(&item, "", bucket + 1));
        }
    }

    #[test]
    fn a_salt_redraws_the_split_and_keeps_a_commit_group_together() {
        let item = |i: usize, commit: usize| json!({"id": format!("h-{i}"), "provenance": ["note", format!("commit:c{commit}")]});
        let items: Vec<Value> = (0..400).map(|i| item(i, i / 4)).collect();
        // Items of one commit share a side, whatever the salt.
        for salt in ["s1", "s2"] {
            for group in items.chunks(4) {
                let sides: Vec<bool> = group.iter().map(|g| in_holdout(g, salt, 30)).collect();
                assert!(sides.iter().all(|s| *s == sides[0]), "{salt} {sides:?}");
            }
        }
        // A different salt gives a different split; the share is still near the percent.
        let side =
            |salt: &str| -> Vec<bool> { items.iter().map(|g| in_holdout(g, salt, 30)).collect() };
        assert_ne!(side("s1"), side("s2"));
        assert_ne!(side("s1"), side(""));
        let held = side("s1").iter().filter(|h| **h).count();
        assert!((60..=180).contains(&held), "{held}");
        // An item with no commit is grouped by its id.
        assert_eq!(group_key(&json!({"id": "flow-1"})), "flow-1");
        assert_eq!(
            group_key(&json!({"id": "x", "provenance": ["file:a", "commit:abc"]})),
            "commit:abc"
        );
    }

    #[test]
    fn the_salt_is_stamped_into_a_table_file_and_read_back() {
        let root = std::env::temp_dir().join(format!("fit-priors-salt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("table.json");
        let table = PriorTable::default();
        table.save(&path).unwrap();
        assert_eq!(file_salt(&path, None).unwrap(), "");
        stamp_salt(&path, "s1").unwrap();
        assert_eq!(file_salt(&path, None).unwrap(), "s1");
        assert_eq!(file_salt(&path, Some("s1")).unwrap(), "s1");
        assert!(file_salt(&path, Some("s2")).is_err());
        // The table still loads (the unknown field is ignored).
        assert_eq!(PriorTable::load(&path).unwrap(), table);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_synthetic_bench_dir_is_fitted_on_the_fit_part_only() {
        let root = std::env::temp_dir().join(format!("fit-priors-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let out = root.join("out");
        let raw = out.join("w").join("flow").join("raw");
        std::fs::create_dir_all(&raw).unwrap();
        std::fs::create_dir_all(out.join("cli").join("traces")).unwrap();
        let ids: Vec<String> = (0..40).map(|i| format!("flow-{i:02}")).collect();
        let node = |name: &str, line: u64| json!({"name": name, "file": "src/a.rs", "line": line, "symbol": name});
        let mut items = Vec::new();
        for id in &ids {
            items.push(json!({
                "id": id,
                "question": "q",
                "gold": {
                    "hops": 2,
                    "entry": node("e", 1),
                    "target": node("t", 9),
                    "intermediates": [node("mid", 5)],
                },
            }));
            // seed -> mid (gold, a call) and seed -> other (a containment).
            let events = [
                json!({"ev": "start", "seeds": ["sym:rust:m::seed/fn"]}),
                json!({"ev": "hop", "src": "sym:rust:m::seed/fn", "dst": "sym:rust:m::mid/fn",
                       "kind": "calls_or_refs", "map": "graph.scip"}),
                json!({"ev": "hop", "src": "sym:rust:m::seed/fn", "dst": "sym:rust:m::other/fn",
                       "kind": "contains", "map": "graph.scip"}),
            ];
            let trace: String = events.iter().map(|e| format!("{e}\n")).collect();
            std::fs::write(out.join("cli/traces").join(format!("{id}.jsonl")), trace).unwrap();
            let card = |id: &str, line: u64| json!({"node": id, "site": {"path": "src/a.rs", "line": line}});
            let raw_json = json!({
                "trace": {"file": format!("{id}.jsonl")},
                "checkpoint": {"answer": [
                    card("sym:rust:m::mid/fn", 5),
                    card("sym:rust:m::other/fn", 50),
                ]},
            });
            std::fs::write(raw.join(raw_name(id)), raw_json.to_string()).unwrap();
        }
        let gold_dir = root.join("gold");
        std::fs::create_dir_all(&gold_dir).unwrap();
        std::fs::write(gold_dir.join("flow.json"), Value::Array(items).to_string()).unwrap();

        let fitted = fit_runs(
            std::slice::from_ref(&out),
            &gold_dir,
            30,
            "",
            &HashMap::new(),
        )
        .unwrap();
        let held: BTreeSet<&String> = ids.iter().filter(|id| is_holdout(id, 30)).collect();
        assert_eq!(fitted.holdout.iter().collect::<BTreeSet<_>>(), held);
        let count = |kind| {
            let pairs = &fitted.fit.counts()["flow"];
            pairs[&("graph.scip".to_owned(), kind)]
        };
        let kept = (ids.len() - held.len()) as u64;
        use lci_maps::EdgeKind;
        assert_eq!(count(EdgeKind::CallsOrRefs).n, kept);
        assert_eq!(count(EdgeKind::CallsOrRefs).positives, kept);
        assert_eq!(count(EdgeKind::Contains).positives, 0);
        assert_eq!(fitted.stats["flow"].items as u64, kept);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
