//! Tests of the traversal entry point and of the pieces it is built from.

use super::rescore::{RERANK_TEXT_CHARS, classify, relevance};
use super::seeds::map_hits;
use super::*;
use crate::models::Models;
use lci_core::{
    config::{TraversePolicy, TraverseRerankScores},
    models::HttpStatusError,
    node::NodeKind,
};
use lci_maps::{
    Budget, EdgeKind, impact::ImpactIndex, scip_graph::ScipMap, traverse::rescore::RerankFailure,
};
use serde_json::json;
use std::{collections::HashMap, sync::Mutex};

/// The walk alone: the tests of the walk do not want the first stage that
/// the defaults now put ahead of it.
fn limits() -> TraverseConfig {
    TraverseConfig {
        enabled: true,
        first_stage: TraverseFirstStage::Off,
        answer_seeds: false,
        exact_seeds: false,
        ..TraverseConfig::default()
    }
}

#[test]
fn the_configured_boost_starts_at_the_engines_default() {
    assert_eq!(
        TraverseConfig::default().corroboration_boost,
        lci_maps::traverse::DEFAULT_CORROBORATION_BOOST
    );
}

#[test]
fn a_budget_above_a_cap_is_lowered_and_named() {
    let asked = BudgetArgs {
        pops: Some(5000),
        max_depth: Some(3),
        est_tokens: None,
        deadline_ms: Some(10),
    };
    let (budget, notes) = clamp_budget(&limits(), &asked);
    assert_eq!(budget.pops, 200);
    assert_eq!(budget.max_depth, 3);
    assert_eq!(budget.est_tokens, 20_000);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].starts_with("budget.pops 5000"), "{notes:?}");
}

#[test]
fn a_ladder_must_end_with_the_agent_and_cannot_use_system_one() {
    let ladder = |rungs: &[&str]| SteerArgs {
        ladder: Some(rungs.iter().map(|s| (*s).to_owned()).collect()),
        ..SteerArgs::default()
    };
    let build = |steer: &SteerArgs| build_ladder(&limits(), steer, 100, &mut Vec::new());
    assert!(build(&ladder(&["rules", "agent"])).is_ok());
    assert!(build(&ladder(&["agent"])).is_ok());
    for bad in [
        &["rules"][..],
        &["rules", "system_one", "agent"],
        &["agent", "rules"],
        &["agent", "agent"],
        &["rules", "oracle", "agent"],
    ] {
        assert!(build(&ladder(bad)).is_err(), "{bad:?}");
    }
    let script = SteerArgs {
        script: Some("1".into()),
        ..SteerArgs::default()
    };
    assert!(build(&script).is_err());
}

#[test]
fn rules_make_a_handler_and_their_absence_makes_none() {
    let mut notes = Vec::new();
    assert!(
        build_ladder(&limits(), &SteerArgs::default(), 100, &mut notes)
            .unwrap()
            .is_none()
    );
    let with_rules = SteerArgs {
        rules: Some(json!([{"when": {"path_glob": "tests/**"}, "then": "prune"}])),
        max_stops: Some(99),
        ..SteerArgs::default()
    };
    let (ladder, stops, plan) = build_ladder(&limits(), &with_rules, 100, &mut notes)
        .unwrap()
        .unwrap();
    assert_eq!(ladder.len(), 1);
    assert_eq!(plan.ladder, ["rules", "agent"], "the default ladder");
    assert_eq!(plan.rules, 1);
    assert_eq!(plan.tau, Some(0.8));
    assert_eq!(stops.max_stops, 4, "capped by the configuration");
    assert_eq!(stops.every_pops, 20, "100 pops over 5 spans");
    assert_eq!(notes.len(), 1, "{notes:?}");
    let bad = SteerArgs {
        rules: Some(json!([{"when": {"nope": 1}, "then": "prune"}])),
        ..SteerArgs::default()
    };
    let error = build_ladder(&limits(), &bad, 100, &mut notes)
        .err()
        .unwrap();
    assert!(error.to_string().starts_with("steer.rules: "), "{error}");
}

#[test]
fn the_disagreement_and_likely_complete_triggers_come_from_the_steer_json() {
    // The shape of docs/spike/multimap/v2/policies/ladder-rules-only.json.
    let steer: SteerArgs = serde_json::from_value(json!({
        "rules": [{"when": {"kind": "test"}, "then": "prune"}],
        "max_stops": 3,
        "stop_every": 10,
        "on_disagreement": 5,
        "likely_complete": 90
    }))
    .unwrap();
    let (_, stops, _) = build_ladder(&limits(), &steer, 100, &mut Vec::new())
        .unwrap()
        .unwrap();
    assert_eq!(stops.every_pops, 10);
    assert_eq!(stops.max_stops, 3);
    assert_eq!(stops.on_disagreement, 5);
    assert_eq!(stops.likely_complete, 90);
    assert_eq!(stops.budget_share, 0);
    assert_eq!(stops.on_stagnation, 0);

    // Absent, they stay off.
    let plain: SteerArgs = serde_json::from_value(json!({"stop_every": 10})).unwrap();
    let (_, stops, _) = build_ladder(&limits(), &plain, 100, &mut Vec::new())
        .unwrap()
        .unwrap();
    assert_eq!((stops.on_disagreement, stops.likely_complete), (0, 0));

    // Either alone starts the ladder, as stop_every does; the periodic
    // stops keep their default spacing.
    for body in [
        json!({"on_disagreement": 2}),
        json!({"likely_complete": 80}),
    ] {
        let alone: SteerArgs = serde_json::from_value(body).unwrap();
        let mut notes = Vec::new();
        let (_, stops, _) = build_ladder(&limits(), &alone, 100, &mut notes)
            .unwrap()
            .unwrap();
        assert_eq!(stops.every_pops, 20, "100 pops over 5 spans");
        assert!(notes.is_empty(), "{notes:?}");
    }

    // Out-of-range values are errors, not silent switch-offs.
    for body in [
        json!({"on_disagreement": 0}),
        json!({"likely_complete": 0}),
        json!({"likely_complete": 101}),
    ] {
        let bad: SteerArgs = serde_json::from_value(body.clone()).unwrap();
        let error = build_ladder(&limits(), &bad, 100, &mut Vec::new())
            .err()
            .unwrap_or_else(|| panic!("{body} was accepted"));
        assert!(error.to_string().starts_with("steer."), "{error}");
    }

    // Unknown keys are still refused.
    assert!(serde_json::from_value::<SteerArgs>(json!({"on_stagnation": 3})).is_err());
}

// ---- steer lint and the effective policy (slice S9a) -------------------

/// The warnings of `steer` in a call with the default budget and maps.
fn warnings_of(steer: &SteerArgs) -> Vec<String> {
    let (steer, _) = presets::resolve(steer).unwrap();
    let limits = TraverseConfig {
        max_stops: 8,
        ..limits()
    };
    let (budget, mut warnings) = clamp_budget(&limits, &BudgetArgs::default());
    let handler = build_ladder(&limits, &steer, budget.pops, &mut warnings).unwrap();
    let actions = steer
        .actions
        .as_ref()
        .map(|value| Steer::from_json(&json!({ "actions": value })).unwrap());
    warnings.extend(lint_steer(
        &steer,
        handler.as_ref().map(|(_, stops, _)| stops),
        actions.as_ref(),
        &budget,
        &MAPS,
    ));
    warnings
}

#[test]
fn the_presets_and_the_shipped_policy_file_lint_clean() {
    let file = include_str!("../../../docs/spike/multimap/v2/policies/ladder-rules-only.json");
    let mut steers: Vec<SteerArgs> = vec![serde_json::from_str(file).unwrap()];
    steers.extend(presets::FILES.iter().map(|(name, _)| SteerArgs {
        policy: Some((*name).to_owned()),
        ..SteerArgs::default()
    }));
    for steer in steers {
        assert_eq!(warnings_of(&steer), Vec::<String>::new(), "{steer:?}");
    }
}

#[test]
fn the_steer_checks_that_need_the_steer_fields_fire_on_a_minimal_bad_input() {
    let steer = |body: Value| serde_json::from_value::<SteerArgs>(body).unwrap();
    for (body, needle) in [
        (
            json!({"rules": [], "ladder": ["agent"]}),
            "lists no `rules` rung",
        ),
        (
            json!({"rules": [{"when": {"map": "nope"}, "then": "prune"}]}),
            "rule 0 cannot fire",
        ),
        (json!({"stop_every": 5000}), "stop_every 5000"),
        (json!({"rules": [], "max_stops": 99}), "max_stops 99"),
        (json!({"max_stops": 2}), "do nothing"),
        (
            json!({"rules": [], "actions": [{"action": {"budget": {"pops": 500}}}]}),
            "raise the budget by 500 pops",
        ),
    ] {
        let found = warnings_of(&steer(body.clone()));
        assert!(
            found.iter().any(|line| line.contains(needle)),
            "{body}: {found:?}"
        );
    }
}

#[test]
fn the_effective_policy_shows_the_defaults_a_call_ran_with() {
    let steer = SteerArgs {
        rules: Some(json!([{"when": {"kind": "test"}, "then": "prune"}])),
        stop_every: Some(7),
        ..SteerArgs::default()
    };
    let handler = build_ladder(&limits(), &steer, 100, &mut Vec::new())
        .unwrap()
        .unwrap();
    let maps = [PlanMap {
        id: "graph.scip".into(),
        version: 1,
        weight: 1.0,
        enabled: true,
    }];
    let (body, plan) = presets::effective(
        Some("impact".into()),
        &steer,
        Some((&handler.1, &handler.2)),
        &maps,
    );
    assert_eq!(body["preset"], "impact");
    assert_eq!(body["stops"]["every_pops"], 7);
    assert_eq!(body["stops"]["max_stops"], 4, "the configured cap");
    assert_eq!(body["ladder"], json!(["rules", "agent"]));
    assert_eq!(body["tau"], 0.8);
    assert_eq!(body["maps"][0]["weight"], 1.0);
    assert_eq!(
        (body["hash"].as_str(), plan.preset.as_deref()),
        (Some(plan.hash.as_str()), Some("impact"))
    );
    // The hash is of the content, not of the name.
    let (renamed, _) = presets::effective(None, &steer, Some((&handler.1, &handler.2)), &maps);
    assert_eq!(renamed["hash"], body["hash"]);
    let (none, _) = presets::effective(Some("explore".into()), &SteerArgs::default(), None, &maps);
    assert_eq!(none["stops"], Value::Null);
    assert_ne!(none["hash"], body["hash"]);
}

// ---- start nodes from the hybrid search (slice S6b) --------------------

use lci_maps::scip_graph::{RawDocument, RawOccurrence, ScipGraph};
use std::sync::atomic::{AtomicUsize, Ordering};

fn symbol(name: &str) -> String {
    format!("rust-analyzer cargo demo 0.1.0 m/{name}().")
}

/// A definition of `name` on 0-based `line`, with an enclosing range of
/// `lines.0..=lines.1`.
fn defined(name: &str, line: u32, lines: (u32, u32)) -> RawOccurrence {
    RawOccurrence {
        range: (line, 3, line, 3 + name.len() as u32),
        symbol: symbol(name),
        roles: 1,
        enclosing: Some((lines.0, 0, lines.1, 1)),
    }
}

/// `src/search.rs` (0-based lines; the hits below count from 1):
/// `search` 0..=9; a gap of two lines; `parse_query` 12..=14;
/// `rank_hits` 15..=30 with the nested `score_inner` 20..=24; five
/// one-line functions on lines 40 to 44. `src/other.rs` has `helper`.
fn loaded() -> Loaded {
    let one_liners = [
        "load_user",
        "save_user",
        "drop_user",
        "list_user",
        "find_user",
    ];
    let mut search = vec![
        defined("search", 0, (0, 9)),
        defined("parse_query", 12, (12, 14)),
        defined("rank_hits", 15, (15, 30)),
        defined("score_inner", 20, (20, 24)),
    ];
    for (i, name) in one_liners.iter().enumerate() {
        let line = 40 + i as u32;
        search.push(defined(name, line, (line, line)));
    }
    let docs = vec![
        RawDocument {
            path: "src/search.rs".into(),
            occurrences: search,
        },
        RawDocument {
            path: "src/other.rs".into(),
            occurrences: vec![defined("helper", 3, (3, 5))],
        },
    ];
    Loaded::new(
        (None, 0),
        ScipMap::with_default_ids(ScipGraph::build(&docs, None)),
    )
}

#[test]
fn a_reranker_score_becomes_relevance_by_what_the_setting_says_and_by_itself() {
    use TraverseRerankScores::{Logit, Probability};
    // Probabilities pass through; one outside [0, 1] is clamped and counted.
    assert_eq!(relevance(0.2, Probability), (0.2, false));
    assert_eq!(relevance(1.0, Probability), (1.0, false));
    assert_eq!(relevance(1.5, Probability), (1.0, true));
    assert_eq!(relevance(-0.5, Probability), (0.0, true));
    // Logits go through the logistic function.
    assert_eq!(relevance(0.0, Logit), (0.5, false));
    let (low, high) = (relevance(-2.0, Logit).0, relevance(3.0, Logit).0);
    assert!(low < 0.5 && high > 0.5 && high < 1.0);
    assert_eq!(relevance(0.5, Logit).0, 1.0 / (1.0 + (-0.5f64).exp()));
    // A score means the same whatever the other documents of its call
    // scored: a probability beside a stray logit is not rescaled.
    assert_eq!(relevance(0.2, Probability).0, 0.2);
    assert_eq!(relevance(0.75, Probability).0, 0.75);
}

#[test]
fn a_failed_rerank_call_is_classified_by_what_went_wrong() {
    let status = anyhow::Error::new(HttpStatusError {
        status: 503,
        body: String::new(),
    })
    .context("reranker HTTP error");
    let cases = [
        (status, RerankFailure::Status),
        (
            anyhow!("reranker skipped as recently unreachable (next attempt in about 9s)"),
            RerankFailure::Unreachable,
        ),
        (
            anyhow!("reranker response count mismatch"),
            RerankFailure::CountMismatch,
        ),
        (
            anyhow!("invalid reranker index/score"),
            RerankFailure::Malformed,
        ),
        (
            anyhow!("reranker response body exceeds 1 byte limit"),
            RerankFailure::Malformed,
        ),
        (
            anyhow::Error::msg("expected value").context("invalid reranker response"),
            RerankFailure::Malformed,
        ),
        (anyhow!("something else"), RerankFailure::Other),
    ];
    for (error, want) in cases {
        assert_eq!(classify(&error), want, "{error:#}");
    }
}

/// Answers each connection with one canned HTTP response (a stand-in
/// reranker on a loopback port) and returns its URL.
async fn canned_reranker(status: &'static str, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rerank", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut seen = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read the request: the head, then the body it announces.
            while let Ok(n) = stream.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&seen).into_owned();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if seen.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    url
}

fn rescorer_at<'a>(
    models: &'a Models,
    loaded: &Loaded,
    scores: TraverseRerankScores,
) -> GraphRescorer<'a> {
    GraphRescorer::new(
        models,
        loaded.scip.clone(),
        Some("model @ uabc".into()),
        scores,
    )
}

fn models_at(url: String) -> Models {
    Models::new(&lci_core::config::Config {
        reranker_url: url,
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn the_frontier_rates_nodes_by_what_the_graph_holds_and_records_a_failed_call() {
    let loaded = loaded();
    let models = models_at("http://127.0.0.1:1/rerank".into());
    let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
    let symbol = loaded.scip.node_id(0).clone();
    let text = rescorer.text(&symbol);
    assert!(text.starts_with("src/search.rs\nfn "), "{text:?}");
    let file = NodeId::file("src/new.rs").unwrap();
    assert_eq!(rescorer.text(&file), "src/new.rs");
    let long = NodeId::file(format!("src/{}.rs", "x".repeat(3000))).unwrap();
    assert_eq!(rescorer.text(&long).chars().count(), RERANK_TEXT_CHARS);

    // The reranker is unreachable: the call fails with that reason, the
    // error is kept, and an empty question is refused without asking.
    let error = (&rescorer)
        .relevance("rank hits", &[symbol.clone(), file.clone()])
        .await
        .unwrap_err();
    assert_eq!(error.reason, RerankFailure::Unreachable, "{error}");
    assert!(!error.message.is_empty());
    let empty = (&rescorer).relevance("  ", &[symbol]).await.unwrap_err();
    assert_eq!(empty.message, "the question is empty");
    assert_eq!(rescorer.errors().len(), 2);
}

#[tokio::test]
async fn the_rescorer_reports_a_reason_per_failure_and_scores_by_its_setting() {
    let loaded = loaded();
    let nodes = [
        loaded.scip.node_id(0).clone(),
        NodeId::file("src/new.rs").unwrap(),
    ];

    // An error status.
    let url = canned_reranker("500 Internal Server Error", "{\"error\":\"x\"}").await;
    let models = models_at(url);
    let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
    let error = (&rescorer).relevance("q", &nodes).await.unwrap_err();
    assert_eq!(error.reason, RerankFailure::Status, "{error}");
    // An answer that is not JSON.
    let models = models_at(canned_reranker("200 OK", "not json").await);
    let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
    let error = (&rescorer).relevance("q", &nodes).await.unwrap_err();
    assert_eq!(error.reason, RerankFailure::Malformed, "{error}");
    // The wrong number of scores.
    let one = "{\"results\":[{\"index\":0,\"relevance_score\":0.5}]}";
    let models = models_at(canned_reranker("200 OK", one).await);
    let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
    let error = (&rescorer).relevance("q", &nodes).await.unwrap_err();
    assert_eq!(error.reason, RerankFailure::CountMismatch, "{error}");

    // Two scores, one outside [0, 1]: each is read by itself.
    let body = "{\"results\":[{\"index\":0,\"relevance_score\":0.25},{\"index\":1,\"relevance_score\":4.0}]}";
    let models = models_at(canned_reranker("200 OK", body).await);
    let probability = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
    let got = (&probability).relevance("q", &nodes).await.unwrap();
    assert_eq!(
        got,
        [0.25, 1.0],
        "the 4.0 is clamped, the 0.25 is not rescaled"
    );
    assert_eq!(probability.clamped(), 1);
    let models = models_at(canned_reranker("200 OK", body).await);
    let logit = rescorer_at(&models, &loaded, TraverseRerankScores::Logit);
    let got = (&logit).relevance("q", &nodes).await.unwrap();
    assert_eq!(got[0], 1.0 / (1.0 + (-0.25f64).exp()));
    assert_eq!(got[1], 1.0 / (1.0 + (-4.0f64).exp()));
    assert_eq!(logit.clamped(), 0);
}

#[test]
fn the_rescorers_identity_names_the_reranker_and_how_its_scores_are_read() {
    let loaded = loaded();
    let models = Models::new(&lci_core::config::Config::default()).unwrap();
    let identity = |reranker: Option<&str>, scores| {
        let rescorer = GraphRescorer::new(
            &models,
            loaded.scip.clone(),
            reranker.map(str::to_owned),
            scores,
        );
        (&rescorer).identity()
    };
    let probability = identity(Some("m @ u1"), TraverseRerankScores::Probability);
    assert_eq!(probability, "m @ u1 scores=probability");
    assert_eq!(
        identity(Some("m @ u1"), TraverseRerankScores::Logit),
        "m @ u1 scores=logit"
    );
    assert_ne!(
        identity(Some("m @ u2"), TraverseRerankScores::Probability),
        probability
    );
    assert!(identity(None, TraverseRerankScores::Probability).contains("unknown"));
}

#[tokio::test]
async fn the_reranker_frontier_needs_the_application() {
    let tree = tempfile::tempdir().unwrap();
    let index = tree.path().join("index.scip");
    std::fs::write(&index, []).unwrap();
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &index,
        scip_root: Some(tree.path()),
        question: "q",
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let limits = TraverseConfig {
        rerank_frontier: true,
        ..limits()
    };
    let error = run(&limits, &Dirs::under(tree.path()), &call)
        .await
        .err()
        .expect("no reranker")
        .to_string();
    assert!(
        error.contains("rerank_frontier needs the reranker"),
        "{error}"
    );
}

// ---- a resume repeats what its handle was made with (slice R1b) --------

/// A search that only offers the reranker (the seeds are given).
struct WithReranker {
    models: Models,
    identity: &'static str,
}

#[lci_maps::async_trait]
impl SeedSearch for WithReranker {
    async fn search(&self, _question: &str, _k: usize) -> Result<Vec<SeedHit>> {
        Err(anyhow!("not asked: the seeds are given"))
    }

    fn reranker(&self) -> Option<&Models> {
        Some(&self.models)
    }

    fn reranker_identity(&self) -> Option<String> {
        Some(self.identity.to_owned())
    }
}

/// An empty index in a temporary tree, and three file nodes to start from:
/// with a cap of one pop the first call leaves a handle.
struct Tree {
    dir: tempfile::TempDir,
    index: PathBuf,
    seeds: Vec<String>,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.scip");
        std::fs::write(&index, []).unwrap();
        let seeds = ["src/a.rs", "src/b.rs", "src/c.rs"]
            .map(|path| NodeId::file(path).unwrap().to_string())
            .to_vec();
        Self { dir, index, seeds }
    }

    async fn run(
        &self,
        limits: &TraverseConfig,
        search: Option<&dyn SeedSearch>,
        steer: &SteerArgs,
        handle: Option<&str>,
        top_k: usize,
    ) -> Result<Value> {
        let budget = BudgetArgs {
            pops: Some(1),
            ..BudgetArgs::default()
        };
        let call = Call {
            scip_index: &self.index,
            scip_root: Some(self.dir.path()),
            question: "q",
            seeds: &self.seeds,
            seed_search: search,
            top_k,
            maps: None,
            budget: &budget,
            steer,
            handle,
            lsp: None,
        };
        Ok(run(limits, &Dirs::under(self.dir.path()), &call)
            .await?
            .response)
    }
}

fn handle_of(response: &Value) -> String {
    response["handle"]
        .as_str()
        .unwrap_or_else(|| panic!("no handle in {response}"))
        .to_owned()
}

#[tokio::test]
async fn a_resume_must_repeat_the_boost_and_the_top_k_of_its_handle() {
    let tree = Tree::new();
    let steer = SteerArgs::default();
    let first = tree.run(&limits(), None, &steer, None, 8).await.unwrap();
    let handle = handle_of(&first);
    let totals = &first["totals"];
    assert!(
        totals["physical_map_calls"].as_u64().unwrap() >= 1,
        "{totals}"
    );
    assert!(totals["physical_external_calls"].is_u64() && totals["external_calls"].is_u64());
    assert!(
        totals.get("rerank_failure_reasons").is_none(),
        "no frontier, no rerank totals"
    );

    // The same settings resume.
    tree.run(&limits(), None, &steer, Some(&handle), 8)
        .await
        .unwrap();
    // Another boost or another k is refused, naming both.
    let other = TraverseConfig {
        corroboration_boost: 0.5,
        ..limits()
    };
    let error = tree
        .run(&other, None, &steer, Some(&handle), 8)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("boost") && error.contains("0.5") && error.contains("0.1"),
        "{error}"
    );
    let error = tree
        .run(&limits(), None, &steer, Some(&handle), 3)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("top_k") || error.contains("k of"), "{error}");
    assert!(error.contains('8') && error.contains('3'), "{error}");
}

#[tokio::test]
async fn a_resume_must_repeat_the_reranker_its_handle_was_made_with() {
    let tree = Tree::new();
    let steer = SteerArgs::default();
    let on = TraverseConfig {
        rerank_frontier: true,
        ..limits()
    };
    let models = || models_at("http://127.0.0.1:1/rerank".into());
    let search = |identity| WithReranker {
        models: models(),
        identity,
    };
    let (a, b) = (search("model-a @ u1"), search("model-b @ u1"));

    let first = tree.run(&on, Some(&a), &steer, None, 8).await.unwrap();
    let handle = handle_of(&first);
    // The totals say what ran: the physical map calls, and the reranker's
    // failures by reason (none here) and skipped expansions.
    let totals = &first["totals"];
    assert!(
        totals["physical_map_calls"].as_u64().unwrap() >= 1,
        "{totals}"
    );
    assert_eq!(totals["rerank_failure_reasons"], json!({}));
    assert_eq!(totals["rerank_skipped"], 0);
    // The same model, endpoint and score reading resume.
    tree.run(&on, Some(&a), &steer, Some(&handle), 8)
        .await
        .unwrap();
    // Another model, another score reading, the frontier off, and a
    // different floor are each refused.
    async fn resume_refused(
        tree: &Tree,
        limits: &TraverseConfig,
        search: Option<&dyn SeedSearch>,
        handle: &str,
    ) -> String {
        tree.run(limits, search, &SteerArgs::default(), Some(handle), 8)
            .await
            .unwrap_err()
            .to_string()
    }
    let error = resume_refused(&tree, &on, Some(&b), &handle).await;
    assert!(
        error.contains("model-a") && error.contains("model-b"),
        "{error}"
    );
    let logit = TraverseConfig {
        rerank_scores: TraverseRerankScores::Logit,
        ..on.clone()
    };
    let error = resume_refused(&tree, &logit, Some(&a), &handle).await;
    assert!(
        error.contains("scores=probability") && error.contains("scores=logit"),
        "{error}"
    );
    let error = resume_refused(&tree, &limits(), None, &handle).await;
    assert!(error.contains("rerank"), "{error}");
    let floor = TraverseConfig {
        rerank_floor: 0.5,
        ..on.clone()
    };
    let error = resume_refused(&tree, &floor, Some(&a), &handle).await;
    assert!(error.contains("floor"), "{error}");
}

#[tokio::test]
async fn a_continuations_effective_policy_shows_the_weights_and_switches_it_restored() {
    let tree = Tree::new();
    // The rules run at the stop after the first pop: weigh one map down and
    // switch another off.
    let steer = SteerArgs {
        rules: Some(json!([
            {"when": {"depth": {"ge": 0}}, "then": {"map_weight": {"id": "structure.contains", "weight": 0.5}}},
            {"when": {"depth": {"ge": 0}}, "then": {"map_off": "graph.scip"}},
        ])),
        stop_every: Some(1),
        ..SteerArgs::default()
    };
    let first = tree.run(&limits(), None, &steer, None, 8).await.unwrap();
    let shown = |response: &Value, id: &str| -> (f64, bool) {
        let map = response["effective_policy"]["maps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|map| map["id"] == id)
            .unwrap_or_else(|| panic!("no {id} in {response}"));
        (
            map["weight"].as_f64().unwrap(),
            map["enabled"].as_bool().unwrap(),
        )
    };
    // The first call shows the maps as registered.
    assert_eq!(shown(&first, "structure.contains"), (1.0, true));
    assert_eq!(shown(&first, "graph.scip"), (1.0, true));
    let handle = handle_of(&first);
    let second = tree
        .run(&limits(), None, &SteerArgs::default(), Some(&handle), 8)
        .await
        .unwrap();
    assert_eq!(shown(&second, "structure.contains"), (0.5, true));
    assert_eq!(shown(&second, "graph.scip"), (1.0, false));
    assert_ne!(
        first["effective_policy"]["hash"],
        second["effective_policy"]["hash"]
    );
}

#[test]
fn a_handle_is_bound_to_the_workspace_and_the_index() {
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = |index: &'static str, root: Option<&'static str>| Call {
        scip_index: Path::new(index),
        scip_root: root.map(Path::new),
        question: "q",
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let stamped = |len: u64| {
        let mut loaded = loaded();
        loaded.stamp = (Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5)), len);
        loaded
    };
    let base = identity_of(&call("graph.scip", Some("work/a")), &stamped(10));
    assert_eq!(
        base,
        identity_of(&call("graph.scip", Some("work/a")), &stamped(10)),
        "the same workspace and index give the same identity"
    );
    for other in [
        identity_of(&call("graph.scip", Some("work/b")), &stamped(10)),
        identity_of(&call("graph.scip", None), &stamped(10)),
        identity_of(&call("other.scip", Some("work/a")), &stamped(10)),
        identity_of(&call("graph.scip", Some("work/a")), &stamped(11)),
    ] {
        assert_ne!(base, other);
    }
    // A short hash of each, not the paths.
    assert!(!base.contains("work") && !base.contains("scip"), "{base}");
}

fn hit(path: &str, lines: (u32, u32), rank: usize) -> SeedHit {
    SeedHit {
        path: path.into(),
        start_line: lines.0,
        end_line: lines.1,
        content_hash: "abc123".into(),
        ordinal: None,
        rank,
        retrievers: vec!["semantic".into(), "lexical".into()],
        semantic_rank: Some(rank),
        lexical_rank: None,
        lsp_rank: None,
        fusion_score: 0.25,
        reranker_score: Some(0.75),
    }
}

/// The name (or node id of a chunk) and `how` of each seed the hits map to.
fn mapped(loaded: &Loaded, question: &str, hits: &[SeedHit]) -> Vec<(String, String)> {
    map_hits(loaded, question, hits)
        .iter()
        .map(|seed| {
            let name = match seed.info["symbol"].as_str() {
                Some(symbol) => symbol.rsplit("::").next().unwrap().to_owned(),
                None => seed.node.to_string(),
            };
            (name, seed.info["how"].as_str().unwrap().to_owned())
        })
        .collect()
}

fn pair(name: &str, how: &str) -> (String, String) {
    (name.to_owned(), how.to_owned())
}

/// Lines are counted from 1 in chunks and from 0 in the graph: `parse_query`
/// is on 0-based line 12, so on 1-based line 13, and ends on line 15.
#[test]
fn a_chunk_maps_to_the_function_it_defines_or_the_innermost_one_around_it() {
    let loaded = loaded();
    // (lines, name, how, the only seed): only some rows pin the whole list.
    for (lines, name, how, only) in [
        ((1, 10), "search", "defined", false),
        ((13, 15), "parse_query", "defined", false),
        ((16, 40), "rank_hits", "defined", false),
        // the nested function starts here, whatever lies around it
        ((21, 23), "score_inner", "defined", false),
        ((13, 13), "parse_query", "defined", true),
        // past the name, inside the body; line 15 is its last line
        ((14, 14), "parse_query", "inside", true),
        ((15, 15), "parse_query", "inside", true),
        ((16, 16), "rank_hits", "defined", true),
        // inside `rank_hits`, before the nested function
        ((17, 19), "rank_hits", "inside", true),
        // inside the nested function: it, not the function around it
        ((22, 23), "score_inner", "inside", true),
        // past the nested function, inside the outer one again
        ((27, 30), "rank_hits", "inside", true),
        // straddling the end of the nested function: only the outer covers it
        ((23, 28), "rank_hits", "inside", true),
    ] {
        let got = mapped(&loaded, "x", &[hit("src/search.rs", lines, 1)]);
        assert_eq!(got.first(), Some(&pair(name, how)), "{lines:?}: {got:?}");
        assert!(!only || got.len() == 1, "{lines:?}: {got:?}");
    }
    // Line 12 (1-based) is the gap line before `parse_query`: no function.
    assert_eq!(
        mapped(&loaded, "x", &[hit("src/search.rs", (12, 12), 1)])[0].1,
        "chunk"
    );
    // A chunk in the other file.
    let got = mapped(&loaded, "x", &[hit("src/other.rs", (4, 6), 1)]);
    assert_eq!(got, [pair("helper", "defined")]);
}

#[tokio::test]
async fn a_chunk_seed_between_functions_is_expanded_by_the_containment_map() {
    let loaded = loaded();
    // Line 12 (1-based) is the gap before `parse_query`: a chunk seed.
    let hits = [
        hit("src/search.rs", (11, 12), 1),
        hit("src/other.rs", (4, 6), 2),
    ];
    let seeds = map_hits(&loaded, "x", &hits);
    let found = Seeds {
        nodes: seeds
            .iter()
            .map(|seed| Seed {
                node: seed.node.clone(),
                map: Some(SEEDS_HYBRID),
            })
            .collect(),
        details: seeds.iter().map(|seed| seed.info.clone()).collect(),
        ..Seeds::default()
    };
    // Only the chunk node has lines to give.
    let lines = chunk_lines(&found);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!((lines[0].0.kind(), lines[0].1), (NodeKind::Chunk, (11, 12)));

    let map = loaded.contains.with_chunk_lines(lines.clone());
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let budget = Budget {
        deadline: Instant::now() + Duration::from_secs(5),
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    };
    let edges = map.expand(&lines[0].0, budget, &view).await.edges;
    // No function lies on those lines: all the file's functions follow.
    assert_eq!(edges.len(), 9, "{edges:?}");
    assert!(
        edges
            .iter()
            .all(|e| e.kind == EdgeKind::Contains && e.map == CONTAINS_MAP_ID)
    );
}

#[test]
fn a_chunk_between_functions_or_in_an_unknown_file_stays_a_chunk_node() {
    let loaded = loaded();
    // The gap after `search` (1-based 11 and 12) belongs to no function.
    let seeds = map_hits(&loaded, "x", &[hit("src/search.rs", (11, 12), 1)]);
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].info["kind"], "chunk");
    assert_eq!(
        seeds[0].node,
        NodeId::chunk("src/search.rs", "abc123", 0).unwrap()
    );
    // Past the last function; and a file the index does not list.
    for (path, lines) in [("src/search.rs", (60, 70)), ("src/missing.rs", (1, 9))] {
        let seeds = map_hits(&loaded, "x", &[hit(path, lines, 1)]);
        assert_eq!(seeds[0].info["kind"], "chunk", "{path}");
        assert_eq!(seeds[0].node.path(), Some(path));
    }
    // The ordinal of the chunk is part of its id.
    let mut second = hit("src/missing.rs", (1, 9), 1);
    second.ordinal = Some(1);
    assert_eq!(
        map_hits(&loaded, "x", &[second])[0].node,
        NodeId::chunk("src/missing.rs", "abc123", 1).unwrap()
    );
    // A chunk with no usable hash falls back to its file; none at all
    // to no seed.
    let mut bare = hit("src/missing.rs", (1, 9), 1);
    bare.content_hash.clear();
    let seeds = map_hits(&loaded, "x", &[bare]);
    assert_eq!(seeds[0].node, NodeId::file("src/missing.rs").unwrap());
    assert_eq!(seeds[0].info["kind"], "file");
    assert!(map_hits(&loaded, "x", &[hit("", (1, 9), 1)]).is_empty());
}

#[test]
fn cards_get_a_site_where_the_source_knows_one() {
    let loaded = loaded();
    let seeds = map_hits(
        &loaded,
        "x",
        &[
            hit("src/search.rs", (11, 12), 1),
            hit("src/search.rs", (2, 2), 2),
        ],
    );
    let symbol = seeds.iter().find(|s| s.info["kind"] == "symbol").unwrap();
    let chunk = seeds.iter().find(|s| s.info["kind"] == "chunk").unwrap();
    let unknown = NodeId::ConfigKey {
        namespace: lci_core::node::ConfigNamespace::Toml,
        key: "nope.key".into(),
    }
    .to_string();
    let file = NodeId::file("src/elsewhere.rs").unwrap().to_string();
    let card = |node: &str| json!({"node": node, "via": null});
    let mut response = json!({
        "results": [
            card(&symbol.node.to_string()),
            card(&chunk.node.to_string()),
            card(&unknown),
            card(&file),
        ],
        "checkpoint": {
            "answer": [card(&symbol.node.to_string())],
            "visited_cards": [],
            "frontier": [card(&symbol.node.to_string())],
        },
    });
    let details: Vec<Value> = seeds.iter().map(|s| s.info.clone()).collect();
    add_sites(&mut response, &loaded, None, &details);
    let site = &response["results"][0]["site"];
    assert_eq!(site["path"], "src/search.rs");
    assert!(site["line"].as_u64().unwrap() >= 1 && site["col"].as_u64().unwrap() >= 1);
    // A chunk node: its file and the first line of the chunk.
    assert_eq!(
        response["results"][1]["site"],
        json!({"path": "src/search.rs", "line": 11, "col": 1})
    );
    // Nothing locates this one, so the field is left out.
    assert!(response["results"][2].get("site").is_none());
    // A file node is at its first line, wherever it is.
    assert_eq!(
        response["results"][3]["site"],
        json!({"path": "src/elsewhere.rs", "line": 1, "col": 1})
    );
    // The checkpoint's cards carry the same site as the results'.
    assert_eq!(response["checkpoint"]["answer"][0]["site"], *site);
    assert_eq!(response["checkpoint"]["frontier"][0]["site"], *site);
}

#[test]
fn a_seed_has_its_site_on_a_run_that_returns_no_checkpoint() {
    let loaded = loaded();
    let seeds = map_hits(&loaded, "x", &[hit("src/search.rs", (13, 15), 1)]);
    let symbol = seeds.iter().find(|s| s.info["kind"] == "symbol").unwrap();
    let given = NodeId::file("src/given.rs").unwrap().to_string();
    // A complete run: the seeds are not results, and there is no checkpoint.
    let mut response = json!({
        "seeds": [symbol.node.to_string(), given],
        "results": [],
    });
    let details: Vec<Value> = seeds.iter().map(|s| s.info.clone()).collect();
    add_sites(&mut response, &loaded, None, &details);
    let sites = &response["seed_sites"];
    assert_eq!(sites[symbol.node.to_string()]["path"], "src/search.rs");
    assert!(sites[symbol.node.to_string()]["line"].as_u64().unwrap() >= 1);
    assert_eq!(
        sites[&given],
        json!({"path": "src/given.rs", "line": 1, "col": 1})
    );
}

#[test]
fn a_symbol_outside_the_graph_is_located_by_scip_impact() {
    // A struct is no node of the SCIP graph (functions only); scip.impact
    // gives it an id of its own and knows where it is defined.
    let symbol = "rust-analyzer cargo demo 0.1.0 types/Config#".to_owned();
    let docs = vec![RawDocument {
        path: "src/types.rs".into(),
        occurrences: vec![RawOccurrence {
            range: (2, 11, 2, 17),
            symbol: symbol.clone(),
            roles: 1,
            enclosing: Some((2, 0, 4, 1)),
        }],
    }];
    let kinds = HashMap::from([(
        symbol.clone(),
        scip::types::symbol_information::Kind::Struct,
    )]);
    let impact = ImpactMap::new(
        ImpactIndex::build(docs, &kinds, None),
        &|_| None,
        ImpactOptions::default(),
    );
    let node = impact.node_of(&symbol).unwrap().to_string();
    let loaded = loaded();
    let mut response = json!({"results": [{"node": node, "via": "scip.impact"}]});
    add_sites(&mut response, &loaded, None, &[]);
    assert!(response["results"][0].get("site").is_none());
    add_sites(&mut response, &loaded, Some(&impact), &[]);
    assert_eq!(
        response["results"][0]["site"],
        json!({"path": "src/types.rs", "line": 3, "col": 12})
    );
}

#[test]
fn a_chunk_with_many_definitions_keeps_the_ones_the_question_names() {
    let loaded = loaded();
    // Lines 41 to 45 (1-based) hold five one-line functions.
    let hits = [hit("src/search.rs", (41, 45), 1)];
    let got = mapped(&loaded, "save user and find user", &hits);
    assert_eq!(
        got,
        [
            pair("load_user", "defined"),
            pair("save_user", "defined"),
            pair("find_user", "defined"),
        ],
        "the two named ones, then the first by line among the rest, in line order"
    );
    let seeds = map_hits(&loaded, "save user and find user", &hits);
    assert!(seeds.iter().all(|s| s.info["symbols_in_chunk"] == 5));
    // A question that names none keeps the first three by line.
    let got = mapped(&loaded, "zebra", &hits);
    assert_eq!(
        got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["load_user", "save_user", "drop_user"]
    );
}

#[test]
fn hits_that_share_a_node_make_one_seed_and_a_windows_path_matches() {
    let loaded = loaded();
    let hits = [
        hit("src/search.rs", (13, 15), 1),
        hit("src\\other.rs", (4, 6), 2),
        hit("src/search.rs", (14, 14), 3),
    ];
    let seeds = map_hits(&loaded, "x", &hits);
    assert_eq!(seeds.len(), 2);
    assert_eq!(seeds[0].info["hit_rank"], 1);
    assert_eq!(seeds[0].info["also_hit_ranks"], json!([3]));
    let helper = seeds[1].info["symbol"].as_str().unwrap();
    assert!(helper.ends_with("helper"), "{helper}");
}

#[test]
fn a_seed_records_the_retrievers_and_the_score_that_found_it() {
    let loaded = loaded();
    let mut found = hit("src/search.rs", (13, 15), 4);
    found.retrievers = vec!["lexical".into()];
    found.semantic_rank = None;
    found.lexical_rank = Some(2);
    found.reranker_score = None;
    found.fusion_score = 0.5;
    let seeds = map_hits(&loaded, "x", &[found]);
    let info = &seeds[0].info;
    assert_eq!(info["kind"], "symbol");
    assert_eq!(info["retrievers"], json!(["lexical"]));
    assert_eq!(info["score"], 0.5);
    assert_eq!(info["score_kind"], "fusion", "no reranker score: fusion");
    assert_eq!(info["hit_rank"], 4);
    assert_eq!(info["channel_ranks"]["lexical"], 2);
    assert_eq!(info["channel_ranks"]["semantic"], json!(null));
    assert_eq!(info["path"], "src/search.rs");
    assert_eq!(info["lines"], json!([13, 15]));
    // With a reranker score, that is the score.
    let seeds = map_hits(&loaded, "x", &[hit("src/search.rs", (13, 15), 1)]);
    assert_eq!(seeds[0].info["score"], 0.75);
    assert_eq!(seeds[0].info["score_kind"], "reranker");
}

/// A search with a canned answer that counts its calls.
struct Canned {
    answer: std::result::Result<Vec<SeedHit>, String>,
    calls: AtomicUsize,
    asked: Mutex<Vec<(String, usize)>>,
}

impl Canned {
    fn new(answer: std::result::Result<Vec<SeedHit>, String>) -> Self {
        Self {
            answer,
            calls: AtomicUsize::new(0),
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[lci_maps::async_trait]
impl SeedSearch for Canned {
    async fn search(&self, question: &str, k: usize) -> Result<Vec<SeedHit>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.asked.lock().unwrap().push((question.to_owned(), k));
        self.answer.clone().map_err(|e| anyhow!("{e}"))
    }
}

async fn seeds_for(
    search: Option<&dyn SeedSearch>,
    question: &str,
    given: &[String],
    wanted: &[MapId],
) -> Seeds {
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: Path::new("unused"),
        scip_root: None,
        question,
        seeds: given,
        seed_search: search,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    find_seeds(&limits(), &call, &loaded(), wanted)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_hybrid_search_finds_the_seeds_and_asks_for_seed_k_hits() {
    let search = Canned::new(Ok(vec![
        hit("src/search.rs", (13, 15), 1),
        hit("src/other.rs", (4, 6), 2),
    ]));
    let found = seeds_for(Some(&search), "how are queries parsed", &[], &MAPS).await;
    assert_eq!(found.source, "hybrid");
    assert!(found.fallback.is_none());
    assert_eq!(found.nodes.len(), 2);
    assert!(found.nodes.iter().all(|s| s.map == Some(SEEDS_HYBRID)));
    assert_eq!(found.details.len(), 2);
    assert_eq!(
        *search.asked.lock().unwrap(),
        [("how are queries parsed".to_owned(), 8)],
        "[traverse] seed_k defaults to 8"
    );
}

#[tokio::test]
async fn the_stand_in_is_a_visible_fallback_and_never_silent() {
    let named = "how does rank hits score a hit";
    let failing = Canned::new(Err("the embedder is down".into()));
    let empty = Canned::new(Ok(Vec::new()));
    for (search, why) in [
        (Some(&failing as &dyn SeedSearch), "embedder is down"),
        (Some(&empty as &dyn SeedSearch), "no hits"),
        (None, "no workspace"),
    ] {
        let found = seeds_for(search, named, &[], &MAPS).await;
        assert_eq!(found.source, "name_tokens", "{why}");
        assert!(!found.nodes.is_empty(), "{why}: the stand-in found names");
        let reason = found.fallback.as_deref().unwrap();
        assert!(reason.contains(why), "{reason}");
        assert!(found.notes[0].contains("name_tokens"), "{:?}", found.notes);
        assert!(
            found
                .details
                .iter()
                .all(|d| d["retrievers"] == json!(["name_tokens"]))
        );
    }
}

#[tokio::test]
async fn explicit_seeds_and_the_stand_in_alone_do_not_search() {
    let search = Canned::new(Ok(vec![hit("src/search.rs", (13, 15), 1)]));
    let id = "sym:rust:search::search/fn";
    let given = seeds_for(Some(&search), "q", &[id.to_owned()], &MAPS).await;
    assert_eq!(given.source, "given");
    assert_eq!(given.nodes.len(), 1);
    assert!(given.nodes[0].map.is_none());
    // Only the stand-in map wanted: no search, no fallback note.
    let names = seeds_for(Some(&search), "rank hits", &[], &[SEEDS_MAP, SCIP_MAP_ID]).await;
    assert_eq!(names.source, "name_tokens");
    assert!(names.fallback.is_none() && names.notes.is_empty());
    assert_eq!(search.calls.load(Ordering::SeqCst), 0);
    // Hybrid wanted, nothing found anywhere, no stand-in: the reason is
    // kept and the source says hybrid.
    let none = seeds_for(None, "zebra", &[], &[SEEDS_HYBRID, SCIP_MAP_ID]).await;
    assert!(none.nodes.is_empty() && none.source == "hybrid");
    assert!(none.fallback.is_some());
    let nothing = seeds_for(None, "zebra", &[], &[SCIP_MAP_ID]).await;
    assert_eq!(nothing.source, "none");
}

#[tokio::test]
async fn the_tree_maps_are_named_not_default_and_need_the_root() {
    assert!(OPT_IN_MAPS.iter().all(|id| !MAPS.contains(id)));
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let run_with = |maps: &[&str]| {
        let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
        let (budget, steer) = (budget.clone(), steer.clone());
        async move {
            let dirs = Dirs::under(Path::new("unused"));
            let call = Call {
                scip_index: Path::new("unused"),
                scip_root: None,
                question: "q",
                seeds: &[],
                seed_search: None,
                top_k: 8,
                maps: Some(&maps),
                budget: &budget,
                steer: &steer,
                handle: None,
                lsp: None,
            };
            run(&limits(), &dirs, &call)
                .await
                .err()
                .map(|e| e.to_string())
        }
    };
    let no_root = run_with(&["graph.scip", "grep.live"]).await.unwrap();
    assert!(no_root.contains("give its root"), "{no_root}");
    let no_root = run_with(&["config.key"]).await.unwrap();
    assert!(no_root.contains("give its root"), "{no_root}");
    for id in [
        SEARCH_LEXICAL_ID,
        SEARCH_VECTOR_ID,
        SCIP_IMPACT_ID,
        LSP_LIVE_ID,
        EXACT_CLOSURE_ID,
        EXACT_PICKAXE_ID,
    ] {
        assert!(!MAPS.contains(&id), "{id} is opt-in");
        let no_root = run_with(&[id]).await.unwrap();
        assert!(no_root.contains("give its root"), "{id}: {no_root}");
    }
    let unknown = run_with(&["nope"]).await.unwrap();
    assert!(
        unknown.contains("grep.live")
            && unknown.contains("config.key")
            && unknown.contains("search.lexical")
            && unknown.contains("search.vector")
            && unknown.contains("scip.impact"),
        "{unknown}"
    );
}

/// One call over the three-file tree: the given maps and first-stage keys.
async fn first_stage_run(tree: &Tree, limits: &TraverseConfig, maps: Option<&[&str]>) -> Value {
    let maps: Option<Vec<String>> = maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
    let (budget, steer) = (
        BudgetArgs {
            pops: Some(1),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "q",
        seeds: &tree.seeds,
        seed_search: None,
        top_k: 8,
        maps: maps.as_deref(),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    run(limits, &Dirs::under(tree.dir.path()), &call)
        .await
        .unwrap()
        .response
}

#[tokio::test]
async fn with_the_first_stage_off_the_output_is_the_walks_alone() {
    let tree = Tree::new();
    let response = first_stage_run(&tree, &limits(), None).await;
    let results = response["results"].as_array().expect("results");
    assert!(
        results.iter().all(|card| card.get("origin").is_none()),
        "{response}"
    );
    // The other keys do nothing while the mode is off.
    let others = TraverseConfig {
        first_k: 3,
        answer_seeds: true,
        ..limits()
    };
    let same = first_stage_run(&tree, &others, None).await;
    assert_eq!(same["results"], response["results"]);
    assert!(same["totals"].get("first_stage").is_none(), "{same}");
    assert!(
        response["totals"].get("first_stage").is_none(),
        "{response}"
    );
    assert!(response.get("exact").is_none(), "{response}");

    // Naming the exact maps without turning the first stage on changes
    // no card either; the report says what each map answered.
    let named = first_stage_run(&tree, &limits(), Some(&["graph.scip", EXACT_CLOSURE_ID])).await;
    assert!(
        named["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|card| card.get("origin").is_none()),
        "{named}"
    );
    assert!(named["totals"].get("first_stage").is_none(), "{named}");
    assert_eq!(named["exact"][0]["map"], EXACT_CLOSURE_ID, "{named}");
    assert_eq!(named["exact"][0]["answered"], false, "{named}");
}

#[tokio::test]
async fn an_exact_map_that_resolves_nothing_abstains_with_a_note() {
    let tree = Tree::new();
    let limits = TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        answer_seeds: true,
        ..limits()
    };
    let response = first_stage_run(&tree, &limits, Some(&["graph.scip", EXACT_CLOSURE_ID])).await;
    let notes = response["notes"].to_string();
    assert!(notes.contains("exact.closure"), "{response}");
    assert_eq!(response["exact"][0]["answered"], false, "{response}");
    // The first stage is the seeds, then the walk's cards after them.
    let results = response["results"].as_array().expect("results");
    let origins: Vec<&str> = results
        .iter()
        .map(|card| card["origin"].as_str().expect("origin"))
        .collect();
    assert!(origins.contains(&"seed"), "{origins:?}");
    let first_walk = origins.iter().position(|o| *o == "walk");
    let last_seed = origins.iter().rposition(|o| *o == "seed").unwrap();
    assert!(
        first_walk.is_none_or(|walk| walk > last_seed),
        "{origins:?}"
    );
    let counts = &response["totals"]["first_stage"];
    assert_eq!(
        counts["seeds"].as_u64().unwrap() as usize,
        origins.iter().filter(|o| **o == "seed").count(),
        "{response}"
    );
    // No node is listed twice.
    let mut nodes: Vec<&str> = results
        .iter()
        .map(|card| card["node"].as_str().unwrap())
        .collect();
    let listed = nodes.len();
    nodes.sort_unstable();
    nodes.dedup();
    assert_eq!(nodes.len(), listed, "{response}");
}

/// One call over the three-file tree under the named policy.
async fn policy_run(
    tree: &Tree,
    limits: &TraverseConfig,
    policy: &str,
    top_k: usize,
    maps: Option<&[&str]>,
) -> Value {
    let maps: Option<Vec<String>> = maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
    let (budget, steer) = (
        BudgetArgs::default(),
        SteerArgs {
            policy: Some(policy.to_owned()),
            ..SteerArgs::default()
        },
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "q",
        seeds: &tree.seeds,
        seed_search: None,
        top_k,
        maps: maps.as_deref(),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    run(limits, &Dirs::under(tree.dir.path()), &call)
        .await
        .unwrap()
        .response
}

fn notes_of(response: &Value) -> String {
    response["notes"].to_string()
}

#[tokio::test]
async fn the_default_configuration_has_the_measured_first_stage() {
    let defaults = TraverseConfig {
        enabled: true,
        ..TraverseConfig::default()
    };
    let tree = Tree::new();
    let response = first_stage_run(&tree, &defaults, None).await;
    let origins: Vec<&str> = response["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|card| card["origin"].as_str().expect("origin"))
        .collect();
    // The start nodes answer (answer_seeds), ahead of the walk's cards.
    assert!(origins.contains(&"seed"), "{response}");
    assert!(response["totals"]["first_stage"].is_object(), "{response}");
}

#[tokio::test]
async fn impact_and_history_answer_from_the_first_stage_alone() {
    let tree = Tree::new();
    let limits = TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        answer_seeds: true,
        ..limits()
    };
    let maps = ["graph.scip", "structure.contains"];
    for policy in ["impact", "history"] {
        let response = policy_run(&tree, &limits, policy, 8, Some(&maps)).await;
        let results = response["results"].as_array().expect("results");
        assert!(!results.is_empty(), "{policy}: {response}");
        assert!(
            results.iter().all(|card| card["origin"] != "walk"),
            "{policy}: {response}"
        );
        assert_eq!(
            response["totals"]["first_stage"]["walk"], 0,
            "{policy}: {response}"
        );
        assert!(
            notes_of(&response).contains("first stage alone answers"),
            "{policy}: {response}"
        );
        assert!(
            response["totals"]["pops"].as_u64().unwrap() <= 1,
            "{response}"
        );
    }
    // The same call under flow (or no policy) keeps the walk: no such note.
    for policy in ["flow", "explore"] {
        let response = policy_run(&tree, &limits, policy, 8, Some(&maps)).await;
        assert!(
            !notes_of(&response).contains("first stage alone"),
            "{policy}: {response}"
        );
    }
}

#[tokio::test]
async fn where_to_walks_only_when_the_call_asks_for_twenty_answers() {
    let tree = Tree::new();
    let limits = TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        answer_seeds: true,
        ..limits()
    };
    let maps = ["graph.scip", "structure.contains"];
    // first_k 10 + top_k 8 = 18: the first stage alone.
    let below = policy_run(&tree, &limits, "where-to", 8, Some(&maps)).await;
    assert!(
        notes_of(&below).contains("first stage alone answers"),
        "{below}"
    );
    // first_k 10 + top_k 10 = 20: the walk runs.
    let at = policy_run(&tree, &limits, "where-to", 10, Some(&maps)).await;
    assert!(!notes_of(&at).contains("first stage alone"), "{at}");
    // A config key moves the threshold, and first_k counts.
    let mut own = limits.clone();
    own.policies.insert(
        "where-to".into(),
        TraversePolicy {
            walk_min_answers: Some(15),
            ..TraversePolicy::default()
        },
    );
    let lowered = policy_run(&tree, &own, "where-to", 8, Some(&maps)).await;
    assert!(
        !notes_of(&lowered).contains("first stage alone"),
        "{lowered}"
    );
    // An uncapped first stage (first_k 0) always meets the minimum.
    let uncapped = TraverseConfig {
        first_k: 0,
        ..limits.clone()
    };
    let all = policy_run(&tree, &uncapped, "where-to", 1, Some(&maps)).await;
    assert!(!notes_of(&all).contains("first stage alone"), "{all}");
}

#[tokio::test]
async fn a_policy_cannot_remove_the_only_answers_a_call_has() {
    // With the first stage off the walk is the only answer: the walk
    // switch is ignored, and the call says so.
    let tree = Tree::new();
    let maps = ["graph.scip", "structure.contains"];
    let response = policy_run(&tree, &limits(), "impact", 8, Some(&maps)).await;
    let notes = notes_of(&response);
    assert!(
        notes.contains("[traverse] first_stage is off: the walk runs"),
        "{notes}"
    );
    assert!(!notes.contains("the walk took one pop"), "{notes}");
}

#[tokio::test]
async fn a_policy_map_that_needs_the_tree_is_reported_when_there_is_no_tree() {
    let tree = Tree::new();
    let limits = TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        answer_seeds: true,
        ..limits()
    };
    let (budget, steer) = (
        BudgetArgs::default(),
        SteerArgs {
            policy: Some("impact".to_owned()),
            ..SteerArgs::default()
        },
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: None,
        question: "q",
        seeds: &tree.seeds,
        seed_search: None,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let response = run(&limits, &Dirs::under(tree.dir.path()), &call)
        .await
        .expect("run")
        .response;
    let notes = notes_of(&response);
    assert!(
        notes.contains("exact.closure") && notes.contains("left out"),
        "{response}"
    );
}

#[tokio::test]
async fn a_policy_adds_its_maps_only_when_the_call_names_none() {
    let tree = Tree::new();
    let limits = TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        answer_seeds: true,
        ..limits()
    };
    // impact reads the exact closure: the exact report shows the operator ran.
    let response = policy_run(&tree, &limits, "impact", 8, None).await;
    assert_eq!(response["exact"][0]["map"], EXACT_CLOSURE_ID, "{response}");
    // A named maps list is used as given.
    let named = policy_run(&tree, &limits, "impact", 8, Some(&["graph.scip"])).await;
    assert!(named.get("exact").is_none(), "{named}");
    // flow and the others add none.
    let flow = policy_run(&tree, &limits, "flow", 8, None).await;
    assert!(flow.get("exact").is_none(), "{flow}");
    // A wrong map in a policy table is an error that names the policy.
    let mut bad = limits.clone();
    bad.policies.insert(
        "flow".into(),
        TraversePolicy {
            extra_maps: Some(vec!["nope.map".into()]),
            ..TraversePolicy::default()
        },
    );
    let (budget, steer) = (
        BudgetArgs::default(),
        SteerArgs {
            policy: Some("flow".into()),
            ..SteerArgs::default()
        },
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "q",
        seeds: &tree.seeds,
        seed_search: None,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let error = run(&bad, &Dirs::under(tree.dir.path()), &call)
        .await
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("policy flow") && error.contains("nope.map"),
        "{error}"
    );
}

#[tokio::test]
async fn a_continuation_does_not_run_the_first_stage_again() {
    let tree = Tree::new();
    let named = ["graph.scip", EXACT_CLOSURE_ID];
    let limits = TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        answer_seeds: true,
        ..limits()
    };
    let first = first_stage_run(&tree, &limits, Some(&named)).await;
    if first.get("handle").is_none() {
        return; // the walk finished within one pop: nothing to continue
    }
    let handle = handle_of(&first);
    let maps: Vec<String> = named.iter().map(|id| (*id).to_owned()).collect();
    let (budget, steer) = (
        BudgetArgs {
            pops: Some(1),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "q",
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: Some(&handle),
        lsp: None,
    };
    let next = run(&limits, &Dirs::under(tree.dir.path()), &call)
        .await
        .unwrap()
        .response;
    assert!(
        next["notes"].to_string().contains("not run again"),
        "{next}"
    );
    assert!(next["totals"].get("first_stage").is_none(), "{next}");
}

// ---- the exact operators answering (slice BR4b) -----------------------

/// The source of the tree the exact operators are asked about.
const EXACT_SOURCE: &str =
    "struct Foo;\nfn alpha() { let _ = Foo; }\nfn beta() { Foo; Foo; }\nstruct Holder { f: Foo }\n";

/// A tree with `a.rs` (see [`EXACT_SOURCE`]) and a SCIP index over it:
/// `alpha` holds one reference to `Foo`, `beta` two, `Holder` (a type, so
/// not a node of the SCIP graph) one.
struct ScipTree {
    dir: tempfile::TempDir,
    index: PathBuf,
}

impl ScipTree {
    #[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
    fn new() -> Self {
        use scip::types::symbol_information::Kind;
        use scip::types::{Document, Index, Occurrence, SymbolInformation};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), EXACT_SOURCE).unwrap();
        let sym = |path: &str| format!("rust-analyzer cargo demo 0.1.0 {path}");
        let occurrence = |range: &[i32], symbol: &str, roles: i32, enclosing: &[i32]| {
            let mut o = Occurrence::default();
            o.range = range.to_vec();
            o.symbol = sym(symbol);
            o.symbol_roles = roles;
            o.enclosing_range = enclosing.to_vec();
            o
        };
        let info = |symbol: &str, kind: Kind| {
            let mut i = SymbolInformation::default();
            i.symbol = sym(symbol);
            i.kind = kind.into();
            i
        };
        let mut document = Document::default();
        document.relative_path = "a.rs".to_owned();
        document.occurrences = vec![
            occurrence(&[0, 7, 10], "a/Foo#", 1, &[0, 0, 0, 11]),
            occurrence(&[1, 3, 8], "a/alpha().", 1, &[1, 0, 1, 27]),
            occurrence(&[1, 21, 24], "a/Foo#", 0, &[]),
            occurrence(&[2, 3, 7], "a/beta().", 1, &[2, 0, 2, 23]),
            occurrence(&[2, 12, 15], "a/Foo#", 0, &[]),
            occurrence(&[2, 17, 20], "a/Foo#", 0, &[]),
            occurrence(&[3, 7, 13], "a/Holder#", 1, &[3, 0, 3, 24]),
            occurrence(&[3, 19, 22], "a/Foo#", 0, &[]),
        ];
        document.symbols = vec![
            info("a/Foo#", Kind::Struct),
            info("a/alpha().", Kind::Function),
            info("a/beta().", Kind::Function),
            info("a/Holder#", Kind::Struct),
        ];
        let mut message = Index::default();
        message.documents = vec![document];
        let index = dir.path().join("index.scip");
        scip::write_message_to_file(&index, message).unwrap();
        Self { dir, index }
    }

    /// `a.rs` in a git repository of one commit.
    fn commit(&self) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(self.dir.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "a.rs"]);
        git(&["commit", "-q", "-m", "add Foo"]);
    }

    async fn run(
        &self,
        limits: &TraverseConfig,
        maps: &[&str],
        seeds: &[String],
        deadline_ms: Option<u64>,
    ) -> Value {
        let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
        let (budget, steer) = (
            BudgetArgs {
                pops: Some(3),
                deadline_ms,
                ..BudgetArgs::default()
            },
            SteerArgs::default(),
        );
        let call = Call {
            scip_index: &self.index,
            scip_root: Some(self.dir.path()),
            question: if maps.iter().any(|id| id == EXACT_PICKAXE_ID) {
                "Which commit first created `Foo`?"
            } else {
                "What breaks if `Foo` changes?"
            },
            seeds,
            seed_search: None,
            top_k: 8,
            maps: Some(&maps),
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: None,
        };
        run(limits, &Dirs::under(self.dir.path()), &call)
            .await
            .unwrap()
            .response
    }
}

fn stage_limits(exact_seeds: bool, answer_seeds: bool) -> TraverseConfig {
    TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        exact_seeds,
        answer_seeds,
        ..limits()
    }
}

fn listed(response: &Value) -> Vec<(String, String)> {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|card| {
            (
                card["node"].as_str().unwrap().to_owned(),
                card["origin"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn exact_closure_answers_a_question_that_starts_no_walk() {
    let tree = ScipTree::new();
    let maps = ["graph.scip", EXACT_CLOSURE_ID];
    let response = tree
        .run(&stage_limits(false, false), &maps, &[], None)
        .await;
    // Nothing the walk can start from: the first stage is the answer, under
    // `totals` as on every response.
    assert_eq!(response["status"], "no_seeds", "{response}");
    assert_eq!(response["totals"]["first_stage"]["exact"], 3, "{response}");
    assert_eq!(response["totals"]["first_stage"]["walk"], 0, "{response}");
    assert!(response.get("first_stage").is_none(), "{response}");
    let results = response["results"].as_array().unwrap();
    // beta holds two references, then alpha and Holder one each.
    assert_eq!(results[0]["site"]["path"], "a.rs", "{response}");
    assert_eq!(results[0]["site"]["line"], 3, "{response}");
    assert_eq!(results[0]["held"], 2, "{response}");
    assert!(results.iter().all(|card| card["origin"] == "exact"));
    // Holder is not a node of the SCIP graph: its card keeps the site the
    // operator gave it (nothing else could locate it).
    let holder = results
        .iter()
        .find(|card| card["name"] == "Holder")
        .unwrap_or_else(|| panic!("no Holder card in {response}"));
    assert_eq!(holder["site"]["line"], 4, "{response}");
    assert_eq!(response["exact"][0]["answered"], true, "{response}");
    let notes = response["notes"].to_string();
    assert!(
        notes.contains("exact.closure: names 1, anchored yes"),
        "{notes}"
    );
}

#[tokio::test]
async fn exact_seeds_start_the_walk_from_the_exact_nodes_the_graph_holds() {
    let tree = ScipTree::new();
    let maps = ["graph.scip", EXACT_CLOSURE_ID];
    let response = tree.run(&stage_limits(true, false), &maps, &[], None).await;
    assert_ne!(response["status"], "no_seeds", "{response}");
    assert_eq!(response["seed_source"], "exact", "{response}");
    // alpha and beta, not Holder (the graph holds functions only).
    assert_eq!(response["seeds"].as_array().unwrap().len(), 2, "{response}");
    let hits = response["seed_hits"].as_array().unwrap();
    assert!(hits.iter().all(|hit| hit["how"] == "exact"), "{response}");
    // The exact cards lead, each node once, and the walk's cards come
    // after them; a card's site is where it is defined.
    let cards = listed(&response);
    assert!(
        cards[..3].iter().all(|(_, origin)| origin == "exact"),
        "{cards:?}"
    );
    assert!(
        cards[3..].iter().all(|(_, origin)| origin == "walk"),
        "{cards:?}"
    );
    let mut nodes: Vec<&String> = cards.iter().map(|(node, _)| node).collect();
    nodes.sort();
    nodes.dedup();
    assert_eq!(nodes.len(), cards.len(), "{cards:?}");
    let first = &response["results"][0];
    assert_eq!(first["site"]["line"], 3, "{response}");
    assert_eq!(first["site"]["col"], 4, "{response}");
}

#[tokio::test]
async fn a_seed_the_exact_stage_also_lists_is_listed_once_and_not_started_twice() {
    let tree = ScipTree::new();
    let maps = ["graph.scip", EXACT_CLOSURE_ID];
    let first = tree
        .run(&stage_limits(false, false), &maps, &[], None)
        .await;
    let beta = first["results"][0]["node"].as_str().unwrap().to_owned();
    // The given seed is also the first exact card: with answer_seeds it is
    // listed once, as the exact card (the first stage lists its own first).
    let response = tree
        .run(
            &stage_limits(true, true),
            &maps,
            std::slice::from_ref(&beta),
            None,
        )
        .await;
    let cards = listed(&response);
    assert_eq!(
        cards.iter().filter(|(node, _)| *node == beta).count(),
        1,
        "{cards:?}"
    );
    assert_eq!(cards[0], (beta.clone(), "exact".to_owned()), "{cards:?}");
    assert_eq!(response["totals"]["first_stage"]["seeds"], 0, "{response}");
    // exact_seeds adds alpha and leaves beta the one start node it was.
    let seeds = response["seeds"].as_array().unwrap();
    assert_eq!(seeds.len(), 2, "{response}");
    assert_eq!(seeds[0], beta.as_str(), "{response}");
}

#[tokio::test]
async fn exact_pickaxe_answers_with_commits_and_counts_its_git_calls() {
    let tree = ScipTree::new();
    tree.commit();
    let maps = ["graph.scip", EXACT_PICKAXE_ID];
    let limits = stage_limits(false, false);
    let response = tree.run(&limits, &maps, &[], None).await;
    let results = response["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{response}");
    assert!(
        results[0]["node"].as_str().unwrap().starts_with("commit:"),
        "{response}"
    );
    assert_eq!(results[0]["origin"], "exact", "{response}");
    let git_calls = response["exact"][0]["git_calls"].as_u64().unwrap();
    assert!(git_calls >= 1, "{response}");
    assert_eq!(
        response["totals"]["physical_external_calls"], git_calls,
        "{response}"
    );
    let notes = response["notes"].to_string();
    assert!(
        notes.contains("exact.pickaxe: class H1, 1 commits"),
        "{notes}"
    );

    // With a walk, the commit leads the results and the git calls add to
    // the totals the walk reports.
    let closure = ["graph.scip", EXACT_CLOSURE_ID];
    let first = tree
        .run(&stage_limits(false, false), &closure, &[], None)
        .await;
    let beta = first["results"][0]["node"].as_str().unwrap().to_owned();
    let walked = tree
        .run(&limits, &maps, std::slice::from_ref(&beta), None)
        .await;
    let cards = listed(&walked);
    assert!(cards[0].0.starts_with("commit:"), "{cards:?}");
    assert_eq!(cards[0].1, "exact");
    assert_eq!(walked["totals"]["first_stage"]["exact"], 1, "{walked}");
    assert!(
        walked["totals"]["physical_external_calls"]
            .as_u64()
            .unwrap()
            >= git_calls,
        "{walked}"
    );
}

/// The pickaxe is driven here through the same `BudgetedGit` the run builds
/// (`end` is the call's deadline), with the clock set by hand: a run-level
/// `deadline_ms` of 1 would depend on a millisecond passing before the first
/// git call, which a fast machine does not guarantee. The run-level wiring of
/// `git_calls` into the response is covered by
/// `exact_pickaxe_answers_with_commits_and_counts_its_git_calls`.
#[test]
fn exact_pickaxe_stops_asking_git_when_the_deadline_has_passed() {
    let tree = ScipTree::new();
    tree.commit();
    let question = "Which commit first created `Foo`?";
    let budgeted = |end: Instant| BudgetedGit {
        git: PathBuf::from("git"),
        tree: tree.dir.path().to_path_buf(),
        per_call: Duration::from_secs(20),
        end,
    };

    // Control: with time left, the same repository gives the commit.
    let open = pickaxe_cards(
        &budgeted(Instant::now() + Duration::from_secs(60)),
        question,
        8,
    );
    assert!(
        open.git_calls >= 1 && !open.cards.is_empty(),
        "{}",
        open.note
    );

    // The deadline is already behind us: no git runs, nothing is found, and
    // the note says why. (If the clock cannot go back a second, `now` is
    // itself never ahead of the later reading, so the budget is spent too.)
    let spent = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let late = pickaxe_cards(&budgeted(spent), question, 8);
    assert_eq!(late.git_calls, 0, "{}", late.note);
    assert!(late.cards.is_empty(), "{}", late.note);
    assert!(
        late.note
            .contains("stopped: the first stage's time ran out"),
        "{}",
        late.note
    );
}

/// A hybrid search that answers after a delay, as one over a loaded
/// embedder does: the hit is the lines of `alpha` in `a.rs`.
struct SlowSearch(Duration);

#[lci_maps::async_trait]
impl SeedSearch for SlowSearch {
    async fn search(&self, _question: &str, _k: usize) -> Result<Vec<SeedHit>> {
        tokio::time::sleep(self.0).await;
        Ok(vec![SeedHit {
            path: "a.rs".into(),
            start_line: 2,
            end_line: 2,
            content_hash: "abc123".into(),
            ordinal: None,
            rank: 1,
            retrievers: vec!["semantic".into()],
            semantic_rank: Some(1),
            lexical_rank: None,
            lsp_rank: None,
            fusion_score: 0.25,
            reranker_score: None,
        }])
    }
}

#[tokio::test]
async fn a_hybrid_search_slower_than_the_deadline_does_not_starve_the_walk() {
    let tree = ScipTree::new();
    // The search takes longer than the whole deadline; the walk's own
    // deadline starts when the search returns.
    let search = SlowSearch(Duration::from_millis(600));
    let (budget, steer) = (
        BudgetArgs {
            pops: Some(3),
            deadline_ms: Some(400),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "What breaks if `Foo` changes?",
        seeds: &[],
        seed_search: Some(&search),
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let response = run(&limits(), &Dirs::under(tree.dir.path()), &call)
        .await
        .unwrap()
        .response;
    assert_eq!(response["seed_source"], "hybrid", "{response}");
    assert!(
        response["totals"]["pops"].as_u64().unwrap() >= 1,
        "the walk popped nothing: {response}"
    );
    assert_ne!(response["stop_reason"], "deadline", "{response}");
}

#[tokio::test]
async fn the_time_of_the_hybrid_search_is_kept_apart_for_the_deadline() {
    let search = SlowSearch(Duration::from_millis(50));
    let found = seeds_for(Some(&search), "alpha", &[], &MAPS).await;
    assert!(found.search_time >= Duration::from_millis(50));
    // Given seeds and the stand-in alone run no search.
    let id = "sym:rust:search::search/fn";
    let given = seeds_for(Some(&search), "q", &[id.to_owned()], &MAPS).await;
    assert_eq!(given.search_time, Duration::ZERO);
    let names = seeds_for(Some(&search), "rank hits", &[], &[SEEDS_MAP]).await;
    assert_eq!(names.search_time, Duration::ZERO);
}

#[tokio::test]
async fn lsp_live_starts_the_servers_the_call_configures() {
    let tree = tempfile::tempdir().unwrap();
    let index = tree.path().join("index.scip");
    std::fs::write(&index, []).unwrap();
    let lsp = lci_core::config::Config {
        rust_analyzer_path: "lci-no-such-language-server".into(),
        ..Default::default()
    };
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let maps = [LSP_LIVE_ID.to_owned()];
    let call = Call {
        scip_index: &index,
        scip_root: Some(tree.path()),
        question: "q",
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: Some(&lsp),
    };
    let dirs = Dirs::under(tree.path());
    let report = run(&limits(), &dirs, &call).await.unwrap();
    let notes = report.response["notes"].to_string();
    assert!(notes.contains("lsp.live is unavailable"), "{notes}");
}

#[tokio::test]
async fn a_tie_break_file_that_cannot_be_read_is_an_error() {
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: Path::new("unused"),
        scip_root: None,
        question: "q",
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let limits = TraverseConfig {
        tie_break: Some("no-such-tie-break.json".into()),
        ..limits()
    };
    let dirs = Dirs::under(Path::new("unused"));
    let error = run(&limits, &dirs, &call).await.err().expect("an error");
    let error = error.to_string();
    assert!(
        error.contains("tie_break no-such-tie-break.json"),
        "{error}"
    );
}

/// A tree whose `a.rs` has `target` and `users` functions that each hold
/// one reference to it, with a SCIP index over it.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn crowded_tree(users: u32) -> (tempfile::TempDir, PathBuf) {
    use scip::types::{Document, Index, Occurrence};
    let dir = tempfile::tempdir().unwrap();
    let source: String = (0..=users).map(|_| "fn f() { f(); }\n").collect();
    std::fs::write(dir.path().join("a.rs"), source).unwrap();
    let sym = |name: &str| format!("rust-analyzer cargo demo 0.1.0 a/{name}().");
    let occurrence = |range: &[i32], symbol: &str, roles: i32, enclosing: &[i32]| {
        let mut o = Occurrence::default();
        o.range = range.to_vec();
        o.symbol = symbol.to_owned();
        o.symbol_roles = roles;
        o.enclosing_range = enclosing.to_vec();
        o
    };
    let mut document = Document::default();
    document.relative_path = "a.rs".to_owned();
    document.occurrences = vec![occurrence(&[0, 3, 9], &sym("target"), 1, &[0, 0, 0, 15])];
    for i in 1..=users as i32 {
        document.occurrences.push(occurrence(
            &[i, 3, 8],
            &sym(&format!("user{i}")),
            1,
            &[i, 0, i, 15],
        ));
        document
            .occurrences
            .push(occurrence(&[i, 9, 15], &sym("target"), 0, &[]));
    }
    // The impact map anchors at declarations, which need a kind.
    document.symbols = std::iter::once("target".to_owned())
        .chain((1..=users).map(|i| format!("user{i}")))
        .map(|name| {
            let mut info = scip::types::SymbolInformation::default();
            info.symbol = sym(&name);
            info.kind = scip::types::symbol_information::Kind::Function.into();
            info
        })
        .collect();
    let mut message = Index::default();
    message.documents = vec![document];
    let index = dir.path().join("index.scip");
    scip::write_message_to_file(&index, message).unwrap();
    (dir, index)
}

#[tokio::test]
async fn the_beam_decides_how_many_of_a_nodes_edges_the_walk_follows() {
    let (dir, index) = crowded_tree(21);
    let loaded = load(&index, Some(dir.path())).await.unwrap();
    let target = loaded
        .scip
        .node_id(
            loaded
                .scip
                .graph()
                .node_index("rust-analyzer cargo demo 0.1.0 a/target().")
                .expect("target is a graph node"),
        )
        .to_string();
    let maps = [SCIP_IMPACT_ID.to_owned()];
    let (budget, steer) = (
        BudgetArgs {
            max_depth: Some(1),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let hops = |beam: usize| {
        let (target, maps, budget, steer, index, dir) =
            (&target, &maps, &budget, &steer, &index, &dir);
        async move {
            let call = Call {
                scip_index: index,
                scip_root: Some(dir.path()),
                question: "q",
                seeds: std::slice::from_ref(target),
                seed_search: None,
                top_k: 40,
                maps: Some(maps),
                budget,
                steer,
                handle: None,
                lsp: None,
            };
            let limits = TraverseConfig { beam, ..limits() };
            let response = run(&limits, &Dirs::under(dir.path()), &call)
                .await
                .unwrap()
                .response;
            response["totals"]["hops"].as_u64().unwrap()
        }
    };
    // 21 users offered: the default beam of 16 drops five, 64 keeps all.
    assert_eq!(TraverseConfig::default().beam, 16);
    assert_eq!(hops(TraverseConfig::default().beam).await, 16);
    assert_eq!(hops(64).await, 21);
}

// ---- trait paths in the question seed the scip.implements walk ----------

/// A tree whose index holds the trait `Tr` (method `m`) in `a.rs`, the types
/// `A` and `B`, and `impl Tr for A`, `impl Tr for B`.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn trait_tree() -> (tempfile::TempDir, PathBuf) {
    use scip::types::{Document, Occurrence, SymbolInformation, symbol_information::Kind};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "pub trait Tr {\n    fn m(&self);\n}\npub struct A;\npub struct B;\nimpl Tr for A {\n    fn m(&self) {}\n}\nimpl Tr for B {\n    fn m(&self) {}\n}\n",
    )
    .unwrap();
    let symbol = |path: &str| format!("rust-analyzer cargo demo 0.1.0 {path}");
    let occurrence = |range: &[i32], path: &str, roles: i32, enclosing: &[i32]| {
        let mut o = Occurrence::default();
        o.range = range.to_vec();
        o.symbol = symbol(path);
        o.symbol_roles = roles;
        o.enclosing_range = enclosing.to_vec();
        o
    };
    let info = |path: &str, kind: Kind| {
        let mut i = SymbolInformation::default();
        i.symbol = symbol(path);
        i.kind = kind.into();
        i
    };
    let mut document = Document::default();
    document.relative_path = "a.rs".to_owned();
    document.occurrences = vec![
        occurrence(&[0, 10, 12], "a/Tr#", 1, &[0, 0, 2, 1]),
        occurrence(&[1, 7, 8], "a/Tr#m().", 1, &[1, 4, 1, 16]),
        occurrence(&[3, 11, 12], "a/A#", 1, &[3, 0, 3, 13]),
        occurrence(&[4, 11, 12], "a/B#", 1, &[4, 0, 4, 13]),
        occurrence(&[5, 5, 7], "a/Tr#", 0, &[]),
        occurrence(&[5, 12, 13], "a/A#", 0, &[]),
        occurrence(&[6, 7, 8], "a/impl#[A][Tr]m().", 1, &[6, 4, 6, 18]),
        occurrence(&[8, 5, 7], "a/Tr#", 0, &[]),
        occurrence(&[8, 12, 13], "a/B#", 0, &[]),
        occurrence(&[9, 7, 8], "a/impl#[B][Tr]m().", 1, &[9, 4, 9, 18]),
    ];
    document.symbols = vec![
        info("a/Tr#", Kind::Trait),
        info("a/Tr#m().", Kind::Method),
        info("a/A#", Kind::Struct),
        info("a/B#", Kind::Struct),
        info("a/impl#[A][Tr]m().", Kind::Method),
        info("a/impl#[B][Tr]m().", Kind::Method),
    ];
    let mut message = scip::types::Index::default();
    message.documents = vec![document];
    let index = dir.path().join("index.scip");
    scip::write_message_to_file(&index, message).unwrap();
    (dir, index)
}

/// Like [`trait_tree`], with `n` types `S00..` that each implement `Tr`.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn many_impls_tree(n: usize) -> (tempfile::TempDir, PathBuf) {
    use scip::types::{Document, Occurrence, SymbolInformation, symbol_information::Kind};
    let dir = tempfile::tempdir().unwrap();
    let mut text = String::from("pub trait Tr {\n    fn m(&self);\n}\n");
    for i in 0..n {
        text.push_str(&format!(
            "pub struct S{i:02};\nimpl Tr for S{i:02} {{\n    fn m(&self) {{}}\n}}\n"
        ));
    }
    std::fs::write(dir.path().join("a.rs"), text).unwrap();
    let symbol = |path: &str| format!("rust-analyzer cargo demo 0.1.0 {path}");
    let occurrence = |range: &[i32], path: &str, roles: i32, enclosing: &[i32]| {
        let mut o = Occurrence::default();
        o.range = range.to_vec();
        o.symbol = symbol(path);
        o.symbol_roles = roles;
        o.enclosing_range = enclosing.to_vec();
        o
    };
    let info = |path: &str, kind: Kind| {
        let mut i = SymbolInformation::default();
        i.symbol = symbol(path);
        i.kind = kind.into();
        i
    };
    let mut document = Document::default();
    document.relative_path = "a.rs".to_owned();
    document.occurrences = vec![
        occurrence(&[0, 10, 12], "a/Tr#", 1, &[0, 0, 2, 1]),
        occurrence(&[1, 7, 8], "a/Tr#m().", 1, &[1, 4, 1, 16]),
    ];
    document.symbols = vec![info("a/Tr#", Kind::Trait), info("a/Tr#m().", Kind::Method)];
    for i in 0..n {
        let line = 3 + 4 * i as i32;
        let ty = format!("a/S{i:02}#");
        let method = format!("a/impl#[S{i:02}][Tr]m().");
        document.occurrences.extend([
            occurrence(&[line, 11, 14], &ty, 1, &[line, 0, line, 15]),
            occurrence(&[line + 1, 5, 7], "a/Tr#", 0, &[]),
            occurrence(&[line + 1, 12, 15], &ty, 0, &[]),
            occurrence(&[line + 2, 7, 8], &method, 1, &[line + 2, 4, line + 2, 18]),
        ]);
        document.symbols.push(info(&ty, Kind::Struct));
        document.symbols.push(info(&method, Kind::Method));
    }
    let mut message = scip::types::Index::default();
    message.documents = vec![document];
    let index = dir.path().join("index.scip");
    scip::write_message_to_file(&index, message).unwrap();
    (dir, index)
}

/// One call over the trait tree.
async fn trait_run(
    tree: &(tempfile::TempDir, PathBuf),
    question: &str,
    seeds: &[String],
    maps: Option<&[&str]>,
) -> Value {
    trait_run_mode(tree, question, seeds, maps, TraitSeedMode::Auto).await
}

async fn trait_run_mode(
    tree: &(tempfile::TempDir, PathBuf),
    question: &str,
    seeds: &[String],
    maps: Option<&[&str]>,
    mode: TraitSeedMode,
) -> Value {
    trait_run_top(tree, question, seeds, maps, mode, 8).await
}

async fn trait_run_top(
    tree: &(tempfile::TempDir, PathBuf),
    question: &str,
    seeds: &[String],
    maps: Option<&[&str]>,
    mode: TraitSeedMode,
    top_k: usize,
) -> Value {
    let maps: Option<Vec<String>> = maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &tree.1,
        scip_root: Some(tree.0.path()),
        question,
        seeds,
        seed_search: None,
        top_k,
        maps: maps.as_deref(),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    run_with_trait_seeds(&limits(), &Dirs::under(tree.0.path()), &call, mode)
        .await
        .unwrap()
        .response
}

fn reached(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results in {response}"))
        .iter()
        .map(|card| card["node"].as_str().unwrap().to_owned())
        .collect()
}

const WITH_IMPLEMENTS: [&str; 2] = ["graph.scip", "scip.implements"];

// The ids as `scip.impact` (trait, types) and `graph.scip` (methods) make them
// from the file `a.rs`; `graph.scip` keeps no container, so the declaration
// and the two members share a name and are told apart by the `~n` counter.
const TRAIT_ID: &str = "sym:rust:a::Tr/trait";
const DECL_ID: &str = "sym:rust:a::m/method";
const A_ID: &str = "sym:rust:a::A/struct";
const B_ID: &str = "sym:rust:a::B/struct";

#[tokio::test]
async fn a_trait_path_in_the_question_starts_the_walk_at_the_trait() {
    let tree = trait_tree();
    let response = trait_run(
        &tree,
        "Which types implement `demo::Tr`?",
        &[],
        Some(&WITH_IMPLEMENTS),
    )
    .await;
    assert_eq!(response["seeds"], json!([TRAIT_ID]), "{response}");
    assert_eq!(response["seed_source"], "traits", "{response}");
    assert_eq!(response["trait_seeds"], 1, "{response}");
    assert_eq!(response["seed_hits"][0]["how"], "trait", "{response}");
    assert_eq!(
        response["seed_hits"][0]["written"], "demo::Tr",
        "{response}"
    );
    // One hop over the Implements edges reaches both implementing types.
    let nodes = reached(&response);
    assert!(
        nodes.contains(&A_ID.to_owned()) && nodes.contains(&B_ID.to_owned()),
        "{response}"
    );
    assert!(
        notes_of(&response).contains("1 start nodes are the traits and trait methods"),
        "{response}"
    );
    // The module path alone (`a::Tr`) and `crate::` in front name it too.
    for question in [
        "Which types implement `a::Tr`?",
        "implementors of `crate::a::Tr`",
    ] {
        let again = trait_run(&tree, question, &[], Some(&WITH_IMPLEMENTS)).await;
        assert_eq!(again["seeds"], json!([TRAIT_ID]), "{question}: {again}");
    }
}

// A trait with more implementors than the beam (16) keeps them all when the
// question names it: the trait is a trait seed, whose `scip.implements` edges
// are not cut.
#[tokio::test]
async fn a_trait_seed_reaches_every_implementor_past_the_beam() {
    let tree = many_impls_tree(20);
    let response = trait_run_top(
        &tree,
        "Which types implement `demo::Tr`?",
        &[],
        Some(&WITH_IMPLEMENTS),
        TraitSeedMode::Auto,
        40,
    )
    .await;
    assert_eq!(response["trait_seeds"], 1, "{response}");
    let types: Vec<String> = reached(&response)
        .into_iter()
        .filter(|id| id.ends_with("/struct"))
        .collect();
    assert_eq!(types.len(), 20, "{response}");
    // A walk from a given seed is not a trait seed: the beam still cuts it.
    let given = trait_run_top(
        &tree,
        "q",
        &[TRAIT_ID.to_owned()],
        Some(&WITH_IMPLEMENTS),
        TraitSeedMode::Auto,
        40,
    )
    .await;
    let cut = reached(&given)
        .into_iter()
        .filter(|id| id.ends_with("/struct"))
        .count();
    assert_eq!(cut, 16, "{given}");
}

// Without `scip.impact` among the maps the implementing types still get a
// site: a type is no node of the SCIP graph, and a result nothing locates is
// dropped by a consumer that needs a place (the trait bench scored Q1 as 0).
#[tokio::test]
async fn the_types_scip_implements_reaches_are_located_without_scip_impact() {
    let tree = trait_tree();
    let response = trait_run(
        &tree,
        "Which types implement `demo::Tr`?",
        &[],
        Some(&WITH_IMPLEMENTS),
    )
    .await;
    let results = response["results"].as_array().expect("results");
    for (id, line) in [(A_ID, 4), (B_ID, 5)] {
        let card = results
            .iter()
            .find(|card| card["node"] == id)
            .unwrap_or_else(|| panic!("{id} not in {response}"));
        assert_eq!(card["site"]["path"], "a.rs", "{id}: {response}");
        assert_eq!(card["site"]["line"], line, "{id}: {response}");
    }
}

#[tokio::test]
async fn a_trait_method_path_starts_the_walk_at_the_declaration() {
    let tree = trait_tree();
    let response = trait_run(
        &tree,
        "What must change if `Tr::m` changes?",
        &[],
        Some(&WITH_IMPLEMENTS),
    )
    .await;
    assert_eq!(response["seeds"], json!([DECL_ID]), "{response}");
    assert_eq!(
        response["seed_hits"][0]["how"], "trait_method",
        "{response}"
    );
    let members = reached(&response);
    assert_eq!(members.len(), 2, "{response}");
    for member in ["1", "2"] {
        let id = format!("sym:rust:a::m/method~{member}");
        assert!(members.contains(&id), "{id} not in {response}");
    }
}

#[tokio::test]
async fn the_ids_the_implements_map_emits_are_accepted_as_seeds() {
    let tree = trait_tree();
    let walk = trait_run(
        &tree,
        "Which types implement `demo::Tr`?",
        &[],
        Some(&WITH_IMPLEMENTS),
    )
    .await;
    let methods = trait_run(
        &tree,
        "What must change if `Tr::m` changes?",
        &[],
        Some(&WITH_IMPLEMENTS),
    )
    .await;
    // Every id the map reached (a trait's types, a declaration's members) and
    // the two it starts from parses, is canonical, and starts a walk.
    let mut ids = reached(&walk);
    ids.extend(reached(&methods));
    ids.push(TRAIT_ID.to_owned());
    ids.push(DECL_ID.to_owned());
    assert!(ids.len() == 6, "{ids:?}");
    for id in ids {
        let parsed = id.parse::<NodeId>().unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(parsed.to_string(), id);
        let given = trait_run(
            &tree,
            "q",
            std::slice::from_ref(&id),
            Some(&WITH_IMPLEMENTS),
        )
        .await;
        assert_eq!(given["seeds"], json!([id]), "{given}");
        assert_eq!(given["seed_source"], "given", "{given}");
    }
    // A type seed walks back to its trait.
    let from_type = trait_run(&tree, "q", &[A_ID.to_owned()], Some(&WITH_IMPLEMENTS)).await;
    assert!(
        reached(&from_type).contains(&TRAIT_ID.to_owned()),
        "{from_type}"
    );
}

#[tokio::test]
async fn given_seeds_win_over_a_trait_path_in_the_question() {
    let tree = trait_tree();
    let response = trait_run(
        &tree,
        "Which types implement `demo::Tr`?",
        &[A_ID.to_owned()],
        Some(&WITH_IMPLEMENTS),
    )
    .await;
    assert_eq!(response["seeds"], json!([A_ID]), "{response}");
    assert_eq!(response["seed_source"], "given", "{response}");
}

#[tokio::test]
async fn without_scip_implements_a_trait_path_changes_nothing() {
    let tree = trait_tree();
    let question = "Which types implement `demo::Tr`?";
    // The default maps and the stand-in alone, with the question's path.
    let stand_in = ["graph.scip", SEEDS_MAP];
    let loaded = load(&tree.1, Some(tree.0.path())).await.unwrap();
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &tree.1,
        scip_root: Some(tree.0.path()),
        question,
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let plain = find_seeds(&limits(), &call, &loaded, &[SEEDS_MAP, SCIP_MAP_ID])
        .await
        .unwrap();
    let expected: Vec<String> = plain.nodes.iter().map(|s| s.node.to_string()).collect();
    for maps in [None, Some(&stand_in[..])] {
        let response = trait_run(&tree, question, &[], maps).await;
        assert_eq!(response["seeds"], json!(expected), "{response}");
        assert_ne!(response["seed_source"], "traits", "{response}");
        assert!(
            !notes_of(&response).contains("the traits and trait methods"),
            "{response}"
        );
        let nodes = reached(&response);
        assert!(
            !nodes.contains(&TRAIT_ID.to_owned()) && !nodes.contains(&A_ID.to_owned()),
            "{response}"
        );
        // No trait record, no field of the opt-in map, no map of it.
        for hit in response["seed_hits"].as_array().into_iter().flatten() {
            assert!(
                hit["how"] != "trait" && hit["how"] != "trait_method",
                "{response}"
            );
        }
        assert!(response.get("trait_seeds").is_none(), "{response}");
        let ids: Vec<&Value> = response["effective_policy"]["maps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|map| &map["id"])
            .collect();
        assert!(!ids.contains(&&json!("scip.implements")), "{response}");
    }
    // With the map on, a question that names no trait seeds exactly as before.
    for question in [
        "Which types implement Tr?",
        "Which types implement `demo::Nope`?",
        "What does `Tr` do?",
    ] {
        let base = trait_run(&tree, question, &[], Some(&stand_in)).await;
        let with = trait_run(
            &tree,
            question,
            &[],
            Some(&["graph.scip", SEEDS_MAP, "scip.implements"]),
        )
        .await;
        assert_eq!(with["seeds"], base["seeds"], "{question}");
        assert_eq!(with["seed_source"], base["seed_source"], "{question}");
        assert_eq!(with["seed_hits"], base["seed_hits"], "{question}");
        // The map is on and nothing was named: the count says 0.
        assert_eq!(with["trait_seeds"], 0, "{question}");
    }
}

fn seed_ids(response: &Value) -> Vec<String> {
    response["seeds"]
        .as_array()
        .unwrap_or_else(|| panic!("no seeds in {response}"))
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect()
}

/// The stand-in seeds (the question's words match method names) and a trait
/// path both seed the call: each mode orders them its own way.
#[tokio::test]
async fn trait_seeds_and_the_stand_in_seeds_combine_per_mode() {
    let tree = trait_tree();
    let words = ["graph.scip", SEEDS_MAP, "scip.implements"];
    let stand_in = ["graph.scip", SEEDS_MAP];
    // An exact path (`demo::Tr`) and a re-exported one (`other::Tr`).
    for (question, exact) in [
        ("Which types A and B implement `demo::Tr`?", true),
        ("Which types A and B implement `other::Tr`?", false),
    ] {
        let plain = trait_run(&tree, question, &[], Some(&stand_in)).await;
        let others: Vec<String> = seed_ids(&plain)
            .into_iter()
            .filter(|id| id != TRAIT_ID)
            .collect();
        // The words match the method nodes `graph.scip` holds (the members of A, B).
        assert!(others.len() >= 2, "{plain}");
        assert_eq!(plain["seed_source"], "name_tokens", "{plain}");
        assert!(plain.get("trait_seeds").is_none(), "{plain}");
        let mut prepended = vec![TRAIT_ID.to_owned()];
        prepended.extend(others.clone());
        for (mode, order, source) in [
            (
                TraitSeedMode::Auto,
                if exact {
                    vec![TRAIT_ID.to_owned()]
                } else {
                    prepended.clone()
                },
                if exact {
                    "traits"
                } else {
                    "traits+name_tokens"
                },
            ),
            (TraitSeedMode::Replace, vec![TRAIT_ID.to_owned()], "traits"),
            (
                TraitSeedMode::Prepend,
                prepended.clone(),
                "traits+name_tokens",
            ),
        ] {
            let response = trait_run_mode(&tree, question, &[], Some(&words), mode).await;
            let label = format!("{question} {mode:?}: {response}");
            assert_eq!(seed_ids(&response), order, "{label}");
            assert_eq!(response["seed_source"], source, "{label}");
            // The trait is there once, and the count says how many are traits.
            let ids = seed_ids(&response);
            let unique: HashSet<&String> = ids.iter().collect();
            assert_eq!(unique.len(), ids.len(), "{label}");
            assert_eq!(response["trait_seeds"], 1, "{label}");
            // One record per start node, in the same order; the trait's first.
            let hits = response["seed_hits"].as_array().unwrap();
            assert_eq!(hits.len(), ids.len(), "{label}");
            for (hit, id) in hits.iter().zip(&ids) {
                assert_eq!(hit["node"], *id, "{label}");
            }
            assert_eq!(hits[0]["how"], "trait", "{label}");
            assert_eq!(hits[0]["score"], u32::from(exact), "{label}");
            for hit in &hits[1..] {
                assert_eq!(hit["how"], "name", "{label}");
            }
        }
    }
}
