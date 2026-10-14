//! The compact `search_code` result an MCP profile endpoint can serve
//! (`[search_profiles.<name>] response = "compact"`).
//!
//! Presentation only: [`compact_view`] is applied to a finished
//! [`SearchReport`], after retrieval, so ranking, candidates, and the report's
//! serde shape are untouched. A hit keeps its locator (`relative_file_path`,
//! `start_line`, `end_line`), `language`, `source_role`, and `code`; the
//! retrieval metadata (scores, ranks, channels, hashes, absolute path) is
//! dropped. The envelope keeps `workspace.path`, `search_profile`, the
//! warning, notes, filters, and the index lifecycle.
//!
//! Two limits shorten `code` and never touch a locator or the order of hits:
//! at most `max_hit_lines` lines per hit, and a ceiling on the serialized
//! size of the whole result. A shortened hit says so with `code_truncated`,
//! `excerpt_end_line` (the last line shown, absent when none is), and a closing
//! line inside `code` that names the range to read from the file.

use super::SearchReport;
use crate::{config::CompactLimits, store::Hit};
use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// A line longer than this many characters is cut and marked, so one
/// minified line cannot defeat the line cap.
pub const COMPACT_MAX_LINE_CHARS: usize = 500;

/// Characters kept back for the closing line of a hit that the response
/// ceiling shortens (the line itself is about 100 to 140 characters).
const CLOSING_LINE_RESERVE: usize = 150;

/// `report` as the compact result: see the module documentation. Fails only
/// when the ceiling cannot hold the mandatory metadata of the results (every
/// locator, the envelope, and a closing line per hit).
pub fn compact_view(report: &SearchReport, limits: CompactLimits) -> Result<Value> {
    let ceiling = limits.max_response_chars;
    let build = |per_hit_budget: Option<usize>| -> Result<Value> {
        let mut view = envelope(report)?;
        if per_hit_budget.is_some() {
            view.insert("response_truncated".into(), Value::Bool(true));
        }
        let hits = report
            .results
            .iter()
            .map(|hit| compact_hit(hit, limits.max_hit_lines, per_hit_budget))
            .collect();
        view.insert("results".into(), Value::Array(hits));
        Ok(Value::Object(view))
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
        bail!(
            "the compact search result cannot fit compact_max_response_chars ({ceiling}): its locators and metadata alone take {floor_chars} characters for {} results; ask for a lower top_k or narrow the search with languages, include_paths, exclude_paths or source_roles",
            report.results.len()
        );
    }
    // The largest per-hit budget (serialized characters of one hit's code)
    // that still fits: `low` always fits, `high` never does.
    let mut low = 0;
    let mut high = report
        .results
        .iter()
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

/// Everything of the report except `results`, in the compact shape.
fn envelope(report: &SearchReport) -> Result<Map<String, Value>> {
    let mut view = Map::new();
    let mut workspace = Map::new();
    workspace.insert("path".into(), serde_json::to_value(&report.workspace.path)?);
    view.insert("workspace".into(), Value::Object(workspace));
    if let Some(profile) = &report.search_profile {
        view.insert("search_profile".into(), Value::String(profile.clone()));
    }
    if let Some(warning) = &report.warning {
        view.insert("warning".into(), Value::String(warning.clone()));
    }
    if !report.notes.is_empty() {
        view.insert("notes".into(), serde_json::to_value(&report.notes)?);
    }
    view.insert("filters".into(), serde_json::to_value(&report.filters)?);
    view.insert("index".into(), serde_json::to_value(&report.index)?);
    Ok(view)
}

/// One compact hit. `per_hit_budget` is the ceiling pass's cap on the
/// serialized characters of this hit's `code`, closing line included.
fn compact_hit(hit: &Hit, max_lines: usize, per_hit_budget: Option<usize>) -> Value {
    let chunk = &hit.chunk;
    let (code, truncated, last_shown) = excerpt(hit, max_lines, per_hit_budget);
    let mut object = Map::new();
    object.insert(
        "relative_file_path".into(),
        Value::String(chunk.relative_file_path.clone()),
    );
    object.insert("start_line".into(), chunk.start_line.into());
    object.insert("end_line".into(), chunk.end_line.into());
    object.insert("language".into(), Value::String(chunk.language.clone()));
    object.insert(
        "source_role".into(),
        serde_json::to_value(hit.source_role).unwrap_or(Value::Null),
    );
    object.insert("code".into(), Value::String(code));
    if truncated {
        object.insert("code_truncated".into(), Value::Bool(true));
        if let Some(last_shown) = last_shown {
            object.insert("excerpt_end_line".into(), last_shown.into());
        }
    }
    Value::Object(object)
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

/// Characters of the compact JSON text of `value`.
fn serialized_chars(value: &Value) -> usize {
    value.to_string().chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{IndexAction, SearchIndexLifecycle, Timings},
        chunk::Chunk,
        filter::{EffectiveFilter, FilterRequest, SourceRole},
        workspace::Workspace,
    };

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
            timings: Timings::default(),
            results,
            index: SearchIndexLifecycle {
                action: IndexAction::Reused,
                wait_ms: 0.0,
            },
            filters: EffectiveFilter::from_request(&FilterRequest::default())
                .unwrap()
                .report(),
            candidates: None,
            rerank_outcome: None,
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

    fn keys(object: &Value) -> Vec<&str> {
        object
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn a_compact_result_keeps_the_locators_and_code_and_drops_the_retrieval_metadata() {
        let code = "fn short() {\n    1\n}\n";
        let view =
            compact_view(&report(vec![hit("src/a.rs", 10, code)]), limits(40, 16_000)).unwrap();
        assert_eq!(
            keys(&view),
            ["workspace", "search_profile", "filters", "index", "results"]
        );
        assert_eq!(view["workspace"], serde_json::json!({"path": "/workspace"}));
        assert_eq!(view["search_profile"], "lean");
        // The locator comes first, so a reader sees where before what.
        assert_eq!(
            keys(&results(&view)[0]),
            [
                "relative_file_path",
                "start_line",
                "end_line",
                "language",
                "source_role",
                "code"
            ]
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
        let view = compact_view(&with, limits(40, 16_000)).unwrap();
        assert_eq!(view["warning"], "the reranker failed");
        assert_eq!(view["notes"][0], "a language server is still starting");
        let mut none = report(vec![]);
        none.search_profile = None;
        let view = compact_view(&none, limits(40, 16_000)).unwrap();
        assert_eq!(keys(&view), ["workspace", "filters", "index", "results"]);
        assert!(results(&view).is_empty());
    }

    #[test]
    fn a_hit_over_the_line_cap_is_cut_with_a_marker_naming_the_rest() {
        let code = numbered_lines(100);
        let view = compact_view(
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

    #[test]
    fn the_cap_leaves_a_chunk_of_exactly_the_limit_alone_and_cuts_one_line_more() {
        let exact = numbered_lines(40);
        let view =
            compact_view(&report(vec![hit("a.rs", 1, &exact)]), limits(40, 100_000)).unwrap();
        assert_eq!(results(&view)[0]["code"], exact.as_str());
        assert!(results(&view)[0].get("code_truncated").is_none());
        assert!(results(&view)[0].get("excerpt_end_line").is_none());
        let one_more = numbered_lines(41);
        let view = compact_view(
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
        let view = compact_view(
            &report(vec![hit("a.rs", 1, unterminated)]),
            limits(40, 100_000),
        )
        .unwrap();
        assert_eq!(results(&view)[0]["code"], unterminated);
    }

    #[test]
    fn the_cap_counts_lines_not_bytes_and_keeps_crlf_endings_and_multibyte_text() {
        let line = |n: usize| format!("// zeile {n} \u{e4}\u{f6}\u{fc}\r\n");
        let crlf: String = (1..=50).map(line).collect();
        let view = compact_view(&report(vec![hit("a.rs", 1, &crlf)]), limits(5, 100_000)).unwrap();
        let shown = results(&view)[0]["code"].as_str().unwrap();
        let prefix: String = (1..=5).map(line).collect();
        assert!(shown.starts_with(&prefix), "{shown:?}");
        assert!(shown.contains("lines 6-50"), "{shown:?}");
        assert_eq!(results(&view)[0]["excerpt_end_line"], 5);
    }

    #[test]
    fn a_huge_single_line_is_cut_on_a_character_boundary_and_marked() {
        // 3000 two-byte characters on one line: a minified file's chunk.
        let line = "\u{e9}".repeat(3000);
        let code = format!("{line}\n");
        let view =
            compact_view(&report(vec![hit("min.js", 7, &code)]), limits(40, 100_000)).unwrap();
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
        let view =
            compact_view(&report(vec![hit("a.rs", 1, &exact)]), limits(40, 100_000)).unwrap();
        assert_eq!(results(&view)[0]["code"], exact.as_str());
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
        let uncapped = compact_view(&full, limits(40, 1_000_000)).unwrap();
        let uncapped_chars = serialized_chars(&uncapped);
        assert!(uncapped_chars > 30_000, "{uncapped_chars}");
        assert!(uncapped.get("response_truncated").is_none());
        let expected = locators(&uncapped);
        for ceiling in [1_000_000, 30_000, 16_000, 13_000] {
            let view = compact_view(&full, limits(40, ceiling)).unwrap();
            assert!(
                serialized_chars(&view) <= ceiling,
                "{ceiling}: {}",
                serialized_chars(&view)
            );
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
        let view = compact_view(&full, limits(40, 1_000_000)).unwrap();
        // Locators, envelope and one closing line per hit fit, code does not.
        let locators_only = {
            let mut bare = view.clone();
            for hit in bare["results"].as_array_mut().unwrap() {
                hit.as_object_mut().unwrap().remove("code");
            }
            serialized_chars(&bare)
        };
        let ceiling = locators_only + 40 * 160;
        let squeezed = compact_view(&full, limits(40, ceiling)).unwrap();
        assert!(serialized_chars(&squeezed) <= ceiling);
        assert_eq!(locators(&squeezed), locators(&view));
        assert_eq!(squeezed["response_truncated"], true);

        // Below what the locators and metadata need there is no response to
        // give, only an actionable and bounded error.
        let error = compact_view(&full, limits(40, 1_000))
            .unwrap_err()
            .to_string();
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
            let mut view = compact_view(&one, limits(40, 100_000)).unwrap();
            view["results"][0].as_object_mut().unwrap().remove("code");
            serialized_chars(&view)
        };
        let ceiling = bare + CLOSING_LINE_RESERVE + 180;
        let view = compact_view(&one, limits(40, ceiling)).unwrap();
        assert!(serialized_chars(&view) <= ceiling);
        let first = &results(&view)[0];
        let shown = first["code"].as_str().unwrap();
        assert!(shown.starts_with("word word"), "{shown}");
        assert_eq!(first["excerpt_end_line"], 1);
        assert_eq!(first["code_truncated"], true);
    }

    #[test]
    fn empty_code_is_fine() {
        let view = compact_view(&report(vec![hit("a.rs", 3, "")]), limits(40, 16_000)).unwrap();
        assert_eq!(results(&view)[0]["code"], "");
        assert!(results(&view)[0].get("code_truncated").is_none());
    }
}
