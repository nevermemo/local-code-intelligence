use super::*;
use crate::{
    Contribution, Evidence, FreshnessModel, MapDescriptor, MapHit, Registry, SnapshotRef,
    SourceLabel, kit::FixedMap,
};
use lci_core::chunk::Chunk;
use std::time::Duration;

fn run_line(run_id: &str) -> Value {
    json!({
        "type": "run",
        "schema": SCHEMA_ID,
        "run_id": run_id,
        "started_at": utc_rfc3339(SystemTime::now()),
        "build": { "version": "0.0.0" },
        "workspace_id": "0123456789abcdef0123456789abcdef",
        "snapshot": { "indexed_at": "1", "fingerprint": "0123456789abcdef" },
        "query": { "text": "query" },
        "plan": { "maps": [], "policy": { "kind": "one-shot" } },
    })
}

fn step_line(run_id: &str, seq: u32) -> Value {
    json!({
        "type": "step", "run_id": run_id, "seq": seq, "at_ms": 0.5,
        "kind": "fan_out", "yield": "Y1", "map_calls": [],
    })
}

fn lines_of(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.ends_with('\n'), "a whole number of lines");
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn the_checked_in_schema_loads_and_accepts_minimal_lines() {
    let id = run_id_at(SystemTime::now());
    validate_line(&run_line(&id)).unwrap();
    validate_line(&step_line(&id, 1)).unwrap();
    validate_line(&json!({
        "type": "end", "run_id": id, "finished_at": utc_rfc3339(SystemTime::now()),
        "outcome": "completed", "results": [], "totals": { "elapsed_ms": 1.0, "steps": 1 },
    }))
    .unwrap();
    let mut wrong = run_line(&id);
    wrong["schema"] = json!("lci.runrecord/1");
    assert!(validate_line(&wrong).is_err());
    assert!(validate_line(&json!({ "type": "other" })).is_err());
}

#[test]
fn run_ids_are_ulids_sortable_by_start_time() {
    let early = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
    let late = early + Duration::from_millis(1);
    let (a, b, c) = (run_id_at(early), run_id_at(early), run_id_at(late));
    for id in [&a, &b, &c] {
        assert_eq!(id.len(), 26);
        assert!(id.bytes().all(|byte| CROCKFORD.contains(&byte)), "{id}");
    }
    assert_ne!(a, b, "the random part differs");
    assert_eq!(a[..10], b[..10], "same millisecond, same time part");
    assert!(a < c && b < c, "{a} {b} {c}");
    // 1_700_000_000_000 ms in base32, as the ULID spec encodes it.
    assert_eq!(&a[..10], "01HF7YAT00");
}

#[test]
fn timestamps_are_rfc3339_utc() {
    let at = |seconds: u64, millis: u64| {
        utc_rfc3339(UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis))
    };
    assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
    assert_eq!(at(951_782_400, 7), "2000-02-29T00:00:00.007Z");
    assert_eq!(at(1_700_000_000, 999), "2023-11-14T22:13:20.999Z");
    assert_eq!(at(4_102_444_799, 0), "2099-12-31T23:59:59.000Z");
}

#[test]
fn the_writer_appends_valid_lines_and_refuses_the_rest() {
    let dir = tempdir();
    let id = run_id_at(SystemTime::now());
    let mut writer = RunRecordWriter::create(&dir.join("nested"), &id).unwrap();
    writer.append(&run_line(&id)).unwrap();
    let mut bad = step_line(&id, 1);
    bad["seq"] = json!(0);
    let error = writer.append(&bad).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    writer.append(&step_line(&id, 1)).unwrap();
    assert_eq!(writer.lines(), 2);
    let lines = lines_of(writer.path());
    assert_eq!(
        lines,
        vec![run_line_like(&lines[0], &id), step_line(&id, 1)]
    );
    assert_eq!(writer.path().file_name().unwrap(), &*format!("{id}.jsonl"));

    // Never overwrites, and only ULIDs name files.
    assert!(RunRecordWriter::create(&dir.join("nested"), &id).is_err());
    assert!(RunRecordWriter::create(&dir, "../escape").is_err());
}

/// The run line has a clock field; compare the rest.
fn run_line_like(written: &Value, id: &str) -> Value {
    let mut expected = run_line(id);
    expected["started_at"] = written["started_at"].clone();
    expected
}

#[test]
fn an_unwritable_directory_is_an_error_not_a_panic() {
    let dir = tempdir();
    let blocker = dir.join("file");
    std::fs::write(&blocker, "not a directory").unwrap();
    let id = run_id_at(SystemTime::now());
    assert!(RunRecordWriter::create(&blocker, &id).is_err());
    assert!(RunRecordWriter::create(&blocker.join("below"), &id).is_err());
}

#[test]
fn redaction_replaces_every_secret_longest_first() {
    let mut value = json!({
        "a": "failed at /work/repo/src/x.rs and /work/repo.",
        "b": ["see http://127.0.0.1:9/v1/embeddings", "connect 127.0.0.1:9 refused"],
        "c": 3,
        "d": "file:///work/repo/lib.rs, \"/work/repo\"",
    });
    redact(
        &mut value,
        &[
            ("/work".to_owned(), "<root>"),
            ("/work/repo".to_owned(), "<workspace>"),
            ("http://127.0.0.1:9/v1".to_owned(), "<embedding_url>"),
            ("127.0.0.1:9".to_owned(), "<embedding_host>"),
            (String::new(), "<never>"),
        ],
    );
    assert_eq!(
        value,
        json!({
            "a": "failed at <workspace>/src/x.rs and <workspace>.",
            "b": ["see <embedding_url>/embeddings", "connect <embedding_host> refused"],
            "c": 3,
            "d": "file://<workspace>/lib.rs, \"<workspace>\"",
        })
    );
}

/// A secret that is also a repository-relative path segment (a workspace
/// at `/src`) never corrupts NodeIds or other paths.
#[test]
fn redaction_leaves_repository_paths_and_numbers_alone() {
    let original = json!({
        "node": "chunk:lib/src/a.rs@00ff#0",
        "hits": [{ "node": "chunk:c++/src/x.cc@00ff#0", "evidence": [{ "via": "file:+/src" }] }],
        "via": "file:src/b.rs",
        "other": "/srcs/x and /src-old/y",
    });
    let mut value = original.clone();
    redact(&mut value, &[("/src".to_owned(), "<workspace>")]);
    assert_eq!(value, original);
}

fn chunk(path: &str, start_line: u32) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line,
        end_line: start_line + 2,
        code: String::new(),
        content_hash: format!("{start_line:016x}"),
    }
}

fn descriptor(id: &'static str, contribution: Contribution) -> MapDescriptor {
    MapDescriptor {
        id,
        version: 3,
        contribution,
        accepts_text: true,
        accepts: vec![],
        emits: vec![],
        freshness: FreshnessModel::PerFile,
        source_labels: vec![SourceLabel::Lexical],
        needs: vec![],
    }
}

#[tokio::test]
async fn registry_calls_and_fused_rows_are_schema_valid_and_keep_score_bits() {
    let chunks = [chunk("a.rs", 1), chunk("a.rs", 9), chunk("b.rs", 1)];
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let hit = |position: usize, rank| MapHit {
        node: view.ids()[position].clone(),
        rank,
        score: Some(0.1 * rank as f32),
        evidence: vec![Evidence {
            via: Some(view.ids()[0].clone()),
            edge: EdgeKind::QueryMatch,
            label: SourceLabel::Lexical,
            detail: Some("x".repeat(DETAIL_MAX_CHARS + 5)),
        }],
    };
    let mut registry = Registry::new(60.0);
    registry
        .register(
            Box::new(FixedMap {
                descriptor: descriptor("lexical", Contribution::RankList),
                hits: vec![hit(2, 1), hit(0, 2), hit(1, 3)],
            }),
            0.7,
            10,
        )
        .unwrap();
    registry
        .register(
            Box::new(FixedMap {
                descriptor: descriptor("other.map", Contribution::RankList),
                hits: vec![hit(0, 1)],
            }),
            1.3,
            10,
        )
        .unwrap();
    let run = registry.run(Some("q"), Duration::from_secs(5), &view).await;
    let (ranked, priors) = split_calls(&run);
    assert_eq!((ranked.len(), priors.len()), (2, 0));
    let id = run_id_at(SystemTime::now());
    let rows: Vec<Value> = run
        .fused
        .iter()
        .enumerate()
        .map(|(at, fused)| Value::Object(fused_row(fused, at + 1, &view)))
        .collect();
    let step = json!({
        "type": "step", "run_id": id, "seq": 1, "at_ms": 0.0, "kind": "fuse", "yield": "none",
        "map_calls": ranked.iter().map(|call| map_call_json(call, true, Some(10))).collect::<Vec<_>>(),
        "frontier": { "size": rows.len(), "rows": rows },
    });
    validate_line(&step).unwrap();
    let text = serde_json::to_string(&step).unwrap();
    let back: Value = serde_json::from_str(&text).unwrap();
    let detail = back["map_calls"][0]["hits"][0]["evidence"][0]["detail"]
        .as_str()
        .unwrap();
    assert_eq!(detail.chars().count(), DETAIL_MAX_CHARS);
    for (row, fused) in back["frontier"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&run.fused)
    {
        assert_eq!(row["node"], fused.node.to_string());
        let score = row["score"].as_f64().unwrap() as f32;
        assert_eq!(score.to_bits(), fused.score.to_bits());
        for part in &fused.parts {
            let value = row["contributions"][part.map].as_f64().unwrap() as f32;
            assert_eq!(value.to_bits(), part.value.to_bits());
        }
    }
}

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lci-runrecord-{}", run_id_at(SystemTime::now())));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const KILL_DIR: &str = "LCI_RUNRECORD_KILL_DIR";

/// The child half of the crash-safety test: with [`KILL_DIR`] set, writes a
/// run line and two step lines, signals that it did, then waits to be
/// killed. Without it (an ordinary test run), does nothing.
#[test]
fn killed_writer_child() {
    let Some(dir) = std::env::var_os(KILL_DIR) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let id = run_id_at(SystemTime::now());
    let mut writer = RunRecordWriter::create(&dir, &id).unwrap();
    writer.append(&run_line(&id)).unwrap();
    writer.append(&step_line(&id, 1)).unwrap();
    writer.append(&step_line(&id, 2)).unwrap();
    std::fs::write(dir.join("ready"), "").unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Crash safety (run-record.md, section 5): a writer process killed after
/// step 2 leaves a file of whole, schema-valid lines (`run`, two `step`s)
/// and no `end`.
#[test]
fn a_killed_writer_leaves_a_readable_prefix() {
    let dir = tempdir();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "run_record::tests::killed_writer_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(KILL_DIR, &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while !dir.join("ready").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the child never wrote its lines"
        );
        assert!(child.try_wait().unwrap().is_none(), "the child exited");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let records: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    let lines = lines_of(&records[0]);
    let kinds: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["run", "step", "step"]);
    for line in &lines {
        validate_line(line).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
