//! `structured` gold: validator for the structured-config question sets, one
//! file `<repo>.json` per repository. It checks the files against the v1
//! schema (families with assignment, reader and documentation spans, three
//! questions per family), the per-repository counts, every span against the
//! checked-out tree, the leak rule for questions that must not name the key,
//! the dev/heldout split and the portability of every string. It scores
//! nothing and never reads LCI output; the facts it checks are the file's own.
//!
//! # Strata
//!
//! A family carries an optional `stratum`: `config` (the default) is the
//! config-semantics family described above. `dependency` is a package-manager
//! declaration: one assignment span (the declaring line(s) in one named
//! manifest), no readers (`readers_scope` `none:package-manager`), no
//! documentation or behaviour requirement, and exactly one `setting_location`
//! question that names the package but not the manifest path. The family-count
//! rules (8 or 6 per repository, three questions each) apply to config
//! families only; a file without config families is a dependency file and
//! needs 1 to 8 families. Dependency families split per repository with the
//! same shuffle, the first `floor(n / 4)` ids dev and the rest heldout (the
//! dev half only shakes the harness down; the held-out half decides).
//!
//! # Split algorithm (the gold builder must match it exactly)
//!
//! Per repository, take the family ids and sort them bytewise (`str` order).
//! Shuffle that list with a Fisher-Yates shuffle driven by SplitMix64 seeded
//! with the file's seed (20261018):
//!
//! ```text
//! state = seed                                   (u64)
//! next():  state += 0x9E3779B97F4A7C15           (wrapping)
//!          z = state
//!          z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9   (wrapping)
//!          z = (z ^ (z >> 27)) * 0x94D049BB133111EB   (wrapping)
//!          return z ^ (z >> 31)
//! for i in (1..n).rev():                         (i = n-1 down to 1)
//!     j = next() % (i + 1)
//!     swap(ids[i], ids[j])
//! ```
//!
//! The first 4 ids of the shuffled list are `dev`, every other id is
//! `heldout`. The split is per repository, so `next()` starts again from the
//! seed for each repository.
//!
//! # Checks
//!
//! Every failure is reported with its repository, item (a family id, a
//! question id or a JSON pointer) and a message under one of the check names
//! `schema`, `counts`, `spans`, `key_leak`, `split` and `portability`. The
//! report is printed (or written to `--out`) and the command fails when any
//! check failed.

use super::emit;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The seed every gold file must record and the split must be drawn with.
const SEED: u64 = 20_261_018;
/// Families per repository below which efficacy stops as underpowered.
const MIN_FAMILIES: usize = 6;
/// A full repository: 8 families, 4 of them dev and 4 heldout.
const FULL_FAMILIES: usize = 8;
const DEV_FAMILIES: usize = 4;
/// Most dependency families one repository may carry.
const MAX_DEPENDENCY_FAMILIES: usize = 8;
/// The `readers_scope` of a dependency family: no code reads it.
const NO_READERS: &str = "none:package-manager";
const STRATA: [&str; 2] = ["config", "dependency"];
/// Key segments at least this long must not appear in a `names_key: false`
/// question.
const MIN_SEGMENT: usize = 4;
/// Shortest last key segment a family may have: live grep makes no edges for
/// names under its own floor (four), and the preregistration asks for six.
const MIN_KEY_SEGMENT: usize = 6;

const FORMATS: [&str; 3] = ["json", "yaml", "toml"];
const SPLITS: [&str; 2] = ["dev", "heldout"];
const KINDS: [&str; 4] = ["assignment", "default", "example", "fixture"];
const HOWS: [&str; 4] = ["literal", "serde", "ast", "constant"];
const TYPES: [&str; 3] = [
    "setting_location",
    "behaviour_to_setting",
    "setting_to_reader",
];

// ------------------------------------------------------------------- split

/// SplitMix64: the generator behind the documented shuffle.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The documented shuffle: Fisher-Yates from the back, `j = next() % (i + 1)`.
fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut rng = SplitMix64(seed);
    for i in (1..items.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

/// The ids the seed assigns to `dev` for one repository's config family ids.
fn expected_dev(ids: &[String], seed: u64) -> BTreeSet<String> {
    expected_dev_count(ids, seed, DEV_FAMILIES)
}

/// The first `count` ids of the seeded shuffle of the sorted `ids`.
fn expected_dev_count(ids: &[String], seed: u64, count: usize) -> BTreeSet<String> {
    let mut sorted = ids.to_vec();
    sorted.sort();
    shuffle(&mut sorted, seed);
    sorted.into_iter().take(count).collect()
}

// ------------------------------------------------------------------ report

struct Finding {
    repo: String,
    item: String,
    check: &'static str,
    message: String,
}

#[derive(Default)]
struct Report {
    findings: Vec<Finding>,
}

impl Report {
    fn fail(&mut self, repo: &str, item: &str, check: &'static str, message: impl Into<String>) {
        self.findings.push(Finding {
            repo: repo.to_string(),
            item: item.to_string(),
            check,
            message: message.into(),
        });
    }
}

#[derive(Default)]
struct Counts {
    families: usize,
    questions: usize,
    dev: usize,
    heldout: usize,
}

// -------------------------------------------------------------------- tree

struct FileInfo {
    sha: String,
    lines: Vec<String>,
}

/// One repository checkout, with the files read so far.
struct Tree {
    root: Option<PathBuf>,
    cache: HashMap<String, Result<Rc<FileInfo>, String>>,
}

impl Tree {
    fn new(root: Option<PathBuf>) -> Tree {
        Tree {
            root,
            cache: HashMap::new(),
        }
    }

    fn load(&mut self, rel: &str) -> Result<Rc<FileInfo>, String> {
        let Some(root) = &self.root else {
            return Err("no --tree was given for this repository".to_string());
        };
        let root = root.clone();
        self.cache
            .entry(rel.to_string())
            .or_insert_with(|| {
                let path = root.join(rel);
                if !path.is_file() {
                    return Err(format!("path {rel:?} does not exist under the tree"));
                }
                let bytes =
                    std::fs::read(&path).map_err(|e| format!("cannot read {rel:?}: {e}"))?;
                // Hashed with CRLF read as LF: the content of the git blob,
                // whatever line endings the checkout wrote.
                let bytes = crlf_to_lf(&bytes);
                let text = String::from_utf8_lossy(&bytes);
                Ok(Rc::new(FileInfo {
                    sha: hex(&Sha256::digest(&bytes)),
                    lines: text.lines().map(str::to_string).collect(),
                }))
            })
            .clone()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `bytes` with every CRLF pair as LF.
fn crlf_to_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if !(b == b'\r' && bytes.get(i + 1) == Some(&b'\n')) {
            out.push(b);
        }
    }
    out
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Why `path` is not a repo-relative `/` path, if it is not.
pub(crate) fn unsafe_path(path: &str) -> Option<&'static str> {
    let bytes = path.as_bytes();
    if path.is_empty() {
        Some("empty path")
    } else if path.contains('\\') {
        Some("backslash in path")
    } else if path.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        Some("path is absolute")
    } else if path.split('/').any(|c| c.is_empty() || c == "..") {
        Some("path has an empty or `..` component")
    } else {
        None
    }
}

// ------------------------------------------------------------------- spans

struct SpanRule<'a> {
    /// assignments, readers and docs carry `end_line` and `sha256`.
    full: bool,
    /// Text the span's lines must contain (the assignment key's last segment).
    needle: Option<&'a str>,
}

fn check_span(
    r: &mut Report,
    tree: &mut Tree,
    repo: &str,
    family: &str,
    label: &str,
    span: &Value,
    rule: &SpanRule,
) {
    let mut fail = |message: String| r.fail(repo, family, "spans", format!("{label}: {message}"));
    let Some(path) = span.get("path").and_then(Value::as_str) else {
        return fail("missing string `path`".to_string());
    };
    if let Some(why) = unsafe_path(path) {
        return fail(format!("{why}: {path:?}"));
    }
    let info = match tree.load(path) {
        Ok(info) => info,
        Err(why) => return fail(why),
    };
    let count = info.lines.len() as i64;
    let line = span.get("line").and_then(Value::as_i64);
    let end = match span.get("end_line") {
        Some(v) => v.as_i64(),
        None if !rule.full => line,
        None => None,
    };
    let (Some(line), Some(end)) = (line, end) else {
        return fail(format!("{path}: `line` and `end_line` must be integers"));
    };
    if !(1 <= line && line <= end && end <= count) {
        fail(format!(
            "{path}: lines {line}-{end} outside 1..={count} (file has {count} lines)"
        ));
    } else if let Some(needle) = rule.needle {
        let text = info.lines[(line - 1) as usize..end as usize].join("\n");
        if !text.contains(needle) {
            fail(format!("{path}:{line}-{end} does not contain `{needle}`"));
        }
    }
    if rule.full {
        match span.get("sha256").and_then(Value::as_str) {
            Some(sha) if is_hex(sha, 64) => {
                if !sha.eq_ignore_ascii_case(&info.sha) {
                    fail(format!(
                        "{path}: sha256 recorded {sha}, file at the tree is {}",
                        info.sha
                    ));
                }
            }
            _ => fail(format!("{path}: `sha256` must be 64 hex characters")),
        }
    }
}

// ----------------------------------------------------------- one repository

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn check_repo(
    r: &mut Report,
    name: &str,
    gold: &Value,
    tree: &mut Tree,
    seen: &mut BTreeMap<String, String>,
) -> Counts {
    let mut counts = Counts::default();
    let top = |r: &mut Report, message: String| r.fail(name, "file", "schema", message);
    if gold.get("version").and_then(Value::as_i64) != Some(1) {
        top(r, "`version` must be 1".into());
    }
    if str_field(gold, "repo") != Some(name) {
        top(r, format!("`repo` must be {name:?} (the file name)"));
    }
    if !str_field(gold, "revision").is_some_and(|s| is_hex(s, 40)) {
        top(r, "`revision` must be 40 hex characters".into());
    }
    if gold.get("seed").and_then(Value::as_u64) != Some(SEED) {
        top(r, format!("`seed` must be {SEED}"));
    }
    if !gold.get("families").is_some_and(Value::is_array) {
        top(r, "`families` must be an array".into());
    }
    if !gold.get("questions").is_some_and(Value::is_array) {
        top(r, "`questions` must be an array".into());
    }

    // Families: schema and spans.
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    let mut splits: BTreeMap<String, String> = BTreeMap::new();
    let mut dependency_ids: BTreeSet<String> = BTreeSet::new();
    let mut manifests: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (i, fam) in list(gold, "families").iter().enumerate() {
        let id = str_field(fam, "id").unwrap_or_default();
        let item = if id.is_empty() {
            format!("families[{i}]")
        } else {
            id.to_string()
        };
        let bad = |r: &mut Report, message: String| r.fail(name, &item, "schema", message);
        if !id.starts_with(&format!("{name}.")) || id.len() <= name.len() + 1 {
            bad(r, format!("family id must be \"{name}.<key>\""));
        } else if let Some(other) = seen.insert(id.to_string(), name.to_string()) {
            bad(r, format!("family id is not unique (also in {other})"));
        }
        match str_field(fam, "split") {
            Some(s) if SPLITS.contains(&s) => {
                splits.insert(id.to_string(), s.to_string());
            }
            other => bad(r, format!("`split` must be dev or heldout, got {other:?}")),
        }
        match str_field(fam, "format") {
            Some(f) if FORMATS.contains(&f) => {}
            other => bad(
                r,
                format!("`format` must be json, yaml or toml, got {other:?}"),
            ),
        }
        let key = str_field(fam, "key").unwrap_or_default();
        if key.is_empty() {
            bad(r, "`key` must be a non-empty string".into());
        } else if key.rsplit('.').next().unwrap_or(key).chars().count() < MIN_KEY_SEGMENT {
            bad(
                r,
                format!("`key` {key:?}: last segment under {MIN_KEY_SEGMENT} characters"),
            );
        }
        keys.insert(id.to_string(), key.to_string());
        let dependency = match fam.get("stratum") {
            None => false,
            Some(Value::String(s)) if STRATA.contains(&s.as_str()) => s == "dependency",
            other => {
                bad(
                    r,
                    format!("`stratum` must be one of {STRATA:?}, got {other:?}"),
                );
                false
            }
        };
        if dependency {
            dependency_ids.insert(id.to_string());
            manifests.insert(
                id.to_string(),
                list(fam, "assignments")
                    .iter()
                    .filter_map(|a| str_field(a, "path").map(str::to_string))
                    .collect(),
            );
        }
        let scope = str_field(fam, "readers_scope").unwrap_or_default();
        let plain_scope = scope == "repo"
            || scope
                .strip_prefix("component:")
                .is_some_and(|c| !c.is_empty());
        if dependency {
            let no_readers = list(fam, "readers").is_empty();
            if scope == NO_READERS {
                if !no_readers {
                    bad(
                        r,
                        format!("`readers` must be empty with `readers_scope` {NO_READERS:?}"),
                    );
                }
            } else if no_readers {
                bad(
                    r,
                    format!("empty `readers` need `readers_scope` {NO_READERS:?}, got {scope:?}"),
                );
            } else if !plain_scope {
                bad(
                    r,
                    "`readers_scope` must be \"repo\", \"component:<name>\" or the no-readers scope"
                        .into(),
                );
            }
            if fam
                .get("behaviour")
                .is_some_and(|b| b.as_str().is_none_or(|b| b.trim().is_empty()))
            {
                bad(
                    r,
                    "`behaviour`, when present, must be a non-empty string".into(),
                );
            }
            if list(fam, "assignments").len() != 1 {
                bad(
                    r,
                    format!(
                        "a dependency family has exactly one assignment, found {}",
                        list(fam, "assignments").len()
                    ),
                );
            }
        } else {
            if !plain_scope {
                bad(
                    r,
                    "`readers_scope` must be \"repo\" or \"component:<name>\"".into(),
                );
            }
            if str_field(fam, "behaviour").is_none_or(|b| b.trim().is_empty()) {
                bad(r, "`behaviour` must be a non-empty string".into());
            }
        }
        let last = key.rsplit('.').next().unwrap_or(key);
        let last = Some(last).filter(|s| !s.is_empty());
        for (field, kinds, needle) in [
            ("assignments", Some(("kind", &KINDS[..])), last),
            ("readers", Some(("how", &HOWS[..])), None),
            ("docs", None, None),
        ] {
            let optional = dependency && field == "docs" && fam.get(field).is_none();
            if !optional && !fam.get(field).is_some_and(Value::is_array) {
                bad(r, format!("`{field}` must be an array"));
            }
            for (j, span) in list(fam, field).iter().enumerate() {
                let label = format!("{field}[{j}]");
                if dependency
                    && field == "assignments"
                    && str_field(span, "kind") != Some("assignment")
                {
                    bad(
                        r,
                        format!("{label}: a dependency assignment has `kind` \"assignment\""),
                    );
                }
                if let Some((k, allowed)) = kinds {
                    match str_field(span, k) {
                        Some(v) if allowed.contains(&v) => {}
                        other => r.fail(
                            name,
                            &item,
                            "schema",
                            format!("{label}: `{k}` must be one of {allowed:?}, got {other:?}"),
                        ),
                    }
                }
                let rule = SpanRule { full: true, needle };
                check_span(r, tree, name, &item, &label, span, &rule);
            }
        }
        if list(fam, "assignments").is_empty() {
            bad(r, "`assignments` must not be empty".into());
        }
        for (j, span) in list(fam, "excluded_readers").iter().enumerate() {
            let rule = SpanRule {
                full: false,
                needle: None,
            };
            check_span(
                r,
                tree,
                name,
                &item,
                &format!("excluded_readers[{j}]"),
                span,
                &rule,
            );
            if str_field(span, "reason").is_none_or(str::is_empty) {
                bad(
                    r,
                    format!("excluded_readers[{j}]: `reason` must be a non-empty string"),
                );
            }
        }
    }

    // Questions: schema and the key-leak rule.
    let mut question_ids = BTreeSet::new();
    let mut per_family: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    for (i, q) in list(gold, "questions").iter().enumerate() {
        let id = str_field(q, "id").unwrap_or_default();
        let item = if id.is_empty() {
            format!("questions[{i}]")
        } else {
            id.to_string()
        };
        let bad = |r: &mut Report, message: String| r.fail(name, &item, "schema", message);
        if id.is_empty() {
            bad(r, "`id` must be a non-empty string".into());
        } else if !question_ids.insert(id) {
            bad(r, "question id is not unique".into());
        }
        let family = str_field(q, "family").unwrap_or_default();
        let known = keys.contains_key(family);
        if !known {
            bad(r, format!("family {family:?} does not exist"));
        }
        let ty = str_field(q, "type").unwrap_or_default();
        if TYPES.contains(&ty) {
            if known {
                *per_family.entry(family).or_default().entry(ty).or_default() += 1;
            }
        } else {
            bad(r, format!("`type` must be one of {TYPES:?}, got {ty:?}"));
        }
        let text = str_field(q, "text").unwrap_or_default();
        if text.trim().is_empty() {
            bad(r, "`text` must be a non-empty string".into());
        }
        let Some(names_key) = q.get("names_key").and_then(Value::as_bool) else {
            bad(r, "`names_key` must be a boolean".into());
            continue;
        };
        if ty == "behaviour_to_setting" && names_key {
            bad(
                r,
                "a behaviour_to_setting question must have names_key false".into(),
            );
        }
        if known && dependency_ids.contains(family) {
            if ty != "setting_location" {
                bad(
                    r,
                    format!("a dependency question has type setting_location, got {ty:?}"),
                );
            }
            if !names_key {
                bad(r, "a dependency question must have names_key true".into());
            }
            if !text.to_lowercase().contains(&keys[family].to_lowercase()) {
                bad(
                    r,
                    format!(
                        "names_key is true but the text lacks the package `{}`",
                        keys[family]
                    ),
                );
            }
            for path in manifests.get(family).into_iter().flatten() {
                if text.contains(path.as_str()) {
                    bad(r, format!("the text spells the manifest path {path:?}"));
                }
            }
        }
        if !names_key && known {
            let lower = text.to_lowercase();
            for seg in keys[family]
                .split('.')
                .filter(|s| s.chars().count() >= MIN_SEGMENT)
            {
                if lower.contains(&seg.to_lowercase()) {
                    r.fail(
                        name,
                        &item,
                        "key_leak",
                        format!("names_key is false but the text contains key segment `{seg}`"),
                    );
                }
            }
        }
    }
    counts.questions = question_ids.len();
    for id in keys.keys() {
        let have = per_family.get(id.as_str());
        if dependency_ids.contains(id) {
            let total: usize = have.map_or(0, |m| m.values().sum());
            let n = have
                .and_then(|m| m.get("setting_location"))
                .copied()
                .unwrap_or(0);
            if total != 1 || n != 1 {
                r.fail(
                    name,
                    id,
                    "schema",
                    format!(
                        "a dependency family needs exactly one `setting_location` question and no other, found {n} of {total}"
                    ),
                );
            }
            continue;
        }
        for ty in TYPES {
            let n = have.and_then(|m| m.get(ty)).copied().unwrap_or(0);
            if n != 1 {
                r.fail(
                    name,
                    id,
                    "schema",
                    format!("needs exactly one `{ty}` question, found {n}"),
                );
            }
        }
    }

    // Counts and split: the family-count rules are the config stratum's; a
    // file with no config family is a dependency file.
    counts.families = keys.len();
    counts.dev = splits.values().filter(|s| *s == "dev").count();
    counts.heldout = splits.values().filter(|s| *s == "heldout").count();
    let config_ids: Vec<String> = keys
        .keys()
        .filter(|id| !dependency_ids.contains(*id))
        .cloned()
        .collect();
    let dependency_list: Vec<String> = dependency_ids.iter().cloned().collect();
    let config_dev = splits
        .iter()
        .filter(|(id, s)| *s == "dev" && !dependency_ids.contains(*id))
        .count();
    let config_heldout = splits
        .iter()
        .filter(|(id, s)| *s == "heldout" && !dependency_ids.contains(*id))
        .count();
    if config_ids.is_empty() {
        if dependency_list.is_empty() || dependency_list.len() > MAX_DEPENDENCY_FAMILIES {
            r.fail(
                name,
                "file",
                "counts",
                format!(
                    "a dependency file needs 1 to {MAX_DEPENDENCY_FAMILIES} families, found {}",
                    dependency_list.len()
                ),
            );
        }
    } else {
        if config_ids.len() < MIN_FAMILIES {
            r.fail(
                name,
                "file",
                "counts",
                format!(
                    "underpowered: {} families, at least {MIN_FAMILIES} are needed",
                    config_ids.len()
                ),
            );
        } else if config_ids.len() == FULL_FAMILIES && (config_dev, config_heldout) != (4, 4) {
            r.fail(
                name,
                "file",
                "counts",
                format!(
                    "{FULL_FAMILIES} families must split 4 dev and 4 heldout, found {config_dev} dev and {config_heldout} heldout"
                ),
            );
        }
        if dependency_list.len() > MAX_DEPENDENCY_FAMILIES {
            r.fail(
                name,
                "file",
                "counts",
                format!(
                    "at most {MAX_DEPENDENCY_FAMILIES} dependency families, found {}",
                    dependency_list.len()
                ),
            );
        }
    }
    let mut dev = expected_dev(&config_ids, SEED);
    dev.extend(expected_dev_count(
        &dependency_list,
        SEED,
        dependency_list.len() / 4,
    ));
    for (id, split) in &splits {
        let want = if dev.contains(id) { "dev" } else { "heldout" };
        if split != want {
            r.fail(
                name,
                id,
                "split",
                format!("split is {split}, the seed {SEED} assigns {want}"),
            );
        }
    }
    portability(r, name, "", gold);
    counts
}

/// Fails every string (and object key) that carries a machine-specific path.
fn portability(r: &mut Report, repo: &str, pointer: &str, v: &Value) {
    let check = |r: &mut Report, text: &str, at: &str| {
        if let Some(why) = machine_path(text) {
            r.fail(repo, at, "portability", format!("{why}: {text:?}"));
        }
    };
    match v {
        Value::String(s) => check(r, s, pointer),
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                portability(r, repo, &format!("{pointer}/{i}"), x);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let at = format!("{pointer}/{k}");
                check(r, k, &at);
                portability(r, repo, &at, x);
            }
        }
        _ => {}
    }
}

fn machine_path(s: &str) -> Option<&'static str> {
    let b = s.as_bytes();
    // A recorded value may hold an escape such as `\n`; only a backslash that
    // forms a Windows path (`C:\`, `\\host`, `\Users\`) is machine-specific.
    if s.contains("\\\\")
        || s.contains("\\Users\\")
        || (1..b.len().saturating_sub(1))
            .any(|i| b[i] == b':' && b[i + 1] == b'\\' && b[i - 1].is_ascii_alphabetic())
    {
        Some("backslash path")
    } else if s.contains("/Users/") || s.contains("/home/") {
        Some("home-directory path")
    } else if (0..b.len().saturating_sub(2)).any(|i| {
        b[i].is_ascii_alphabetic()
            && b[i + 1] == b':'
            && b[i + 2] == b'/'
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
    }) {
        Some("drive-letter path")
    } else {
        None
    }
}

// ------------------------------------------------------------------ driver

fn validate_files(
    files: &[(String, Value)],
    trees: &BTreeMap<String, PathBuf>,
) -> (Vec<Finding>, BTreeMap<String, Counts>) {
    let mut report = Report::default();
    let mut seen = BTreeMap::new();
    let mut all = BTreeMap::new();
    for (name, gold) in files {
        let mut tree = Tree::new(trees.get(name).cloned());
        let counts = check_repo(&mut report, name, gold, &mut tree, &mut seen);
        all.insert(name.clone(), counts);
    }
    (report.findings, all)
}

fn report_json(findings: &[Finding], counts: &BTreeMap<String, Counts>) -> Value {
    let mut by_check: BTreeMap<&str, usize> = BTreeMap::new();
    for f in findings {
        *by_check.entry(f.check).or_default() += 1;
    }
    let repos: serde_json::Map<String, Value> = counts
        .iter()
        .map(|(name, c)| {
            let value = json!({
                "families": c.families, "questions": c.questions,
                "dev": c.dev, "heldout": c.heldout,
            });
            (name.clone(), value)
        })
        .collect();
    json!({
        "ok": findings.is_empty(),
        "repos": repos,
        "failures_by_check": by_check,
        "failures": findings.iter().map(|f| json!({
            "repo": f.repo, "item": f.item, "check": f.check, "message": f.message,
        })).collect::<Vec<_>>(),
    })
}

/// `--tree <repo>=<dir>` arguments to a map.
fn parse_trees(specs: &[String]) -> Result<BTreeMap<String, PathBuf>> {
    let mut trees = BTreeMap::new();
    for spec in specs {
        let Some((repo, dir)) = spec
            .split_once('=')
            .filter(|(r, d)| !r.is_empty() && !d.is_empty())
        else {
            bail!("--tree must look like <repo>=<dir>, got {spec:?}");
        };
        trees.insert(repo.to_string(), PathBuf::from(dir));
    }
    Ok(trees)
}

pub fn run(gold_dir: &Path, tree_specs: &[String], out: Option<&Path>) -> Result<()> {
    let trees = parse_trees(tree_specs)?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(gold_dir)
        .with_context(|| format!("failed to read gold dir {}", gold_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        bail!("no <repo>.json files in {}", gold_dir.display());
    }
    let mut files = Vec::new();
    for path in &paths {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        files.push((name, value));
    }
    let (findings, counts) = validate_files(&files, &trees);
    emit(&report_json(&findings, &counts), out)?;
    for f in &findings {
        eprintln!("FAIL [{}] {} {}: {}", f.check, f.repo, f.item, f.message);
    }
    if findings.is_empty() {
        eprintln!("structured gold: all checks passed ({} files)", files.len());
        Ok(())
    } else {
        bail!("structured gold: {} check failure(s)", findings.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "[s]\nlisten = 1\nother = 2\n";

    /// A tree with one config file, unique per test.
    fn tree_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lci-structured-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("cfg")).unwrap();
        std::fs::write(dir.join("cfg/a.toml"), CONFIG).unwrap();
        dir
    }

    fn sha() -> String {
        hex(&Sha256::digest(CONFIG.as_bytes()))
    }

    /// A valid repo file `r` with `n` families, splits drawn by the seed.
    fn gold(n: usize) -> Value {
        let ids: Vec<String> = (0..n).map(|i| format!("r.f{i}")).collect();
        let dev = expected_dev(&ids, SEED);
        let span = || json!({"path": "cfg/a.toml", "line": 2, "end_line": 2, "sha256": sha()});
        let mut families = Vec::new();
        let mut questions = Vec::new();
        for id in &ids {
            let mut assignment = span();
            assignment["kind"] = json!("assignment");
            assignment["value"] = json!("1");
            let mut reader = span();
            reader["how"] = json!("literal");
            families.push(json!({
                "id": id, "split": if dev.contains(id) { "dev" } else { "heldout" },
                "format": "toml", "key": "s.listen",
                "assignments": [assignment], "readers": [reader], "readers_scope": "repo",
                "excluded_readers": [{"path": "cfg/a.toml", "line": 3, "reason": "computed key"}],
                "behaviour": "Chooses the network address it serves on.", "docs": [],
            }));
            for (n, ty, text, names) in [
                (1, "setting_location", "Where is `s.listen` set?", true),
                (
                    2,
                    "behaviour_to_setting",
                    "Which setting chooses the network address it serves on?",
                    false,
                ),
                (3, "setting_to_reader", "What reads `s.listen`?", true),
            ] {
                questions.push(json!({
                    "id": format!("{id}.q{n}"), "family": id, "type": ty,
                    "text": text, "names_key": names,
                }));
            }
        }
        json!({
            "version": 1, "repo": "r", "revision": "a".repeat(40), "seed": SEED,
            "families": families, "questions": questions,
        })
    }

    fn findings(g: &Value, tree: &Path) -> Vec<(&'static str, String)> {
        let trees = BTreeMap::from([("r".to_string(), tree.to_path_buf())]);
        let (found, _) = validate_files(&[("r".to_string(), g.clone())], &trees);
        found.into_iter().map(|f| (f.check, f.message)).collect()
    }

    fn has(found: &[(&'static str, String)], check: &str, needle: &str) -> bool {
        found.iter().any(|(c, m)| *c == check && m.contains(needle))
    }

    #[test]
    fn splitmix64_matches_the_reference_vector() {
        // Published first output of SplitMix64 with seed 0.
        assert_eq!(SplitMix64(0).next(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn the_shuffle_is_a_deterministic_permutation() {
        let mut a: Vec<u32> = (0..10).collect();
        let mut b = a.clone();
        shuffle(&mut a, SEED);
        shuffle(&mut b, SEED);
        assert_eq!(a, b);
        assert_ne!(a, (0..10).collect::<Vec<_>>());
        a.sort();
        assert_eq!(a, (0..10).collect::<Vec<_>>());
        let ids: Vec<String> = (0..8).map(|i| format!("r.f{i}")).collect();
        assert_eq!(expected_dev(&ids, SEED).len(), 4);
    }

    #[test]
    fn a_valid_minimal_fixture_passes() {
        let dir = tree_dir("valid");
        for n in [6, 8] {
            assert_eq!(findings(&gold(n), &dir), vec![], "{n} families");
        }
        let trees = BTreeMap::from([("r".to_string(), dir.clone())]);
        let (found, counts) = validate_files(&[("r".to_string(), gold(8))], &trees);
        assert!(found.is_empty());
        assert_eq!(
            (counts["r"].dev, counts["r"].heldout, counts["r"].questions),
            (4, 4, 24)
        );
        let report = report_json(&found, &counts);
        assert_eq!(report["ok"], true);
        // A checkout that wrote CRLF hashes as the LF blob the gold records.
        std::fs::write(dir.join("cfg/a.toml"), CONFIG.replace('\n', "\r\n")).unwrap();
        assert_eq!(findings(&gold(8), &dir), vec![], "CRLF checkout");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schema_violations_are_reported() {
        let dir = tree_dir("schema");
        let cases: Vec<(&str, Value, &str)> = vec![
            ("/version", json!(2), "`version` must be 1"),
            ("/repo", json!("other"), "`repo` must be"),
            ("/revision", json!("abc"), "`revision`"),
            ("/seed", json!(1), "`seed`"),
            ("/families/0/id", json!("x.f0"), "family id must be"),
            ("/families/0/split", json!("test"), "`split`"),
            ("/families/0/format", json!("ini"), "`format`"),
            ("/families/0/assignments/0/kind", json!("guess"), "`kind`"),
            ("/families/0/readers/0/how", json!("magic"), "`how`"),
            ("/questions/0/type", json!("nonsense"), "`type`"),
            ("/questions/0/family", json!("r.nope"), "does not exist"),
            ("/questions/1/id", json!("r.f0.q1"), "not unique"),
            ("/questions/1/names_key", json!(true), "names_key false"),
        ];
        for (ptr, value, needle) in cases {
            let mut g = gold(6);
            *g.pointer_mut(ptr).unwrap() = value;
            let found = findings(&g, &dir);
            assert!(has(&found, "schema", needle), "{ptr}: {found:?}");
        }
        // a family with only two of the three question types
        let mut g = gold(6);
        g["questions"].as_array_mut().unwrap().remove(2);
        let found = findings(&g, &dir);
        assert!(
            has(&found, "schema", "exactly one `setting_to_reader`"),
            "{found:?}"
        );
        // a duplicate family id across two files
        let trees = BTreeMap::from([("r".to_string(), dir.clone())]);
        let mut other = gold(6);
        other["repo"] = json!("s");
        let (found, _) = validate_files(&[("r".into(), gold(6)), ("s".into(), other)], &trees);
        assert!(
            found.iter().any(|f| f.message.contains("family id")),
            "dup/prefix"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn counts_flag_underpowered_repos_and_a_bad_eight_split() {
        let dir = tree_dir("counts");
        let found = findings(&gold(5), &dir);
        assert!(
            has(&found, "counts", "underpowered: 5 families"),
            "{found:?}"
        );
        let mut g = gold(8);
        let fam = g["families"].as_array_mut().unwrap();
        let heldout = fam.iter_mut().find(|f| f["split"] == "heldout").unwrap();
        heldout["split"] = json!("dev");
        let found = findings(&g, &dir);
        assert!(
            has(&found, "counts", "found 5 dev and 3 heldout"),
            "{found:?}"
        );
        assert!(found.iter().any(|(c, _)| *c == "split"), "{found:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn span_failures_are_reported_per_item() {
        let dir = tree_dir("spans");
        let cases: Vec<(&str, Value, &str)> = vec![
            (
                "/families/0/assignments/0/path",
                json!("cfg/missing.toml"),
                "does not exist",
            ),
            ("/families/0/assignments/0/path", json!("../a.toml"), "`..`"),
            (
                "/families/0/assignments/0/path",
                json!("/etc/a.toml"),
                "absolute",
            ),
            (
                "/families/0/assignments/0/path",
                json!("cfg\\a.toml"),
                "backslash in path",
            ),
            (
                "/families/0/assignments/0/end_line",
                json!(9),
                "outside 1..=3",
            ),
            ("/families/0/assignments/0/line", json!(0), "outside 1..=3"),
            (
                "/families/0/readers/0/sha256",
                json!("0".repeat(64)),
                "sha256 recorded",
            ),
            ("/families/0/readers/0/sha256", json!("short"), "64 hex"),
            (
                "/families/0/assignments/0/line",
                json!(3),
                "does not contain `listen`",
            ),
            (
                "/families/0/excluded_readers/0/line",
                json!(4),
                "outside 1..=3",
            ),
        ];
        for (ptr, value, needle) in cases {
            let mut g = gold(6);
            *g.pointer_mut(ptr).unwrap() = value;
            if ptr.ends_with("/line") && ptr.contains("assignments") {
                // keep end_line >= line for the needle case
                g["families"][0]["assignments"][0]["end_line"] = json!(3);
            }
            let found = findings(&g, &dir);
            assert!(has(&found, "spans", needle), "{ptr}: {found:?}");
        }
        // no tree for the repository
        let (found, _) = validate_files(&[("r".into(), gold(6))], &BTreeMap::new());
        assert!(
            found
                .iter()
                .any(|f| f.check == "spans" && f.message.contains("no --tree"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn questions_that_do_not_name_the_key_must_not_leak_it() {
        let dir = tree_dir("leak");
        let mut g = gold(6);
        g["questions"][1]["text"] = json!("Which setting changes the LISTEN address?");
        let found = findings(&g, &dir);
        assert!(has(&found, "key_leak", "key segment `listen`"), "{found:?}");
        // short segments are fine
        g["families"][0]["key"] = json!("s.ab");
        g["families"][0]["assignments"][0]["line"] = json!(1);
        g["families"][0]["assignments"][0]["end_line"] = json!(1);
        g["questions"][1]["text"] = json!("Which setting changes the ab number?");
        let found = findings(&g, &dir);
        assert!(!found.iter().any(|(c, _)| *c == "key_leak"), "{found:?}");
        // but a key that short is itself refused
        assert!(has(&found, "schema", "last segment under 6"), "{found:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_split_that_differs_from_the_seed_fails() {
        let dir = tree_dir("split");
        let mut g = gold(8);
        // swap one dev and one heldout family: counts stay 4/4, the split is wrong
        let fam = g["families"].as_array_mut().unwrap();
        let a = fam.iter().position(|f| f["split"] == "dev").unwrap();
        let b = fam.iter().position(|f| f["split"] == "heldout").unwrap();
        fam[a]["split"] = json!("heldout");
        fam[b]["split"] = json!("dev");
        let found = findings(&g, &dir);
        assert_eq!(
            found.iter().filter(|(c, _)| *c == "split").count(),
            2,
            "{found:?}"
        );
        assert!(!found.iter().any(|(c, _)| *c == "counts"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn machine_paths_in_any_string_fail() {
        let dir = tree_dir("portable");
        for bad in [
            "see C:/work/x",
            "see c:\\work",
            concat!("/Us", "ers/someone/x"),
            concat!("/ho", "me/someone/x"),
            "\\\\host\\share",
            "under \\Users\\someone",
        ] {
            let mut g = gold(6);
            g["families"][0]["behaviour"] = json!(bad);
            let found = findings(&g, &dir);
            assert!(
                found.iter().any(|(c, _)| *c == "portability"),
                "{bad}: {found:?}"
            );
        }
        for fine in [
            "see https://example.org/x",
            "key: value",
            "\"passages\\nQuery: {query}\"",
            "a:/b is odd but not a drive",
        ] {
            assert_eq!(
                machine_path(fine).is_some(),
                fine.starts_with("a:/b"),
                "{fine}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A valid dependency-only file `r` with `n` families.
    fn dep_gold(n: usize) -> Value {
        let ids: Vec<String> = (0..n).map(|i| format!("r.dep.p{i}")).collect();
        let dev = expected_dev_count(&ids, SEED, n / 4);
        let mut families = Vec::new();
        let mut questions = Vec::new();
        for id in &ids {
            families.push(json!({
                "id": id, "stratum": "dependency",
                "split": if dev.contains(id) { "dev" } else { "heldout" },
                "format": "toml", "key": "listen",
                "assignments": [{
                    "path": "cfg/a.toml", "line": 2, "end_line": 2, "sha256": sha(),
                    "value": "1", "kind": "assignment",
                }],
                "readers": [], "readers_scope": NO_READERS,
                "excluded_readers": [], "docs": [],
                "behaviour": "Declares the version of listen.",
            }));
            questions.push(json!({
                "id": format!("{id}.q1"), "family": id, "type": "setting_location",
                "text": "Where does the s crate declare the version of listen?", "names_key": true,
            }));
        }
        json!({
            "version": 1, "repo": "r", "revision": "a".repeat(40), "seed": SEED,
            "families": families, "questions": questions,
        })
    }

    #[test]
    fn a_valid_dependency_fixture_passes() {
        let dir = tree_dir("dep-valid");
        for n in 1..=MAX_DEPENDENCY_FAMILIES {
            assert_eq!(findings(&dep_gold(n), &dir), vec![], "{n} families");
        }
        let trees = BTreeMap::from([("r".to_string(), dir.clone())]);
        let (found, counts) = validate_files(&[("r".to_string(), dep_gold(5))], &trees);
        assert!(found.is_empty());
        assert_eq!(
            (counts["r"].dev, counts["r"].heldout, counts["r"].questions),
            (1, 4, 5)
        );
        // behaviour and docs may be present or absent
        let mut g = dep_gold(2);
        g["families"][0]
            .as_object_mut()
            .unwrap()
            .remove("behaviour");
        g["families"][1].as_object_mut().unwrap().remove("docs");
        assert_eq!(findings(&g, &dir), vec![]);
        // a config stratum written out is the same as the default
        let mut g = gold(6);
        g["families"][0]["stratum"] = json!("config");
        assert_eq!(findings(&g, &dir), vec![]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dependency_family_breaks_a_rule() {
        let dir = tree_dir("dep-bad");
        let cases: Vec<(&str, Value, &str, &str)> = vec![
            (
                "/families/0/stratum",
                json!("lockfile"),
                "schema",
                "`stratum`",
            ),
            (
                "/families/0/readers_scope",
                json!("repo"),
                "schema",
                "empty `readers` need",
            ),
            (
                "/families/0/readers_scope",
                json!("component:x"),
                "schema",
                "empty `readers` need",
            ),
            (
                "/families/0/assignments/0/kind",
                json!("example"),
                "schema",
                "`kind` \"assignment\"",
            ),
            (
                "/families/0/behaviour",
                json!(""),
                "schema",
                "`behaviour`, when present",
            ),
            (
                "/questions/0/type",
                json!("setting_to_reader"),
                "schema",
                "has type setting_location",
            ),
            (
                "/questions/0/names_key",
                json!(false),
                "schema",
                "names_key true",
            ),
            (
                "/questions/0/text",
                json!("Where is the dependency declared?"),
                "schema",
                "lacks the package",
            ),
            (
                "/questions/0/text",
                json!("Where does cfg/a.toml declare listen?"),
                "schema",
                "spells the manifest path",
            ),
            (
                "/families/0/split",
                json!("dev"),
                "split",
                "assigns heldout",
            ),
        ];
        for (ptr, value, check, needle) in cases {
            let mut g = dep_gold(1);
            *g.pointer_mut(ptr).unwrap() = value;
            let found = findings(&g, &dir);
            assert!(has(&found, check, needle), "{ptr}: {found:?}");
        }
        // readers present with the no-readers scope
        let mut g = dep_gold(1);
        g["families"][0]["readers"] = json!([{
            "path": "cfg/a.toml", "line": 2, "end_line": 2, "sha256": sha(), "how": "literal",
        }]);
        let found = findings(&g, &dir);
        assert!(
            has(&found, "schema", "`readers` must be empty"),
            "{found:?}"
        );
        // two assignments
        let mut g = dep_gold(1);
        let again = g["families"][0]["assignments"][0].clone();
        g["families"][0]["assignments"]
            .as_array_mut()
            .unwrap()
            .push(again);
        let found = findings(&g, &dir);
        assert!(
            has(&found, "schema", "exactly one assignment, found 2"),
            "{found:?}"
        );
        // a second question, and one of another type only
        let mut g = dep_gold(1);
        let mut q2 = g["questions"][0].clone();
        q2["id"] = json!("r.dep.p0.q2");
        g["questions"].as_array_mut().unwrap().push(q2);
        let found = findings(&g, &dir);
        assert!(
            has(&found, "schema", "exactly one `setting_location` question"),
            "{found:?}"
        );
        let mut g = dep_gold(1);
        g["questions"] = json!([]);
        let found = findings(&g, &dir);
        assert!(
            has(&found, "schema", "exactly one `setting_location` question"),
            "{found:?}"
        );
        // the assignment must still contain the package
        let mut g = dep_gold(1);
        g["families"][0]["key"] = json!("missing");
        g["questions"][0]["text"] = json!("Where is missing declared?");
        let found = findings(&g, &dir);
        assert!(
            has(&found, "spans", "does not contain `missing`"),
            "{found:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_files_need_one_to_eight_families() {
        let dir = tree_dir("dep-counts");
        let found = findings(&dep_gold(MAX_DEPENDENCY_FAMILIES + 1), &dir);
        assert!(
            has(&found, "counts", "needs 1 to 8 families, found 9"),
            "{found:?}"
        );
        let found = findings(&dep_gold(0), &dir);
        assert!(
            has(&found, "counts", "needs 1 to 8 families, found 0"),
            "{found:?}"
        );
        // the config minimum does not apply to a dependency file
        assert!(!has(
            &findings(&dep_gold(1), &dir),
            "counts",
            "underpowered"
        ));
        // a config file with a dependency stratum keeps its own rules
        let mut g = gold(6);
        let extra = dep_gold(3);
        for key in ["families", "questions"] {
            g[key]
                .as_array_mut()
                .unwrap()
                .extend(extra[key].as_array().unwrap().iter().cloned());
        }
        assert_eq!(findings(&g, &dir), vec![]);
        let mut g = gold(5);
        g["families"]
            .as_array_mut()
            .unwrap()
            .extend(dep_gold(3)["families"].as_array().unwrap().iter().cloned());
        let found = findings(&g, &dir);
        assert!(
            has(&found, "counts", "underpowered: 5 families"),
            "{found:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_families_split_floor_n_over_four() {
        let ids: Vec<String> = (0..5).map(|i| format!("r.dep.p{i}")).collect();
        assert_eq!(expected_dev_count(&ids, SEED, 2).len(), 2);
        let dir = tree_dir("dep-split");
        for n in [1, 3, 4, 5, 8] {
            let g = dep_gold(n);
            let dev = g["families"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|f| f["split"] == "dev")
                .count();
            assert_eq!(dev, n / 4, "{n} families");
            assert_eq!(findings(&g, &dir), vec![]);
            if n >= 4 {
                // swap one dev and one heldout: counts stay, the split is wrong
                let mut g = g;
                let fam = g["families"].as_array_mut().unwrap();
                let a = fam.iter().position(|f| f["split"] == "dev").unwrap();
                let b = fam.iter().position(|f| f["split"] == "heldout").unwrap();
                fam[a]["split"] = json!("heldout");
                fam[b]["split"] = json!("dev");
                let found = findings(&g, &dir);
                assert_eq!(found.iter().filter(|(c, _)| *c == "split").count(), 2);
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_families_may_not_use_the_no_readers_scope() {
        let dir = tree_dir("config-scope");
        let mut g = gold(6);
        g["families"][0]["readers_scope"] = json!(NO_READERS);
        let found = findings(&g, &dir);
        assert!(
            has(&found, "schema", "`readers_scope` must be"),
            "{found:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tree_arguments_are_parsed() {
        let trees = parse_trees(&["vite=/x/vite".into(), "lci=y".into()]).unwrap();
        assert_eq!(trees["vite"], PathBuf::from("/x/vite"));
        assert!(parse_trees(&["novalue".into()]).is_err());
        assert!(parse_trees(&["=x".into()]).is_err());
    }
}
