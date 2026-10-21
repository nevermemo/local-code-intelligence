//! `supplement.md`: extra reporting beside a bench run's frozen outputs.
//!
//! Everything here is computed from the scorer report and the per-item metrics
//! the run already wrote (`score.json`, `items.json`). Nothing is re-run and no
//! other output changes. The file adds, per set, the secondary metrics and
//! strata that the one-line row leaves out, a second table of the costs per
//! stratum (the `costs` block `maps bench` adds to `score.json`: mean
//! expansions, external calls, `lsp.live` ms and answer tokens, and the budget
//! hits), and a footer that names which metrics sit near a ceiling, so a delta
//! on a saturated metric is not read as evidence.

use crate::gold::Set;
use serde_json::Value;
use std::fmt::Write;

/// What the table of a set shows: the stratum dimension, its metrics, and the
/// stratum values whose rows are marked "not used for decisions".
struct Plan {
    dimension: &'static str,
    metrics: &'static [&'static str],
    not_for_decisions: &'static [&'static str],
    footer: &'static [&'static str],
}

fn plan(set: Set) -> Option<Plan> {
    match set {
        Set::Impact => Some(Plan {
            dimension: "band",
            metrics: &["decl_recall", "r2_decl_recall", "tests_recall"],
            not_for_decisions: &[],
            footer: &[
                "Ceiling: first-ring declaration recall (R1) on unique names is saturated by a word-boundary grep plus the enclosing declaration (cited baseline: .995 unique, .79 some, .42 common).",
                "Read the common band, r2_decl_recall and tests_recall; keep decl_recall as a regression column, and do not read a walk or filter delta on a saturated band as evidence.",
                "Not computed here: the grep baseline column (it needs a grep run over the frozen tree; the cited figures above are not re-measured).",
            ],
        }),
        Set::Flow => Some(Plan {
            dimension: "stratum",
            metrics: &["path_recall", "tau", "spurious"],
            not_for_decisions: &[],
            footer: &[
                "Ceiling: a breadth-first expansion reaches 97 to 100% of the gold paths inside a 200-pop budget (M0.4 results on a 1,070-node graph, cited and not re-measured; about .99), so path_recall near that is the reference line, not a gain.",
                "Read path_recall together with the answer precision above and tau; recall alone rises with answer count.",
            ],
        }),
        Set::WhereTo => Some(Plan {
            dimension: "form",
            metrics: &["reg_hit@3", "reg_mrr", "set_f1"],
            not_for_decisions: &[],
            footer: &[
                "The tantivy items (30) are a loss guard only: a delta there that is not a loss is not a gain claim.",
            ],
        }),
        Set::History => Some(Plan {
            dimension: "family",
            metrics: &["mrr", "hit@3", "hit@10"],
            not_for_decisions: &["H1", "H3a", "H3b"],
            footer: &[
                "H3a and H3b floor near zero by construction, and H1 is circular with the exact pickaxe operator; their rows are printed but not used for decisions.",
            ],
        }),
        Set::Instructions | Set::Traits => None,
    }
}

fn cell(value: &Value, name: &str) -> String {
    let v = match value["metrics"][name].as_f64() {
        Some(v) => format!("{v:.3}"),
        None => return "-".into(),
    };
    match value["n_defined"][name].as_u64() {
        Some(k) => format!("{v} ({k})"),
        None => v,
    }
}

/// The mean per-item `precision` of the scored items, and how many there are.
fn mean_precision(items: &Value) -> Option<(f64, usize)> {
    let map = items["items"].as_object()?;
    let values: Vec<f64> = map
        .values()
        .filter_map(|m| m["precision"].as_f64())
        .collect();
    if values.is_empty() {
        return None;
    }
    Some((
        values.iter().sum::<f64>() / values.len() as f64,
        values.len(),
    ))
}

/// The cost table of a set: the means per value of the plan's dimension next
/// to the budget hits, or `None` for a report with no `costs` block (a run from
/// before it).
fn costs_table(plan: &Plan, report: &Value) -> Option<String> {
    let costs = report.get("costs").filter(|c| c.is_object())?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} | n | expansions | external_calls | lsp_ms | answer_tokens | budget hits",
        plan.dimension
    );
    let _ = writeln!(out, "{}", ["---"; 7].join(" | "));
    let mut row = |label: &str, cell: &Value| {
        let mean = |name: &str| {
            cell["mean"][name]
                .as_f64()
                .map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"))
        };
        let hits = cell["budget_hits"]
            .as_object()
            .filter(|h| !h.is_empty())
            .map_or_else(
                || "none".to_owned(),
                |h| {
                    h.iter()
                        .map(|(kind, n)| format!("{kind}={n}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                },
            );
        let _ = writeln!(
            out,
            "{label} | {} | {} | {} | {} | {} | {hits}",
            cell["n"],
            mean("expansions"),
            mean("external_calls"),
            mean("lsp_ms"),
            mean("answer_tokens"),
        );
    };
    row("all", &costs["overall"]);
    for (value, cell) in costs["strata"][plan.dimension]
        .as_object()
        .into_iter()
        .flatten()
    {
        row(value, cell);
    }
    Some(out)
}

/// The supplement of one set, or `None` when the set has none or nothing was
/// scored.
pub fn render(set: Set, report: &Value, items: &Value) -> Option<String> {
    let plan = plan(set)?;
    if report.is_null() {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Supplement ({})\n\nComputed from `score.json` and `items.json` of this run; no output of the run was changed.\n",
        super::bench::set_name(set)
    );
    let _ = write!(out, "{} | n", plan.dimension);
    for name in plan.metrics {
        let _ = write!(out, " | {name}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", vec!["---"; plan.metrics.len() + 2].join(" | "));
    let overall = &report["overall"];
    let mut row = |label: &str, value: &Value| {
        let _ = write!(out, "{label} | {}", value["n"]);
        for name in plan.metrics {
            let _ = write!(out, " | {}", cell(value, name));
        }
        let _ = writeln!(out);
    };
    row("all", overall);
    if let Some(strata) = report["strata"][plan.dimension].as_object() {
        for (value, cell_value) in strata {
            let label = if plan.not_for_decisions.contains(&value.as_str()) {
                format!("{value} (not used for decisions)")
            } else {
                value.clone()
            };
            row(&label, cell_value);
        }
    }
    if set == Set::Flow
        && let Some((p, n)) = mean_precision(items)
    {
        let _ = writeln!(
            out,
            "\nAnswer precision (share of returned answers that are not spurious, mean per item): {p:.3} over {n}"
        );
    }
    if let Some(table) = costs_table(&plan, report) {
        let _ = writeln!(
            out,
            "\nCosts per stratum (mean per item; a budget hit is a walk cut by the pop cap, the token budget or a deadline, or a spent external-call allowance, and `maps compare` leaves such items out by default):\n"
        );
        let _ = write!(out, "{table}");
    }
    let _ = writeln!(out);
    for line in plan.footer {
        let _ = writeln!(out, "- {line}");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn impact_report() -> Value {
        json!({
            "overall": {"n": 3, "metrics": {"decl_recall": 0.6667, "r2_decl_recall": 0.5, "tests_recall": 0.25},
                        "n_defined": {"r2_decl_recall": 2, "tests_recall": 1}},
            "strata": {"band": {
                "unique": {"n": 1, "metrics": {"decl_recall": 1.0}},
                "common": {"n": 2, "metrics": {"decl_recall": 0.5, "r2_decl_recall": 0.5}, "n_defined": {"r2_decl_recall": 1}}
            }}
        })
    }

    #[test]
    fn impact_shows_the_common_band_and_the_secondary_recalls_with_a_ceiling_footer() {
        let text = render(Set::Impact, &impact_report(), &Value::Null).unwrap();
        assert!(text.contains("band | n | decl_recall | r2_decl_recall | tests_recall"));
        assert!(
            text.contains("all | 3 | 0.667 | 0.500 (2) | 0.250 (1)"),
            "{text}"
        );
        assert!(
            text.contains("common | 2 | 0.500 | 0.500 (1) | -"),
            "{text}"
        );
        assert!(text.contains("unique | 1 | 1.000 | - | -"), "{text}");
        assert!(text.contains("Ceiling: first-ring declaration recall"));
    }

    #[test]
    fn flow_adds_the_mean_answer_precision_from_the_item_records() {
        let report = json!({"overall": {"n": 2, "metrics": {"path_recall": 0.5, "tau": 1.0, "spurious": 1.0}}, "strata": {}});
        let items =
            json!({"set": "flow", "items": {"a": {"precision": 1.0}, "b": {"precision": 0.5}}});
        let text = render(Set::Flow, &report, &items).unwrap();
        assert!(text.contains("mean per item): 0.750 over 2"), "{text}");
        assert!(text.contains("about .99), so path_recall near that"));
    }

    #[test]
    fn history_marks_the_families_not_used_for_decisions() {
        let report = json!({
            "overall": {"n": 2, "metrics": {"mrr": 0.5}},
            "strata": {"family": {
                "H2": {"n": 1, "metrics": {"mrr": 1.0}},
                "H3a": {"n": 1, "metrics": {"mrr": 0.0}}
            }}
        });
        let text = render(Set::History, &report, &Value::Null).unwrap();
        assert!(
            text.contains("H3a (not used for decisions) | 1 | 0.000"),
            "{text}"
        );
        assert!(text.contains("\nH2 | 1 | 1.000"), "{text}");
    }

    #[test]
    fn a_costs_block_adds_one_table_per_plan_dimension() {
        let mut report = impact_report();
        report["costs"] = json!({
            "overall": {"n": 3, "mean": {"expansions": 10.0, "external_calls": 1.0, "lsp_ms": 12.5, "answer_tokens": 40.0},
                        "budget_hits": {"pops": 1}},
            "strata": {
                "band": {"common": {"n": 2, "mean": {"expansions": 12.0, "external_calls": 1.5, "lsp_ms": 18.75, "answer_tokens": 50.0},
                                    "budget_hits": {"pops": 1, "tokens": 1}},
                         "unique": {"n": 1, "mean": {"expansions": 6.0, "external_calls": 0.0, "lsp_ms": 0.0, "answer_tokens": 20.0},
                                    "budget_hits": {}}},
                "kind": {"fn": {"n": 3, "mean": {}, "budget_hits": {}}}
            }
        });
        let text = render(Set::Impact, &report, &Value::Null).unwrap();
        assert!(
            text.contains(
                "band | n | expansions | external_calls | lsp_ms | answer_tokens | budget hits"
            ),
            "{text}"
        );
        assert!(
            text.contains("all | 3 | 10.00 | 1.00 | 12.50 | 40.00 | pops=1"),
            "{text}"
        );
        assert!(
            text.contains("common | 2 | 12.00 | 1.50 | 18.75 | 50.00 | pops=1 tokens=1"),
            "{text}"
        );
        assert!(
            text.contains("unique | 1 | 6.00 | 0.00 | 0.00 | 20.00 | none"),
            "{text}"
        );
        // Only the plan's dimension is tabled, and a report without costs has no table.
        assert!(!text.contains("fn | 3"), "{text}");
        let plain = render(Set::Impact, &impact_report(), &Value::Null).unwrap();
        assert!(!plain.contains("Costs per stratum"), "{plain}");
    }

    #[test]
    fn nothing_is_written_for_an_unscored_run_or_a_set_without_a_supplement() {
        assert!(render(Set::Impact, &Value::Null, &Value::Null).is_none());
        assert!(render(Set::Instructions, &impact_report(), &Value::Null).is_none());
    }
}
