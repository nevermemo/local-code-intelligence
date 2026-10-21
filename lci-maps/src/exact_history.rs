//! The exact history baseline of the multimap spike: the question families
//! told apart by wording (H1 first creation, H2 last modification, H3 free
//! "why" text) and the `git log -G/-S/--grep` calls that answer them, in a
//! fixed order (see [`history_commits`]).
//!
//! Git sits behind the [`GitLog`] trait so tests can fake it; [`RealGit`] runs
//! the binary with a timeout ([`run_capture`]). A call that fails (a timeout,
//! an unknown revision) leaves its list empty and is counted in a [`Tally`]
//! rather than aborting the question.

use crate::process::{KillScope, wait_limited};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The history question families this arm tells apart by wording (not by the
/// gold's `stratum`). The patterns come from the wording described in the R3
/// design and are not checked against the private gold here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryQuery {
    /// H1, "first created `X`": commits that introduced a declaration of X,
    /// oldest first.
    Creation { idents: Vec<String> },
    /// H2, "most recent commit that modified `X` (path)", with the sha the
    /// history is cut at: commits that changed the count of X in the path,
    /// newest first; the plain file log when there are none.
    LastModified {
        idents: Vec<String>,
        path: Option<String>,
        sha: Option<String>,
    },
    /// H3, free "why" text: commits matching up to five tokens by message
    /// (`--grep -i`) or by diff (`-S`).
    Why { tokens: Vec<String> },
}

impl HistoryQuery {
    /// `"H1"`, `"H2"` or `"H3"`.
    pub fn family(&self) -> &'static str {
        match self {
            Self::Creation { .. } => "H1",
            Self::LastModified { .. } => "H2",
            Self::Why { .. } => "H3",
        }
    }
}

/// The most tokens of an H3 query.
const MAX_TOKENS: usize = 5;
/// Words of six letters or more that say nothing about the code.
const STOP_WORDS: [&str; 22] = [
    "because", "should", "would", "could", "which", "where", "there", "their", "these", "those",
    "before", "after", "being", "using", "commit", "commits", "change", "changed", "reason",
    "happened", "behind", "decide",
];
const PATH_EXTENSIONS: [&str; 8] = [".rs", ".toml", ".md", ".json", ".py", ".ts", ".js", ".yml"];

fn is_ident(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(|c: char| c.is_ascii_digit())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The identifiers in the backticks of a question: the last `::` segment of a
/// span, `()` and generic arguments dropped; spans that are not paths of
/// identifiers (code, file names) are skipped. First occurrence order.
pub fn backtick_idents(question: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for span in question.split('`').skip(1).step_by(2) {
        let mut depth = 0u32;
        let text: String = span
            .trim()
            .chars()
            .filter(|&c| match c {
                '<' => {
                    depth += 1;
                    false
                }
                '>' => {
                    depth = depth.saturating_sub(1);
                    false
                }
                _ => depth == 0,
            })
            .collect();
        let text = text.trim().strip_suffix("()").unwrap_or(text.trim());
        let Some(last) = text.rsplit("::").next() else {
            continue;
        };
        if text.split("::").all(is_ident) && !found.iter().any(|f| f == last) {
            found.push(last.to_owned());
        }
    }
    found
}

fn trim_token(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '`' | '(' | ')' | ',' | ';' | ':' | '\'' | '"' | '?' | '.'
        )
    })
}

/// The first token that looks like a file path: it has a `/`, or a known
/// extension, and does not start with `-`.
fn path_token(question: &str) -> Option<String> {
    question
        .split_whitespace()
        .map(trim_token)
        .find(|t| {
            !t.starts_with('-')
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
                && (t.contains('/') || PATH_EXTENSIONS.iter().any(|e| t.ends_with(e)))
                && t.chars().any(|c| c.is_ascii_alphabetic())
        })
        .map(str::to_owned)
}

/// The first token that looks like an abbreviated or full commit sha: seven to
/// forty hex digits with at least one decimal digit.
fn sha_token(question: &str) -> Option<String> {
    question
        .split_whitespace()
        .map(trim_token)
        .find(|t| {
            (7..=40).contains(&t.len())
                && t.chars().all(|c| c.is_ascii_hexdigit())
                && t.chars().any(|c| c.is_ascii_digit())
        })
        .map(|t| t.to_ascii_lowercase())
}

/// Up to [`MAX_TOKENS`] tokens for an H3 query: the backticked identifiers,
/// then words of six or more letters (lower case, not in [`STOP_WORDS`]), in
/// the order of the question, each once.
pub fn why_tokens(question: &str) -> Vec<String> {
    let mut tokens: Vec<String> = backtick_idents(question);
    let outside: String = question.split('`').step_by(2).collect::<Vec<_>>().join(" ");
    for word in outside.split(|c: char| !c.is_ascii_alphabetic()) {
        let word = word.to_ascii_lowercase();
        if word.len() >= 6
            && !STOP_WORDS.contains(&word.as_str())
            && !tokens.iter().any(|t| t.eq_ignore_ascii_case(&word))
        {
            tokens.push(word);
        }
    }
    tokens.truncate(MAX_TOKENS);
    tokens
}

/// Tells the family of a question by its wording.
pub fn classify(question: &str) -> HistoryQuery {
    let lower = question.to_ascii_lowercase();
    let idents = backtick_idents(question);
    let created = [
        "first created",
        "first introduced",
        "first added",
        "was created",
        "first appeared",
    ]
    .iter()
    .any(|p| lower.contains(p));
    let modified = [
        "most recent",
        "last modified",
        "latest commit",
        "last changed",
        "last touched",
    ]
    .iter()
    .any(|p| lower.contains(p));
    if modified && (path_token(question).is_some() || !idents.is_empty()) {
        return HistoryQuery::LastModified {
            idents,
            path: path_token(question),
            sha: sha_token(question),
        };
    }
    if created && !idents.is_empty() {
        return HistoryQuery::Creation { idents };
    }
    HistoryQuery::Why {
        tokens: why_tokens(question),
    }
}

/// `git log` over a tree: the lines of its output, or why it failed.
pub trait GitLog {
    /// Runs `git log <args>` and returns its non-empty trimmed lines.
    fn log(&self, args: &[String]) -> Result<Vec<String>, String>;
}

/// [`GitLog`] over the git binary and a local checkout, each call under a
/// timeout.
pub struct RealGit {
    /// The git binary.
    pub git: PathBuf,
    /// The checkout, passed as `git -C <tree>`.
    pub tree: PathBuf,
    /// Longest one call may run; a slower one is killed and fails.
    pub timeout: Duration,
}

/// Runs `command`, killing it after `timeout`; the stdout text, or the error.
pub fn run_capture(mut command: Command, timeout: Duration) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("run git: {e}"))?;
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
    let waited = wait_limited(child, timeout, KillScope::Process)
        .map_err(|e| format!("wait for git: {e}"))?;
    let Some(status) = waited.status else {
        let _ = (stdout.join(), stderr.join());
        return Err(format!("timeout after {}s", timeout.as_secs()));
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if !status.success() {
        let text = String::from_utf8_lossy(&err);
        return Err(format!("git exited with {status}: {}", text.trim()));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

impl GitLog for RealGit {
    fn log(&self, args: &[String]) -> Result<Vec<String>, String> {
        let mut command = Command::new(&self.git);
        command.arg("-C").arg(&self.tree).arg("log").args(args);
        let text = run_capture(command, self.timeout)?;
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// Calls that failed in a run (a timeout, an unknown revision), with the
/// first reason.
#[derive(Debug, Default)]
pub struct Tally {
    /// How many calls failed.
    pub failures: usize,
    /// The reason of the first failure.
    pub first_failure: Option<String>,
}

fn ask(git: &dyn GitLog, args: &[&str], tally: &mut Tally) -> Vec<String> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    git.log(&args).unwrap_or_else(|why| {
        tally.failures += 1;
        tally.first_failure.get_or_insert(why);
        Vec::new()
    })
}

/// Like [`ask`] on `rev`, and on `HEAD` when `rev` fails (a sha the tree does
/// not have).
fn ask_at(git: &dyn GitLog, rev: &str, rest: &[String], tally: &mut Tally) -> Vec<String> {
    let mut args = vec![rev.to_owned()];
    args.extend(rest.iter().cloned());
    match git.log(&args) {
        Ok(lines) => lines,
        Err(_) if rev != "HEAD" => {
            args[0] = "HEAD".to_owned();
            git.log(&args).unwrap_or_else(|why| {
                tally.failures += 1;
                tally.first_failure.get_or_insert(why);
                Vec::new()
            })
        }
        Err(why) => {
            tally.failures += 1;
            tally.first_failure.get_or_insert(why);
            Vec::new()
        }
    }
}

/// One commit of each list in turn, first list first, each commit once, up to
/// `k`.
pub fn interleave(lists: &[Vec<String>], k: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for at in 0..longest {
        for list in lists {
            if let Some(sha) = list.get(at)
                && seen.insert(sha.clone())
            {
                out.push(sha.clone());
            }
        }
    }
    out.truncate(k);
    out
}

/// The H3 order: commits matched by more tokens first, then newer first (a
/// lower index in `recency`, the tree's `git log` order; unknown last), then
/// by hash.
pub fn rank_by_tokens(
    matches: &[(String, usize)],
    recency: &HashMap<String, usize>,
    k: usize,
) -> Vec<String> {
    let mut ranked: Vec<&(String, usize)> = matches.iter().collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| {
                let at = |sha: &String| recency.get(sha).copied().unwrap_or(usize::MAX);
                at(&a.0).cmp(&at(&b.0))
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .take(k)
        .map(|(sha, _)| sha.clone())
        .collect()
}

/// The commits of the baseline for one question.
///
/// Order, fixed:
/// * H1: per identifier `git log --reverse -E -G '(fn|struct|enum|type|trait|
///   const|static|mod|union)\s+X\b' -- '*.rs'` (oldest first; `-S X` when the
///   regex finds nothing); several identifiers are interleaved first
///   identifier first.
/// * H2: per identifier `git log <sha|HEAD> -S X -- <path>` (newest first);
///   when no identifier finds anything, `git log <sha|HEAD> -- <path>`;
///   several identifiers interleaved.
/// * H3: the union of `--grep t -i` and `-S t` over the tokens, ranked by the
///   number of tokens a commit matches, then newest first (the order of
///   `git log` on HEAD), then hash.
pub fn history_commits(
    git: &dyn GitLog,
    recency: &RefCell<Option<HashMap<String, usize>>>,
    query: &HistoryQuery,
    k: usize,
    tally: &mut Tally,
) -> Vec<String> {
    match query {
        HistoryQuery::Creation { idents } => {
            let lists: Vec<Vec<String>> = idents
                .iter()
                .map(|x| {
                    let regex = format!(
                        "(fn|struct|enum|type|trait|const|static|mod|union)[[:space:]]+{x}([^A-Za-z0-9_]|$)"
                    );
                    let by_decl = ask(
                        git,
                        &["--reverse", "--format=%H", "-E", &format!("-G{regex}"), "--", "*.rs"],
                        tally,
                    );
                    if by_decl.is_empty() {
                        ask(
                            git,
                            &["--reverse", "--format=%H", &format!("-S{x}"), "--", "*.rs"],
                            tally,
                        )
                    } else {
                        by_decl
                    }
                })
                .collect();
            interleave(&lists, k)
        }
        HistoryQuery::LastModified { idents, path, sha } => {
            let rev = sha.as_deref().unwrap_or("HEAD");
            let tail: Vec<String> = match path {
                Some(p) => vec!["--".to_owned(), p.clone()],
                None => Vec::new(),
            };
            let with = |extra: &[String]| -> Vec<String> {
                let mut v: Vec<String> = vec!["--format=%H".to_owned()];
                v.extend(extra.iter().cloned());
                v.extend(tail.iter().cloned());
                v
            };
            let lists: Vec<Vec<String>> = idents
                .iter()
                .map(|x| ask_at(git, rev, &with(&[format!("-S{x}")]), tally))
                .collect();
            let found = interleave(&lists, k);
            if !found.is_empty() || path.is_none() {
                return found;
            }
            let mut plain = ask_at(git, rev, &with(&[]), tally);
            plain.truncate(k);
            plain
        }
        HistoryQuery::Why { tokens } => {
            if tokens.is_empty() {
                return Vec::new();
            }
            let mut counts: Vec<(String, usize)> = Vec::new();
            let mut at: HashMap<String, usize> = HashMap::new();
            for token in tokens {
                let mut hit: BTreeSet<String> = BTreeSet::new();
                hit.extend(ask(
                    git,
                    &["--format=%H", "-i", &format!("--grep={token}")],
                    tally,
                ));
                hit.extend(ask(git, &["--format=%H", &format!("-S{token}")], tally));
                for sha in hit {
                    match at.get(&sha) {
                        Some(&i) => counts[i].1 += 1,
                        None => {
                            at.insert(sha.clone(), counts.len());
                            counts.push((sha, 1));
                        }
                    }
                }
            }
            if recency.borrow().is_none() {
                let order = ask(git, &["--format=%H"], tally)
                    .into_iter()
                    .enumerate()
                    .map(|(i, sha)| (sha, i))
                    .collect();
                *recency.borrow_mut() = Some(order);
            }
            let guard = recency.borrow();
            let empty = HashMap::new();
            rank_by_tokens(&counts, guard.as_ref().unwrap_or(&empty), k)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn questions_are_told_apart_by_their_wording() {
        let h1 = classify("Which commit first created `RuleList::from_json`?");
        assert_eq!(
            h1,
            HistoryQuery::Creation {
                idents: vec!["from_json".into()]
            }
        );
        let h2 = classify(
            "What is the most recent commit that modified `glob_match` (lci-maps/src/traverse/rules.rs) as of 1a2b3c4d5e?",
        );
        assert_eq!(
            h2,
            HistoryQuery::LastModified {
                idents: vec!["glob_match".into()],
                path: Some("lci-maps/src/traverse/rules.rs".into()),
                sha: Some("1a2b3c4d5e".into()),
            }
        );
        let h3 = classify(
            "Why does the traverse budget reject a larger `max_pops` than the configured cap?",
        );
        let HistoryQuery::Why { tokens } = h3 else {
            panic!("H3 expected");
        };
        assert_eq!(tokens[0], "max_pops");
        assert!(tokens.len() <= MAX_TOKENS);
        assert!(tokens.contains(&"traverse".to_owned()));
        assert!(!tokens.contains(&"because".to_owned()));
        // "first created" with no identifier is free text.
        assert_eq!(classify("When was the cache first created?").family(), "H3");
    }

    #[test]
    fn identifiers_come_from_paths_of_names_only() {
        assert_eq!(
            backtick_idents("`crate::a::Foo<T>::run()` and `x + y` and `Foo` and `src/a.rs`"),
            ["run", "Foo"]
        );
        assert_eq!(sha_token("at deadbeef"), None);
        assert_eq!(sha_token("at cafe1234 now"), Some("cafe1234".into()));
        assert_eq!(path_token("see -rf/x and a/b.rs"), Some("a/b.rs".into()));
    }

    #[test]
    fn interleave_and_rank_are_fixed_orders() {
        let a = vec!["1".to_owned(), "2".to_owned(), "3".to_owned()];
        let b = vec!["2".to_owned(), "9".to_owned()];
        assert_eq!(interleave(&[a, b], 4), ["1", "2", "9", "3"]);
        let recency: HashMap<String, usize> = [("n".to_owned(), 0), ("o".to_owned(), 5)]
            .into_iter()
            .collect();
        let hits = vec![
            ("o".to_owned(), 2),
            ("n".to_owned(), 1),
            ("z".to_owned(), 1),
            ("m".to_owned(), 2),
        ];
        // 2 tokens first (known newer before unknown), then 1 token (newer first).
        assert_eq!(rank_by_tokens(&hits, &recency, 10), ["o", "m", "n", "z"]);
        assert_eq!(rank_by_tokens(&hits, &recency, 2), ["o", "m"]);
    }

    struct Fake(Vec<(&'static str, Vec<&'static str>)>);

    impl GitLog for Fake {
        fn log(&self, args: &[String]) -> Result<Vec<String>, String> {
            let key = args.join(" ");
            self.0
                .iter()
                .find(|(k, _)| key.starts_with(k))
                .map(|(_, v)| v.iter().map(|s| (*s).to_owned()).collect())
                .ok_or_else(|| format!("unexpected call: {key}"))
        }
    }

    #[test]
    fn h2_falls_back_to_the_file_log_and_an_unknown_sha_to_head() {
        let git = Fake(vec![
            ("HEAD --format=%H -Sglob_match -- a/b.rs", vec![]),
            ("HEAD --format=%H -- a/b.rs", vec!["c3", "c2", "c1"]),
        ]);
        let query = HistoryQuery::LastModified {
            idents: vec!["glob_match".into()],
            path: Some("a/b.rs".into()),
            sha: None,
        };
        let mut tally = Tally::default();
        let recency = RefCell::new(None);
        let commits = history_commits(&git, &recency, &query, 2, &mut tally);
        assert_eq!(commits, ["c3", "c2"]);
        assert_eq!(tally.failures, 0);
        // A sha the tree lacks: the call on it fails, HEAD answers.
        let git = Fake(vec![("HEAD --format=%H -Sx -- p.rs", vec!["h1"])]);
        let query = HistoryQuery::LastModified {
            idents: vec!["x".into()],
            path: Some("p.rs".into()),
            sha: Some("deadbee1".into()),
        };
        let commits = history_commits(&git, &recency, &query, 10, &mut tally);
        assert_eq!(commits, ["h1"]);
    }

    #[test]
    fn a_failed_git_call_counts_and_leaves_that_list_empty() {
        let git = Fake(vec![]);
        let query = HistoryQuery::Creation {
            idents: vec!["Foo".into()],
        };
        let mut tally = Tally::default();
        let commits = history_commits(&git, &RefCell::new(None), &query, 10, &mut tally);
        assert!(commits.is_empty());
        // The -G call and the -S fallback both failed.
        assert_eq!(tally.failures, 2);
        assert!(tally.first_failure.unwrap().contains("unexpected call"));
    }

    /// Runs git in `dir` (hooks and signing off), panicking on failure.
    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git on PATH");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn real_git_orders_h1_oldest_first_and_h2_newest_first() {
        let dir = temp_dir("hist");
        git_in(&dir, &["init", "-q"]);
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn alpha() {}\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "add alpha"]);
        std::fs::write(&file, "fn alpha() {}\nfn beta() {}\n").unwrap();
        git_in(
            &dir,
            &["commit", "-q", "-am", "add beta, the reason is speed"],
        );
        std::fs::write(&file, "fn alpha() { alpha_helper(); }\nfn beta() {}\n").unwrap();
        git_in(&dir, &["commit", "-q", "-am", "touch alpha"]);
        let git = RealGit {
            git: PathBuf::from("git"),
            tree: dir.clone(),
            timeout: Duration::from_secs(30),
        };
        let recency = RefCell::new(None);
        let mut tally = Tally::default();
        let all = git.log(&["--format=%H".to_owned()]).unwrap();
        assert_eq!(all.len(), 3);
        let (newest, middle, oldest) = (&all[0], &all[1], &all[2]);

        // H1: `alpha` was created by the oldest commit (the newest rewrites its
        // line, which -G also matches, so it comes second); `beta` by the middle one.
        let q = classify("Which commit first created `alpha`?");
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(&c, &[oldest.clone(), newest.clone()]);
        let q = classify("Which commit first created `beta`?");
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(c, [middle.as_str()]);
        // H2: the count of "alpha" changed in the oldest and the newest commit; newest first.
        let q = classify("What is the most recent commit that modified `alpha` (a.rs)?");
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(&c, &[newest.clone(), oldest.clone()]);
        // H2 cut at the middle commit's sha: only the oldest is before it.
        let q = HistoryQuery::LastModified {
            idents: vec!["alpha".into()],
            path: Some("a.rs".into()),
            sha: Some(middle.clone()),
        };
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(c, [oldest.as_str()]);
        // H3: a message token finds the middle commit.
        let q = HistoryQuery::Why {
            tokens: vec!["reason".into()],
        };
        let c = history_commits(&git, &recency, &q, 10, &mut tally);
        assert_eq!(c, [middle.as_str()]);
        assert_eq!(tally.failures, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lci-maps-hist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
