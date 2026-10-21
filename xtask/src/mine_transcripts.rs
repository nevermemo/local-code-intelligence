//! `cargo xtask mine-transcripts` (K6a-2): counts question shapes over
//! synthetic agent transcripts and nothing else. Input is the simulated
//! agent's trace shape (`xtask/src/sim`, `Trace::to_json`): either a `sim.json`
//! (`items[*].trace`, with the question joined by id from a gold file when the
//! item carries none) or a JSONL file of `{question, trace}` lines. No reader
//! of any real agent's files lives here; a converter of those, if one is ever
//! written, writes INTO this shape locally, so the repository holds no format
//! fact and no real transcript line.
//!
//! Per question the report keeps a `shape` (a closed wording table, a COUNTING
//! key that never routes a question: d391) and a `trace_class` read from the
//! steps alone:
//!
//! * `grep_easy`: the trace stopped on the gold within 2 calls of its first
//!   `search` or `probe` step;
//! * `agent_trap`: 3 or more `search`/`probe` steps, or a `search` after 2
//!   reads (a backtrack), and the gold was not reached;
//! * `core`: the gold was reached at depth 1 or more (a followed name) or
//!   after 2 or more reads;
//! * `unresolved`: anything else (a budget stop), counted.
//!
//! A trace that ends `stop = "answered"` (the agent stopped searching and
//! acted or answered; nothing verified the place it found) is classed like
//! `gold`. That is a weaker claim than a verified gold, so a real-transcript
//! class mix is a statement about how chains end, not about correctness.
//!
//! The output holds shapes, class counts, call and token quantiles and the
//! question templates (backticked names, paths and shas replaced, identifier
//! like words replaced, capped at 30 words): never a raw question and never an
//! `arg` of a step. Default output `test-results/transcripts/shapes.json`
//! (gitignored). With `--counts-only` the report drops `templates` and the
//! `unreadable` list, so every string left in it is a shape name, a class name,
//! a source kind or a key name (the closed vocabulary `SHAPES`, `CLASSES`,
//! `SOURCE_KINDS`, `KEYS`; a test pins it). That report is the one that may be
//! quoted when the input was a real transcript.

use crate::gold::load_gold;
use anyhow::{Context, Result, bail};
use clap::Args;
use lci_maps::exact_history::{HistoryQuery, classify as classify_history};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct MineTranscriptsArgs {
    /// A `sim.json` (items with a `trace`) or a `.jsonl` file of
    /// `{question, trace}` lines; repeatable.
    #[arg(long, required = true)]
    input: Vec<PathBuf>,
    /// A gold file whose `question` fields are joined by item `id` onto
    /// `sim.json` items (a sim item carries no question text of its own).
    #[arg(long)]
    gold: Option<PathBuf>,
    /// Where to write the shapes report.
    #[arg(long, default_value = "test-results/transcripts/shapes.json")]
    out: PathBuf,
    /// Write only counts and quantiles: no `templates`, no `unreadable` list.
    /// Every string in the report is then from a closed vocabulary.
    #[arg(long)]
    counts_only: bool,
}

/// The closed vocabulary of a `--counts-only` report (besides numbers); the
/// test that pins it is the only reader.
#[cfg(test)]
const SHAPES: [&str; 9] = [
    "history-h1",
    "history-h2",
    "history-h3",
    "where-configured",
    "what-tests",
    "who-calls",
    "how-flows",
    "where-is",
    "other",
];
#[cfg(test)]
const CLASSES: [&str; 4] = ["grep_easy", "agent_trap", "core", "unresolved"];
#[cfg(test)]
const SOURCE_KINDS: [&str; 2] = ["sim.json", "jsonl"];
#[cfg(test)]
const KEYS: [&str; 11] = [
    "sources",
    "kind",
    "records",
    "unreadable",
    "questions",
    "no_question",
    "counts",
    "calls",
    "tokens",
    "p50",
    "p90",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    call: u64,
    kind: String,
    depth: u64,
    cumulative_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Trace {
    stop: String,
    steps: Vec<Step>,
}

#[derive(Debug, Clone)]
struct Record {
    question: Option<String>,
    trace: Trace,
}

struct Source {
    kind: &'static str,
    records: Vec<Record>,
    unreadable: Vec<Value>,
}

// ------------------------------------------------------------------ reading

fn parse_trace(v: &Value) -> Option<Trace> {
    let steps = v.get("steps")?.as_array()?;
    let steps = steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            Some(Step {
                call: s
                    .get("call")
                    .and_then(Value::as_u64)
                    .unwrap_or(i as u64 + 1),
                kind: s.get("kind")?.as_str()?.to_string(),
                depth: s.get("depth").and_then(Value::as_u64).unwrap_or(0),
                cumulative_tokens: s
                    .get("cumulative_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Trace {
        stop: v
            .get("stop")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        steps,
    })
}

/// A `sim.json`: `items[*].trace`. An item without a readable trace is named
/// (by `id`) under `unreadable`, never an error.
fn parse_sim(text: &str, questions: &BTreeMap<String, String>) -> Result<Source> {
    let doc: Value = serde_json::from_str(text).context("sim.json is not valid JSON")?;
    let Some(items) = doc.get("items").and_then(Value::as_array) else {
        bail!("sim.json has no `items` array");
    };
    let mut source = Source {
        kind: "sim.json",
        records: Vec::new(),
        unreadable: Vec::new(),
    };
    for (i, item) in items.iter().enumerate() {
        let id = item.get("id").and_then(Value::as_str);
        match item.get("trace").and_then(parse_trace) {
            Some(trace) => {
                let question = item
                    .get("question")
                    .and_then(Value::as_str)
                    .or_else(|| id.and_then(|id| questions.get(id).map(String::as_str)))
                    .map(str::to_string);
                source.records.push(Record { question, trace });
            }
            None => source.unreadable.push(
                json!({"source": "sim.json", "id": id.map_or(format!("#{i}"), str::to_string)}),
            ),
        }
    }
    Ok(source)
}

/// JSONL of `{question, trace}`; a line that is not one is named by number.
fn parse_jsonl(text: &str) -> Source {
    let mut source = Source {
        kind: "jsonl",
        records: Vec::new(),
        unreadable: Vec::new(),
    };
    for (n, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let parsed = serde_json::from_str::<Value>(line).ok().and_then(|v| {
            Some(Record {
                question: v
                    .get("question")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                trace: parse_trace(v.get("trace")?)?,
            })
        });
        match parsed {
            Some(record) => source.records.push(record),
            None => source
                .unreadable
                .push(json!({"source": "jsonl", "line": n + 1})),
        }
    }
    source
}

// ------------------------------------------------------------------ classes

fn is_search(kind: &str) -> bool {
    matches!(kind, "search" | "probe")
}

fn is_read(kind: &str) -> bool {
    kind == "read" || kind.ends_with("-read")
}

/// The class of a trace from its steps alone.
fn trace_class(trace: &Trace) -> &'static str {
    // `answered` (the agent stopped looking and answered or acted) counts like
    // the gold: the report cannot tell whether the place it found was right.
    let gold = matches!(trace.stop.as_str(), "gold" | "answered");
    let steps = &trace.steps;
    let Some(last) = steps.last() else {
        return "unresolved";
    };
    let first_search = steps.iter().find(|s| is_search(&s.kind));
    if gold && first_search.is_some_and(|s| last.call <= s.call + 2) {
        return "grep_easy";
    }
    if !gold {
        let searches = steps.iter().filter(|s| is_search(&s.kind)).count();
        let mut reads = 0;
        let mut backtrack = false;
        for s in steps {
            if s.kind == "search" && reads >= 2 {
                backtrack = true;
            }
            if is_read(&s.kind) {
                reads += 1;
            }
        }
        if searches >= 3 || backtrack {
            return "agent_trap";
        }
    }
    let reads_before = steps[..steps.len() - 1]
        .iter()
        .filter(|s| is_read(&s.kind))
        .count();
    if gold && (last.depth >= 1 || reads_before >= 2) {
        return "core";
    }
    "unresolved"
}

/// The question shape: a closed table over the wording. A counting key for
/// reports; nothing routes by it.
fn shape(question: Option<&str>) -> &'static str {
    let Some(q) = question else {
        return "other";
    };
    let lower = q.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |w: &str| words.contains(&w);
    let any = |phrases: &[&str]| phrases.iter().any(|p| lower.contains(p));
    match classify_history(q) {
        HistoryQuery::Creation { .. } => return "history-h1",
        HistoryQuery::LastModified { .. } => return "history-h2",
        HistoryQuery::Why { .. } if has("why") => return "history-h3",
        HistoryQuery::Why { .. } => {}
    }
    if has("where") && words.iter().any(|w| w.starts_with("config")) {
        "where-configured"
    } else if has("test") || has("tests") {
        "what-tests"
    } else if any(&[
        "who calls",
        "what calls",
        "callers of",
        "called by",
        "called from",
        "what uses",
        "who uses",
        "what depends",
        "what breaks",
        "what would break",
        "references to",
    ]) {
        "who-calls"
    } else if has("how") || has("flow") || any(&["path from", "trace "]) {
        "how-flows"
    } else if has("where") || any(&["which file", "find the"]) {
        "where-is"
    } else {
        "other"
    }
}

// ---------------------------------------------------------------- templates

/// A token that looks like a file path: a `/` or a known extension.
fn path_like(token: &str) -> bool {
    const EXT: [&str; 8] = [".rs", ".toml", ".md", ".json", ".py", ".ts", ".js", ".yml"];
    !token.is_empty()
        && !token.starts_with('-')
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
        && (token.contains('/') || EXT.iter().any(|e| token.ends_with(e)))
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

fn sha_like(token: &str) -> bool {
    (7..=40).contains(&token.len())
        && token.chars().all(|c| c.is_ascii_hexdigit())
        && token.chars().any(|c| c.is_ascii_digit())
}

/// A word that reads as an identifier: `::`, `_`, a digit or an inner capital.
fn ident_like(word: &str) -> bool {
    word.contains("::")
        || word.contains('_')
        || word.chars().any(|c| c.is_ascii_digit())
        || word.chars().skip(1).any(|c| c.is_ascii_uppercase())
}

const TEMPLATE_WORDS: usize = 30;

/// The question with every name, path, sha and identifier-like word replaced,
/// lower-cased and capped: its shape, not its text.
fn template(question: &str) -> String {
    // Backticked spans first (they may hold spaces), each as one placeholder.
    let flat: String = question
        .split('`')
        .enumerate()
        .map(|(i, segment)| {
            if i % 2 == 1 {
                if path_like(segment.trim()) {
                    "<path>"
                } else {
                    "<ident>"
                }
                .to_string()
            } else {
                segment.to_string()
            }
        })
        .collect();
    const EDGE: &str = "(),;:'\"?!.";
    let mut out: Vec<String> = Vec::new();
    for token in flat.split_whitespace() {
        let core = token.trim_matches(|c: char| EDGE.contains(c));
        let lead_len = token.len() - token.trim_start_matches(|c: char| EDGE.contains(c)).len();
        let (lead, tail) = if core.is_empty() {
            (token, "")
        } else {
            (&token[..lead_len], &token[lead_len + core.len()..])
        };
        let word = if core.is_empty() {
            String::new()
        } else if path_like(core) {
            "<path>".into()
        } else if sha_like(core) {
            "<sha>".into()
        } else if ident_like(core) {
            "<ident>".into()
        } else {
            core.to_ascii_lowercase()
        };
        out.push(format!("{lead}{word}{tail}"));
    }
    let capped = out.len() > TEMPLATE_WORDS;
    out.truncate(TEMPLATE_WORDS);
    let mut text = out.join(" ");
    if capped {
        text.push_str(" ...");
    }
    text
}

// ------------------------------------------------------------------- report

/// Nearest-rank quantile of a sorted list.
fn quantile(sorted: &[u64], q: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

fn report(sources: &[Source], counts_only: bool) -> Value {
    let mut counts: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut templates: BTreeMap<String, usize> = BTreeMap::new();
    let mut calls: Vec<u64> = Vec::new();
    let mut tokens: Vec<u64> = Vec::new();
    let mut no_question = 0usize;
    let mut questions = 0usize;
    for record in sources.iter().flat_map(|s| &s.records) {
        questions += 1;
        let class = trace_class(&record.trace);
        *counts
            .entry(shape(record.question.as_deref()))
            .or_default()
            .entry(class)
            .or_default() += 1;
        match &record.question {
            Some(q) => *templates.entry(template(q)).or_default() += 1,
            None => no_question += 1,
        }
        let last = record.trace.steps.last();
        calls.push(last.map_or(0, |s| s.call));
        tokens.push(last.map_or(0, |s| s.cumulative_tokens));
    }
    calls.sort_unstable();
    tokens.sort_unstable();
    let mut templates: Vec<(String, usize)> = templates.into_iter().collect();
    templates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut doc = json!({
        "sources": sources.iter().map(|s| json!({
            "kind": s.kind, "records": s.records.len(), "unreadable": s.unreadable.len(),
        })).collect::<Vec<_>>(),
        "questions": questions,
        "no_question": no_question,
        "counts": counts,
        "calls": {"p50": quantile(&calls, 0.5), "p90": quantile(&calls, 0.9)},
        "tokens": {"p50": quantile(&tokens, 0.5), "p90": quantile(&tokens, 0.9)},
    });
    if !counts_only {
        doc["unreadable"] = json!(
            sources
                .iter()
                .flat_map(|s| s.unreadable.clone())
                .collect::<Vec<_>>()
        );
        doc["templates"] = json!(
            templates
                .iter()
                .map(|(t, n)| json!({"template": t, "n": n}))
                .collect::<Vec<_>>()
        );
    }
    doc
}

fn read_source(path: &Path, questions: &BTreeMap<String, String>) -> Result<Source> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if path.extension().is_some_and(|e| e == "jsonl") {
        Ok(parse_jsonl(&text))
    } else {
        parse_sim(&text, questions).with_context(|| format!("in {}", path.display()))
    }
}

pub fn run(args: MineTranscriptsArgs) -> Result<()> {
    let mut questions = BTreeMap::new();
    if let Some(gold) = &args.gold {
        for item in load_gold(gold)? {
            if let (Some(id), Some(q)) = (
                item.get("id").and_then(Value::as_str),
                item.get("question").and_then(Value::as_str),
            ) {
                questions.insert(id.to_string(), q.to_string());
            }
        }
    }
    let sources = args
        .input
        .iter()
        .map(|p| read_source(p, &questions))
        .collect::<Result<Vec<_>>>()?;
    let doc = report(&sources, args.counts_only);
    if let Some(parent) = args.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&args.out, serde_json::to_string_pretty(&doc)? + "\n")
        .with_context(|| format!("failed to write {}", args.out.display()))?;
    eprintln!(
        "mine-transcripts: {} questions from {} input(s) -> {}",
        doc["questions"],
        args.input.len(),
        args.out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(call: u64, kind: &str, depth: u64, cumulative: u64) -> Value {
        json!({"call": call, "kind": kind, "arg": format!("arg-{call}"), "tokens": 10,
               "cumulative_tokens": cumulative, "depth": depth})
    }

    fn line(question: &str, stop: &str, steps: Vec<Value>) -> String {
        json!({"question": question, "trace": {"terms": [], "stop": stop, "steps": steps}})
            .to_string()
    }

    fn three_lines() -> String {
        [
            // search, then one read that hits the gold: within 2 calls
            line(
                "Where is `Alpha` defined?",
                "gold",
                vec![step(1, "search", 0, 100), step(2, "read", 0, 400)],
            ),
            // three searches, the budget ends it
            line(
                "What breaks if `Beta` changes?",
                "calls",
                vec![
                    step(1, "probe", 0, 100),
                    step(2, "probe", 0, 400),
                    step(3, "search", 0, 900),
                ],
            ),
            // a followed name: depth 1 at the stop
            line(
                "How does data flow from `Gamma` to `Delta`?",
                "gold",
                vec![
                    step(1, "search", 0, 100),
                    step(2, "read", 0, 500),
                    step(3, "follow-grep", 1, 900),
                    step(4, "follow-read", 1, 1500),
                ],
            ),
        ]
        .join("\n")
    }

    #[test]
    fn three_synthetic_transcripts_land_in_three_classes() {
        let source = parse_jsonl(&three_lines());
        assert!(source.unreadable.is_empty());
        let r = report(&[source], false);
        assert_eq!(r["questions"], 3);
        assert_eq!(r["counts"]["where-is"], json!({"grep_easy": 1}));
        assert_eq!(r["counts"]["who-calls"], json!({"agent_trap": 1}));
        assert_eq!(r["counts"]["how-flows"], json!({"core": 1}));
        assert_eq!(r["calls"], json!({"p50": 3, "p90": 4}));
        assert_eq!(r["tokens"], json!({"p50": 900, "p90": 1500}));
        let templates: Vec<&str> = r["templates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["template"].as_str().unwrap())
            .collect();
        assert!(templates.contains(&"where is <ident> defined?"));
        assert!(templates.contains(&"how does data flow from <ident> to <ident>?"));
        // a trace that ends on a budget with one search is unresolved, and so is no trace
        let budget = Trace {
            stop: "tokens".into(),
            steps: vec![Step {
                call: 1,
                kind: "search".into(),
                depth: 0,
                cumulative_tokens: 5,
            }],
        };
        assert_eq!(trace_class(&budget), "unresolved");
        // a search after two reads, no gold, is a backtrack
        let back = Trace {
            stop: "calls".into(),
            steps: ["read", "read", "search"]
                .iter()
                .enumerate()
                .map(|(i, k)| Step {
                    call: i as u64 + 1,
                    kind: (*k).into(),
                    depth: 0,
                    cumulative_tokens: 0,
                })
                .collect(),
        };
        assert_eq!(trace_class(&back), "agent_trap");
    }

    #[test]
    fn shapes_follow_a_closed_wording_table() {
        for (question, want) in [
            ("Which commit first created `Foo`?", "history-h1"),
            (
                "What is the most recent commit that modified `Foo`?",
                "history-h2",
            ),
            ("Why was the retry removed?", "history-h3"),
            ("Where is the timeout configured?", "where-configured"),
            ("Which tests cover `Foo`?", "what-tests"),
            ("Who calls `Foo::run`?", "who-calls"),
            ("How does a request reach the store?", "how-flows"),
            ("Where is `Foo` defined?", "where-is"),
            ("Tell me about the latest release", "other"),
        ] {
            assert_eq!(shape(Some(question)), want, "{question}");
        }
        assert_eq!(shape(None), "other");
    }

    #[test]
    fn a_sim_json_is_read_as_transcripts() {
        let trace = json!({"terms": [], "stop": "gold", "steps": [step(1, "search", 0, 50)]});
        let doc = json!({"arm": "A0", "items": [
            {"id": "a", "trace": trace},
            {"id": "b", "trace": trace},
            {"id": "c", "tool_calls": 3}
        ]});
        let mut questions = BTreeMap::new();
        questions.insert("a".to_string(), "Where is `Alpha` defined?".to_string());
        let source = parse_sim(&doc.to_string(), &questions).unwrap();
        assert_eq!(source.records.len(), 2);
        assert_eq!(
            source.unreadable,
            vec![json!({"source": "sim.json", "id": "c"})]
        );
        let r = report(&[source], false);
        assert_eq!(r["questions"], 2);
        assert_eq!(r["no_question"], 1);
        assert_eq!(r["unreadable"], json!([{"source": "sim.json", "id": "c"}]));
        assert_eq!(r["counts"]["where-is"], json!({"grep_easy": 1}));
        assert_eq!(r["counts"]["other"], json!({"grep_easy": 1}));
        assert!(parse_sim("{}", &questions).is_err());
        // a JSONL line without a trace is named by its line number
        let jsonl = parse_jsonl("{\"question\": \"x\"}\nnot json\n");
        assert_eq!(jsonl.unreadable.len(), 2);
        assert_eq!(jsonl.unreadable[1], json!({"source": "jsonl", "line": 2}));
    }

    #[test]
    fn nothing_of_the_input_text_beyond_templates_reaches_the_output() {
        let question = "Where is `ZzTopSecret` defined near secret/dir/file.rs and abc1234def?";
        let text = line(
            question,
            "gold",
            vec![step(1, "search", 0, 10), step(2, "read", 0, 20)],
        );
        let r = report(&[parse_jsonl(&text)], false);
        let out = serde_json::to_string(&r).unwrap();
        for leaked in [
            "ZzTopSecret",
            "secret/dir",
            "file.rs",
            "abc1234def",
            "arg-1",
            "arg-2",
            question,
        ] {
            assert!(!out.contains(leaked), "{leaked} reached the output");
        }
        assert_eq!(
            r["templates"][0]["template"],
            "where is <ident> defined near <path> and <sha>?"
        );
        // long and identifier-like text is capped and replaced
        let long = format!("{} snake_case CamelCase", "word ".repeat(40));
        let t = template(&long);
        assert!(t.ends_with(" ..."));
        assert_eq!(t.split_whitespace().count(), TEMPLATE_WORDS + 1);
        assert_eq!(
            template("see snake_case and CamelCase"),
            "see <ident> and <ident>"
        );
    }

    #[test]
    fn counts_only_output_holds_only_vocabulary_strings_and_numbers() {
        let text = [
            line(
                "Where is `ZzTopSecret` defined near secret/dir/file.rs and abc1234def, odd wordquux?",
                "gold",
                vec![step(1, "search", 0, 10), step(2, "read", 0, 20)],
            ),
            line(
                "Who calls `Qq::Rr`? plainwordzebra",
                "answered",
                vec![step(1, "probe", 0, 10)],
            ),
            "not json at all secretline".to_string(),
        ]
        .join("\n");
        let full = report(&[parse_jsonl(&text)], false);
        assert!(full.get("templates").is_some() && full.get("unreadable").is_some());
        let r = report(&[parse_jsonl(&text)], true);
        assert!(
            r.get("templates").is_none(),
            "templates in a counts-only report"
        );
        assert!(
            r.get("unreadable").is_none(),
            "unreadable list in a counts-only report"
        );
        assert_eq!(r["sources"][0]["unreadable"], 1);
        let vocabulary: Vec<&str> = SHAPES
            .iter()
            .chain(&CLASSES)
            .chain(&SOURCE_KINDS)
            .copied()
            .collect();
        fn walk(v: &Value, vocabulary: &[&str], strings: &mut usize) {
            match v {
                Value::String(s) => {
                    *strings += 1;
                    assert!(
                        vocabulary.contains(&s.as_str()),
                        "string {s:?} is not vocabulary"
                    );
                }
                Value::Number(_) | Value::Null => {}
                Value::Array(items) => items.iter().for_each(|i| walk(i, vocabulary, strings)),
                Value::Object(map) => {
                    for (k, child) in map {
                        assert!(
                            KEYS.contains(&k.as_str())
                                || SHAPES.contains(&k.as_str())
                                || CLASSES.contains(&k.as_str()),
                            "key {k:?} is not vocabulary"
                        );
                        walk(child, vocabulary, strings);
                    }
                }
                Value::Bool(_) => panic!("a boolean in a counts-only report"),
            }
        }
        let mut strings = 0;
        walk(&r, &vocabulary, &mut strings);
        assert_eq!(strings, 1, "only the source kind is a string value");
        let out = serde_json::to_string(&r).unwrap();
        for leaked in [
            "ZzTopSecret",
            "secret",
            "file.rs",
            "abc1234def",
            "wordquux",
            "zebra",
            "Qq",
        ] {
            assert!(!out.contains(leaked), "{leaked} reached the output");
        }
        // every shape the miner can name is in the vocabulary
        // (one wording per branch of `shape()`, so a new branch cannot slip past the vocabulary)
        let mut seen = std::collections::BTreeSet::new();
        for q in [
            None,
            Some("x"),
            Some("Where is it?"),
            Some("Who calls it?"),
            Some("Which commit first created `Foo`?"),
            Some("What is the most recent commit that modified `Foo`?"),
            Some("when was x created"),
            Some("when was x last changed"),
            Some("why did x change"),
            Some("where is x configured"),
            Some("which tests cover x"),
            Some("how does x flow"),
        ] {
            let s = shape(q);
            assert!(SHAPES.contains(&s), "{s} is not in the vocabulary");
            seen.insert(s);
        }
        assert_eq!(
            seen.len(),
            SHAPES.len(),
            "a shape has no wording here: {seen:?}"
        );
    }

    #[test]
    fn an_answered_stop_classes_like_gold() {
        let trace = |stop: &str, kinds: &[&str]| Trace {
            stop: stop.into(),
            steps: kinds
                .iter()
                .enumerate()
                .map(|(i, k)| Step {
                    call: i as u64 + 1,
                    kind: (*k).into(),
                    depth: 0,
                    cumulative_tokens: 0,
                })
                .collect(),
        };
        // a search and one read, then the answer: within 2 calls of the first search
        assert_eq!(
            trace_class(&trace("answered", &["search", "read"])),
            "grep_easy"
        );
        // a search and three reads, then the answer: several reads
        assert_eq!(
            trace_class(&trace("answered", &["search", "read", "read", "read"])),
            "core"
        );
        // the same chains classed exactly as under `gold`
        for kinds in [
            &["search", "read"][..],
            &["search", "read", "read", "read"][..],
        ] {
            assert_eq!(
                trace_class(&trace("answered", kinds)),
                trace_class(&trace("gold", kinds))
            );
        }
        // an abandoned or capped chain is not helped by the new stop
        assert_eq!(
            trace_class(&trace("exhausted", &["search", "read"])),
            "unresolved"
        );
        assert_eq!(
            trace_class(&trace("calls", &["search", "probe", "search"])),
            "agent_trap"
        );
    }
}
