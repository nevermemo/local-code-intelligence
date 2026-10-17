//! The offline RunRecord harness (spike slice S4, the part that needs no
//! live model; spec `docs/spike/multimap/specs/run-record.md`, section 5).
//! It reads RunRecords back, checking every whole line against the schema;
//! scores a record's results against a gold query with the same metric code
//! as `evaluate` ([`query_metrics`]); replays every `fuse` step from the
//! recorded map hits and the plan's weights and checks every recorded score
//! bit for bit; and checks contract C5 offline (every used hit's evidence
//! carries a label its map's card lists).

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use lci_core::node::NodeId;
use lci_maps::run_record::validate_line;
use serde_json::Value;

use super::metrics::query_metrics;
use super::types::{EvalQuery, EvaluationFile, QueryMetrics, ResultEvidence};

/// A RunRecord as read back.
#[derive(Debug, Clone)]
pub struct RunRecordFile {
    pub run: Value,
    pub steps: Vec<Value>,
    /// `None` when the run did not finish (the writer was killed or
    /// failed): the record is a prefix, and only its steps can be used.
    pub end: Option<Value>,
    /// The file ended inside a line (a writer killed mid-write). That line
    /// is dropped; every line before it was checked.
    pub torn_tail: bool,
}

/// What [`RunRecordFile::replay_fuse`] checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseReplay {
    pub fuse_steps: usize,
    /// Nodes fused, summed over the steps.
    pub nodes: usize,
    /// Frontier rows whose score and contributions were compared bit for
    /// bit, summed over the steps (fewer than `nodes` when the record capped
    /// its rows).
    pub rows_checked: usize,
}

fn field<'v>(value: &'v Value, name: &str) -> Result<&'v Value> {
    value
        .get(name)
        .with_context(|| format!("missing field {name:?}"))
}

fn text<'v>(value: &'v Value, name: &str) -> Result<&'v str> {
    field(value, name)?
        .as_str()
        .with_context(|| format!("{name:?} is not a string"))
}

fn array<'v>(value: &'v Value, name: &str) -> Result<&'v [Value]> {
    match value.get(name) {
        None => Ok(&[]),
        Some(items) => items
            .as_array()
            .map(Vec::as_slice)
            .with_context(|| format!("{name:?} is not an array")),
    }
}

/// A recorded number narrowed to the `f32` it was written from.
fn float(value: &Value, name: &str) -> Result<f32> {
    field(value, name)?
        .as_f64()
        .map(|number| number as f32)
        .with_context(|| format!("{name:?} is not a number"))
}

fn used(call: &Value) -> bool {
    matches!(
        call.get("status").and_then(Value::as_str),
        Some("ok" | "partial")
    )
}

impl RunRecordFile {
    pub fn read(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path)
            .with_context(|| format!("failed to read RunRecord {}", path.display()))?;
        Self::parse_bytes(&raw).with_context(|| format!("RunRecord {}", path.display()))
    }

    /// [`Self::parse`] over raw bytes. Only the whole lines must be UTF-8: a
    /// writer killed mid-write can cut its last line inside a multibyte
    /// character, and that torn tail is dropped like any other.
    pub fn parse_bytes(raw: &[u8]) -> Result<Self> {
        let whole = raw
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |last| last + 1);
        let body = std::str::from_utf8(&raw[..whole]).context("a whole line is not UTF-8")?;
        let mut record = Self::parse(body)?;
        record.torn_tail = whole < raw.len();
        Ok(record)
    }

    /// Every `*.jsonl` record in `dir`, in file-name (run id, so start
    /// time) order.
    pub fn read_dir(dir: &Path) -> Result<Vec<(PathBuf, Self)>> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("failed to list {}", dir.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()?;
        paths.retain(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
        paths.sort();
        paths
            .into_iter()
            .map(|path| Self::read(&path).map(|record| (path, record)))
            .collect()
    }

    /// Parses a record: the first line is `run`, then `step` lines with
    /// `seq` 1, 2, ..., then at most one `end`, nothing after it, and the
    /// same `run_id` throughout. Every whole line must pass the schema; an
    /// unterminated last line is dropped and reported in `torn_tail`.
    pub fn parse(raw: &str) -> Result<Self> {
        let (body, torn_tail) = match raw.rfind('\n') {
            Some(last) => (&raw[..=last], last + 1 < raw.len()),
            None => ("", !raw.is_empty()),
        };
        let mut lines = body.lines().enumerate().map(|(index, line)| {
            let value: Value = serde_json::from_str(line)
                .with_context(|| format!("line {}: not JSON", index + 1))?;
            validate_line(&value)
                .map_err(|error| anyhow::anyhow!("line {}: {error}", index + 1))?;
            Ok::<_, anyhow::Error>((index + 1, value))
        });
        let (_, run) = lines.next().context("no run line")??;
        ensure!(
            text(&run, "type")? == "run",
            "line 1: the first line is not the run line"
        );
        let run_id = text(&run, "run_id")?.to_owned();
        let mut steps = Vec::new();
        let mut end = None;
        for line in lines {
            let (number, value) = line?;
            ensure!(end.is_none(), "line {number}: a line after the end line");
            ensure!(
                text(&value, "run_id")? == run_id,
                "line {number}: another run's line"
            );
            match text(&value, "type")? {
                "step" => {
                    let seq = field(&value, "seq")?.as_u64().unwrap_or(0);
                    ensure!(
                        seq == steps.len() as u64 + 1,
                        "line {number}: step seq {seq} after {}",
                        steps.len()
                    );
                    steps.push(value);
                }
                "end" => end = Some(value),
                other => bail!("line {number}: a second {other} line"),
            }
        }
        Ok(Self {
            run,
            steps,
            end,
            torn_tail,
        })
    }

    pub fn run_id(&self) -> &str {
        self.run["run_id"].as_str().unwrap_or_default()
    }

    pub fn query_text(&self) -> &str {
        self.run["query"]["text"].as_str().unwrap_or_default()
    }

    /// The end line; an error naming the missing outcome for a record cut
    /// short, never a parse error.
    pub fn end(&self) -> Result<&Value> {
        self.end.as_ref().with_context(|| {
            format!(
                "RunRecord {}: outcome missing (the run did not finish; {} steps recorded)",
                self.run_id(),
                self.steps.len()
            )
        })
    }

    /// The final results as the evidence `evaluate` scores: rank, path (from
    /// the NodeId), lines, source role (required) and snippet (required when
    /// the run captured snippets). The language is not recorded and is left
    /// empty; no metric reads it.
    pub fn result_evidence(&self) -> Result<Vec<ResultEvidence>> {
        let snippets = self.run["capture"]["snippets"].as_bool().unwrap_or(false);
        array(self.end()?, "results")?
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let node: NodeId = text(row, "node")?.parse()?;
                let path = node
                    .path()
                    .with_context(|| format!("result {node} names no file"))?;
                let line = |name| {
                    row.get(name)
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                        .with_context(|| format!("result {node}: no {name}"))
                };
                let snippet = match row.get("snippet").and_then(Value::as_str) {
                    Some(snippet) => snippet.to_owned(),
                    None if snippets => bail!("result {node}: snippets were captured, not here"),
                    None => String::new(),
                };
                Ok(ResultEvidence {
                    rank: index + 1,
                    relative_file_path: path.to_owned(),
                    language: String::new(),
                    source_role: text(row, "source_role")
                        .with_context(|| format!("result {node}"))?
                        .to_owned(),
                    start_line: line("start_line")?,
                    end_line: line("end_line")?,
                    snippet,
                    reranker_score: None,
                })
            })
            .collect()
    }

    /// Scores the record's results against `query` with the metric code of
    /// `evaluate`. Refuses a query for another text, and a snippet
    /// expectation when the record has no snippets.
    pub fn score(&self, query: &EvalQuery) -> Result<QueryMetrics> {
        ensure!(
            query.query == self.query_text(),
            "the record answers {:?}, not {:?}",
            self.query_text(),
            query.query
        );
        let snippets = self.run["capture"]["snippets"].as_bool().unwrap_or(false);
        ensure!(
            query.required_snippet.is_none() || snippets,
            "the query expects a snippet but the record has none (capture.snippets is off)"
        );
        let outcome = text(self.end()?, "outcome")?;
        ensure!(
            outcome == "completed",
            "the run ended {outcome:?}; only a completed run is scored"
        );
        ensure!(
            query.filters.is_none(),
            "the query has filters; recorded registry runs are unfiltered"
        );
        if let Some(top_k) = query.top_k {
            let planned = self.run["plan"]["top_k"].as_u64();
            ensure!(
                planned == Some(top_k as u64),
                "the query asks for top_k {top_k}, the run used {planned:?}"
            );
        }
        // The registry path has no reranker (no `rerank` step yet).
        Ok(query_metrics(query, &self.result_evidence()?, false))
    }

    /// The query of `file` this record answers: the one with the record's
    /// query text. Ambiguity is an error.
    pub fn gold_query<'f>(&self, file: &'f EvaluationFile) -> Result<&'f EvalQuery> {
        let mut matching = file
            .queries
            .iter()
            .filter(|query| query.query == self.query_text());
        let query = matching
            .next()
            .with_context(|| format!("no gold query for {:?}", self.query_text()))?;
        ensure!(
            matching.next().is_none(),
            "more than one gold query for {:?}",
            self.query_text()
        );
        Ok(query)
    }

    /// Contract C5 offline: every hit of every used map call carries
    /// evidence, and every label on it is one the plan lists for that map.
    pub fn check_labels(&self) -> Result<()> {
        let mut cards: HashMap<&str, HashSet<&str>> = HashMap::new();
        for map in array(&self.run["plan"], "maps")? {
            let labels = array(map, "labels")?.iter().filter_map(Value::as_str);
            cards.insert(text(map, "map")?, labels.collect());
        }
        for step in &self.steps {
            for call in array(step, "map_calls")?.iter().filter(|call| used(call)) {
                let map = text(call, "map")?;
                let card = cards
                    .get(map)
                    .with_context(|| format!("map {map} is not in the plan"))?;
                for hit in array(call, "hits")? {
                    let evidence = array(hit, "evidence")?;
                    ensure!(!evidence.is_empty(), "map {map}: a hit without a label");
                    for item in evidence {
                        let label = text(item, "label")?;
                        ensure!(
                            card.contains(label),
                            "map {map}: label {label} is not on its card"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Replays every `fuse` step: re-adds `weight / (rrf_k + rank)` for each
    /// hit of the used calls of the preceding `fan_out` step, in recorded
    /// (fusion) order, then each used prior call's values, in `f32` as the
    /// registry does, and checks against the recorded frontier: the pool
    /// size; every row's score and per-map contributions bit for bit; ranks
    /// 1, 2, ...; the order (score descending, ties by path, lines and
    /// content hash); and that no node left out of a capped row list belongs
    /// before the last row (by score, or on a tie by path: the lines of a
    /// left-out node are not recorded).
    pub fn replay_fuse(&self) -> Result<FuseReplay> {
        let plan = field(&self.run, "plan")?;
        let rrf_k = float(plan, "rrf_k")?;
        let mut weights = HashMap::new();
        for map in array(plan, "maps")? {
            weights.insert(text(map, "map")?, float(map, "weight")?);
        }
        let mut replay = FuseReplay {
            fuse_steps: 0,
            nodes: 0,
            rows_checked: 0,
        };
        let mut fan_out = None;
        for step in &self.steps {
            match text(step, "kind")? {
                "fan_out" => fan_out = Some(step),
                "fuse" => {
                    let fan_out = fan_out.context("a fuse step with no fan_out step before it")?;
                    let seq = step["seq"].as_u64().unwrap_or_default();
                    let (nodes, rows) = replay_step(fan_out, step, rrf_k, &weights)
                        .with_context(|| format!("fuse step {seq}"))?;
                    replay.fuse_steps += 1;
                    replay.nodes += nodes;
                    replay.rows_checked += rows;
                }
                _ => {}
            }
        }
        ensure!(replay.fuse_steps > 0, "no fuse step to replay");
        Ok(replay)
    }
}

struct Replayed<'v> {
    node: &'v str,
    score: f32,
    parts: Vec<(&'v str, f32)>,
}

fn replay_step(
    fan_out: &Value,
    fuse: &Value,
    rrf_k: f32,
    weights: &HashMap<&str, f32>,
) -> Result<(usize, usize)> {
    let mut pool: Vec<Replayed<'_>> = Vec::new();
    let mut position: HashMap<&str, usize> = HashMap::new();
    for call in array(fan_out, "map_calls")?
        .iter()
        .filter(|call| used(call))
    {
        let map = text(call, "map")?;
        let weight = *weights
            .get(map)
            .with_context(|| format!("map {map} has no weight in the plan"))?;
        for hit in array(call, "hits")? {
            let node = text(hit, "node")?;
            let rank = field(hit, "rank")?
                .as_u64()
                .and_then(|rank| u32::try_from(rank).ok())
                .context("a rank that is not a u32")?;
            let value = weight / (rrf_k + rank as f32);
            match position.get(node) {
                Some(&at) => {
                    pool[at].score += value;
                    pool[at].parts.push((map, value));
                }
                None => {
                    position.insert(node, pool.len());
                    pool.push(Replayed {
                        node,
                        score: 0.0 + value,
                        parts: vec![(map, value)],
                    });
                }
            }
        }
    }
    for call in array(fuse, "map_calls")?.iter().filter(|call| used(call)) {
        let map = text(call, "map")?;
        for hit in array(call, "hits")? {
            let node = text(hit, "node")?;
            let value = float(hit, "score")
                .with_context(|| format!("prior {map} has no value for {node}"))?;
            let &at = position
                .get(node)
                .with_context(|| format!("prior {map} answered for {node}, outside the pool"))?;
            pool[at].score += value;
            pool[at].parts.push((map, value));
        }
    }

    let frontier = field(fuse, "frontier")?;
    let size = field(frontier, "size")?.as_u64().unwrap_or(u64::MAX);
    ensure!(
        size == pool.len() as u64,
        "the frontier holds {size} nodes, the replay {}",
        pool.len()
    );
    let rows = array(frontier, "rows")?;
    ensure!(
        size == 0 || !rows.is_empty(),
        "the frontier holds {size} nodes but no rows, so nothing can be checked"
    );
    let mut seen = HashSet::new();
    let mut previous: Option<(f32, TieKey, usize)> = None;
    for (index, row) in rows.iter().enumerate() {
        let node = text(row, "node")?;
        let rank = field(row, "fused_rank")?.as_u64().unwrap_or_default();
        ensure!(
            rank == index as u64 + 1,
            "{node}: fused_rank {rank} at row {}",
            index + 1
        );
        let replayed = &pool[*position
            .get(node)
            .with_context(|| format!("{node} is in the frontier but no map returned it"))?];
        seen.insert(node);
        let recorded = float(row, "score")?;
        ensure!(
            recorded.to_bits() == replayed.score.to_bits(),
            "{node}: recorded score {recorded} ({:#010x}), replayed {} ({:#010x})",
            recorded.to_bits(),
            replayed.score,
            replayed.score.to_bits()
        );
        let contributions = field(row, "contributions")?
            .as_object()
            .context("contributions is not an object")?;
        ensure!(
            contributions.len() == replayed.parts.len(),
            "{node}: {} contributions recorded, {} replayed",
            contributions.len(),
            replayed.parts.len()
        );
        for (map, value) in &replayed.parts {
            let recorded = contributions
                .get(*map)
                .and_then(Value::as_f64)
                .with_context(|| format!("{node}: no contribution from {map}"))?
                as f32;
            ensure!(
                recorded.to_bits() == value.to_bits(),
                "{node}: {map} recorded {recorded}, replayed {value}"
            );
        }
        let key = TieKey::of(node, row)?;
        // The registry's sort is stable: nodes equal on score and tie key
        // (same-line twins, N-5) keep their order of first contribution.
        let inserted = position[node];
        if let Some((score, before, before_inserted)) = &previous {
            let in_order = match score.total_cmp(&replayed.score) {
                Ordering::Greater => true,
                Ordering::Equal => {
                    before.cmp(&key).then(before_inserted.cmp(&inserted)) == Ordering::Less
                }
                Ordering::Less => false,
            };
            ensure!(
                in_order,
                "{node} at row {} is out of the fused order",
                index + 1
            );
        }
        previous = Some((replayed.score, key, inserted));
    }
    if let Some((last, last_key, _)) = previous {
        for left_out in pool.iter().filter(|r| !seen.contains(r.node)) {
            // A left-out node may tie the last row only if it sorts after
            // it. Its lines are not recorded, so on a tie only the path
            // (the first tie key) can be compared.
            let before_last = match left_out.score.total_cmp(&last) {
                Ordering::Greater => true,
                Ordering::Equal => {
                    let path = left_out.node.parse::<NodeId>().ok();
                    match (path.as_ref().and_then(NodeId::path), &last_key.chunk) {
                        (Some(path), Some((last_path, ..))) => path < last_path.as_str(),
                        _ => left_out.node < last_key.node.as_str(),
                    }
                }
                Ordering::Less => false,
            };
            ensure!(
                !before_last,
                "{} scores {} and belongs before the last of the {} rows",
                left_out.node,
                left_out.score,
                rows.len()
            );
        }
    }
    Ok((pool.len(), rows.len()))
}

/// Today's tie rule after the score (`lci_maps::fused_order`): path, start
/// line, end line and content hash when both nodes are chunks with lines;
/// otherwise the NodeId strings.
#[derive(Debug)]
struct TieKey {
    node: String,
    chunk: Option<(String, u32, u32, String)>,
}

impl TieKey {
    fn of(node: &str, row: &Value) -> Result<Self> {
        let parsed: NodeId = node.parse()?;
        let line = |name| row.get(name).and_then(Value::as_u64).map(|n| n as u32);
        let chunk = match (&parsed, line("start_line"), line("end_line")) {
            (
                NodeId::Chunk {
                    path, content_hash, ..
                },
                Some(start),
                Some(end),
            ) => Some((path.clone(), start, end, content_hash.clone())),
            _ => None,
        };
        Ok(Self {
            node: node.to_owned(),
            chunk,
        })
    }

    fn cmp(&self, other: &Self) -> Ordering {
        match (&self.chunk, &other.chunk) {
            (Some(a), Some(b)) => a.cmp(b),
            _ => self.node.cmp(&other.node),
        }
    }
}

#[cfg(test)]
mod tests;
