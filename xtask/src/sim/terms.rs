//! Terms and searching for the simulated agent: which query terms it probes,
//! and a cached grep over the tree built on `lci_maps::live_grep::scan`.
//!
//! `scan` is case sensitive; an agent greps case-insensitively. A query term
//! (always lower case) is therefore searched in its spellings (`term`,
//! `Term`, `TERM`, and the token as the question wrote it, `parseConfig`) and
//! the results merged; that is still one tool call. A name taken from code is
//! searched exactly as written.

/// The tokens of a question as written, by lower-cased form (the first
/// spelling wins): `parseConfig` for the term `parseconfig`.
pub fn original_spellings(question: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for token in question.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#')) {
        let token = token.trim_matches('#');
        if !token.is_empty() {
            out.entry(token.to_lowercase())
                .or_insert_with(|| token.to_owned());
        }
    }
    out
}

use lci_maps::live_grep::{GrepOptions, LiveMatch, scan};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// The longest a single scan may run before it stops (and is counted in
/// [`Searcher::incomplete_scans`]). Generous: a stop here would make a run
/// depend on the machine's speed.
const SCAN_DEADLINE: Duration = Duration::from_secs(120);
/// Occurrences one scan keeps: all of them (see [`Searcher::search`]).
const SCAN_MATCHES: usize = usize::MAX;

/// Tokens of a tool output: characters / 4 rounded up, as `maps bench` does.
pub fn tokens_of(text: &str) -> u64 {
    text.chars().count().div_ceil(4) as u64
}

/// The query terms the agent probes: the `cap` longest terms of
/// `lci_core::lexical::query_terms` (ties in first-seen order), longest first.
pub fn pick_probe_terms(terms: &[String], cap: usize) -> Vec<String> {
    let mut order: Vec<usize> = (0..terms.len()).collect();
    order.sort_by(|&a, &b| {
        terms[b]
            .chars()
            .count()
            .cmp(&terms[a].chars().count())
            .then(a.cmp(&b))
    });
    order.truncate(cap);
    order.into_iter().map(|i| terms[i].clone()).collect()
}

fn spellings(term: &str) -> Vec<String> {
    let mut chars = term.chars();
    let capitalized = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => String::new(),
    };
    let mut out = vec![term.to_owned()];
    for variant in [capitalized, term.to_uppercase()] {
        if !out.contains(&variant) {
            out.push(variant);
        }
    }
    out
}

/// What one search found: occurrences (exact first, then by path and line;
/// 0-based lines) and the distinct files they lie in, in that order.
#[derive(Debug, Default)]
pub struct Hits {
    pub matches: Vec<LiveMatch>,
    pub files: Vec<String>,
}

/// A grep over a tree with caches for results and file text. The caches only
/// save work; they never change a result.
pub struct Searcher {
    root: PathBuf,
    options: GrepOptions,
    scans: HashMap<String, Rc<Hits>>,
    texts: HashMap<String, Rc<FileText>>,
    /// Scans that ended on the deadline or the byte cap, so their results may
    /// depend on the machine (a determinism warning).
    pub incomplete_scans: u32,
}

/// A file read once: the source and its lines (no `\r`, no final empty line).
#[derive(Debug)]
pub struct FileText {
    pub source: String,
    pub lines: Vec<String>,
}

impl Searcher {
    pub fn new(root: &Path) -> Self {
        let options = GrepOptions {
            max_matches: SCAN_MATCHES,
            timeout: SCAN_DEADLINE,
            ..GrepOptions::default()
        };
        Self {
            root: root.to_path_buf(),
            options,
            scans: HashMap::new(),
            texts: HashMap::new(),
            incomplete_scans: 0,
        }
    }

    pub fn options(&self) -> &GrepOptions {
        &self.options
    }

    /// Every occurrence of `needle`, exactly as written. Nothing is cut: a
    /// file count (a name's rarity) must not depend on a match cap. The agent
    /// is shown only the first lines of it.
    pub fn search(&mut self, needle: &str) -> Rc<Hits> {
        self.search_spellings(vec![needle.to_owned()])
    }

    /// Every occurrence of a query term (lower case) in its lower,
    /// Capitalized and UPPER spellings and, when given, as the question
    /// wrote it (`parseConfig`): one call for the agent.
    pub fn search_term(&mut self, term: &str, original: Option<&str>) -> Rc<Hits> {
        let mut needles = spellings(term);
        if let Some(original) =
            original.filter(|o| !o.is_empty() && !needles.iter().any(|n| n == o))
        {
            needles.push(original.to_owned());
        }
        self.search_spellings(needles)
    }

    fn search_spellings(&mut self, needles: Vec<String>) -> Rc<Hits> {
        let key = needles.join("\u{0}");
        if let Some(hits) = self.scans.get(&key) {
            return Rc::clone(hits);
        }
        let mut matches: Vec<LiveMatch> = Vec::new();
        for spelling in &needles {
            let result = scan(
                &self.root,
                spelling,
                &self.options,
                Instant::now() + SCAN_DEADLINE,
            );
            if matches!(result.stopped, Some("deadline" | "byte cap")) {
                self.incomplete_scans += 1;
            }
            matches.extend(result.matches);
        }
        matches.sort_by(|a, b| {
            (!a.exact, &a.path, a.line, a.col).cmp(&(!b.exact, &b.path, b.line, b.col))
        });
        matches.dedup_by(|a, b| (&a.path, a.line, a.col) == (&b.path, b.line, b.col));
        let mut files: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for m in &matches {
            if seen.insert(&m.path) {
                files.push(m.path.clone());
            }
        }
        let hits = Rc::new(Hits { matches, files });
        self.scans.insert(key, Rc::clone(&hits));
        hits
    }

    /// The text of a file (empty when it cannot be read).
    pub fn text(&mut self, path: &str) -> Rc<FileText> {
        if let Some(text) = self.texts.get(path) {
            return Rc::clone(text);
        }
        let bytes = std::fs::read(self.root.join(path)).unwrap_or_default();
        let source = String::from_utf8_lossy(&bytes).into_owned();
        let mut lines: Vec<String> = source
            .split('\n')
            .map(|l| l.trim_end_matches('\r').to_owned())
            .collect();
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        let text = Rc::new(FileText { source, lines });
        self.texts.insert(path.to_owned(), Rc::clone(&text));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_terms_are_the_longest_first() {
        let terms: Vec<String> = ["ab", "alphabet", "beta", "gamma_ray", "delta"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            pick_probe_terms(&terms, 3),
            ["gamma_ray", "alphabet", "delta"]
        );
        assert_eq!(pick_probe_terms(&terms, 99).len(), 5);
    }

    #[test]
    fn spellings_cover_the_common_cases_once() {
        assert_eq!(spellings("parse"), ["parse", "Parse", "PARSE"]);
        assert_eq!(spellings("123"), ["123"]);
    }

    fn temp_tree(tag: &str, files: &[(&str, String)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("xtask-sim-terms-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (path, text) in files {
            std::fs::write(dir.join(path), text).unwrap();
        }
        dir
    }

    #[test]
    fn a_names_file_count_is_not_cut_at_a_match_cap() {
        // 250 occurrences in the first file in path order, then two more files.
        let many = "hotname();\n".repeat(250);
        let dir = temp_tree(
            "cap",
            &[
                ("a.rs", many),
                ("b.rs", "fn x() { hotname(); }\n".into()),
                ("c.rs", "fn y() { hotname(); }\n".into()),
            ],
        );
        let mut searcher = Searcher::new(&dir);
        let hits = searcher.search("hotname");
        assert_eq!(hits.matches.len(), 252);
        assert_eq!(hits.files, ["a.rs", "b.rs", "c.rs"]);
        let term = searcher.search_term("hotname", None);
        assert_eq!(term.files.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_term_also_matches_the_spelling_the_question_used() {
        let dir = temp_tree("camel", &[("a.rs", "fn parseConfig() {}\n".into())]);
        let mut searcher = Searcher::new(&dir);
        assert!(searcher.search_term("parseconfig", None).files.is_empty());
        let originals = original_spellings("where is parseConfig used?");
        assert_eq!(originals["parseconfig"], "parseConfig");
        let hits = searcher.search_term("parseconfig", Some(&originals["parseconfig"]));
        assert_eq!(hits.files, ["a.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tokens_are_chars_over_four_rounded_up() {
        assert_eq!(tokens_of(""), 0);
        assert_eq!(tokens_of("abcd"), 1);
        assert_eq!(tokens_of("abcde"), 2);
    }
}
