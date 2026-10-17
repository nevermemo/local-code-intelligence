//! `cargo xtask sim`: a deterministic simulated agent over a frozen tree.
//! Arm A0 (grep-only) is implemented here; A1 and A2 (the `lci` tools in
//! front of the same chain) are slice 7b. See
//! `docs/spike/multimap/v2/sim-agent.md`.
//!
//! * `terms`: query terms to probe and a cached grep over `live_grep::scan`.
//! * `follow`: identifier extraction (tree-sitter) and the frontier policies.
//! * `sim_loop`: the loop, the budgets and the trace.
//! * `score`: gold matching and the per-item scores.

mod follow;
mod score;
mod sim_loop;
mod terms;

use crate::gold::{self, Set};
use crate::maps::bench::set_name;
use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub use follow::Policy;
use score::{GoldTarget, ItemScore};
use sim_loop::{SimConfig, run_question};
use terms::Searcher;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Arm {
    /// Grep-only: probe the question's terms, search, read, follow names.
    A0,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::A0 => "A0 grep-only",
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
    /// The frozen tree the questions are about.
    #[arg(long)]
    tree: PathBuf,
    #[arg(long, value_enum, default_value = "a0")]
    arm: Arm,
    /// Frontier order: `greedy` (rarest first, depth-first) or `bfs`.
    #[arg(long, value_enum, default_value = "greedy")]
    policy: Policy,
    /// Tool calls allowed per question.
    #[arg(long, default_value_t = 25)]
    max_calls: u32,
    /// Tokens of tool output allowed per question (characters / 4).
    #[arg(long, default_value_t = 20_000)]
    max_tokens: u64,
    /// Query terms probed (the longest ones), one call each.
    #[arg(long, default_value_t = 6)]
    probe_terms: usize,
    /// Files read after the first search.
    #[arg(long, default_value_t = 3)]
    read_files: usize,
    /// Stop each question at the first read that hits the gold. Gold-aware:
    /// measures calls to the gold, not what a gold-blind agent would do.
    #[arg(long)]
    stop_at_gold: bool,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/<set>`.
    #[arg(long, default_value = "sim-a0")]
    wave: String,
    /// Run only the first N items.
    #[arg(long)]
    limit: Option<usize>,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
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
    let mut searcher = Searcher::new(&args.tree);
    let scores = simulate(args.set, &items, &mut searcher, &cfg);
    let dir = args.out_dir.join(slug(&args.wave)).join(set_name(args.set));
    write_run(&dir, args.set, args.arm, &cfg, &scores, &searcher)?;
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
    if searcher.incomplete_scans > 0 {
        eprintln!(
            "warning: {} scans stopped on a deadline or byte cap; the run may not repeat",
            searcher.incomplete_scans
        );
    }
    Ok(())
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

/// Runs every item and scores it; deterministic for a given tree and config.
fn simulate(set: Set, items: &[Value], searcher: &mut Searcher, cfg: &SimConfig) -> Vec<ItemScore> {
    items
        .iter()
        .map(|item| {
            let id = item["id"].as_str().unwrap_or_default();
            let question = item["question"].as_str().unwrap_or_default();
            let target = GoldTarget::from_item(set, item);
            let trace = run_question(searcher, cfg, question, Some(&target));
            ItemScore::from_trace(id, trace, &target)
        })
        .collect()
}

/// Writes `sim.json` (config, aggregate, every item with its trace),
/// `items.json` (numbers per item, the shape `maps compare` reads) and
/// `meta.json`. Not written: `run.json` and `score.json`, which need
/// `maps::baseline::write_arm_run` (private; a 7b follow-up).
fn write_run(
    dir: &Path,
    set: Set,
    arm: Arm,
    cfg: &SimConfig,
    scores: &[ItemScore],
    searcher: &Searcher,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
    let records: serde_json::Map<String, Value> = scores
        .iter()
        .map(|s| (s.id.clone(), s.metrics_json()))
        .collect();
    std::fs::write(
        dir.join("items.json"),
        pretty(&json!({ "set": set_name(set), "items": records }))?,
    )?;
    let full: Vec<Value> = scores.iter().map(ItemScore::full_json).collect();
    std::fs::write(
        dir.join("sim.json"),
        pretty(&json!({
            "arm": arm.name(),
            "set": set_name(set),
            "config": cfg.to_json(),
            "aggregate": score::aggregate(scores),
            "items": full,
        }))?,
    )?;
    std::fs::write(
        dir.join("meta.json"),
        pretty(&json!({
            "arm": format!("simulated agent, {}", arm.name()),
            "gold_aware": cfg.stop_at_gold,
            "policy": cfg.policy.name(),
            "items": scores.len(),
            "incomplete_scans": searcher.incomplete_scans,
            "tokens": "characters / 4, rounded up, as maps bench",
            "note": "expansions = tool calls; no run.json or score.json (answers are not ranked lists)",
        }))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_loop::Stop;
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
        let trace = run_question(&mut searcher, cfg, "quartz zephyr", None);
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
        let trace = run_question(&mut searcher, &config, "quartz zephyr", Some(&gold));
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
        );
        assert_eq!(none.stop, Stop::NoMatch);
        assert!(none.steps.iter().all(|s| s.kind == "probe"));
        let empty = run_question(&mut searcher, &SimConfig::default(), "to a", None);
        assert_eq!((empty.stop, empty.steps.len()), (Stop::NoTerms, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_run_dir_is_read_by_compare() {
        let dir = fork_tree("rundir");
        let item = json!({
            "id": "where-to-t1", "question": "quartz zephyr",
            "gold": {"form": "registration", "expected_files": ["src/alpha_leaf.rs"]}
        });
        let mut searcher = Searcher::new(&dir);
        let config = cfg(Policy::Greedy);
        let scores = simulate(Set::WhereTo, &[item], &mut searcher, &config);
        let out = dir.join("out");
        write_run(&out, Set::WhereTo, Arm::A0, &config, &scores, &searcher).unwrap();
        for file in ["sim.json", "items.json", "meta.json"] {
            assert!(out.join(file).is_file(), "{file}");
        }
        let data = crate::maps::compare::load_run(&out, None).unwrap();
        assert_eq!(data.set, "where-to");
        let record = &data.items["where-to-t1"];
        assert_eq!(record["calls_to_gold"], Some(8.0));
        assert_eq!(record["hit_at_12"], Some(1.0));
        assert_eq!(record["expansions"], record["tool_calls"]);
        let sim: Value =
            serde_json::from_str(&fs::read_to_string(out.join("sim.json")).unwrap()).unwrap();
        assert_eq!(sim["items"][0]["trace"]["steps"][0]["kind"], "probe");
        let _ = fs::remove_dir_all(&dir);
    }
}
