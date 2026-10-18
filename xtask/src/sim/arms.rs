//! Arms A1 and A2: the one `lci` call that goes in front of the A0 chain.
//!
//! * A1 (LCI search first): `lci search <workspace> <question> --top-k N`.
//!   The call shows the snippets (chunk code), so their spans count as text
//!   read by that one call.
//! * A2 (LCI multimap walk): `lci traverse --scip ...`, built by
//!   `CliTraverser::command` (the call `maps bench` makes). The call prints
//!   one line per result; the agent then reads 40 lines around each returned
//!   site (a read call per file, see `SimConfig::read_files`).
//!
//! Both build an [`Opening`] for the loop. The tools are traits so that tests
//! run with stubs and no `lci` binary.
//!
//! What the agent is "shown" is a compact rendering of the tool's JSON: A1
//! `path:first-last` and the chunk code per result; A2 `path:line  text` per
//! result. Tokens are charged on that rendering (characters / 4), not on the
//! JSON envelope (timings, index lifecycle, scores), which an agent client
//! does not print either.

use super::sim_loop::{LISTING_LINE_CHARS, Opening, Window};
use crate::maps::bench::{ItemFlags, Traversed, Traverser, parse_start_maps};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The step kind of the A1 call.
pub const SEARCH_KIND: &str = "lci-search";
/// The step kind of the A2 call.
pub const TRAVERSE_KIND: &str = "lci-traverse";

/// `lci search`: the JSON the CLI printed.
pub trait SearchTool {
    fn search(&self, question: &str) -> Result<Value, String>;
}

/// The real `lci search`. The tree must be indexed beforehand (a missing or
/// stale index is refreshed by the call itself, which can take minutes) and
/// the embedder must be up.
pub struct CliSearch {
    pub lci: PathBuf,
    /// The workspace: the frozen tree.
    pub workspace: PathBuf,
    /// `--config` for the CLI, if the data directory is not the default one.
    pub config: Option<PathBuf>,
    pub top_k: usize,
    pub timeout: Duration,
}

impl CliSearch {
    fn command(&self, question: &str) -> Command {
        let mut command = Command::new(&self.lci);
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        command
            .arg("search")
            .arg(&self.workspace)
            .arg(question)
            .arg(format!("--top-k={}", self.top_k));
        command
    }
}

impl SearchTool for CliSearch {
    fn search(&self, question: &str) -> Result<Value, String> {
        let out = run_command(self.command(question), self.timeout)?;
        let value: Value =
            serde_json::from_slice(&out).map_err(|e| format!("lci search printed no JSON: {e}"))?;
        if !value["results"].is_array() {
            return Err("lci search printed no `results` list".to_owned());
        }
        Ok(value)
    }
}

/// Runs a command to its end (or the timeout) and returns its stdout.
fn run_command(mut command: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|e| format!("run the lci binary: {e}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = (stdout.join(), stderr.join());
                return Err(format!("timeout after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("wait for lci: {e}")),
        }
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&err);
        let tail: String = stderr
            .chars()
            .rev()
            .take(400)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(format!("lci exited with {status}: {}", tail.trim()));
    }
    Ok(out)
}

/// The SCIP index arm A2 needs, or a plain refusal.
pub fn require_scip(scip: Option<&Path>) -> Result<PathBuf> {
    match scip {
        None => bail!(
            "arm A2 (lci multimap walk) needs a SCIP index of the tree: pass --scip <index.scip>. \
             A tree without one can only run arms A0 and A1"
        ),
        Some(path) if !path.is_file() => bail!(
            "arm A2 needs a SCIP index of the tree, but the --scip path is not a file; \
             a tree without a SCIP index can only run arms A0 and A1"
        ),
        Some(path) => Ok(path.to_path_buf()),
    }
}

/// How an item starts: the A0 chain alone, or behind one `lci` call.
pub enum Opener<'a> {
    /// A0.
    Grep,
    /// A1.
    Search {
        tool: &'a dyn SearchTool,
        /// Recorded in `sim.json` and `meta.json`.
        settings: Value,
    },
    /// A2.
    Traverse {
        tool: &'a dyn Traverser,
        settings: Value,
        /// Where the CLI writes its traces (`<out>/traces`), to read which
        /// maps a call had enabled; `None` leaves `maps` out of the record.
        traces: Option<PathBuf>,
    },
}

/// The maps a call had switched on, from the start event of its trace.
fn enabled_maps(traces: Option<&Path>, raw: &Value) -> Option<Vec<String>> {
    let file = raw["trace"]["file"].as_str()?;
    let text = std::fs::read_to_string(traces?.join(file)).ok()?;
    Some(
        parse_start_maps(&text)?
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(id, _)| id)
            .collect(),
    )
}

/// What a traversal's run files record: status, stop reason, the maps that
/// were on, and the flags `maps compare` reads from `raw/<id>.json`.
fn traverse_info(traversed: &Traversed, raw: &Value, traces: Option<&Path>) -> Value {
    let flags = ItemFlags::from_raw(raw);
    json!({
        "status": traversed.status,
        "stop_reason": raw["stop_reason"],
        "maps": enabled_maps(traces, raw),
        "deadline": flags.deadline,
        "map_failures": flags.map_failures,
        "rerank_failures": flags.rerank_failures,
    })
}

impl Opener<'_> {
    /// Makes the one call for a question. `Ok(None)` for A0; an error means
    /// the tool failed, and the item is skipped and counted (never run as if
    /// the call had returned nothing).
    pub fn open(&self, id: &str, question: &str) -> Result<Option<Opening>, String> {
        match self {
            Opener::Grep => Ok(None),
            Opener::Search { tool, .. } => {
                Ok(Some(search_opening(question, &tool.search(question)?)))
            }
            Opener::Traverse { tool, traces, .. } => {
                let (traversed, raw) = tool
                    .traverse(id, question)
                    .map_err(|error| error.to_string())?;
                let mut opening = traverse_opening(question, &traversed);
                opening.info = Some(traverse_info(&traversed, &raw, traces.as_deref()));
                opening.raw = Some(raw);
                Ok(Some(opening))
            }
        }
    }

    /// The settings of the opening call, for the run files.
    pub fn describe(&self) -> Value {
        match self {
            Opener::Grep => json!({ "tool": "none" }),
            Opener::Search { settings, .. } | Opener::Traverse { settings, .. } => settings.clone(),
        }
    }
}

/// A1: the snippets of the results, in rank order: the code results, then the
/// documentation lane's (`docs.results`, present when that lane ran), each
/// lane in its own ranking. Their spans are the text the call showed.
pub fn search_opening(question: &str, value: &Value) -> Opening {
    let mut output = String::new();
    let mut windows = Vec::new();
    let code = value["results"].as_array().into_iter().flatten();
    // The full report keeps the lane under `docs.results`; the compact view
    // (a search profile) makes `docs` the list itself.
    let docs_list = match &value["docs"] {
        Value::Array(list) => Some(list),
        lane => lane["results"].as_array(),
    };
    let docs = docs_list.into_iter().flatten();
    for result in code.chain(docs) {
        let path = result["relative_file_path"]
            .as_str()
            .or_else(|| result["file_path"].as_str());
        let (Some(path), Some(first), Some(last)) = (
            path,
            result["start_line"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok()),
            result["end_line"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok()),
        ) else {
            continue;
        };
        let path = path.replace('\\', "/");
        let code = result["code"].as_str().unwrap_or_default();
        output.push_str(&format!("{path}:{first}-{last}\n{}\n", code.trim_end()));
        windows.push(Window {
            path,
            start: first,
            end: last.max(first),
        });
    }
    if windows.is_empty() {
        output.push_str("no results\n");
    }
    Opening {
        kind: SEARCH_KIND,
        arg: question.to_owned(),
        output,
        listed: Vec::new(),
        windows,
        reads: Vec::new(),
        // A docs-lane failure fails open in `lci`: the code results stand, so it
        // is a note on the item (`sim.json` `tool`, `meta.json`), not a skip.
        info: docs_warning(value).map(|warning| json!({ "docs_warning": warning })),
        ..Opening::default()
    }
}

/// The docs lane's own failure, from the full report (`docs.warning`) or the
/// compact view (`docs_warning`).
fn docs_warning(value: &Value) -> Option<&str> {
    value["docs"]["warning"]
        .as_str()
        .or_else(|| value["docs_warning"].as_str())
}

/// A2: one line per result; the sites (a result without one is only a line
/// of text) grouped by file, files in result order, lines in result order.
pub fn traverse_opening(question: &str, traversed: &Traversed) -> Opening {
    let mut output = String::new();
    let mut listed: Vec<(String, u32)> = Vec::new();
    let mut reads: Vec<(String, Vec<u32>)> = Vec::new();
    for result in &traversed.results {
        let text: String = result
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(LISTING_LINE_CHARS)
            .collect();
        let site = result
            .site
            .as_ref()
            .and_then(|s| Some((s.path.clone(), u32::try_from(s.line).ok()?)));
        match site {
            Some((path, line)) => {
                output.push_str(&format!("{path}:{line}  {text}\n"));
                match reads.iter_mut().find(|(p, _)| *p == path) {
                    Some((_, lines)) => lines.push(line),
                    None => reads.push((path.clone(), vec![line])),
                }
                listed.push((path, line));
            }
            None => output.push_str(&format!("{}  {text}\n", result.id)),
        }
    }
    if traversed.results.is_empty() {
        output.push_str("no results\n");
    }
    Opening {
        kind: TRAVERSE_KIND,
        arg: question.to_owned(),
        output,
        listed,
        windows: Vec::new(),
        reads,
        ..Opening::default()
    }
}

/// Stubs for the tests of the arms and of the loop: no `lci` binary.
#[cfg(test)]
pub(super) mod stubs {
    use super::*;
    use crate::maps::bench::{ResultNode, Site, TraverseError};
    use std::cell::Cell;

    pub struct StubSearch {
        pub reply: Result<Value, String>,
        pub calls: Cell<u32>,
    }

    impl SearchTool for StubSearch {
        fn search(&self, _question: &str) -> Result<Value, String> {
            self.calls.set(self.calls.get() + 1);
            self.reply.clone()
        }
    }

    pub struct StubTraverse {
        pub results: Vec<ResultNode>,
        pub fail: bool,
        pub calls: Cell<u32>,
        /// The raw reply the stub returns.
        pub raw: Value,
    }

    impl Traverser for StubTraverse {
        fn traverse(
            &self,
            _id: &str,
            _question: &str,
        ) -> Result<(Traversed, Value), TraverseError> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                return Err(TraverseError::Cli("stub failure".into()));
            }
            Ok((
                Traversed {
                    status: "ok".into(),
                    results: self.results.clone(),
                    ..Traversed::default()
                },
                self.raw.clone(),
            ))
        }

        fn default_maps(&self, _question: &str) -> Result<Vec<String>, TraverseError> {
            Ok(Vec::new())
        }

        fn preflight(&self, _map: &str, _question: &str) -> Result<(), TraverseError> {
            Ok(())
        }
    }

    pub fn node(id: &str, text: &str, site: Option<(&str, u64)>) -> ResultNode {
        ResultNode {
            id: id.to_owned(),
            text: text.to_owned(),
            site: site.map(|(path, line)| Site {
                path: path.to_owned(),
                line,
            }),
        }
    }

    pub fn search_reply(chunks: &[(&str, u32, u32, &str)]) -> Value {
        let results: Vec<Value> = chunks
            .iter()
            .map(|(path, first, last, code)| {
                json!({
                    "file_path": format!("/abs/{path}"), "relative_file_path": path,
                    "start_line": first, "end_line": last, "code": code,
                    "fusion_score": 0.5,
                })
            })
            .collect();
        json!({ "query": "q", "results": results })
    }
}

#[cfg(test)]
mod tests {
    use super::stubs::*;
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_docs_lane_warning_is_a_note_on_the_opening_not_a_failure() {
        let mut reply = search_reply(&[("src/a.rs", 1, 4, "fn a() {}")]);
        assert_eq!(search_opening("q", &reply).info, None);
        // The full report: `docs.warning`, the lane's own failure.
        reply["docs"] = json!({ "results": [], "warning": "Docs reranker unavailable" });
        let full = search_opening("q", &reply);
        assert_eq!(
            full.info,
            Some(json!({ "docs_warning": "Docs reranker unavailable" }))
        );
        assert_eq!(full.windows.len(), 1);
        // The compact view: `docs` is the list, `docs_warning` the failure.
        let compact = json!({
            "results": reply["results"],
            "docs": [{ "relative_file_path": "docs/g.md", "start_line": 1, "end_line": 2, "code": "# g" }],
            "docs_warning": "lane down",
        });
        let opening = search_opening("q", &compact);
        assert_eq!(opening.info, Some(json!({ "docs_warning": "lane down" })));
        assert_eq!(opening.windows.len(), 2);
        let search = StubSearch {
            reply: Ok(reply),
            calls: Cell::new(0),
        };
        let opener = Opener::Search {
            tool: &search,
            settings: json!({}),
        };
        assert!(opener.open("t", "q").unwrap().is_some());
    }

    #[test]
    fn a_search_reply_becomes_windows_over_the_snippet_spans() {
        let reply = search_reply(&[
            ("src\\a.rs", 3, 9, "fn a() {}\n"),
            ("src/b.rs", 10, 12, "fn b() {}"),
        ]);
        let opening = search_opening("where is a", &reply);
        assert_eq!(opening.kind, "lci-search");
        let spans: Vec<(&str, u32, u32)> = opening
            .windows
            .iter()
            .map(|w| (w.path.as_str(), w.start, w.end))
            .collect();
        assert_eq!(spans, [("src/a.rs", 3, 9), ("src/b.rs", 10, 12)]);
        assert_eq!(
            opening.output,
            "src/a.rs:3-9\nfn a() {}\nsrc/b.rs:10-12\nfn b() {}\n"
        );
        assert!(opening.reads.is_empty());
        let empty = search_opening("q", &json!({ "results": [] }));
        assert_eq!(empty.output, "no results\n");
        assert!(empty.windows.is_empty());
    }

    #[test]
    fn the_docs_lane_snippets_follow_the_code_results_each_in_rank_order() {
        let mut reply = search_reply(&[
            ("src/a.rs", 1, 4, "fn a() {}"),
            ("src/b.rs", 5, 8, "fn b()"),
        ]);
        reply["docs"] = json!({
            "results": search_reply(&[("docs/z.md", 1, 3, "# z"), ("docs/y.md", 2, 5, "# y")])["results"],
            "candidate_count": 2,
        });
        let opening = search_opening("q", &reply);
        let paths: Vec<&str> = opening.windows.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, ["src/a.rs", "src/b.rs", "docs/z.md", "docs/y.md"]);
        assert!(opening.output.contains("docs/z.md:1-3\n# z\n"));
        // Docs alone are an opening, not "no results".
        let docs_only = json!({ "results": [], "docs": reply["docs"] });
        let only = search_opening("q", &docs_only);
        assert_eq!(only.windows.len(), 2);
        assert!(!only.output.contains("no results"));
    }

    #[test]
    fn a_traversal_reply_becomes_sites_grouped_by_file_in_result_order() {
        let traversed = Traversed {
            results: vec![
                node("sym:one", "fn one", Some(("src/b.rs", 30))),
                node("commit:abc", "a commit", None),
                node("sym:two", "fn two", Some(("src/a.rs", 5))),
                node("sym:three", "fn three", Some(("src/b.rs", 7))),
            ],
            ..Traversed::default()
        };
        let opening = traverse_opening("q", &traversed);
        assert_eq!(opening.kind, "lci-traverse");
        assert_eq!(
            opening.reads,
            [
                ("src/b.rs".to_owned(), vec![30, 7]),
                ("src/a.rs".to_owned(), vec![5])
            ]
        );
        assert_eq!(opening.listed.len(), 3);
        assert_eq!(
            opening.output,
            "src/b.rs:30  fn one\ncommit:abc  a commit\nsrc/a.rs:5  fn two\nsrc/b.rs:7  fn three\n"
        );
        assert!(opening.windows.is_empty());
    }

    #[test]
    fn a_failed_tool_is_an_error_not_an_empty_opening() {
        let search = StubSearch {
            reply: Err("embedder down".into()),
            calls: Cell::new(0),
        };
        let opener = Opener::Search {
            tool: &search,
            settings: json!({}),
        };
        assert_eq!(opener.open("t", "q").unwrap_err(), "embedder down");
        let traverse = StubTraverse {
            results: Vec::new(),
            fail: true,
            calls: Cell::new(0),
            raw: json!({}),
        };
        let opener = Opener::Traverse {
            tool: &traverse,
            settings: json!({}),
            traces: None,
        };
        assert!(opener.open("t", "q").unwrap_err().contains("stub failure"));
        assert_eq!(Opener::Grep.open("t", "q"), Ok(None));
    }

    #[test]
    fn a2_refuses_a_tree_without_a_scip_index() {
        let none = require_scip(None).unwrap_err().to_string();
        assert!(none.contains("SCIP") && none.contains("--scip") && none.contains("A0 and A1"));
        let missing = require_scip(Some(Path::new("no-such-index.scip")))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("not a file"));
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert_eq!(require_scip(Some(&here)).unwrap(), here);
    }

    #[test]
    fn the_search_command_is_the_cli_call() {
        let tool = CliSearch {
            lci: PathBuf::from("lci"),
            workspace: PathBuf::from("tree"),
            config: Some(PathBuf::from("c.toml")),
            top_k: 10,
            timeout: Duration::from_secs(1),
        };
        let command = tool.command("how does X work");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--config",
                "c.toml",
                "search",
                "tree",
                "how does X work",
                "--top-k=10"
            ]
        );
    }

    #[test]
    fn a_result_text_is_one_line_cut_at_the_listing_width() {
        let long = format!("fn\n   wide   {}", "x".repeat(400));
        let traversed = Traversed {
            results: vec![node("sym:w", &long, Some(("src/a.rs", 1)))],
            ..Traversed::default()
        };
        let opening = traverse_opening("q", &traversed);
        let line = opening.output.lines().next().unwrap();
        let text = line.strip_prefix("src/a.rs:1  ").unwrap();
        assert_eq!(text.chars().count(), LISTING_LINE_CHARS);
        assert!(text.starts_with("fn wide xxx"));
        assert_eq!(opening.output.lines().count(), 1);
    }

    #[test]
    fn the_enabled_maps_come_from_the_start_event_of_the_trace() {
        let dir = std::env::temp_dir().join(format!("xtask-sim-traces-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("t1.jsonl"),
            "{\"ev\":\"start\",\"maps\":[{\"id\":\"calls\",\"enabled\":true},{\"id\":\"docs\",\"enabled\":false},{\"id\":\"impls\",\"enabled\":true}]}\n{\"ev\":\"end\"}\n",
        )
        .unwrap();
        let raw = json!({ "trace": { "file": "t1.jsonl" }, "stop_reason": "deadline" });
        assert_eq!(
            enabled_maps(Some(&dir), &raw),
            Some(vec!["calls".to_owned(), "impls".to_owned()])
        );
        // No trace dir, no trace file or an unreadable one: unknown, not empty.
        assert_eq!(enabled_maps(None, &raw), None);
        assert_eq!(enabled_maps(Some(&dir), &json!({})), None);
        assert_eq!(
            enabled_maps(Some(&dir), &json!({ "trace": { "file": "gone.jsonl" } })),
            None
        );
        let info = traverse_info(&Traversed::default(), &raw, Some(&dir));
        assert_eq!(info["maps"], json!(["calls", "impls"]));
        assert_eq!(info["deadline"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
