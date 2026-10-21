//! Tests of the traversal entry point and of the pieces it is built from.

use super::rescore::{RERANK_TEXT_CHARS, classify, relevance};
use super::seeds::map_hits;
use super::*;
use crate::models::Models;
use lci_core::{
    config::{TraversePolicy, TraverseRerankScores, profile::ScipMode},
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
        lsp_sessions: None,
    };
    let limits = TraverseConfig {
        rerank_frontier: true,
        ..limits()
    };
    let error = run(
        &limits,
        &RunProfileConfig::default(),
        &Dirs::under(tree.path()),
        &call,
    )
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
            lsp_sessions: None,
        };
        Ok(run(
            limits,
            &RunProfileConfig::default(),
            &Dirs::under(self.dir.path()),
            &call,
        )
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
        lsp_sessions: None,
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
        lsp_sessions: None,
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
                lsp_sessions: None,
            };
            run(&limits(), &RunProfileConfig::default(), &dirs, &call)
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
        lsp_sessions: None,
    };
    run(
        limits,
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
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
        lsp_sessions: None,
    };
    run(
        limits,
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
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
        lsp_sessions: None,
    };
    let response = run(
        &limits,
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
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
        lsp_sessions: None,
    };
    let error = run(
        &bad,
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
    .await
    .map(|_| ())
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("policy flow") && error.contains("nope.map"),
        "{error}"
    );
}

/// One call over the three-file tree with the given steer and question;
/// `rooted` false is a call without a tree.
async fn steered_run(
    tree: &Tree,
    limits: &TraverseConfig,
    question: &str,
    steer: &SteerArgs,
    rooted: bool,
) -> Value {
    let budget = BudgetArgs::default();
    let call = Call {
        scip_index: &tree.index,
        scip_root: rooted.then(|| tree.dir.path()),
        question,
        seeds: &tree.seeds,
        seed_search: None,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    run(
        limits,
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
    .await
    .unwrap()
    .response
}

#[tokio::test]
async fn a_policy_table_routes_an_intent_to_grep_without_naming_maps() {
    let tree = Tree::new();
    let mut routed = limits();
    routed.policies.insert(
        "flow".into(),
        TraversePolicy {
            extra_maps: Some(vec![GREP_LIVE_MAP_ID.to_owned()]),
            ..TraversePolicy::default()
        },
    );
    // The intent alone brings grep.live in, beside the default maps.
    let flow = policy_run(&tree, &routed, "flow", 8, None).await;
    assert_eq!(flow["effective_policy"]["preset"], "flow", "{flow}");
    assert!(policy_has_map(&flow, GREP_LIVE_MAP_ID), "{flow}");
    let grep = flow["effective_policy"]["maps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|map| map["id"] == GREP_LIVE_MAP_ID)
        .unwrap();
    assert_eq!(grep["enabled"], true, "{flow}");
    for id in MAPS {
        assert!(policy_has_map(&flow, id), "{id} {flow}");
    }
    // Another intent has no table entry: grep.live is not run.
    let explore = policy_run(&tree, &routed, "explore", 8, None).await;
    assert!(!policy_has_map(&explore, GREP_LIVE_MAP_ID), "{explore}");
    // Without a tree the map cannot run and the call says so.
    let steer = SteerArgs {
        policy: Some("flow".to_owned()),
        ..SteerArgs::default()
    };
    let rootless = steered_run(&tree, &routed, "q", &steer, false).await;
    assert!(!policy_has_map(&rootless, GREP_LIVE_MAP_ID), "{rootless}");
    let notes = notes_of(&rootless);
    assert!(
        notes.contains(GREP_LIVE_MAP_ID) && notes.contains("left out") && notes.contains("--root"),
        "{notes}"
    );
}

// Gold is not an input of `Call` at all (labels live only in xtask), so the gold half holds by type;
// this test pins the text half: wording of every intent still resolves to `explore`.
#[tokio::test]
async fn the_question_text_and_the_gold_never_pick_a_policy() {
    let tree = Tree::new();
    let limits = TraverseConfig {
        first_stage: TraverseFirstStage::Append,
        answer_seeds: true,
        ..limits()
    };
    let wording = "Where should I register a new command? what breaks if I change search? \
                   how does flow get from a to b";
    let none = SteerArgs::default();
    let asked = steered_run(&tree, &limits, wording, &none, true).await;
    let plain = steered_run(&tree, &limits, "q", &none, true).await;
    // Every intent's wording, and still the default policy.
    assert_eq!(asked["effective_policy"]["preset"], "explore", "{asked}");
    assert_eq!(asked["results"], plain["results"]);
    assert_eq!(asked["totals"]["pops"], plain["totals"]["pops"]);
    assert_eq!(
        asked["effective_policy"]["hash"],
        plain["effective_policy"]["hash"]
    );
    // Only the caller's name picks one.
    let named = SteerArgs {
        policy: Some("where-to".to_owned()),
        ..SteerArgs::default()
    };
    let routed = steered_run(&tree, &limits, wording, &named, true).await;
    assert_eq!(routed["effective_policy"]["preset"], "where-to", "{routed}");
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
        lsp_sessions: None,
    };
    let next = run(
        &limits,
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
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
            lsp_sessions: None,
        };
        run(
            limits,
            &RunProfileConfig::default(),
            &Dirs::under(self.dir.path()),
            &call,
        )
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
        lsp_sessions: None,
    };
    let response = run(
        &limits(),
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
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
        lsp_sessions: None,
    };
    let dirs = Dirs::under(tree.path());
    let report = run(&limits(), &RunProfileConfig::default(), &dirs, &call)
        .await
        .unwrap();
    let notes = report.response["notes"].to_string();
    assert!(notes.contains("lsp.live is unavailable"), "{notes}");
}

/// Which language-server settings `lsp.live` opens under `[run_profile.lsp]
/// sessions`: the settings of the call's own (`per_call`, and `shared` when the
/// call has no manager) or the manager's (`shared`). Each names a server binary
/// that does not exist, and the note of the missing one says whose it was.
#[tokio::test]
async fn lsp_live_opens_the_call_settings_or_the_shared_manager_as_the_profile_says() {
    use lci_core::config::{LspProfile, LspSessions};
    let tree = tempfile::tempdir().unwrap();
    let index = tree.path().join("index.scip");
    std::fs::write(&index, []).unwrap();
    let missing = |name: &str| lci_core::config::Config {
        rust_analyzer_path: name.into(),
        ..Default::default()
    };
    let (own, shared) = (missing("lci-missing-own"), missing("lci-missing-shared"));
    let manager = Arc::new(lci_core::lsp::Manager::new(&shared));
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let maps = [LSP_LIVE_ID.to_owned()];
    let notes_of = |sessions: LspSessions, offered: Option<Arc<lci_core::lsp::Manager>>| {
        let (index, tree, budget, steer, maps, own) = (&index, &tree, &budget, &steer, &maps, &own);
        async move {
            let call = Call {
                scip_index: index,
                scip_root: Some(tree.path()),
                question: "q",
                seeds: &[],
                seed_search: None,
                top_k: 8,
                maps: Some(maps),
                budget,
                steer,
                handle: None,
                lsp: Some(own),
                lsp_sessions: offered,
            };
            let profile = RunProfileConfig {
                lsp: LspProfile {
                    sessions,
                    ..LspProfile::default()
                },
                ..RunProfileConfig::default()
            };
            let dirs = Dirs::under(tree.path());
            let report = run(&limits(), &profile, &dirs, &call).await.unwrap();
            report.response["notes"].to_string()
        }
    };
    let per_call = notes_of(LspSessions::PerCall, Some(manager.clone())).await;
    assert!(per_call.contains("lci-missing-own"), "{per_call}");
    assert!(!per_call.contains("lci-missing-shared"), "{per_call}");
    let shared_notes = notes_of(LspSessions::Shared, Some(manager.clone())).await;
    assert!(
        shared_notes.contains("lci-missing-shared"),
        "{shared_notes}"
    );
    assert!(!shared_notes.contains("lci-missing-own"), "{shared_notes}");
    let alone = notes_of(LspSessions::Shared, None).await;
    assert!(alone.contains("lci-missing-own"), "{alone}");
}

/// The manager a `per_call` walk builds from the call's settings never syncs,
/// even when those settings (the App's whole configuration, under the MCP and
/// REST server) say `sync = "watch"`: its servers start on the tree as it is
/// and stop with the walk.
#[test]
fn a_per_call_manager_never_syncs_whatever_the_call_settings_say() {
    use lci_core::config::LspSync;
    let mut watching = lci_core::config::Config::default();
    watching.run_profile.lsp.sync = LspSync::Watch;
    assert_eq!(
        lci_core::lsp::Manager::new(&watching).sync_mode(),
        LspSync::Watch,
        "the control: a manager of these settings syncs"
    );
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    for lsp in [Some(&watching), None] {
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
            lsp,
            lsp_sessions: None,
        };
        assert_eq!(per_call_manager(&call).sync_mode(), LspSync::Off);
    }
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
        lsp_sessions: None,
    };
    let limits = TraverseConfig {
        tie_break: Some("no-such-tie-break.json".into()),
        ..limits()
    };
    let dirs = Dirs::under(Path::new("unused"));
    let error = run(&limits, &RunProfileConfig::default(), &dirs, &call)
        .await
        .err()
        .expect("an error");
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
    let loaded = load(&index, Some(dir.path()), ScipMode::Authoritative)
        .await
        .unwrap();
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
                lsp_sessions: None,
            };
            let limits = TraverseConfig { beam, ..limits() };
            let response = run(
                &limits,
                &RunProfileConfig::default(),
                &Dirs::under(dir.path()),
                &call,
            )
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
        lsp_sessions: None,
    };
    run_with_trait_seeds(
        &limits(),
        &RunProfileConfig::default(),
        &Dirs::under(tree.0.path()),
        &call,
        mode,
    )
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
    // A given start node that is the trait is walked the same way, though the
    // question added no trait seed.
    let given = trait_run_top(
        &tree,
        "q",
        &[TRAIT_ID.to_owned()],
        Some(&WITH_IMPLEMENTS),
        TraitSeedMode::Auto,
        40,
    )
    .await;
    assert_eq!(given["trait_seeds"], 0, "{given}");
    let given_types = reached(&given)
        .into_iter()
        .filter(|id| id.ends_with("/struct"))
        .count();
    assert_eq!(given_types, 20, "{given}");
    // A given start node that is no trait is priced as before: the beam cuts
    // the walk at the trait one hop on.
    let plain = trait_run_top(
        &tree,
        "q",
        &["sym:rust:a::S00/struct".to_owned()],
        Some(&WITH_IMPLEMENTS),
        TraitSeedMode::Auto,
        40,
    )
    .await;
    let cut = reached(&plain)
        .into_iter()
        .filter(|id| id.ends_with("/struct"))
        .count();
    assert!(cut < 20, "{plain}");
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
    let loaded = load(&tree.1, Some(tree.0.path()), ScipMode::Authoritative)
        .await
        .unwrap();
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
        lsp_sessions: None,
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

/// The first event of a run's trace: the `start` event.
fn start_event(report: &Report) -> Value {
    let trace = report.trace.as_ref().expect("the trace was written");
    let text = std::fs::read_to_string(trace).unwrap();
    serde_json::from_str(text.lines().next().expect("a trace has events")).unwrap()
}

#[tokio::test]
async fn the_walk_reads_walk_groups_from_the_run_profile() {
    let tree = Tree::new();
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
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    let dirs = Dirs::under(tree.dir.path());
    // The default profile ranks as ever and says nothing about grouping.
    let plain = run(&limits(), &RunProfileConfig::default(), &dirs, &call)
        .await
        .unwrap();
    let rank = &start_event(&plain)["rank"];
    assert!(rank.get("groups").is_none(), "{rank}");
    assert!(rank.get("grep_corroboration").is_none(), "{rank}");
    assert!(!notes_of(&plain.response).contains("groups claim"));
    // The profile's walk keys reach the start event.
    let profile = RunProfileConfig {
        walk: lci_core::config::WalkProfile {
            groups: lci_core::config::WalkGroups::Claim,
            grep_corroboration: 0.5,
            ..lci_core::config::WalkProfile::default()
        },
        ..RunProfileConfig::default()
    };
    let claim = run(&limits(), &profile, &dirs, &call).await.unwrap();
    let rank = &start_event(&claim)["rank"];
    assert_eq!(rank["groups"], "claim", "{rank}");
    assert_eq!(rank["grep_corroboration"], 0.5, "{rank}");
    let notes = notes_of(&claim.response);
    assert!(
        notes.contains("groups claim") && notes.contains("score bound is off"),
        "{notes}"
    );
    // A handle made under claim groups resumes under them, whatever the
    // profile of the resuming call says.
    let handle = handle_of(&claim.response);
    let resume_call = Call {
        handle: Some(&handle),
        ..call
    };
    let resumed = run(&limits(), &RunProfileConfig::default(), &dirs, &resume_call)
        .await
        .unwrap();
    let rank = &start_event(&resumed)["rank"];
    assert_eq!(rank["groups"], "claim", "{rank}");
    assert_eq!(rank["grep_corroboration"], 0.5, "{rank}");
}

#[tokio::test]
async fn the_walk_reads_walk_pricing_from_the_run_profile() {
    use lci_core::config::{WalkPricing, WalkProfile};
    let (dir, index) = crowded_tree(30);
    let loaded = load(&index, Some(dir.path()), ScipMode::Authoritative)
        .await
        .unwrap();
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
    let call = Call {
        scip_index: &index,
        scip_root: Some(dir.path()),
        question: "q",
        seeds: std::slice::from_ref(&target),
        seed_search: None,
        top_k: 40,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    let dirs = Dirs::under(dir.path());
    // The potentials of the depth-1 cards, best first: one per rank.
    let potentials = |report: &Report| {
        let mut values: Vec<f64> = report.response["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|card| card["depth"] == 1)
            .map(|card| card["potential"].as_f64().unwrap())
            .collect();
        values.sort_by(|a, b| b.total_cmp(a));
        values
    };
    let close = |got: &[f64], want: Vec<f64>| {
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (g, w) in got.iter().zip(&want) {
            assert!((g - w).abs() < 1e-9, "{got:?} against {want:?}");
        }
    };
    // 30 edges are offered and the default beam keeps 16: ranks 0..16.
    let rank = run(&limits(), &RunProfileConfig::default(), &dirs, &call)
        .await
        .unwrap();
    close(
        &potentials(&rank),
        (0..16).map(|r| 1.0 / (1.0 + r as f64)).collect(),
    );
    assert!((potentials(&rank)[8] - 1.0 / 9.0).abs() < 1e-9);
    assert!(start_event(&rank).get("pricing").is_none());
    let profile = RunProfileConfig {
        walk: WalkProfile {
            pricing: WalkPricing::Relative,
            ..WalkProfile::default()
        },
        ..RunProfileConfig::default()
    };
    let relative = run(&limits(), &profile, &dirs, &call).await.unwrap();
    close(
        &potentials(&relative),
        (0..16)
            .map(|r| 1.0 / (1.0 + r as f64 * 8.0 / 30.0))
            .collect(),
    );
    assert!((potentials(&relative)[8] - 1.0 / (1.0 + 8.0 * 8.0 / 30.0)).abs() < 1e-9);
    assert_eq!(start_event(&relative)["pricing"], "relative");
}

// ---- `[run_profile.traverse] map_freshness` (slice K2a-2) ---------------------------

/// The maps built from the SCIP index and the tree: they state a snapshot.
const SCIP_BUILT_MAPS: [&str; 7] = [
    "graph.scip",
    "structure.contains",
    "seeds.scip_names",
    "exact.closure",
    "config.key",
    "scip.impact",
    "scip.implements",
];

/// One call over `dir` with the given maps and the key `[run_profile.traverse]
/// map_freshness` as `fresh` says, starting from the file
/// node `a.rs`; its traces go under `data`, outside the tree.
async fn freshness_call(
    dir: &Path,
    index: &Path,
    data: &Path,
    fresh: bool,
    maps: &[&str],
) -> Value {
    freshness_call_gated(dir, index, data, fresh, false, maps).await
}

/// [`freshness_call`] with `[run_profile.freshness] stat_gate` as given.
async fn freshness_call_gated(
    dir: &Path,
    index: &Path,
    data: &Path,
    fresh: bool,
    stat_gate: bool,
    maps: &[&str],
) -> Value {
    let profile = lci_core::config::RunProfileConfig {
        traverse: lci_core::config::RunProfileTraverse {
            map_freshness: fresh,
            ..Default::default()
        },
        freshness: lci_core::config::profile::RunProfileFreshness {
            stat_gate,
            ..Default::default()
        },
        ..Default::default()
    };
    let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
    let seeds = [NodeId::file("a.rs").unwrap().to_string()];
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: index,
        scip_root: Some(dir),
        question: "q",
        seeds: &seeds,
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    run(&limits(), &profile, &Dirs::under(data), &call)
        .await
        .unwrap()
        .response
}

/// `maps` of a response as `id -> (observed, stale, lag)`, checking that
/// every entry has exactly the four documented fields.
fn freshness_of(response: &Value) -> HashMap<String, (String, bool, Value)> {
    response["maps"]
        .as_array()
        .unwrap_or_else(|| panic!("no maps in {response}"))
        .iter()
        .map(|entry| {
            let mut fields: Vec<&str> = entry
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            fields.sort_unstable();
            assert_eq!(fields, ["id", "lag", "observed", "stale"], "{entry}");
            (
                entry["id"].as_str().unwrap().to_owned(),
                (
                    entry["observed"].as_str().unwrap().to_owned(),
                    entry["stale"].as_bool().unwrap(),
                    entry["lag"].clone(),
                ),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_traverse_answer_names_each_maps_observation_and_lag_when_the_key_is_on() {
    let (dir, index) = crowded_tree(2);
    let data = tempfile::tempdir().unwrap();
    let named: Vec<&str> = SCIP_BUILT_MAPS.into_iter().chain(["grep.live"]).collect();
    let call = || freshness_call(dir.path(), &index, data.path(), true, &named);

    // The first call that sees the index's stamp takes the baseline: every
    // SCIP-built map reads the snapshot, nothing differs; grep.live reads the
    // tree itself.
    let first = freshness_of(&call().await);
    let mut ids: Vec<&str> = first.keys().map(String::as_str).collect();
    ids.sort_unstable();
    let mut want: Vec<&str> = named.clone();
    want.sort_unstable();
    assert_eq!(ids, want, "one entry for each map that states a freshness");
    for id in SCIP_BUILT_MAPS {
        assert_eq!(first[id], ("snapshot".to_owned(), false, json!(0)), "{id}");
    }
    assert_eq!(first["grep.live"], ("live".to_owned(), false, json!(0)));

    // Appending to a file (a different byte length, no sleep and no clock)
    // makes the SCIP-built maps stale by one file; grep.live is not.
    let source = dir.path().join("a.rs");
    let mut text = std::fs::read_to_string(&source).unwrap();
    text.push_str("fn appended() {}\n");
    std::fs::write(&source, text).unwrap();
    let edited = freshness_of(&call().await);
    for id in SCIP_BUILT_MAPS {
        assert_eq!(edited[id], ("snapshot".to_owned(), true, json!(1)), "{id}");
    }
    assert_eq!(edited["grep.live"], ("live".to_owned(), false, json!(0)));

    // The lag counts files: one more added and the first removed make two.
    std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
    std::fs::remove_file(&source).unwrap();
    let files = freshness_of(&call().await);
    for id in SCIP_BUILT_MAPS {
        assert_eq!(files[id], ("snapshot".to_owned(), true, json!(2)), "{id}");
    }

    // A rewritten index has a new stamp, hence a new baseline: the maps are
    // built from the tree as it is and nothing differs again.
    let (_other, regenerated) = crowded_tree(3);
    std::fs::copy(&regenerated, &index).unwrap();
    let rewritten = freshness_of(&call().await);
    for id in SCIP_BUILT_MAPS {
        assert_eq!(
            rewritten[id],
            ("snapshot".to_owned(), false, json!(0)),
            "{id}"
        );
    }
}

#[tokio::test]
async fn with_the_key_off_the_answer_has_no_maps_and_the_key_on_adds_only_that_key() {
    let (dir, index) = crowded_tree(2);
    let data = tempfile::tempdir().unwrap();
    let named: Vec<&str> = SCIP_BUILT_MAPS.into_iter().chain(["grep.live"]).collect();
    let off = freshness_call(dir.path(), &index, data.path(), false, &named).await;
    let mut with = freshness_call(dir.path(), &index, data.path(), true, &named).await;

    assert!(off.get("maps").is_none(), "{off}");
    assert!(
        !lci_core::config::Config::default()
            .run_profile
            .traverse
            .map_freshness,
        "off by default"
    );
    // Without the `maps` key and the timings, the two answers are one.
    let strip = |response: &mut Value| {
        if let Some(object) = response.as_object_mut() {
            object.remove("maps");
        }
        if let Some(totals) = response["totals"].as_object_mut() {
            totals.remove("elapsed_ms");
            if let Some(per_map) = totals.get_mut("per_map").and_then(Value::as_object_mut) {
                per_map.values_mut().for_each(|stat| {
                    stat.as_object_mut().map(|stat| stat.remove("ms"));
                });
            }
        }
        if let Some(trace) = response["trace"].as_object_mut() {
            trace.remove("file");
        }
        // The cached maps keep one timing log across calls, so the second call's
        // p50 note counts more calls: the timings are not the answer.
        if let Some(notes) = response["notes"].as_array_mut() {
            notes.retain(|note| !note.as_str().is_some_and(|n| n.contains("expand p50")));
        }
    };
    let mut off = off;
    strip(&mut off);
    strip(&mut with);
    assert_eq!(off, with);
}

#[tokio::test]
async fn files_newer_than_the_index_count_from_the_first_call_once_each() {
    let (dir, index) = crowded_tree(2);
    let data = tempfile::tempdir().unwrap();
    // The index is ten seconds older than its tree (no sleep): `a.rs` is newer.
    let past = SystemTime::now() - Duration::from_secs(10);
    std::fs::File::options()
        .write(true)
        .open(&index)
        .unwrap()
        .set_modified(past)
        .unwrap();
    let named = ["graph.scip", "structure.contains", "grep.live"];
    let lag = |response: &Value| {
        let maps = freshness_of(response);
        let scip = maps["graph.scip"].clone();
        assert_eq!(maps["structure.contains"], scip);
        assert_eq!(maps["grep.live"], ("live".to_owned(), false, json!(0)));
        (scip.1, scip.2)
    };
    let call = || freshness_call(dir.path(), &index, data.path(), true, &named);
    let source = dir.path().join("a.rs");
    let edit = |text: &str| {
        let mut now = std::fs::read_to_string(&source).unwrap();
        now.push_str(text);
        std::fs::write(&source, now).unwrap();
    };

    // The first call has no earlier call to compare with, yet it does not
    // say "fresh": the file newer than the index is already counted.
    edit("fn edited() {}\n");
    assert_eq!(lag(&call().await), (true, json!(1)));
    // A second edit of the same file is the same one file.
    edit("fn edited_again() {}\n");
    assert_eq!(lag(&call().await), (true, json!(1)));
    // A file added counts too.
    std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
    assert_eq!(lag(&call().await), (true, json!(2)));
    // A file removed that the index did not predate does not.
    std::fs::remove_file(dir.path().join("b.rs")).unwrap();
    assert_eq!(lag(&call().await), (true, json!(1)));
}

#[tokio::test]
async fn without_a_tree_or_a_modified_time_the_lag_is_null_and_a_note_says_so() {
    // No tree: the call gives no root.
    let (dir, index) = crowded_tree(2);
    let data = tempfile::tempdir().unwrap();
    let profile = lci_core::config::RunProfileConfig {
        traverse: lci_core::config::RunProfileTraverse {
            map_freshness: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let maps = ["graph.scip", "structure.contains"].map(String::from);
    let seeds = [NodeId::file("a.rs").unwrap().to_string()];
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &index,
        scip_root: None,
        question: "q",
        seeds: &seeds,
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    let response = run(&limits(), &profile, &Dirs::under(data.path()), &call)
        .await
        .unwrap()
        .response;
    for (_, (observed, stale, lag)) in freshness_of(&response) {
        assert_eq!(
            (observed.as_str(), stale, lag),
            ("snapshot", false, Value::Null)
        );
    }
    assert!(
        response["notes"].to_string().contains("has no tree"),
        "{response}"
    );
    drop(dir);

    // An index file with no modified time is not compared.
    let tree = tempfile::tempdir().unwrap();
    for stat_gate in [false, true] {
        let lag = freshness::tree_lag(
            &tree.path().join("index.scip"),
            tree.path(),
            (None, 7),
            stat_gate,
        )
        .await
        .unwrap();
        assert_eq!(lag, None);
    }
}

/// `[run_profile.freshness] stat_gate` changes the cost of the read of the
/// tree, not the lag: the same edits give the same lags with the gate on and
/// off (every edit here moves the length or adds or removes a file).
#[tokio::test]
async fn the_lag_is_the_same_with_the_stat_gate_on_and_off() {
    let mut lags = Vec::new();
    for stat_gate in [false, true] {
        let (dir, index) = crowded_tree(2);
        let data = tempfile::tempdir().unwrap();
        let named = ["graph.scip", "structure.contains", "grep.live"];
        // The index is ten seconds older than its tree (no sleep): `a.rs` is
        // newer, as in the lag test above.
        std::fs::File::options()
            .write(true)
            .open(&index)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        let call =
            || freshness_call_gated(dir.path(), &index, data.path(), true, stat_gate, &named);
        let lag = |response: &Value| freshness_of(response)["graph.scip"].clone();
        let source = dir.path().join("a.rs");
        let mut seen = vec![lag(&call().await)];
        for text in ["fn edited() {}\n", "fn edited_again() {}\n"] {
            let mut now = std::fs::read_to_string(&source).unwrap();
            now.push_str(text);
            std::fs::write(&source, now).unwrap();
            seen.push(lag(&call().await));
        }
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        seen.push(lag(&call().await));
        std::fs::remove_file(dir.path().join("b.rs")).unwrap();
        seen.push(lag(&call().await));
        // The gate holds one table per root, and it follows the tree.
        let gate = lci_core::chunk::stat_gate(dir.path());
        let gated = gate.lock().unwrap().len();
        assert_eq!(gated, if stat_gate { 1 } else { 0 }, "a.rs only");
        lags.push(seen);
    }
    assert_eq!(lags[0], lags[1]);
    assert!(lags[0].last().unwrap().1, "a.rs is newer than the index");
}

// ---- `[run_profile.freshness] verify_on_read` (slice K2b) ---------------------------

/// One call over `tree` under the two keys as given, with every map named; the
/// question names `Foo` and asks which commit created it.
async fn verified_run(
    tree: &ScipTree,
    limits: &TraverseConfig,
    labels: bool,
    verify: bool,
    maps: &[&str],
) -> Value {
    let mut profile = RunProfileConfig::default();
    profile.traverse.map_freshness = labels;
    profile.freshness.verify_on_read = verify;
    let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
    let (budget, steer) = (
        BudgetArgs {
            pops: Some(3),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "Which commit first created `Foo`?",
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    run(limits, &profile, &Dirs::under(tree.dir.path()), &call)
        .await
        .unwrap()
        .response
}

/// The cards of a response that name a file, by `file_changed`: the files
/// whose card says `true`, the files whose card says `false`.
fn changed_files(response: &Value) -> (Vec<String>, Vec<String>) {
    let (mut changed, mut same) = (Vec::new(), Vec::new());
    for card in response["results"].as_array().unwrap() {
        let Some(path) = card["site"]["path"].as_str() else {
            continue;
        };
        match &card["file_changed"] {
            Value::Bool(true) => changed.push(path.to_owned()),
            Value::Bool(false) => same.push(path.to_owned()),
            other => panic!("{path}: {other} in {card}"),
        }
    }
    (changed, same)
}

#[tokio::test]
async fn file_changed_is_null_for_a_commit_card_and_absent_with_the_key_off() {
    let tree = ScipTree::new();
    tree.commit();
    let maps = ["graph.scip", EXACT_CLOSURE_ID, EXACT_PICKAXE_ID];
    // With a walk (`exact_seeds`) and without one (no start node): the cards of
    // both paths say whether their file changed.
    for limits in [stage_limits(true, false), stage_limits(false, false)] {
        let response = verified_run(&tree, &limits, true, true, &maps).await;
        let results = response["results"].as_array().unwrap();
        let commits: Vec<&Value> = results
            .iter()
            .filter(|card| card["node"].as_str().unwrap().starts_with("commit:"))
            .collect();
        assert_eq!(commits.len(), 1, "{response}");
        // A commit is history, not a file in the tree: nothing to compare.
        assert!(commits[0]["file_changed"].is_null(), "{}", commits[0]);
        assert!(
            commits[0].as_object().unwrap().contains_key("file_changed"),
            "the key is there, null: {}",
            commits[0]
        );
        let notes = response["notes"].to_string();
        assert!(
            notes.contains("verify_on_read: file_changed is null on 1 cards"),
            "{notes}"
        );
        // The tree is as the index saw it.
        let (changed, same) = changed_files(&response);
        assert!(changed.is_empty(), "{response}");
        assert!(same.iter().all(|path| path == "a.rs") && !same.is_empty());
    }

    // Either key off: the answer has no `file_changed` anywhere.
    for (labels, verify) in [(true, false), (false, true), (false, false)] {
        let response = verified_run(&tree, &stage_limits(true, false), labels, verify, &maps).await;
        assert!(!response.to_string().contains("file_changed"), "{response}");
        assert_eq!(response.get("maps").is_some(), labels, "{response}");
    }

    // An edit after the first call: the cards of the changed file say so.
    let source = tree.dir.path().join("a.rs");
    let mut text = std::fs::read_to_string(&source).unwrap();
    text.push_str("fn appended() {}\n");
    std::fs::write(&source, text).unwrap();
    let response = verified_run(&tree, &stage_limits(true, false), true, true, &maps).await;
    let (changed, same) = changed_files(&response);
    assert!(!changed.is_empty() && same.is_empty(), "{response}");
    assert!(changed.iter().all(|path| path == "a.rs"));
}

#[tokio::test]
async fn verify_on_read_labels_the_live_map_that_places_by_the_index() {
    let tree = ScipTree::new();
    let named = ["graph.scip", "grep.live"];
    let limits = stage_limits(true, false);
    let first = verified_run(&tree, &limits, true, true, &named).await;
    let entry = |response: &Value, id: &str| {
        response["maps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} in {response}"))
    };
    // No edit: the index's places hold, and the entry says whose they are.
    assert_eq!(
        entry(&first, "grep.live"),
        json!({"id": "grep.live", "observed": "live", "stale": false, "lag": 0, "places": "index"})
    );
    assert!(entry(&first, "graph.scip").get("places").is_none());
    // An edit: the same lag as the maps built from the index.
    let source = tree.dir.path().join("a.rs");
    let mut text = std::fs::read_to_string(&source).unwrap();
    text.push_str("fn appended() {}\n");
    std::fs::write(&source, text).unwrap();
    let edited = verified_run(&tree, &limits, true, true, &named).await;
    assert_eq!(
        entry(&edited, "grep.live"),
        json!({"id": "grep.live", "observed": "live", "stale": true, "lag": 1, "places": "index"})
    );
    assert_eq!(
        entry(&edited, "graph.scip"),
        json!({"id": "graph.scip", "observed": "snapshot", "stale": true, "lag": 1})
    );
    // With the key off the entry is what it was: live, not stale, no places.
    let off = verified_run(&tree, &limits, true, false, &named).await;
    assert_eq!(
        entry(&off, "grep.live"),
        json!({"id": "grep.live", "observed": "live", "stale": false, "lag": 0})
    );
}

// ---- `[run_profile.scip] mode` (slice K20a) ---------------------------

/// The wire form of a one-document SCIP index (`scip.proto`: Index.documents
/// = 2, Document.relative_path = 1 and .occurrences = 2, Occurrence.range = 1,
/// .symbol = 2, .symbol_roles = 3, .enclosing_range = 7): one function per
/// line, calling the functions after its name. The root crate has no
/// protobuf writer.
fn scip_file_bytes(functions: &[(&str, &[&str])]) -> Vec<u8> {
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    fn field(tag: u64, payload: &[u8], out: &mut Vec<u8>) {
        varint(tag << 3 | 2, out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }
    fn ints(tag: u64, values: &[u64], out: &mut Vec<u8>) {
        let mut packed = Vec::new();
        values.iter().for_each(|v| varint(*v, &mut packed));
        field(tag, &packed, out);
    }
    let occurrence = |range: [u64; 4], name: &str, roles: u64, enclosing: Option<[u64; 4]>| {
        let mut out = Vec::new();
        ints(1, &range, &mut out);
        field(
            2,
            format!("rust-analyzer cargo demo 0.1.0 m/{name}().").as_bytes(),
            &mut out,
        );
        if roles != 0 {
            varint(3 << 3, &mut out);
            varint(roles, &mut out);
        }
        if let Some(enclosing) = enclosing {
            ints(7, &enclosing, &mut out);
        }
        out
    };
    let mut document = Vec::new();
    field(1, b"src/m.rs", &mut document);
    for (line, (name, callees)) in functions.iter().enumerate() {
        let line = line as u64;
        let definition = occurrence([line, 3, line, 6], name, 1, Some([line, 0, line, 200]));
        field(2, &definition, &mut document);
        for (i, callee) in callees.iter().enumerate() {
            let col = 10 + 5 * i as u64;
            field(
                2,
                &occurrence([line, col, line, col + 4], callee, 0, None),
                &mut document,
            );
        }
    }
    let mut index = Vec::new();
    field(2, &document, &mut index);
    index
}

/// A tree with an `index.scip` for `alpha` calling `beta` and `gamma`, and
/// the id of `alpha` to start from.
struct ScipFileTree {
    dir: tempfile::TempDir,
    index: PathBuf,
    seed: String,
}

impl ScipFileTree {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.scip");
        std::fs::write(
            &index,
            scip_file_bytes(&[("alpha", &["beta", "gamma"]), ("beta", &[]), ("gamma", &[])]),
        )
        .unwrap();
        let loaded = load(&index, Some(dir.path()), ScipMode::Authoritative)
            .await
            .unwrap();
        let alpha = loaded
            .scip
            .graph()
            .node_index("rust-analyzer cargo demo 0.1.0 m/alpha().")
            .expect("alpha is in the fixture index");
        let seed = loaded.scip.node_id(alpha).to_string();
        Self { dir, index, seed }
    }

    async fn run(
        &self,
        mode: ScipMode,
        maps: &[&str],
        handle: Option<&str>,
        pops: u32,
    ) -> Result<Value> {
        self.run_at(&self.index, mode, maps, handle, pops).await
    }

    /// `run`, naming the index file by `index` (another spelling of its path).
    async fn run_at(
        &self,
        index: &Path,
        mode: ScipMode,
        maps: &[&str],
        handle: Option<&str>,
        pops: u32,
    ) -> Result<Value> {
        let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
        let (budget, steer) = (
            BudgetArgs {
                pops: Some(pops),
                ..BudgetArgs::default()
            },
            SteerArgs::default(),
        );
        let seeds = [self.seed.clone()];
        let call = Call {
            scip_index: index,
            scip_root: Some(self.dir.path()),
            question: "q",
            seeds: &seeds,
            seed_search: None,
            top_k: 8,
            maps: Some(&maps),
            budget: &budget,
            steer: &steer,
            handle,
            lsp: None,
            lsp_sessions: None,
        };
        let mut profile = RunProfileConfig::default();
        profile.scip.mode = mode;
        Ok(
            run(&limits(), &profile, &Dirs::under(self.dir.path()), &call)
                .await?
                .response,
        )
    }
}

/// The ids of the cards the walk reached.
fn reached_ids(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|card| card["node"].as_str().expect("node").to_owned())
        .collect()
}

#[tokio::test]
async fn the_scip_built_maps_are_empty_and_say_so_under_scip_mode_off() {
    let tree = ScipFileTree::new().await;
    let maps = [
        "graph.scip",
        "structure.contains",
        SCIP_IMPACT_ID,
        SCIP_IMPLEMENTS_ID,
        CONFIG_KEY_MAP_ID,
        EXACT_CLOSURE_ID,
    ];
    // Authoritative: the graph is read and the walk reaches what `alpha`
    // calls.
    let on = tree
        .run(ScipMode::Authoritative, &maps, None, 20)
        .await
        .unwrap();
    let reached = reached_ids(&on);
    for callee in ["beta", "gamma"] {
        assert!(
            reached.iter().any(|id| id.contains(callee)),
            "{callee}: {on}"
        );
    }
    assert!(
        on["totals"]["per_map"]["graph.scip"]["expands"]
            .as_u64()
            .unwrap()
            > 0,
        "{on}"
    );
    assert!(!on["notes"].to_string().contains("scip.mode"), "{on}");

    // Off: nothing reaches anything, and every map names the mode.
    let off = tree.run(ScipMode::Off, &maps, None, 20).await.unwrap();
    assert_eq!(reached_ids(&off), Vec::<String>::new(), "{off}");
    // The maps are asked (one expansion of the seed each) and return no
    // edge: no hop was made.
    assert_eq!(off["totals"]["hops"], 0, "{off}");
    assert!(on["totals"]["hops"].as_u64().unwrap() > 0, "{on}");
    assert_eq!(off["exact"][0]["map"], EXACT_CLOSURE_ID, "{off}");
    assert_eq!(off["exact"][0]["answered"], false, "{off}");
    let notes: Vec<String> = off["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|note| note.as_str().unwrap().to_owned())
        .collect();
    // Each map has a note of its own that names the mode; structure.contains
    // shares the one of graph.scip (the graph both read).
    for id in maps {
        let own = if id == "structure.contains" {
            "graph.scip"
        } else {
            id
        };
        assert!(
            notes.iter().any(|note| note.starts_with(own)
                && note.contains(id)
                && note.contains("scip.mode = off")),
            "no note for {id} names the mode in {notes:?}"
        );
    }
}

#[tokio::test]
async fn a_handle_made_with_a_graph_does_not_resume_under_scip_mode_off() {
    let tree = ScipFileTree::new().await;
    let first = tree
        .run(ScipMode::Authoritative, &["graph.scip"], None, 1)
        .await
        .unwrap();
    let handle = handle_of(&first);
    // The same mode resumes.
    tree.run(ScipMode::Authoritative, &["graph.scip"], Some(&handle), 1)
        .await
        .unwrap();
    // No graph is another identity, as another index is.
    let error = tree
        .run(ScipMode::Off, &["graph.scip"], Some(&handle), 1)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("belongs to another workspace or index"),
        "{error}"
    );
}

#[tokio::test]
async fn a_handle_is_bound_to_the_index_stamp_not_to_the_spelling_of_its_path() {
    let tree = ScipFileTree::new().await;
    let maps = ["graph.scip"];
    let first = tree
        .run(ScipMode::Authoritative, &maps, None, 1)
        .await
        .unwrap();
    let handle = handle_of(&first);
    // Each call builds its own store, so this reads the handle from its file,
    // as a restarted server would.
    tree.run(ScipMode::Authoritative, &maps, Some(&handle), 1)
        .await
        .unwrap();

    // Another spelling of the same index path (a `..` segment) is the same
    // index.
    std::fs::create_dir(tree.dir.path().join("sub")).unwrap();
    let spelled = tree.dir.path().join("sub").join("..").join("index.scip");
    assert_ne!(spelled, tree.index);
    tree.run_at(&spelled, ScipMode::Authoritative, &maps, Some(&handle), 1)
        .await
        .unwrap();

    // The same bytes with a later modified time are another index.
    let stamped = std::fs::metadata(&tree.index).unwrap().modified().unwrap();
    let set_modified = |time| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&tree.index)
            .unwrap()
            .set_modified(time)
            .unwrap();
    };
    set_modified(stamped + std::time::Duration::from_secs(1));
    let error = tree
        .run(ScipMode::Authoritative, &maps, Some(&handle), 1)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("belongs to another workspace or index"),
        "{error}"
    );
    // The stamp is the binding, not the content: it resumes again when the
    // stamp is back.
    set_modified(stamped);
    tree.run(ScipMode::Authoritative, &maps, Some(&handle), 1)
        .await
        .unwrap();

    // A text that cannot be a handle is refused before the store is asked
    // (empty or over 64 characters), then by the store (not 17 characters of
    // `h` and lowercase hex); a well-formed handle the store never wrote is
    // unknown.
    for (given, wanted) in [
        ("h".repeat(65), "handle is not a handle this server issued"),
        ("h".repeat(20), "not a handle: "),
        (format!("h{}", "0".repeat(16)), "unknown handle"),
    ] {
        let error = tree
            .run(ScipMode::Authoritative, &maps, Some(&given), 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(wanted), "{given}: {error}");
    }
}

/// A call that names `probe.compile` beside `graph.scip` on the tree fixture,
/// under the given service settings.
async fn run_naming_probe_compile(tree: &Tree, lsp: Option<&lci_core::config::Config>) -> Value {
    let maps = ["graph.scip".to_owned(), "probe.compile".to_owned()];
    let budget = BudgetArgs {
        pops: Some(1),
        ..BudgetArgs::default()
    };
    let steer = SteerArgs::default();
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "q",
        seeds: &tree.seeds,
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp,
        lsp_sessions: None,
    };
    run(
        &limits(),
        &RunProfileConfig::default(),
        &Dirs::under(tree.dir.path()),
        &call,
    )
    .await
    .unwrap()
    .response
}

fn policy_has_map(response: &Value, id: &str) -> bool {
    response["effective_policy"]["maps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|map| map["id"] == id)
}

#[tokio::test]
async fn probe_compile_is_skipped_without_the_trust_grant() {
    let untrusted = lci_core::config::Config {
        rust_analyzer_trusted: false,
        ..lci_core::config::Config::default()
    };
    for lsp in [None, Some(&untrusted)] {
        let tree = Tree::new();
        let response = run_naming_probe_compile(&tree, lsp).await;
        assert!(policy_has_map(&response, "graph.scip"), "{response}");
        assert!(!policy_has_map(&response, "probe.compile"), "{response}");
        let notes = notes_of(&response);
        assert!(notes.contains("probe.compile skipped"), "{notes}");
        assert!(notes.contains("rust_analyzer_trusted"), "{notes}");
        assert!(
            response["warnings"]
                .to_string()
                .contains("probe.compile skipped"),
            "{response}"
        );
        // The scratch checkout is the first thing a probe creates: cargo was
        // never near. (The default path: the test assumes LCI_PROBE_SCRATCH_DIR
        // is unset.)
        let scratch = lci_maps::probe_map::ProbeConfig::for_tree(tree.dir.path()).scratch;
        assert!(!scratch.exists(), "{}", scratch.display());
    }
}

#[tokio::test]
async fn probe_compile_registers_under_the_grant_and_says_what_runs() {
    let trusted = lci_core::config::Config {
        rust_analyzer_trusted: true,
        ..lci_core::config::Config::default()
    };
    let tree = Tree::new();
    let response = run_naming_probe_compile(&tree, Some(&trusted)).await;
    assert!(policy_has_map(&response, "probe.compile"), "{response}");
    let notes = notes_of(&response);
    assert!(notes.contains("build scripts"), "{notes}");
    assert!(!notes.contains("probe.compile skipped"), "{notes}");
    // A file node is not a unit the map accepts, so no expansion ran cargo.
    let scratch = lci_maps::probe_map::ProbeConfig::for_tree(tree.dir.path()).scratch;
    assert!(!scratch.exists(), "{}", scratch.display());
}

// ---- `[run_profile.traverse] card_paths` (slice K27) --------------------------

fn card_paths_profile(on: bool) -> RunProfileConfig {
    let mut profile = RunProfileConfig::default();
    profile.traverse.card_paths = on;
    profile
}

/// The crowded tree walked from `user1` through `graph.scip` (the call to
/// `target`) and `scip.impact` (the other callers of `target`): the walk has
/// hops of two kinds and goes two hops deep. The handles are kept in the
/// tree's own directory, so the calls of one world can continue each other.
struct PathsWorld {
    dir: tempfile::TempDir,
    index: PathBuf,
    start: String,
}

impl PathsWorld {
    async fn new() -> Self {
        let (dir, index) = crowded_tree(3);
        let loaded = load(&index, Some(dir.path()), ScipMode::Authoritative)
            .await
            .unwrap();
        let graph = loaded.scip.graph();
        let user = graph
            .node_index("rust-analyzer cargo demo 0.1.0 a/user1().")
            .expect("user1 is a graph node");
        let start = loaded.scip.node_id(user).to_string();
        Self { dir, index, start }
    }

    async fn call(
        &self,
        profile: &RunProfileConfig,
        limits: &TraverseConfig,
        pops: Option<u32>,
        handle: Option<&str>,
    ) -> Report {
        let maps = [
            "graph.scip".to_owned(),
            SCIP_IMPACT_ID.to_owned(),
            "structure.contains".to_owned(),
        ];
        let budget = BudgetArgs {
            pops,
            max_depth: Some(3),
            ..BudgetArgs::default()
        };
        let steer = SteerArgs::default();
        let seeds = [self.start.clone()];
        let call = Call {
            scip_index: &self.index,
            scip_root: Some(self.dir.path()),
            question: "q",
            seeds: &seeds,
            seed_search: None,
            top_k: 8,
            maps: Some(&maps),
            budget: &budget,
            steer: &steer,
            handle,
            lsp: None,
            lsp_sessions: None,
        };
        run(limits, profile, &Dirs::under(self.dir.path()), &call)
            .await
            .unwrap()
    }
}

/// The events of a run's trace.
fn trace_events(report: &Report) -> Vec<Value> {
    let trace = report.trace.as_ref().expect("the trace was written");
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// `(src, dst)` to the `(kind, witness)` of the latest `hop` or `relax` event
/// of that edge: the route the walk ended with.
fn route_events(report: &Report) -> HashMap<(String, String), (Value, Value)> {
    trace_events(report)
        .into_iter()
        .filter(|event| matches!(event["ev"].as_str(), Some("hop" | "relax")))
        .map(|event| {
            (
                (
                    event["src"].as_str().unwrap().to_owned(),
                    event["dst"].as_str().unwrap().to_owned(),
                ),
                (event["kind"].clone(), event["witness"].clone()),
            )
        })
        .collect()
}

/// The cards of the lists a response can have.
fn card_lists(response: &Value) -> Vec<(&'static str, &Vec<Value>)> {
    [
        "/results",
        "/checkpoint/answer",
        "/checkpoint/visited_cards",
        "/checkpoint/frontier",
    ]
    .into_iter()
    .filter_map(|list| Some((list, response.pointer(list)?.as_array()?)))
    .collect()
}

/// Takes `kind` and `path` off the cards, and the clocks and the handle (a
/// snapshot with kinds is another snapshot) out of a response.
fn without_paths_and_clocks(response: &mut Value) {
    for list in ["/results", "/checkpoint/answer"] {
        if let Some(Value::Array(cards)) = response.pointer_mut(list) {
            for card in cards {
                if let Some(object) = card.as_object_mut() {
                    object.remove("kind");
                    object.remove("path");
                }
            }
        }
    }
    if let Some(object) = response.as_object_mut() {
        object.remove("handle");
    }
    if let Some(object) = response["checkpoint"].as_object_mut() {
        object.remove("handle");
    }
    if let Some(totals) = response["totals"].as_object_mut() {
        totals.remove("elapsed_ms");
        if let Some(per_map) = totals.get_mut("per_map").and_then(Value::as_object_mut) {
            per_map.values_mut().for_each(|stat| {
                stat.as_object_mut().map(|stat| stat.remove("ms"));
            });
        }
    }
    if let Some(trace) = response["trace"].as_object_mut() {
        trace.remove("file");
    }
    if let Some(notes) = response["notes"].as_array_mut() {
        notes.retain(|note| !note.as_str().is_some_and(|n| n.contains("expand p50")));
    }
}

#[tokio::test]
async fn cards_carry_their_path_with_the_key_on_and_nothing_with_it_off() {
    let world = PathsWorld::new().await;
    for pops in [None, Some(2)] {
        let off = world
            .call(&card_paths_profile(false), &limits(), pops, None)
            .await;
        let on = world
            .call(&card_paths_profile(true), &limits(), pops, None)
            .await;
        assert_eq!(off.response["checkpoint"].is_object(), pops.is_some());

        // Off: no card has `kind` or `path`, and the profile default is off.
        assert!(!RunProfileConfig::default().traverse.card_paths);
        for (list, cards) in card_lists(&off.response) {
            assert!(
                cards
                    .iter()
                    .all(|c| c.get("path").is_none() && c.get("kind").is_none()),
                "{list}"
            );
        }
        // On: the same answer, the same trace, and the paths added.
        let (mut stripped_off, mut stripped_on) = (off.response.clone(), on.response.clone());
        without_paths_and_clocks(&mut stripped_off);
        without_paths_and_clocks(&mut stripped_on);
        assert_eq!(stripped_off, stripped_on);
        assert_eq!(
            std::fs::read(off.trace.as_ref().unwrap()).unwrap(),
            std::fs::read(on.trace.as_ref().unwrap()).unwrap()
        );

        let hops = route_events(&on);
        let results = on.response["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert!(
            results.iter().any(|card| card["depth"].as_u64() >= Some(2)),
            "a walk two hops deep: {}",
            on.response
        );
        let seeds = on.response["seeds"].as_array().unwrap();
        let kinds: std::collections::BTreeSet<&str> = hops
            .values()
            .filter_map(|(kind, _)| kind.as_str())
            .collect();
        assert!(!kinds.is_empty(), "{kinds:?}");
        for (list, cards) in card_lists(&on.response) {
            for card in cards {
                if !matches!(list, "/results" | "/checkpoint/answer") {
                    // The steering views carry no route.
                    assert!(
                        card.get("path").is_none() && card.get("kind").is_none(),
                        "{list}: {card}"
                    );
                    continue;
                }
                let depth = card["depth"].as_u64().unwrap() as usize;
                let path = card["path"].as_array().expect("a path");
                assert_eq!(path.len(), depth + 1, "{card}");
                assert!(seeds.contains(&path[0]["node"]), "{card}");
                assert_eq!(path[0]["kind"], Value::Null);
                assert_eq!(path[0]["witness"], Value::Null);
                let last = path.last().unwrap();
                assert_eq!(last["node"], card["node"]);
                assert_eq!(last["map"], card["via"]);
                assert_eq!(last["witness"], card["witness"]);
                assert_eq!(last["kind"], card["kind"]);
                if depth == 0 {
                    assert_eq!(card["kind"], Value::Null);
                }
                for pair in path.windows(2) {
                    let edge = (
                        pair[0]["node"].as_str().unwrap().to_owned(),
                        pair[1]["node"].as_str().unwrap().to_owned(),
                    );
                    let (kind, witness) = hops
                        .get(&edge)
                        .unwrap_or_else(|| panic!("no hop event for {edge:?}"));
                    assert_eq!(&pair[1]["kind"], kind, "{edge:?}");
                    assert_eq!(&pair[1]["witness"], witness, "{edge:?}");
                    assert!(pair[1]["kind"].is_string());
                }
            }
        }
        // The answer's size is the mechanism's: one entry per hop and card.
        let longest = results
            .iter()
            .flat_map(|card| card["path"].as_array().unwrap())
            .map(|entry| entry["node"].as_str().unwrap().len())
            .max()
            .unwrap();
        let (chars_off, chars_on) = (
            off.response.to_string().len(),
            on.response.to_string().len(),
        );
        let max_depth = 3;
        assert!(
            chars_on - chars_off <= results.len() * 2 * (max_depth + 1) * (longest + 48),
            "{chars_off} -> {chars_on} chars"
        );
    }
}

#[tokio::test]
async fn a_first_stage_card_has_no_path_and_a_walk_card_has_one() {
    let world = PathsWorld::new().await;
    let report = world
        .call(
            &card_paths_profile(true),
            &stage_limits(false, true),
            None,
            None,
        )
        .await;
    let results = report.response["results"].as_array().unwrap();
    let origins: Vec<&str> = results
        .iter()
        .map(|card| card["origin"].as_str().unwrap())
        .collect();
    assert!(
        origins.contains(&"seed") && origins.contains(&"walk"),
        "{origins:?}"
    );
    for card in results {
        let has = card.get("path").is_some();
        assert_eq!(has, card["origin"] == "walk", "{card}");
        assert_eq!(card.get("kind").is_some(), has, "{card}");
    }
}

#[tokio::test]
async fn a_resume_shows_the_whole_path_and_notes_missing_kinds() {
    let world = PathsWorld::new().await;
    let on = card_paths_profile(true);
    let off = card_paths_profile(false);
    let note = "path kinds before this call are not kept";
    let route = |card: &Value| -> Vec<(String, Value)> {
        card["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["node"].as_str().unwrap().to_owned(),
                    entry["kind"].clone(),
                )
            })
            .collect()
    };

    // Both calls with the key on: the second call's paths go through the
    // nodes the first found, with their kinds, and the walk is the straight
    // run's.
    let first = world.call(&on, &limits(), Some(1), None).await;
    let handle = handle_of(&first.response);
    let second = world.call(&on, &limits(), Some(20), Some(&handle)).await;
    assert!(
        !notes_of(&second.response).contains(note),
        "{}",
        second.response
    );
    let straight = world.call(&on, &limits(), Some(20), None).await;
    for response in [&second.response, &straight.response] {
        for card in response["results"].as_array().unwrap() {
            let path = route(card);
            assert_eq!(path.len() as u64, card["depth"].as_u64().unwrap() + 1);
            assert!(path[1..].iter().all(|(_, kind)| kind.is_string()), "{card}");
        }
    }
    let paths = |response: &Value| -> Vec<(String, Vec<(String, Value)>)> {
        let mut paths: Vec<_> = response["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|card| (card["node"].as_str().unwrap().to_owned(), route(card)))
            .collect();
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        paths
    };
    assert_eq!(paths(&second.response), paths(&straight.response));
    let found_first: std::collections::HashSet<String> = first.response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["node"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        second.response["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|card| route(card)
                .iter()
                .skip(1)
                .any(|(node, _)| found_first.contains(node))
                && card["depth"].as_u64() >= Some(2)),
        "a path through a node of the first call: {}",
        second.response
    );

    // A handle made with the key off: the old hops have no kind, the note
    // says so, and the hops of this call have theirs.
    let first = world.call(&off, &limits(), Some(1), None).await;
    let handle = handle_of(&first.response);
    let second = world.call(&on, &limits(), Some(20), Some(&handle)).await;
    assert!(
        notes_of(&second.response).contains(note),
        "{}",
        second.response
    );
    let found_first: std::collections::HashSet<String> = first.response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["node"].as_str().unwrap().to_owned())
        .collect();
    let (mut old_hops, mut new_hops) = (0, 0);
    for card in second.response["results"].as_array().unwrap() {
        for (node, kind) in route(card).into_iter().skip(1) {
            if found_first.contains(&node) {
                assert_eq!(kind, Value::Null, "{card}");
                old_hops += 1;
            } else {
                assert!(kind.is_string(), "{card}");
                new_hops += 1;
            }
        }
        // The card's own kind is the one of its last hop.
        let last = route(card).pop().unwrap().1;
        assert_eq!(card["kind"], last);
    }
    assert!(old_hops > 0 && new_hops > 0, "{old_hops} {new_hops}");
    // And with the key off in both calls there is no note.
    let second = world.call(&off, &limits(), Some(20), Some(&handle)).await;
    assert!(!notes_of(&second.response).contains(note));
    assert!(second.response["results"][0].get("path").is_none());
}

// ---- `seeds.bm25` (slice K21a-1) -----------------------------------------------

/// A graph of one function per entry `(symbol path, file, documentation)`,
/// each defined on a line of its own, with the documentation by symbol.
fn bm25_graph(
    entries: &[(&str, &str, Option<&str>)],
) -> (ScipMap, lci_maps::scip_graph::SymbolDocs) {
    let mut documents: Vec<RawDocument> = Vec::new();
    let mut docs = lci_maps::scip_graph::SymbolDocs::new();
    for (line, (path, file, doc)) in entries.iter().enumerate() {
        let symbol = format!("rust-analyzer cargo demo 0.1.0 {path}");
        let line = line as u32;
        let occurrence = RawOccurrence {
            range: (line, 3, line, 8),
            symbol: symbol.clone(),
            roles: 1,
            enclosing: Some((line, 0, line, 20)),
        };
        match documents.iter_mut().find(|document| document.path == *file) {
            Some(document) => document.occurrences.push(occurrence),
            None => documents.push(RawDocument {
                path: (*file).to_owned(),
                occurrences: vec![occurrence],
            }),
        }
        if let Some(doc) = doc {
            docs.insert(symbol, (*doc).to_owned());
        }
    }
    (
        ScipMap::with_default_ids(ScipGraph::build(&documents, None)),
        docs,
    )
}

/// Twelve functions: `Config::parse` holds both words of "parse config" in its
/// name and its path; `parse_flags` only the first; `load_settings` holds
/// "parses the config file" in its documentation and `config_loader` in its
/// file; `render_page` has `config` in its file's name alone. Eleven of them
/// are in the namespace `util`.
const BM25_ENTRIES: [(&str, &str, Option<&str>); 12] = [
    ("config/Config#parse().", "src/config.rs", None),
    ("util/parse_flags().", "src/cli.rs", None),
    (
        "util/load_settings().",
        "src/config_loader.rs",
        Some("Parses the config file and returns the settings."),
    ),
    ("util/render_page().", "src/config.rs", None),
    ("util/open_socket().", "src/net.rs", None),
    ("util/alpha_one().", "src/misc.rs", None),
    ("util/bravo_two().", "src/misc.rs", None),
    ("util/delta_three().", "src/misc.rs", None),
    ("util/gamma_four().", "src/misc.rs", None),
    ("util/kilo_five().", "src/misc.rs", None),
    ("util/lima_six().", "src/misc.rs", None),
    ("util/mike_seven().", "src/misc.rs", None),
];

#[test]
fn bm25_seeds_rank_a_rare_name_over_a_common_word_and_read_docs() {
    use super::seeds::Bm25Seeds;
    let (graph, docs) = bm25_graph(&BM25_ENTRIES);
    let with_docs = Bm25Seeds::new(&graph, &docs);
    let names = |found: &[(NodeId, String, f64)]| -> Vec<String> {
        found.iter().map(|(_, name, _)| name.clone()).collect()
    };

    // The entity that holds both words in its name and its path is first; the
    // one whose documentation says "parses the config file" beats the one with
    // `config` in a file name alone; nothing else is returned.
    let found = with_docs.matches("parse config", 12);
    assert_eq!(
        names(&found),
        [
            "Config::parse",
            "util::load_settings",
            "util::parse_flags",
            "util::render_page"
        ],
        "{found:?}"
    );
    assert!(found.windows(2).all(|pair| pair[0].2 >= pair[1].2));
    assert_eq!(with_docs.matches("parse config", 2).len(), 2, "capped");

    // The documentation is read: without it `load_settings` scores less.
    let without_docs = Bm25Seeds::new(&graph, &Default::default());
    let score = |seeds: &Bm25Seeds| {
        seeds
            .matches("parse config", 12)
            .into_iter()
            .find(|(_, name, _)| name == "util::load_settings")
            .map(|(_, _, score)| score)
    };
    assert!(score(&with_docs) > score(&without_docs));

    // `util` is in eleven of the twelve nodes: dropped as a scoring term
    // (more than a quarter of them) unless it is the only one.
    assert_eq!(
        with_docs.matches("util parse", 12),
        with_docs.matches("parse", 12)
    );
    assert_eq!(with_docs.matches("util", 3).len(), 3);

    // No scoring term, no seed.
    assert!(with_docs.matches("", 3).is_empty());
    assert!(with_docs.matches("the and for", 3).is_empty());
    assert!(with_docs.matches("zebra", 3).is_empty());
    assert!(
        Bm25Seeds::new(
            &ScipMap::with_default_ids(ScipGraph::empty()),
            &Default::default()
        )
        .matches("parse", 3)
        .is_empty()
    );
}

/// [`seeds_for`] over a given `loaded`, with the question's maps.
async fn seeds_in(
    loaded: &Loaded,
    search: Option<&dyn SeedSearch>,
    question: &str,
    wanted: &[MapId],
) -> Seeds {
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: Path::new("unused"),
        scip_root: None,
        question,
        seeds: &[],
        seed_search: search,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    find_seeds(&limits(), &call, loaded, wanted).await.unwrap()
}

#[tokio::test]
async fn bm25_seeds_go_after_the_hybrid_ones_without_duplicates_and_replace_a_failed_search() {
    let with_bm25: Vec<MapId> = MAPS.iter().copied().chain([SEEDS_BM25]).collect();
    let question = "rank hits";
    let loaded = loaded();

    // The key's map not wanted: the entity index is never built.
    let hybrid = Canned::new(Ok(vec![hit("src/search.rs", (13, 15), 1)]));
    let plain = seeds_in(&loaded, Some(&hybrid), question, &MAPS).await;
    assert_eq!(plain.source, "hybrid");
    assert!(!loaded.bm25_built());

    // Wanted, with the hybrid search answering: its nodes first, then the
    // entities of the question's words, tagged as bm25's.
    let found = seeds_in(&loaded, Some(&hybrid), question, &with_bm25).await;
    assert!(loaded.bm25_built());
    assert_eq!(found.source, "hybrid+bm25");
    assert!(found.fallback.is_none() && found.notes.is_empty());
    assert_eq!(found.nodes.len(), 2, "{:?}", found.details);
    assert_eq!(found.nodes[0].node, plain.nodes[0].node);
    assert_eq!(found.nodes[0].map, Some(SEEDS_HYBRID));
    assert_eq!(found.nodes[1].map, Some(SEEDS_BM25));
    assert_eq!(found.details.len(), found.nodes.len());
    let detail = &found.details[1];
    assert_eq!(detail["how"], "bm25");
    assert_eq!(detail["retrievers"], json!(["bm25"]));
    assert_eq!(detail["score_kind"], "bm25");
    assert_eq!(detail["node"], found.nodes[1].node.to_string());
    assert!(detail["symbol"].as_str().unwrap().ends_with("rank_hits"));
    assert!(detail["score"].as_f64().unwrap() > 0.0);

    // A node the hybrid search gave is not added again, and the source stays.
    let inside = Canned::new(Ok(vec![hit("src/search.rs", (17, 19), 1)]));
    let same = seeds_in(&loaded, Some(&inside), question, &with_bm25).await;
    assert_eq!(same.source, "hybrid");
    assert_eq!(same.nodes.len(), 1);
    assert_eq!(same.nodes[0].map, Some(SEEDS_HYBRID));
    assert_eq!(same.nodes[0].node, found.nodes[1].node, "rank_hits");

    // The hybrid search failed or found nothing, or there is none: the
    // entities are the start nodes, the reason stays visible, and the
    // stand-in is not used.
    let failing = Canned::new(Err("the embedder is down".into()));
    let empty = Canned::new(Ok(Vec::new()));
    for (search, why) in [
        (Some(&failing as &dyn SeedSearch), "embedder is down"),
        (Some(&empty as &dyn SeedSearch), "no hits"),
        (None, "no workspace"),
    ] {
        let found = seeds_in(&loaded, search, question, &with_bm25).await;
        assert_eq!(found.source, "bm25", "{why}");
        assert_eq!(found.nodes.len(), 1, "{why}");
        assert_eq!(found.nodes[0].map, Some(SEEDS_BM25));
        assert!(found.fallback.as_deref().unwrap().contains(why), "{why}");
        assert!(found.notes[0].contains("bm25"), "{:?}", found.notes);
        assert!(found.details.iter().all(|d| d["how"] == "bm25"));
    }

    // Only the entity map wanted: no search at all.
    let calls = hybrid.calls.load(Ordering::SeqCst);
    let alone = seeds_in(&loaded, Some(&hybrid), question, &[SEEDS_BM25, SCIP_MAP_ID]).await;
    assert_eq!(alone.source, "bm25");
    assert!(alone.fallback.is_none() && alone.notes.is_empty());
    assert_eq!(hybrid.calls.load(Ordering::SeqCst), calls);

    // Nothing for the entities to match: the stand-in answers, as without.
    let zebra = seeds_in(&loaded, None, "zebra", &with_bm25).await;
    assert_eq!(zebra.source, "name_tokens");
    assert!(zebra.nodes.is_empty());
}

/// One call over the `crowded_tree` index, under the run profile with
/// `[run_profile.seeds] bm25` as `bm25` says.
async fn bm25_call(
    tree: &(tempfile::TempDir, PathBuf),
    bm25: bool,
    maps: Option<&[&str]>,
    question: &str,
    search: Option<&dyn SeedSearch>,
) -> Value {
    let data = tempfile::tempdir().unwrap();
    let profile = lci_core::config::RunProfileConfig {
        seeds: lci_core::config::RunProfileSeeds {
            bm25,
            ..Default::default()
        },
        ..Default::default()
    };
    let maps: Option<Vec<String>> = maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &tree.1,
        scip_root: Some(tree.0.path()),
        question,
        seeds: &[],
        seed_search: search,
        top_k: 8,
        maps: maps.as_deref(),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    run(&limits(), &profile, &Dirs::under(data.path()), &call)
        .await
        .unwrap()
        .response
}

#[tokio::test]
async fn seeds_bm25_is_inert_with_the_key_off_and_seeds_after_hybrid_with_it_on() {
    let tree = crowded_tree(2);
    let loaded = load(&tree.1, Some(tree.0.path()), ScipMode::Authoritative)
        .await
        .unwrap();
    let id_of = |name: &str| {
        let graph = loaded.scip.graph();
        let at = graph
            .node_index(&format!("rust-analyzer cargo demo 0.1.0 a/{name}()."))
            .unwrap();
        loaded.scip.node_id(at).to_string()
    };
    assert!(
        !lci_core::config::Config::default().run_profile.seeds.bm25,
        "off by default"
    );

    // Key off, the map named: the note, and the answer is the one without it
    // (nothing registers, nothing seeds, the index is not built).
    let named = Some(&["seeds.bm25", "graph.scip"][..]);
    let mut off = bm25_call(&tree, false, named, "target", None).await;
    let mut without = bm25_call(&tree, false, Some(&["graph.scip"]), "target", None).await;
    let notes = off["notes"].to_string();
    assert!(
        notes.contains("seeds.bm25 is off ([run_profile.seeds] bm25 = false)"),
        "{notes}"
    );
    assert_eq!(off["seed_source"], without["seed_source"]);
    assert_eq!(off["seeds"], json!([]));
    off.as_object_mut().unwrap().remove("notes");
    without.as_object_mut().unwrap().remove("notes");
    assert_eq!(off, without);
    assert!(!loaded.bm25_built());

    // Key off and the default maps with a hybrid hit: as before the key.
    let hybrid = Canned::new(Ok(vec![hit("a.rs", (2, 2), 1)]));
    let plain = bm25_call(&tree, false, None, "target", Some(&hybrid)).await;
    assert_eq!(plain["seed_source"], "hybrid");
    assert_eq!(plain["seeds"], json!([id_of("user1")]));
    assert!(!plain.to_string().contains("bm25"), "{plain}");
    assert!(!loaded.bm25_built());

    // Key on, no maps named: the map is in the default set, after the hybrid
    // nodes; `target` is the only entity of the question's word.
    let on = bm25_call(&tree, true, None, "target", Some(&hybrid)).await;
    assert!(loaded.bm25_built());
    assert_eq!(on["seed_source"], "hybrid+bm25");
    assert_eq!(on["seeds"], json!([id_of("user1"), id_of("target")]));
    assert_eq!(on["seed_hits"][1]["how"], "bm25");
    assert_eq!(
        on["seed_hits"][0]["retrievers"],
        plain["seed_hits"][0]["retrievers"]
    );

    // Key on and the hybrid search down: the entities are the start nodes.
    let down = NoSearch("the embedder is down".into());
    let stand_in = bm25_call(&tree, true, None, "target", Some(&down)).await;
    assert_eq!(stand_in["seed_source"], "bm25");
    assert_eq!(stand_in["seeds"], json!([id_of("target")]));
    assert!(
        stand_in["seed_fallback"]
            .as_str()
            .unwrap()
            .contains("embedder is down")
    );

    // Key on, the map named without the others: it runs and needs no tree.
    let named_on = bm25_call(&tree, true, named, "target", None).await;
    assert_eq!(named_on["seed_source"], "bm25");
    assert_eq!(named_on["seeds"], json!([id_of("target")]));
    assert!(!named_on["notes"].to_string().contains("is off"));
}

/// A tree with `a.rs` holding `widget` and `plain`, and a SCIP index over it
/// whose symbol information gives `widget` a documentation line.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn documented_tree() -> (tempfile::TempDir, PathBuf) {
    use scip::types::{Document, Index, Occurrence, SymbolInformation};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn widget() {}\nfn plain() {}\n").unwrap();
    let sym = |name: &str| format!("rust-analyzer cargo demo 0.1.0 a/{name}().");
    let mut document = Document::default();
    document.relative_path = "a.rs".to_owned();
    for (line, name) in ["widget", "plain"].into_iter().enumerate() {
        let mut o = Occurrence::default();
        o.range = vec![line as i32, 3, 9];
        o.symbol = sym(name);
        o.symbol_roles = 1;
        o.enclosing_range = vec![line as i32, 0, line as i32, 14];
        document.occurrences.push(o);
        let mut info = SymbolInformation::default();
        info.symbol = sym(name);
        if name == "widget" {
            info.documentation = vec!["Frobnicates the gizmo.".to_owned()];
        }
        document.symbols.push(info);
    }
    let mut message = Index::default();
    message.documents = vec![document];
    let index = dir.path().join("index.scip");
    scip::write_message_to_file(&index, message).unwrap();
    (dir, index)
}

#[tokio::test]
async fn seeds_bm25_reads_the_indexs_documentation_only_when_the_map_runs() {
    let tree = documented_tree();
    let loaded = load(&tree.1, Some(tree.0.path()), ScipMode::Authoritative)
        .await
        .unwrap();
    assert_eq!(loaded.index.as_deref(), Some(tree.1.as_path()));
    let widget = loaded
        .scip
        .node_id(
            loaded
                .scip
                .graph()
                .node_index("rust-analyzer cargo demo 0.1.0 a/widget().")
                .unwrap(),
        )
        .to_string();
    // No name, path or hybrid hit holds "gizmo"; the key off never builds the
    // entity index, so no documentation was read.
    let off = bm25_call(&tree, false, None, "gizmo", None).await;
    assert_eq!(off["seeds"], json!([]));
    assert!(!loaded.bm25_built());
    // The key on: the build reads the file's documentation and the node whose
    // documentation holds the word is the seed.
    let on = bm25_call(&tree, true, None, "gizmo", None).await;
    assert!(loaded.bm25_built());
    assert_eq!(on["seed_source"], "bm25");
    assert_eq!(on["seeds"], json!([widget]));
}

/// One call over the three-file tree, two pops, writing under `dirs`.
async fn trajectory_run(tree: &Tree, dirs: &Dirs) -> Report {
    let (budget, steer) = (
        BudgetArgs {
            pops: Some(2),
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
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    run(&limits(), &RunProfileConfig::default(), dirs, &call)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_cli_switch_reaches_the_walk_through_dirs() {
    let tree = Tree::new();
    let (off_dir, on_dir) = (tree.dir.path().join("off"), tree.dir.path().join("on"));
    let off = trajectory_run(&tree, &Dirs::under(&off_dir)).await;
    let on_dirs = Dirs {
        trajectory_candidates: 2,
        ..Dirs::under(&on_dir)
    };
    let on = trajectory_run(&tree, &on_dirs).await;
    let events = |report: &Report| {
        lci_maps::traverse::trace::read_trace(report.trace.as_ref().unwrap()).unwrap()
    };
    let (off_events, on_events) = (events(&off), events(&on));
    assert!(off_events.iter().all(|e| e["ev"] != "pop"));
    let pops: Vec<&Value> = on_events.iter().filter(|e| e["ev"] == "pop").collect();
    assert!(!pops.is_empty(), "{on_events:?}");
    assert!(
        pops.iter()
            .all(|e| e["candidates"].as_array().unwrap().len() <= 2)
    );
    // Only the trace differs: the other events and the answer are equal.
    let without: Vec<&Value> = on_events.iter().filter(|e| e["ev"] != "pop").collect();
    assert_eq!(without.len(), off_events.len());
    let strip = |report: &Report| {
        let mut response = report.response.clone();
        if let Some(object) = response.as_object_mut() {
            object.remove("trace");
            object.remove("notes");
        }
        if let Some(totals) = response["totals"].as_object_mut() {
            totals.remove("elapsed_ms");
            if let Some(per_map) = totals.get_mut("per_map").and_then(Value::as_object_mut) {
                per_map.values_mut().for_each(|stat| {
                    stat.as_object_mut().map(|stat| stat.remove("ms"));
                });
            }
        }
        response
    };
    assert_eq!(strip(&off), strip(&on));
    assert_eq!(
        on.response["trace"]["events"].as_u64().unwrap() as usize,
        on_events.len()
    );
    assert_eq!(
        off.response["trace"]["events"].as_u64().unwrap() as usize,
        off_events.len()
    );
}

// ---- `seeds.literal` (slice K22a-1) --------------------------------------------

use lci_core::lexical::{LiteralMatch, LiteralSearch};

/// A search with canned hybrid and literal answers that counts the literal
/// calls (`literal: None` is a search that cannot run ripgrep).
struct LiteralStub {
    hybrid: Canned,
    literal: Option<std::result::Result<LiteralSearch, String>>,
    literal_calls: AtomicUsize,
    asked: Mutex<Vec<(Vec<String>, u32)>>,
}

impl LiteralStub {
    fn new(
        hybrid: std::result::Result<Vec<SeedHit>, String>,
        literal: Option<std::result::Result<LiteralSearch, String>>,
    ) -> Self {
        Self {
            hybrid: Canned::new(hybrid),
            literal,
            literal_calls: AtomicUsize::new(0),
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[lci_maps::async_trait]
impl SeedSearch for LiteralStub {
    async fn search(&self, question: &str, k: usize) -> Result<Vec<SeedHit>> {
        self.hybrid.search(question, k).await
    }

    async fn literal_search(
        &self,
        names: &[String],
        max_per_file: u32,
    ) -> Option<Result<LiteralSearch>> {
        self.literal_calls.fetch_add(1, Ordering::SeqCst);
        self.asked
            .lock()
            .unwrap()
            .push((names.to_vec(), max_per_file));
        self.literal
            .clone()
            .map(|answer| answer.map_err(|error| anyhow!("{error}")))
    }
}

/// One line ripgrep reported: `name` is the index of the needle.
fn literal_line(path: &str, line: u32, text: &str, name: usize) -> LiteralMatch {
    LiteralMatch {
        relative_path: path.to_owned(),
        line,
        text: text.to_owned(),
        name,
    }
}

fn literal_found(matches: Vec<LiteralMatch>) -> LiteralSearch {
    LiteralSearch {
        matches,
        warning: None,
    }
}

/// A call with the question and search, over no tree.
fn bare_call<'a>(
    question: &'a str,
    search: Option<&'a dyn SeedSearch>,
    budget: &'a BudgetArgs,
    steer: &'a SteerArgs,
) -> Call<'a> {
    Call {
        scip_index: Path::new("unused"),
        scip_root: None,
        question,
        seeds: &[],
        seed_search: search,
        top_k: 8,
        maps: None,
        budget,
        steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    }
}

#[test]
fn literal_needles_are_the_last_segments_of_the_code_spans() {
    use super::seeds::literal_needles;
    assert_eq!(
        literal_needles("where is `Config::parse()` used, and `crate::shapes::Shape`, `x`?"),
        ["parse", "Shape"]
    );
    // Once each, in question order, at most four.
    assert_eq!(
        literal_needles("`a::Alpha` `Beta` `b::Alpha` `Gamma` `Delta` `Epsilon`"),
        ["Alpha", "Beta", "Gamma", "Delta"]
    );
    assert!(literal_needles("no code spans here, only Words").is_empty());
}

#[tokio::test]
async fn literal_seeds_resolve_definitions_then_occurrences_then_files() {
    use super::seeds::literal_seeds;
    use lci_maps::lsp_live::ScipPlaces;
    let docs = vec![
        RawDocument {
            path: "a.rs".into(),
            occurrences: vec![defined("parse", 9, (9, 15))],
        },
        RawDocument {
            path: "b.rs".into(),
            occurrences: vec![
                defined("main", 30, (30, 45)),
                defined("other", 50, (50, 60)),
            ],
        },
    ];
    let loaded = Loaded::new(
        (None, 0),
        ScipMap::with_default_ids(ScipGraph::build(&docs, None)),
    );
    let id_of = |name: &str| {
        let at = loaded.scip.graph().node_index(&symbol(name)).unwrap();
        loaded.scip.node_id(at).clone()
    };
    let places = ScipPlaces::from_scip(&loaded.scip);
    // ripgrep's order: by path, then line. `parse` is needle 0, `Shape` 1.
    let matches = vec![
        // A use inside `parse` before its definition line is reported.
        literal_line("a.rs", 13, "    parse(x)", 0),
        literal_line("a.rs", 10, "    pub fn parse() {}", 0),
        literal_line("b.rs", 40, "    let c = Config::parse();", 0),
        literal_line("b.rs", 55, "    parse(1);", 0),
        literal_line("b.rs", 56, "    parse(2);", 0),
        literal_line("shapes.rs", 3, "pub trait Shape {", 1),
        literal_line("shapes.rs", 9, "fn draw(shape: &dyn Shape) {}", 1),
        literal_line("shapes.rs", 12, "const DEFAULT: Shape = make();", 1),
        literal_line("shapes.rs", 20, "impl Draw for Shape {", 1),
        literal_line("notes.txt", 1, "// the trait Shape is documented", 1),
        literal_line("README.md", 5, "The trait Shape is documented here", 1),
    ];
    let stub = LiteralStub::new(Ok(Vec::new()), Some(Ok(literal_found(matches))));
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let question = "where is `Config::parse` used, and what is `Shape`?";
    let call = bare_call(question, Some(&stub), &budget, &steer);

    let (seeds, details, notes) = literal_seeds(&limits(), &call, &HashSet::new(), &places)
        .await
        .unwrap();
    assert_eq!(
        stub.asked.lock().unwrap().as_slice(),
        [(vec!["parse".to_owned(), "Shape".to_owned()], 20)]
    );
    let file = NodeId::file("shapes.rs").unwrap();
    // The definition, the occurrences (most lines first), the file once.
    let nodes: Vec<NodeId> = seeds.iter().map(|seed| seed.node.clone()).collect();
    assert_eq!(
        nodes,
        [id_of("parse"), id_of("other"), id_of("main"), file.clone()]
    );
    assert!(seeds.iter().all(|seed| seed.map == Some(SEEDS_LITERAL)));
    let hows: Vec<&str> = details.iter().map(|d| d["how"].as_str().unwrap()).collect();
    assert_eq!(hows, ["definition", "occurrence", "occurrence", "file"]);
    assert_eq!(details[0]["name"], "parse");
    assert_eq!(
        details[0]["line"], 10,
        "the definition line, not the earlier use"
    );
    assert_eq!(details[0]["score"], 2);
    assert_eq!(details[0]["kind"], "symbol");
    assert_eq!(details[0]["retrievers"], json!([SEEDS_LITERAL]));
    assert_eq!(details[0]["score_kind"], "occurrences");
    assert_eq!(details[1]["score"], 2, "two lines of `other`");
    assert_eq!(details[2]["score"], 1);
    assert_eq!(details[3]["kind"], "file");
    assert_eq!(details[3]["name"], "Shape");
    assert_eq!(details[3]["path"], "shapes.rs");
    assert_eq!(details[3]["line"], 3);
    // The signature, the constant, the comment and the prose file declare nothing: 4 dropped.
    let note = notes.join("\n");
    assert!(note.contains("searched parse, Shape"), "{note}");
    assert!(
        note.contains("4 start nodes (1 definition, 2 occurrence, 1 file)"),
        "{note}"
    );
    assert!(note.contains("4 lines were in no known symbol"), "{note}");

    // A node an earlier source gave is left out, and the list is cut at seed_k.
    let known: HashSet<NodeId> = [id_of("main")].into();
    let cut = TraverseConfig {
        seed_k: 2,
        ..limits()
    };
    let (seeds, _, _) = literal_seeds(&cut, &call, &known, &places).await.unwrap();
    let nodes: Vec<NodeId> = seeds.iter().map(|seed| seed.node.clone()).collect();
    assert_eq!(nodes, [id_of("parse"), id_of("other")]);

    // No ripgrep: a note, no seeds. A failed search: the same, with the error.
    let none = LiteralStub::new(Ok(Vec::new()), None);
    let failing = LiteralStub::new(Ok(Vec::new()), Some(Err("rg exploded".into())));
    for (stub, expect) in [
        (&none, "needs the application's ripgrep"),
        (&failing, "rg exploded"),
    ] {
        let call = bare_call(question, Some(stub), &budget, &steer);
        let (seeds, details, notes) = literal_seeds(&limits(), &call, &HashSet::new(), &places)
            .await
            .unwrap();
        assert!(seeds.is_empty() && details.is_empty());
        assert!(notes.join("\n").contains(expect), "{notes:?}");
    }
    // A question with no code span asks nothing.
    let quiet = bare_call("no names here", Some(&stub), &budget, &steer);
    let before = stub.literal_calls.load(Ordering::SeqCst);
    let (seeds, _, notes) = literal_seeds(&limits(), &quiet, &HashSet::new(), &places)
        .await
        .unwrap();
    assert!(seeds.is_empty());
    assert!(notes[0].contains("nothing in code spans"), "{notes:?}");
    assert_eq!(stub.literal_calls.load(Ordering::SeqCst), before);
}

/// One call over the `crowded_tree` index, under the run profile with
/// `[run_profile.seeds] literal` as `literal` says.
async fn literal_call(
    tree: &(tempfile::TempDir, PathBuf),
    literal: bool,
    maps: Option<&[&str]>,
    question: &str,
    search: Option<&dyn SeedSearch>,
) -> Value {
    let data = tempfile::tempdir().unwrap();
    let profile = lci_core::config::RunProfileConfig {
        seeds: lci_core::config::RunProfileSeeds {
            literal,
            ..Default::default()
        },
        ..Default::default()
    };
    let maps: Option<Vec<String>> = maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &tree.1,
        scip_root: Some(tree.0.path()),
        maps: maps.as_deref(),
        ..bare_call(question, search, &budget, &steer)
    };
    run(&limits(), &profile, &Dirs::under(data.path()), &call)
        .await
        .unwrap()
        .response
}

#[tokio::test]
async fn seeds_literal_is_inert_with_the_key_off_and_seeds_after_hybrid_with_it_on() {
    let tree = crowded_tree(2);
    let loaded = load(&tree.1, Some(tree.0.path()), ScipMode::Authoritative)
        .await
        .unwrap();
    let id_of = |name: &str| {
        let graph = loaded.scip.graph();
        let at = graph
            .node_index(&format!("rust-analyzer cargo demo 0.1.0 a/{name}()."))
            .unwrap();
        loaded.scip.node_id(at).to_string()
    };
    assert!(
        !lci_core::config::Config::default()
            .run_profile
            .seeds
            .literal,
        "off by default"
    );
    let question = "who calls `target`, and what is `Gadget`?";
    let matches = || {
        literal_found(vec![
            // The definition of `target` (line 1, the name is at line 0).
            literal_line("a.rs", 1, "fn target() {}", 0),
            // A trait the graph lacks.
            literal_line("gadget.rs", 4, "pub trait Gadget {", 1),
        ])
    };
    let hit_user1 = || Ok(vec![hit("a.rs", (2, 2), 1)]);

    // Key off, the map named: the note, and the answer is the one without it;
    // ripgrep is never asked.
    let stub = LiteralStub::new(hit_user1(), Some(Ok(matches())));
    let named = Some(&["seeds.literal", "graph.scip"][..]);
    let mut off = literal_call(&tree, false, named, question, Some(&stub)).await;
    let mut without =
        literal_call(&tree, false, Some(&["graph.scip"]), question, Some(&stub)).await;
    let notes = off["notes"].to_string();
    assert!(
        notes.contains("seeds.literal is off ([run_profile.seeds] literal = false)"),
        "{notes}"
    );
    assert_eq!(off["seed_source"], without["seed_source"]);
    off.as_object_mut().unwrap().remove("notes");
    without.as_object_mut().unwrap().remove("notes");
    assert_eq!(off, without);

    // Key off and the default maps with a hybrid hit: as before the key.
    let plain = literal_call(&tree, false, None, question, Some(&stub)).await;
    assert_eq!(plain["seed_source"], "hybrid");
    assert_eq!(plain["seeds"], json!([id_of("user1")]));
    assert!(!plain.to_string().contains("literal"), "{plain}");
    assert_eq!(stub.literal_calls.load(Ordering::SeqCst), 0);
    assert!(!loaded.places_built());

    // Key on, no maps named: the map is in the default set, after the hybrid
    // nodes. The definition of `target` is a symbol, `Gadget` a file.
    let on = literal_call(&tree, true, None, question, Some(&stub)).await;
    assert_eq!(stub.literal_calls.load(Ordering::SeqCst), 1);
    assert_eq!(on["seed_source"], "hybrid+literal");
    let file = NodeId::file("gadget.rs").unwrap().to_string();
    assert_eq!(
        on["seeds"],
        json!([id_of("user1"), id_of("target"), file.clone()])
    );
    assert_eq!(on["seed_hits"][1]["how"], "definition");
    assert_eq!(on["seed_hits"][1]["name"], "target");
    assert_eq!(on["seed_hits"][2]["how"], "file");
    assert_eq!(
        on["seed_hits"][0]["retrievers"],
        plain["seed_hits"][0]["retrievers"]
    );
    assert_eq!(
        stub.asked.lock().unwrap().as_slice(),
        [(vec!["target".to_owned(), "Gadget".to_owned()], 20)]
    );
    // The file start node has a site, at the line that declares the name.
    assert_eq!(
        on["seed_sites"][&file],
        json!({"path": "gadget.rs", "line": 4, "col": 1}),
        "{on}"
    );

    // Key on and the hybrid search down: the literal nodes are the start nodes.
    let down = LiteralStub::new(Err("the embedder is down".into()), Some(Ok(matches())));
    let stand_in = literal_call(&tree, true, None, question, Some(&down)).await;
    assert_eq!(stand_in["seed_source"], "literal");
    assert_eq!(stand_in["seeds"], json!([id_of("target"), file]));
    assert!(
        stand_in["seed_fallback"]
            .as_str()
            .unwrap()
            .contains("embedder is down")
    );

    // Key on, a search that cannot run ripgrep: a note, the hybrid seeds stay.
    let no_rg = LiteralStub::new(hit_user1(), None);
    let failed_open = literal_call(&tree, true, None, question, Some(&no_rg)).await;
    assert_eq!(failed_open["seed_source"], "hybrid");
    assert!(
        failed_open["notes"]
            .to_string()
            .contains("seeds.literal needs the application's ripgrep"),
        "{failed_open}"
    );
    let no_app = NoSearch("no workspace".into());
    let failed_open = literal_call(&tree, true, None, question, Some(&no_app)).await;
    assert!(
        failed_open["notes"]
            .to_string()
            .contains("seeds.literal needs the application's ripgrep"),
        "{failed_open}"
    );
}

#[test]
fn declares_reads_the_word_before_the_name_across_languages() {
    use super::seeds::declares;
    for (line, name) in [
        ("pub trait Shape {", "Shape"),
        ("impl Draw for Shape {", "Shape"),
        ("export function Render() {", "Render"),
        ("object Registry {", "Registry"),
        ("public delegate void Handler();", "Handler"),
        ("func (s *Server) Start() error {", "Start"),
        ("func (Server) Start() {", "Start"),
        ("class Circle : Shape {", "Circle"),
    ] {
        assert!(declares(line, name), "{line}");
    }
    for (line, name) in [
        ("fn draw(shape: &dyn Shape) {}", "Shape"),
        ("const DEFAULT: Shape = make();", "Shape"),
        ("// trait Shape", "Shape"),
        ("class Circle : Shape {", "Shape"),
        ("func start(s Server) Name {", "Name"),
        ("trait ShapeKind {", "Shape"),
    ] {
        assert!(!declares(line, name), "{line}");
    }
}

// ---- `[run_profile.traverse] answer_labels` (slice K13) ----------------------------

/// One call over `tree` under `profile`, a budget of `pops`, the given maps and
/// a question that names `Foo` and asks for the commit that created it.
async fn labelled_run(
    tree: &ScipTree,
    limits: &TraverseConfig,
    profile: &RunProfileConfig,
    pops: u32,
    maps: &[&str],
) -> Value {
    labelled_run_under(tree, limits, profile, pops, maps, None).await
}

/// [`labelled_run`] with the language-server settings of the call.
async fn labelled_run_under(
    tree: &ScipTree,
    limits: &TraverseConfig,
    profile: &RunProfileConfig,
    pops: u32,
    maps: &[&str],
    lsp: Option<&lci_core::config::Config>,
) -> Value {
    let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
    let (budget, steer) = (
        BudgetArgs {
            pops: Some(pops),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "Which commit first created `Foo`?",
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp,
        lsp_sessions: None,
    };
    run(limits, profile, &Dirs::under(tree.dir.path()), &call)
        .await
        .unwrap()
        .response
}

/// A profile with the four keys that K13 reads, as given.
fn label_profile(
    answer_labels: bool,
    map_freshness: bool,
    verify_on_read: bool,
    card_paths: bool,
) -> RunProfileConfig {
    let mut profile = RunProfileConfig::default();
    profile.traverse.answer_labels = answer_labels;
    profile.traverse.map_freshness = map_freshness;
    profile.traverse.card_paths = card_paths;
    profile.freshness.verify_on_read = verify_on_read;
    profile
}

/// The lists of a response that hold cards.
const CARD_POINTERS: [&str; 4] = [
    "/results",
    "/checkpoint/answer",
    "/checkpoint/visited_cards",
    "/checkpoint/frontier",
];

/// The cards of every list that holds cards.
fn all_cards(response: &Value) -> Vec<&Value> {
    CARD_POINTERS
        .iter()
        .filter_map(|list| response.pointer(list).and_then(Value::as_array))
        .flatten()
        .collect()
}

/// `response` without the K13 labels (no `completeness`, no card `freshness`,
/// not the one `notes` line a partial answer gets) and without its wall clock.
fn without_labels(response: &Value) -> Value {
    let mut stripped = response.clone();
    stripped.as_object_mut().unwrap().remove("completeness");
    for list in CARD_POINTERS {
        if let Some(Value::Array(cards)) = stripped.pointer_mut(list) {
            for card in cards {
                card.as_object_mut().unwrap().remove("freshness");
            }
        }
    }
    if let Some(notes) = stripped.get_mut("notes").and_then(Value::as_array_mut) {
        notes.retain(|note| {
            let note = note.as_str().unwrap();
            !note.starts_with("partial answer (") && !note.starts_with("degraded maps: ")
        });
    }
    stripped
}

fn sorted_keys(card: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = card
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The keys a walked card of `results` has with every K13 key on (`site` and
/// `commit` come and go with what the source knows, and are left out).
const RESULT_KEYS: [&str; 17] = [
    "depth",
    "fanout",
    "file_changed",
    "freshness",
    "kind",
    "maps",
    "n_maps",
    "node",
    "node_kind",
    "origin",
    "overlap",
    "path",
    "potential",
    "ranked_potential",
    "text",
    "via",
    "witness",
];

/// The same for a card of the checkpoint's `answer`: no tie-break facts, no
/// origin.
const CHECKPOINT_KEYS: [&str; 13] = [
    "depth",
    "file_changed",
    "freshness",
    "kind",
    "maps",
    "n_maps",
    "node",
    "path",
    "potential",
    "ranked_potential",
    "text",
    "via",
    "witness",
];

#[tokio::test]
async fn the_card_schema_is_closed_and_the_labels_are_absent_with_the_key_off() {
    let tree = ScipTree::new();
    tree.commit();
    let maps = ["graph.scip", EXACT_CLOSURE_ID, EXACT_PICKAXE_ID];
    let plain = limits();
    let limits = stage_limits(true, false);
    let on = labelled_run(
        &tree,
        &limits,
        &label_profile(true, true, true, true),
        1,
        &maps,
    )
    .await;
    assert_eq!(on["status"], "budget_exhausted", "{on}");

    // The keys of a card are closed.
    let closed = |keys: &[&str], card: &Value| {
        let mut found = sorted_keys(card);
        found.retain(|key| !["site", "commit"].contains(key));
        // A first-stage card (origin `exact`, `seed`) has `name` and `held`
        // where a walked card has the tie-break facts and the path.
        if card["origin"] != "walk" && card.get("origin").is_some() {
            found.retain(|key| !["name", "held"].contains(key));
            let core: Vec<&str> = keys
                .iter()
                .copied()
                .filter(|key| !["fanout", "kind", "node_kind", "overlap", "path"].contains(key))
                .collect();
            assert_eq!(found, core, "{card}");
        } else {
            assert_eq!(found, keys, "{card}");
        }
    };
    let results = on["results"].as_array().unwrap();
    assert!(results.len() >= 2, "{on}");
    for card in results {
        closed(&RESULT_KEYS, card);
    }
    // A walk without a first stage: its cards have no `origin`, and the
    // checkpoint's `answer` has cards.
    let walked = PathsWorld::new()
        .await
        .call(
            &label_profile(true, true, true, true),
            &plain,
            Some(2),
            None,
        )
        .await
        .response;
    let no_origin: Vec<&str> = RESULT_KEYS
        .iter()
        .copied()
        .filter(|key| *key != "origin")
        .collect();
    let walked_results = walked["results"].as_array().unwrap();
    assert!(!walked_results.is_empty(), "{walked}");
    for card in walked_results {
        closed(&no_origin, card);
    }
    let answer = walked["checkpoint"]["answer"].as_array().unwrap();
    assert!(!answer.is_empty(), "{walked}");
    for card in answer {
        closed(&CHECKPOINT_KEYS, card);
    }
    for response in [&on, &walked] {
        for card in all_cards(response) {
            let word = card["freshness"]
                .as_str()
                .unwrap_or_else(|| panic!("{card}"));
            assert!(super::response::FRESHNESS_WORDS.contains(&word), "{card}");
        }
    }
    assert_eq!(
        sorted_keys(&on["completeness"]),
        ["complete", "estimate", "why"],
        "{on}"
    );

    // With the key off and the other three on: neither label, and the answer is
    // the labelled one without them.
    let off = labelled_run(
        &tree,
        &limits,
        &label_profile(false, true, true, true),
        1,
        &maps,
    )
    .await;
    let text = off.to_string();
    assert!(
        !text.contains("\"freshness\"") && !text.contains("\"completeness\""),
        "{off}"
    );
    assert!(!text.contains("partial answer"), "{off}");
    assert_eq!(off["trace"]["hash"], on["trace"]["hash"]);
    assert_eq!(off["handle"], on["handle"]);
    let clock = |mut response: Value| {
        for key in ["totals", "trace"] {
            if let Some(object) = response[key].as_object_mut() {
                object.retain(|name, _| !name.ends_with("_ms") && name != "file");
            }
        }
        if let Some(per_map) = response["totals"]["per_map"].as_object_mut() {
            per_map.values_mut().for_each(|stat| {
                stat.as_object_mut().map(|stat| stat.remove("ms"));
            });
        }
        if let Some(notes) = response["notes"].as_array_mut() {
            notes.retain(|note| !note.as_str().is_some_and(|n| n.contains("expand p50")));
        }
        response
    };
    let (on_plain, off_plain) = (clock(without_labels(&on)), clock(off));
    for (key, value) in on_plain.as_object().unwrap() {
        assert_eq!(Some(value), off_plain.get(key), "{key}");
    }
    assert_eq!(on_plain, off_plain);
}

#[tokio::test]
async fn a_capped_walk_is_labelled_partial_and_keeps_its_results() {
    let tree = ScipTree::new();
    let maps = ["graph.scip", EXACT_CLOSURE_ID];
    let limits = stage_limits(true, false);
    let on = labelled_run(
        &tree,
        &limits,
        &label_profile(true, false, false, false),
        1,
        &maps,
    )
    .await;
    assert_ne!(on["status"], "complete", "{on}");
    assert_eq!(on["completeness"]["complete"], false, "{on}");
    assert_eq!(on["completeness"]["why"], "pops", "{on}");
    let estimate = &on["completeness"]["estimate"];
    assert!(
        estimate.is_null() || (0.0..=1.0).contains(&estimate.as_f64().unwrap()),
        "{estimate}"
    );
    let handle = on["handle"].as_str().expect("a capped walk has a handle");
    let notes = on["notes"].to_string();
    assert!(
        notes.contains("partial answer (pops): results are what the walk found so far"),
        "{notes}"
    );
    assert!(notes.contains(handle), "{notes}");

    // Nothing is withheld: the results are the key-off call's, card for card.
    let off = labelled_run(
        &tree,
        &limits,
        &label_profile(false, false, false, false),
        1,
        &maps,
    )
    .await;
    assert!(!on["results"].as_array().unwrap().is_empty(), "{on}");
    assert_eq!(without_labels(&on)["results"], off["results"]);
    assert_eq!(on["status"], off["status"]);
}

#[tokio::test]
async fn a_complete_walk_is_labelled_complete_with_no_note() {
    let tree = ScipTree::new();
    let maps = ["graph.scip", EXACT_CLOSURE_ID];
    {
        let limits = stage_limits(true, false);
        let on = labelled_run(
            &tree,
            &limits,
            &label_profile(true, false, false, false),
            50,
            &maps,
        )
        .await;
        assert_eq!(on["status"], "complete", "{on}");
        assert_eq!(on["completeness"]["complete"], true, "{on}");
        // `complete`, or `bound` when the score bound ended the walk early.
        let why = on["completeness"]["why"].as_str().unwrap();
        assert!(["complete", "bound"].contains(&why), "{on}");
        assert_eq!(on["stop_reason"], why);
        assert!(!on.to_string().contains("partial answer"), "{on}");
        assert!(on.get("checkpoint").is_none(), "{on}");
    }
}

#[tokio::test]
async fn an_answer_with_no_start_node_is_labelled_partial() {
    let tree = ScipTree::new();
    let maps = ["graph.scip", EXACT_CLOSURE_ID];
    // Nothing the walk can start from: the first stage is the answer.
    let limits = stage_limits(false, false);
    let on = labelled_run(
        &tree,
        &limits,
        &label_profile(true, true, true, false),
        3,
        &maps,
    )
    .await;
    assert_eq!(on["status"], "no_seeds", "{on}");
    assert_eq!(
        on["completeness"],
        json!({"complete": false, "why": "no_seeds", "estimate": null}),
        "{on}"
    );
    assert!(
        on["notes"]
            .to_string()
            .contains("partial answer (no_seeds)"),
        "{on}"
    );
    // Nothing is withheld, and its cards say what is known of their file.
    assert!(!on["results"].as_array().unwrap().is_empty(), "{on}");
    for card in all_cards(&on) {
        assert_eq!(card["freshness"], "fresh", "{card}");
    }
    let off = labelled_run(
        &tree,
        &limits,
        &label_profile(false, true, true, false),
        3,
        &maps,
    )
    .await;
    assert_eq!(off["results"], without_labels(&on)["results"]);
    assert!(!off.to_string().contains("completeness"), "{off}");
}

#[tokio::test]
async fn cards_say_fresh_changed_stale_or_unverified() {
    let tree = ScipTree::new();
    tree.commit();
    let maps = ["graph.scip", EXACT_CLOSURE_ID, EXACT_PICKAXE_ID];
    let limits = stage_limits(true, false);
    // Every card of the lists whose node passes `scope` says `want`.
    let only = |response: &Value, want: &str, scope: &dyn Fn(&str) -> bool| {
        let seen: Vec<(&str, &str)> = all_cards(response)
            .into_iter()
            .map(|card| {
                (
                    card["node"].as_str().unwrap(),
                    card["freshness"].as_str().unwrap(),
                )
            })
            .filter(|(node, _)| scope(node))
            .collect();
        assert!(!seen.is_empty(), "{response}");
        assert!(
            seen.iter().all(|(_, word)| *word == want),
            "{want}: {seen:?} in {response}"
        );
    };
    let any = |_: &str| true;
    let is_commit = |node: &str| node.starts_with("commit:");
    let is_file = |node: &str| !node.starts_with("commit:");

    // (a) The label alone cannot know: nothing was checked.
    let alone = labelled_run(
        &tree,
        &limits,
        &label_profile(true, false, false, false),
        3,
        &maps,
    )
    .await;
    assert!(
        alone.get("maps").is_none() && !alone.to_string().contains("file_changed"),
        "{alone}"
    );
    only(&alone, "unverified", &any);

    // (b) The tree is as the index saw it: the cards of the maps built from the
    // index are fresh, and so is the commit (its map is live).
    let seen = labelled_run(
        &tree,
        &limits,
        &label_profile(true, true, false, false),
        3,
        &maps,
    )
    .await;
    only(&seen, "fresh", &any);
    // (d) The commit card says `fresh` under `map_freshness`, `unverified` without it.
    only(&seen, "fresh", &is_commit);
    only(&alone, "unverified", &is_commit);

    // (c) After an edit: with `verify_on_read` the cards of `a.rs` are
    // `changed`; with `map_freshness` alone their maps are `stale`. The commit
    // stays fresh.
    let source = tree.dir.path().join("a.rs");
    let mut text = std::fs::read_to_string(&source).unwrap();
    text.push_str("fn appended() {}\n");
    std::fs::write(&source, text).unwrap();
    let verified = labelled_run(
        &tree,
        &limits,
        &label_profile(true, true, true, false),
        3,
        &maps,
    )
    .await;
    only(&verified, "changed", &is_file);
    only(&verified, "fresh", &is_commit);
    let stale = labelled_run(
        &tree,
        &limits,
        &label_profile(true, true, false, false),
        3,
        &maps,
    )
    .await;
    only(&stale, "stale", &is_file);
    only(&stale, "fresh", &is_commit);
    // `verify_on_read` is read through `map_freshness`: alone it checks nothing,
    // and the cards say so.
    let files = labelled_run(
        &tree,
        &limits,
        &label_profile(true, false, true, false),
        3,
        &maps,
    )
    .await;
    assert!(!files.to_string().contains("file_changed"), "{files}");
    only(&files, "unverified", &any);
}

#[tokio::test]
async fn a_walk_that_lost_a_map_is_not_labelled_complete() {
    let tree = ScipTree::new();
    // `lsp.live` is asked for, and its server is not there: the walk still runs
    // to the end on the other maps.
    let lsp = lci_core::config::Config {
        rust_analyzer_path: "lci-no-such-language-server".into(),
        ..Default::default()
    };
    let maps = ["graph.scip", EXACT_CLOSURE_ID, LSP_LIVE_ID];
    let limits = stage_limits(true, false);
    let run_with = |labels: bool, pops: u32| {
        let (tree, limits, maps, lsp) = (&tree, &limits, &maps, &lsp);
        async move {
            labelled_run_under(
                tree,
                limits,
                &label_profile(labels, false, false, false),
                pops,
                maps,
                Some(lsp),
            )
            .await
        }
    };
    let on = run_with(true, 50).await;
    assert_eq!(on["status"], "complete", "{on}");
    assert!(
        on["notes"].to_string().contains("lsp.live is unavailable"),
        "{on}"
    );
    assert_eq!(
        on["completeness"],
        json!({"complete": false, "why": "degraded", "degraded": ["lsp.live"], "estimate": on["completeness"]["estimate"]}),
        "{on}"
    );
    let notes = on["notes"].to_string();
    assert!(
        notes.contains("partial answer (degraded): lsp.live"),
        "{notes}"
    );
    // Nothing is withheld, and the key off says nothing of it.
    let off = run_with(false, 50).await;
    assert!(!on["results"].as_array().unwrap().is_empty(), "{on}");
    assert_eq!(without_labels(&on)["results"], off["results"]);
    assert!(!off.to_string().contains("completeness"), "{off}");
    assert!(!off.to_string().contains("degraded"), "{off}");

    // A capped walk keeps its own reason and still names the lost map.
    let capped = run_with(true, 1).await;
    assert_eq!(capped["completeness"]["why"], "pops", "{capped}");
    assert_eq!(capped["completeness"]["complete"], false, "{capped}");
    assert_eq!(
        capped["completeness"]["degraded"],
        json!(["lsp.live"]),
        "{capped}"
    );
    assert!(
        capped["notes"]
            .to_string()
            .contains("degraded maps: lsp.live"),
        "{capped}"
    );

    // Every map of the call is there: no `degraded` key.
    let sound = labelled_run(
        &tree,
        &limits,
        &label_profile(true, false, false, false),
        50,
        &["graph.scip", EXACT_CLOSURE_ID],
    )
    .await;
    assert_eq!(sound["completeness"]["complete"], true, "{sound}");
    assert!(sound["completeness"].get("degraded").is_none(), "{sound}");
}

#[tokio::test]
async fn naming_tests_tested_by_registers_it_and_the_default_set_does_not() {
    assert!(OPT_IN_MAPS.contains(&TESTED_BY_ID) && !MAPS.contains(&TESTED_BY_ID));
    let tree = Tree::new();
    let listed = |response: &Value| -> bool {
        response["effective_policy"]["maps"]
            .as_array()
            .unwrap_or_else(|| panic!("no maps in {response}"))
            .iter()
            .any(|map| map["id"] == TESTED_BY_ID && map["enabled"] == true)
    };
    let named = first_stage_run(&tree, &limits(), Some(&["graph.scip", TESTED_BY_ID])).await;
    assert!(listed(&named), "{named}");
    // A plain call (the default set) does not register it.
    let plain = first_stage_run(&tree, &limits(), None).await;
    assert!(!listed(&plain), "{plain}");
    // Naming another opt-in map does not turn it on either.
    let other = first_stage_run(&tree, &limits(), Some(&["graph.scip", SCIP_IMPACT_ID])).await;
    assert!(!listed(&other), "{other}");
}

#[tokio::test]
async fn naming_docs_mentions_registers_it_and_the_default_set_does_not() {
    assert!(OPT_IN_MAPS.contains(&DOCS_MENTIONS_ID) && !MAPS.contains(&DOCS_MENTIONS_ID));
    let tree = Tree::new();
    let listed = |response: &Value| -> bool {
        response["effective_policy"]["maps"]
            .as_array()
            .unwrap_or_else(|| panic!("no maps in {response}"))
            .iter()
            .any(|map| map["id"] == DOCS_MENTIONS_ID && map["enabled"] == true)
    };
    let named = first_stage_run(&tree, &limits(), Some(&["graph.scip", DOCS_MENTIONS_ID])).await;
    assert!(listed(&named), "{named}");
    // A plain call (the default set) does not register it.
    let plain = first_stage_run(&tree, &limits(), None).await;
    assert!(!listed(&plain), "{plain}");
    // Naming another opt-in map does not turn it on either.
    let other = first_stage_run(&tree, &limits(), Some(&["graph.scip", SCIP_IMPACT_ID])).await;
    assert!(!listed(&other), "{other}");
}
