//! RunRecords of registry runs (spike slice S3; spec
//! `docs/spike/multimap/specs/run-record.md`). [`App::record_through_maps`]
//! is [`App::fuse_through_maps`] plus a RunRecord: a `run` line before the
//! registry runs, then a `fan_out` step (Y1: the rank-list calls), a `fuse`
//! step (the prior calls and the fused pool), a `cap_cut` step (Y2: the
//! per-file cap and the shortlist cut, with the cap the production search
//! uses), and an `end` line whose results are what `search_code` returns
//! with the reranker off. The registry path has no reranker.
//!
//! Fail-open: a record that cannot be written is a log warning and an
//! error in [`Recorded::record`]; the fused pool is returned as
//! [`App::fuse_through_maps`] returns it. The production search never writes
//! a RunRecord.

use super::{
    LEGACY_RUN_BUDGET, LegacySetup, MapFusion, lexical_descriptor, lsp_descriptor, map_fusion,
    semantic_descriptor, source_role_descriptor,
};
use crate::{
    app::App,
    store::{Hit, bounded_snippet},
};
use anyhow::ensure;
use lci_core::node::NodeId;
use lci_maps::{
    MapDescriptor, Run,
    run_record::{self as record, RecordOptions, Recorded, RunRecordWriter},
};
use serde_json::{Map as JsonMap, Value, json};
use std::{
    collections::HashMap,
    path::Path,
    time::{Instant, SystemTime},
};

/// A writer that redacts every line before writing it.
struct Recorder {
    writer: RunRecordWriter,
    secrets: Vec<(String, &'static str)>,
}

impl Recorder {
    fn write(&mut self, mut line: Value) -> Result<(), String> {
        record::redact(&mut line, &self.secrets);
        self.writer
            .append(&line)
            .map_err(|error| format!("{}: {error}", self.writer.path().display()))
    }
}

fn ms(from: Instant, to: Instant) -> f64 {
    to.saturating_duration_since(from).as_secs_f64() * 1000.0
}

impl App {
    /// Spike slice S3, for tests and the offline harness only:
    /// [`Self::fuse_through_maps`] that also writes a RunRecord of the run
    /// under `options.dir`. The production search never calls this.
    #[doc(hidden)]
    pub async fn record_through_maps(
        &self,
        path: &Path,
        query: &str,
        options: &RecordOptions,
    ) -> anyhow::Result<Recorded<MapFusion>> {
        let top_k = options.top_k.unwrap_or(self.config.default_top_k);
        ensure!((1..=40).contains(&top_k), "top_k must be 1..=40");
        let started_at = SystemTime::now();
        let clock = Instant::now();
        let run_id = record::run_id_at(started_at);
        let secrets = self.record_secrets(path);
        let recorded = self
            .with_legacy_run(
                path,
                query,
                |setup| {
                    let recorder = RunRecordWriter::create(&options.dir, &run_id)
                        .map_err(|error| format!("{}: {error}", options.dir.display()))
                        .and_then(|writer| {
                            let mut recorder = Recorder {
                                writer,
                                secrets: secrets.clone(),
                            };
                            recorder.write(
                                self.run_line(setup, &run_id, started_at, query, top_k, options),
                            )?;
                            Ok(recorder)
                        });
                    (recorder, Instant::now())
                },
                |(recorder, registry_started), setup, run| {
                    let registry_finished = Instant::now();
                    let fusion = map_fusion(setup, run);
                    let record = recorder.and_then(|mut recorder| {
                        let timing = Timing {
                            clock,
                            registry_started,
                            registry_finished,
                        };
                        for line in self.run_lines(setup, run, &run_id, top_k, options, timing) {
                            recorder.write(line)?;
                        }
                        Ok(recorder.writer.path().to_owned())
                    });
                    Recorded {
                        value: fusion,
                        record,
                    }
                },
            )
            .await?;
        if let Err(error) = &recorded.record {
            tracing::warn!(%error, "RunRecord not written; the run is unaffected");
        }
        Ok(recorded)
    }

    /// Strings no record may contain (run-record.md 3.2), each with its
    /// placeholder: the workspace and the data directory (as given when
    /// absolute, and resolved), each also with `/` separators, as a
    /// `file://` URI and with spaces percent-encoded; the service URLs and
    /// their `host:port`; and every absolute executable path in the
    /// configuration. A relative path is never a secret: it could match
    /// repository paths.
    fn record_secrets(&self, path: &Path) -> Vec<(String, &'static str)> {
        let config = &self.config;
        let mut secrets = Vec::new();
        let mut add_path = |path: &Path, placeholder: &'static str| {
            let candidates = [Some(path.to_path_buf()), dunce::canonicalize(path).ok()];
            for candidate in candidates.into_iter().flatten() {
                if !candidate.is_absolute() {
                    continue;
                }
                let raw = candidate.to_string_lossy().into_owned();
                let slashed = raw.replace('\\', "/");
                let uri = if slashed.starts_with('/') {
                    format!("file://{slashed}")
                } else {
                    format!("file:///{slashed}")
                };
                for form in [raw, slashed.replace(' ', "%20"), uri, slashed] {
                    secrets.push((form, placeholder));
                }
            }
        };
        add_path(path, "<workspace>");
        add_path(&config.data_dir, "<data_dir>");
        for (url, placeholder, host_placeholder) in [
            (&config.embedding_url, "<embedding_url>", "<embedding_host>"),
            (&config.reranker_url, "<reranker_url>", "<reranker_host>"),
        ] {
            secrets.push((url.clone(), placeholder));
            if let Some(host) = url
                .split_once("://")
                .and_then(|(_, rest)| rest.split('/').next())
                .filter(|host| !host.is_empty())
            {
                secrets.push((host.to_owned(), host_placeholder));
            }
        }
        let executables = [
            Some(&config.ripgrep_path),
            Some(&config.rust_analyzer_path),
            config.csharp.path.as_ref(),
            config.typescript.path.as_ref(),
            config.python.path.as_ref(),
            config.go.path.as_ref(),
            config.java.path.as_ref(),
            config.clangd.path.as_ref(),
        ];
        for executable in executables.into_iter().flatten() {
            if Path::new(executable).is_absolute() {
                secrets.push((executable.clone(), "<executable>"));
                secrets.push((executable.replace('\\', "/"), "<executable>"));
            }
        }
        secrets
    }

    fn run_line(
        &self,
        setup: &LegacySetup<'_, '_>,
        run_id: &str,
        started_at: SystemTime,
        query: &str,
        top_k: usize,
        options: &RecordOptions,
    ) -> Value {
        let config = &self.config;
        let channels = config.channels;
        let fusion = &config.fusion;
        let map = |descriptor: MapDescriptor, weight: f32, limit: Option<usize>, enabled| {
            let mut entry = JsonMap::new();
            entry.insert("map".into(), json!(descriptor.id));
            entry.insert("version".into(), json!(descriptor.version));
            entry.insert(
                "contribution".into(),
                json!(record::contribution_name(descriptor.contribution)),
            );
            entry.insert("weight".into(), record::number(weight));
            if let Some(limit) = limit {
                entry.insert("limit".into(), json!(limit));
            }
            entry.insert("enabled".into(), json!(enabled));
            let labels: Vec<&str> = descriptor
                .source_labels
                .iter()
                .map(|l| l.as_str())
                .collect();
            entry.insert("labels".into(), json!(labels));
            Value::Object(entry)
        };
        // Fusion order: the legacy three, then the prior (decision M-3).
        let maps = vec![
            map(
                semantic_descriptor(),
                fusion.semantic_weight,
                Some(config.semantic_candidate_count),
                channels.semantic,
            ),
            map(
                lexical_descriptor(),
                fusion.lexical_weight,
                Some(config.lexical_candidate_count),
                channels.lexical,
            ),
            map(
                lsp_descriptor(),
                fusion.lsp_weight,
                Some(config.lsp_candidate_count),
                channels.lsp,
            ),
            map(source_role_descriptor(), 1.0, None, true),
        ];
        let snapshot = setup.snapshot;
        let manifest = crate::manifest::load(&config.data_dir, setup.workspace);
        let is_hex = |value: &str| {
            (16..=64).contains(&value.len())
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        // A manifest written before fingerprints existed has none; the
        // snapshot's own identity then stands in (run-record.md, R-8).
        let fingerprint = if is_hex(&manifest.fingerprint) {
            manifest.fingerprint
        } else {
            crate::workspace::hash(&format!("{}\0{}", snapshot.indexed_at, snapshot.identity))
        };
        let mut plan = json!({
            "maps": maps,
            "rrf_k": record::number(config.rrf_k),
            "max_chunks_per_file": fusion.max_chunks_per_file,
            "rerank_candidate_count": config.rerank_candidate_count,
            "top_k": top_k,
            "policy": { "kind": "one-shot" },
            "budget": { "deadline_ms": LEGACY_RUN_BUDGET.as_millis() as u64 },
        });
        if let Some(arm) = &options.arm {
            plan["arm"] = json!(arm);
        }
        let mut line = json!({
            "type": "run",
            "schema": record::SCHEMA_ID,
            "run_id": run_id,
            "started_at": record::utc_rfc3339(started_at),
            "build": { "version": env!("CARGO_PKG_VERSION") },
            "workspace_id": setup.workspace.id,
            "snapshot": {
                "indexed_at": snapshot.indexed_at,
                "fingerprint": fingerprint,
                "embedding_identity_sha256": crate::workspace::hash(&snapshot.identity),
            },
            "endpoint": "harness",
            "query": { "text": query },
            "filters": {},
            "plan": plan,
            "capture": {
                "snippets": options.capture_snippets,
                "frontier_rows": options.frontier_rows,
            },
        });
        if let Some(item) = &options.item {
            line["item"] = json!({ "set": item.set, "id": item.id });
        }
        line
    }

    /// The `step` lines and the `end` line of a finished registry run.
    fn run_lines(
        &self,
        setup: &LegacySetup<'_, '_>,
        run: &Run,
        run_id: &str,
        top_k: usize,
        options: &RecordOptions,
        timing: Timing,
    ) -> Vec<Value> {
        let config = &self.config;
        let hits: HashMap<&NodeId, &Hit> = setup.view.ids().iter().zip(setup.universe).collect();
        let row = |fused: &lci_maps::Fused, fused_rank: usize| {
            let mut row = record::fused_row(fused, fused_rank, setup.view);
            if let Some(hit) = hits.get(&fused.node) {
                row.insert("source_role".into(), json!(hit.source_role));
            }
            row
        };
        let limits: HashMap<&str, usize> = [
            (super::SEMANTIC, config.semantic_candidate_count),
            (super::LEXICAL, config.lexical_candidate_count),
            (super::LSP, config.lsp_candidate_count),
        ]
        .into_iter()
        .collect();
        let calls_json = |calls: &[&lci_maps::MapCall]| -> Vec<Value> {
            calls
                .iter()
                .map(|call| record::map_call_json(call, true, limits.get(call.id).copied()))
                .collect()
        };
        let messages = |calls: &[&lci_maps::MapCall]| {
            let run = Run {
                calls: calls.iter().map(|call| (*call).clone()).collect(),
                fused: Vec::new(),
            };
            let warnings: Vec<String> = run.warnings().iter().map(|(_, w)| w.to_string()).collect();
            let notes: Vec<String> = run.notes().iter().map(|(_, n)| n.to_string()).collect();
            (warnings, notes)
        };
        let (ranked, priors) = record::split_calls(run);
        let fan_out_ms = ranked.iter().map(|c| c.engine_ms).fold(0.0, f64::max);
        let (fan_out_warnings, fan_out_notes) = messages(&ranked);
        let fan_out = json!({
            "type": "step", "run_id": run_id, "seq": 1,
            "at_ms": ms(timing.clock, timing.registry_started) + fan_out_ms,
            "kind": "fan_out", "yield": "Y1",
            "map_calls": calls_json(&ranked),
            "engine_cost": { "elapsed_ms": fan_out_ms },
            "warnings": fan_out_warnings,
            "notes": fan_out_notes,
        });

        let pool = run.fused.len();
        let fused_rows: Vec<Value> = run
            .fused
            .iter()
            .take(options.frontier_rows)
            .enumerate()
            .map(|(at, fused)| Value::Object(row(fused, at + 1)))
            .collect();
        let (fuse_warnings, fuse_notes) = messages(&priors);
        let fuse = json!({
            "type": "step", "run_id": run_id, "seq": 2,
            "at_ms": ms(timing.clock, timing.registry_finished),
            "kind": "fuse", "yield": "none",
            "map_calls": calls_json(&priors),
            // The priors and the fusion: the registry's time after the
            // fan-out step.
            "engine_cost": {
                "elapsed_ms": (ms(timing.registry_started, timing.registry_finished) - fan_out_ms).max(0.0)
            },
            "frontier": { "size": pool, "rows": fused_rows },
            "warnings": fuse_warnings,
            "notes": fuse_notes,
        });

        // The production search's cap and cut, on positions in fused order.
        let positions: Vec<(usize, &str)> = run
            .fused
            .iter()
            .enumerate()
            .map(|(at, fused)| {
                let path = setup.view.chunk(&fused.node);
                (
                    at,
                    path.map_or("", |chunk| chunk.relative_file_path.as_str()),
                )
            })
            .collect();
        let capped: Vec<usize> =
            super::super::cap_by_file(positions, config.fusion.max_chunks_per_file, |(_, path)| {
                path
            })
            .into_iter()
            .map(|(at, _)| at)
            .collect();
        let shortlist = config.rerank_candidate_count;
        let capped_row = |capped_rank: usize, at: usize| {
            let mut row = row(&run.fused[at], at + 1);
            row.insert("capped_rank".into(), json!(capped_rank));
            row.insert("shortlisted".into(), json!(capped_rank <= shortlist));
            row
        };
        let cap_rows: Vec<Value> = capped
            .iter()
            .take(options.frontier_rows)
            .enumerate()
            .map(|(position, &at)| Value::Object(capped_row(position + 1, at)))
            .collect();
        let cap_cut = json!({
            "type": "step", "run_id": run_id, "seq": 3,
            "at_ms": ms(timing.clock, Instant::now()),
            "kind": "cap_cut", "yield": "Y2",
            "frontier": { "size": pool, "rows": cap_rows },
        });

        let results: Vec<Value> = capped
            .iter()
            .take(shortlist.min(top_k))
            .enumerate()
            .map(|(position, &at)| {
                let mut row = capped_row(position + 1, at);
                if options.capture_snippets
                    && let Some(chunk) = setup.view.chunk(&run.fused[at].node)
                {
                    row.insert("snippet".into(), json!(bounded_snippet(&chunk.code)));
                }
                Value::Object(row)
            })
            .collect();
        let per_map: JsonMap<String, Value> = run
            .calls
            .iter()
            .map(|call| {
                let mut cost = record::cost_json(&call.response.cost);
                cost["elapsed_ms"] = json!(call.engine_ms.max(0.0));
                (call.id.to_owned(), cost)
            })
            .collect();
        let all = run.calls.iter().collect::<Vec<_>>();
        let (warnings, notes) = messages(&all);
        let end = json!({
            "type": "end", "run_id": run_id,
            "finished_at": record::utc_rfc3339(SystemTime::now()),
            "outcome": "completed",
            "results": results,
            "totals": {
                "elapsed_ms": ms(timing.clock, Instant::now()),
                "steps": 3,
                "engine": { "elapsed_ms": ms(timing.registry_started, timing.registry_finished) },
                "per_map": per_map,
            },
            "warnings": warnings,
            "notes": notes,
        });
        vec![fan_out, fuse, cap_cut, end]
    }
}

#[derive(Clone, Copy)]
struct Timing {
    clock: Instant,
    registry_started: Instant,
    registry_finished: Instant,
}
