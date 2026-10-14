//! The compiler probe (L5) of slice M1.3: parsing `cargo check
//! --message-format=json` output into error sites and mapping each site to
//! the SCIP unit that holds it. The process handling lives in the
//! `compile_probe` example; this module is pure so it can be tested on
//! synthetic cargo output.
//!
//! Rules (pre-registration `docs/spike/multimap/v2/m1.3-prereg.md`, sections
//! 3 and 4): primary spans of level-error messages; a span from a macro
//! expansion is replaced by its expansion site when that is inside the
//! workspace, else skipped and counted; key `(code, file, line, column)`,
//! first occurrence kept; a site's node is the innermost SCIP unit holding the
//! 0-based start position, else it is unattributed.
//!
//! Expansion rule (one level): the expansion site is read from
//! `span.expansion.span` only, the call site rustc reports for the span; a
//! further expansion recorded inside that call site is not followed. If the
//! call site is outside the workspace the span is skipped and counted.

use crate::impact::ImpactIndex;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};

/// One error site after de-duplication. `line` and `col` are 1-based, as rustc
/// prints them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub code: Option<String>,
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub message: String,
    pub target: String,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub sites: Vec<Site>,
    pub external_spans_skipped: usize,
    pub error_messages: usize,
    /// `name|kind` of every target that produced an artifact or an error.
    pub reached: BTreeSet<String>,
    pub build_success: Option<bool>,
}

/// Paths are compared with forward slashes and relative to the workspace root.
/// The root prefix is matched character by character, ignoring case (Windows
/// paths), so no byte offset of a lowercased copy is ever used.
pub fn relative(path: &str, root: &str) -> String {
    let p = path.replace('\\', "/");
    let mut w = root.replace('\\', "/");
    if !w.ends_with('/') {
        w.push('/');
    }
    let mut chars = p.char_indices();
    for wc in w.chars() {
        match chars.next() {
            Some((_, pc)) if pc.to_lowercase().eq(wc.to_lowercase()) => {}
            _ => return p,
        }
    }
    let rest = chars.next().map_or(p.len(), |(i, _)| i);
    p[rest..].to_string()
}

/// A workspace source file as opposed to a registry or toolchain one: after
/// [`relative`] the path is still relative (no leading `/`, no drive colon, no
/// `..`). Registry, git and toolchain sources are reported by cargo with
/// absolute paths, so any member file (a root `build.rs`, `benches/`, a new
/// crate) counts without a list of directories.
pub fn in_workspace(rel: &str) -> bool {
    !(rel.is_empty() || rel.starts_with('/') || rel.starts_with("../") || rel.contains(':'))
}

fn unit_key(target: &Value) -> String {
    let name = target["name"].as_str().unwrap_or("?");
    let kind = target["kind"][0].as_str().unwrap_or("?");
    format!("{name}|{kind}")
}

fn num(v: &Value) -> u32 {
    v.as_u64().unwrap_or(0) as u32
}

/// Parse the JSON lines of `cargo check --message-format=json`.
pub fn parse_cargo_json(stdout: &str, root: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut seen: HashSet<(Option<String>, String, u32, u32)> = HashSet::new();
    for line in stdout.lines() {
        if !line.starts_with('{') {
            continue;
        }
        let Ok(j) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match j["reason"].as_str() {
            Some("compiler-artifact") => {
                out.reached.insert(unit_key(&j["target"]));
            }
            Some("build-finished") => out.build_success = j["success"].as_bool(),
            Some("compiler-message") => {
                let msg = &j["message"];
                let level = msg["level"].as_str().unwrap_or("");
                if level != "error" && level != "error: internal compiler error" {
                    continue;
                }
                out.error_messages += 1;
                out.reached.insert(unit_key(&j["target"]));
                let code = msg["code"]["code"].as_str().map(str::to_string);
                let target = j["target"]["name"].as_str().unwrap_or("?").to_string();
                let text: String = msg["message"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(160)
                    .collect();
                for span in msg["spans"].as_array().into_iter().flatten() {
                    if span["is_primary"].as_bool() != Some(true) {
                        continue;
                    }
                    let mut file = relative(span["file_name"].as_str().unwrap_or(""), root);
                    let mut l = num(&span["line_start"]);
                    let mut c = num(&span["column_start"]);
                    if !in_workspace(&file) {
                        let exp = &span["expansion"]["span"];
                        let ef = relative(exp["file_name"].as_str().unwrap_or(""), root);
                        if !exp.is_null() && in_workspace(&ef) {
                            file = ef;
                            l = num(&exp["line_start"]);
                            c = num(&exp["column_start"]);
                        } else {
                            out.external_spans_skipped += 1;
                            continue;
                        }
                    }
                    if seen.insert((code.clone(), file.clone(), l, c)) {
                        out.sites.push(Site {
                            code: code.clone(),
                            file,
                            line: l,
                            col: c,
                            message: text.clone(),
                            target: target.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out.sites
        .sort_by(|a, b| (a.file.as_str(), a.line, a.col).cmp(&(b.file.as_str(), b.line, b.col)));
    out
}

/// A `compile_error_at` edge: from the patch node to the unit holding the
/// site, or to the site node when no unit holds it.
pub fn edge_json(index: Option<&ImpactIndex>, patch: &str, site: &Site) -> Value {
    let unit = index.and_then(|ix| {
        ix.unit_at(
            &site.file,
            site.line.saturating_sub(1),
            site.col.saturating_sub(1),
        )
        .map(|d| ix.defs[d].symbol.clone())
    });
    let attributed = unit.is_some();
    let to = unit.unwrap_or_else(|| format!("site:{}:{}:{}", site.file, site.line, site.col));
    json!({
        "kind": "compile_error_at",
        "from": format!("patch:{patch}"),
        "to": to,
        "attributed": attributed,
        "code": site.code,
        "file": site.file,
        "line": site.line,
        "col": site.col,
        "message": site.message,
        "target": site.target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scip_graph::{RawDocument, RawOccurrence};
    use scip::types::symbol_information::Kind;
    use std::collections::HashMap;

    fn msg(code: &str, file: &str, l: u32, c: u32, primary: bool, level: &str) -> String {
        json!({"reason": "compiler-message", "target": {"name": "t", "kind": ["lib"]},
            "message": {"level": level, "message": "boom", "code": {"code": code},
            "spans": [{"file_name": file, "line_start": l, "column_start": c,
                       "is_primary": primary, "expansion": null}]}})
        .to_string()
    }

    #[test]
    fn dedups_across_units_and_keeps_only_primary_errors() {
        let out = [
            msg("E0599", "src\\a.rs", 3, 5, true, "error"),
            msg("E0599", "src/a.rs", 3, 5, true, "error"),
            msg("E0599", "src/a.rs", 4, 1, false, "error"),
            msg("E0599", "src/a.rs", 5, 1, true, "warning"),
            msg("E0061", "C:/tree/src/b.rs", 1, 2, true, "error"),
            "not json".to_string(),
            r#"{"reason":"build-finished","success":false}"#.to_string(),
        ]
        .join("\n");
        let p = parse_cargo_json(&out, "C:/tree");
        assert_eq!(p.error_messages, 4);
        assert_eq!(p.sites.len(), 2);
        assert_eq!(p.sites[0].file, "src/a.rs");
        assert_eq!(p.sites[1].file, "src/b.rs");
        assert_eq!(p.build_success, Some(false));
        assert!(p.reached.contains("t|lib"));
    }

    #[test]
    fn external_spans_are_skipped_or_replaced_by_the_expansion_site() {
        let ext = json!({"reason": "compiler-message", "target": {"name": "t", "kind": ["lib"]},
            "message": {"level": "error", "message": "m", "code": {"code": "E1"},
            "spans": [{"file_name": "C:/registry/x.rs", "line_start": 1, "column_start": 1,
                       "is_primary": true, "expansion": {"span": {"file_name": "src/m.rs",
                       "line_start": 7, "column_start": 9}}},
                      {"file_name": "C:/registry/y.rs", "line_start": 1, "column_start": 1,
                       "is_primary": true, "expansion": null}]}})
        .to_string();
        let p = parse_cargo_json(&ext, "C:/tree");
        assert_eq!(p.sites.len(), 1);
        assert_eq!((p.sites[0].line, p.sites[0].col), (7, 9));
        assert_eq!(p.external_spans_skipped, 1);
    }

    #[test]
    fn any_relative_path_is_in_the_workspace() {
        for rel in [
            "build.rs",
            "benches/b.rs",
            "newcrate/src/lib.rs",
            "src/a.rs",
        ] {
            assert!(in_workspace(rel), "{rel}");
        }
        for rel in [
            "/rustc/abc/library/core/src/lib.rs",
            "C:/reg/x.rs",
            "../x.rs",
            "",
        ] {
            assert!(!in_workspace(rel), "{rel}");
        }
    }

    #[test]
    fn relative_matches_the_root_without_byte_offsets() {
        assert_eq!(relative("c:\\Tree\\build.rs", "C:/tree"), "build.rs");
        assert_eq!(
            relative("C:/other/build.rs", "C:/tree"),
            "C:/other/build.rs"
        );
        // Lowercasing U+0130 changes the byte length; the strip must not rely on it.
        assert_eq!(relative("\u{130}/src/a.rs", "\u{130}"), "src/a.rs");
        assert_eq!(relative("x", "a-much-longer-root"), "x");
    }

    #[test]
    fn nested_expansion_is_not_followed() {
        // The call site is itself in an external file whose own expansion would
        // point into the workspace; the current rule reads one level and skips.
        let nested = json!({"reason": "compiler-message", "target": {"name": "t", "kind": ["lib"]},
            "message": {"level": "error", "message": "m", "code": {"code": "E1"},
            "spans": [{"file_name": "C:/registry/x.rs", "line_start": 1, "column_start": 1,
                       "is_primary": true, "expansion": {"span": {"file_name": "C:/registry/y.rs",
                       "line_start": 2, "column_start": 2,
                       "expansion": {"span": {"file_name": "src/m.rs",
                                              "line_start": 7, "column_start": 9}}}}}]}})
        .to_string();
        let p = parse_cargo_json(&nested, "C:/tree");
        assert!(p.sites.is_empty());
        assert_eq!(p.external_spans_skipped, 1);
    }

    #[test]
    fn sites_map_to_the_innermost_unit_or_stay_unattributed() {
        // `fn outer` spans lines 0..=9 and holds `fn inner` on lines 2..=4.
        let outer = "rust-analyzer cargo demo 0.1.0 a/outer().";
        let inner = "rust-analyzer cargo demo 0.1.0 a/inner().";
        let occ = |range, symbol: &str, enclosing| RawOccurrence {
            range,
            symbol: symbol.to_string(),
            roles: 1,
            enclosing: Some(enclosing),
        };
        let docs = vec![RawDocument {
            path: "src/a.rs".to_string(),
            occurrences: vec![
                occ((0, 3, 0, 8), outer, (0, 0, 9, 1)),
                occ((2, 7, 2, 12), inner, (2, 4, 4, 5)),
            ],
        }];
        let kinds: HashMap<String, Kind> = [(outer, Kind::Function), (inner, Kind::Function)]
            .into_iter()
            .map(|(s, k)| (s.to_string(), k))
            .collect();
        let ix = ImpactIndex::build(docs, &kinds, None);
        let site = |line, col| Site {
            code: Some("E0599".into()),
            file: "src/a.rs".into(),
            line,
            col,
            message: String::new(),
            target: "t".into(),
        };
        let hit = edge_json(Some(&ix), "p", &site(4, 9));
        assert_eq!(hit["to"], inner);
        assert_eq!(hit["attributed"], true);
        assert_eq!(edge_json(Some(&ix), "p", &site(8, 2))["to"], outer);
        let none = edge_json(Some(&ix), "p", &site(20, 1));
        assert_eq!(none["to"], "site:src/a.rs:20:1");
        assert_eq!(none["attributed"], false);
        assert_eq!(edge_json(None, "p", &site(4, 9))["attributed"], false);
    }
}
