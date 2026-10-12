use super::*;
use lci_core::chunk::Chunk;
use lci_maps::{
    Contribution, EdgeKind, Evidence, FreshnessModel, MapDescriptor, MapHit, Registry, Run,
    SnapshotRef, SnapshotView, SourceLabel,
    kit::FixedMap,
    run_record::{self as record, SCHEMA_ID},
};
use serde_json::json;
use std::time::{Duration, SystemTime};

fn chunk(path: &str, start_line: u32, hash: &str) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line,
        end_line: start_line + 3,
        code: format!("fn at_{start_line}() {{}}"),
        content_hash: hash.into(),
    }
}

/// Five chunks; two pairs that the maps below make tie.
fn universe() -> Vec<Chunk> {
    vec![
        chunk("b.rs", 1, "0000000000000001"),
        chunk("a.rs", 9, "0000000000000002"),
        chunk("a.rs", 1, "0000000000000003"),
        chunk("tests/c.rs", 1, "0000000000000004"),
        chunk("d.rs", 5, "0000000000000005"),
    ]
}

fn fixed(
    view: &SnapshotView<'_>,
    id: &'static str,
    contribution: Contribution,
    label: SourceLabel,
    hits: &[(usize, u32, Option<f32>)],
) -> Box<dyn lci_maps::Map + 'static> {
    Box::new(FixedMap {
        descriptor: MapDescriptor {
            id,
            version: 2,
            contribution,
            accepts_text: true,
            accepts: vec![],
            emits: vec![],
            freshness: FreshnessModel::Live,
            source_labels: vec![label],
            needs: vec![],
        },
        hits: hits
            .iter()
            .map(|&(at, rank, score)| MapHit {
                node: view.ids()[at].clone(),
                rank,
                score,
                evidence: vec![Evidence {
                    via: None,
                    edge: EdgeKind::QueryMatch,
                    label,
                    detail: None,
                }],
            })
            .collect(),
    })
}

const WEIGHTS: [(&str, f32); 3] = [("one", 0.7), ("two", 1.3), ("prior.x", 1.0)];
const RRF_K: f32 = 60.0;

async fn registry_run(view: &SnapshotView<'_>) -> Run {
    let mut registry = Registry::new(RRF_K);
    // Ties on purpose: a.rs:1 and a.rs:9 get the same terms, and so do b.rs
    // and d.rs; the prior lifts one node by a value that is not a power of 2.
    registry
        .register(
            fixed(
                view,
                "two",
                Contribution::RankList,
                SourceLabel::Vector,
                &[
                    (1, 1, Some(0.9)),
                    (2, 1, Some(0.9)),
                    (0, 3, None),
                    (4, 3, None),
                ],
            ),
            WEIGHTS[1].1,
            10,
        )
        .unwrap();
    registry
        .register(
            fixed(
                view,
                "one",
                Contribution::RankList,
                SourceLabel::Lexical,
                &[(3, 1, None), (0, 2, None), (4, 2, None)],
            ),
            WEIGHTS[0].1,
            10,
        )
        .unwrap();
    registry
        .register(
            fixed(
                view,
                "prior.x",
                Contribution::Prior,
                SourceLabel::Prior,
                &[(3, 1, Some(0.004)), (1, 2, Some(0.0))],
            ),
            1.0,
            usize::MAX,
        )
        .unwrap();
    registry
        .run(Some("query"), Duration::from_secs(5), view)
        .await
}

/// A record of `run` as the registry path writes it: run, fan_out, fuse
/// (with at most `rows` frontier rows) and end lines.
fn record_text(run: &Run, view: &SnapshotView<'_>, rows: usize) -> String {
    let id = record::run_id_at(SystemTime::now());
    let maps: Vec<Value> = run
        .calls
        .iter()
        .map(|call| {
            let weight = WEIGHTS.iter().find(|(map, _)| *map == call.id).unwrap().1;
            let label = match call.id {
                "one" => "lexical",
                "two" => "vector",
                _ => "prior",
            };
            json!({ "map": call.id, "version": call.version, "weight": record::number(weight), "labels": [label] })
        })
        .collect();
    let (ranked, priors) = record::split_calls(run);
    let frontier: Vec<Value> = run
        .fused
        .iter()
        .take(rows)
        .enumerate()
        .map(|(at, fused)| {
            let mut row = record::fused_row(fused, at + 1, view);
            row.insert("source_role".into(), json!("source"));
            Value::Object(row)
        })
        .collect();
    let lines = [
        json!({
            "type": "run", "schema": SCHEMA_ID, "run_id": id,
            "started_at": record::utc_rfc3339(SystemTime::now()),
            "build": { "version": "test" },
            "workspace_id": "0123456789abcdef",
            "snapshot": { "indexed_at": "1", "fingerprint": "0123456789abcdef" },
            "query": { "text": "query" },
            "plan": { "maps": maps, "rrf_k": record::number(RRF_K), "policy": { "kind": "one-shot" } },
            "capture": { "snippets": false },
        }),
        json!({
            "type": "step", "run_id": id, "seq": 1, "at_ms": 1.0, "kind": "fan_out", "yield": "Y1",
            "map_calls": ranked.iter().map(|c| record::map_call_json(c, true, Some(10))).collect::<Vec<_>>(),
        }),
        json!({
            "type": "step", "run_id": id, "seq": 2, "at_ms": 2.0, "kind": "fuse", "yield": "none",
            "map_calls": priors.iter().map(|c| record::map_call_json(c, true, None)).collect::<Vec<_>>(),
            "frontier": { "size": run.fused.len(), "rows": frontier.clone() },
        }),
        json!({
            "type": "end", "run_id": id, "finished_at": record::utc_rfc3339(SystemTime::now()),
            "outcome": "completed", "results": frontier.iter().take(3).collect::<Vec<_>>(),
            "totals": { "elapsed_ms": 2.0, "steps": 2 },
        }),
    ];
    lines.iter().map(|line| format!("{line}\n")).collect()
}

fn edit(raw: &str, line: usize, change: impl FnOnce(&mut Value)) -> String {
    let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
    let mut value: Value = serde_json::from_str(&lines[line]).unwrap();
    change(&mut value);
    lines[line] = value.to_string();
    lines.iter().map(|text| format!("{text}\n")).collect()
}

#[tokio::test]
async fn replaying_the_fuse_step_reproduces_every_score_bit_for_bit() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let run = registry_run(&view).await;
    assert_eq!(run.fused.len(), 5);
    assert!(
        run.fused
            .windows(2)
            .any(|pair| pair[0].score.to_bits() == pair[1].score.to_bits()),
        "the fixture has a tie"
    );
    let raw = record_text(&run, &view, 50);
    let parsed = RunRecordFile::parse(&raw).unwrap();
    assert_eq!(
        parsed.replay_fuse().unwrap(),
        FuseReplay {
            fuse_steps: 1,
            nodes: 5,
            rows_checked: 5
        }
    );
    parsed.check_labels().unwrap();

    // A capped frontier is checked as far as it goes.
    let capped = RunRecordFile::parse(&record_text(&run, &view, 2)).unwrap();
    assert_eq!(capped.replay_fuse().unwrap().rows_checked, 2);
}

#[tokio::test]
async fn the_replay_refuses_a_record_off_by_one_bit_or_out_of_order() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let run = registry_run(&view).await;
    let raw = record_text(&run, &view, 50);
    let next_up = |value: &Value| {
        let bits = (value.as_f64().unwrap() as f32).to_bits() + 1;
        record::number(f32::from_bits(bits))
    };
    let tampered = [
        edit(&raw, 2, |fuse| {
            let score = &mut fuse["frontier"]["rows"][1]["score"];
            *score = next_up(score);
        }),
        edit(&raw, 2, |fuse| {
            let row = &mut fuse["frontier"]["rows"][0]["contributions"];
            let map = row.as_object().unwrap().keys().next().unwrap().clone();
            row[&map] = next_up(&row[&map]);
        }),
        edit(&raw, 2, |fuse| {
            fuse["frontier"]["rows"].as_array_mut().unwrap().swap(0, 1);
        }),
        edit(&raw, 2, |fuse| {
            // Tied rows swapped: same scores, wrong tie order.
            let rows = fuse["frontier"]["rows"].as_array_mut().unwrap();
            let tie = (0..rows.len() - 1)
                .find(|&at| rows[at]["score"] == rows[at + 1]["score"])
                .unwrap();
            rows.swap(tie, tie + 1);
            rows[tie]["fused_rank"] = json!(tie + 1);
            rows[tie + 1]["fused_rank"] = json!(tie + 2);
        }),
        edit(&raw, 2, |fuse| fuse["frontier"]["size"] = json!(4)),
        edit(&raw, 0, |run| {
            run["plan"]["maps"][0]["weight"] = json!(0.71)
        }),
        edit(&raw, 1, |fan_out| {
            fan_out["map_calls"][0]["hits"][0]["rank"] = json!(2)
        }),
        edit(&raw, 2, |fuse| {
            fuse["map_calls"][0]["hits"][0]["score"] = json!(0.005);
        }),
        // A call recorded as failed keeps its hits out of the fusion.
        edit(&raw, 1, |fan_out| {
            fan_out["map_calls"][0]["status"] = json!("failed")
        }),
        edit(&raw, 2, |fuse| fuse["frontier"]["rows"] = json!([])),
    ];
    for (index, raw) in tampered.iter().enumerate() {
        let parsed = RunRecordFile::parse(raw).unwrap();
        assert!(
            parsed.replay_fuse().is_err(),
            "tampered record {index} replayed"
        );
    }
    // Dropping a capped-out node's place: a capped row list that skips a
    // better-scoring node is refused.
    let skipped = edit(&record_text(&run, &view, 2), 2, |fuse| {
        fuse["frontier"]["rows"].as_array_mut().unwrap().remove(0);
        fuse["frontier"]["rows"][0]["fused_rank"] = json!(1);
    });
    assert!(
        RunRecordFile::parse(&skipped)
            .unwrap()
            .replay_fuse()
            .is_err()
    );
}

#[tokio::test]
async fn the_reader_keeps_a_prefix_and_refuses_broken_records() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let run = registry_run(&view).await;
    let raw = record_text(&run, &view, 50);
    let lines: Vec<&str> = raw.lines().collect();

    // Killed after step 2, mid-way through the end line.
    let cut = format!(
        "{}\n{}\n{}\n{}",
        lines[0],
        lines[1],
        lines[2],
        &lines[3][..20]
    );
    let prefix = RunRecordFile::parse(&cut).unwrap();
    assert!(prefix.torn_tail);
    assert_eq!(prefix.steps.len(), 2);
    assert!(prefix.end.is_none());
    let missing = prefix.end().unwrap_err().to_string();
    assert!(missing.contains("outcome missing"), "{missing}");
    prefix.replay_fuse().unwrap();
    let whole = RunRecordFile::parse(&raw).unwrap();
    assert!(!whole.torn_tail && whole.end.is_some());

    let broken = [
        String::new(),
        format!("{}\n", lines[1]),
        format!("{}\n{}\n{}\n", lines[0], lines[2], lines[1]),
        format!("{raw}{}\n", lines[1]),
        edit(&raw, 1, |step| step["kind"] = json!("fan-out")),
        edit(&raw, 2, |step| {
            step["run_id"] = json!("01HF7YAT000000000000000000")
        }),
        format!("{}\nnot json\n", lines[0]),
    ];
    for (index, raw) in broken.iter().enumerate() {
        assert!(
            RunRecordFile::parse(raw).is_err(),
            "broken record {index} read"
        );
    }
}

fn gold(expected: &str) -> EvalQuery {
    EvalQuery {
        query: "query".into(),
        workspace: "fixture".into(),
        top_k: None,
        filters: None,
        expected_paths: vec![expected.into()],
        preferred_paths: vec![],
        disfavored_paths: vec![],
        expected_roles: vec![],
        required_snippet: None,
        tags: vec![],
    }
}

#[tokio::test]
async fn a_record_is_scored_with_the_evaluation_metrics() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let run = registry_run(&view).await;
    let parsed = RunRecordFile::parse(&record_text(&run, &view, 50)).unwrap();
    let evidence = parsed.result_evidence().unwrap();
    let paths: Vec<&str> = evidence
        .iter()
        .map(|row| row.relative_file_path.as_str())
        .collect();
    let fused: Vec<&str> = run
        .fused
        .iter()
        .take(3)
        .map(|f| f.node.path().unwrap())
        .collect();
    assert_eq!(paths, fused);
    let second = paths[1];
    let metrics = parsed.score(&gold(second)).unwrap();
    assert_eq!(metrics.first_expected_rank, Some(2));
    assert!(metrics.hit && !metrics.hit_at_1 && metrics.hit_at_3);
    assert_eq!(metrics.reciprocal_rank, 0.5);
    assert_eq!(metrics.window, 3);
    assert!(!parsed.score(&gold("nowhere.rs")).unwrap().hit);

    let mut other = gold(second);
    other.query = "another query".into();
    assert!(parsed.score(&other).is_err());
    let mut snippet = gold(second);
    snippet.required_snippet = Some("fn".into());
    assert!(parsed.score(&snippet).is_err(), "no snippets were captured");
    let mut window = gold(second);
    window.top_k = Some(3);
    assert!(
        parsed.score(&window).is_err(),
        "the record's plan has no top_k 3"
    );
    let failed = RunRecordFile::parse(&edit(&record_text(&run, &view, 50), 3, |end| {
        end["outcome"] = json!("failed");
    }))
    .unwrap();
    assert!(
        failed.score(&gold(second)).is_err(),
        "a failed run is not scored"
    );
    let roleless = RunRecordFile::parse(&edit(&record_text(&run, &view, 50), 3, |end| {
        end["results"][0]
            .as_object_mut()
            .unwrap()
            .remove("source_role");
    }))
    .unwrap();
    assert!(
        roleless.result_evidence().is_err(),
        "a result without a role"
    );
}

#[tokio::test]
async fn a_label_off_the_card_or_a_hit_without_one_fails_the_offline_c5_check() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let run = registry_run(&view).await;
    let raw = record_text(&run, &view, 50);
    RunRecordFile::parse(&raw).unwrap().check_labels().unwrap();
    let off_card = edit(&raw, 1, |step| {
        step["map_calls"][0]["hits"][0]["evidence"][0]["label"] = json!("scip");
    });
    assert!(
        RunRecordFile::parse(&off_card)
            .unwrap()
            .check_labels()
            .is_err()
    );
    let unlabelled = edit(&raw, 1, |step| {
        step["map_calls"][0]["hits"][0]["evidence"] = json!([]);
    });
    assert!(
        RunRecordFile::parse(&unlabelled)
            .unwrap()
            .check_labels()
            .is_err()
    );
}
