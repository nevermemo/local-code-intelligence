//! `cargo xtask maps fit-tiebreak`: fit the tie-break
//! (`lci_maps::traverse::tiebreak`) from the raw outputs `maps bench` wrote.
//!
//! A raw output (`<out>/<wave>/<set>/raw/<item>.json`) lists the result cards
//! with the feature values the tie-break reads (`Facts::from_card`); run the
//! bench with `--top-k 40 --score-k 10` to give the fit a pool wider than the
//! scored ten. A result is gold when the set's own scorer credits the answer
//! the bench adapter offers for it ([`super::fit_priors::is_gold`]).
//!
//! Within one item, every pair (a gold result, a result that is not) with the
//! same `ranked_potential` is a training example: only such pairs are ordered
//! by a tie-break. The weights are fitted by pairwise logistic regression:
//! full-batch gradient descent, a fixed number of iterations from zero, on the
//! feature differences scaled to unit root mean square, with an L2 penalty on
//! the scaled weights; the same inputs give the same file. Items on the
//! `--holdout` list (the `holdout` array of any JSON file, such as a prior
//! table) are kept out of the fit and only reported on.
//!
//! One file per set: run it once for each set, with that set's raw outputs.
//! "Discovery order" below is the order of the cards in the raw output, which
//! is discovery order when the bench ran without a tie-break.

use super::{
    bench::{self, ResultNode, Site},
    fit_priors::is_gold,
};
use crate::gold::{self, Set};
use anyhow::{Context, Result, bail};
use clap::Args;
use lci_maps::traverse::tiebreak::{Facts, TieBreak};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct FitTiebreakArgs {
    /// A `raw` directory of a bench run of the set (`<out>/<wave>/<set>/raw`);
    /// give several to fit on several runs.
    #[arg(long, required = true)]
    raw_dir: Vec<PathBuf>,
    /// Directory holding the gold files (private; no default).
    #[arg(long)]
    gold_dir: PathBuf,
    /// The gold set the raw outputs are of.
    #[arg(long, value_enum)]
    set: Set,
    /// A JSON file with a `holdout` array of item ids to keep out of the fit.
    #[arg(long)]
    holdout: PathBuf,
    /// Where the tie-break is written. It is derived from private gold: keep
    /// it out of the repository.
    #[arg(long)]
    out: PathBuf,
    /// L2 penalty on the scaled weights.
    #[arg(long, default_value_t = 0.01)]
    l2: f64,
    /// Gradient descent iterations.
    #[arg(long, default_value_t = 2000)]
    iterations: u32,
}

/// One (gold, not gold) pair of results that tie.
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    /// Features of the gold result minus those of the other.
    diff: BTreeMap<String, f64>,
    /// Whether discovery order already puts the gold result first.
    in_order: bool,
}

/// What the raw outputs held.
#[derive(Debug, Default, PartialEq)]
pub struct Collected {
    pub train: Vec<Pair>,
    pub test: Vec<Pair>,
    pub train_items: usize,
    pub test_items: usize,
    /// Items without a raw output, without the feature fields (a bench from
    /// before the tie-break) or outside the set's scored population.
    pub no_raw: usize,
    pub no_features: usize,
    pub unscored: usize,
}

fn raw_name(id: &str) -> String {
    let slug: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{slug}.json")
}

/// The pairs of one raw output, or `None` when a card lacks the fields.
fn item_pairs(set: Set, item: &Value, raw: &Value) -> Result<Option<Vec<Pair>>> {
    let mut results = Vec::new();
    for card in raw["results"].as_array().into_iter().flatten() {
        let (Some(id), Some(facts)) = (card["node"].as_str(), Facts::from_card(card)) else {
            return Ok(None);
        };
        let node = ResultNode {
            id: id.to_owned(),
            text: String::new(),
            site: Site::from_json(&card["site"]),
        };
        let features: BTreeMap<String, f64> = facts.features().into_iter().collect();
        let rank = card["ranked_potential"].as_f64().unwrap_or(f64::NAN);
        results.push((rank, is_gold(set, item, node)?, features));
    }
    let mut pairs = Vec::new();
    for (g, (rank_g, gold_g, features_g)) in results.iter().enumerate() {
        for (n, (rank_n, gold_n, features_n)) in results.iter().enumerate() {
            if !*gold_g || *gold_n || rank_g != rank_n {
                continue;
            }
            let mut diff = BTreeMap::new();
            for name in features_g.keys().chain(features_n.keys()) {
                let d = features_g.get(name).copied().unwrap_or(0.0)
                    - features_n.get(name).copied().unwrap_or(0.0);
                if d != 0.0 {
                    diff.insert(name.clone(), d);
                }
            }
            pairs.push(Pair {
                diff,
                in_order: g < n,
            });
        }
    }
    Ok(Some(pairs))
}

/// The pairs of every gold item of `set` found in the raw directories, split
/// by the hold-out list.
pub fn collect(
    set: Set,
    raw_dirs: &[PathBuf],
    gold_dir: &Path,
    holdout: &BTreeSet<String>,
) -> Result<Collected> {
    let name = bench::set_name(set);
    let items = gold::load_gold(&gold_dir.join(format!("{name}.json")))?;
    let mut out = Collected::default();
    for item in &items {
        let id = item["id"].as_str().unwrap_or_default();
        // An item the scorer leaves out of its population (impact keeps the
        // primary ones) has no gold to fit against.
        let empty = gold::Run::new();
        let population = gold::score_set(set, std::slice::from_ref(item), &empty, false, false)?;
        if population["scored_items"] == 0 {
            out.unscored += 1;
            continue;
        }
        let held = holdout.contains(id);
        for dir in raw_dirs {
            let Ok(text) = std::fs::read_to_string(dir.join(raw_name(id))) else {
                out.no_raw += 1;
                continue;
            };
            let raw: Value = serde_json::from_str(&text)
                .with_context(|| format!("raw output of {id} is not JSON"))?;
            let Some(pairs) = item_pairs(set, item, &raw)? else {
                out.no_features += 1;
                continue;
            };
            let (items, target) = if held {
                (&mut out.test_items, &mut out.test)
            } else {
                (&mut out.train_items, &mut out.train)
            };
            *items += 1;
            target.extend(pairs);
        }
    }
    Ok(out)
}

fn sigmoid(x: f64) -> f64 {
    if x >= 0.0 {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}

/// Pairwise logistic regression on the pairs' feature differences: weights
/// that make `w . diff` positive. No pairs: an error.
pub fn fit_weights(pairs: &[Pair], l2: f64, iterations: u32) -> Result<BTreeMap<String, f64>> {
    if pairs.is_empty() {
        bail!("no pair of tied results (one gold, one not) to fit on");
    }
    let names: Vec<&String> = pairs
        .iter()
        .flat_map(|pair| pair.diff.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let n = pairs.len() as f64;
    let mut rows = vec![vec![0.0; names.len()]; pairs.len()];
    for (row, pair) in rows.iter_mut().zip(pairs) {
        for (j, name) in names.iter().enumerate() {
            row[j] = pair.diff.get(*name).copied().unwrap_or(0.0);
        }
    }
    // Unit root mean square per feature; every column has one.
    let scale: Vec<f64> = (0..names.len())
        .map(|j| (rows.iter().map(|row| row[j] * row[j]).sum::<f64>() / n).sqrt())
        .collect();
    for row in &mut rows {
        for (x, s) in row.iter_mut().zip(&scale) {
            *x /= s;
        }
    }
    // The gradient of the mean loss is Lipschitz with 0.25 * (mean squared
    // norm of a row, which is the number of features) + l2.
    let step = 1.0 / (0.25 * names.len() as f64 + l2);
    let mut w = vec![0.0; names.len()];
    for _ in 0..iterations {
        let mut grad: Vec<f64> = w.iter().map(|w| l2 * w).collect();
        for row in &rows {
            let margin: f64 = row.iter().zip(&w).map(|(x, w)| x * w).sum();
            let pull = sigmoid(-margin) / n;
            for (g, x) in grad.iter_mut().zip(row) {
                *g -= pull * x;
            }
        }
        for (w, g) in w.iter_mut().zip(&grad) {
            *w -= step * g;
        }
    }
    Ok(names
        .into_iter()
        .zip(w.iter().zip(&scale))
        .map(|(name, (w, s))| (name.clone(), w / s))
        .filter(|(_, weight)| weight.abs() > 1e-12)
        .collect())
}

/// The share of pairs the weights put the gold result first in, with a score
/// tie falling back on discovery order.
pub fn accuracy(pairs: &[Pair], weights: &BTreeMap<String, f64>) -> Option<f64> {
    let right = pairs.iter().filter(|pair| {
        let score: f64 = pair
            .diff
            .iter()
            .map(|(name, d)| weights.get(name).copied().unwrap_or(0.0) * d)
            .sum();
        score > 0.0 || (score == 0.0 && pair.in_order)
    });
    (!pairs.is_empty()).then(|| right.count() as f64 / pairs.len() as f64)
}

fn holdout_ids(path: &Path) -> Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the hold-out file {}", path.display()))?;
    let doc: Value = serde_json::from_str(&text).context("the hold-out file is not JSON")?;
    let ids = doc["holdout"]
        .as_array()
        .context("the hold-out file has no `holdout` array")?;
    Ok(ids
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

pub fn run(args: FitTiebreakArgs) -> Result<()> {
    if !args.l2.is_finite() || args.l2 < 0.0 {
        bail!("--l2 must be at least 0");
    }
    let holdout = holdout_ids(&args.holdout)?;
    let collected = collect(args.set, &args.raw_dir, &args.gold_dir, &holdout)?;
    let weights = fit_weights(&collected.train, args.l2, args.iterations)?;
    let set = bench::set_name(args.set);
    let fitted_on = format!(
        "{set}; {} raw dirs; {} items, {} pairs; l2 {}, {} iterations",
        args.raw_dir.len(),
        collected.train_items,
        collected.train.len(),
        args.l2,
        args.iterations,
    );
    let tie_break = TieBreak::new(weights.clone(), holdout, fitted_on)?;
    tie_break.save(&args.out)?;
    println!("wrote {} ({set})", args.out.display());
    println!(
        "items: {} train, {} hold-out; skipped {} without a raw output, {} without the feature fields, {} outside the scored population",
        collected.train_items,
        collected.test_items,
        collected.no_raw,
        collected.no_features,
        collected.unscored,
    );
    let none = BTreeMap::new();
    for (part, pairs) in [("train", &collected.train), ("hold-out", &collected.test)] {
        if let (Some(before), Some(after)) = (accuracy(pairs, &none), accuracy(pairs, &weights)) {
            println!(
                "{part}: {} tied pairs, gold first in {:.1}% by discovery order, {:.1}% with the tie-break",
                pairs.len(),
                before * 100.0,
                after * 100.0,
            );
        }
    }
    for (name, weight) in &weights {
        println!("  {name}: {weight:.4}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `n` ids; the first `held` are on the hold-out list.
    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("flow-{i:02}")).collect()
    }

    /// A card for the flow gold of the test: `mid` (line 5) is gold, the
    /// other lines are not. `overlap` is the only feature that differs.
    fn card(name: &str, line: u64, overlap: f64, rank: f64) -> Value {
        json!({
            "node": format!("sym:rust:m::{name}/fn"),
            "site": {"path": "src/a.rs", "line": line},
            "ranked_potential": rank, "depth": 1, "n_maps": 1, "maps": ["graph.scip"],
            "fanout": 3, "overlap": overlap, "node_kind": "sym",
            "witness": "compiler", "via": "graph.scip",
        })
    }

    #[test]
    fn a_feature_that_separates_gold_gets_a_positive_weight_from_the_train_items_only() {
        let root = std::env::temp_dir().join(format!("fit-tiebreak-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (raw, gold_dir) = (root.join("raw"), root.join("gold"));
        std::fs::create_dir_all(&raw).unwrap();
        std::fs::create_dir_all(&gold_dir).unwrap();
        let held: BTreeSet<String> = ids(40).into_iter().take(10).collect();
        let node = |name: &str, line: u64| json!({"name": name, "file": "src/a.rs", "line": line, "symbol": name});
        let mut items = Vec::new();
        for id in ids(40) {
            items.push(json!({"id": id, "question": "q", "gold": {
                "hops": 2, "entry": node("e", 1), "target": node("t", 9),
                "intermediates": [node("mid", 5)]}}));
            // The gold result is found last among four that tie; a held-out
            // item would teach the opposite.
            let gold_overlap = if held.contains(&id) { 0.1 } else { 0.8 };
            let other_overlap = if held.contains(&id) { 0.8 } else { 0.1 };
            let results = [
                card("x", 40, other_overlap, 0.5),
                card("y", 50, other_overlap, 0.5),
                card("mid", 5, gold_overlap, 0.5),
                card("z", 60, 0.9, 0.25),
            ];
            let raw_json = json!({ "results": results });
            std::fs::write(raw.join(raw_name(&id)), raw_json.to_string()).unwrap();
        }
        std::fs::write(gold_dir.join("flow.json"), Value::Array(items).to_string()).unwrap();

        let collected = collect(Set::Flow, std::slice::from_ref(&raw), &gold_dir, &held).unwrap();
        assert_eq!((collected.train_items, collected.test_items), (30, 10));
        // Two non-gold results tie with the gold one; the lower rank does not.
        assert_eq!((collected.train.len(), collected.test.len()), (60, 20));
        assert!(collected.train.iter().all(|pair| !pair.in_order));

        let weights = fit_weights(&collected.train, 0.01, 2000).unwrap();
        assert!(weights["overlap"] > 0.0, "{weights:?}");
        assert_eq!(weights.len(), 1, "features that never differ get no weight");
        assert_eq!(accuracy(&collected.train, &BTreeMap::new()), Some(0.0));
        assert_eq!(accuracy(&collected.train, &weights), Some(1.0));
        // The held-out items were not seen: they run the other way.
        assert_eq!(accuracy(&collected.test, &weights), Some(0.0));
        assert_eq!(fit_weights(&collected.train, 0.01, 2000).unwrap(), weights);

        // The weights order the gold card first, as `ranked` would.
        let tie_break = TieBreak::new(weights, BTreeSet::new(), String::new()).unwrap();
        let score = |c: &Value| tie_break.score(&Facts::from_card(c).unwrap());
        assert!(score(&card("mid", 5, 0.8, 0.5)) > score(&card("x", 40, 0.1, 0.5)));

        // A bench from before the tie-break has no feature fields: skipped.
        let old = json!({"results": [{"node": "sym:rust:m::mid/fn", "ranked_potential": 0.5}]});
        std::fs::write(raw.join(raw_name("flow-00")), old.to_string()).unwrap();
        let collected = collect(Set::Flow, &[raw], &gold_dir, &held).unwrap();
        assert_eq!(collected.no_features, 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_tied_pair_is_an_error() {
        assert!(fit_weights(&[], 0.01, 10).is_err());
    }
}
