//! `cargo xtask mine-queries`: turn a repository's git history into a
//! retrieval evaluation definition (roadmap E2).
//!
//! Each qualifying commit becomes one query. The query text is the commit
//! subject (plus the first body paragraph when it adds words) and the
//! relevant files are the supported-extension files the commit touched. Gold
//! labels come from merged changes rather than annotators, so a repository
//! yields hundreds of labelled queries at no annotation cost.
//!
//! Two approximations are deliberate and documented in the README:
//!
//! - `expected_paths` keeps only touched files that still exist at `HEAD`,
//!   because evaluation searches the checked-out tree, not the commit's
//!   parent. A file a later commit renamed is followed to its new path
//!   (git's rename detection, so a move with heavy edits is lost). A file
//!   may have been rewritten since, so a mined query can be harder or easier
//!   than it was when written.
//! - Query text is whatever the author wrote. It often names the files it
//!   changed or quotes an identifier, so every query is tagged `keyword` or
//!   `keyword-agnostic` and the report stratifies on that tag.
//!
//! Two more tag axes let the report separate queries a miss says little about.
//! `gold-drift` marks a commit with more than half of its changed lines (added
//! plus deleted, from `git log --numstat`) in files that are not gold:
//! unsupported or excluded files, deleted files, and files gone from `HEAD`
//! even after later renames. The gold is then a minority of the change, so a
//! miss may not be a retrieval failure. A pure move counts on both sides of the
//! rename. `short-query` and `long-query` mark query text of fewer than
//! [`SHORT_QUERY_MAX_WORDS`] words and of [`LONG_QUERY_MIN_WORDS`] words or
//! more.
//!
//! The result is evaluation data, never ranking input: it is generated, not
//! hand-edited, and must not be used to tune retrieval.
//!
//! History parsing and mining are pure (text in, text out), tested on synthetic
//! `git log` output; command tests also exercise temporary repositories.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use lci_core::evaluation::{EvaluationFile, validate_definition};

/// Extensions LCI indexes (exact, lowercase). Mirrors the `extension` fields
/// of `ADAPTERS` in `lci-core/src/language.rs`. The
/// `extensions_match_the_language_registry` test fails when the two drift apart.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "cs", "go", "java", "c", "h", "cpp", "hpp", "cc", "cxx",
    "hh", "hxx",
];

/// Commits touching more supported files than this are bulk changes (moves,
/// mass renames, vendoring) with no usable single-topic query.
const MAX_TOUCHED_FILES: usize = 10;
/// Shorter subjects ("Fix bug", "wip") carry no retrievable intent.
const MIN_SUBJECT_WORDS: usize = 4;
/// Upper bound on query text, in characters.
const MAX_QUERY_CHARS: usize = 300;
/// A body paragraph is appended only when it contributes at least this many
/// distinct words the subject lacks.
const MIN_NEW_BODY_WORDS: usize = 3;
/// Result window for mined queries: wide enough that the `@10` metrics are
/// not bounded by `top_k`.
const MINED_TOP_K: usize = 10;

const TAG_KEYWORD: &str = "keyword";
const TAG_KEYWORD_AGNOSTIC: &str = "keyword-agnostic";
const TAG_GOLD_DRIFT: &str = "gold-drift";
const TAG_SHORT_QUERY: &str = "short-query";
const TAG_LONG_QUERY: &str = "long-query";

/// Query text with fewer words than this (exclusive bound) is `short-query`.
/// Words are counted on the text sent to search, like [`MIN_SUBJECT_WORDS`].
const SHORT_QUERY_MAX_WORDS: usize = 8;
/// Query text with at least this many words is `long-query`.
const LONG_QUERY_MIN_WORDS: usize = 25;

/// Directory names whose contents are vendored, generated or build output.
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".venv",
    "__generated__",
    "__pycache__",
    "bower_components",
    "dist",
    "generated",
    "node_modules",
    "obj",
    "site-packages",
    "target",
    "third-party",
    "third_party",
    "vendor",
    "venv",
];

/// Lowercase file-name suffixes of dependency lockfiles (`Cargo.lock`,
/// `yarn.lock`, `package-lock.json`, `pnpm-lock.yaml`, `packages.lock.json`,
/// `go.sum`).
const LOCKFILE_SUFFIXES: &[&str] = &[".lock", "-lock.json", "-lock.yaml", ".lock.json", "go.sum"];

/// Lowercase file-name suffixes of generated code.
const GENERATED_SUFFIXES: &[&str] = &[
    ".min.js",
    ".bundle.js",
    ".d.ts",
    ".pb.go",
    ".g.cs",
    ".g.i.cs",
    ".designer.cs",
    "_pb2.py",
    "_pb2_grpc.py",
    "_generated.rs",
    "_generated.go",
];

/// Stems that identify nothing about a file: finding them in a query does not
/// make it a keyword query.
const GENERIC_STEMS: &[&str] = &[
    "app", "base", "common", "core", "index", "init", "lib", "main", "mod", "test", "tests",
    "types", "util", "utils",
];

/// Whole words (lowercase) that mark a subject as a formatting, lint or typo
/// fix. Such commits touch whatever file was dirty, so their message says
/// nothing about where the code lives. `typos` is the plural of `typo`;
/// `linter` and `formatter` can name a feature and are not listed.
const NOISE_WORDS: &[&str] = &[
    "clippy",
    "fmt",
    "formatting",
    "lint",
    "rustfmt",
    "typo",
    "typos",
    "whitespace",
];

/// Trailer keys (lowercase) that never start a useful body paragraph.
const TRAILER_KEYS: &[&str] = &[
    "acked-by",
    "cc",
    "change-id",
    "closes",
    "co-authored-by",
    "fixes",
    "helped-by",
    "reported-by",
    "resolves",
    "reviewed-by",
    "signed-off-by",
    "suggested-by",
    "tested-by",
];

/// `git log` pretty format: record separator, hash, subject and body, each
/// field closed by a unit separator. `--name-status` lines follow the format.
const LOG_FORMAT: &str = "--format=%x1e%H%x1f%s%x1f%b%x1f";
/// The same for the `--numstat` call, which needs only the hash: git drops
/// `--numstat` output when `--name-status` is also given.
const NUMSTAT_FORMAT: &str = "--format=%x1e%H%x1f";
const RECORD_SEPARATOR: char = '\u{1e}';
const FIELD_SEPARATOR: char = '\u{1f}';

// ---------------------------------------------------------------------------
// Parsed history
// ---------------------------------------------------------------------------

/// One path touched by a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    /// First letter of the `--name-status` code: `A`, `M`, `D`, `R`, `C`, `T`.
    status: char,
    /// The path after the change (the new path of a rename or copy).
    path: String,
    /// For a rename (`R`) or copy (`C`): the path it came from.
    renamed_from: Option<String>,
}

/// A file move recorded by a commit: `from` stopped existing and `to` took
/// its place. Copies are not moves and never become a `Rename`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rename {
    from: String,
    to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Commit {
    hash: String,
    subject: String,
    body: String,
    changes: Vec<Change>,
    /// Lines added plus deleted per path (see [`parse_numstat`]). Empty when
    /// the commit has no line data, which never flags `gold-drift`.
    lines: BTreeMap<String, u64>,
}

/// Parse the output of `git log --name-status` with [`LOG_FORMAT`].
///
/// Paths git quotes (tabs, newlines, quotes in the name) are skipped: they
/// cannot be represented as exact evaluation patterns anyway.
fn parse_log(raw: &str) -> Vec<Commit> {
    raw.split(RECORD_SEPARATOR)
        .filter_map(|record| {
            let mut fields = record.splitn(4, FIELD_SEPARATOR);
            let hash = fields.next()?.trim();
            let subject = fields.next()?.trim();
            let body = fields.next()?.trim();
            let status_lines = fields.next()?;
            if hash.is_empty() {
                return None;
            }
            Some(Commit {
                hash: hash.to_owned(),
                subject: subject.to_owned(),
                body: body.replace("\r\n", "\n"),
                changes: status_lines.lines().filter_map(parse_change).collect(),
                lines: BTreeMap::new(),
            })
        })
        .collect()
}

/// Parse the output of `git log --numstat --no-renames` with
/// [`NUMSTAT_FORMAT`] into the lines added plus deleted per path, by commit
/// hash. `--no-renames` lists a move as a delete and an add, so both of its
/// paths appear (the brace form `{src => lci-core/src}/x.rs` never does). A
/// binary file (`-\t-`) counts 0, and paths git quotes are skipped, as in
/// [`parse_change`].
fn parse_numstat(raw: &str) -> BTreeMap<String, BTreeMap<String, u64>> {
    raw.split(RECORD_SEPARATOR)
        .filter_map(|record| {
            let (hash, numstat_lines) = record.split_once(FIELD_SEPARATOR)?;
            let hash = hash.trim();
            if hash.is_empty() {
                return None;
            }
            let mut lines = BTreeMap::new();
            for (path, count) in numstat_lines.lines().filter_map(parse_numstat_line) {
                *lines.entry(path).or_insert(0) += count;
            }
            Some((hash.to_owned(), lines))
        })
        .collect()
}

fn parse_numstat_line(line: &str) -> Option<(String, u64)> {
    let mut columns = line.trim_end_matches('\r').splitn(3, '\t');
    let count = |column: &str| match column {
        "-" => Some(0),
        digits => digits.parse::<u64>().ok(),
    };
    let added = count(columns.next()?)?;
    let deleted = count(columns.next()?)?;
    let path = columns.next()?;
    if path.is_empty() || path.starts_with('"') {
        return None;
    }
    Some((path.to_owned(), added + deleted))
}

/// The lines a change touched: its path's count, plus for a rename the count
/// of the path it came from, because `--no-renames` reports a move as a
/// delete of the old path and an add of the new one. A copy leaves its source
/// where it is, so only the copy's own lines count.
fn change_churn(lines: &BTreeMap<String, u64>, change: &Change) -> u64 {
    let count = |path: &str| lines.get(path).copied().unwrap_or(0);
    let moved_from = match (change.status, &change.renamed_from) {
        ('R', Some(from)) => count(from),
        _ => 0,
    };
    count(&change.path) + moved_from
}

fn parse_change(line: &str) -> Option<Change> {
    let mut columns = line.trim_end_matches('\r').split('\t');
    let status = columns.next()?.chars().next()?;
    if !status.is_ascii_uppercase() {
        return None;
    }
    // Renames and copies list the old path first; the new one is the file
    // that can still exist at HEAD, and the old one lets later renames be
    // chased from it.
    let first = columns.next()?;
    let (renamed_from, path) = match columns.next() {
        Some(new) if matches!(status, 'R' | 'C') => (Some(first), new),
        Some(new) => (None, new),
        None => (None, first),
    };
    if path.is_empty() || path.starts_with('"') {
        return None;
    }
    Some(Change {
        status,
        path: path.to_owned(),
        renamed_from: renamed_from
            .filter(|old| !old.is_empty() && !old.starts_with('"'))
            .map(str::to_owned),
    })
}

/// The renames in `commits` (newest first) in chronological order, oldest
/// first, plus for each commit the offset of the first rename made by a
/// strictly later commit: `renames[offsets[i]..]` are the moves that happened
/// after `commits[i]`.
fn collect_renames(commits: &[Commit]) -> (Vec<Rename>, Vec<usize>) {
    let mut renames = Vec::new();
    let mut offsets = vec![0; commits.len()];
    for (index, commit) in commits.iter().enumerate().rev() {
        // Everything pushed so far came from older commits; this commit's own
        // moves are not "later" than itself, so the offset is taken after.
        for change in &commit.changes {
            if let (Some(from), 'R') = (&change.renamed_from, change.status) {
                renames.push(Rename {
                    from: from.clone(),
                    to: change.path.clone(),
                });
            }
        }
        offsets[index] = renames.len();
    }
    (renames, offsets)
}

/// Follow `path` through `later_renames` (chronological, oldest first) to the
/// name it has after all of them. One forward pass, so a chain `a -> b -> c`
/// ends at `c`, a move that preceded the commit is never in the list, and a
/// cycle `a -> b -> a` cannot loop.
// Renames are applied one at a time, so `a -> b` plus `b -> c` made by a
// single commit double-steps `a` to `c`.
fn chase_renames(path: &str, later_renames: &[Rename]) -> String {
    let mut current = path;
    for rename in later_renames {
        if rename.from == current {
            current = &rename.to;
        }
    }
    current.to_owned()
}

// ---------------------------------------------------------------------------
// Path rules
// ---------------------------------------------------------------------------

fn has_supported_extension(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name.rsplit_once('.'),
        Some((stem, extension)) if !stem.is_empty() && SUPPORTED_EXTENSIONS.contains(&extension)
    )
}

/// Lockfiles, vendored and generated paths, and paths that cannot be written
/// as an exact evaluation pattern (glob metacharacters, backslashes).
fn is_excluded_path(path: &str) -> bool {
    if path.contains(['*', '?', '[', ']', '{', '}', '\\']) {
        return true;
    }
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or_default().to_ascii_lowercase();
    if parts
        .iter()
        .any(|dir| EXCLUDED_DIRS.contains(&dir.to_ascii_lowercase().as_str()))
    {
        return true;
    }
    name.contains(".generated.")
        || LOCKFILE_SUFFIXES
            .iter()
            .chain(GENERATED_SUFFIXES)
            .any(|suffix| name.ends_with(suffix))
}

fn is_eligible_path(path: &str) -> bool {
    has_supported_extension(path) && !is_excluded_path(path)
}

// ---------------------------------------------------------------------------
// Query text
// ---------------------------------------------------------------------------

/// Lowercase alphanumeric runs, split at every other character.
fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
}

/// Split identifiers and prose into lowercase words: at every non-alphanumeric
/// character and at camelCase boundaries, so `SourceRole`, `source_role`,
/// `source-role.rs` and "source role" all yield `[source, role]`.
fn split_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (index, &ch) in chars.iter().enumerate() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if index > 0 && !current.is_empty() {
            let previous = chars[index - 1];
            let next_is_lower = chars.get(index + 1).is_some_and(|next| next.is_lowercase());
            // `fooBar` splits before `B`; `HTTPServer` splits before `S`.
            let boundary = ch.is_uppercase()
                && (previous.is_lowercase()
                    || previous.is_ascii_digit()
                    || (previous.is_uppercase() && next_is_lower));
            if boundary {
                words.push(std::mem::take(&mut current));
            }
        }
        current.extend(ch.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Strip a trailing `(#123)` pull-request reference.
fn clean_subject(subject: &str) -> &str {
    let subject = subject.trim();
    subject
        .rfind(" (#")
        .filter(|&open| {
            subject[open + 3..].strip_suffix(')').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
        .map_or(subject, |open| subject[..open].trim_end())
}

fn starts_like_list_or_fence(line: &str) -> bool {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    line.starts_with(['-', '*', '+', '\u{2022}'])
        || line.starts_with("```")
        || (digits > 0 && line[digits..].starts_with(['.', ')']))
}

fn is_trailer(line: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(key, _)| TRAILER_KEYS.contains(&key.trim().to_ascii_lowercase().as_str()))
}

/// The prose lines at the start of the body's first paragraph, joined by
/// spaces; `None` when the body opens with a list, fence or trailer.
fn first_paragraph(body: &str) -> Option<String> {
    let paragraph = body.trim().split("\n\n").next()?;
    let prose: Vec<&str> = paragraph
        .lines()
        .map(str::trim)
        .take_while(|line| !starts_like_list_or_fence(line) && !is_trailer(line))
        .collect();
    let joined = prose.join(" ");
    let joined = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// Cut `text` to at most `max` characters, preferring a word boundary.
fn truncate_words(text: &str, max: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(max) else {
        return text.to_owned();
    };
    let head = &text[..end];
    let cut = if text[end..].starts_with(char::is_whitespace) {
        head
    } else {
        head.rsplit_once(char::is_whitespace)
            .map_or(head, |(words, _)| words)
    };
    cut.trim_end_matches(|ch: char| ch.is_whitespace() || ",;:-([{".contains(ch))
        .to_owned()
}

/// The query text for a commit: the subject, followed by the first body
/// paragraph when that paragraph adds at least [`MIN_NEW_BODY_WORDS`]
/// distinct words, capped at [`MAX_QUERY_CHARS`] characters.
fn query_text(subject: &str, body: &str) -> String {
    let subject = clean_subject(subject);
    let text = match first_paragraph(body) {
        Some(paragraph) if adds_words(subject, &paragraph) => {
            let separator = if subject.ends_with(['.', '!', '?', ':', ';']) {
                " "
            } else {
                ". "
            };
            format!("{subject}{separator}{paragraph}")
        }
        _ => subject.to_owned(),
    };
    truncate_words(&text, MAX_QUERY_CHARS)
}

fn adds_words(subject: &str, paragraph: &str) -> bool {
    let known: BTreeSet<String> = tokens(subject).collect();
    let new: BTreeSet<String> = tokens(paragraph)
        .filter(|token| !known.contains(token))
        .collect();
    new.len() >= MIN_NEW_BODY_WORDS
}

// ---------------------------------------------------------------------------
// Tagging
// ---------------------------------------------------------------------------

/// A file stem identifies the file only if a query naming it is no accident:
/// two or more words (`source_role`), or one word that is not a generic stem
/// like `mod` or `main`.
fn is_distinctive(words: &[String]) -> bool {
    match words {
        [] => false,
        [word] => word.chars().count() >= 3 && !GENERIC_STEMS.contains(&word.as_str()),
        _ => true,
    }
}

fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// Whether the query text names one of the touched files: the words of a
/// distinctive file stem appear in the query, in order and adjacent, however
/// they are spelled (`source_role`, `SourceRole`, "source role").
fn names_a_touched_file<S: AsRef<str>>(query: &str, touched: &[S]) -> bool {
    let query_words = split_words(query);
    touched.iter().any(|path| {
        let stem_words = split_words(file_stem(path.as_ref()));
        is_distinctive(&stem_words)
            && query_words
                .windows(stem_words.len())
                .any(|window| window == stem_words.as_slice())
    })
}

/// Common proper nouns and product names spelled with an inner capital. They
/// are prose in a commit message, not identifiers, so the inner-capital rule
/// ignores a word that is exactly one of these. Keep the list short: a longer
/// word that merely contains one (`GitHubClient`) is still an identifier.
const PROPER_NOUNS: &[&str] = &[
    "GitHub",
    "GitLab",
    "iOS",
    "JavaScript",
    "LanceDB",
    "macOS",
    "nDCG",
    "OpenAI",
    "PowerShell",
    "TypeScript",
    "WebAssembly",
];

/// Whether the text contains a token shaped like a code identifier, which a
/// lexical search matches without any understanding of the change:
///
/// - `snake_case` (an underscore between letters or digits),
/// - a `::` path (`App::index_lock`),
/// - a `--flag`,
/// - a `/` between lowercase letters or digits (`src/lsp`), so prose
///   alternatives such as "LSP/embedding" or "Go/Java" do not count,
/// - a `.` between word characters (`index.rs`, `Node.js`), except decimals,
///   version numbers and abbreviations such as "e.g.", which are prose,
/// - an inner capital: `camelCase`, `PascalCase`, or an acronym followed by a
///   word (`HTTPServer`). A capitalised first letter, an all-caps acronym, a
///   plural acronym (`APIs`) and the [`PROPER_NOUNS`] spellings are ordinary
///   prose.
fn has_identifier_shape(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let at = |index: Option<usize>| index.and_then(|index| chars.get(index)).copied();
    // `\w`: letters, digits and the underscore.
    let word = |index: Option<usize>| at(index).is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
    let letter = |index: Option<usize>| at(index).is_some_and(char::is_alphabetic);
    let lower = |index: Option<usize>| at(index).is_some_and(char::is_lowercase);
    let lower_or_digit =
        |index: Option<usize>| at(index).is_some_and(|ch| ch.is_lowercase() || ch.is_ascii_digit());
    let upper = |index: Option<usize>| at(index).is_some_and(char::is_uppercase);
    // Length of the alphanumeric run just before / after `index`.
    let run_before = |index: usize| {
        chars[..index]
            .iter()
            .rev()
            .take_while(|ch| ch.is_alphanumeric())
            .count()
    };
    let run_after = |index: usize| {
        chars[index + 1..]
            .iter()
            .take_while(|ch| ch.is_alphanumeric())
            .count()
    };

    (0..chars.len()).any(|index| {
        let previous = index.checked_sub(1);
        let next = Some(index + 1);
        let after_next = Some(index + 2);
        match chars[index] {
            '_' => word(previous) && word(next),
            '/' => lower_or_digit(previous) && lower_or_digit(next),
            ':' => word(previous) && at(next) == Some(':') && word(after_next),
            '-' => at(next) == Some('-') && letter(after_next) && at(previous) != Some('-'),
            '.' => {
                word(previous) && letter(next) && (run_before(index) > 1 || run_after(index) > 1)
            }
            ch if ch.is_uppercase() => {
                let start = index - run_before(index);
                let end = index + 1 + run_after(index);
                let token: String = chars[start..end].iter().collect();
                !PROPER_NOUNS.contains(&token.as_str())
                    && (lower(previous) || (upper(previous) && lower(next) && lower(after_next)))
            }
            _ => false,
        }
    })
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// A mined query, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MinedQuery {
    commit: String,
    subject: String,
    query: String,
    expected_paths: Vec<String>,
    /// The keyword tag first, then `gold-drift` and the length tag when they
    /// apply, in that order.
    tags: Vec<&'static str>,
}

/// Why a commit did not become a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Skip {
    Merge,
    Revert,
    ShortSubject,
    Noise,
    NoSupportedFiles,
    TooManyFiles,
    NotAtHead,
    Duplicate,
}

impl Skip {
    const fn label(self) -> &'static str {
        match self {
            Self::Merge => "merge commit",
            Self::Revert => "revert",
            Self::ShortSubject => "subject under 4 words",
            Self::Noise => "formatting, lint or typo fix",
            Self::NoSupportedFiles => "touches no supported source file",
            Self::TooManyFiles => "touches more than 10 supported files",
            Self::NotAtHead => "no touched file survives at HEAD",
            Self::Duplicate => "same query text as a newer commit",
        }
    }
}

fn is_merge_subject(subject: &str) -> bool {
    let subject = subject.to_ascii_lowercase();
    [
        "merge branch",
        "merge pull request",
        "merge remote-tracking",
        "merge tag",
    ]
    .iter()
    .any(|prefix| subject.starts_with(prefix))
}

fn is_revert(commit: &Commit) -> bool {
    let first_word = commit
        .subject
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| !ch.is_alphanumeric())
        .to_ascii_lowercase();
    first_word == "revert"
        || commit
            .body
            .to_ascii_lowercase()
            .contains("this reverts commit")
}

/// Whether the subject says the commit only reformatted, linted or fixed a
/// typo: no retrievable intent, and the touched files are incidental.
fn is_noise_subject(subject: &str) -> bool {
    tokens(subject).any(|token| NOISE_WORDS.contains(&token.as_str()))
}

/// Turn one commit into a query, or say why it does not qualify.
///
/// `head_files` is the set of paths in the tree at `HEAD`. `later_renames` are
/// the file moves made by commits after this one, oldest first: a path this
/// commit touched is followed through them to the name it has at `HEAD`.
fn mine_commit(
    commit: &Commit,
    head_files: &BTreeSet<String>,
    later_renames: &[Rename],
) -> Result<MinedQuery, Skip> {
    if is_merge_subject(&commit.subject) {
        return Err(Skip::Merge);
    }
    if is_revert(commit) {
        return Err(Skip::Revert);
    }
    let subject = clean_subject(&commit.subject);
    if subject.split_whitespace().count() < MIN_SUBJECT_WORDS {
        return Err(Skip::ShortSubject);
    }
    if is_noise_subject(subject) {
        return Err(Skip::Noise);
    }

    let eligible: Vec<&Change> = commit
        .changes
        .iter()
        .filter(|change| is_eligible_path(&change.path))
        .collect();
    let touched: Vec<&str> = eligible
        .iter()
        .map(|change| change.path.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if touched.is_empty() {
        return Err(Skip::NoSupportedFiles);
    }
    if touched.len() > MAX_TOUCHED_FILES {
        return Err(Skip::TooManyFiles);
    }
    // The gold path of a change, if it has one. A path the commit deleted is
    // not a result even if a later commit recreated it: the file at HEAD is
    // not the one this commit touched. A path a later commit moved is looked
    // for under its new name.
    let gold_path = |change: &Change| -> Option<String> {
        (is_eligible_path(&change.path) && change.status != 'D')
            .then(|| chase_renames(&change.path, later_renames))
            .filter(|path| is_eligible_path(path) && head_files.contains(path))
    };
    let expected_paths: Vec<String> = commit
        .changes
        .iter()
        .filter_map(gold_path)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if expected_paths.is_empty() {
        return Err(Skip::NotAtHead);
    }

    // Every change that is not gold is drift away from the gold, including
    // the unsupported files (documentation, lockfiles) the file cap ignores.
    let (mut gold_churn, mut drift_churn) = (0, 0);
    for change in &commit.changes {
        let churn = change_churn(&commit.lines, change);
        if gold_path(change).is_some() {
            gold_churn += churn;
        } else {
            drift_churn += churn;
        }
    }

    let query = query_text(&commit.subject, &commit.body);
    let mut tags = vec![
        if names_a_touched_file(&query, &touched) || has_identifier_shape(&query) {
            TAG_KEYWORD
        } else {
            TAG_KEYWORD_AGNOSTIC
        },
    ];
    // Strictly more than half of the churn: the 50% bound was fixed in the
    // roadmap before any set was tagged, so an exact tie is not drift.
    if drift_churn > gold_churn {
        tags.push(TAG_GOLD_DRIFT);
    }
    // Words of the text sent to search, so an appended body paragraph counts
    // and a stripped `(#12)` does not.
    match query.split_whitespace().count() {
        words if words < SHORT_QUERY_MAX_WORDS => tags.push(TAG_SHORT_QUERY),
        words if words >= LONG_QUERY_MIN_WORDS => tags.push(TAG_LONG_QUERY),
        _ => {}
    }
    Ok(MinedQuery {
        commit: commit.hash.clone(),
        subject: subject.to_owned(),
        query,
        expected_paths,
        tags,
    })
}

#[derive(Debug, Default)]
struct Mined {
    queries: Vec<MinedQuery>,
    skipped: BTreeMap<Skip, usize>,
    /// Commits examined before `max` queries were found or history ended.
    scanned: usize,
}

/// Mine up to `max` queries from `commits` (newest first), dropping repeated
/// query text so a recurring subject ("Update dependencies in the lockfile")
/// does not weigh the set.
fn mine(commits: &[Commit], head_files: &BTreeSet<String>, max: usize) -> Mined {
    let mut mined = Mined::default();
    let mut seen = BTreeSet::new();
    // Moves are collected from the whole history, including commits that are
    // themselves skipped (a bulk move touches too many files to be a query):
    // a file renamed by any later commit must be chased.
    let (renames, offsets) = collect_renames(commits);
    for (index, commit) in commits.iter().enumerate() {
        if mined.queries.len() >= max {
            break;
        }
        mined.scanned += 1;
        match mine_commit(commit, head_files, &renames[offsets[index]..]) {
            Ok(query) if seen.insert(query.query.to_lowercase()) => mined.queries.push(query),
            Ok(_) => *mined.skipped.entry(Skip::Duplicate).or_default() += 1,
            Err(reason) => *mined.skipped.entry(reason).or_default() += 1,
        }
    }
    mined
}

// ---------------------------------------------------------------------------
// TOML output
// ---------------------------------------------------------------------------

/// A TOML basic string.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn toml_array(values: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let items: Vec<String> = values
        .into_iter()
        .map(|value| toml_string(value.as_ref()))
        .collect();
    format!("[{}]", items.join(", "))
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(7)]
}

/// Render the evaluation definition. `head` is the commit the set was mined
/// at and the tree evaluation is expected to run against.
fn render_toml(key: &str, head: &str, max: usize, queries: &[MinedQuery]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Git-mined retrieval queries for workspace {}.\n\
         #\n\
         # GENERATED by `cargo xtask mine-queries --repo <repo> --key {key} --out <file> --max {max}`\n\
         # at HEAD {head}. Do not edit it by hand and do not use it to tune ranking:\n\
         # it is a measurement, and regenerating it is the only way to change it.\n\
         #\n\
         # Each query is a commit message (the subject, plus the first body paragraph\n\
         # when that adds words). Merges, reverts, subjects under {MIN_SUBJECT_WORDS} words and\n\
         # formatting, lint or typo commits are skipped. `expected_paths` are the\n\
         # supported source files the commit touched that still exist at HEAD, followed\n\
         # through later renames; evaluation searches the checked-out tree, not the\n\
         # commit's parent, so a file may have changed since. That makes these labels\n\
         # an approximation, not ground truth.\n\
         #\n\
         # Tags: `keyword` means the query text names a touched file (its stem words,\n\
         # in any spelling) or contains a code-identifier-shaped token (snake_case,\n\
         # `::`, `--flag`, `/` between lowercase letters or digits, `.` between word\n\
         # characters, camelCase or PascalCase; common proper nouns like JavaScript\n\
         # do not count); `keyword-agnostic` means it does neither. `gold-drift` means\n\
         # more than half of the commit's changed lines (added plus deleted; a pure\n\
         # move counts on both sides) are in files that are not gold: unsupported or\n\
         # excluded files, deleted files, and files gone from HEAD even after later\n\
         # renames, so a miss may not be a retrieval failure. `short-query` means the\n\
         # query text has fewer than {SHORT_QUERY_MAX_WORDS} words and `long-query` that it has\n\
         # {LONG_QUERY_MIN_WORDS} or more (query-text words, so an appended body paragraph counts).\n\
         # The report breaks the metrics out per tag. `top_k = {MINED_TOP_K}` keeps the\n\
         # @10 metrics from being window-bound.\n\
         # The checked-in hand-written set stays evaluations/core.toml.\n",
        toml_string(key)
    );
    let _ = writeln!(
        out,
        "[[workspaces]]\nkey = {}\nrequired = true",
        toml_string(key)
    );
    for query in queries {
        let _ = writeln!(
            out,
            "\n# {} {}\n[[queries]]\nworkspace = {}\nquery = {}\ntop_k = {MINED_TOP_K}\nexpected_paths = {}\ntags = {}",
            short_hash(&query.commit),
            query.subject.replace(['\r', '\n'], " "),
            toml_string(key),
            toml_string(&query.query),
            toml_array(&query.expected_paths),
            toml_array(&query.tags),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to run git; is it on PATH?")?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn write_definition(out: &Path, rendered: &str) -> Result<()> {
    let definition: EvaluationFile =
        toml::from_str(rendered).context("failed to parse mined evaluation definition")?;
    validate_definition(&definition).context("invalid mined evaluation definition")?;

    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(out, rendered).with_context(|| format!("failed to write {}", out.display()))
}

pub fn run(repo: PathBuf, key: String, out: PathBuf, max: usize) -> Result<()> {
    ensure!(!key.trim().is_empty(), "--key must not be empty");
    ensure!(max > 0, "--max must be at least 1");

    // Paths in the log and in the tree are relative to the repository root,
    // which is also what an evaluation workspace root must be.
    ensure!(
        git(&repo, &["rev-parse", "--show-prefix"])?
            .trim()
            .is_empty(),
        "--repo {} is inside a repository; pass the repository root",
        repo.display()
    );
    let head = git(&repo, &["rev-parse", "HEAD"])?.trim().to_owned();
    let head_files: BTreeSet<String> = git(
        &repo,
        &["ls-tree", "-r", "--full-tree", "--name-only", "-z", "HEAD"],
    )?
    .split('\0')
    .filter(|path| !path.is_empty())
    .map(str::to_owned)
    .collect();
    let log = git(
        &repo,
        &[
            "-c",
            "core.quotepath=off",
            "log",
            "--no-merges",
            "--name-status",
            "-M",
            "--no-color",
            "--no-show-signature",
            "--encoding=UTF-8",
            LOG_FORMAT,
            "HEAD",
        ],
    )?;
    // Line counts need a second call: git drops `--numstat` output when
    // `--name-status` is present. `--no-renames` keeps the paths plain.
    let numstat = git(
        &repo,
        &[
            "-c",
            "core.quotepath=off",
            "log",
            "--no-merges",
            "--numstat",
            "--no-renames",
            "--no-color",
            "--no-show-signature",
            "--encoding=UTF-8",
            NUMSTAT_FORMAT,
            "HEAD",
        ],
    )?;
    let mut lines = parse_numstat(&numstat);
    let mut commits = parse_log(&log);
    for commit in &mut commits {
        // The numstat log prints a record for every commit, even one with no
        // files; a missing record means the two logs disagree (e.g. HEAD moved
        // between them), and that commit can never be flagged `gold-drift`.
        match lines.remove(&commit.hash) {
            Some(commit_lines) => commit.lines = commit_lines,
            None => eprintln!(
                "warning: no line counts for commit {}; it cannot be tagged {TAG_GOLD_DRIFT}",
                commit.hash
            ),
        }
    }
    let mined = mine(&commits, &head_files, max);
    ensure!(
        !mined.queries.is_empty(),
        "no commit in {} qualified as a query ({} scanned)",
        repo.display(),
        mined.scanned
    );

    write_definition(&out, &render_toml(&key, &head, max, &mined.queries))?;

    let count = |tag: &str| {
        mined
            .queries
            .iter()
            .filter(|query| query.tags.contains(&tag))
            .count()
    };
    println!(
        "Wrote {} queries to {} (HEAD {}): {} {TAG_KEYWORD}, {} {TAG_KEYWORD_AGNOSTIC}; \
         {} {TAG_GOLD_DRIFT}, {} {TAG_SHORT_QUERY}, {} {TAG_LONG_QUERY}.",
        mined.queries.len(),
        out.display(),
        short_hash(&head),
        count(TAG_KEYWORD),
        count(TAG_KEYWORD_AGNOSTIC),
        count(TAG_GOLD_DRIFT),
        count(TAG_SHORT_QUERY),
        count(TAG_LONG_QUERY),
    );
    println!(
        "Scanned {} of {} non-merge commits; skipped:",
        mined.scanned,
        commits.len()
    );
    for (reason, skipped) in &mined.skipped {
        println!("  {skipped:>4}  {}", reason.label());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_core::evaluation::{EvaluationFile, validate_definition};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "xtask-mine-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn change(status: char, path: &str) -> Change {
        Change {
            status,
            path: path.to_owned(),
            renamed_from: None,
        }
    }

    fn moved(status: char, from: &str, to: &str) -> Change {
        Change {
            renamed_from: Some(from.to_owned()),
            ..change(status, to)
        }
    }

    fn rename(from: &str, to: &str) -> Rename {
        Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    /// `mine_commit` as if no later commit moved any file.
    fn mine_commit(commit: &Commit, head_files: &BTreeSet<String>) -> Result<MinedQuery, Skip> {
        super::mine_commit(commit, head_files, &[])
    }

    fn commit(subject: &str, body: &str, changes: &[(char, &str)]) -> Commit {
        Commit {
            hash: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            subject: subject.to_owned(),
            body: body.to_owned(),
            changes: changes
                .iter()
                .map(|(status, path)| change(*status, path))
                .collect(),
            lines: BTreeMap::new(),
        }
    }

    /// `commit` with the `--numstat` line counts of the given paths.
    fn with_lines(commit: Commit, lines: &[(&str, u64)]) -> Commit {
        Commit {
            lines: lines
                .iter()
                .map(|(path, count)| ((*path).to_owned(), *count))
                .collect(),
            ..commit
        }
    }

    fn head(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    // -- git log parsing ----------------------------------------------------

    #[test]
    fn parses_name_status_log_output() {
        let raw = format!(
            "{rs}aaaa{us}Add the metric helper{us}First paragraph.\n\nSecond one.{us}\n\n\
             M\tsrc/metric.rs\nA\tsrc/new.rs\nD\tsrc/old.rs\nR087\tsrc/before.rs\tsrc/after.rs\n\
             C075\tsrc/origin.rs\tsrc/copy.rs\nR100\t\"src/we\\\"ird.rs\"\tsrc/fixed.rs\n\
             M\t\"src/we\\\"ird.rs\"\n\
             {rs}bbbb{us}Subject only{us}{us}\n\nM\tREADME.md\r\n\
             {rs}cccc{us}No files{us}{us}\n",
            rs = RECORD_SEPARATOR,
            us = FIELD_SEPARATOR,
        );
        let commits = parse_log(&raw);
        assert_eq!(commits.len(), 3);

        assert_eq!(commits[0].hash, "aaaa");
        assert_eq!(commits[0].subject, "Add the metric helper");
        assert_eq!(commits[0].body, "First paragraph.\n\nSecond one.");
        assert_eq!(
            commits[0].changes,
            vec![
                change('M', "src/metric.rs"),
                change('A', "src/new.rs"),
                change('D', "src/old.rs"),
                // A rename lists the file at its new path and remembers the
                // old one; a copy does too, but the original stays where it is.
                moved('R', "src/before.rs", "src/after.rs"),
                moved('C', "src/origin.rs", "src/copy.rs"),
                // A quoted old path cannot be matched, so only the new path
                // is kept; a quoted path alone is dropped.
                change('R', "src/fixed.rs"),
            ]
        );

        assert_eq!(commits[1].body, "");
        assert_eq!(commits[1].changes, vec![change('M', "README.md")]);
        assert!(commits[2].changes.is_empty());
    }

    #[test]
    fn parses_numstat_log_output() {
        let raw = format!(
            "{rs}aaaa{us}\n\n12\t3\tsrc/metric.rs\n-\t-\tassets/logo.png\n\
             4\t5\t\"src/we\\\"ird.rs\"\n0\t323\tsrc/lexical.rs\r\n\
             {rs}bbbb{us}\n\n323\t0\tlci-core/src/lexical.rs\n0\t323\tsrc/lexical.rs\r\n\
             {rs}cccc{us}\n",
            rs = RECORD_SEPARATOR,
            us = FIELD_SEPARATOR,
        );
        let numstat = parse_numstat(&raw);
        assert_eq!(numstat.len(), 3);

        // Lines added plus deleted; a binary file is present but counts 0; a
        // quoted path is dropped; a CRLF line ending is tolerated.
        assert_eq!(
            numstat["aaaa"],
            BTreeMap::from([
                ("assets/logo.png".to_owned(), 0),
                ("src/lexical.rs".to_owned(), 323),
                ("src/metric.rs".to_owned(), 15),
            ])
        );
        // `--no-renames` reports a move as an add and a delete: both paths.
        assert_eq!(
            numstat["bbbb"],
            BTreeMap::from([
                ("lci-core/src/lexical.rs".to_owned(), 323),
                ("src/lexical.rs".to_owned(), 323),
            ])
        );
        assert!(numstat["cccc"].is_empty());
    }

    #[test]
    fn a_rename_joins_the_lines_of_both_its_paths() {
        let lines = BTreeMap::from([
            ("src/old.rs".to_owned(), 7),
            ("lci-core/src/new.rs".to_owned(), 40),
            ("src/template.rs".to_owned(), 100),
            ("src/copy.rs".to_owned(), 9),
            ("src/edited.rs".to_owned(), 3),
        ]);
        // A move is a delete of the old path plus an add of the new one.
        assert_eq!(
            change_churn(&lines, &moved('R', "src/old.rs", "lci-core/src/new.rs")),
            47
        );
        // A copy leaves its source in place: only the copy's own lines count.
        assert_eq!(
            change_churn(&lines, &moved('C', "src/template.rs", "src/copy.rs")),
            9
        );
        assert_eq!(change_churn(&lines, &change('M', "src/edited.rs")), 3);
        // A path with no line data (git printed it quoted, or the commit has
        // none) contributes nothing.
        assert_eq!(change_churn(&lines, &change('M', "src/unknown.rs")), 0);
        assert_eq!(change_churn(&BTreeMap::new(), &change('M', "src/a.rs")), 0);
    }

    // -- filters ------------------------------------------------------------

    #[test]
    fn a_plain_commit_becomes_a_query_with_files_that_survive_at_head() {
        let tree = head(&["src/metrics.rs", "src/app/search.rs"]);
        let mined = mine_commit(
            &commit(
                "Rank candidates before reranking them",
                "",
                &[
                    ('M', "src/metrics.rs"),
                    ('M', "src/app/search.rs"),
                    ('D', "src/removed.rs"),
                    ('M', "README.md"),
                ],
            ),
            &tree,
        )
        .unwrap();
        assert_eq!(mined.query, "Rank candidates before reranking them");
        assert_eq!(
            mined.expected_paths,
            ["src/app/search.rs", "src/metrics.rs"],
            "sorted, and without the deleted or non-source files"
        );
        assert_eq!(mined.commit, "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn skips_merges_and_reverts() {
        let tree = head(&["src/a.rs"]);
        let touched = [('M', "src/a.rs")];
        for subject in [
            "Merge branch 'fix/thing' into main",
            "Merge pull request #12 from owner/branch",
        ] {
            assert_eq!(
                mine_commit(&commit(subject, "", &touched), &tree),
                Err(Skip::Merge),
                "{subject}"
            );
        }
        for subject in [
            "Revert \"Add a thing that broke the build\"",
            "revert: add a thing that broke the build",
        ] {
            assert_eq!(
                mine_commit(&commit(subject, "", &touched), &tree),
                Err(Skip::Revert),
                "{subject}"
            );
        }
        assert_eq!(
            mine_commit(
                &commit(
                    "Undo the thing that broke the build",
                    "This reverts commit 0123abc.",
                    &touched
                ),
                &tree
            ),
            Err(Skip::Revert)
        );
        // Only a leading "revert" word marks a revert.
        assert!(
            mine_commit(
                &commit("Handle reverted files in the watcher", "", &touched),
                &tree
            )
            .is_ok()
        );
    }

    #[test]
    fn skips_subjects_under_four_words() {
        let tree = head(&["src/a.rs"]);
        let touched = [('M', "src/a.rs")];
        for subject in ["wip", "Fix bug", "Fix the bug", "  Fix   the   bug  "] {
            assert_eq!(
                mine_commit(&commit(subject, "", &touched), &tree),
                Err(Skip::ShortSubject),
                "{subject:?}"
            );
        }
        assert!(mine_commit(&commit("Fix the parser bug", "", &touched), &tree).is_ok());
        // A pull-request reference does not count toward the words.
        assert_eq!(
            mine_commit(&commit("Fix the bug (#12)", "", &touched), &tree),
            Err(Skip::ShortSubject)
        );
    }

    #[test]
    fn skips_commits_without_supported_source_files() {
        let tree = head(&["README.md", "src/a.rs"]);
        for changes in [
            vec![],
            vec![('M', "README.md"), ('M', "docs/guide.md")],
            // Lockfiles, vendored, generated and build-output paths do not
            // count even though their extension is supported.
            vec![('M', "Cargo.lock"), ('M', "package-lock.json")],
            vec![
                ('M', "vendor/lib/util.go"),
                ('M', "node_modules/x/index.js"),
            ],
            vec![
                ('M', "web/app.min.js"),
                ('M', "web/types/api.d.ts"),
                ('M', "proto/api.pb.go"),
                ('M', "src/schema.generated.rs"),
                ('M', "target/debug/build/x.rs"),
            ],
            // Glob metacharacters cannot be written as an exact pattern.
            vec![('M', "src/[id].tsx")],
        ] {
            assert_eq!(
                mine_commit(
                    &commit("Update the project documentation", "", &changes),
                    &tree
                ),
                Err(Skip::NoSupportedFiles),
                "{changes:?}"
            );
        }
    }

    #[test]
    fn caps_the_touched_file_count_at_ten() {
        let paths: Vec<String> = (0..11).map(|index| format!("src/m{index}.rs")).collect();
        let tree: BTreeSet<String> = paths.iter().cloned().collect();
        let changes = |count: usize| {
            paths[..count]
                .iter()
                .map(|path| ('M', path.as_str()))
                .collect::<Vec<_>>()
        };
        let subject = "Rename every single module here";
        assert!(mine_commit(&commit(subject, "", &changes(10)), &tree).is_ok());
        assert_eq!(
            mine_commit(&commit(subject, "", &changes(11)), &tree),
            Err(Skip::TooManyFiles)
        );
        // Unsupported files do not count toward the cap.
        let mut mixed = changes(10);
        mixed.extend([('M', "a.md"), ('M', "b.toml")]);
        assert!(mine_commit(&commit(subject, "", &mixed), &tree).is_ok());
    }

    #[test]
    fn expected_paths_must_still_exist_at_head() {
        let tree = head(&["src/new_name.rs", "src/recreated.rs"]);
        // Deleted outright: nothing to find at HEAD.
        assert_eq!(
            mine_commit(
                &commit("Remove the legacy runner", "", &[('D', "src/runner.rs")]),
                &tree
            ),
            Err(Skip::NotAtHead)
        );
        // Deleted here and recreated by a later commit: the file at HEAD is
        // not the one this commit touched, so it is not an answer.
        assert_eq!(
            mine_commit(
                &commit("Remove the legacy runner", "", &[('D', "src/recreated.rs")]),
                &tree
            ),
            Err(Skip::NotAtHead)
        );
        // Renamed: the new path survives, the old one is not expected.
        let mined = mine_commit(
            &commit(
                "Rename the runner module",
                "",
                &[('R', "src/new_name.rs"), ('D', "src/old_name.rs")],
            ),
            &tree,
        )
        .unwrap();
        assert_eq!(mined.expected_paths, ["src/new_name.rs"]);
    }

    // -- renames --------------------------------------------------------------

    /// A commit that moved files, built from `(from, to)` pairs.
    fn move_commit(subject: &str, moves: &[(&str, &str)]) -> Commit {
        Commit {
            changes: moves
                .iter()
                .map(|(from, to)| moved('R', from, to))
                .collect(),
            ..commit(subject, "", &[])
        }
    }

    #[test]
    fn a_path_renamed_by_a_later_commit_is_chased() {
        let moves = [rename("src/lsp.rs", "lci-core/src/lsp.rs")];
        assert_eq!(chase_renames("src/lsp.rs", &moves), "lci-core/src/lsp.rs");
        // Other paths, including a sibling that shares a prefix, stay put.
        assert_eq!(chase_renames("src/lsp_old.rs", &moves), "src/lsp_old.rs");
        assert_eq!(chase_renames("src/lsp.rs", &[]), "src/lsp.rs");

        let tree = head(&["lci-core/src/lsp.rs"]);
        let mined = super::mine_commit(
            &commit(
                "Teach the adapter about initialization options",
                "",
                &[('M', "src/lsp.rs")],
            ),
            &tree,
            &moves,
        )
        .unwrap();
        assert_eq!(mined.expected_paths, ["lci-core/src/lsp.rs"]);
        // Without the move the file is simply not at HEAD.
        assert_eq!(
            mine_commit(
                &commit(
                    "Teach the adapter about initialization options",
                    "",
                    &[('M', "src/lsp.rs")],
                ),
                &tree
            ),
            Err(Skip::NotAtHead)
        );
    }

    #[test]
    fn rename_chains_resolve_in_order_and_cycles_terminate() {
        let chain = [rename("a.rs", "b.rs"), rename("b.rs", "c.rs")];
        assert_eq!(chase_renames("a.rs", &chain), "c.rs");
        assert_eq!(chase_renames("b.rs", &chain), "c.rs");
        assert_eq!(chase_renames("c.rs", &chain), "c.rs");

        // `b -> c` happened before `a -> b`: the file that was `a` is now `b`,
        // and the old `b` (now `c`) is a different file.
        let reversed = [rename("b.rs", "c.rs"), rename("a.rs", "b.rs")];
        assert_eq!(chase_renames("a.rs", &reversed), "b.rs");

        let cycle = [rename("a.rs", "b.rs"), rename("b.rs", "a.rs")];
        assert_eq!(chase_renames("a.rs", &cycle), "a.rs");
        let rotation = [
            rename("a.rs", "b.rs"),
            rename("b.rs", "c.rs"),
            rename("c.rs", "a.rs"),
        ];
        assert_eq!(chase_renames("b.rs", &rotation), "a.rs");
        assert_eq!(chase_renames("c.rs", &rotation), "a.rs");
    }

    #[test]
    fn renames_are_collected_oldest_first_and_exclude_copies() {
        // Newest first, like `git log`.
        let mut copy_and_move = move_commit("Split the template", &[("a.rs", "b.rs")]);
        copy_and_move.changes.push(moved('C', "x.rs", "y.rs"));
        let commits = [
            move_commit("Move b again", &[("b.rs", "c.rs")]),
            copy_and_move,
            commit("Edit a", "", &[('M', "a.rs")]),
        ];
        let (renames, offsets) = collect_renames(&commits);
        assert_eq!(renames, [rename("a.rs", "b.rs"), rename("b.rs", "c.rs")]);
        // Moves made after commit i are `renames[offsets[i]..]`.
        assert_eq!(&renames[offsets[2]..], &renames[..]);
        assert_eq!(&renames[offsets[1]..], [rename("b.rs", "c.rs")]);
        assert!(renames[offsets[0]..].is_empty());
    }

    #[test]
    fn mining_chases_only_renames_that_came_after_the_commit() {
        // Newest first. `src/lsp.rs` is recreated after the move, so the
        // newest commit's file is the one at that path, not the moved one.
        let tree = head(&["src/lsp.rs", "lci-core/src/lsp.rs", "lci-core/src/chunk.rs"]);
        let commits = [
            commit(
                "Recreate the lsp compatibility shim",
                "",
                &[('A', "src/lsp.rs")],
            ),
            // Short subject: skipped as a query, but its moves still count.
            move_commit(
                "Reorganize crates",
                &[
                    ("src/lsp.rs", "lci-core/src/lsp.rs"),
                    ("src/chunk.rs", "lci-core/src/chunk.rs"),
                ],
            ),
            commit(
                "Teach the adapter about initialization options",
                "",
                &[('M', "src/lsp.rs"), ('M', "src/chunk.rs")],
            ),
        ];
        let mined = mine(&commits, &tree, 10);
        let expected: Vec<&[String]> = mined
            .queries
            .iter()
            .map(|query| query.expected_paths.as_slice())
            .collect();
        assert_eq!(
            expected,
            [
                &["src/lsp.rs".to_owned()][..],
                &[
                    "lci-core/src/chunk.rs".to_owned(),
                    "lci-core/src/lsp.rs".to_owned()
                ][..],
            ]
        );
        assert_eq!(mined.skipped[&Skip::ShortSubject], 1);
    }

    #[test]
    fn a_copy_does_not_move_the_original() {
        let tree = head(&["src/template.rs", "src/copy.rs"]);
        let commits = [
            Commit {
                changes: vec![moved('C', "src/template.rs", "src/copy.rs")],
                ..commit("Copy the template into a second module", "", &[])
            },
            commit(
                "Tune template defaults for faster startup",
                "",
                &[('M', "src/template.rs")],
            ),
        ];
        let mined = mine(&commits, &tree, 10);
        assert_eq!(mined.queries.len(), 2);
        assert_eq!(mined.queries[0].expected_paths, ["src/copy.rs"]);
        assert_eq!(mined.queries[1].expected_paths, ["src/template.rs"]);
    }

    #[test]
    fn skips_formatting_lint_and_typo_commits() {
        let tree = head(&["src/a.rs"]);
        let touched = [('M', "src/a.rs")];
        for subject in [
            "Fix Python mock reranker lint",
            "Apply clippy suggestions across the crate",
            "Run cargo fmt on the whole tree",
            "Reformat with rustfmt after the merge",
            "Fix formatting in the search module",
            "Fix typo in the retriever comment",
            "Fix typos in the retriever comments",
            "Strip trailing whitespace from search module",
            "FIX LINT ERRORS IN SEARCH MODULE",
        ] {
            assert_eq!(
                mine_commit(&commit(subject, "", &touched), &tree),
                Err(Skip::Noise),
                "{subject}"
            );
        }
        // Whole words only, and only in the subject.
        for subject in [
            "Add a formatter option to the report output",
            "Handle typology names in the search module",
            "Accept flinty and whitespaced names in the lexer",
        ] {
            assert!(
                mine_commit(&commit(subject, "", &touched), &tree).is_ok(),
                "{subject}"
            );
        }
        assert!(
            mine_commit(
                &commit(
                    "Speed up the search module",
                    "Also fixes a typo and some lint.",
                    &touched
                ),
                &tree
            )
            .is_ok()
        );
    }

    #[test]
    fn mining_drops_repeated_query_text_and_honors_max() {
        let tree = head(&["src/a.rs", "src/b.rs", "src/c.rs"]);
        let make = |subject: &str, path: &str| commit(subject, "", &[('M', path)]);
        let commits = [
            make("Tune the retrieval window size", "src/a.rs"),
            make("Tune the Retrieval window size", "src/b.rs"),
            make("Fix it", "src/b.rs"),
            make("Document the evaluation workflow clearly", "src/b.rs"),
            make("Handle empty indexes without crashing", "src/c.rs"),
        ];
        let mined = mine(&commits, &tree, 10);
        let queries: Vec<&str> = mined.queries.iter().map(|q| q.query.as_str()).collect();
        assert_eq!(
            queries,
            [
                "Tune the retrieval window size",
                "Document the evaluation workflow clearly",
                "Handle empty indexes without crashing"
            ]
        );
        assert_eq!(mined.skipped[&Skip::Duplicate], 1);
        assert_eq!(mined.skipped[&Skip::ShortSubject], 1);
        assert_eq!(mined.scanned, 5);

        let limited = mine(&commits, &tree, 2);
        assert_eq!(limited.queries.len(), 2);
        assert_eq!(limited.queries[0].query, "Tune the retrieval window size");
        assert_eq!(limited.scanned, 4, "stops as soon as max is reached");
    }

    // -- query text -----------------------------------------------------------

    #[test]
    fn query_text_appends_a_body_paragraph_only_when_it_adds_words() {
        let subject = "Cap rerank documents";
        // Adds words: appended after a sentence break.
        assert_eq!(
            query_text(
                subject,
                "The reranker accepted oversized chunks and one request timed out.\n\nMore detail."
            ),
            "Cap rerank documents. The reranker accepted oversized chunks and one request timed out."
        );
        // A hard-wrapped paragraph is joined; subject punctuation is respected.
        assert_eq!(
            query_text(
                "Cap rerank documents:",
                "send only a head of\neach chunk to the reranker"
            ),
            "Cap rerank documents: send only a head of each chunk to the reranker"
        );
        // Repeats the subject: nothing new to add.
        assert_eq!(
            query_text(subject, "Cap rerank documents, rerank documents."),
            subject
        );
        // Opens with a list, a trailer or a fence: not prose, so not used.
        for body in [
            "- first change to the reranker\n- second change to the reranker",
            "Co-Authored-By: Someone Else <someone@example.com>",
            "```\nsome output the reranker printed\n```",
            "1. first thing the reranker does",
            "",
        ] {
            assert_eq!(query_text(subject, body), subject, "{body:?}");
        }
        // Prose stops at the first list line of its paragraph.
        assert_eq!(
            query_text(subject, "Changes in the reranker path:\n- one\n- two"),
            "Cap rerank documents. Changes in the reranker path:"
        );
        // Pull-request references are stripped from the subject.
        assert_eq!(query_text("Cap rerank documents (#42)", ""), subject);
    }

    #[test]
    fn query_text_is_capped_at_a_word_boundary() {
        let long_body = "alpha beta gamma delta ".repeat(40);
        let text = query_text("Cap rerank documents", &long_body);
        assert!(text.chars().count() <= MAX_QUERY_CHARS, "{}", text.len());
        assert!(text.chars().count() > MAX_QUERY_CHARS - 10, "cut too early");
        assert!(text.starts_with("Cap rerank documents. alpha beta gamma delta alpha"));
        assert!(
            text.split(' ')
                .skip(3)
                .all(|word| ["alpha", "beta", "gamma", "delta"].contains(&word)),
            "cut mid-word: {text:?}"
        );

        // A single unbroken token is hard-cut; multibyte text stays valid.
        let long_subject = "é".repeat(400);
        assert_eq!(
            query_text(&long_subject, "").chars().count(),
            MAX_QUERY_CHARS
        );
        // Short text is untouched.
        assert_eq!(truncate_words("a b c", 300), "a b c");
    }

    // -- tagging --------------------------------------------------------------

    #[test]
    fn splits_identifiers_and_prose_into_words() {
        for text in [
            "source_role",
            "SourceRole",
            "sourceRole",
            "source-role",
            "source role",
            "SOURCE ROLE",
        ] {
            assert_eq!(split_words(text), ["source", "role"], "{text}");
        }
        assert_eq!(split_words("HTTPServer"), ["http", "server"]);
        assert_eq!(split_words("parse2Json"), ["parse2", "json"]);
        assert!(split_words("-- ::").is_empty());
    }

    #[test]
    fn tags_queries_that_name_a_touched_file() {
        let touched = |paths: &[&str]| -> Vec<String> {
            paths.iter().map(|path| (*path).to_owned()).collect()
        };
        let files = touched(&["lci-core/src/filter/source_role.rs", "src/app/search.rs"]);
        for query in [
            "Fix search ranking when the index is stale",
            "Classify source role for Go test files",
            "Handle SourceRole::Test in the filter",
            "Edit source_role.rs to add Rust tests",
        ] {
            assert!(names_a_touched_file(query, &files), "{query}");
        }
        for query in [
            // The words are present but not as the file's name.
            "Classify the role of each source file",
            "Make rankings of the index fresher",
            "Handle empty indexes without crashing",
        ] {
            assert!(!names_a_touched_file(query, &files), "{query}");
        }
    }

    #[test]
    fn generic_file_stems_never_make_a_query_keyword() {
        let files = vec![
            "src/mod.rs".to_owned(),
            "src/main.rs".to_owned(),
            "lci-core/src/lib.rs".to_owned(),
            "src/index.ts".to_owned(),
            "src/types.ts".to_owned(),
            "src/ab.rs".to_owned(),
        ];
        assert!(!names_a_touched_file(
            "Move the main entry point and the index types into mod ab",
            &files
        ));
        // A distinctive stem of the same length class still counts.
        assert!(names_a_touched_file(
            "Wire the lsp client",
            &["src/lsp.rs".to_owned()]
        ));
    }

    #[test]
    fn code_identifier_shapes_are_detected() {
        for (shape, text) in [
            (
                "snake_case",
                "List indexed files through list_indexed_files",
            ),
            (
                "snake_case",
                "Cap rerank_document_max_bytes for slow models",
            ),
            ("dunder", "Stop importing __init__ twice"),
            ("::", "Serialize App::index_lock across processes"),
            ("--flag", "Add the --max flag to the miner"),
            ("--flag", "Honor `--key` when writing the file"),
            ("/", "Read src/lsp before starting the adapter"),
            ("/", "Move the fixtures under src/app today"),
            ("/", "Edit lci-core/src/x.rs for the new adapter"),
            ("/", "Cap scores at 1/10 of the maximum"),
            (".", "Cache lookups in index.rs for speed"),
            (".", "Run the Node.js bridge in a child process"),
            ("PascalCase", "Store ResultEvidence beside the hit"),
            ("PascalCase", "Rename GitHubClient::new for clarity"),
            ("camelCase", "Cache the parsed tree in chunkCache"),
            ("camelCase", "Rename the iterator to nextItem here"),
            ("acronym+word", "Restart the HTTPServer after config reload"),
        ] {
            assert!(has_identifier_shape(text), "{shape}: {text}");
        }
        for text in [
            "Make results appear faster for large workspaces",
            // A capitalised first letter, acronyms and plural acronyms.
            "Add a capitalised First Word to the Rust LSP and MCP docs",
            "Update the LSPs and APIs and PRs and IDs",
            // Prose alternatives with a slash.
            "Support Go/Java projects in the indexer",
            "Wait for LSP/embedding readiness before searching",
            // Proper nouns with an inner capital.
            "Document the JavaScript runtime",
            "Link the GitHub issue from the commit",
            "Port the TypeScript, GitLab, PowerShell, LanceDB, OpenAI, nDCG, macOS, iOS              and WebAssembly docs",
            // Abbreviations, decimals and version numbers.
            "Fix the parser, e.g. for macros, i.e. all of them",
            "Bump to version 1.5 and wait for 3.x and v1.2.3 releases",
            // Sentence and dash punctuation, hyphenated words, lone marks.
            "Fix this. Then fix that.",
            "Handle empty indexes -- without crashing",
            "Handle the case --- carefully",
            "Use well-known cross-process locks",
            "Use _ as a placeholder, not __ or _suffix",
            "Prefix_ and the rest",
            "Compare a: b with c : d",
        ] {
            assert!(!has_identifier_shape(text), "{text}");
        }
    }

    #[test]
    fn identifier_shaped_text_makes_a_query_keyword_even_without_a_file_name() {
        let tree = head(&["src/app/search.rs"]);
        let tag = |subject: &str, body: &str| {
            mine_commit(&commit(subject, body, &[('M', "src/app/search.rs")]), &tree)
                .unwrap()
                .tags[0]
        };
        // None of these name search.rs; the identifier alone is lexical.
        for subject in [
            "Expose list_indexed_files to the agent",
            "Rename App::index_lock for clarity",
            "Add a --top-k flag to the command",
            "Document src/lsp behavior clearly",
            "Cache results for index.rs instead",
            "Return ResultEvidence from the engine",
            "Guard against nullCoalesce misuse here",
        ] {
            assert_eq!(tag(subject, ""), TAG_KEYWORD, "{subject}");
        }
        // The text sent to search includes the body paragraph.
        assert_eq!(
            tag(
                "Make results appear faster for large workspaces",
                "Most of the time went to the index_lock around every result."
            ),
            TAG_KEYWORD
        );
        // Plain English stays keyword-agnostic.
        assert_eq!(
            tag("Make results appear faster for large workspaces", ""),
            TAG_KEYWORD_AGNOSTIC
        );
    }

    #[test]
    fn mined_queries_carry_the_keyword_tag() {
        let tree = head(&["src/app/search.rs", "src/metrics.rs"]);
        let keyword = mine_commit(
            &commit(
                "Speed up search for large workspaces",
                "",
                &[('M', "src/app/search.rs")],
            ),
            &tree,
        )
        .unwrap();
        assert_eq!(keyword.tags[0], "keyword");

        let agnostic = mine_commit(
            &commit(
                "Make results appear faster for large workspaces",
                "",
                &[('M', "src/app/search.rs")],
            ),
            &tree,
        )
        .unwrap();
        assert_eq!(agnostic.tags[0], "keyword-agnostic");

        // The tag looks at the text sent to search, so a file named only in the
        // appended body paragraph still makes the query a keyword query.
        let via_body = mine_commit(
            &commit(
                "Make results appear faster for large workspaces",
                "Most of the time went to metrics aggregation over every result.",
                &[('M', "src/metrics.rs")],
            ),
            &tree,
        )
        .unwrap();
        assert_eq!(via_body.tags[0], "keyword");

        // A deleted touched file still counts as named.
        let deleted = mine_commit(
            &commit(
                "Remove the unused retriever module again",
                "",
                &[('D', "src/retriever.rs"), ('M', "src/metrics.rs")],
            ),
            &tree,
        )
        .unwrap();
        assert_eq!(deleted.tags[0], "keyword");
        assert_eq!(deleted.expected_paths, ["src/metrics.rs"]);
    }

    /// Nine words, no file name or identifier shape: keyword-agnostic and
    /// neither short nor long, so only the tag under test shows up.
    const MID_SUBJECT: &str = "Make results appear faster for very large indexed workspaces";

    /// The tags of a commit that changes `changes` (status, path, lines)
    /// while `tree` is the tree at HEAD.
    fn tags_of(changes: &[(char, &str, u64)], tree: &[&str]) -> Vec<&'static str> {
        let paths: Vec<(char, &str)> = changes
            .iter()
            .map(|&(status, path, _)| (status, path))
            .collect();
        let lines: Vec<(&str, u64)> = changes.iter().map(|&(_, path, n)| (path, n)).collect();
        let commit = with_lines(commit(MID_SUBJECT, "", &paths), &lines);
        mine_commit(&commit, &head(tree)).unwrap().tags
    }

    #[test]
    fn flags_gold_drift_when_most_churn_is_in_dropped_or_unsupported_files() {
        let tree = ["src/a.rs"];
        let flagged = vec![TAG_KEYWORD_AGNOSTIC, TAG_GOLD_DRIFT];
        let plain = vec![TAG_KEYWORD_AGNOSTIC];

        // Unsupported files: documentation and a lockfile outweigh the gold.
        assert_eq!(
            tags_of(&[('M', "src/a.rs", 10), ('M', "docs/guide.md", 30)], &tree),
            flagged
        );
        assert_eq!(
            tags_of(&[('M', "src/a.rs", 10), ('M', "Cargo.lock", 30)], &tree),
            flagged
        );
        // Excluded paths are unsupported too, even with a supported extension.
        assert_eq!(
            tags_of(&[('M', "src/a.rs", 10), ('M', "vendor/x/y.go", 30)], &tree),
            flagged
        );
        // A deleted source file is dropped gold.
        assert_eq!(
            tags_of(&[('M', "src/a.rs", 10), ('D', "src/gone.rs", 30)], &tree),
            flagged
        );
        // So is an eligible file that is not at HEAD (a split with no
        // detected rename looks like this).
        assert_eq!(
            tags_of(
                &[('M', "src/a.rs", 10), ('M', "src/vanished.rs", 30)],
                &tree
            ),
            flagged
        );

        // Gold is the majority.
        assert_eq!(
            tags_of(&[('M', "src/a.rs", 40), ('M', "README.md", 10)], &tree),
            plain
        );
        // Exactly half is not "more than half".
        assert_eq!(
            tags_of(&[('M', "src/a.rs", 20), ('M', "README.md", 20)], &tree),
            plain
        );
        // Without line data (nothing printed, or only binary files) there is
        // no drift to measure.
        assert_eq!(
            tags_of(&[('M', "src/a.rs", 0), ('M', "README.md", 0)], &tree),
            plain
        );
        let no_data = commit(MID_SUBJECT, "", &[('M', "src/a.rs"), ('M', "README.md")]);
        assert_eq!(mine_commit(&no_data, &head(&tree)).unwrap().tags, plain);
    }

    #[test]
    fn a_file_kept_through_a_later_rename_is_not_drift() {
        let tree = head(&["lci-core/src/lsp.rs", "src/other.rs"]);
        let moves = [rename("src/lsp.rs", "lci-core/src/lsp.rs")];
        let edit = with_lines(
            commit(
                MID_SUBJECT,
                "",
                &[('M', "src/lsp.rs"), ('M', "src/other.rs")],
            ),
            &[("src/lsp.rs", 30), ("src/other.rs", 20)],
        );
        // Chased to its new name, `src/lsp.rs` is gold, not drift.
        let chased = super::mine_commit(&edit, &tree, &moves).unwrap();
        assert_eq!(
            chased.expected_paths,
            ["lci-core/src/lsp.rs", "src/other.rs"]
        );
        assert_eq!(chased.tags, [TAG_KEYWORD_AGNOSTIC]);
        // Without the later move the same 30 lines are in a file gone from
        // HEAD, which outweighs the 20 that survived.
        let unchased = mine_commit(&edit, &tree).unwrap();
        assert_eq!(unchased.expected_paths, ["src/other.rs"]);
        assert_eq!(unchased.tags, [TAG_KEYWORD_AGNOSTIC, TAG_GOLD_DRIFT]);

        // A commit that itself moves a file that survives is all gold: the
        // delete and the add of a pure move both count on the gold side.
        let pure_move = with_lines(
            Commit {
                changes: vec![moved('R', "src/old_home.rs", "src/new_home.rs")],
                ..commit(MID_SUBJECT, "", &[])
            },
            &[("src/old_home.rs", 50), ("src/new_home.rs", 50)],
        );
        let moved_tree = head(&["src/new_home.rs"]);
        // The new path's stem is distinctive but absent from the subject.
        let mined = mine_commit(&pure_move, &moved_tree).unwrap();
        assert_eq!(mined.expected_paths, ["src/new_home.rs"]);
        assert_eq!(mined.tags, [TAG_KEYWORD_AGNOSTIC]);
    }

    #[test]
    fn tags_query_length_on_the_text_sent_to_search() {
        let tree = head(&["src/a.rs"]);
        let tags = |subject: &str, body: &str| {
            mine_commit(&commit(subject, body, &[('M', "src/a.rs")]), &tree)
                .unwrap()
                .tags
        };
        let words = |count: usize| {
            (0..count)
                .map(|index| format!("word{index}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let agnostic = vec![TAG_KEYWORD_AGNOSTIC];
        let short = vec![TAG_KEYWORD_AGNOSTIC, TAG_SHORT_QUERY];
        let long = vec![TAG_KEYWORD_AGNOSTIC, TAG_LONG_QUERY];

        // Short is fewer than 8 words, long is 25 or more; between is neither.
        assert_eq!(tags(&words(7), ""), short);
        assert_eq!(tags(&words(8), ""), agnostic);
        assert_eq!(tags(&words(24), ""), agnostic);
        assert_eq!(tags(&words(25), ""), long);

        // The appended body paragraph counts: a 5-word subject is short, and
        // the same subject with 22 new words of body is 27 words of query.
        let paragraph: String = (0..22).map(|index| format!("extra{index} ")).collect();
        assert_eq!(tags(&words(5), ""), short);
        assert_eq!(tags(&words(5), &paragraph), long);
        // A body that lifts it to 8 words only leaves the short band.
        assert_eq!(tags(&words(5), "extra0 extra1 extra2"), agnostic);

        // A pull-request reference is stripped before the text is sent, so it
        // does not count toward the length.
        assert_eq!(tags(&format!("{} (#12)", words(7)), ""), short);
        assert_eq!(tags(&format!("{} (#12)", words(24)), ""), agnostic);

        // The keyword tag is always first, whatever follows it.
        let keyword = tags("Expose list_indexed_files to the agent", "");
        assert_eq!(keyword, [TAG_KEYWORD, TAG_SHORT_QUERY]);
    }

    // -- paths ----------------------------------------------------------------

    #[test]
    fn brace_paths_are_excluded() {
        for path in ["src/{id}/page.tsx", "src/{id/page.tsx", "src/id}/page.tsx"] {
            assert!(is_excluded_path(path), "{path}");
            assert!(!is_eligible_path(path), "{path}");
        }
    }

    #[test]
    fn invalid_rendered_definition_is_not_written() {
        let tmp = TempDir::new();
        let query = MinedQuery {
            commit: "0123456789abcdef".to_owned(),
            subject: "Handle dynamic route pages correctly".to_owned(),
            query: "Handle dynamic route pages correctly".to_owned(),
            expected_paths: vec!["src/{id/page.tsx".to_owned()],
            tags: vec!["keyword-agnostic"],
        };
        let rendered = render_toml("fixture", "deadbeef", 1, &[query]);
        let definition: EvaluationFile = toml::from_str(&rendered).unwrap();
        assert!(validate_definition(&definition).is_err());

        let out = tmp.0.join("output/queries.toml");
        let error = write_definition(&out, &rendered).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid mined evaluation definition")
        );
        assert!(!out.parent().unwrap().exists());

        let existing = tmp.0.join("existing.toml");
        std::fs::write(&existing, "existing definition").unwrap();
        assert!(write_definition(&existing, &rendered).is_err());
        assert_eq!(
            std::fs::read_to_string(existing).unwrap(),
            "existing definition"
        );
    }

    #[test]
    fn follows_git_renames_when_config_disables_detection() {
        let tmp = TempDir::new();
        let repo = tmp.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        git(&repo, &["config", "--local", "diff.renames", "false"]).unwrap();
        std::fs::write(repo.join("old.rs"), "fn retrieve() {}\n").unwrap();
        git(&repo, &["add", "old.rs"]).unwrap();
        let commit_fixture = |subject: &str| {
            git(
                &repo,
                &[
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    subject,
                ],
            )
            .unwrap();
        };
        let subject = "Handle retrieval requests without blocking";
        commit_fixture(subject);
        git(&repo, &["mv", "old.rs", "new.rs"]).unwrap();
        commit_fixture("Move retrieval into another module");

        let out = tmp.0.join("queries.toml");
        run(repo, "fixture".to_owned(), out.clone(), 10).unwrap();
        let definition: EvaluationFile =
            toml::from_str(&std::fs::read_to_string(out).unwrap()).unwrap();
        validate_definition(&definition).unwrap();
        let original = definition
            .queries
            .iter()
            .find(|query| query.query == subject)
            .expect("the original commit must survive the later rename");
        assert_eq!(original.expected_paths, ["new.rs"]);
    }

    #[test]
    fn extension_and_path_rules() {
        for path in [
            "a.rs",
            "src/b.tsx",
            "x/y/z.hpp",
            "Program.cs",
            "a.b.c.py",
            "src/a.h",
            "src/b.cc",
            "src/c.cxx",
            "src/d.hh",
            "src/e.hxx",
        ] {
            assert!(is_eligible_path(path), "{path}");
        }
        for path in [
            "README.md",
            "Cargo.toml",
            "Makefile",
            ".rs",
            "src/a.RS",
            "src/a.rs.bak",
            "src/a.inl",
            "src/Vendor/a.rs.orig",
            "vendor/a.rs",
            "a/node_modules/b/c.js",
            "Third_Party/x.c",
            "dist/app.js",
            "yarn.lock",
            "web/pnpm-lock.yaml",
            "src/api_pb2.py",
            "Models/Foo.designer.cs",
            "src/a b[1].rs",
            "src/a*.rs",
        ] {
            assert!(!is_eligible_path(path), "{path}");
        }
        // `build.rs` is a file, not a build-output directory.
        assert!(is_eligible_path("xtask/src/build.rs"));
    }

    #[test]
    fn extensions_match_the_language_registry() {
        let registry = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lci-core/src/language.rs"
        ))
        .expect("read the language registry");
        let registered: BTreeSet<&str> = registry
            .lines()
            .filter_map(|line| line.trim().strip_prefix("extension: \""))
            .filter_map(|rest| rest.split('"').next())
            .collect();
        let listed: BTreeSet<&str> = SUPPORTED_EXTENSIONS.iter().copied().collect();
        assert_eq!(
            listed, registered,
            "update SUPPORTED_EXTENSIONS to match lci-core/src/language.rs"
        );
    }

    // -- TOML ------------------------------------------------------------------

    #[test]
    fn escapes_toml_strings() {
        assert_eq!(toml_string("plain"), "\"plain\"");
        assert_eq!(
            toml_string("say \"hi\" \\ and\ttab\nnewline\u{7}é"),
            "\"say \\\"hi\\\" \\\\ and\\ttab\\nnewline\\u0007é\""
        );
        assert_eq!(toml_array(["a/b.rs", "c.rs"]), "[\"a/b.rs\", \"c.rs\"]");
        assert_eq!(toml_array(Vec::<String>::new()), "[]");
    }

    #[test]
    fn renders_a_definition_with_provenance_comments_and_tags() {
        let queries = vec![
            MinedQuery {
                commit: "0123456789abcdef".to_owned(),
                subject: "Speed up search".to_owned(),
                query: "Speed up \"search\" for big repos".to_owned(),
                expected_paths: vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()],
                tags: vec!["keyword", "gold-drift", "short-query"],
            },
            MinedQuery {
                commit: "fedcba9876543210".to_owned(),
                subject: "Make results faster".to_owned(),
                query: "Make results faster".to_owned(),
                expected_paths: vec!["src/c.rs".to_owned()],
                tags: vec!["keyword-agnostic"],
            },
        ];
        let toml = render_toml("self", "deadbeef", 300, &queries);

        assert!(toml.starts_with("# Git-mined retrieval queries for workspace \"self\"."));
        assert!(toml.contains("GENERATED by `cargo xtask mine-queries"));
        assert!(toml.contains("at HEAD deadbeef."));
        assert!(toml.contains("do not use it to tune ranking"));
        assert!(toml.contains("formatting, lint or typo commits are skipped"));
        assert!(toml.contains("through later renames"));
        assert!(toml.contains("code-identifier-shaped token"));
        for phrase in [
            "`gold-drift` means",
            "`short-query` means",
            "`long-query` that",
            "fewer than 8 words",
            "# 25 or more (query-text words",
            "a pure\n# move counts on both sides",
        ] {
            assert!(toml.contains(phrase), "{phrase}");
        }
        assert!(toml.contains("[[workspaces]]\nkey = \"self\"\nrequired = true\n"));
        assert!(toml.contains(
            "\n# 0123456 Speed up search\n[[queries]]\nworkspace = \"self\"\n\
             query = \"Speed up \\\"search\\\" for big repos\"\ntop_k = 10\n\
             expected_paths = [\"src/a.rs\", \"src/b.rs\"]\n\
             tags = [\"keyword\", \"gold-drift\", \"short-query\"]\n"
        ));
        assert!(toml.contains(
            "\n# fedcba9 Make results faster\n[[queries]]\nworkspace = \"self\"\n\
             query = \"Make results faster\"\ntop_k = 10\nexpected_paths = [\"src/c.rs\"]\n\
             tags = [\"keyword-agnostic\"]\n"
        ));
        assert_eq!(toml.matches("[[queries]]").count(), 2);
    }
}
