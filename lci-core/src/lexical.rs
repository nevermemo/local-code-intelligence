use crate::{language, path::normalize_path_separators};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
};

/// Most terms one query contributes to the `rg` pass. A safety bound on
/// command-line length and work, not a quality knob: rarity decides which
/// terms score (see [`select_terms`]). It must not exceed 32 because the
/// terms a line is credited with are a `u32` bit mask.
pub const MAX_QUERY_TERMS: usize = 24;
const _: () = assert!(MAX_QUERY_TERMS <= 32);
/// Shortest term kept, in characters.
const MIN_TERM_CHARS: usize = 3;
/// Terms of at most this many characters must match a whole word (see
/// `TermMatcher`); longer terms match as substrings. A three- or
/// four-letter fragment is otherwise inside far too many unrelated
/// identifiers (`padding`, `address`, `added`).
pub const WORD_MATCH_MAX_CHARS: usize = 4;
/// Terms credited in more than this fraction of the indexed files are dropped
/// from scoring, unless every term is that common.
pub const MAX_DOCUMENT_FREQUENCY: f64 = 0.25;
/// Most terms that score a chunk: the rarest ones that survive the
/// document-frequency cut.
pub const MAX_SCORING_TERMS: usize = 12;
/// BM25 term-frequency saturation: more credited lines of one term in a chunk
/// help with diminishing returns.
pub const BM25_K1: f64 = 1.2;
/// BM25 length normalisation (0 = none, 1 = full): a chunk longer than the
/// average needs proportionally more credited lines to score the same, so a
/// long chunk does not win just by containing many terms.
pub const BM25_B: f64 = 0.75;

/// One line ripgrep reported, with the query terms the line is credited with.
#[derive(Debug, Clone)]
pub struct LexicalMatch {
    pub relative_path: String,
    pub line: u32,
    /// Bit `i` is set when `LexicalSearch::terms[i]` is credited on this
    /// line. Never zero: uncredited lines are not reported.
    pub term_mask: u32,
}

/// The terms one lexical search ran with and the credited lines it found.
#[derive(Debug, Clone, Default)]
pub struct LexicalSearch {
    pub terms: Vec<String>,
    pub matches: Vec<LexicalMatch>,
    pub warning: Option<String>,
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    data: Option<EventData>,
}
#[derive(Deserialize)]
struct EventData {
    path: Option<TextValue>,
    lines: Option<TextValue>,
    line_number: Option<u32>,
}
/// ripgrep's `ArbitraryData`: `text` for valid UTF-8, otherwise only `bytes`.
#[derive(Deserialize)]
struct TextValue {
    text: Option<String>,
}

/// Generic words that never identify code: English function words and the
/// verbs that fill commit messages and issue titles. Applied to whole tokens
/// and to identifier sub-tokens, by both [`terms`] and [`query_terms`]. Bare
/// code nouns (`get`, `set`, `new`, `run`, `call`, `handle`, `check`, `move`,
/// `return`) are deliberately absent: when they are common the document-
/// frequency cut ([`select_terms`]) removes them, and when they are the
/// distinguishing word they must stay searchable.
const STOP_WORDS: &[&str] = &[
    // Function words.
    "the",
    "and",
    "for",
    "with",
    "from",
    "into",
    "when",
    "where",
    "which",
    "that",
    "this",
    "than",
    "then",
    "also",
    "only",
    "not",
    "are",
    "was",
    "were",
    "has",
    "have",
    "been",
    "will",
    "should",
    "can",
    "its",
    "all",
    "any",
    "each",
    "some",
    "such",
    "via",
    "per",
    "after",
    "before",
    "over",
    "under",
    // Query-instruction filler.
    "code",
    "generated",
    "given",
    "retrieve",
    "relevant",
    // Commit-message and code-review verbs.
    "add",
    "adds",
    "added",
    "remove",
    "removes",
    "removed",
    "fix",
    "fixes",
    "fixed",
    "use",
    "uses",
    "used",
    "make",
    "makes",
    "update",
    "updates",
    "updated",
    "change",
    "changes",
    "changed",
    "allow",
    "allows",
    "avoid",
    "avoids",
    "ensure",
    "ensures",
    "keep",
    "keeps",
    "rename",
    "renames",
    "support",
    "supports",
    "implement",
    "implements",
    "improve",
    "improves",
    "prevent",
    "prevents",
    "replace",
    "replaces",
    "refactor",
    "simplify",
];

/// The query's tokens (alphanumerics, `_`, and `#`), with `#` trimmed from
/// the ends; it stays inside a token, for symbol-style queries.
fn raw_tokens(query: &str) -> impl Iterator<Item = &str> {
    query
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#'))
        .map(|raw| raw.trim_matches('#'))
}

/// Whole lowercase tokens of the query, longest first, at most 12. Used for
/// language-server symbol queries; the ripgrep channel uses [`query_terms`].
pub fn terms(query: &str) -> Vec<String> {
    let mut counts = HashMap::<String, usize>::new();
    for raw in raw_tokens(query) {
        let term = raw.to_lowercase();
        if term.len() >= 3 && !STOP_WORDS.contains(&term.as_str()) {
            *counts.entry(term).or_default() += 1;
        }
    }
    let mut terms: Vec<_> = counts.into_keys().collect();
    terms.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    terms.truncate(12);
    terms
}

/// Whether `term` can be a query term: at least three characters and not a
/// stop word.
pub fn usable_term(term: &str) -> bool {
    term.chars().count() >= MIN_TERM_CHARS && !STOP_WORDS.contains(&term)
}

/// Splits an identifier at `_`, camelCase humps (`parseConfig`, and
/// `HTTPServer` as `HTTP` + `Server`), and letter/digit boundaries.
fn split_identifier(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    for (index, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some(previous) = current.chars().next_back() {
            let next_is_lower = chars.get(index + 1).is_some_and(|next| next.is_lowercase());
            let boundary = (previous.is_lowercase() && c.is_uppercase())
                || (previous.is_uppercase() && c.is_uppercase() && next_is_lower)
                || (previous.is_ascii_digit() != c.is_ascii_digit());
            if boundary {
                parts.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// The lowercase terms the ripgrep channel searches for: each token of the
/// query as written (original first), then its identifier sub-tokens
/// (`parseConfig` yields `parseconfig`, `parse`, `config`). Terms shorter than
/// three characters, [`STOP_WORDS`], and purely numeric sub-tokens (`256` in
/// `sha256`) are dropped and duplicates removed. If
/// more than [`MAX_QUERY_TERMS`] remain, the longest are kept; which of the
/// rest score is decided later, by rarity ([`select_terms`]). The result keeps
/// first-seen order, and its indices are the bit positions of
/// [`LexicalMatch::term_mask`].
pub fn query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::<String>::new();
    let mut found = Vec::new();
    for raw in raw_tokens(query) {
        let original = raw.to_lowercase();
        // Numeric fragments (`256` of `sha256`) carry no identifier meaning.
        let parts = split_identifier(raw)
            .into_iter()
            .filter(|part| !part.chars().all(|c| c.is_ascii_digit()))
            .map(|part| part.to_lowercase());
        for term in std::iter::once(original).chain(parts) {
            if usable_term(&term) && seen.insert(term.clone()) {
                found.push(term);
            }
        }
    }
    if found.len() > MAX_QUERY_TERMS {
        let mut order: Vec<usize> = (0..found.len()).collect();
        order.sort_by(|&a, &b| {
            found[b]
                .chars()
                .count()
                .cmp(&found[a].chars().count())
                .then(a.cmp(&b))
        });
        order.truncate(MAX_QUERY_TERMS);
        order.sort_unstable();
        found = order.into_iter().map(|i| found[i].clone()).collect();
    }
    found
}

fn fold_char(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

struct MatcherTerm {
    chars: Vec<char>,
    /// The term's lowercase bytes when it is ASCII. An ASCII line cannot hold
    /// a term with any other character.
    ascii: Option<Vec<u8>>,
}

/// Decides, per ripgrep line, which query terms the line really holds.
/// ripgrep matches every term as a case-insensitive substring; this narrows
/// the credit so a short term only counts as a word: it must start after a
/// non-alphanumeric character (or the line start) and end before one, or
/// before an uppercase letter (`add_item` and `addItem` hold `add`; `padding`,
/// `address`, and `added` do not). Longer terms count as substrings. Matching
/// is case-insensitive. Source lines are almost always ASCII, so those are
/// matched on bytes without allocating; any other line is matched on
/// characters with simple per-character folding. Both give the same answer
/// for an ASCII line.
struct TermMatcher {
    terms: Vec<MatcherTerm>,
}

impl TermMatcher {
    fn new(terms: &[String]) -> Self {
        Self {
            terms: terms
                .iter()
                .map(|term| MatcherTerm {
                    chars: term.chars().collect(),
                    ascii: term
                        .is_ascii()
                        .then(|| term.to_ascii_lowercase().into_bytes()),
                })
                .collect(),
        }
    }

    /// Bit `i` is set when term `i` is credited on `line`.
    fn credit(&self, line: &str) -> u32 {
        if line.is_ascii() {
            self.credit_ascii(line.as_bytes())
        } else {
            self.credit_unicode(line)
        }
    }

    fn credit_ascii(&self, line: &[u8]) -> u32 {
        let mut mask = 0u32;
        for (index, term) in self.terms.iter().enumerate() {
            let Some(needle) = term.ascii.as_deref() else {
                continue;
            };
            let Some(last_start) = line.len().checked_sub(needle.len()) else {
                continue;
            };
            let whole_word = needle.len() <= WORD_MATCH_MAX_CHARS;
            let credited = (0..=last_start).any(|start| {
                if line[start].to_ascii_lowercase() != needle[0] {
                    return false;
                }
                let end = start + needle.len();
                line[start..end].eq_ignore_ascii_case(needle)
                    && (!whole_word || {
                        let before = start == 0 || !line[start - 1].is_ascii_alphanumeric();
                        let after = end == line.len()
                            || !line[end].is_ascii_alphanumeric()
                            || line[end].is_ascii_uppercase();
                        before && after
                    })
            });
            if credited {
                mask |= 1 << index;
            }
        }
        mask
    }

    fn credit_unicode(&self, line: &str) -> u32 {
        let original: Vec<char> = line.chars().collect();
        let folded: Vec<char> = original.iter().map(|&c| fold_char(c)).collect();
        let mut mask = 0u32;
        for (index, term) in self.terms.iter().enumerate() {
            let term = &term.chars;
            let Some(last_start) = folded.len().checked_sub(term.len()) else {
                continue;
            };
            let whole_word = term.len() <= WORD_MATCH_MAX_CHARS;
            let credited = (0..=last_start).any(|start| {
                let end = start + term.len();
                folded[start..end] == term[..]
                    && (!whole_word || {
                        let before = start == 0 || !original[start - 1].is_alphanumeric();
                        let after = end == original.len()
                            || !original[end].is_alphanumeric()
                            || original[end].is_uppercase();
                        before && after
                    })
            });
            if credited {
                mask |= 1 << index;
            }
        }
        mask
    }
}

/// Resolves the default ripgrep command on Windows, including the copy bundled
/// with common VS Code installations. Explicit custom commands and paths are
/// returned unchanged.
pub fn resolve_ripgrep_path(configured: &str) -> PathBuf {
    resolve_ripgrep_path_from(
        configured,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
        std::env::var_os("ProgramFiles").as_deref(),
        cfg!(windows),
    )
}

fn resolve_ripgrep_path_from(
    configured: &str,
    path: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
    program_files: Option<&OsStr>,
    windows: bool,
) -> PathBuf {
    let configured_path = PathBuf::from(configured);
    if !matches!(configured.to_ascii_lowercase().as_str(), "rg" | "rg.exe") {
        return configured_path;
    }

    let names: &[&str] = if windows { &["rg.exe", "rg"] } else { &["rg"] };
    if let Some(found) = first_file_on_path(path, names) {
        return found;
    }
    if windows {
        for root in vscode_install_roots(local_app_data, program_files) {
            if let Some(found) = find_vscode_ripgrep(&root) {
                return found;
            }
        }
    }
    configured_path
}

/// The first of `names`, taken directory by directory in `path` order, that
/// is a file.
fn first_file_on_path<S: AsRef<str>>(path: Option<&OsStr>, names: &[S]) -> Option<PathBuf> {
    for directory in std::env::split_paths(path?) {
        for name in names {
            let candidate = directory.join(name.as_ref());
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Finds the executable `name` in the directories of the process's `PATH`:
/// the first directory that holds it wins. On Windows `name.exe` and
/// `name.cmd` are tried before the bare `name` in each directory, because npm
/// installs a bare, extensionless POSIX shell script next to the working
/// `name.cmd` shim, and that script is not a valid Win32 program. Nothing is
/// started: this only looks for files.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    find_on_path_in(name, std::env::var_os("PATH").as_deref(), cfg!(windows))
}

/// The first of `<name>.exe` and `<name>.cmd`, taken directory by directory in
/// `path` order, that is a file: the Win32 programs a bare name can mean. Unlike
/// [`find_on_path_in`] it never returns the extensionless name, so a POSIX
/// script earlier on `PATH` cannot hide a `.cmd` shim in a later directory.
/// `PATHEXT` is not consulted, so a `.bat` is not found by name.
pub fn find_windows_program_on_path_in(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    first_file_on_path(path, &[format!("{name}.exe"), format!("{name}.cmd")])
}

/// [`find_on_path`] over an explicit `PATH` value and platform, so the lookup
/// order is testable anywhere.
pub fn find_on_path_in(name: &str, path: Option<&OsStr>, windows: bool) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let names: Vec<String> = if windows {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            name.to_owned(),
        ]
    } else {
        vec![name.to_owned()]
    };
    first_file_on_path(path, &names)
}

fn vscode_install_roots(
    local_app_data: Option<&OsStr>,
    program_files: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(base) = local_app_data {
        let programs = Path::new(base).join("Programs");
        roots.push(programs.join("Microsoft VS Code"));
        roots.push(programs.join("Microsoft VS Code Insiders"));
        roots.push(programs.join("VSCodium"));
    }
    if let Some(base) = program_files {
        let base = Path::new(base);
        roots.push(base.join("Microsoft VS Code"));
        roots.push(base.join("Microsoft VS Code Insiders"));
        roots.push(base.join("VSCodium"));
    }
    roots
}

fn find_vscode_ripgrep(install_root: &Path) -> Option<PathBuf> {
    let mut app_roots = vec![install_root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(install_root) {
        let mut children: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort_by(|a, b| b.cmp(a));
        app_roots.extend(children);
    }

    const RELATIVE_PATHS: &[&str] = &[
        "resources/app/node_modules.asar.unpacked/@vscode/ripgrep/bin/rg.exe",
        "resources/app/node_modules.asar.unpacked/@vscode/ripgrep-universal/bin/win32-x64/rg.exe",
    ];
    app_roots.into_iter().find_map(|root| {
        RELATIVE_PATHS
            .iter()
            .map(|relative| root.join(relative))
            .find(|candidate| candidate.is_file())
    })
}

/// Searches the workspace with one ripgrep pass over every [`query_terms`]
/// term and credits each reported line with the terms it really holds (see
/// [`TermMatcher`]). Lines that hold no term after crediting are dropped. A
/// query with no usable term (only stop words or fragments) is an ordinary
/// empty answer: ripgrep is not started and no match is returned.
pub async fn search(rg_path: &str, root: &Path, query: &str) -> Result<LexicalSearch> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Ok(LexicalSearch::default());
    }
    let run = run_ripgrep(
        rg_path,
        root,
        &[
            "--json",
            "--line-number",
            "--ignore-case",
            "--fixed-strings",
        ],
        &terms,
    )
    .await?;
    let warning = run
        .failure
        .map(|failure| format!("Lexical retrieval incomplete: {failure}"));
    let matcher = TermMatcher::new(&terms);
    let matches = parse_matches(&run.stdout, root, |text| {
        let term_mask = matcher.credit(text);
        (term_mask != 0).then_some(term_mask)
    })?
    .into_iter()
    .map(|(relative_path, line, term_mask)| LexicalMatch {
        relative_path,
        line,
        term_mask,
    })
    .collect();
    Ok(LexicalSearch {
        terms,
        matches,
        warning,
    })
}

/// What one ripgrep process answered.
struct RgRun {
    stdout: Vec<u8>,
    /// Set when ripgrep exited with a code other than 0 (matches) and 1 (none):
    /// the code and the tail of its stderr. Its stdout is still the answer so
    /// far.
    failure: Option<String>,
}

/// Runs ripgrep with `flags`, one case-insensitive `--glob` per language adapter
/// and one `-e`
/// per needle over `root`. The command every ripgrep caller of the crate
/// starts.
async fn run_ripgrep(
    rg_path: &str,
    root: &Path,
    flags: &[&str],
    needles: &[String],
) -> Result<RgRun> {
    let resolved_rg = resolve_ripgrep_path(rg_path);
    let mut command = tokio::process::Command::new(&resolved_rg);
    command.args(flags);
    // The adapter globs match an extension in any case, like the registry (N94).
    command.arg("--glob-case-insensitive");
    for adapter in language::ADAPTERS {
        command.arg("--glob").arg(adapter.glob());
    }
    for needle in needles {
        command.arg("-e").arg(needle);
    }
    command
        .arg("--")
        .arg(root)
        // tokio's `output()` leaves stdin inherited; ripgrep must not read ours (N107).
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = command.output().await.with_context(|| {
        format!(
            "start ripgrep at {}; install rg, add it to PATH, or set ripgrep_path explicitly",
            resolved_rg.display()
        )
    })?;
    ensure!(
        output.status.code().is_some(),
        "ripgrep terminated without an exit code: {}",
        output.status
    );
    let failure = output
        .status
        .code()
        .filter(|&code| !matches!(code, 0 | 1))
        .map(|code| {
            let tail = &output.stderr[output.stderr.len().saturating_sub(512)..];
            format!(
                "ripgrep exited with code {code}: {}",
                String::from_utf8_lossy(tail).trim()
            )
        });
    Ok(RgRun {
        stdout: output.stdout,
        failure,
    })
}

/// The `match` events of ripgrep's `--json` output as (path relative to
/// `root`, 1-based line, whatever `accept` makes of the line's text). The one
/// parser of ripgrep's JSON lines: a line `accept` turns down (`None`) is
/// dropped before its path is resolved, a line that is not valid UTF-8 (it
/// arrives as `bytes` only) is dropped, and a line of output that is not JSON
/// is an error.
pub fn parse_matches<T>(
    stdout: &[u8],
    root: &Path,
    mut accept: impl FnMut(&str) -> Option<T>,
) -> Result<Vec<(String, u32, T)>> {
    // ripgrep repeats a file's path on each of its lines; resolving a path
    // touches the file system, so each distinct path is resolved once.
    let mut relative_paths = HashMap::<String, String>::new();
    let mut matches = Vec::new();
    for line in stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let event: Event = serde_json::from_slice(line).context("invalid ripgrep JSON")?;
        if event.kind != "match" {
            continue;
        }
        let Some(data) = event.data else { continue };
        let Some(path) = data.path.and_then(|path| path.text) else {
            continue;
        };
        let Some(text) = data.lines.and_then(|lines| lines.text) else {
            continue;
        };
        let Some(line) = data.line_number else {
            continue;
        };
        let Some(kept) = accept(&text) else {
            continue;
        };
        let relative = match relative_paths.get(&path) {
            Some(relative) => relative.clone(),
            None => {
                let relative = relative_to_root(root, Path::new(&path));
                relative_paths.insert(path, relative.clone());
                relative
            }
        };
        matches.push((relative, line, kept));
    }
    Ok(matches)
}

/// One line that holds a whole-identifier occurrence of a literal needle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralMatch {
    pub relative_path: String,
    /// 1-based.
    pub line: u32,
    pub text: String,
    /// Index into the needles of [`search_literals`]: the first needle whose
    /// whole-identifier occurrence the line holds.
    pub name: usize,
}

/// The lines [`search_literals`] found and the failure of ripgrep, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiteralSearch {
    pub matches: Vec<LiteralMatch>,
    pub warning: Option<String>,
}

/// The index of the first of `needles` that `text` holds as a whole
/// identifier: no letter, digit or `_` on either side (so `Shape` is not
/// found in `ShapeKind` or `Shape_kind`).
pub fn first_whole_identifier(text: &str, needles: &[String]) -> Option<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    needles.iter().position(|needle| {
        !needle.is_empty()
            && text.match_indices(needle.as_str()).any(|(start, _)| {
                let before = text[..start].chars().next_back();
                let after = text[start + needle.len()..].chars().next();
                !before.is_some_and(word) && !after.is_some_and(word)
            })
    })
}

/// Exact, case-sensitive whole-word search for `needles` over the files of the
/// language adapters: `rg --json --line-number --fixed-strings
/// --word-regexp --sort path --max-count <max_per_file>`. `--sort path` makes
/// ripgrep single-threaded, which the few needles a caller gives can afford,
/// and the order deterministic. A line is kept only when it holds a needle as
/// a whole identifier ([`first_whole_identifier`]); ripgrep's own word
/// boundary only narrows what it reports. No needle is an ordinary empty
/// answer: ripgrep is not started.
pub async fn search_literals(
    rg_path: &str,
    root: &Path,
    needles: &[String],
    max_per_file: u32,
) -> Result<LiteralSearch> {
    if needles.is_empty() {
        return Ok(LiteralSearch::default());
    }
    let max_count = max_per_file.max(1).to_string();
    let run = run_ripgrep(
        rg_path,
        root,
        &[
            "--json",
            "--line-number",
            "--fixed-strings",
            "--word-regexp",
            "--sort",
            "path",
            "--max-count",
            &max_count,
        ],
        needles,
    )
    .await?;
    let matches = parse_matches(&run.stdout, root, |text| {
        first_whole_identifier(text, needles).map(|name| (name, text.to_owned()))
    })?
    .into_iter()
    .map(|(relative_path, line, (name, text))| LiteralMatch {
        relative_path,
        line,
        text,
        name,
    })
    .collect();
    Ok(LiteralSearch {
        matches,
        warning: run
            .failure
            .map(|failure| format!("Literal search incomplete: {failure}")),
    })
}

/// `reported` (as ripgrep printed it) relative to `root`, with `/` separators.
fn relative_to_root(root: &Path, reported: &Path) -> String {
    let normalized = if reported.is_absolute() {
        dunce::canonicalize(reported).unwrap_or_else(|_| reported.to_path_buf())
    } else {
        root.join(reported)
    };
    let relative = normalized
        .strip_prefix(root)
        .unwrap_or(&normalized)
        .to_string_lossy();
    normalize_path_separators(&relative)
}

/// A credited line already attributed to an indexed chunk and its file.
#[derive(Debug, Clone, Copy)]
pub struct CreditedLine {
    /// Identifies the indexed file; two lines share a file when equal.
    pub file: usize,
    /// Identifies the chunk the line falls in.
    pub chunk: usize,
    /// Bit `i` is set when term `i` is credited on the line.
    pub term_mask: u32,
}

/// A chunk's lexical score: the sum over scoring terms of
/// [`bm25_term_score`] with `tf` = credited lines of that term in the chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredChunk {
    pub chunk: usize,
    pub score: f64,
    /// Lines of the chunk credited with at least one scoring term.
    pub credited_lines: u32,
}

/// Number of distinct files in which each of the `term_count` terms is
/// credited on at least one line. At most [`MAX_QUERY_TERMS`] terms are
/// tracked (the width of a term mask is the real bound).
pub fn document_frequencies(term_count: usize, lines: &[CreditedLine]) -> Vec<usize> {
    let mut file_masks = HashMap::<usize, u32>::new();
    for line in lines {
        *file_masks.entry(line.file).or_default() |= line.term_mask;
    }
    let mut frequencies = vec![0; term_count.min(MAX_QUERY_TERMS)];
    for mask in file_masks.values() {
        for (term, frequency) in frequencies.iter_mut().enumerate() {
            if mask & (1 << term) != 0 {
                *frequency += 1;
            }
        }
    }
    frequencies
}

/// Okapi-style inverse document frequency of a term credited in
/// `document_frequency` of `total_files` files; always positive.
pub fn inverse_document_frequency(document_frequency: usize, total_files: usize) -> f64 {
    let df = document_frequency as f64;
    (((total_files as f64 - df + 0.5) / (df + 0.5)) + 1.0).ln()
}

/// The terms (indices into the query terms) that score chunks, rarest first.
/// Terms credited in no file are skipped; terms credited in more than
/// [`MAX_DOCUMENT_FREQUENCY`] of `total_files` are dropped unless that would
/// leave none, in which case every credited term stays; at most
/// [`MAX_SCORING_TERMS`] of the rarest remain (ties keep query order).
pub fn select_terms(document_frequencies: &[usize], total_files: usize) -> Vec<usize> {
    let credited: Vec<usize> = (0..document_frequencies.len())
        .filter(|&term| document_frequencies[term] > 0)
        .collect();
    let rare: Vec<usize> = credited
        .iter()
        .copied()
        .filter(|&term| {
            document_frequencies[term] as f64 <= MAX_DOCUMENT_FREQUENCY * total_files as f64
        })
        .collect();
    let mut selected = if rare.is_empty() { credited } else { rare };
    selected.sort_by_key(|&term| (document_frequencies[term], term));
    selected.truncate(MAX_SCORING_TERMS);
    selected
}

/// One term's BM25 contribution to a chunk: `idf * tf * (k1 + 1) / (tf + k1 *
/// (1 - b + b * length / average_length))`, where `tf` is the chunk's credited
/// lines of the term and `length` the chunk's length in lines.
pub fn bm25_term_score(idf: f64, tf: u32, length: f64, average_length: f64) -> f64 {
    let tf = f64::from(tf);
    let norm = 1.0 - BM25_B + BM25_B * length / average_length;
    idf * tf * (BM25_K1 + 1.0) / (tf + BM25_K1 * norm)
}

/// Scores the chunks that hold at least one scoring term (see
/// [`select_terms`]) by BM25 over IDF-weighted term evidence instead of
/// matching-line count: a rare identifier outweighs many lines of a common
/// word, and a short chunk outweighs a long one holding the same terms.
/// `total_files` is the number of indexed files the document frequencies are
/// relative to. `chunk_lengths[i]` is the length in lines of chunk `i` for
/// every indexed chunk of the filtered set; their mean is the average length.
/// Chunks are returned by score descending, then chunk index.
pub fn score_chunks(
    term_count: usize,
    total_files: usize,
    chunk_lengths: &[u32],
    lines: &[CreditedLine],
) -> Vec<ScoredChunk> {
    let average_length = if chunk_lengths.is_empty() {
        1.0
    } else {
        chunk_lengths
            .iter()
            .map(|&length| f64::from(length.max(1)))
            .sum::<f64>()
            / chunk_lengths.len() as f64
    };
    let term_count = term_count.min(MAX_QUERY_TERMS);
    let frequencies = document_frequencies(term_count, lines);
    let selected = select_terms(&frequencies, total_files);
    let selected_mask = selected.iter().fold(0u32, |mask, &term| mask | (1 << term));
    #[derive(Default)]
    struct Evidence {
        per_term: [u32; MAX_QUERY_TERMS],
        credited_lines: u32,
    }
    let mut evidence = HashMap::<usize, Evidence>::new();
    for line in lines {
        let scoring = line.term_mask & selected_mask;
        if scoring == 0 {
            continue;
        }
        let chunk = evidence.entry(line.chunk).or_default();
        chunk.credited_lines += 1;
        for &term in &selected {
            if scoring & (1 << term) != 0 {
                chunk.per_term[term] += 1;
            }
        }
    }
    let idf: Vec<f64> = (0..term_count)
        .map(|term| inverse_document_frequency(frequencies[term], total_files))
        .collect();
    let mut scored: Vec<ScoredChunk> = evidence
        .into_iter()
        .map(|(chunk, evidence)| {
            let length = chunk_lengths
                .get(chunk)
                .map_or(average_length, |&length| f64::from(length.max(1)));
            ScoredChunk {
                chunk,
                score: selected
                    .iter()
                    .filter(|&&term| evidence.per_term[term] > 0)
                    .map(|&term| {
                        bm25_term_score(idf[term], evidence.per_term[term], length, average_length)
                    })
                    .sum(),
                credited_lines: evidence.credited_lines,
            }
        })
        .collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk.cmp(&b.chunk)));
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn relative_path_preserves_backslash_in_unix_filename() {
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join(r"a\b.rs"), "needle\n").unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/b.rs"), "needle\n").unwrap();

        // ripgrep reports absolute paths, so take the canonicalizing branch.
        assert_eq!(relative_to_root(&root, &root.join(r"a\b.rs")), r"a\b.rs");
    }

    #[cfg(unix)]
    fn fake_ripgrep(temp: &TempDir, stdout: &str, stderr: &str, exit: i32) -> PathBuf {
        let executable = temp.path().join("rg");
        write_script(
            &executable,
            &format!(
                "#!/bin/sh\ncat <<'STDOUT'\n{stdout}\nSTDOUT\ncat >&2 <<'STDERR'\n{stderr}\nSTDERR\nexit {exit}\n"
            ),
        );
        executable
    }

    /// Writes an executable script and waits until it can be executed. A
    /// child that another test thread forks while the file is open for
    /// writing inherits that descriptor until its own exec, and executing
    /// the script meanwhile fails with ETXTBSY; once one exec succeeds no
    /// process can still hold it open for writing.
    #[cfg(unix)]
    fn write_script(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        for _ in 0..200 {
            match std::process::Command::new(path).output() {
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                _ => break,
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn partial_ripgrep_output_keeps_matches_and_warns() {
        let temp = TempDir::new().unwrap();
        let stderr = format!(
            "{}\nblocked: Permission denied (os error 13)",
            "x".repeat(2048)
        );
        let rg = fake_ripgrep(
            &temp,
            r#"{"type":"match","data":{"path":{"text":"lib.rs"},"lines":{"text":"fn needle() {}\n"},"line_number":7}}"#,
            &stderr,
            2,
        );
        let found = search(rg.to_str().unwrap(), temp.path(), "needle")
            .await
            .unwrap();
        assert_eq!(found.terms, ["needle"]);
        assert_eq!(found.matches.len(), 1);
        assert_eq!(found.matches[0].relative_path, "lib.rs");
        assert_eq!(found.matches[0].line, 7);
        assert_eq!(found.matches[0].term_mask, 1);
        let warning = found.warning.unwrap();
        assert!(warning.contains("Permission denied (os error 13)"));
        assert!(warning.len() < 600, "stderr warning is not bounded");

        let rg = fake_ripgrep(&temp, "not JSON", "Permission denied", 2);
        let error = search(rg.to_str().unwrap(), temp.path(), "needle")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid ripgrep JSON"));

        write_script(&rg, "#!/bin/sh\nkill -TERM $$\n");
        assert!(
            search(rg.to_str().unwrap(), temp.path(), "needle")
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn partial_ripgrep_output_without_matches_still_warns() {
        let temp = TempDir::new().unwrap();
        let rg = fake_ripgrep(&temp, "", "blocked: Permission denied (os error 13)", 2);
        let found = search(rg.to_str().unwrap(), temp.path(), "needle")
            .await
            .unwrap();
        assert!(found.matches.is_empty());
        assert!(found.warning.unwrap().contains("Permission denied"));

        for exit in [0, 1] {
            let rg = fake_ripgrep(&temp, "", "", exit);
            let found = search(rg.to_str().unwrap(), temp.path(), "needle")
                .await
                .unwrap();
            assert!(found.matches.is_empty());
            assert!(found.warning.is_none());
        }
    }

    #[test]
    fn whole_identifier_credit_skips_longer_names_and_underscores() {
        let needles = vec!["Shape".to_owned(), "Circle".to_owned()];
        assert_eq!(first_whole_identifier("struct ShapeKind;", &needles), None);
        assert_eq!(
            first_whole_identifier("let Shape_kind = 1;", &needles),
            None
        );
        assert_eq!(first_whole_identifier("my_Shape", &needles), None);
        // One line holding both `ShapeKind` and a standalone `Shape`.
        assert_eq!(
            first_whole_identifier("impl ShapeKind for Shape {}", &needles),
            Some(0)
        );
        assert_eq!(first_whole_identifier("trait Shape {", &needles), Some(0));
        assert_eq!(first_whole_identifier("Shape::new()", &needles), Some(0));
        assert_eq!(first_whole_identifier("(Circle)", &needles), Some(1));
        // The first needle that stands alone wins, in needle order.
        assert_eq!(
            first_whole_identifier("Circle and Shape", &needles),
            Some(0)
        );
        assert_eq!(first_whole_identifier("shape", &needles), None);
    }

    #[test]
    fn parse_matches_keeps_match_events_the_closure_accepts() {
        let root = Path::new("root");
        let stdout = concat!(
            r#"{"type":"begin","data":{"path":{"text":"a.rs"}}}"#,
            "\n",
            r#"{"type":"match","data":{"path":{"text":"a.rs"},"lines":{"text":"fn one() {}\n"},"line_number":3}}"#,
            "\n",
            r#"{"type":"match","data":{"path":{"text":"a.rs"},"lines":{"text":"fn two() {}\n"},"line_number":9}}"#,
            "\n",
            r#"{"type":"match","data":{"path":{"text":"b.rs"},"lines":{"bytes":"/w=="},"line_number":1}}"#,
            "\n",
            r#"{"type":"end","data":{}}"#,
            "\n",
        );
        let kept = parse_matches(stdout.as_bytes(), root, |text| {
            text.contains("one").then_some(text.len())
        })
        .unwrap();
        assert_eq!(kept, vec![("a.rs".to_owned(), 3, 12)]);
        assert!(parse_matches(b"not JSON", root, |_| Some(())).is_err());
    }

    /// The runner that `search_literals` shares with `search` has to find
    /// ripgrep; where it is not installed the real-process tests are skipped.
    fn real_ripgrep() -> Option<String> {
        let resolved = resolve_ripgrep_path("rg");
        std::process::Command::new(&resolved)
            .arg("--version")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|_| resolved.to_string_lossy().into_owned())
    }

    /// Pins that ripgrep's `--word-regexp` treats `_` as a word character, the
    /// premise of the literal seeds, and that the needle of a line is the one
    /// that stands alone. Runs the real ripgrep over real files.
    #[tokio::test]
    async fn search_literals_runs_rg_whole_word_and_reports_the_needle() {
        let Some(rg) = real_ripgrep() else {
            eprintln!("skipped: ripgrep is not installed");
            return;
        };
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        std::fs::write(
            root.join("a.rs"),
            "trait Shape {}\nstruct ShapeKind;\nlet Shape_kind = 1;\nimpl ShapeKind for Shape {}\nfn circle() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("b.rs"), "let my_Shape = 1;\nfn Circle() {}\n").unwrap();
        let needles = vec!["Shape".to_owned(), "Circle".to_owned()];
        let found = search_literals(&rg, &root, &needles, 20).await.unwrap();
        assert!(found.warning.is_none(), "{:?}", found.warning);
        let got: Vec<(&str, u32, usize)> = found
            .matches
            .iter()
            .map(|m| (m.relative_path.as_str(), m.line, m.name))
            .collect();
        // `_` is a word character: neither `Shape_kind` nor `my_Shape` match,
        // `ShapeKind` alone does not, and the case-sensitive `circle` does
        // not either. Sorted by path.
        assert_eq!(got, vec![("a.rs", 1, 0), ("a.rs", 4, 0), ("b.rs", 2, 1)]);
        assert!(found.matches[0].text.contains("trait Shape"));
        // No needle: ripgrep is not started.
        let none = search_literals("missing-ripgrep-binary", &root, &[], 20)
            .await
            .unwrap();
        assert!(none.matches.is_empty() && none.warning.is_none());
    }

    /// The lexical channel reads a file of a registered language whatever the
    /// case of its extension (N94): the adapter globs are case-insensitive.
    /// Runs the real ripgrep over real files.
    #[tokio::test]
    async fn search_literals_reads_files_with_a_mixed_case_extension() {
        let Some(rg) = real_ripgrep() else {
            eprintln!("skipped: ripgrep is not installed");
            return;
        };
        let temp = TempDir::new().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        for name in ["Lib.RS", "Tool.Py", "plain.rs", "notes.txt"] {
            std::fs::write(root.join(name), "fn Marker() {}\n").unwrap();
        }
        let needles = vec!["Marker".to_owned()];
        let found = search_literals(&rg, &root, &needles, 20).await.unwrap();
        assert!(found.warning.is_none(), "{:?}", found.warning);
        let paths: Vec<&str> = found
            .matches
            .iter()
            .map(|m| m.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["Lib.RS", "Tool.Py", "plain.rs"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_literals_warns_on_a_failed_ripgrep_and_keeps_its_matches() {
        let temp = TempDir::new().unwrap();
        let rg = fake_ripgrep(
            &temp,
            r#"{"type":"match","data":{"path":{"text":"lib.rs"},"lines":{"text":"impl ShapeKind for Shape {}\n"},"line_number":4}}"#,
            "blocked: Permission denied (os error 13)",
            2,
        );
        let needles = vec!["Shape".to_owned()];
        let found = search_literals(rg.to_str().unwrap(), temp.path(), &needles, 20)
            .await
            .unwrap();
        assert_eq!(found.matches.len(), 1);
        assert_eq!(found.matches[0].name, 0);
        assert!(found.warning.unwrap().contains("Permission denied"));
    }

    #[tokio::test]
    async fn ripgrep_spawn_failure_still_fails() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing-ripgrep");
        let error = search(missing.to_str().unwrap(), temp.path(), "needle")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("start ripgrep"));
        assert!(error.to_string().contains("set ripgrep_path explicitly"));
    }

    #[test]
    fn query_terms_extract_split_and_filter() {
        // (query, the terms `query_terms` keeps, in order)
        let cases: &[(&str, &[&str])] = &[
            // identifiers split, the original first
            ("parseConfig", &["parseconfig", "parse", "config"]),
            (
                "build_production_feature_pipeline",
                &[
                    "build_production_feature_pipeline",
                    "build",
                    "production",
                    "feature",
                    "pipeline",
                ],
            ),
            // Acronym runs split before the last capital; letter/digit
            // boundaries split; fragments under three characters and purely
            // numeric fragments are dropped, but a whole token is never
            // dropped for being numeric.
            (
                "HTTPServer sha256 404",
                &["httpserver", "http", "server", "sha256", "sha", "404"],
            ),
            // Duplicates collapse, whatever their case or origin.
            ("config Config", &["config"]),
            ("ab c", &[]),
            // The stop list drops generic verbs...
            (
                "add fix the update handler for ensures when parsing",
                &["handler", "parsing"],
            ),
            // ...sub-tokens too; the original identifier survives.
            ("addUserName", &["addusername", "user", "name"]),
            ("add fix when the", &[]),
            // Bare code nouns stay searchable: the document-frequency cut
            // removes them when they are common; the stop list must not, or
            // a query about `get` or `run` loses its word.
            ("set get new run", &["set", "get", "new", "run"]),
            ("getUserName", &["getusername", "get", "user", "name"]),
        ];
        for (query, expected) in cases {
            assert_eq!(query_terms(query), *expected, "{query:?}");
        }

        // `terms` applies the same filters.
        assert_eq!(
            terms("lower syn AST expressions into generated Slang compute shader code"),
            vec![
                "expressions",
                "compute",
                "shader",
                "lower",
                "slang",
                "ast",
                "syn"
            ]
        );
        assert_eq!(
            terms("add fix the update handler for ensures when parsing"),
            vec!["handler", "parsing"]
        );
        assert_eq!(terms("handle call"), vec!["handle", "call"]);
    }

    #[test]
    fn caps_query_terms_keeping_the_longest_in_first_seen_order() {
        let mut query: String = ('a'..='z').map(|c| format!("xqz{c} ")).collect();
        query.push_str("longestwordhere");
        let terms = query_terms(&query);
        assert_eq!(terms.len(), MAX_QUERY_TERMS);
        assert_eq!(terms.first().map(String::as_str), Some("xqza"));
        assert_eq!(terms.last().map(String::as_str), Some("longestwordhere"));
        assert!(terms.contains(&"xqzw".to_owned()));
        assert!(!terms.contains(&"xqzx".to_owned()));
    }

    #[tokio::test]
    async fn a_query_of_only_stop_words_is_an_empty_search_without_starting_ripgrep() {
        let temp = TempDir::new().unwrap();
        // The command does not exist: reaching ripgrep would be an error.
        let found = search(
            "definitely-missing-ripgrep",
            temp.path(),
            "add fix when the",
        )
        .await
        .unwrap();
        assert!(found.terms.is_empty() && found.matches.is_empty());
    }

    #[test]
    fn short_terms_need_word_hits_long_terms_match_substrings() {
        let matcher = TermMatcher::new(&["add".to_owned(), "retry".to_owned(), "lsp".to_owned()]);
        let add = 0b001;
        let retry = 0b010;
        let lsp = 0b100;
        // A short term is a whole word: delimited, snake_case, or camelCase.
        assert_eq!(matcher.credit("add(1, 2)"), add);
        assert_eq!(matcher.credit("let x = add_item(y);"), add);
        assert_eq!(matcher.credit("list.addItem(y)"), add);
        assert_eq!(matcher.credit("Add the item"), add);
        // ...never a fragment of a longer word.
        assert_eq!(matcher.credit("padding: 4px"), 0);
        assert_eq!(matcher.credit("let address = 1;"), 0);
        assert_eq!(matcher.credit("it was added"), 0);
        assert_eq!(matcher.credit("fooAdd(x)"), 0);
        // A long term matches inside identifiers, case-insensitively.
        assert_eq!(matcher.credit("let n = RetryPolicy::new();"), retry);
        assert_eq!(matcher.credit("noretrying"), retry);
        // Every credited term sets its own bit.
        assert_eq!(matcher.credit("lsp: retry add"), add | retry | lsp);
        assert_eq!(matcher.credit("unrelated line"), 0);
    }

    #[test]
    fn ascii_lines_match_on_bytes_and_other_lines_on_characters() {
        let matcher = TermMatcher::new(&[
            "add".to_owned(),
            "retry".to_owned(),
            "lsp".to_owned(),
            "café".to_owned(),
        ]);
        // The two paths agree on every ASCII line.
        for line in [
            "add(1, 2)",
            "let x = add_item(y);",
            "list.addItem(y)",
            "Add the item",
            "ADD",
            "x.ADDItem",
            "padding: 4px",
            "let address = 1;",
            "it was added",
            "fooAdd(x)",
            "RetryPolicy",
            "noretrying",
            "RETRY",
            "lsp: retry add",
            "unrelated line",
            "ad",
            "",
        ] {
            assert_eq!(
                matcher.credit_ascii(line.as_bytes()),
                matcher.credit_unicode(line),
                "{line:?}"
            );
        }
        // A non-ASCII line is matched on characters, case-insensitively; the
        // four-character `café` is a whole word.
        assert_eq!(matcher.credit("Un CAFÉ noir, add_item"), 0b1001);
        assert_eq!(matcher.credit("x.CAFÉRetry"), 0b1010);
        assert_eq!(matcher.credit("des cafés"), 0);
        // A non-ASCII term cannot be held by an ASCII line.
        assert_eq!(matcher.credit("cafe"), 0);
    }

    fn credited(file: usize, chunk: usize, term_mask: u32) -> CreditedLine {
        CreditedLine {
            file,
            chunk,
            term_mask,
        }
    }

    #[test]
    fn rare_terms_outrank_common_and_frequent_terms_are_cut() {
        // Eight indexed files, one chunk each (chunk == file). Term 0 is
        // everywhere (and file 0 repeats it on ten lines), term 1 is in two
        // files (exactly the cutoff), term 2 is in one file.
        let mut lines = vec![credited(0, 0, 0b001); 10];
        lines.extend((3..7).map(|file| credited(file, file, 0b001)));
        lines.extend((1..3).map(|file| credited(file, file, 0b011)));
        lines.push(credited(7, 7, 0b101));

        let frequencies = document_frequencies(3, &lines);
        assert_eq!(frequencies, vec![8, 2, 1]);
        // The term in every file is cut; the rest are kept rarest first.
        assert_eq!(select_terms(&frequencies, 8), vec![2, 1]);

        let scored = score_chunks(3, 8, &[10; 8], &lines);
        // File 0's ten lines of the common term earn nothing...
        assert!(scored.iter().all(|s| s.chunk != 0 && s.chunk <= 7));
        assert_eq!(
            scored.iter().map(|s| s.chunk).collect::<Vec<_>>(),
            vec![7, 1, 2]
        );
        // ...and the rarest identifier outranks the middling one.
        let rare = inverse_document_frequency(1, 8);
        let middling = inverse_document_frequency(2, 8);
        assert!(rare > middling && middling > 0.0);
        assert!((scored[0].score - rare).abs() < 1e-9);
        assert!((scored[1].score - middling).abs() < 1e-9);
        assert!(scored.iter().all(|s| s.credited_lines == 1));
    }

    #[test]
    fn repeated_lines_raise_a_score_with_diminishing_returns() {
        // One term in two of eight equal-length files: three credited lines
        // beat one, but by tf * (k1 + 1) / (tf + k1), not by 3.
        let mut lines = vec![credited(0, 0, 1); 3];
        lines.push(credited(1, 1, 1));
        let scored = score_chunks(1, 8, &[10; 8], &lines);
        let idf = inverse_document_frequency(2, 8);
        assert_eq!(scored.iter().map(|s| s.chunk).collect::<Vec<_>>(), [0, 1]);
        assert!((scored[0].score - idf * 3.0 * (BM25_K1 + 1.0) / (3.0 + BM25_K1)).abs() < 1e-9);
        // A chunk of average length with one credited line scores its idf.
        assert!((scored[1].score - idf).abs() < 1e-9);
        assert!(scored[0].score > scored[1].score && scored[0].score < 3.0 * scored[1].score);
        assert_eq!(scored[0].credited_lines, 3);
        // Hand-computed: idf = ln 3.6 = 1.280934, and at average length the
        // length norm is 1, so 3 lines score 1.280934 * 3 * 2.2 / (3 + 1.2)
        // = 2.0129 against 1.2809 for one line.
        assert!(
            (scored[0].score - 2.0129).abs() < 1e-4,
            "{}",
            scored[0].score
        );
        assert!(
            (scored[1].score - 1.2809).abs() < 1e-4,
            "{}",
            scored[1].score
        );
    }

    #[test]
    fn a_short_chunk_beats_a_much_longer_chunk_with_the_same_term_counts() {
        // Chunks 0 (400 lines) and 1 (8 lines) each hold the term on two
        // lines. Chunk 0 comes first in index order, so only length can put
        // chunk 1 ahead.
        let lengths = [400, 8, 40, 40, 40, 40, 40, 40];
        let mut lines = vec![credited(0, 0, 1); 2];
        lines.extend(vec![credited(1, 1, 1); 2]);
        let scored = score_chunks(1, 8, &lengths, &lines);
        assert_eq!(scored.iter().map(|s| s.chunk).collect::<Vec<_>>(), [1, 0]);
        let idf = inverse_document_frequency(2, 8);
        let average = f64::from(lengths.iter().sum::<u32>()) / 8.0;
        assert!((scored[0].score - bm25_term_score(idf, 2, 8.0, average)).abs() < 1e-9);
        assert!((scored[1].score - bm25_term_score(idf, 2, 400.0, average)).abs() < 1e-9);
        assert!(scored[0].score > scored[1].score);
        // Hand-computed, independent of `bm25_term_score` (k1 = 1.2, b = 0.75):
        // df 2 of 8 gives idf = ln(6.5 / 2.5 + 1) = ln 3.6 = 1.280934, and the
        // average length is 648 / 8 = 81.
        //   chunk 1: tf 2, length 8:  norm = 0.25 + 0.75 * 8 / 81   = 0.324074
        //            1.280934 * 2 * 2.2 / (2 + 1.2 * 0.324074)      = 2.3593
        //   chunk 0: tf 2, length 400: norm = 0.25 + 0.75 * 400 / 81 = 3.953704
        //            1.280934 * 2 * 2.2 / (2 + 1.2 * 3.953704)      = 0.83567
        assert!((idf - 1.280_934).abs() < 1e-6);
        assert!((average - 81.0).abs() < 1e-12);
        assert!(
            (scored[0].score - 2.3593).abs() < 1e-4,
            "{}",
            scored[0].score
        );
        assert!(
            (scored[1].score - 0.83567).abs() < 1e-4,
            "{}",
            scored[1].score
        );
        // Equal lengths and counts score equally (ties go to the lower index).
        let even = score_chunks(1, 8, &[40; 8], &lines);
        assert!((even[0].score - even[1].score).abs() < 1e-12);
        assert_eq!(even.iter().map(|s| s.chunk).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn keeps_all_terms_when_every_term_is_common() {
        // Four files, both terms in all of them: nothing is rare, so nothing
        // is cut, and the more frequent chunk still wins.
        let mut lines = Vec::new();
        for file in 0..4 {
            lines.push(credited(file, file, 0b11));
        }
        lines.push(credited(2, 2, 0b01));
        assert_eq!(
            select_terms(&document_frequencies(2, &lines), 4),
            vec![0, 1]
        );
        let scored = score_chunks(2, 4, &[10; 4], &lines);
        assert_eq!(scored.len(), 4);
        assert_eq!(scored[0].chunk, 2);
        assert!(scored.iter().all(|s| s.score > 0.0));
        // A term credited nowhere never scores.
        assert_eq!(select_terms(&[0, 3], 3), vec![1]);
        assert!(select_terms(&[0, 0], 3).is_empty());
        assert!(score_chunks(2, 3, &[10; 3], &[]).is_empty());
    }

    #[test]
    fn caps_kept_terms_at_twelve_by_rarity() {
        // Twenty-four terms in a hundred files, term i in 24 - i of them.
        let frequencies: Vec<usize> = (0..24).map(|term| 24 - term).collect();
        let expected: Vec<usize> = (12..24).rev().collect();
        assert_eq!(select_terms(&frequencies, 100), expected);
        // Equal rarity keeps query order.
        assert_eq!(
            select_terms(&[1; 24], 100),
            (0..MAX_SCORING_TERMS).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ripgrep_path_resolution() {
        // an explicit path is preserved
        {
            let resolved =
                resolve_ripgrep_path_from(r"D:\tools\custom-rg.exe", None, None, None, true);
            assert_eq!(resolved, PathBuf::from(r"D:\tools\custom-rg.exe"));
        }

        // rg on PATH comes first
        {
            let temp = TempDir::new().unwrap();
            let path_rg = temp.path().join("rg.exe");
            std::fs::write(&path_rg, b"").unwrap();
            let resolved =
                resolve_ripgrep_path_from("rg", Some(temp.path().as_os_str()), None, None, true);
            assert_eq!(resolved, path_rg);
        }

        // the versioned VS Code bundled ripgrep
        {
            let temp = TempDir::new().unwrap();
            let bundled = temp
                .path()
                .join("Programs/Microsoft VS Code/645f29cc31/resources/app")
                .join("node_modules.asar.unpacked/@vscode/ripgrep-universal/bin/win32-x64/rg.exe");
            std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
            std::fs::write(&bundled, b"").unwrap();

            let resolved = resolve_ripgrep_path_from(
                "rg.exe",
                None,
                Some(temp.path().as_os_str()),
                None,
                true,
            );
            assert_eq!(resolved, bundled);
        }

        // the default command stays when ripgrep is not found
        {
            let temp = TempDir::new().unwrap();
            let resolved = resolve_ripgrep_path_from(
                "rg",
                Some(temp.path().as_os_str()),
                Some(temp.path().as_os_str()),
                None,
                true,
            );
            assert_eq!(resolved, PathBuf::from("rg"));
        }

        // on non-Windows only the bare rg name is tried, and the Windows-only VS Code bundle probe is skipped even when local_app_data and program_files are supplied
        {
            // On non-Windows, only the bare `rg` name is tried (no `.exe`
            // variant), and the Windows-only VS Code bundle probe is skipped
            // entirely even when local_app_data/program_files are supplied.
            let temp = TempDir::new().unwrap();
            let path_rg = temp.path().join("rg");
            std::fs::write(&path_rg, b"").unwrap();
            let resolved = resolve_ripgrep_path_from(
                "rg",
                Some(temp.path().as_os_str()),
                Some(temp.path().as_os_str()),
                Some(temp.path().as_os_str()),
                false,
            );
            assert_eq!(resolved, path_rg);
        }

        // the VS Code bundle is not probed on non-Windows
        {
            let temp = TempDir::new().unwrap();
            let bundled = temp
                .path()
                .join("Programs/Microsoft VS Code/resources/app")
                .join("node_modules.asar.unpacked/@vscode/ripgrep/bin/rg.exe");
            std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
            std::fs::write(&bundled, b"").unwrap();

            let resolved =
                resolve_ripgrep_path_from("rg", None, Some(temp.path().as_os_str()), None, false);
            assert_eq!(resolved, PathBuf::from("rg"));
        }
    }

    #[test]
    fn find_on_path_in_orders_names_and_directories() {
        // on Windows: exe, then cmd, then the bare name
        {
            let temp = TempDir::new().unwrap();
            let dir = temp.path();
            for name in ["tool", "tool.cmd", "tool.exe"] {
                std::fs::write(dir.join(name), b"").unwrap();
            }
            let path = Some(dir.as_os_str());
            assert_eq!(
                find_on_path_in("tool", path, true),
                Some(dir.join("tool.exe"))
            );
            std::fs::remove_file(dir.join("tool.exe")).unwrap();
            assert_eq!(
                find_on_path_in("tool", path, true),
                Some(dir.join("tool.cmd"))
            );
            // The bare script is the last resort, never preferred over a shim.
            std::fs::remove_file(dir.join("tool.cmd")).unwrap();
            assert_eq!(find_on_path_in("tool", path, true), Some(dir.join("tool")));
            std::fs::remove_file(dir.join("tool")).unwrap();
            assert_eq!(find_on_path_in("tool", path, true), None);
        }

        // off Windows: only the bare name
        {
            let temp = TempDir::new().unwrap();
            let dir = temp.path();
            std::fs::write(dir.join("tool.exe"), b"").unwrap();
            std::fs::write(dir.join("tool.cmd"), b"").unwrap();
            assert_eq!(find_on_path_in("tool", Some(dir.as_os_str()), false), None);
            std::fs::write(dir.join("tool"), b"").unwrap();
            assert_eq!(
                find_on_path_in("tool", Some(dir.as_os_str()), false),
                Some(dir.join("tool"))
            );
        }

        // the first directory that has the tool wins
        {
            let temp = TempDir::new().unwrap();
            let first = temp.path().join("first");
            let second = temp.path().join("second");
            for dir in [&first, &second] {
                std::fs::create_dir_all(dir).unwrap();
            }
            // A `.cmd` in an earlier directory beats an `.exe` in a later one:
            // directory order comes before name order.
            std::fs::write(first.join("tool.cmd"), b"").unwrap();
            std::fs::write(second.join("tool.exe"), b"").unwrap();
            let path = std::env::join_paths([&first, &second]).unwrap();
            assert_eq!(
                find_on_path_in("tool", Some(path.as_os_str()), true),
                Some(first.join("tool.cmd"))
            );
            // A directory named like the tool is not a tool.
            std::fs::remove_file(first.join("tool.cmd")).unwrap();
            std::fs::create_dir(first.join("tool.exe")).unwrap();
            assert_eq!(
                find_on_path_in("tool", Some(path.as_os_str()), true),
                Some(second.join("tool.exe"))
            );
            // No PATH, or an empty name, finds nothing.
            assert_eq!(find_on_path_in("tool", None, true), None);
            assert_eq!(find_on_path_in("", Some(path.as_os_str()), true), None);
        }
    }
}
