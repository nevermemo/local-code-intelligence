//! `cargo xtask maps baseline`: simple, non-walk arms written as run dirs that
//! the bench scorer and `maps compare` read (`run.json`, `items.json`,
//! `score.json`, `meta.json` under `<out>/<wave>/<set>`; no trend row).
//!
//! * `impact`: M1.4 arm N. The anchor comes from the backticked names of the
//!   question (no gold), the answer is the exact one-hop reverse closure of
//!   the anchor symbol, ordered by how many references of the anchor each
//!   declaration holds (most first), then by path and line, and cut to the
//!   bench's `--top-k`. Everything is `lci_maps::impact`'s public API.
//! * `history`: `git log -G/-S/--grep` over the frozen eval tree, from the
//!   identifiers of the question, in the fixed order documented on
//!   [`HistoryArgs`].
//!
//! Both write the answers as the bench's adapters do (impact `{path, line}`,
//! history `{commit}`) and score them with [`super::bench::item_record`], so a
//! baseline run pairs with any bench run in `maps compare`. Neither arm
//! expands anything: `expansions` is 0.

use super::bench::{self, item_record, select_items, set_name, slug};
use crate::gold::{self, Set};
use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use lci_maps::exact_history::{RealGit, Tally, classify, history_commits};
use lci_maps::impact::{ImpactIndex, dir_source};
use lci_maps::scip_graph::SourceText;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Subcommand)]
pub enum BaselineCommand {
    /// M1.4 arm N on the impact set: anchor from the question's backticked
    /// names, then the exact one-hop reverse closure of the anchor, ordered by
    /// the anchor's reference count per declaration, cut to --top-k. Not given
    /// the gold. Writes a run dir for `maps compare`.
    Impact(ImpactArgs),
    /// `git log -G/-S/--grep` on the frozen eval tree, from the identifiers of
    /// each history question (H1 first creation, H2 last modification, H3
    /// free text), top --top-k commits in a fixed order. Writes a run dir for
    /// `maps compare`.
    History(HistoryArgs),
}

pub fn run(command: BaselineCommand) -> Result<()> {
    match command {
        BaselineCommand::Impact(args) => run_impact(args),
        BaselineCommand::History(args) => run_history(args),
    }
}

// ------------------------------------------------------------ the run dir

/// What a written arm looks like in one line.
pub(super) struct ArmSummary {
    pub n: usize,
    pub precision: f64,
    pub f05: f64,
    pub headline: String,
}

/// Writes `run.json`, `items.json`, `score.json` and `meta.json` under `dir`
/// for the scored `(item, answers)` pairs, with `expansions` and `model_calls`
/// per pair (0 for the arms that make no model calls; the oracle filter carries
/// its source run's).
pub(super) fn write_arm_run(
    dir: &Path,
    set: Set,
    scored: &[(&Value, Vec<Value>)],
    expansions: &[u64],
    model_calls: &[u64],
    meta: &Value,
) -> Result<ArmSummary> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut run = Map::new();
    let mut records = Map::new();
    let (mut precision, mut f05) = (0.0, 0.0);
    for (((item, answers), work), calls) in scored.iter().zip(expansions).zip(model_calls) {
        let id = item["id"].as_str().context("gold item without an id")?;
        let (p, f, mut record) = item_record(set, item, answers, Some(*work))?;
        record.insert("model_calls".into(), json!(*calls));
        precision += p;
        f05 += f;
        run.insert(id.to_owned(), Value::Array(answers.clone()));
        records.insert(id.to_owned(), Value::Object(record));
    }
    let run = Value::Object(run);
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
    std::fs::write(dir.join("run.json"), pretty(&run)?)?;
    std::fs::write(
        dir.join("items.json"),
        pretty(&json!({ "set": set_name(set), "items": records }))?,
    )?;
    std::fs::write(dir.join("meta.json"), pretty(meta)?)?;
    let n = scored.len();
    let mut headline = String::from("nothing scored");
    if n > 0 {
        let items: Vec<Value> = scored.iter().map(|(item, _)| (*item).clone()).collect();
        let parsed = gold::parse_run(&run.to_string())?;
        let report = gold::score_set(set, &items, &parsed, false, false)?;
        std::fs::write(dir.join("score.json"), pretty(&report)?)?;
        headline = bench::headline(set)
            .iter()
            .filter_map(|name| {
                let v = report["overall"]["metrics"][*name].as_f64()?;
                Some(format!("{name}={v:.3}"))
            })
            .collect::<Vec<_>>()
            .join("; ");
    }
    let denominator = n.max(1) as f64;
    Ok(ArmSummary {
        n,
        precision: precision / denominator,
        f05: f05 / denominator,
        headline,
    })
}

fn say(label: &str, dir: &Path, summary: &ArmSummary) {
    eprintln!(
        "{label}: {} items scored, precision {:.3}, f0.5 {:.3}; {}\nrun dir: {}",
        summary.n,
        summary.precision,
        summary.f05,
        summary.headline,
        dir.display()
    );
}

// ----------------------------------------------------------------- impact

#[derive(Args)]
pub struct ImpactArgs {
    /// Directory holding impact.json (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The SCIP index of the frozen tree.
    #[arg(long)]
    scip: PathBuf,
    /// The frozen tree the index describes (read for the index's
    /// macro-expansion repair).
    #[arg(long)]
    tree: PathBuf,
    /// Answers per item, the bench's `--top-k`; 0 keeps the whole closure
    /// (arm N-full).
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/impact`.
    #[arg(long, default_value = "baseline-n")]
    wave: String,
    /// Run only the first N primary items.
    #[arg(long)]
    limit: Option<usize>,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

/// What arm N made of one question.
#[derive(Debug, Clone, PartialEq)]
pub struct ImpactPick {
    pub answers: Vec<Value>,
    /// Backticked names the question gave.
    pub names: usize,
    pub anchored: bool,
    /// Declarations in the closure, before the cut.
    pub closure: usize,
}

/// Arm N for one question: anchor (`anchor_n`), closure, order by the anchor's
/// reference count per declaration (descending), then path, line, symbol; cut
/// to `top_k` (0 = no cut). Answers are `{path, line}` of each declaration's
/// name, the form the bench's impact adapter offers.
pub fn pick_impact(ix: &ImpactIndex, question: &str, top_k: usize) -> ImpactPick {
    let ranked = ix.ranked_closure(question);
    let closure = ranked.decls.len();
    let keep = if top_k == 0 {
        closure
    } else {
        top_k.min(closure)
    };
    let answers = ranked
        .decls
        .into_iter()
        .take(keep)
        .map(|r| json!({ "path": r.decl.path, "line": r.decl.line }))
        .collect();
    ImpactPick {
        answers,
        names: ranked.names,
        anchored: ranked.anchored,
        closure,
    }
}

fn run_impact(args: ImpactArgs) -> Result<()> {
    let items = select_items(
        Set::Impact,
        gold::load_gold(&args.gold_dir.join("impact.json"))?,
        args.limit,
        None,
    );
    let source = dir_source(&args.tree);
    let ix = ImpactIndex::load(&args.scip, Some(&source as &dyn SourceText))
        .map_err(|e| anyhow!("load the SCIP index: {e}"))?;
    let (mut no_names, mut no_anchor, mut empty) = (0usize, 0usize, 0usize);
    let mut scored: Vec<(&Value, Vec<Value>)> = Vec::new();
    for item in &items {
        let question = item["question"].as_str().unwrap_or_default();
        let pick = pick_impact(&ix, question, args.top_k);
        no_names += usize::from(pick.names == 0);
        no_anchor += usize::from(!pick.anchored);
        empty += usize::from(pick.answers.is_empty());
        scored.push((item, pick.answers));
    }
    let dir = args.out_dir.join(slug(&args.wave)).join("impact");
    let cap = if args.top_k == 0 {
        "uncapped (N-full)".to_owned()
    } else {
        format!("top {}", args.top_k)
    };
    let meta = json!({
        "arm": "M1.4 arm N (baseline impact)",
        "gold_aware": false,
        "answers": cap,
        "order": "anchor reference count per declaration desc, then path, line, symbol",
        "items": items.len(),
        "items_without_backticked_names": no_names,
        "items_without_anchor": no_anchor,
        "items_with_empty_answer": empty,
        "note": "the anchor reads the backticked names of the question; the engine's hybrid seeds do not parse backticks; the closure comes from the same SCIP index as the gold's R1",
    });
    let zeros = vec![0u64; scored.len()];
    let summary = write_arm_run(&dir, Set::Impact, &scored, &zeros, &zeros, &meta)?;
    say("baseline impact (arm N)", &dir, &summary);
    eprintln!("without names {no_names}, without anchor {no_anchor}, empty {empty}");
    Ok(())
}

// ---------------------------------------------------------------- history

#[derive(Args)]
pub struct HistoryArgs {
    /// Directory holding history.json (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The frozen eval tree: a local git checkout. Only read with `git log`.
    #[arg(long)]
    tree: PathBuf,
    /// Commits per item, the bench's `--top-k`.
    #[arg(long, default_value_t = 10)]
    top_k: usize,
    /// Label of this measurement; the run dir is `<out-dir>/<wave>/history`.
    #[arg(long, default_value = "baseline-git")]
    wave: String,
    /// Run only the first N items.
    #[arg(long)]
    limit: Option<usize>,
    /// Longest one `git log` call may run, in seconds; a slower call is killed
    /// and counts as a failure (its commits are left out).
    #[arg(long, default_value_t = 60)]
    git_timeout_s: u64,
    /// The git binary.
    #[arg(long, default_value = "git")]
    git: PathBuf,
    /// Where the run dir is written.
    #[arg(long)]
    out_dir: PathBuf,
}

fn run_history(args: HistoryArgs) -> Result<()> {
    let items = select_items(
        Set::History,
        gold::load_gold(&args.gold_dir.join("history.json"))?,
        args.limit,
        None,
    );
    let git = RealGit {
        git: args.git.clone(),
        tree: args.tree.clone(),
        timeout: Duration::from_secs(args.git_timeout_s),
    };
    let recency = RefCell::new(None);
    let mut tally = Tally::default();
    let mut families: HashMap<&'static str, usize> = HashMap::new();
    let mut scored: Vec<(&Value, Vec<Value>)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let question = item["question"].as_str().unwrap_or_default();
        let query = classify(question);
        *families.entry(query.family()).or_default() += 1;
        let commits = history_commits(&git, &recency, &query, args.top_k, &mut tally);
        scored.push((
            item,
            commits.iter().map(|c| json!({ "commit": c })).collect(),
        ));
        if (index + 1) % 10 == 0 {
            eprintln!("history: {}/{} done", index + 1, items.len());
        }
    }
    let dir = args.out_dir.join(slug(&args.wave)).join("history");
    let meta = json!({
        "arm": "git log baseline (baseline history)",
        "gold_aware": false,
        "answers": format!("top {}", args.top_k),
        "order": "H1 oldest first from -G declaration regex (else -S); H2 newest first from -S in the question's path at its sha (else the file log); H3 by tokens matched then newest first; see history_commits",
        "items": items.len(),
        "families_by_wording": families,
        "git_failures": tally.failures,
        "first_git_failure": tally.first_failure,
        "note": "the engine's git.history reads the tree HEAD only; this arm honours the sha in an H2 question, a small edge on those items",
    });
    let zeros = vec![0u64; scored.len()];
    let summary = write_arm_run(&dir, Set::History, &scored, &zeros, &zeros, &meta)?;
    say("baseline history (git log)", &dir, &summary);
    if tally.failures > 0 {
        eprintln!(
            "warning: {} git calls failed (first: {})",
            tally.failures,
            tally.first_failure.as_deref().unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_maps::scip_graph::{RawDocument, RawOccurrence};
    use scip::types::symbol_information::Kind;

    fn sym(path: &str) -> String {
        format!("rust-analyzer cargo demo 0.1.0 {path}")
    }

    fn occ(
        range: (u32, u32, u32, u32),
        symbol: &str,
        roles: i32,
        enclosing: Option<(u32, u32, u32, u32)>,
    ) -> RawOccurrence {
        RawOccurrence {
            range,
            symbol: symbol.to_owned(),
            roles,
            enclosing,
        }
    }

    /// a.rs: `struct Foo` (line 1), `fn alpha` uses Foo once (line 2),
    /// `fn beta` twice (line 3), `fn gamma` once (line 4), `fn Foo_user`
    /// does not exist; all in one file.
    fn demo() -> ImpactIndex {
        let (foo, alpha, beta, gamma) = (
            sym("a/Foo#"),
            sym("a/alpha()."),
            sym("a/beta()."),
            sym("a/gamma()."),
        );
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 7, 0, 10), &foo, 1, Some((0, 0, 0, 14))),
                occ((1, 3, 1, 8), &alpha, 1, Some((1, 0, 1, 30))),
                occ((1, 12, 1, 15), &foo, 0, None),
                occ((2, 3, 2, 7), &beta, 1, Some((2, 0, 2, 40))),
                occ((2, 12, 2, 15), &foo, 0, None),
                occ((2, 20, 2, 23), &foo, 0, None),
                occ((3, 3, 3, 8), &gamma, 1, Some((3, 0, 3, 30))),
                occ((3, 12, 3, 15), &foo, 0, None),
            ],
        }];
        let kinds: HashMap<String, Kind> = [
            (foo, Kind::Struct),
            (alpha, Kind::Function),
            (beta, Kind::Function),
            (gamma, Kind::Function),
        ]
        .into_iter()
        .collect();
        ImpactIndex::build(docs, &kinds, None)
    }

    #[test]
    fn arm_n_orders_the_closure_by_references_held_and_cuts_to_k() {
        let ix = demo();
        let full = pick_impact(&ix, "What breaks if `Foo` changes?", 0);
        assert!(full.anchored);
        assert_eq!((full.names, full.closure), (1, 3));
        // beta holds two references, alpha and gamma one each (path, line order).
        let lines: Vec<u64> = full
            .answers
            .iter()
            .map(|a| a["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, [3, 2, 4]);
        assert!(full.answers.iter().all(|a| a["path"] == "a.rs"));
        let capped = pick_impact(&ix, "What breaks if `Foo` changes?", 2);
        assert_eq!(capped.closure, 3);
        let lines: Vec<u64> = capped
            .answers
            .iter()
            .map(|a| a["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, [3, 2]);
    }

    #[test]
    fn arm_n_without_a_name_or_an_anchor_answers_nothing() {
        let ix = demo();
        let none = pick_impact(&ix, "What breaks if the parser changes?", 10);
        assert_eq!(
            (none.names, none.anchored, none.answers.len()),
            (0, false, 0)
        );
        let unknown = pick_impact(&ix, "What breaks if `Nowhere` changes?", 10);
        assert_eq!((unknown.names, unknown.anchored), (1, false));
        assert!(unknown.answers.is_empty());
    }

    #[test]
    fn arm_n_run_dir_is_read_by_compare_and_the_scorer() {
        let ix = demo();
        let item = json!({
            "id": "impact-t1", "primary": true,
            "question": "What breaks if `Foo` changes?",
            "stratum": "sym-fn/deg2-4/tests-near",
            "ambiguity": {"band": "some"},
            "seed": {"path": "a.rs", "line": 1, "span": [1, 1], "name": "Foo", "symbol": "sym:foo"},
            "accept": {"neutral_decls": [], "neutral_files": [], "decoys": []},
            "gold": {"r1": {
                "decls": [
                    {"path": "a.rs", "line": 2, "span": [2, 2], "name": "alpha", "bare": "alpha", "symbol": "sym:alpha"},
                    {"path": "a.rs", "line": 3, "span": [3, 3], "name": "beta", "bare": "beta", "symbol": "sym:beta"}
                ],
                "files": ["a.rs"]
            }, "r2": {"decls": []}, "tests": []}
        });
        let pick = pick_impact(&ix, item["question"].as_str().unwrap(), 10);
        let dir = temp_dir("arm-n");
        let scored = vec![(&item, pick.answers)];
        let summary = write_arm_run(
            &dir,
            Set::Impact,
            &scored,
            &[0],
            &[0],
            &json!({"arm": "test"}),
        )
        .unwrap();
        assert_eq!(summary.n, 1);
        for file in ["run.json", "items.json", "score.json", "meta.json"] {
            assert!(dir.join(file).is_file(), "{file}");
        }
        let data = super::super::compare::load_run(&dir, None).unwrap();
        assert_eq!(data.set, "impact");
        let record = &data.items["impact-t1"];
        assert_eq!(record["decl_recall"], Some(1.0));
        assert_eq!(record["expansions"], Some(0.0));
        assert_eq!(record["model_calls"], Some(0.0));
        assert!(record["f0.5"].is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-br3-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
