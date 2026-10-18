//! `cargo xtask sim`: a deterministic simulated agent over a frozen tree.
//! Arm A0 is the grep-only chain; A1 and A2 put one `lci` call (search, or a
//! SCIP-backed traversal) in front of the same chain. See
//! `docs/spike/multimap/v2/sim-agent.md`.
//!
//! * `terms`: query terms to probe and a cached grep over `live_grep::scan`.
//! * `follow`: identifier extraction (tree-sitter) and the frontier policies.
//! * `sim_loop`: the loop, the budgets and the trace.
//! * `arms`: the opening call of A1 and A2.
//! * `score`: gold matching and the per-item scores.

mod arms;
mod follow;
mod score;
mod sim_loop;
mod terms;

use crate::gold::{self, Set};
use crate::maps::baseline::write_arm_run;
use crate::maps::bench::{CliTraverser, ItemFlags, default_lci, set_name, slug as item_slug};
use anyhow::{Context, Result};
use arms::{CliSearch, Opener};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use follow::Policy;
use score::{GoldTarget, ItemScore};
use sim_loop::{SimConfig, run_question};
use terms::Searcher;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Arm {
    /// Grep-only: probe the question's terms, search, read, follow names.
    A0,
    /// `lci search` first (one call, its snippets read), then the A0 chain.
    A1,
    /// `lci traverse --scip` first (one call, 40 lines read at each site),
    /// then the A0 chain. Needs a SCIP index of the tree.
    A2,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::A0 => "A0 grep-only",
            Arm::A1 => "A1 lci-search-first",
            Arm::A2 => "A2 lci-multimap-walk",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Arm::A0 => "a0",
            Arm::A1 => "a1",
            Arm::A2 => "a2",
        }
    }
}

#[derive(Args)]
pub struct SimArgs {
    /// The gold set to run.
    #[arg(long, value_enum)]
    set: Set,
    /// Directory holding the set's gold file (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The frozen tree the questions are about. For A1 it must be indexed
    /// (`lci index`) and the embedder must be up.
    #[arg(long)]
    tree: PathBuf,
    #[arg(long, value_enum, default_value = "a0")]
    arm: Arm,
    /// Frontier order: `greedy` (rarest first, depth-first) or `bfs`.
    #[arg(long, value_enum, default_value = "greedy")]
    policy: Policy,
    /// Tool calls allowed per question (the `lci` call of A1 and A2 counts).
    #[arg(long, default_value_t = 25)]
    max_calls: u32,
    /// Tokens of tool output allowed per question (characters / 4).
    #[arg(long, default_value_t = 20_000)]
    max_tokens: u64,
    /// Query terms probed (the longest ones), one call each.
    #[arg(long, default_value_t = 6)]
    probe_terms: usize,
    /// Files read after the first search (and, for A2, after the `lci`
    /// call: the first files of the returned sites).
    #[arg(long, default_value_t = 3)]
    read_files: usize,
    /// Stop each question at the first read that hits the gold. Gold-aware:
    /// measures calls to the gold, not what a gold-blind agent would do.
    #[arg(long)]
    stop_at_gold: bool,
    /// The `lci` binary (A1, A2); default the newest build under `target/`.
    #[arg(long)]
    lci: Option<PathBuf>,
    /// `--config` for `lci search` (A1), when the index is not in the default
    /// data directory.
    #[arg(long)]
    lci_config: Option<PathBuf>,
    /// The SCIP index of the tree (A2; the arm is refused without it).
    #[arg(long)]
    scip: Option<PathBuf>,
    /// Maps for `lci traverse` (A2), comma separated; unset leaves the
    /// choice to the CLI.
    #[arg(long, value_delimiter = ',')]
    maps: Vec<String>,
    /// Results the `lci` call returns (`--top-k` of search and traverse).
    #[arg(long, default_value_t = 10)]
    lci_top_k: usize,
    /// Longest one `lci` call may run, in seconds.
    #[arg(long, default_value_t = 300)]
    lci_timeout_s: u64,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/<set>`.
    /// Default `sim-<arm>`.
    #[arg(long)]
    wave: Option<String>,
    /// Run only the first N items.
    #[arg(long)]
    limit: Option<usize>,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

/// An item left out of the run because its `lci` call failed.
#[derive(Debug, PartialEq, Eq)]
struct Skipped {
    id: String,
    reason: String,
}

pub fn run(args: SimArgs) -> Result<()> {
    let gold_path = args.gold_dir.join(format!("{}.json", set_name(args.set)));
    let items = select_items(args.set, gold::load_gold(&gold_path)?, args.limit);
    let cfg = SimConfig {
        policy: args.policy,
        max_calls: args.max_calls,
        max_tokens: args.max_tokens,
        probe_terms: args.probe_terms,
        read_files: args.read_files,
        stop_at_gold: args.stop_at_gold,
        ..SimConfig::default()
    };
    let wave = args
        .wave
        .clone()
        .unwrap_or_else(|| format!("sim-{}", args.arm.slug()));
    let dir = args.out_dir.join(slug(&wave)).join(set_name(args.set));
    reject_run_dir_in_tree(&args.tree, &dir)?;
    let timeout = Duration::from_secs(args.lci_timeout_s);

    let (search, traverse);
    let opener = match args.arm {
        Arm::A0 => Opener::Grep,
        Arm::A1 => {
            search = CliSearch {
                lci: lci_binary(&args)?,
                workspace: args.tree.clone(),
                config: args.lci_config.clone(),
                top_k: args.lci_top_k,
                timeout,
            };
            let settings = json!({
                "tool": "lci search", "top_k": args.lci_top_k,
                "config": args.lci_config.is_some(),
                "snippets": "chunk code of each result; its span is read as one window",
            });
            Opener::Search {
                tool: &search,
                settings,
            }
        }
        Arm::A2 => {
            let scip = arms::require_scip(args.scip.as_deref())?;
            traverse = CliTraverser {
                lci: lci_binary(&args)?,
                scip,
                tree: args.tree.clone(),
                maps: args.maps.clone(),
                policy: None,
                policy_dir: None,
                max_depth: None,
                fusion: None,
                pass_through: false,
                rerank_frontier: false,
                no_score_bound: false,
                priors: None,
                tie_break: None,
                first_stage: None,
                first_k: None,
                answer_seeds: false,
                exact_seeds: false,
                top_k: args.lci_top_k,
                out: dir.join("cli"),
                timeout,
                probe: std::cell::RefCell::new(None),
            };
            let maps = if args.maps.is_empty() {
                json!("chosen by the CLI")
            } else {
                json!(args.maps)
            };
            let settings = json!({
                "tool": "lci traverse --scip", "top_k": args.lci_top_k, "maps": maps,
                "sites": "40-line windows, one read call per file, the first read_files files",
            });
            Opener::Traverse {
                tool: &traverse,
                settings,
                traces: Some(dir.join("cli").join("traces")),
            }
        }
    };

    let mut searcher = Searcher::new(&args.tree);
    let (scored, skipped) = simulate(args.set, &items, &mut searcher, &cfg, &opener);
    write_run(
        &dir,
        &RunInfo {
            set: args.set,
            arm: args.arm,
            cfg: &cfg,
            opener: &opener,
            searcher: &searcher,
        },
        &scored,
        &skipped,
    )?;
    let scores: Vec<&ItemScore> = scored.iter().map(|(_, s)| s).collect();
    let summary = score::aggregate(&scores);
    eprintln!(
        "sim {} {} on {}: {} items; hit@12 {}, hit@25 {}, mean calls {}, mean tokens {}\nrun dir: {}",
        args.arm.name(),
        cfg.policy.name(),
        set_name(args.set),
        scores.len(),
        show(&summary["hit_at_12"]),
        show(&summary["hit_at_25"]),
        show(&summary["mean_tool_calls"]),
        show(&summary["mean_tokens_read"]),
        dir.display()
    );
    if let Some(first) = skipped.first() {
        eprintln!(
            "warning: {} items skipped because the lci call failed (first: {}: {})",
            skipped.len(),
            first.id,
            first.reason
        );
    }
    let docs_warned = scored
        .iter()
        .filter(|(_, s)| {
            s.tool
                .as_ref()
                .is_some_and(|t| t["docs_warning"].is_string())
        })
        .count();
    if docs_warned > 0 {
        eprintln!(
            "warning: the docs lane failed on {docs_warned} items (see `docs_lane_warnings` in meta.json); they ran on their code results"
        );
    }
    if searcher.incomplete_scans > 0 {
        eprintln!(
            "warning: {} scans stopped on a deadline or byte cap; the run may not repeat",
            searcher.incomplete_scans
        );
    }
    Ok(())
}

/// `path` with `.` and `..` applied by name only.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` made absolute with symlinks and `.`/`..` resolved, also when its
/// last components do not exist yet (the run dir is created later).
fn resolve_path(path: &Path) -> Result<PathBuf> {
    let absolute =
        std::path::absolute(path).with_context(|| format!("make {} absolute", path.display()))?;
    let existing_start = absolute.clone();
    let mut existing = absolute;
    let mut tail = Vec::new();
    while !existing.exists() {
        let last = existing
            .components()
            .next_back()
            .map(|c| c.as_os_str().to_owned());
        if !existing.pop() {
            // Not even the root exists (a drive that is not there): nothing
            // to resolve, so the lexical form is the answer.
            return Ok(lexical(&existing_start));
        }
        tail.extend(last);
    }
    let mut resolved = existing
        .canonicalize()
        .with_context(|| format!("resolve {}", existing.display()))?;
    for part in tail.into_iter().rev() {
        if part == ".." {
            resolved.pop();
        } else if part != "." {
            resolved.push(part);
        }
    }
    Ok(resolved)
}

/// A run dir inside the frozen tree would be scanned by the next grep (and by
/// the next arm), so questions and results would change the candidates.
fn reject_run_dir_in_tree(tree: &Path, dir: &Path) -> Result<()> {
    let (tree_abs, dir_abs) = (resolve_path(tree)?, resolve_path(dir)?);
    if dir_abs.starts_with(&tree_abs) {
        anyhow::bail!(
            "the run dir {} is inside --tree {}; its files would be scanned as source; use an --out-dir outside the tree",
            dir_abs.display(),
            tree_abs.display()
        );
    }
    Ok(())
}

fn lci_binary(args: &SimArgs) -> Result<PathBuf> {
    match &args.lci {
        Some(path) => Ok(path.clone()),
        None => default_lci(),
    }
}

fn show(value: &Value) -> String {
    value
        .as_f64()
        .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.3}"))
}

fn slug(text: &str) -> String {
    let s: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if s.is_empty() { "wave".into() } else { s }
}

/// The scored population of a set (impact keeps its `primary` items), cut to
/// the first `limit`. The same rule as `maps bench`.
fn select_items(set: Set, items: Vec<Value>, limit: Option<usize>) -> Vec<Value> {
    let kept = items.into_iter().filter(|item| match set {
        Set::Impact => item.get("primary").and_then(Value::as_bool) == Some(true),
        _ => true,
    });
    match limit {
        Some(n) => kept.take(n).collect(),
        None => kept.collect(),
    }
}

/// Runs every item and scores it; deterministic for a given tree, config and
/// tool output. An item whose `lci` call fails is skipped and returned apart
/// (never run as if the call had found nothing).
fn simulate<'a>(
    set: Set,
    items: &'a [Value],
    searcher: &mut Searcher,
    cfg: &SimConfig,
    opener: &Opener,
) -> (Vec<(&'a Value, ItemScore)>, Vec<Skipped>) {
    let mut scored = Vec::new();
    let mut skipped = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let id = item["id"].as_str().unwrap_or_default();
        let question = item["question"].as_str().unwrap_or_default();
        match opener.open(id, question) {
            Ok(opening) => {
                let target = GoldTarget::from_item(set, item);
                let trace = run_question(searcher, cfg, question, Some(&target), opening.as_ref());
                let mut score = ItemScore::from_trace(id, trace, &target);
                if let Some(opening) = &opening {
                    score.tool = opening.info.clone();
                    score.raw = opening.raw.clone();
                }
                scored.push((item, score));
            }
            Err(reason) => skipped.push(Skipped {
                id: id.to_owned(),
                reason,
            }),
        }
        if (index + 1) % 10 == 0 {
            eprintln!("sim: {}/{} done", index + 1, items.len());
        }
    }
    (scored, skipped)
}

/// What a run dir records about how it was made.
struct RunInfo<'a> {
    set: Set,
    arm: Arm,
    cfg: &'a SimConfig,
    opener: &'a Opener<'a>,
    searcher: &'a Searcher,
}

fn write_raw(dir: &Path, id: &str, raw: &Value) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", item_slug(id)));
    std::fs::write(&path, serde_json::to_string_pretty(raw)? + "\n")
        .with_context(|| format!("write {}", path.display()))
}

fn remove_raw(dir: &Path, id: &str) -> Result<()> {
    let path = dir.join(format!("{}.json", item_slug(id)));
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Writes the run dir `maps compare` reads: `run.json` (the text each item
/// read, as `{path, line}`), `items.json`, `score.json` and `meta.json` from
/// `maps::baseline::write_arm_run` (`expansions` = tool calls), then the sim
/// numbers merged into `items.json`, and `sim.json` (config, aggregate, every
/// item with its trace).
fn write_run(
    dir: &Path,
    info: &RunInfo,
    scored: &[(&Value, ItemScore)],
    skipped: &[Skipped],
) -> Result<()> {
    let RunInfo {
        set,
        arm,
        cfg,
        opener,
        searcher,
    } = *info;
    let pairs: Vec<(&Value, Vec<Value>)> = scored
        .iter()
        .map(|(item, score)| (*item, score.answers_json()))
        .collect();
    let calls: Vec<u64> = scored
        .iter()
        .map(|(_, score)| u64::from(score.tool_calls))
        .collect();
    let zeros = vec![0u64; scored.len()];
    let flags: Vec<ItemFlags> = scored
        .iter()
        .filter_map(|(_, score)| score.raw.as_ref().map(ItemFlags::from_raw))
        .collect();
    let meta = json!({
        "arm": format!("simulated agent, {}", arm.name()),
        "gold_aware": cfg.stop_at_gold,
        "policy": cfg.policy.name(),
        "items": scored.len(),
        "opening": opener.describe(),
        "skipped_lci_failures": skipped
            .iter()
            .map(|s| json!({ "id": s.id, "reason": s.reason }))
            .collect::<Vec<_>>(),
        // A1: the docs lane failed open; the item ran on its code results.
        "docs_lane_warnings": scored
            .iter()
            .filter_map(|(_, score)| {
                let warning = score.tool.as_ref()?["docs_warning"].as_str()?;
                Some(json!({ "id": score.id, "warning": warning }))
            })
            .collect::<Vec<_>>(),
        "deadline_stops": flags.iter().filter(|f| f.deadline).count(),
        "flagged_items": flags.iter().filter(|f| f.any()).count(),
        "incomplete_scans": searcher.incomplete_scans,
        "tokens": "characters / 4, rounded up, as maps bench",
        "note": "expansions = tool calls (the lci call counts); model_calls = 0 (embedder and reranker calls inside lci are not counted); run.json is the reading list (first line of each window read), so the gold scorer's own metrics are a by-product and the sim numbers are the result",
    });
    write_arm_run(dir, set, &pairs, &calls, &zeros, &meta)?;

    // The sim numbers join the scorer's metrics in items.json.
    let items_path = dir.join("items.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&items_path)?)
        .with_context(|| format!("read {}", items_path.display()))?;
    for (_, score) in scored {
        if let (Some(record), Value::Object(numbers)) =
            (doc["items"].get_mut(&score.id), score.metrics_json())
        {
            for (key, value) in numbers {
                record[&key] = value;
            }
            record["lci_failure"] = json!(0);
        }
    }
    // A skipped item (its lci call failed) is written with null metrics and a
    // failure flag, and a raw file `maps compare` reads as a map failure, so
    // compare counts it as excluded and not as missing.
    let raw_dir = dir.join("raw");
    for skip in skipped {
        doc["items"][&skip.id] = json!({
            "tool_calls": null, "tokens_read": null, "calls_to_gold": null,
            "tokens_to_gold": null, "hit_at_12": null, "hit_at_25": null,
            "listed_at": null, "expansions": null, "model_calls": null,
            "lci_failure": 1,
        });
        write_raw(
            &raw_dir,
            &skip.id,
            &json!({
                "skipped": skip.reason,
                "checkpoint": {"maps": [{"failures": 1}]},
            }),
        )?;
    }
    // The raw reply of each A2 traversal, for the flags compare reads
    // (deadline stop, map and reranker failures).
    for (_, score) in scored {
        match &score.raw {
            Some(raw) => write_raw(&raw_dir, &score.id, raw)?,
            // A rerun that recovered an item must not leave the earlier
            // failure marker for compare to read.
            None => remove_raw(&raw_dir, &score.id)?,
        }
    }
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
    std::fs::write(&items_path, pretty(&doc)?)?;

    let scores: Vec<&ItemScore> = scored.iter().map(|(_, s)| s).collect();
    let full: Vec<Value> = scores.iter().map(|s| s.full_json()).collect();
    std::fs::write(
        dir.join("sim.json"),
        pretty(&json!({
            "arm": arm.name(),
            "set": set_name(set),
            "config": cfg.to_json(),
            "opening": opener.describe(),
            "aggregate": score::aggregate(&scores),
            "skipped_lci_failures": skipped.len(),
            "skipped": skipped
                .iter()
                .map(|s| json!({ "id": s.id, "reason": s.reason }))
                .collect::<Vec<_>>(),
            "items": full,
        }))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maps::bench::{Traversed, Traverser};
    use arms::stubs::{StubSearch, StubTraverse, node, search_reply};
    use arms::{SearchTool, search_opening, traverse_opening};
    use sim_loop::{Opening, Stop};
    use std::cell::Cell;
    use std::fs;

    /// A tree where `quartz zephyr` appears only in `src/a.rs`, which calls
    /// two names that fork: each is defined in a file that calls a leaf in a
    /// file of its own.
    ///
    /// Greedy call order: 2 probes, 1 search, read a.rs (4), then
    /// alpha_branch grep+read (5, 6), deep_alpha_leaf grep+read (7, 8).
    /// Bfs: beta_branch comes before deep_alpha_leaf (grep+read 7, 8), so the
    /// leaf is read at call 10.
    fn fork_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-sim-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        let files = [
            (
                "src/a.rs",
                "// quartz zephyr entry\nfn quartz_entry() {\n    zephyr_helper();\n    alpha_branch();\n    beta_branch();\n}\n",
            ),
            (
                "src/alpha.rs",
                "fn alpha_branch() {\n    deep_alpha_leaf();\n}\n",
            ),
            (
                "src/beta.rs",
                "fn beta_branch() {\n    deep_beta_leaf();\n}\n",
            ),
            ("src/alpha_leaf.rs", "fn deep_alpha_leaf() {}\n"),
            ("src/beta_leaf.rs", "fn deep_beta_leaf() {}\n"),
        ];
        for (path, text) in files {
            fs::write(dir.join(path), text).unwrap();
        }
        dir
    }

    fn cfg(policy: Policy) -> SimConfig {
        SimConfig {
            policy,
            ..SimConfig::default()
        }
    }

    fn leaf_file() -> GoldTarget {
        GoldTarget::new(Vec::new(), ["src/alpha_leaf.rs".to_owned()])
    }

    fn run_with(dir: &Path, cfg: &SimConfig, gold: &GoldTarget) -> ItemScore {
        let mut searcher = Searcher::new(dir);
        let trace = run_question(&mut searcher, cfg, "quartz zephyr", None, None);
        ItemScore::from_trace("t", trace, gold)
    }

    #[test]
    fn two_runs_of_one_configuration_are_equal() {
        let dir = fork_tree("determinism");
        for policy in [Policy::Greedy, Policy::Bfs] {
            let a = run_with(&dir, &cfg(policy), &leaf_file());
            let b = run_with(&dir, &cfg(policy), &leaf_file());
            assert_eq!(a, b, "{policy:?}");
            assert_eq!(a.full_json(), b.full_json());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn greedy_goes_deep_first_and_bfs_goes_wide_first() {
        let dir = fork_tree("order");
        let greedy = run_with(&dir, &cfg(Policy::Greedy), &leaf_file());
        let bfs = run_with(&dir, &cfg(Policy::Bfs), &leaf_file());
        let kinds = |s: &ItemScore| -> Vec<(&'static str, String)> {
            s.trace
                .steps
                .iter()
                .map(|st| (st.kind, st.arg.clone()))
                .collect()
        };
        let greedy_names: Vec<String> = kinds(&greedy)
            .into_iter()
            .filter(|(k, _)| *k == "follow-grep")
            .map(|(_, a)| a)
            .collect();
        let bfs_names: Vec<String> = kinds(&bfs)
            .into_iter()
            .filter(|(k, _)| *k == "follow-grep")
            .map(|(_, a)| a)
            .collect();
        assert_eq!(
            greedy_names,
            [
                "alpha_branch",
                "deep_alpha_leaf",
                "beta_branch",
                "deep_beta_leaf"
            ]
        );
        assert_eq!(
            bfs_names,
            [
                "alpha_branch",
                "beta_branch",
                "deep_alpha_leaf",
                "deep_beta_leaf"
            ]
        );
        assert_eq!(greedy.calls_to_gold, Some(8));
        assert_eq!(bfs.calls_to_gold, Some(10));
        assert!(greedy.tokens_to_gold < bfs.tokens_to_gold);
        assert!(greedy.hit_at_12 && bfs.hit_at_12);
        assert_eq!(greedy.trace.stop, Stop::Exhausted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_call_budget_stops_the_run_before_the_gold() {
        let dir = fork_tree("budget-calls");
        let config = SimConfig {
            max_calls: 5,
            ..cfg(Policy::Greedy)
        };
        let score = run_with(&dir, &config, &leaf_file());
        assert_eq!(score.tool_calls, 5);
        assert_eq!(score.trace.stop, Stop::Calls);
        assert_eq!(score.calls_to_gold, None);
        assert!(!score.hit_at_12 && !score.hit_at_25);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_token_budget_refuses_a_call_that_would_pass_it() {
        let dir = fork_tree("budget-tokens");
        let unlimited = run_with(&dir, &cfg(Policy::Greedy), &leaf_file());
        // The read of a.rs (call 4) is the first big output; allow just less
        // than everything up to and including it.
        let before_read: u64 = unlimited.trace.steps[..3].iter().map(|s| s.tokens).sum();
        let config = SimConfig {
            max_tokens: before_read + unlimited.trace.steps[3].tokens - 1,
            ..cfg(Policy::Greedy)
        };
        let score = run_with(&dir, &config, &leaf_file());
        assert_eq!(score.trace.stop, Stop::Tokens);
        assert_eq!(score.tool_calls, 3);
        assert!(score.tokens_read <= config.max_tokens);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_gold_is_hit_by_span_or_by_file() {
        let dir = fork_tree("gold");
        let by_file = run_with(&dir, &cfg(Policy::Greedy), &leaf_file());
        assert_eq!(by_file.calls_to_gold, Some(8));
        // A span on the line the leaf is defined on.
        let on_span = GoldTarget::new(vec![("src/alpha_leaf.rs".into(), 1, 1)], []);
        let by_span = run_with(&dir, &cfg(Policy::Greedy), &on_span);
        assert_eq!(by_span.calls_to_gold, Some(8));
        // A span in a file the agent reads, but on lines it never shows.
        let off_span = GoldTarget::new(vec![("src/alpha_leaf.rs".into(), 50, 60)], []);
        let missed = run_with(&dir, &cfg(Policy::Greedy), &off_span);
        assert_eq!(missed.calls_to_gold, None);
        assert!(missed.gold_known);
        // The entry file is read at call 4 and listed first in the search.
        let entry = GoldTarget::new(Vec::new(), ["src/a.rs".to_owned()]);
        let early = run_with(&dir, &cfg(Policy::Greedy), &entry);
        assert_eq!((early.calls_to_gold, early.listed_at), (Some(4), Some(1)));
        // A gold file the agent only meets after the search.
        assert_eq!(by_file.listed_at, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hit_at_12_is_the_twelve_call_prefix_of_a_longer_run() {
        let dir = fork_tree("prefix");
        let targets = ["src/a.rs", "src/alpha_leaf.rs", "src/beta_leaf.rs"];
        for policy in [Policy::Greedy, Policy::Bfs] {
            for file in targets {
                let gold = GoldTarget::new(Vec::new(), [file.to_owned()]);
                let long = run_with(&dir, &cfg(policy), &gold);
                let short = run_with(
                    &dir,
                    &SimConfig {
                        max_calls: 12,
                        ..cfg(policy)
                    },
                    &gold,
                );
                assert_eq!(long.hit_at_12, short.calls_to_gold.is_some(), "{file}");
                assert_eq!(
                    long.calls_to_gold.filter(|c| *c <= 12),
                    short.calls_to_gold,
                    "{file}"
                );
            }
        }
        // The boundary: the last leaf is read at call 12 and not within 11.
        let gold = GoldTarget::new(Vec::new(), ["src/beta_leaf.rs".to_owned()]);
        let long = run_with(&dir, &cfg(Policy::Greedy), &gold);
        assert_eq!(long.calls_to_gold, Some(12));
        assert!(long.hit_at_12);
        let eleven = SimConfig {
            max_calls: 11,
            ..cfg(Policy::Greedy)
        };
        assert_eq!(run_with(&dir, &eleven, &gold).calls_to_gold, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_at_gold_ends_the_run_at_the_hit() {
        let dir = fork_tree("stop");
        let config = SimConfig {
            stop_at_gold: true,
            ..cfg(Policy::Greedy)
        };
        let gold = leaf_file();
        let mut searcher = Searcher::new(&dir);
        let trace = run_question(&mut searcher, &config, "quartz zephyr", Some(&gold), None);
        let score = ItemScore::from_trace("t", trace, &gold);
        assert_eq!(score.trace.stop, Stop::Gold);
        assert_eq!(score.tool_calls, 8);
        assert_eq!(score.calls_to_gold, Some(8));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unmatched_or_empty_question_makes_no_reads() {
        let dir = fork_tree("nomatch");
        let mut searcher = Searcher::new(&dir);
        let none = run_question(
            &mut searcher,
            &SimConfig::default(),
            "nonexistentterm",
            None,
            None,
        );
        assert_eq!(none.stop, Stop::NoMatch);
        assert!(none.steps.iter().all(|s| s.kind == "probe"));
        let empty = run_question(&mut searcher, &SimConfig::default(), "to a", None, None);
        assert_eq!((empty.stop, empty.steps.len()), (Stop::NoTerms, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    fn where_to_item(expected: &str) -> Value {
        json!({
            "id": "where-to-t1", "question": "quartz zephyr",
            "gold": {"form": "registration", "expected_files": [expected]}
        })
    }

    /// Runs the A0 chain behind `opening`.
    fn run_after(dir: &Path, cfg: &SimConfig, gold: &GoldTarget, opening: &Opening) -> ItemScore {
        let mut searcher = Searcher::new(dir);
        let trace = run_question(&mut searcher, cfg, "quartz zephyr", None, Some(opening));
        ItemScore::from_trace("t", trace, gold)
    }

    fn kinds(score: &ItemScore) -> Vec<&'static str> {
        score.trace.steps.iter().map(|s| s.kind).collect()
    }

    /// What `lci search` returns for the fork tree: the entry file, whole.
    fn search_stub() -> StubSearch {
        StubSearch {
            reply: Ok(search_reply(&[(
                "src/a.rs",
                1,
                6,
                "// quartz zephyr entry\nfn quartz_entry() {",
            )])),
            calls: Cell::new(0),
        }
    }

    /// What `lci traverse` returns: `alpha_branch` in alpha.rs.
    fn traverse_stub() -> StubTraverse {
        StubTraverse {
            results: vec![node(
                "sym:alpha",
                "fn alpha_branch",
                Some(("src/alpha.rs", 1)),
            )],
            fail: false,
            calls: Cell::new(0),
            raw: json!({}),
        }
    }

    fn a1_opening() -> Opening {
        search_opening(
            "quartz zephyr",
            &search_stub().search("quartz zephyr").unwrap(),
        )
    }

    fn a2_opening() -> Opening {
        let (traversed, _) = traverse_stub().traverse("t", "quartz zephyr").unwrap();
        traverse_opening("quartz zephyr", &traversed)
    }

    #[test]
    fn a1_reads_the_snippet_spans_in_its_one_call_then_follows_a0() {
        let dir = fork_tree("a1");
        let a0 = run_with(&dir, &cfg(Policy::Greedy), &leaf_file());
        let a1 = run_after(&dir, &cfg(Policy::Greedy), &leaf_file(), &a1_opening());
        assert_eq!(kinds(&a1)[..2], ["lci-search", "probe"]);
        let first = &a1.trace.steps[0];
        assert_eq!(first.call, 1);
        assert_eq!(first.windows.len(), 1);
        let w = &first.windows[0];
        assert_eq!((w.path.as_str(), w.start, w.end), ("src/a.rs", 1, 6));
        // The same chain, except that its first read of src/a.rs is dropped:
        // the opening already showed those lines. So the call count is A0's
        // (one call more, one read fewer).
        let mut chain = kinds(&a0);
        let read = chain.iter().position(|k| *k == "read").unwrap();
        chain.remove(read);
        assert_eq!(kinds(&a1)[1..], chain[..]);
        assert!(!kinds(&a1).contains(&"read"));
        assert_eq!(a1.tool_calls, a0.tool_calls);
        assert_eq!(a1.calls_to_gold, Some(8));
        assert_eq!(a1.calls_to_gold, a0.calls_to_gold);
        // The snippet is gold-bearing at call 1 when the gold is its file.
        let entry = GoldTarget::new(Vec::new(), ["src/a.rs".to_owned()]);
        let early = run_after(&dir, &cfg(Policy::Greedy), &entry, &a1_opening());
        assert_eq!((early.calls_to_gold, early.listed_at), (Some(1), Some(1)));
        // A gold span off the snippet lines is no hit.
        let off = GoldTarget::new(vec![("src/a.rs".into(), 40, 50)], []);
        let missed = run_after(&dir, &cfg(Policy::Greedy), &off, &a1_opening());
        assert_eq!(missed.listed_at, None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An A2 opening whose sites are these files (line 1 each).
    fn sites_opening(sites: &[(&str, u64)]) -> Opening {
        let traversed = Traversed {
            results: sites
                .iter()
                .enumerate()
                .map(|(i, (path, line))| node(&format!("sym:{i}"), "fn x", Some((path, *line))))
                .collect(),
            ..Traversed::default()
        };
        traverse_opening("quartz zephyr", &traversed)
    }

    fn site_reads(score: &ItemScore) -> Vec<&str> {
        score
            .trace
            .steps
            .iter()
            .filter(|s| s.kind == "site-read")
            .map(|s| s.windows[0].path.as_str())
            .collect()
    }

    #[test]
    fn a_site_that_is_not_readable_costs_no_call_and_takes_no_file_slot() {
        let dir = fork_tree("sites");
        // A missing file and a line past the end come first; four files can
        // be read, and the default cap is three files.
        let opening = sites_opening(&[
            ("src/missing.rs", 1),
            ("src/a.rs", 99),
            ("src/alpha.rs", 1),
            ("src/beta.rs", 1),
            ("src/alpha_leaf.rs", 1),
            ("src/beta_leaf.rs", 1),
        ]);
        let three = run_after(&dir, &cfg(Policy::Greedy), &leaf_file(), &opening);
        assert_eq!(
            site_reads(&three),
            ["src/alpha.rs", "src/beta.rs", "src/alpha_leaf.rs"]
        );
        let wide = SimConfig {
            read_files: 10,
            ..cfg(Policy::Greedy)
        };
        let four = run_after(&dir, &wide, &leaf_file(), &opening);
        assert_eq!(
            site_reads(&four),
            [
                "src/alpha.rs",
                "src/beta.rs",
                "src/alpha_leaf.rs",
                "src/beta_leaf.rs"
            ]
        );
        for score in [&three, &four] {
            for step in &score.trace.steps {
                assert!(
                    step.windows
                        .iter()
                        .all(|w| w.path != "src/missing.rs"
                            && !(w.path == "src/a.rs" && w.start > 6)),
                    "{:?}",
                    step.kind
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_opening_listed_nothing_so_listed_at_stays_null() {
        let dir = fork_tree("empty-opening");
        let empty = search_opening("quartz zephyr", &json!({ "results": [] }));
        let entry = GoldTarget::new(Vec::new(), ["src/a.rs".to_owned()]);
        let score = run_after(&dir, &cfg(Policy::Greedy), &entry, &empty);
        assert_eq!(kinds(&score)[0], "lci-search");
        // The A0 search that follows lists src/a.rs first and reads it, but
        // it is not the listing the opening showed.
        assert!(score.calls_to_gold.is_some());
        assert_eq!(score.listed_at, None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `x.rs` and `y.rs` both hold the question's terms; each names a
    /// function defined in a file of its own.
    fn two_reads_tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-sim-reads-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        let files = [
            (
                "src/x.rs",
                "// quartz zephyr one\nfn entry_x() {\n    first_target_fn();\n}\n",
            ),
            (
                "src/y.rs",
                "// quartz zephyr two\nfn entry_y() {\n    second_target_fn();\n}\n",
            ),
            ("src/z1.rs", "fn first_target_fn() {}\n"),
            ("src/z2.rs", "fn second_target_fn() {}\n"),
        ];
        for (path, text) in files {
            fs::write(dir.join(path), text).unwrap();
        }
        dir
    }

    #[test]
    fn the_candidate_limit_and_the_policy_order_apply_per_first_read() {
        let dir = two_reads_tree();
        // Two names per read: entry_x and first_target_fn come from x.rs,
        // entry_y and second_target_fn from y.rs. One batch for both reads
        // would cut the second file's names away.
        let followed = |policy, opening: Option<&Opening>| -> Vec<String> {
            let config = SimConfig {
                max_candidates: 2,
                ..cfg(policy)
            };
            let mut searcher = Searcher::new(&dir);
            let trace = run_question(&mut searcher, &config, "quartz zephyr", None, opening);
            ItemScore::from_trace("t", trace, &leaf_file())
                .trace
                .steps
                .iter()
                .filter(|s| s.kind == "follow-grep")
                .map(|s| s.arg.clone())
                .collect()
        };
        // A0: the chain's first reads in read order, whatever the policy.
        for policy in [Policy::Bfs, Policy::Greedy] {
            assert_eq!(
                followed(policy, None),
                ["first_target_fn", "second_target_fn"],
                "{policy:?}"
            );
        }
        // Behind an opening each arm follows its own evidence first, under
        // both policies: A1's snippet of y.rs, or A2's site read of y.rs,
        // comes before the chain's read of x.rs.
        let a1 = search_opening(
            "quartz zephyr",
            &search_reply(&[("src/y.rs", 1, 4, "fn entry_y() {")]),
        );
        let a2 = sites_opening(&[("src/y.rs", 1)]);
        for opening in [&a1, &a2] {
            for policy in [Policy::Bfs, Policy::Greedy] {
                assert_eq!(
                    followed(policy, Some(opening)),
                    ["second_target_fn", "first_target_fn"],
                    "{} {policy:?}",
                    opening.kind
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_differing_only_in_case_take_one_candidate_slot() {
        let dir = std::env::temp_dir().join(format!("xtask-sim-case-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        let files = [
            (
                "src/x.rs",
                "// quartz zephyr\nfn entry_x() {\n    Target_fn();\n    target_fn();\n    other_fn();\n}\n",
            ),
            ("src/t1.rs", "fn Target_fn() {}\n"),
            ("src/t2.rs", "fn target_fn() {}\n"),
            ("src/o.rs", "fn other_fn() {}\n"),
        ];
        for (path, text) in files {
            fs::write(dir.join(path), text).unwrap();
        }
        let config = SimConfig {
            max_candidates: 3,
            ..cfg(Policy::Bfs)
        };
        let mut searcher = Searcher::new(&dir);
        let trace = run_question(&mut searcher, &config, "quartz zephyr", None, None);
        let followed: Vec<String> = ItemScore::from_trace("t", trace, &leaf_file())
            .trace
            .steps
            .iter()
            .filter(|s| s.kind == "follow-grep")
            .map(|s| s.arg.to_lowercase())
            .collect();
        // Before the fix Target_fn and target_fn took two of the three slots
        // and cut other_fn away.
        assert!(followed.iter().any(|n| n == "other_fn"), "{followed:?}");
        assert_eq!(
            followed.iter().filter(|n| *n == "target_fn").count(),
            1,
            "{followed:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_dir_inside_the_tree_is_refused_by_resolved_path() {
        let tree = fork_tree("rundir-in-tree");
        let inside = tree.join("out").join("sim-a0").join("where-to");
        let err = reject_run_dir_in_tree(&tree, &inside)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside --tree"), "{err}");
        // The tree itself, and a path that climbs out and back in.
        assert!(reject_run_dir_in_tree(&tree, &tree).is_err());
        let wander = tree
            .join("src")
            .join("..")
            .join("new")
            .join("..")
            .join("run");
        assert!(reject_run_dir_in_tree(&tree, &wander).is_err());
        // Beside the tree, or reached by `..` from inside it, is fine.
        let beside = PathBuf::from(format!("{}-out", tree.display())).join("a0");
        assert!(reject_run_dir_in_tree(&tree, &beside).is_ok());
        let climbs_out = tree.join("..").join("elsewhere");
        assert!(reject_run_dir_in_tree(&tree, &climbs_out).is_ok());
        // A tree path with a different spelling of the same dir.
        let respelled = tree.join("src").join("..");
        assert!(reject_run_dir_in_tree(&respelled, &inside).is_err());
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn a_path_whose_root_does_not_exist_resolves_without_hanging() {
        // A drive that is not there (Windows), or a missing top-level dir.
        let missing = if cfg!(windows) {
            PathBuf::from(r"Q:\xtask-no-such-drive\out\..\run")
        } else {
            PathBuf::from("/xtask-no-such-top-level-dir/out/../run")
        };
        if cfg!(windows) && Path::new("Q:\\").exists() {
            return; // that drive exists here
        }
        let (send, receive) = std::sync::mpsc::channel();
        let tree = fork_tree("missing-root");
        let probe = missing.clone();
        let probe_tree = tree.clone();
        std::thread::spawn(move || {
            let resolved = resolve_path(&probe).map(|p| p.to_string_lossy().into_owned());
            let verdict = reject_run_dir_in_tree(&probe_tree, &probe).is_ok();
            let _ = send.send((resolved, verdict));
        });
        let (resolved, outside_tree) = receive
            .recv_timeout(Duration::from_secs(20))
            .expect("resolve_path must return for a missing root");
        let resolved = resolved.unwrap();
        assert!(
            resolved.ends_with("run") && !resolved.contains(".."),
            "{resolved}"
        );
        assert!(outside_tree);
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn a_docs_lane_warning_reaches_the_run_files_and_does_not_flag_the_item() {
        let dir = fork_tree("docs-warning");
        let outside = PathBuf::from(format!("{}-out", dir.display()));
        let config = cfg(Policy::Greedy);
        let items = [where_to_item("src/alpha_leaf.rs")];
        let mut reply = search_reply(&[("src/a.rs", 1, 6, "// quartz zephyr entry")]);
        reply["docs"] = json!({ "results": [], "warning": "Docs reranker unavailable" });
        let search = StubSearch {
            reply: Ok(reply),
            calls: Cell::new(0),
        };
        let opener = Opener::Search {
            tool: &search,
            settings: json!({}),
        };
        let mut searcher = Searcher::new(&dir);
        let (scored, skipped) = simulate(Set::WhereTo, &items, &mut searcher, &config, &opener);
        let info = RunInfo {
            set: Set::WhereTo,
            arm: Arm::A1,
            cfg: &config,
            opener: &opener,
            searcher: &searcher,
        };
        write_run(&outside, &info, &scored, &skipped).unwrap();
        let read = |name: &str| -> Value {
            serde_json::from_str(&fs::read_to_string(outside.join(name)).unwrap()).unwrap()
        };
        let meta = read("meta.json");
        assert_eq!(
            meta["docs_lane_warnings"],
            json!([{ "id": "where-to-t1", "warning": "Docs reranker unavailable" }])
        );
        assert_eq!(meta["skipped_lci_failures"], json!([]));
        assert_eq!(
            read("sim.json")["items"][0]["tool"]["docs_warning"],
            "Docs reranker unavailable"
        );
        let data = crate::maps::compare::load_run(&outside, None).unwrap();
        assert_eq!(data.items["where-to-t1"]["lci_failure"], Some(0.0));
        assert_eq!(data.flags["where-to-t1"], None);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_rerun_that_recovered_an_item_drops_its_stale_failure_marker() {
        let dir = fork_tree("stale-raw");
        let outside = PathBuf::from(format!("{}-out", dir.display()));
        let out = outside.join("a2");
        let config = cfg(Policy::Greedy);
        let items = [where_to_item("src/alpha_leaf.rs")];
        let traverse = traverse_stub();
        let opener = Opener::Traverse {
            tool: &traverse,
            settings: json!({}),
            traces: None,
        };
        let failed = || {
            vec![Skipped {
                id: "where-to-t1".into(),
                reason: "stub failure".into(),
            }]
        };
        // First run: the lci call failed, which leaves a map-failure marker.
        let mut searcher = Searcher::new(&dir);
        let info = RunInfo {
            set: Set::WhereTo,
            arm: Arm::A2,
            cfg: &config,
            opener: &opener,
            searcher: &searcher,
        };
        write_run(&out, &info, &[], &failed()).unwrap();
        let first = crate::maps::compare::load_run(&out, None).unwrap();
        assert!(first.flags["where-to-t1"].is_some_and(|f| f.any()));
        // Rerun: the item succeeded, with no raw reply (A0/A1 style).
        let (scored, _) = simulate(Set::WhereTo, &items, &mut searcher, &config, &Opener::Grep);
        assert!(scored[0].1.raw.is_none());
        let grep = Opener::Grep;
        let info = RunInfo {
            set: Set::WhereTo,
            arm: Arm::A0,
            cfg: &config,
            opener: &grep,
            searcher: &searcher,
        };
        write_run(&out, &info, &scored, &[]).unwrap();
        assert!(!out.join("raw").join("where-to-t1.json").exists());
        let again = crate::maps::compare::load_run(&out, None).unwrap();
        assert_eq!(again.items["where-to-t1"]["lci_failure"], Some(0.0));
        assert_eq!(again.flags["where-to-t1"], None);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_skipped_item_is_excluded_by_compare_and_not_counted_as_missing() {
        let dir = fork_tree("skipped");
        let outside = PathBuf::from(format!("{}-out", dir.display()));
        let config = cfg(Policy::Greedy);
        let items = [
            where_to_item("src/alpha_leaf.rs"),
            json!({"id": "where-to-t2", "question": "zephyr",
                "gold": {"form": "registration", "expected_files": ["src/a.rs"]}}),
        ];
        let write = |arm: Arm, opener: &Opener, items: &[Value], skipped: Vec<Skipped>| {
            let mut searcher = Searcher::new(&dir);
            let (scored, _) = simulate(Set::WhereTo, items, &mut searcher, &config, opener);
            let out = outside.join(arm.slug());
            let info = RunInfo {
                set: Set::WhereTo,
                arm,
                cfg: &config,
                opener,
                searcher: &searcher,
            };
            write_run(&out, &info, &scored, &skipped).unwrap();
            crate::maps::compare::load_run(&out, None).unwrap()
        };
        let a = write(Arm::A0, &Opener::Grep, &items, Vec::new());
        let traverse = traverse_stub();
        let opener = Opener::Traverse {
            tool: &traverse,
            settings: json!({}),
            traces: None,
        };
        // The second item's lci call failed.
        let b = write(
            Arm::A2,
            &opener,
            &items[..1],
            vec![Skipped {
                id: "where-to-t2".into(),
                reason: "stub failure".into(),
            }],
        );
        assert_eq!(b.items["where-to-t2"]["lci_failure"], Some(1.0));
        assert_eq!(b.items["where-to-t2"]["tool_calls"], None);
        assert!(b.flags["where-to-t2"].is_some_and(|f| f.any()));
        let metrics = ["calls_to_gold".to_owned()];
        let c = crate::maps::compare::compare(&a, &b, &metrics, false, 100, 1);
        assert_eq!((c.paired, c.only_a, c.only_b), (2, 0, 0));
        assert_eq!((c.excluded, c.excluded_b), (1, 1));
        assert_eq!(c.rows[0].n, 1);
        let meta: Value = serde_json::from_str(
            &fs::read_to_string(outside.join("a2").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["skipped_lci_failures"][0]["id"], "where-to-t2");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn a2_charges_the_call_and_reads_each_site_before_the_a0_chain() {
        let dir = fork_tree("a2");
        let a2 = run_after(&dir, &cfg(Policy::Greedy), &leaf_file(), &a2_opening());
        let steps = &a2.trace.steps;
        assert_eq!(
            kinds(&a2)[..4],
            ["lci-traverse", "site-read", "probe", "probe"]
        );
        assert_eq!(steps[0].listed, [("src/alpha.rs".to_owned(), 1)]);
        assert!(steps[0].windows.is_empty());
        // 40 lines around line 1, cut to the 3-line file.
        let w = &steps[1].windows[0];
        assert_eq!((w.path.as_str(), w.start, w.end), ("src/alpha.rs", 1, 3));
        // The site read's batch is followed before the chain's read of a.rs:
        // traverse (1), site read (2), 2 probes (3, 4), search (5), read a.rs
        // (6); alpha_branch is covered by the site read and a.rs, so the next
        // name is deep_alpha_leaf: grep (7), read (8). That is A0's 8 on this
        // fixture (the A2 call and the site read replace A0's alpha_branch
        // grep and read).
        assert_eq!(a2.calls_to_gold, Some(8));
        let a0 = run_with(&dir, &cfg(Policy::Greedy), &leaf_file());
        assert!(a2.calls_to_gold <= a0.calls_to_gold);
        // The gold in the listed site's file is listed first and read second.
        let alpha = GoldTarget::new(Vec::new(), ["src/alpha.rs".to_owned()]);
        let score = run_after(&dir, &cfg(Policy::Greedy), &alpha, &a2_opening());
        assert_eq!((score.calls_to_gold, score.listed_at), (Some(2), Some(1)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_opening_call_counts_against_the_budgets() {
        let dir = fork_tree("opening-budget");
        let one_call = SimConfig {
            max_calls: 1,
            ..cfg(Policy::Greedy)
        };
        let a2 = run_after(&dir, &one_call, &leaf_file(), &a2_opening());
        assert_eq!((a2.tool_calls, a2.trace.stop), (1, Stop::Calls));
        let tiny = SimConfig {
            max_tokens: 1,
            ..cfg(Policy::Greedy)
        };
        let a1 = run_after(&dir, &tiny, &leaf_file(), &a1_opening());
        assert_eq!((a1.tool_calls, a1.trace.stop), (0, Stop::Tokens));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_shown_by_the_opening_are_followed_when_the_grep_finds_nothing() {
        let dir = fork_tree("opening-names");
        let mut searcher = Searcher::new(&dir);
        let opening = a1_opening();
        let trace = run_question(
            &mut searcher,
            &cfg(Policy::Greedy),
            "nonexistentterm",
            None,
            Some(&opening),
        );
        assert_eq!(trace.steps[0].kind, "lci-search");
        assert!(trace.steps.iter().any(|s| s.kind == "follow-grep"));
        assert_ne!(trace.stop, Stop::NoMatch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_lci_call_per_question_is_made_and_counted() {
        let dir = fork_tree("count");
        let items = [
            where_to_item("src/alpha_leaf.rs"),
            json!({"id": "where-to-t2", "question": "quartz",
                "gold": {"form": "registration", "expected_files": ["src/a.rs"]}}),
            json!({"id": "where-to-t3", "question": "zephyr",
                "gold": {"form": "registration", "expected_files": ["src/a.rs"]}}),
        ];
        let search = search_stub();
        let opener = Opener::Search {
            tool: &search,
            settings: json!({}),
        };
        let config = cfg(Policy::Greedy);
        let mut searcher = Searcher::new(&dir);
        let (scored, skipped) = simulate(Set::WhereTo, &items, &mut searcher, &config, &opener);
        assert_eq!((scored.len(), skipped.len(), search.calls.get()), (3, 0, 3));
        for (_, score) in &scored {
            let calls = score
                .trace
                .steps
                .iter()
                .filter(|s| s.kind == "lci-search")
                .count();
            assert_eq!(calls, 1);
            assert_eq!(score.tool_calls as usize, score.trace.steps.len());
        }
        // A0 makes none; the lci call is one more, and the opening's windows
        // replace A0's first read of the same lines, so the counts match.
        let (a0, _) = simulate(Set::WhereTo, &items, &mut searcher, &config, &Opener::Grep);
        let lci_steps = a0
            .iter()
            .flat_map(|(_, s)| &s.trace.steps)
            .filter(|st| st.kind.starts_with("lci"))
            .count();
        assert_eq!(lci_steps, 0);
        assert_eq!(scored[0].1.tool_calls, a0[0].1.tool_calls);

        // A failed call skips the item and says why; nothing is scored.
        let broken = StubTraverse {
            fail: true,
            ..traverse_stub()
        };
        let opener = Opener::Traverse {
            tool: &broken,
            settings: json!({}),
            traces: None,
        };
        let (scored, skipped) = simulate(Set::WhereTo, &items, &mut searcher, &config, &opener);
        assert_eq!((scored.len(), skipped.len(), broken.calls.get()), (0, 3, 3));
        assert_eq!(skipped[0].id, "where-to-t1");
        assert!(skipped[0].reason.contains("stub failure"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_runs_of_an_lci_arm_are_equal() {
        let dir = fork_tree("lci-determinism");
        let items = [where_to_item("src/alpha_leaf.rs")];
        let (search, traverse) = (search_stub(), traverse_stub());
        let openers = [
            Opener::Search {
                tool: &search,
                settings: json!({}),
            },
            Opener::Traverse {
                tool: &traverse,
                settings: json!({}),
                traces: None,
            },
        ];
        for opener in &openers {
            for policy in [Policy::Greedy, Policy::Bfs] {
                let run = || {
                    let mut searcher = Searcher::new(&dir);
                    simulate(Set::WhereTo, &items, &mut searcher, &cfg(policy), opener).0
                };
                let (a, b) = (run(), run());
                assert_eq!((a.len(), b.len()), (1, 1));
                assert_eq!(a[0].1, b[0].1, "{policy:?}");
                assert_eq!(a[0].1.full_json(), b[0].1.full_json());
                assert_eq!(a[0].1.answers_json(), b[0].1.answers_json());
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_run_dir_is_read_by_compare() {
        let dir = fork_tree("rundir");
        let items = [where_to_item("src/alpha_leaf.rs")];
        let search = search_stub();
        // The A2 site is the gold file itself, so A2 reaches it at call 2 and a
        // run that treated A2 as A0 would fail; its raw reply stopped on a deadline.
        let traverse = StubTraverse {
            results: vec![node(
                "sym:leaf",
                "fn deep_alpha_leaf",
                Some(("src/alpha_leaf.rs", 1)),
            )],
            raw: json!({"stop_reason": "deadline", "totals": {"rerank_failures": 0}}),
            ..traverse_stub()
        };
        let config = cfg(Policy::Greedy);
        let arms = [
            (Arm::A0, Opener::Grep, "a0", 8.0, "probe"),
            (
                Arm::A1,
                Opener::Search {
                    tool: &search,
                    settings: json!({"tool": "lci search"}),
                },
                "a1",
                8.0,
                "lci-search",
            ),
            (
                Arm::A2,
                Opener::Traverse {
                    tool: &traverse,
                    settings: json!({"tool": "lci traverse --scip"}),
                    traces: None,
                },
                "a2",
                2.0,
                "lci-traverse",
            ),
        ];
        // The run dirs go beside the tree: inside it, the next arm would grep them.
        let outside = &PathBuf::from(format!("{}-out", dir.display()));
        let read = |path: PathBuf| -> Value {
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
        };
        for (arm, opener, name, to_gold, first_step) in &arms {
            let mut searcher = Searcher::new(&dir);
            let (scored, skipped) = simulate(Set::WhereTo, &items, &mut searcher, &config, opener);
            let out = outside.join(name);
            let info = RunInfo {
                set: Set::WhereTo,
                arm: *arm,
                cfg: &config,
                opener,
                searcher: &searcher,
            };
            write_run(&out, &info, &scored, &skipped).unwrap();
            for file in [
                "run.json",
                "items.json",
                "score.json",
                "meta.json",
                "sim.json",
            ] {
                assert!(out.join(file).is_file(), "{name}: {file}");
            }
            let data = crate::maps::compare::load_run(&out, None).unwrap();
            assert_eq!(data.set, "where-to");
            let record = &data.items["where-to-t1"];
            assert_eq!(record["calls_to_gold"], Some(*to_gold), "{name}");
            assert_eq!(record["hit_at_12"], Some(1.0));
            assert_eq!(record["expansions"], record["tool_calls"]);
            // The scorer's own metrics sit beside the sim's.
            assert!(record["precision"].is_some() && record["f0.5"].is_some());
            let run = read(out.join("run.json"));
            let answers = run["where-to-t1"].as_array().unwrap();
            assert!(!answers.is_empty());
            assert!(
                answers
                    .iter()
                    .all(|a| a["path"].is_string() && a["line"].is_u64())
            );
            let sim = read(out.join("sim.json"));
            assert_eq!(sim["items"][0]["trace"]["steps"][0]["kind"], *first_step);
            let meta = read(out.join("meta.json"));
            assert_eq!(meta["arm"], format!("simulated agent, {}", arm.name()));
            assert_eq!(meta["opening"], opener.describe());
            // A2's raw reply is kept where compare reads flags from, so a
            // deadline stop is flagged; the other arms have no raw file.
            let flag = data.flags["where-to-t1"];
            if *arm == Arm::A2 {
                assert!(flag.is_some_and(|f| f.deadline && f.any()), "{name}");
                assert_eq!(meta["deadline_stops"], 1);
                assert_eq!(meta["flagged_items"], 1);
            } else {
                assert_eq!(flag, None, "{name}");
                assert_eq!(meta["deadline_stops"], 0);
            }
            assert_eq!(
                read(out.join("items.json"))["items"]["where-to-t1"]["lci_failure"],
                0
            );
        }
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(outside);
    }
}
