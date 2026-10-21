//! The compact `search_code` result an MCP profile endpoint can serve
//! (`[search_profiles.<name>] response = "compact"`).
//!
//! Presentation only: [`compact_view`] is applied to a finished
//! [`SearchReport`], after retrieval, so ranking, candidates, and the report's
//! serde shape are untouched. A hit keeps its locator (`relative_file_path`,
//! `start_line`, `end_line`), `language`, `source_role`, and `code`; the
//! retrieval metadata (scores, ranks, channels, hashes, absolute path) is
//! dropped. The envelope keeps `workspace.path`, `search_profile`, the
//! warning, notes, filters, and the index lifecycle. When the report carries a
//! documentation lane, its hits follow `results` under `docs` (the same compact
//! shape, sharing the response ceiling) and its failure, if any, is
//! `docs_warning`; both are absent otherwise. The last key is `profile_hash`,
//! the run-profile hash of the server's configuration.
//!
//! Two limits shorten `code` and never touch a locator or the order of hits:
//! at most `max_hit_lines` lines per hit, and a ceiling on the serialized
//! size of the whole result. A shortened hit says so with `code_truncated`,
//! `excerpt_end_line` (the last line shown, absent when none is), and a closing
//! line inside `code` that names the range to read from the file.
//!
//! The result is built from typed structs and returned as JSON text, because
//! the order of their fields (locators first) is the order of the keys in the
//! text, whether or not `serde_json` keeps the insertion order of a `Map`.
//! (Parsing the text into a `serde_json::Value` is free to reorder the keys.)

use super::{SearchIndexLifecycle, SearchReport};
use crate::{
    config::CompactLimits,
    filter::{EffectiveFilterReport, SourceRole},
    store::Hit,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::Path;

/// A line longer than this many characters is cut and marked, so one
/// minified line cannot defeat the line cap.
pub const COMPACT_MAX_LINE_CHARS: usize = 500;

/// Characters kept back for the closing line of a hit that the response
/// ceiling shortens (the line itself is about 100 to 140 characters).
const CLOSING_LINE_RESERVE: usize = 150;

/// The compact result: the envelope of the report, then the hits. The field
/// order is the key order of the JSON text.
#[derive(Serialize)]
struct CompactReport<'a> {
    workspace: CompactWorkspace<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<&'a str>,
    /// The docs lane's own warning, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    docs_warning: Option<&'a str>,
    #[serde(skip_serializing_if = "no_notes")]
    notes: &'a [String],
    filters: &'a EffectiveFilterReport,
    index: &'a SearchIndexLifecycle,
    /// Set when the response ceiling shortened the code of the hits.
    #[serde(skip_serializing_if = "is_false")]
    response_truncated: bool,
    results: Vec<CompactHit<'a>>,
    /// The documentation lane, after the code results; present exactly when
    /// the search report carries one. Its hits share the response ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    docs: Option<Vec<CompactHit<'a>>>,
    /// The run-profile hash of the server's configuration; always the last
    /// key, so the text ends with it.
    profile_hash: &'a str,
}

#[derive(Serialize)]
struct CompactWorkspace<'a> {
    path: &'a Path,
}

/// One compact hit: the locator first, then what the reader needs to judge it.
#[derive(Serialize)]
struct CompactHit<'a> {
    relative_file_path: &'a str,
    start_line: u32,
    end_line: u32,
    language: &'a str,
    source_role: SourceRole,
    code: String,
    #[serde(skip_serializing_if = "is_false")]
    code_truncated: bool,
    /// The last line shown; absent when the code was shortened to nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt_end_line: Option<u32>,
}

fn no_notes(notes: &&[String]) -> bool {
    notes.is_empty()
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

/// `report` as the compact result, as JSON text whose keys are in the order of
/// the module documentation, ending with `profile_hash`. Fails only when the
/// ceiling cannot hold the mandatory metadata of the results (every locator,
/// the envelope, the hash, and a closing line per hit).
pub fn compact_view(
    report: &SearchReport,
    limits: CompactLimits,
    profile_hash: &str,
) -> Result<String> {
    let ceiling = limits.max_response_chars;
    // Code results first, then the docs lane's: one list for the ceiling.
    let docs_hits: &[Hit] = report
        .docs
        .as_ref()
        .map_or(&[], |docs| docs.results.as_slice());
    let all_hits = || report.results.iter().chain(docs_hits);
    let build = |per_hit_budget: Option<usize>| -> Result<String> {
        let view = CompactReport {
            workspace: CompactWorkspace {
                path: &report.workspace.path,
            },
            search_profile: report.search_profile.as_deref(),
            warning: report.warning.as_deref(),
            docs_warning: report
                .docs
                .as_ref()
                .and_then(|docs| docs.warning.as_deref()),
            notes: &report.notes,
            filters: &report.filters,
            index: &report.index,
            response_truncated: per_hit_budget.is_some(),
            results: report
                .results
                .iter()
                .map(|hit| compact_hit(hit, limits.max_hit_lines, per_hit_budget))
                .collect(),
            docs: report.docs.as_ref().map(|docs| {
                docs.results
                    .iter()
                    .map(|hit| compact_hit(hit, limits.max_hit_lines, per_hit_budget))
                    .collect()
            }),
            profile_hash,
        };
        Ok(serde_json::to_string(&view)?)
    };

    let whole = build(None)?;
    if serialized_chars(&whole) <= ceiling {
        return Ok(whole);
    }
    // Over the ceiling: shorten code across hits, never dropping or
    // reordering one. The floor keeps no code at all, only each hit's locator
    // and closing line; if even that is over, no amount of shortening helps.
    let floor = build(Some(0))?;
    let floor_chars = serialized_chars(&floor);
    if floor_chars > ceiling {
        let counted = if docs_hits.is_empty() {
            format!("{} results", report.results.len())
        } else {
            format!(
                "{} results and {} docs",
                report.results.len(),
                docs_hits.len()
            )
        };
        bail!(
            "the compact search result cannot fit compact_max_response_chars ({ceiling}): its locators and metadata alone take {floor_chars} characters for {counted}; ask for a lower top_k or narrow the search with languages, include_paths, exclude_paths or source_roles",
        );
    }
    // The largest per-hit budget (serialized characters of one hit's code)
    // that still fits: `low` always fits, `high` never does.
    let mut low = 0;
    let mut high = all_hits()
        .map(|hit| {
            let (code, ..) = excerpt(hit, limits.max_hit_lines, None);
            json_chars(&code) + CLOSING_LINE_RESERVE
        })
        .max()
        .unwrap_or(0)
        .max(1);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if serialized_chars(&build(Some(middle))?) <= ceiling {
            low = middle;
        } else {
            high = middle;
        }
    }
    build(Some(low))
}

/// One compact hit. `per_hit_budget` is the ceiling pass's cap on the
/// serialized characters of this hit's `code`, closing line included.
fn compact_hit(hit: &Hit, max_lines: usize, per_hit_budget: Option<usize>) -> CompactHit<'_> {
    let chunk = &hit.chunk;
    let (code, code_truncated, last_shown) = excerpt(hit, max_lines, per_hit_budget);
    CompactHit {
        relative_file_path: &chunk.relative_file_path,
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        language: &chunk.language,
        source_role: hit.source_role,
        code,
        code_truncated,
        excerpt_end_line: if code_truncated { last_shown } else { None },
    }
}

/// The code of `hit` as shown: at most `max_lines` lines, each at most
/// [`COMPACT_MAX_LINE_CHARS`] characters, and, with a `budget`, at most that
/// many serialized characters. Returns the text (with a closing line when
/// anything was shortened), whether anything was, and the number of the last
/// line shown (`None` when no line is). Short code comes back byte-identical.
fn excerpt(hit: &Hit, max_lines: usize, budget: Option<usize>) -> (String, bool, Option<u32>) {
    let chunk = &hit.chunk;
    let lines: Vec<&str> = chunk.code.split_inclusive('\n').collect();
    let line_room = budget.map(|budget| budget.saturating_sub(CLOSING_LINE_RESERVE));
    let mut shown = String::new();
    let mut used = 0usize;
    let mut kept = 0usize;
    let mut cut_lines = false;
    for line in lines.iter().take(max_lines) {
        let (body, ending) = match line.strip_suffix('\n') {
            Some(body) => match body.strip_suffix('\r') {
                Some(body) => (body, "\r\n"),
                None => (body, "\n"),
            },
            None => (*line, ""),
        };
        let (mut text, was_cut) = cut_long_line(body);
        text.push_str(ending);
        if let Some(room) = line_room {
            if used + json_chars(&text) > room {
                // The ceiling stops here. If not one line has been kept yet,
                // show the start of this one so the hit is not just a locator.
                if kept == 0 {
                    let remaining = room.saturating_sub(used);
                    if let Some(prefix) = fit_prefix(body, remaining) {
                        text = prefix;
                        text.push_str(if ending.is_empty() { "\n" } else { ending });
                        shown.push_str(&text);
                        kept += 1;
                        cut_lines = true;
                    }
                }
                break;
            }
            used += json_chars(&text);
        }
        shown.push_str(&text);
        kept += 1;
        cut_lines |= was_cut;
    }
    let dropped = kept < lines.len();
    if !dropped && !cut_lines {
        return (shown, false, kept_line(chunk.start_line, kept));
    }
    if !shown.is_empty() && !shown.ends_with('\n') {
        shown.push('\n');
    }
    let first_unseen = chunk.start_line.saturating_add(kept as u32);
    let last_line = chunk.end_line.max(first_unseen);
    let mut closing = if dropped {
        format!(
            "[lci: code truncated; lines {first_unseen}-{last_line} of this file are not shown, read them from the file"
        )
    } else {
        format!(
            "[lci: long lines were cut; read lines {}-{} of this file for the full text",
            chunk.start_line, chunk.end_line
        )
    };
    if dropped && cut_lines {
        closing.push_str("; long lines in the excerpt are cut too");
    }
    closing.push(']');
    shown.push_str(&closing);
    (shown, true, kept_line(chunk.start_line, kept))
}

/// The number of the last of `kept` lines that start at `start_line`.
fn kept_line(start_line: u32, kept: usize) -> Option<u32> {
    (kept > 0).then(|| start_line.saturating_add(kept as u32 - 1))
}

/// `body` cut to [`COMPACT_MAX_LINE_CHARS`] characters, with a marker saying
/// how much was cut, and whether it was cut at all. Cuts on a character
/// boundary.
fn cut_long_line(body: &str) -> (String, bool) {
    match body.char_indices().nth(COMPACT_MAX_LINE_CHARS) {
        None => (body.to_owned(), false),
        Some((end, _)) => {
            let more = body[end..].chars().count();
            (format!("{} ... [+{more} characters]", &body[..end]), true)
        }
    }
}

/// The longest prefix of `body` (on a character boundary, with a cut marker)
/// whose serialized size is at most `room`, if any non-empty one is.
fn fit_prefix(body: &str, room: usize) -> Option<String> {
    const MARKER: &str = " ... [line cut]";
    let mut characters = body.chars().count().min(COMPACT_MAX_LINE_CHARS);
    while characters > 0 {
        let end = body
            .char_indices()
            .nth(characters)
            .map_or(body.len(), |(end, _)| end);
        let text = format!("{}{MARKER}", &body[..end]);
        // Leave room for the line ending the caller adds.
        if json_chars(&text) + 2 <= room {
            return Some(text);
        }
        characters = characters * 3 / 4;
    }
    None
}

/// Characters of `text` once serialized as a JSON string, quotes included.
fn json_chars(text: &str) -> usize {
    serde_json::to_string(text).map_or(text.chars().count() + 2, |json| json.chars().count())
}

/// Characters of the compact JSON text of the result.
fn serialized_chars(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{IndexAction, Timings},
        chunk::Chunk,
        filter::{EffectiveFilter, FilterRequest, SourceRole},
        workspace::Workspace,
    };
    use serde_json::Value;

    /// A run-profile hash: 64 lowercase hex characters.
    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn hit(path: &str, start_line: u32, code: &str) -> Hit {
        let lines = code.split_inclusive('\n').count().max(1) as u32;
        Hit {
            file_path: format!("/absolute/{path}"),
            chunk: Chunk {
                relative_file_path: path.to_owned(),
                language: "rust".to_owned(),
                start_line,
                end_line: start_line + lines - 1,
                code: code.to_owned(),
                content_hash: "hash".to_owned(),
            },
            semantic_score: Some(0.5),
            semantic_rank: Some(1),
            lexical_rank: Some(2),
            lexical_match_count: 3,
            lexical_score: Some(1.5),
            lsp_rank: None,
            fusion_score: 0.03,
            retrieval_channels: vec!["semantic".to_owned(), "lexical".to_owned()],
            reranker_score: Some(0.9),
            source_role: SourceRole::Source,
            source_role_prior: 0.004,
        }
    }

    fn report(results: Vec<Hit>) -> SearchReport {
        SearchReport {
            workspace: Workspace {
                id: "ws_0123456789abcdef".to_owned(),
                path: "/workspace".into(),
            },
            query: "a query".to_owned(),
            search_profile: Some("lean".to_owned()),
            candidate_count: 24,
            reranked: true,
            warning: None,
            notes: Vec::new(),
            lsp_over_budget: Vec::new(),
            timings: Timings::default(),
            results,
            docs: None,
            index: SearchIndexLifecycle {
                action: IndexAction::Reused,
                wait_ms: 0.0,
            },
            filters: EffectiveFilter::from_request(&FilterRequest::default())
                .unwrap()
                .report(),
            candidates: None,
            rerank_outcome: None,
            freshness: None,
            channel_outcomes: Default::default(),
        }
    }

    fn numbered_lines(count: usize) -> String {
        (1..=count)
            .map(|n| format!("let value_{n} = {n};\n"))
            .collect()
    }

    fn limits(max_hit_lines: usize, max_response_chars: usize) -> CompactLimits {
        CompactLimits {
            max_hit_lines,
            max_response_chars,
        }
    }

    fn results(view: &Value) -> &Vec<Value> {
        view["results"].as_array().unwrap()
    }

    fn locators(view: &Value) -> Vec<(String, u64, u64)> {
        results(view)
            .iter()
            .map(|hit| {
                (
                    hit["relative_file_path"].as_str().unwrap().to_owned(),
                    hit["start_line"].as_u64().unwrap(),
                    hit["end_line"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    /// The compact result parsed into a `Value`, whose keys `serde_json` may
    /// reorder: use it for values and for the SET of keys, never for their
    /// order (that is what [`assert_key_order`] is for).
    fn view_of(report: &SearchReport, limits: CompactLimits) -> Result<Value> {
        Ok(serde_json::from_str(&compact_view(report, limits, HASH)?)?)
    }

    fn chars_of(view: &Value) -> usize {
        view.to_string().chars().count()
    }

    /// The keys of `object`, sorted.
    fn keys(object: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = object
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    fn sorted(mut keys: Vec<&str>) -> Vec<&str> {
        keys.sort_unstable();
        keys
    }

    /// Asserts that each of `keys` occurs as a JSON key of `text`, in this
    /// order. The text is what the server sends, so this is the order a reader
    /// of the response sees.
    fn assert_key_order(text: &str, keys: &[&str]) {
        let mut from = 0;
        for key in keys {
            let needle = format!("\"{key}\":");
            match text[from..].find(&needle) {
                Some(at) => from += at + needle.len(),
                None => panic!("key {key} not found in order after byte {from} of {text}"),
            }
        }
    }

    #[test]
    fn a_compact_result_keeps_the_locators_and_code_and_drops_the_retrieval_metadata() {
        let code = "fn short() {\n    1\n}\n";
        let one = report(vec![hit("src/a.rs", 10, code)]);
        let text = compact_view(&one, limits(40, 16_000), HASH).unwrap();
        let view = view_of(&one, limits(40, 16_000)).unwrap();
        assert_eq!(
            keys(&view),
            sorted(vec![
                "workspace",
                "search_profile",
                "filters",
                "index",
                "results",
                "profile_hash"
            ])
        );
        // The order of the keys in the text does not depend on how
        // `serde_json` orders a parsed map: the struct fields set it.
        assert_key_order(
            &text,
            &[
                "workspace",
                "path",
                "search_profile",
                "filters",
                "index",
                "results",
                "relative_file_path",
                "start_line",
                "end_line",
                "language",
                "source_role",
                "code",
                "profile_hash",
            ],
        );
        assert!(
            text.starts_with(
                "{\"workspace\":{\"path\":\"/workspace\"},\"search_profile\":\"lean\","
            ),
            "{text}"
        );
        assert!(
            text.contains("\"results\":[{\"relative_file_path\":\"src/a.rs\",\"start_line\":10,\"end_line\":12,\"language\":\"rust\",\"source_role\":\"source\",\"code\":"),
            "{text}"
        );
        assert_eq!(view["workspace"], serde_json::json!({"path": "/workspace"}));
        assert_eq!(view["search_profile"], "lean");
        // The locator comes first, so a reader sees where before what.
        assert_eq!(
            keys(&results(&view)[0]),
            sorted(vec![
                "relative_file_path",
                "start_line",
                "end_line",
                "language",
                "source_role",
                "code"
            ])
        );
        let first = &results(&view)[0];
        assert_eq!(first["relative_file_path"], "src/a.rs");
        assert_eq!(first["start_line"], 10);
        assert_eq!(first["end_line"], 12);
        assert_eq!(first["source_role"], "source");
        // Code of a chunk within the limits is byte-identical.
        assert_eq!(first["code"], code);
    }

    #[test]
    fn a_warning_and_notes_stay_in_the_envelope_and_are_absent_when_there_are_none() {
        let mut with = report(vec![hit("src/a.rs", 1, "x\n")]);
        with.warning = Some("the reranker failed".to_owned());
        with.notes = vec!["a language server is still starting".to_owned()];
        let view = view_of(&with, limits(40, 16_000)).unwrap();
        assert_eq!(view["warning"], "the reranker failed");
        assert_eq!(view["notes"][0], "a language server is still starting");
        let mut none = report(vec![]);
        none.search_profile = None;
        let view = view_of(&none, limits(40, 16_000)).unwrap();
        assert_eq!(
            keys(&view),
            sorted(vec![
                "workspace",
                "filters",
                "index",
                "results",
                "profile_hash"
            ])
        );
        assert!(results(&view).is_empty());
    }

    #[test]
    fn the_compact_text_ends_with_the_profile_hash() {
        let text = compact_view(
            &report(vec![hit("src/a.rs", 1, "x\n")]),
            limits(40, 16_000),
            HASH,
        )
        .unwrap();
        assert!(
            text.ends_with(&format!(",\"profile_hash\":\"{HASH}\"}}")),
            "{text}"
        );
        // With a docs lane and a squeezed ceiling it is still the last key.
        let both = with_docs(
            report(vec![hit("src/a.rs", 1, &numbered_lines(60))]),
            vec![hit("docs/a.md", 1, &numbered_lines(60))],
            None,
        );
        let whole = chars_of(&view_of(&both, limits(40, 100_000)).unwrap());
        let text = compact_view(&both, limits(40, whole * 3 / 4), HASH).unwrap();
        assert!(text.contains("\"response_truncated\":true"), "{text}");
        assert!(
            text.ends_with(&format!(",\"profile_hash\":\"{HASH}\"}}")),
            "{text}"
        );
        assert_eq!(text.matches("\"profile_hash\"").count(), 1);
        assert_eq!(HASH.len(), 64);
    }

    #[test]
    fn the_line_cap_cuts_with_a_marker_and_leaves_exact_fits_alone() {
        // a hit over the line cap is cut with a marker naming the rest
        {
            let code = numbered_lines(100);
            let view = view_of(
                &report(vec![hit("src/big.rs", 10, &code)]),
                limits(40, 100_000),
            )
            .unwrap();
            let first = &results(&view)[0];
            // The locator still names the whole chunk (lines 10 to 109); only
            // `code` is shortened.
            assert_eq!(first["start_line"], 10);
            assert_eq!(first["end_line"], 109);
            assert_eq!(first["code_truncated"], true);
            assert_eq!(first["excerpt_end_line"], 49);
            let shown = first["code"].as_str().unwrap();
            let kept: String = numbered_lines(40);
            assert!(shown.starts_with(&kept), "{shown}");
            let closing = &shown[kept.len()..];
            assert!(
                closing.starts_with("[lci:") && closing.ends_with(']'),
                "{closing}"
            );
            assert!(closing.contains("lines 50-109"), "{closing}");
            assert_eq!(closing.lines().count(), 1, "one closing line");
            assert!(view.get("response_truncated").is_none());
        }

        // a chunk of exactly the limit is left alone and one line more is cut
        {
            let exact = numbered_lines(40);
            let view = view_of(&report(vec![hit("a.rs", 1, &exact)]), limits(40, 100_000)).unwrap();
            assert_eq!(results(&view)[0]["code"], exact.as_str());
            assert!(results(&view)[0].get("code_truncated").is_none());
            assert!(results(&view)[0].get("excerpt_end_line").is_none());
            let one_more = numbered_lines(41);
            let view = view_of(
                &report(vec![hit("a.rs", 1, &one_more)]),
                limits(40, 100_000),
            )
            .unwrap();
            assert_eq!(results(&view)[0]["code_truncated"], true);
            assert_eq!(results(&view)[0]["excerpt_end_line"], 40);
            assert!(
                results(&view)[0]["code"]
                    .as_str()
                    .unwrap()
                    .contains("lines 41-41")
            );
            // A chunk without a final newline keeps that too.
            let unterminated = "fn f() {\n}";
            let view = view_of(
                &report(vec![hit("a.rs", 1, unterminated)]),
                limits(40, 100_000),
            )
            .unwrap();
            assert_eq!(results(&view)[0]["code"], unterminated);
        }

        // the cap counts lines, not bytes, and keeps CRLF endings and multibyte text
        {
            let line = |n: usize| format!("// zeile {n} \u{e4}\u{f6}\u{fc}\r\n");
            let crlf: String = (1..=50).map(line).collect();
            let view = view_of(&report(vec![hit("a.rs", 1, &crlf)]), limits(5, 100_000)).unwrap();
            let shown = results(&view)[0]["code"].as_str().unwrap();
            let prefix: String = (1..=5).map(line).collect();
            assert!(shown.starts_with(&prefix), "{shown:?}");
            assert!(shown.contains("lines 6-50"), "{shown:?}");
            assert_eq!(results(&view)[0]["excerpt_end_line"], 5);
        }

        // a huge single line is cut on a character boundary and marked
        {
            // 3000 two-byte characters on one line: a minified file's chunk.
            let line = "\u{e9}".repeat(3000);
            let code = format!("{line}\n");
            let view =
                view_of(&report(vec![hit("min.js", 7, &code)]), limits(40, 100_000)).unwrap();
            let first = &results(&view)[0];
            let shown = first["code"].as_str().unwrap();
            assert!(
                shown.starts_with(&"\u{e9}".repeat(COMPACT_MAX_LINE_CHARS)),
                "{shown}"
            );
            assert!(shown.chars().count() < 800, "{}", shown.chars().count());
            assert!(shown.contains("[+2500 characters]"), "{shown}");
            assert!(shown.contains("long lines were cut"), "{shown}");
            assert!(shown.contains("lines 7-7"), "{shown}");
            assert_eq!(first["code_truncated"], true);
            assert_eq!(first["excerpt_end_line"], 7);
            // A line of exactly the limit is left alone.
            let exact = format!("{}\n", "x".repeat(COMPACT_MAX_LINE_CHARS));
            let view = view_of(&report(vec![hit("a.rs", 1, &exact)]), limits(40, 100_000)).unwrap();
            assert_eq!(results(&view)[0]["code"], exact.as_str());
        }
    }

    fn forty_hits() -> SearchReport {
        let code = numbered_lines(60);
        report(
            (0..40)
                .map(|n| hit(&format!("src/file_{n:02}.rs"), 100 + n, &code))
                .collect(),
        )
    }

    #[test]
    fn the_response_ceiling_holds_and_every_locator_survives_in_order() {
        let full = forty_hits();
        let uncapped = view_of(&full, limits(40, 1_000_000)).unwrap();
        let uncapped_chars = chars_of(&uncapped);
        assert!(uncapped_chars > 30_000, "{uncapped_chars}");
        assert!(uncapped.get("response_truncated").is_none());
        let expected = locators(&uncapped);
        for ceiling in [1_000_000, 30_000, 16_000, 13_000] {
            let view = view_of(&full, limits(40, ceiling)).unwrap();
            assert!(chars_of(&view) <= ceiling, "{ceiling}: {}", chars_of(&view));
            assert_eq!(locators(&view), expected, "{ceiling}");
            let shortened = view.get("response_truncated").is_some();
            assert_eq!(shortened, ceiling < uncapped_chars, "{ceiling}");
            if shortened {
                assert_eq!(view["response_truncated"], true);
                // The budget is shared, not spent on the first hits.
                let shown: Vec<usize> = results(&view)
                    .iter()
                    .map(|hit| hit["code"].as_str().unwrap().len())
                    .collect();
                let (least, most) = (shown.iter().min().unwrap(), shown.iter().max().unwrap());
                assert!(
                    *least > 0 && most - least < 400,
                    "{ceiling}: {least} {most}"
                );
                for hit in results(&view) {
                    assert_eq!(hit["code_truncated"], true, "{ceiling}");
                    let code = hit["code"].as_str().unwrap();
                    assert!(code.contains("[lci: code truncated"), "{ceiling}: {code}");
                }
            }
        }
    }

    #[test]
    fn a_ceiling_squeezes_the_code_down_to_the_markers_and_then_reports_an_error() {
        let full = forty_hits();
        let view = view_of(&full, limits(40, 1_000_000)).unwrap();
        // Locators, envelope and one closing line per hit fit, code does not.
        let locators_only = {
            let mut bare = view.clone();
            for hit in bare["results"].as_array_mut().unwrap() {
                hit.as_object_mut().unwrap().remove("code");
            }
            chars_of(&bare)
        };
        let ceiling = locators_only + 40 * 160;
        let squeezed = view_of(&full, limits(40, ceiling)).unwrap();
        assert!(chars_of(&squeezed) <= ceiling);
        assert_eq!(locators(&squeezed), locators(&view));
        assert_eq!(squeezed["response_truncated"], true);

        // Below what the locators and metadata need there is no response to
        // give, only an actionable and bounded error.
        let error = view_of(&full, limits(40, 1_000)).unwrap_err().to_string();
        assert!(error.contains("compact_max_response_chars"), "{error}");
        assert!(error.contains("top_k"), "{error}");
        assert!(error.contains("40 results"), "{error}");
        assert!(error.len() < 600, "{}", error.len());
    }

    #[test]
    fn a_ceiling_shows_the_start_of_the_first_line_when_not_even_one_line_fits() {
        // One hit whose first line is long; a ceiling that leaves room for
        // part of it shows that part instead of nothing.
        let code = format!("{}\nsecond\n", "word ".repeat(100));
        let one = report(vec![hit("a.rs", 1, &code)]);
        let bare = {
            let mut view = view_of(&one, limits(40, 100_000)).unwrap();
            view["results"][0].as_object_mut().unwrap().remove("code");
            chars_of(&view)
        };
        let ceiling = bare + CLOSING_LINE_RESERVE + 180;
        let view = view_of(&one, limits(40, ceiling)).unwrap();
        assert!(chars_of(&view) <= ceiling);
        let first = &results(&view)[0];
        let shown = first["code"].as_str().unwrap();
        assert!(shown.starts_with("word word"), "{shown}");
        assert_eq!(first["excerpt_end_line"], 1);
        assert_eq!(first["code_truncated"], true);
    }

    fn with_docs(mut base: SearchReport, docs: Vec<Hit>, warning: Option<&str>) -> SearchReport {
        base.docs = Some(super::super::search::DocsLane {
            candidate_count: docs.len(),
            results: docs,
            reranked: true,
            warning: warning.map(str::to_owned),
            timings: Default::default(),
        });
        base
    }

    #[test]
    fn the_docs_list_follows_the_results_in_the_compact_shape_and_keeps_its_own_warning() {
        let mut guide = hit("docs/guide.md", 1, "# Guide\ntext\n");
        guide.chunk.language = "markdown".to_owned();
        guide.source_role = SourceRole::Documentation;
        let both = with_docs(
            report(vec![hit("src/a.rs", 10, "fn a() {}\n")]),
            vec![guide],
            Some("Docs reranker unavailable"),
        );
        let text = compact_view(&both, limits(40, 16_000), HASH).unwrap();
        // The docs list comes after the results, its warning before them.
        assert_key_order(
            &text,
            &[
                "workspace",
                "search_profile",
                "docs_warning",
                "filters",
                "index",
                "results",
                "docs",
                "relative_file_path",
                "profile_hash",
            ],
        );
        let view = view_of(&both, limits(40, 16_000)).unwrap();
        assert_eq!(
            keys(&view),
            sorted(vec![
                "workspace",
                "search_profile",
                "docs_warning",
                "filters",
                "index",
                "results",
                "docs",
                "profile_hash"
            ])
        );
        assert_eq!(view["docs"][0]["relative_file_path"], "docs/guide.md");
        assert_eq!(view["docs"][0]["source_role"], "documentation");
        assert_eq!(keys(&view["docs"][0]), keys(&results(&view)[0]));
        assert_eq!(view["results"][0]["relative_file_path"], "src/a.rs");

        // Without a lane, neither key exists.
        let plain = view_of(
            &report(vec![hit("src/a.rs", 10, "x\n")]),
            limits(40, 16_000),
        )
        .unwrap();
        assert!(plain.get("docs").is_none() && plain.get("docs_warning").is_none());
    }

    #[test]
    fn docs_hits_share_the_response_ceiling_with_the_code_hits() {
        let code = numbered_lines(60);
        let docs: Vec<Hit> = (0..3)
            .map(|n| {
                let mut doc = hit(&format!("docs/part_{n}.md"), 1, &code);
                doc.chunk.language = "markdown".to_owned();
                doc
            })
            .collect();
        let base = with_docs(
            report(
                (0..5)
                    .map(|n| hit(&format!("src/f_{n}.rs"), 100 + n, &code))
                    .collect(),
            ),
            docs,
            None,
        );
        let whole = view_of(&base, limits(40, 1_000_000)).unwrap();
        let ceiling = chars_of(&whole) / 2;
        let view = view_of(&base, limits(40, ceiling)).unwrap();
        assert!(chars_of(&view) <= ceiling, "{}", chars_of(&view));
        assert_eq!(view["response_truncated"], true);
        let docs_list = view["docs"].as_array().unwrap();
        assert_eq!(docs_list.len(), 3);
        // The docs are shortened too, not left whole while the code pays.
        for hit in docs_list {
            assert_eq!(hit["code_truncated"], true);
        }
        let error = view_of(&base, limits(40, 1_000)).unwrap_err().to_string();
        assert!(error.contains("5 results and 3 docs"), "{error}");
    }

    #[test]
    fn empty_code_is_fine() {
        let view = view_of(&report(vec![hit("a.rs", 3, "")]), limits(40, 16_000)).unwrap();
        assert_eq!(results(&view)[0]["code"], "");
        assert!(results(&view)[0].get("code_truncated").is_none());
    }
}
