//! `cargo xtask bench mine`: replayed-commit tasks for the spike bench set
//! (`docs/spike/multimap/bench.md`, section 4).
//!
//! Each qualifying fix commit becomes one task. The search runs on the fix
//! commit's parent (the base commit), so every gold file exists there and
//! holds the code before the fix. Gold files are the non-test source files
//! the fix modified; gold symbols are the enclosing declaration names git's
//! built-in diff drivers print in the hunk headers of those files. Neither
//! comes from a SCIP or LSP index or from LCI's own parser (lesson L8).
//!
//! The output is data only: repository URL, commit ids, query text and gold
//! paths and names. No code from the mined repository is stored.
//!
//! Everything except [`run`] is pure (text in, text out) and is tested on
//! synthetic `git` output.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::mine::{
    MIN_SUBJECT_WORDS, SUPPORTED_EXTENSIONS, TAG_KEYWORD, TAG_KEYWORD_AGNOSTIC, clean_subject, git,
    has_identifier_shape, is_eligible_path, is_merge_subject, is_noise_subject,
    names_a_touched_file, query_text, split_words,
};

/// Schema id of the bench set file.
pub const SET_SCHEMA: &str = "lci.bench-set/1";
/// A fix touching more source files than this is not a focused fix.
pub const MAX_GOLD_FILES: usize = 3;
/// Tag of a task whose query names one of its gold symbols.
const TAG_NAMES_SYMBOL: &str = "names-symbol";
/// Shortest gold symbol name that `names-symbol` looks for.
const MIN_SYMBOL_CHARS: usize = 3;

const LOG_FORMAT: &str = "--format=%x1e%H%x1f%P%x1f%cI%x1f%s%x1f%b%x1f";
const RECORD_SEPARATOR: char = '\u{1e}';
const FIELD_SEPARATOR: char = '\u{1f}';

/// The attributes file that maps extensions to git's built-in diff drivers.
pub const DIFF_ATTRIBUTES: &str = "*.rs diff=rust\n*.go diff=golang\n*.py diff=python\n";

/// Words that mark a commit as a fix.
const FIX_WORDS: &[&str] = &[
    "fix",
    "fixes",
    "fixed",
    "fixing",
    "bug",
    "bugfix",
    "regression",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCommit {
    pub hash: String,
    pub parents: Vec<String>,
    pub date: String,
    pub subject: String,
    pub body: String,
    /// `(status letter, path)`; for a rename or copy the path is the new one.
    pub changes: Vec<(char, String)>,
}

/// Parse `git log --name-status` output written with [`LOG_FORMAT`].
pub fn parse_log(raw: &str) -> Vec<LogCommit> {
    raw.split(RECORD_SEPARATOR)
        .filter_map(|record| {
            let mut fields = record.splitn(6, FIELD_SEPARATOR);
            let hash = fields.next()?.trim().to_owned();
            if hash.is_empty() {
                return None;
            }
            let parents = fields
                .next()?
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let date = fields.next()?.trim().to_owned();
            let subject = fields.next()?.trim().to_owned();
            let body = fields.next()?.trim().to_owned();
            let changes = fields
                .next()
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut parts = line.split('\t');
                    let status = parts.next()?.chars().next()?;
                    let path = parts.next_back()?;
                    (!path.is_empty() && !path.starts_with('"')).then(|| (status, path.to_owned()))
                })
                .collect();
            Some(LogCommit {
                hash,
                parents,
                date,
                subject,
                body,
                changes,
            })
        })
        .collect()
}

/// Test, benchmark and fixture files: never gold (the bench asks for the
/// code a fix changed, not the test that pins it).
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    parts.iter().any(|dir| {
        matches!(
            *dir,
            "test" | "tests" | "testdata" | "testing" | "benches" | "fixtures" | "integration"
        )
    }) || name.ends_with("_test.go")
        || name.ends_with("_test.py")
        || name.starts_with("test_")
        || name == "tests.rs"
        || name == "conftest.py"
}

fn is_fix(subject: &str, body: &str) -> bool {
    let words = |text: &str| split_words(text).into_iter().collect::<BTreeSet<_>>();
    let found = |text: &str| words(text).iter().any(|w| FIX_WORDS.contains(&w.as_str()));
    found(subject) || found(body)
}

fn is_revert(commit: &LogCommit) -> bool {
    commit
        .subject
        .split_whitespace()
        .next()
        .is_some_and(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .eq_ignore_ascii_case("revert")
        })
        || commit
            .body
            .to_ascii_lowercase()
            .contains("this reverts commit")
}

/// Why a commit did not become a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Skip {
    Merge,
    Revert,
    ShortSubject,
    Noise,
    NotAFix,
    NoSourceFiles,
    NotOnlyModified,
    TooManyFiles,
    Duplicate,
}

impl Skip {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Merge => "merge commit",
            Self::Revert => "revert",
            Self::ShortSubject => "subject under 4 words",
            Self::Noise => "formatting, lint or typo fix",
            Self::NotAFix => "message does not say fix or bug",
            Self::NoSourceFiles => "modifies no non-test source file",
            Self::NotOnlyModified => "adds, deletes or renames a non-test source file",
            Self::TooManyFiles => "modifies more than 3 non-test source files",
            Self::Duplicate => "same query text as a newer task",
        }
    }
}

/// A commit that qualifies, before its symbols are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub hash: String,
    pub base: String,
    pub date: String,
    pub query: String,
    pub gold_files: Vec<String>,
}

pub fn select(commit: &LogCommit) -> Result<Candidate, Skip> {
    if commit.parents.len() != 1 || is_merge_subject(&commit.subject) {
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
    if !is_fix(subject, &commit.body) {
        return Err(Skip::NotAFix);
    }
    let source: Vec<&(char, String)> = commit
        .changes
        .iter()
        .filter(|(_, path)| is_eligible_path(path) && !is_test_path(path))
        .collect();
    if source.is_empty() {
        return Err(Skip::NoSourceFiles);
    }
    if source.iter().any(|(status, _)| *status != 'M') {
        return Err(Skip::NotOnlyModified);
    }
    let gold_files: Vec<String> = source
        .iter()
        .map(|(_, path)| path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if gold_files.len() > MAX_GOLD_FILES {
        return Err(Skip::TooManyFiles);
    }
    Ok(Candidate {
        hash: commit.hash.clone(),
        base: commit.parents[0].clone(),
        date: commit.date.clone(),
        query: query_text(&commit.subject, &commit.body),
        gold_files,
    })
}

/// The identifier that starts `text`, if any.
fn identifier(text: &str) -> Option<String> {
    let name: String = text
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit())).then_some(name)
}

/// The text after the first whole-word occurrence of `keyword` followed by
/// whitespace.
fn after_keyword<'a>(header: &'a str, keyword: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(found) = header[from..].find(keyword) {
        let start = from + found;
        let end = start + keyword.len();
        let before_ok = header[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let rest = &header[end..];
        if before_ok && rest.starts_with(char::is_whitespace) {
            return Some(rest.trim_start());
        }
        from = end;
    }
    None
}

/// The declaration name in a hunk header, by the language of the file's
/// extension. Headers that name no single declaration (an `impl` block, a
/// module, a Go `var` group) give `None`.
pub fn header_symbol(extension: &str, header: &str) -> Option<String> {
    let header = header.trim();
    match extension {
        "rs" => [
            "fn", "struct", "enum", "trait", "union", "type", "const", "static",
        ]
        .iter()
        .filter_map(|keyword| {
            after_keyword(header, keyword).map(|rest| (header.len() - rest.len(), rest))
        })
        .min_by_key(|(position, _)| *position)
        .and_then(|(_, rest)| identifier(rest.trim_start_matches("mut ").trim_start())),
        "go" => {
            if let Some(rest) = after_keyword(header, "func") {
                let rest = if rest.starts_with('(') {
                    // Skip the receiver: `func (s *Server) Name(`.
                    let close = rest.find(')')?;
                    rest[close + 1..].trim_start()
                } else {
                    rest
                };
                identifier(rest)
            } else {
                after_keyword(header, "type").and_then(identifier)
            }
        }
        "py" => after_keyword(header, "def")
            .or_else(|| after_keyword(header, "class"))
            .and_then(identifier),
        _ => None,
    }
}

/// Gold symbols per file from `git diff -U0` output written with
/// [`DIFF_ATTRIBUTES`]: `path -> names`, only for paths in `gold_files`.
pub fn hunk_symbols(diff: &str, gold_files: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let mut symbols: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut current: Option<&String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            current = gold_files.iter().find(|gold| gold.as_str() == path);
        } else if let (Some(path), Some(rest)) = (current, line.strip_prefix("@@ ")) {
            let Some((_, header)) = rest.split_once(" @@") else {
                continue;
            };
            let extension = path.rsplit_once('.').map_or("", |(_, ext)| ext);
            if let Some(name) = header_symbol(extension, header) {
                symbols.entry(path.clone()).or_default().insert(name);
            }
        }
    }
    symbols
}

/// Tags of a task: the keyword axis first, then `names-symbol`.
pub fn task_tags(query: &str, gold_files: &[String], symbols: &BTreeSet<String>) -> Vec<String> {
    let mut tags = vec![
        if names_a_touched_file(query, gold_files) || has_identifier_shape(query) {
            TAG_KEYWORD
        } else {
            TAG_KEYWORD_AGNOSTIC
        }
        .to_owned(),
    ];
    let words: BTreeSet<String> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(str::to_lowercase)
        .collect();
    if symbols.iter().any(|name| {
        name.chars().count() >= MIN_SYMBOL_CHARS && words.contains(&name.to_lowercase())
    }) {
        tags.push(TAG_NAMES_SYMBOL.to_owned());
    }
    tags
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// One source repository to mine.
pub struct Source {
    pub key: String,
    pub path: PathBuf,
    pub license: String,
}

fn mine_repo(source: &Source, max: usize, attributes: &Path) -> Result<Value> {
    let repo = &source.path;
    let url = git(repo, &["remote", "get-url", "origin"])?
        .trim()
        .to_owned();
    let snapshot = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();
    let log = git(
        repo,
        &[
            "-c",
            "core.quotepath=off",
            "log",
            "--first-parent",
            "--name-status",
            "--no-color",
            "--no-show-signature",
            "--encoding=UTF-8",
            LOG_FORMAT,
            "HEAD",
        ],
    )?;
    let commits = parse_log(&log);
    let attributes_arg = format!("core.attributesFile={}", attributes.display());
    let mut skipped: BTreeMap<Skip, usize> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut tasks = Vec::new();
    let mut scanned = 0;
    for commit in &commits {
        if tasks.len() >= max {
            break;
        }
        scanned += 1;
        let candidate = match select(commit) {
            Ok(candidate) if seen.insert(candidate.query.to_lowercase()) => candidate,
            Ok(_) => {
                *skipped.entry(Skip::Duplicate).or_default() += 1;
                continue;
            }
            Err(reason) => {
                *skipped.entry(reason).or_default() += 1;
                continue;
            }
        };
        let mut args = vec![
            "-c",
            attributes_arg.as_str(),
            "-c",
            "core.quotepath=off",
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            candidate.base.as_str(),
            candidate.hash.as_str(),
            "--",
        ];
        args.extend(candidate.gold_files.iter().map(String::as_str));
        let diff = git(repo, &args)?;
        let symbols = hunk_symbols(&diff, &candidate.gold_files);
        let all_symbols: BTreeSet<String> = symbols.values().flatten().cloned().collect();
        let tags = task_tags(&candidate.query, &candidate.gold_files, &all_symbols);
        let gold_symbols: Vec<Value> = symbols
            .iter()
            .flat_map(|(path, names)| {
                names
                    .iter()
                    .map(move |name| json!({"path": path, "name": name}))
            })
            .collect();
        tasks.push(json!({
            "id": format!("{}-{}", source.key, short(&candidate.hash)),
            "repo": source.key,
            "fix_commit": candidate.hash,
            "base_commit": candidate.base,
            "date": candidate.date,
            "query": candidate.query,
            "query_source": "commit-message",
            "gold_files": candidate.gold_files,
            "gold_symbols": gold_symbols,
            "tags": tags,
        }));
    }
    // Oldest first: the runner replays a repository's tasks in this order.
    tasks.reverse();
    let skipped: BTreeMap<&str, usize> = skipped.iter().map(|(k, v)| (k.label(), *v)).collect();
    Ok(json!({
        "repo": {
            "key": source.key,
            "url": url,
            "license": source.license,
            "snapshot": snapshot,
            "commits_scanned": scanned,
            "skipped": skipped,
        },
        "tasks": tasks,
    }))
}

pub fn run(sources: Vec<Source>, max: usize, version: &str, out: &Path) -> Result<()> {
    ensure!(max > 0, "--max must be at least 1");
    ensure!(!sources.is_empty(), "pass at least one --source");
    let scratch = std::env::temp_dir().join(format!("lci-bench-attrs-{}", std::process::id()));
    std::fs::write(&scratch, DIFF_ATTRIBUTES).context("writing the diff attributes file")?;
    let mut repos = Vec::new();
    let mut tasks = Vec::new();
    for source in &sources {
        let mined = mine_repo(source, max, &scratch)?;
        eprintln!(
            "{}: {} tasks from {} commits",
            source.key,
            mined["tasks"].as_array().map_or(0, Vec::len),
            mined["repo"]["commits_scanned"]
        );
        repos.push(mined["repo"].clone());
        tasks.extend(mined["tasks"].as_array().cloned().unwrap_or_default());
    }
    let _ = std::fs::remove_file(&scratch);
    let set = json!({
        "schema": SET_SCHEMA,
        "version": version,
        "filter": {
            "walk": "git log --first-parent from the snapshot commit, newest first",
            "per_repo_max": max,
            "fix_words": FIX_WORDS,
            "min_subject_words": MIN_SUBJECT_WORDS,
            "supported_extensions": SUPPORTED_EXTENSIONS,
            "source_files": "supported extension, not excluded (vendored, generated, lockfile), not a test path",
            "max_gold_files": MAX_GOLD_FILES,
            "only_modified_source_files": true,
            "skip": ["merge commits", "reverts", "formatting, lint or typo fixes", "duplicate query text"],
            "query": "commit subject plus the first body paragraph when it adds 3 or more words, at most 300 characters",
            "gold_symbols": "names from git's built-in rust/golang/python diff driver hunk headers of git diff -U0 base..fix",
        },
        "repos": repos,
        "tasks": tasks,
    });
    let mut text = serde_json::to_string_pretty(&set)?;
    text.push('\n');
    std::fs::write(out, text).with_context(|| format!("writing {}", out.display()))?;
    eprintln!(
        "wrote {} tasks to {}",
        set["tasks"].as_array().map_or(0, Vec::len),
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, body: &str, changes: &[(char, &str)]) -> LogCommit {
        LogCommit {
            hash: "a".repeat(40),
            parents: vec!["b".repeat(40)],
            date: "2026-01-02T03:04:05+00:00".into(),
            subject: subject.into(),
            body: body.into(),
            changes: changes.iter().map(|(s, p)| (*s, (*p).to_owned())).collect(),
        }
    }

    #[test]
    fn parse_log_reads_parents_and_changes() {
        let raw = "\u{1e}h1\u{1f}p1\u{1f}2026-01-01T00:00:00Z\u{1f}fix the parser crash\u{1f}body\u{1f}\n\nM\tsrc/a.rs\nR100\tsrc/old.rs\tsrc/new.rs\n\
                   \u{1e}h2\u{1f}p2 p3\u{1f}2026-01-02T00:00:00Z\u{1f}Merge x\u{1f}\u{1f}\n";
        let commits = parse_log(raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].parents, vec!["p1"]);
        assert_eq!(
            commits[0].changes,
            vec![('M', "src/a.rs".into()), ('R', "src/new.rs".into())]
        );
        assert_eq!(commits[1].parents.len(), 2);
    }

    #[test]
    fn test_paths_are_never_gold() {
        for path in [
            "tests/foo.rs",
            "src/x/tests.rs",
            "pkg/server_test.go",
            "tests/data/cases/a.py",
            "src/test_utils.py",
            "caddytest/integration/x.go",
            "benches/b.rs",
            "conftest.py",
        ] {
            assert!(is_test_path(path), "{path}");
        }
        for path in [
            "src/lib.rs",
            "modules/caddyhttp/server.go",
            "src/black/linegen.py",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn select_keeps_a_focused_fix_and_says_why_others_fail() {
        let ok = commit(
            "fix panic when the timer wheel overflows",
            "",
            &[
                ('M', "tokio/src/time/wheel.rs"),
                ('M', "tokio/tests/time.rs"),
            ],
        );
        let candidate = select(&ok).unwrap();
        assert_eq!(candidate.gold_files, vec!["tokio/src/time/wheel.rs"]);
        assert_eq!(candidate.base, "b".repeat(40));

        let cases = [
            (
                commit("add a new feature to the timer", "", &[('M', "src/a.rs")]),
                Skip::NotAFix,
            ),
            (
                commit("fix bug", "", &[('M', "src/a.rs")]),
                Skip::ShortSubject,
            ),
            (
                commit("fix the docs for the timer", "", &[('M', "README.md")]),
                Skip::NoSourceFiles,
            ),
            (
                commit(
                    "fix the timer when it overflows",
                    "",
                    &[('A', "src/new.rs")],
                ),
                Skip::NotOnlyModified,
            ),
            (
                commit(
                    "fix all the things in the tree",
                    "",
                    &[('M', "a.rs"), ('M', "b.rs"), ('M', "c.rs"), ('M', "d.rs")],
                ),
                Skip::TooManyFiles,
            ),
            (
                commit(
                    "Revert \"fix the timer overflow bug\"",
                    "",
                    &[('M', "a.rs")],
                ),
                Skip::Revert,
            ),
            (
                commit("fix formatting of the timer module", "", &[('M', "a.rs")]),
                Skip::Noise,
            ),
        ];
        for (commit, reason) in cases {
            assert_eq!(select(&commit), Err(reason), "{}", commit.subject);
        }
        let mut merge = commit("fix the timer when it overflows", "", &[('M', "a.rs")]);
        merge.parents.push("c".repeat(40));
        assert_eq!(select(&merge), Err(Skip::Merge));
    }

    #[test]
    fn a_fix_word_in_the_body_counts() {
        let c = commit(
            "handle an empty header in the parser",
            "This fixes a crash on empty input.",
            &[('M', "src/parse.py")],
        );
        assert!(select(&c).is_ok());
    }

    #[test]
    fn header_symbols_per_language() {
        let cases = [
            (
                "rs",
                "pub(crate) async fn poll_ready<T>(&mut self) -> Poll<()> {",
                Some("poll_ready"),
            ),
            ("rs", "impl<T> Sender<T> {", None),
            ("rs", "pub struct Wheel {", Some("Wheel")),
            ("rs", "mod tests {", None),
            (
                "go",
                "func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {",
                Some("ServeHTTP"),
            ),
            (
                "go",
                "func parseAddress(addr string) (Address, error) {",
                Some("parseAddress"),
            ),
            ("go", "type Handler struct {", Some("Handler")),
            (
                "py",
                "    def visit_default(self, node: Leaf) -> Iterator[Line]:",
                Some("visit_default"),
            ),
            (
                "py",
                "class LineGenerator(Visitor[Line]):",
                Some("LineGenerator"),
            ),
            ("py", "async def main():", Some("main")),
            ("py", "", None),
            ("c", "int main(void) {", None),
        ];
        for (extension, header, expected) in cases {
            assert_eq!(
                header_symbol(extension, header).as_deref(),
                expected,
                "{header}"
            );
        }
    }

    #[test]
    fn hunk_symbols_reads_only_gold_files() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
                    @@ -10 +10 @@ fn first() {\n-x\n+y\n@@ -20,0 +21 @@ impl Foo {\n+z\n\
                    diff --git a/tests/t.rs b/tests/t.rs\n--- a/tests/t.rs\n+++ b/tests/t.rs\n\
                    @@ -1 +1 @@ fn in_test() {\n";
        let symbols = hunk_symbols(diff, &["src/a.rs".to_owned()]);
        assert_eq!(symbols.len(), 1);
        assert_eq!(
            symbols["src/a.rs"].iter().collect::<Vec<_>>(),
            vec!["first"]
        );
    }

    #[test]
    fn tags_mark_keywords_and_named_symbols() {
        let gold = vec!["src/black/linegen.py".to_owned()];
        let symbols: BTreeSet<String> = ["visit_default".to_owned()].into();
        assert_eq!(
            task_tags("crash in visit_default on empty input", &gold, &symbols),
            vec![TAG_KEYWORD, TAG_NAMES_SYMBOL]
        );
        assert_eq!(
            task_tags("crash on empty input with comments", &gold, &symbols),
            vec![TAG_KEYWORD_AGNOSTIC]
        );
    }
}
