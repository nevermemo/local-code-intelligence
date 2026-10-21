//! The JSON a traversal call answers with: the policy, the start nodes, the
//! reached nodes, the call sites, the commits and the checkpoint.

use super::first_stage::{FirstStage, merge, origin_counts};
use super::{Call, MAPS, OPT_IN_MAPS, SteerArgs, freshness::Observer, index::Loaded, seeds::Seeds};
use crate::config::TraverseFirstStage;
use lci_core::node::{NodeId, NodeKind};
use lci_maps::{
    MapId,
    git_history::GitHistoryMap,
    scip_impact::ImpactMap,
    traverse::{
        State, TraversalBudget, TraversalResult,
        handler::{Action, Checkpoint, RAISE_MAX_POPS, RAISE_MAX_TOKENS, StopPolicy},
        lint::{LintContext, lint},
        rules::RuleList,
        steer::Steer,
        stop::{Card, MapStat},
        trace::{self},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    str::FromStr,
};

/// `effective_policy` (every response) and `warnings` (when there are any).
pub(super) fn add_policy(response: &mut Value, effective: Value, warnings: Vec<String>) {
    if let Some(object) = response.as_object_mut() {
        object.insert("effective_policy".into(), effective);
        if !warnings.is_empty() {
            object.insert("warnings".into(), json!(warnings));
        }
    }
}

/// `maps` (only with `[run_profile.traverse] map_freshness`; `None` otherwise leaves the
/// response as it was): for each map of the call that states it, `id`,
/// `observed` (`snapshot`, `live` or `as_of:<point>`), `stale` and `lag`
/// (see [`super::freshness`]).
pub(super) fn add_maps(response: &mut Value, maps: Option<Value>) {
    if let (Some(maps), Some(object)) = (maps, response.as_object_mut()) {
        object.insert("maps".into(), maps);
    }
}

/// `file_changed` on every card (only with `[run_profile.freshness]
/// verify_on_read`; otherwise the response is left as it was): `true` when the
/// content hash of the file the card cites now differs from the one its maps
/// read, `false` when it does not, `null` when no reference applies (see
/// [`super::freshness`]). One note says how many cards are `null`.
pub(super) fn add_file_changed(response: &mut Value, observer: &Observer) {
    if !observer.verifies() {
        return;
    }
    let mut unknown = 0usize;
    for list in CARD_LISTS {
        let Some(Value::Array(items)) = response.pointer_mut(list) else {
            continue;
        };
        for card in items {
            let verdict = observer.file_changed(card);
            unknown += usize::from(verdict.is_none());
            if let Some(object) = card.as_object_mut() {
                object.insert("file_changed".into(), json!(verdict));
            }
        }
    }
    if unknown == 0 {
        return;
    }
    let note = format!(
        "verify_on_read: file_changed is null on {unknown} cards: no source file to compare (a commit, a file that is not a source file, or no tree was read)"
    );
    match response.get_mut("notes").and_then(Value::as_array_mut) {
        Some(notes) => notes.push(json!(note)),
        None => {
            if let Some(object) = response.as_object_mut() {
                object.insert("notes".into(), json!([note]));
            }
        }
    }
}

/// The words `freshness` says on a card (`[run_profile.traverse]
/// answer_labels`).
pub(super) const FRESHNESS_WORDS: [&str; 4] = ["fresh", "changed", "stale", "unverified"];

/// What one card's `freshness` is, from the card's `file_changed` and from the
/// answer's `maps` (`None` when the answer has no `maps` key), in this order:
/// `file_changed` true is `changed`, false is `fresh`; otherwise, when the
/// answer states its maps, any map of the card (`maps` plus `via`) that is
/// `stale` makes it `stale` and all of them found with a lag make it `fresh`;
/// anything else is `unverified`.
///
/// `kept` names the maps whose evidence this walk took from a server it found
/// running (a session kept across calls: `lsp.live`, see
/// `lci_core::lsp::WalkSessions`). Nothing checks what such a server read, so
/// a card that cites one is never `fresh`: it is `unverified` (a `changed` or
/// `stale` verdict stands, being a positive finding).
fn card_freshness(card: &Value, maps: Option<&Vec<Value>>, kept: &[MapId]) -> &'static str {
    let mut named: Vec<&str> = card["maps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    named.extend(card["via"].as_str());
    let from_kept = named.iter().any(|id| kept.contains(id));
    match card.get("file_changed").and_then(Value::as_bool) {
        Some(true) => return FRESHNESS_WORDS[1],
        Some(false) if from_kept => return FRESHNESS_WORDS[3],
        Some(false) => return FRESHNESS_WORDS[0],
        None => {}
    }
    let Some(maps) = maps else {
        return FRESHNESS_WORDS[3];
    };
    if named.is_empty() {
        return FRESHNESS_WORDS[3];
    }
    let mut verified = !from_kept;
    for id in named {
        match maps.iter().find(|entry| entry["id"].as_str() == Some(id)) {
            Some(entry) if entry["stale"].as_bool() == Some(true) => return FRESHNESS_WORDS[2],
            Some(entry) if !entry["lag"].is_null() => {}
            _ => verified = false,
        }
    }
    if verified {
        FRESHNESS_WORDS[0]
    } else {
        FRESHNESS_WORDS[3]
    }
}

/// The maps an answer lacks: those the call asked for and could not set up
/// (`unavailable`) and those with failed calls (`MapStat.failures`), sorted.
fn degraded_maps(unavailable: &[MapId], stats: Option<&BTreeMap<MapId, MapStat>>) -> Vec<MapId> {
    let mut degraded = unavailable.to_vec();
    degraded.extend(
        stats
            .into_iter()
            .flatten()
            .filter(|(_, stat)| stat.failures > 0)
            .map(|(id, _)| *id),
    );
    degraded.sort_unstable();
    degraded.dedup();
    degraded
}

/// With `[run_profile.traverse] answer_labels`, the two labels of an answer
/// (the key off never calls this, so the response is as it was): `completeness`
/// `{complete, why, estimate}` on the answer (`complete` is `status ==
/// "complete"` and no map degraded, `why` the answer's `stop_reason`,
/// `no_seeds` without a walk, `degraded` when the walk ended complete but a map
/// the policy asked for was unavailable at setup (`unavailable`) or had failed
/// calls (`MapStat.failures`); those maps are then listed in `degraded`, also
/// beside another `why`; `estimate` the K29 fraction or `null`; a partial
/// answer also gets one note) and `freshness` on every card of [`CARD_LISTS`]. Read from the response as
/// it is, so it runs after `add_maps` and `add_file_changed`; the trace and the
/// checkpoint hash are made before it and do not see it.
pub(super) fn add_answer_labels(
    response: &mut Value,
    result: Option<&TraversalResult>,
    unavailable: &[MapId],
    kept: &[MapId],
    restored: &HashSet<String>,
) {
    let maps = response.get("maps").and_then(Value::as_array).cloned();
    for list in CARD_LISTS {
        let Some(Value::Array(items)) = response.pointer_mut(list) else {
            continue;
        };
        for card in items {
            // A card of a node restored from a checkpoint keeps what an
            // earlier call gathered, and the checkpoint does not say whether
            // that call used a kept server: such a card that cites `lsp.live`
            // is never `fresh` (a conservative choice that leaves the
            // checkpoint format alone). The cards the call's own walk found
            // follow `kept`.
            let restored_card = card["node"]
                .as_str()
                .is_some_and(|node| restored.contains(node));
            let word = if restored_card {
                card_freshness(card, maps.as_ref(), &[lci_maps::lsp_live::LSP_LIVE_ID])
            } else {
                card_freshness(card, maps.as_ref(), kept)
            };
            if let Some(object) = card.as_object_mut() {
                object.insert("freshness".into(), json!(word));
            }
        }
    }
    let walked_to_the_end = response["status"].as_str() == Some("complete");
    let degraded = degraded_maps(unavailable, result.map(|r| &r.maps));
    let complete = walked_to_the_end && degraded.is_empty();
    let why = if walked_to_the_end && !degraded.is_empty() {
        "degraded".to_owned()
    } else {
        response["stop_reason"]
            .as_str()
            .unwrap_or("no_seeds")
            .to_owned()
    };
    let estimate = result
        .and_then(|r| r.estimate.as_ref())
        .map(|lc| lc.fraction);
    let handle = response["handle"].as_str().map(str::to_owned);
    let Some(object) = response.as_object_mut() else {
        return;
    };
    let mut completeness = json!({"complete": complete, "why": why, "estimate": estimate});
    if !degraded.is_empty() {
        completeness["degraded"] = json!(degraded);
    }
    object.insert("completeness".into(), completeness);
    if complete {
        return;
    }
    let lost = degraded.join(", ");
    let mut lines = Vec::new();
    if walked_to_the_end {
        // The frontier dried up, but not with every map: nothing more to resume.
        lines.push(format!(
            "partial answer (degraded): {lost} was unavailable or had failed calls, so the results lack what it would add"
        ));
    } else {
        let mut note = format!("partial answer ({why}): results are what the walk found so far");
        if let Some(handle) = handle {
            note.push_str(&format!("; resume with handle {handle}"));
        }
        lines.push(note);
        if !degraded.is_empty() {
            lines.push(format!(
                "degraded maps: {lost} was unavailable or had failed calls, so the results lack what it would add"
            ));
        }
    }
    match object.get_mut("notes").and_then(Value::as_array_mut) {
        Some(notes) => notes.extend(lines.into_iter().map(Value::from)),
        None => {
            object.insert("notes".into(), json!(lines));
        }
    }
}

/// The lists of a response whose items are cards.
const CARD_LISTS: [&str; 4] = [
    "/results",
    "/checkpoint/answer",
    "/checkpoint/visited_cards",
    "/checkpoint/frontier",
];

/// The steer lint of a call (see [`lci_maps::traverse::lint`]), plus the two
/// checks that need the steer's own fields: a ladder that omits `rules` while
/// rules are given, and `budget` actions that add more than the engine
/// allows in total. The lint reads the call; nothing it finds stops the run.
pub(super) fn lint_steer(
    steer: &SteerArgs,
    stops: Option<&StopPolicy>,
    actions: Option<&Steer>,
    budget: &TraversalBudget,
    selected: &[MapId],
) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(stops) = stops {
        let known: Vec<&str> = MAPS.iter().chain(&OPT_IN_MAPS).copied().collect();
        let list = steer
            .rules
            .as_ref()
            .and_then(|r| RuleList::from_json(r).ok());
        let cx = LintContext {
            pops: budget.pops,
            max_depth: budget.max_depth,
            known_maps: &known,
            selected_maps: selected,
        };
        found.extend(lint(
            list.as_ref().map_or(&[][..], RuleList::rules),
            stops,
            &cx,
        ));
    }
    if steer.rules.is_some()
        && steer
            .ladder
            .as_ref()
            .is_some_and(|ladder| !ladder.iter().any(|rung| rung == "rules"))
    {
        found.push(
            "steer.ladder lists no `rules` rung while steer.rules is given: the rules rung runs first regardless"
                .to_owned(),
        );
    }
    let (pops, tokens) = actions.map_or((0, 0), |steer| {
        steer
            .actions
            .iter()
            .fold((0u64, 0u64), |(pops, tokens), directive| {
                match &directive.action {
                    Action::Budget { pops: p, tokens: t } => (pops + u64::from(*p), tokens + t),
                    _ => (pops, tokens),
                }
            })
    });
    if pops > u64::from(RAISE_MAX_POPS) || tokens > RAISE_MAX_TOKENS {
        found.push(format!(
            "steer.actions raise the budget by {pops} pops and {tokens} tokens; the most a traversal accepts is {RAISE_MAX_POPS} pops and {RAISE_MAX_TOKENS} tokens in total, and the cap applies"
        ));
    }
    found
}

/// The lines of the chunk start nodes (1-based, inclusive), which a chunk's
/// node id does not carry, for `structure.contains`.
pub(super) fn chunk_lines(found: &Seeds) -> Vec<(NodeId, (u32, u32))> {
    found
        .nodes
        .iter()
        .zip(&found.details)
        .filter(|(seed, _)| seed.node.kind() == NodeKind::Chunk)
        .filter_map(|(seed, info)| {
            let line = |i: usize| u32::try_from(info["lines"][i].as_u64()?).ok();
            Some((seed.node.clone(), (line(0)?, line(1)?)))
        })
        .collect()
}

/// Gives every commit card the subject (first line) and date of its commit.
pub(super) fn add_commits(response: &mut Value, history: &GitHistoryMap) {
    for list in ["/results", "/checkpoint/answer"] {
        let Some(Value::Array(items)) = response.pointer_mut(list) else {
            continue;
        };
        for card in items {
            let commit = card["node"]
                .as_str()
                .and_then(|node| NodeId::from_str(node).ok())
                .and_then(|node| match node {
                    NodeId::Commit { sha } => history.commit_node(&sha),
                    _ => None,
                });
            if let (Some(commit), Some(object)) = (commit, card.as_object_mut()) {
                object.insert(
                    "commit".into(),
                    json!({"subject": commit.subject, "date": commit.date}),
                );
            }
        }
    }
}

pub(super) fn no_seeds_response(call: &Call<'_>, found: &Seeds, notes: Vec<String>) -> Value {
    let mut response = json!({
        "mode": "traverse",
        "status": "no_seeds",
        "stop_reason": "no_seeds",
        "question": call.question,
        "seeds": [],
        "seed_source": found.source,
        "results": [],
        "notes": notes_with(
            notes,
            "no start node: the hybrid search and the symbol names in the question found nothing in the SCIP graph, and no seeds were given; name a function or method the code defines",
        ),
    });
    if let (Some(reason), Some(object)) = (&found.fallback, response.as_object_mut()) {
        object.insert("seed_fallback".into(), json!(reason));
    }
    response
}

fn notes_with(mut notes: Vec<String>, note: &str) -> Vec<String> {
    notes.push(note.to_owned());
    notes
}

/// Gives a response that has no walk (no start node) the first-stage cards as
/// its results, and the report of the exact operators.
pub(super) fn add_first_stage_only(
    response: &mut Value,
    stage: &FirstStage,
    top_k: usize,
    exact_report: &[Value],
) {
    let Some(object) = response.as_object_mut() else {
        return;
    };
    if stage.active() {
        let results = merge(stage.mode, &stage.cards, &[], stage.first_k, top_k);
        // The counts sit under `totals`, as on a response with a walk.
        object.insert(
            "totals".into(),
            json!({
                "first_stage": origin_counts(&results),
                "physical_external_calls": stage.git_calls,
            }),
        );
        object.insert("results".into(), json!(results));
    }
    if !exact_report.is_empty() {
        object.insert("exact".into(), json!(exact_report));
    }
}

pub(super) fn response_json(
    call: &Call<'_>,
    result: &TraversalResult,
    found: &Seeds,
    handle: Option<&str>,
    trace_path: Option<&Path>,
    notes: Vec<String>,
    stage: &FirstStage,
) -> Value {
    // With a first stage the walk gives as many cards as can be left once the
    // first stage's repeats are skipped.
    let walk_cards = if stage.active() {
        call.top_k + stage.cards.len()
    } else {
        call.top_k
    };
    let results: Vec<Value> = result
        .ranked()
        .into_iter()
        .take(walk_cards)
        .map(|at| {
            let state = &result.states[at];
            let mut card = trace::card_json(&Card::ranked(state, result.ranked_potential(state)));
            // The tie-break features a card does not already carry, so a fit can
            // read them from the raw output.
            if let Some(object) = card.as_object_mut() {
                object.extend(
                    (result.facts(state).card_fields()).map(|(key, value)| (key.to_owned(), value)),
                );
            }
            card
        })
        .collect();
    let results = if stage.active() && stage.walk_off {
        // The first stage alone answers: no walk card, whatever the mode.
        merge(
            TraverseFirstStage::Append,
            &stage.cards,
            &[],
            stage.first_k,
            call.top_k,
        )
    } else if stage.active() {
        merge(
            stage.mode,
            &stage.cards,
            &results,
            stage.first_k,
            call.top_k,
        )
    } else {
        results
    };
    let first_stage_counts = stage.active().then(|| origin_counts(&results));
    let steering: Vec<Value> = result
        .events
        .iter()
        .filter(|event| event["ev"] == "steer")
        .map(|event| {
            json!({
                "stop": event["stop"],
                "rung": event["rung"],
                "handler": event["handler"],
                "confidence": event["confidence"],
                "actions": event["actions"],
                "outcome": event["outcome"],
            })
        })
        .collect();
    let totals = &result.totals;
    // Per map: the calls made to it over the whole traversal and the
    // milliseconds it took in this call.
    let per_map: serde_json::Map<String, Value> = result
        .maps
        .iter()
        .map(|(id, stat)| {
            let ms = result.map_ms.get(id).copied().unwrap_or(0.0);
            ((*id).to_owned(), json!({"expands": stat.calls, "ms": ms}))
        })
        .collect();
    let mut notes = notes;
    if result.fusion == lci_maps::traverse::Fusion::LogOdds {
        notes.push(
            "fusion log_odds: results are ranked by capped log-odds over the evidence sources (ranked_potential is the fused score; a source that did not deliver a node costs the cap); the score bound is off"
                .to_owned(),
        );
    }
    if result.fusion == lci_maps::traverse::Fusion::Rrf {
        notes.push(
            "fusion rrf: results are ranked by weighted reciprocal rank fusion over the evidence sources (ranked_potential is the fused score); the score bound is off"
                .to_owned(),
        );
    }
    if result.grouping.groups == lci_maps::traverse::Groups::Claim {
        notes.push(
            "groups claim: maps that assert one claim count once, an edge is worth the noisy-OR over its claims, and the score bound is off"
                .to_owned(),
        );
    }
    if result.events.iter().any(|e| e["external_spent"] == true) {
        notes.push(
            "the external-call allowance was spent: from then on only maps that need no external call were asked"
                .to_owned(),
        );
    }
    let mut response = json!({
        "mode": "traverse",
        "status": result.status.as_str(),
        "question": call.question,
        "seeds": found.nodes.iter().map(|seed| seed.node.to_string()).collect::<Vec<_>>(),
        "results": results,
        "totals": {
            "pops": totals.pops,
            "hops": totals.hops,
            "cross_map_hops": totals.cross_map_hops,
            "tokens": totals.tokens,
            // The allowance's account (a retried pop pays what its earlier run
            // did not) and the work that ran: every map call and the external
            // calls they made.
            "external_calls": totals.external_calls,
            "physical_map_calls": totals.physical_map_calls,
            // The git calls of `exact.pickaxe` are physical external calls too.
            "physical_external_calls": totals.physical_external_calls + stage.git_calls,
            "stops": totals.stops,
            "steers": totals.steers,
            "per_map": per_map,
            "elapsed_ms": totals.elapsed_ms,
        },
        "steering": steering,
        "trace": {
            "hash": trace::trace_hash(&result.events),
            "events": result.events.len(),
            "file": trace_path.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()),
        },
    });
    if let Some(object) = response.as_object_mut() {
        if !found.source.is_empty() {
            object.insert("seed_source".into(), json!(found.source));
            object.insert("seed_hits".into(), json!(found.details));
            if let Some(reason) = &found.fallback {
                object.insert("seed_fallback".into(), json!(reason));
            }
        }
        object.insert("stop_reason".into(), json!(result.stop_reason.as_str()));
        if let (Some(counts), Some(totals)) = (
            first_stage_counts,
            object.get_mut("totals").and_then(Value::as_object_mut),
        ) {
            totals.insert("first_stage".into(), counts);
        }
        if let (true, Some(totals)) = (
            result.pass_through,
            object.get_mut("totals").and_then(Value::as_object_mut),
        ) {
            totals.insert(
                "pass_through_pops".into(),
                json!(result.totals.pass_through_pops),
            );
        }
        if let (Some(_), Some(totals)) = (
            result.rerank_floor,
            object.get_mut("totals").and_then(Value::as_object_mut),
        ) {
            // `rerank_ms` is this call's wall clock; the counts are the whole
            // traversal's.
            totals.insert("rerank_calls".into(), json!(result.totals.rerank_calls));
            totals.insert("rerank_docs".into(), json!(result.totals.rerank_docs));
            totals.insert(
                "rerank_failures".into(),
                json!(result.totals.rerank_failures),
            );
            totals.insert(
                "rerank_failure_reasons".into(),
                Value::Object(
                    result
                        .totals
                        .rerank_failure_reasons
                        .iter()
                        .map(|(reason, n)| (reason.as_str().to_owned(), json!(n)))
                        .collect(),
                ),
            );
            totals.insert("rerank_skipped".into(), json!(result.totals.rerank_skipped));
            totals.insert("rerank_ms".into(), json!(result.rerank_ms));
        }
        if let Some(checkpoint) = &result.checkpoint {
            object.insert("handle".into(), json!(handle));
            object.insert("checkpoint".into(), checkpoint_response(checkpoint));
        }
        if !notes.is_empty() {
            object.insert("notes".into(), json!(notes));
        }
    }
    response
}

/// Every card of the response (the result items and the checkpoint's answer,
/// visited cards and frontier) gets `site: {path, line, col}` where the
/// source knows where the node is: the definition of a SCIP symbol (the line
/// and column of its name), the first line of the chunk of a hybrid seed that
/// is a chunk node, the start of a file node. Paths are repository-relative
/// with `/`; lines and columns count from 1, like the other line ranges of
/// `search_code`. A node nothing locates has no `site`. The start nodes
/// (`seeds`, which a complete run does not return as results) are located in
/// `seed_sites`, by node id. A symbol the SCIP graph does not hold (a type
/// `scip.impact` offers) is located by that map.
pub(super) fn add_sites(
    response: &mut Value,
    loaded: &Loaded,
    impact: Option<&ImpactMap>,
    seeds: &[Value],
) {
    let mut wanted: HashSet<String> = HashSet::new();
    for list in CARD_LISTS {
        if let Some(Value::Array(items)) = response.pointer(list) {
            wanted.extend(
                items
                    .iter()
                    .filter_map(|card| card["node"].as_str().map(str::to_owned)),
            );
        }
    }
    let given: Vec<String> = response["seeds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|seed| seed.as_str().map(str::to_owned))
        .collect();
    wanted.extend(given.iter().cloned());
    if wanted.is_empty() {
        return;
    }
    let mut sites: HashMap<String, Value> = HashMap::new();
    // A file node is at its first line (`structure.contains` climbs to files,
    // grep.live names them).
    for node in &wanted {
        if let Ok(NodeId::File { path }) = NodeId::from_str(node) {
            sites.insert(node.clone(), site_json(&path, 1, 1));
        }
    }
    // Chunk and file seeds first, so a symbol's definition wins.
    for seed in seeds {
        let (Some(node), Some(path), Some(line)) = (
            seed["node"].as_str(),
            seed["path"].as_str(),
            seed["lines"][0].as_u64().or(Some(1)),
        ) else {
            continue;
        };
        if wanted.contains(node) && seed["kind"] != "symbol" {
            sites.insert(node.to_owned(), site_json(path, line, 1));
        }
    }
    let graph = loaded.scip.graph();
    for (i, node) in graph.nodes.iter().enumerate() {
        let id = loaded.scip.node_id(i).to_string();
        if wanted.contains(&id) {
            sites.insert(
                id,
                site_json(
                    &node.file,
                    u64::from(node.line) + 1,
                    u64::from(node.col) + 1,
                ),
            );
        }
    }
    if let Some(impact) = impact {
        for node in &wanted {
            let found = NodeId::from_str(node)
                .ok()
                .and_then(|id| impact.site_of(&id));
            if let (false, Some((path, line, col))) = (sites.contains_key(node), found) {
                sites.insert(
                    node.clone(),
                    site_json(path, u64::from(line) + 1, u64::from(col) + 1),
                );
            }
        }
    }
    let seed_sites: serde_json::Map<String, Value> = given
        .iter()
        .filter_map(|seed| Some((seed.clone(), sites.get(seed)?.clone())))
        .collect();
    if let (false, Some(object)) = (seed_sites.is_empty(), response.as_object_mut()) {
        object.insert("seed_sites".into(), Value::Object(seed_sites));
    }
    for list in CARD_LISTS {
        let Some(Value::Array(items)) = response.pointer_mut(list) else {
            continue;
        };
        for card in items {
            let site = card["node"]
                .as_str()
                .and_then(|node| sites.get(node))
                .cloned();
            if let (Some(site), Some(object)) = (site, card.as_object_mut()) {
                object.insert("site".into(), site);
            }
        }
    }
}

/// `[run_profile.traverse] card_paths`: every walked card of `/results` and of
/// `/checkpoint/answer` gets `kind` (the kind of the edge that discovered the
/// node, null for a start node) and `path`: the route from its start node, one
/// `{node, map, kind, witness}` entry per node (`depth + 1` entries, the last
/// one the card itself), read from the walk's state table (a continuation's
/// table holds the earlier calls' states too). The cards of the first stage
/// (`origin` `exact` or `seed`) were not walked and get neither. The visited
/// and frontier lists stay as they are.
pub(super) fn add_paths(response: &mut Value, result: &TraversalResult) {
    let by_node: HashMap<String, usize> = result
        .states
        .iter()
        .enumerate()
        .map(|(at, state)| (state.node.to_string(), at))
        .collect();
    for list in ["/results", "/checkpoint/answer"] {
        let Some(Value::Array(items)) = response.pointer_mut(list) else {
            continue;
        };
        for card in items {
            if matches!(card["origin"].as_str(), Some("exact" | "seed")) {
                continue;
            }
            let Some(&at) = card["node"].as_str().and_then(|node| by_node.get(node)) else {
                continue;
            };
            let Some(chain) = route(&result.states, at) else {
                continue;
            };
            let path: Vec<Value> = chain
                .iter()
                .map(|&i| {
                    let state = &result.states[i];
                    json!({
                        "node": state.node.to_string(),
                        "map": state.via,
                        "kind": state.kind.map(|kind| kind.as_str()),
                        "witness": state.witness.map(|witness| witness.as_str()),
                    })
                })
                .collect();
            if let Some(object) = card.as_object_mut() {
                object.insert(
                    "kind".into(),
                    json!(result.states[at].kind.map(|k| k.as_str())),
                );
                object.insert("path".into(), Value::Array(path));
            }
        }
    }
}

/// The state indexes from the start node down to `at`, following parents;
/// none when the chain does not end at a root within the table (it always does
/// for a walk's own states).
fn route(states: &[State], at: usize) -> Option<Vec<usize>> {
    let mut chain = vec![at];
    let mut here = at;
    while let Some(parent) = states[here].parent {
        if chain.len() > states.len() {
            return None;
        }
        chain.push(parent);
        here = parent;
    }
    chain.reverse();
    Some(chain)
}

fn site_json(path: &str, line: u64, col: u64) -> Value {
    json!({"path": path.replace('\\', "/"), "line": line, "col": col})
}

fn checkpoint_response(checkpoint: &Checkpoint) -> Value {
    let mut value = trace::checkpoint_json(checkpoint);
    if let Some(object) = value.as_object_mut() {
        object.insert("handle".into(), json!(checkpoint.handle));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, stale: bool, lag: Value) -> Value {
        json!({"id": id, "observed": "snapshot", "stale": stale, "lag": lag})
    }

    /// A card that cites a map whose evidence came from a kept server is never
    /// `fresh`, with `file_changed` false or a lag of 0 alike; a card of other
    /// maps is as before, and a `changed` or `stale` finding stands.
    #[test]
    fn a_card_citing_a_kept_session_map_is_unverified_not_fresh() {
        let kept = ["lsp.live"];
        let fresh = vec![
            entry("graph.scip", false, json!(0)),
            entry("lsp.live", false, json!(0)),
        ];
        let by_lag = json!({"maps": ["graph.scip", "lsp.live"], "via": "lsp.live"});
        assert_eq!(card_freshness(&by_lag, Some(&fresh), &[]), "fresh");
        assert_eq!(card_freshness(&by_lag, Some(&fresh), &kept), "unverified");
        let by_file = json!({"file_changed": false, "maps": ["lsp.live"]});
        assert_eq!(card_freshness(&by_file, Some(&fresh), &[]), "fresh");
        assert_eq!(card_freshness(&by_file, Some(&fresh), &kept), "unverified");
        // A card of an index map only is unaffected.
        let index_only = json!({"maps": ["graph.scip"], "via": "graph.scip"});
        assert_eq!(card_freshness(&index_only, Some(&fresh), &kept), "fresh");
        // Findings stand.
        let changed = json!({"file_changed": true, "maps": ["lsp.live"]});
        assert_eq!(card_freshness(&changed, Some(&fresh), &kept), "changed");
        let stale = vec![entry("lsp.live", true, json!(2))];
        assert_eq!(card_freshness(&by_lag, Some(&stale), &kept), "stale");
    }

    /// A continuation that makes no language-server request has an empty
    /// `kept`, yet the cards it restores from its checkpoint may hold what a
    /// kept server gave an earlier call: those that cite `lsp.live` are
    /// `unverified` (cards of nodes its own walk found follow `kept`).
    #[test]
    fn restored_lsp_live_cards_stay_unverified_in_a_continuation() {
        let mut response = json!({
            "status": "complete",
            "maps": [entry("graph.scip", false, json!(0)), entry("lsp.live", false, json!(0))],
            "results": [
                {"node": "restored-lsp", "maps": ["lsp.live"], "via": "lsp.live"},
                {"node": "restored-index", "maps": ["graph.scip"], "via": "graph.scip"},
                {"node": "new-lsp", "maps": ["lsp.live"], "via": "lsp.live"},
            ],
        });
        let restored: HashSet<String> = ["restored-lsp", "restored-index"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        add_answer_labels(&mut response, None, &[], &[], &restored);
        let words: Vec<&str> = response["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|card| card["freshness"].as_str().unwrap())
            .collect();
        assert_eq!(
            words,
            ["unverified", "fresh", "fresh"],
            "restored lsp.live card; restored index card; the call's own fresh-session card"
        );
        // With no checkpoint nothing is restored.
        add_answer_labels(&mut response, None, &[], &[], &HashSet::new());
        assert_eq!(response["results"][0]["freshness"], "fresh");
    }

    #[test]
    fn card_freshness_reads_file_changed_before_the_maps() {
        let stale = vec![entry("graph.scip", true, json!(1))];
        let fresh = vec![entry("graph.scip", false, json!(0))];
        // The card's own file is as its maps read it: fresh, whatever the maps say.
        let card = json!({"file_changed": false, "maps": ["graph.scip"], "via": "graph.scip"});
        assert_eq!(card_freshness(&card, Some(&stale), &[]), "fresh");
        // A changed file is changed, though the maps say fresh.
        let card = json!({"file_changed": true, "maps": ["graph.scip"], "via": "graph.scip"});
        assert_eq!(card_freshness(&card, Some(&fresh), &[]), "changed");
    }

    #[test]
    fn card_freshness_needs_every_map_of_the_card_found_with_a_lag() {
        let card = json!({"file_changed": null, "maps": ["graph.scip"], "via": "graph.scip"});
        let fresh = vec![entry("graph.scip", false, json!(0))];
        assert_eq!(card_freshness(&card, Some(&fresh), &[]), "fresh");
        let stale = vec![entry("graph.scip", true, json!(2))];
        assert_eq!(card_freshness(&card, Some(&stale), &[]), "stale");
        // A map the answer does not list: nothing to say.
        let other = vec![entry("grep.live", false, json!(0))];
        assert_eq!(card_freshness(&card, Some(&other), &[]), "unverified");
        // A map listed with a null lag was not compared.
        let unknown = vec![entry("graph.scip", false, Value::Null)];
        assert_eq!(card_freshness(&card, Some(&unknown), &[]), "unverified");
        // One map unverified keeps the whole card unverified, a stale one wins.
        let card = json!({"maps": ["graph.scip", "grep.live"], "via": "graph.scip"});
        assert_eq!(card_freshness(&card, Some(&fresh), &[]), "unverified");
        let mixed = vec![
            entry("graph.scip", false, json!(0)),
            entry("grep.live", true, json!(1)),
        ];
        assert_eq!(card_freshness(&card, Some(&mixed), &[]), "stale");
        // No `maps` on the answer, or a card with no map: unverified.
        assert_eq!(card_freshness(&card, None, &[]), "unverified");
        assert_eq!(
            card_freshness(&json!({"maps": []}), Some(&fresh), &[]),
            "unverified"
        );
    }

    #[test]
    fn degraded_maps_are_the_unavailable_and_the_ones_with_failed_calls() {
        let failed = MapStat {
            failures: 1,
            ..MapStat::default()
        };
        let stats = BTreeMap::from([
            ("graph.scip", MapStat::default()),
            ("grep.live", failed),
            ("lsp.live", failed),
        ]);
        assert!(degraded_maps(&[], None).is_empty());
        assert!(
            degraded_maps(
                &[],
                Some(&BTreeMap::from([("graph.scip", MapStat::default())]))
            )
            .is_empty()
        );
        // Sorted, once each.
        assert_eq!(
            degraded_maps(&["lsp.live", "git.history"], Some(&stats)),
            ["git.history", "grep.live", "lsp.live"]
        );
    }
}
