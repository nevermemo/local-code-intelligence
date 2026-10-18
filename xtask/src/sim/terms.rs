//! Terms and searching for the simulated agent: which query terms it probes,
//! and a cached grep over the tree built on `lci_maps::live_grep::Corpus`
//! (the tree read once; its scans equal `live_grep::scan`).
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

use super::rg::RgCommand;
use lci_maps::live_grep::{Corpus, GrepOptions, LiveMatch};
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

/// The needles of one search of a query term: its spellings and, when given
/// and not among them, the token as the question wrote it.
pub fn term_needles(term: &str, original: Option<&str>) -> Vec<String> {
    let mut needles = spellings(term);
    if let Some(original) = original.filter(|o| !o.is_empty() && !needles.iter().any(|n| n == o)) {
        needles.push(original.to_owned());
    }
    needles
}

/// What one search found: occurrences (exact first, then by path and line;
/// 0-based lines) and the distinct files they lie in, in that order.
#[derive(Debug, Default)]
pub struct Hits {
    pub matches: Vec<LiveMatch>,
    pub files: Vec<String>,
}

/// What finds the occurrences: `live_grep::scan` in this process, or an
/// external `rg` run in the tree root (the R arm of `sim-structured`).
pub enum Backend {
    Live(GrepOptions),
    Rg(RgCommand),
}

/// What the scans of one [`Searcher`] did, summed over its calls. A cached
/// search is not scanned again, so the totals count each distinct search once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanTotals {
    /// Scans run (one per spelling searched).
    pub scans: u64,
    /// Files the walk left out by name (`GrepOptions::skip_files`).
    pub skipped_by_name: u64,
    pub files_read: u64,
    pub bytes_read: u64,
    /// Scans that stopped before the walk ended, for any reason.
    pub stopped: u64,
    /// Bytes and lines of `rg` standard output (the R arm only).
    pub rg_stdout_bytes: u64,
    pub rg_stdout_lines: u64,
    /// Of those, lines that were not `path:line:text` and were dropped.
    pub rg_ignored_lines: u64,
}

impl ScanTotals {
    pub fn add(&mut self, other: &ScanTotals) {
        self.scans += other.scans;
        self.skipped_by_name += other.skipped_by_name;
        self.files_read += other.files_read;
        self.bytes_read += other.bytes_read;
        self.stopped += other.stopped;
        self.rg_stdout_bytes += other.rg_stdout_bytes;
        self.rg_stdout_lines += other.rg_stdout_lines;
        self.rg_ignored_lines += other.rg_ignored_lines;
    }
}

/// A grep over a tree with caches for results and file text. The caches only
/// save work; they never change a result.
pub struct Searcher {
    root: PathBuf,
    backend: Backend,
    /// Where the name filter (`usable_identifier`) reads its limits when the
    /// backend is not `Live`; the defaults, as `Searcher::new` has them.
    names: GrepOptions,
    scans: HashMap<String, Rc<Hits>>,
    texts: HashMap<String, Rc<FileText>>,
    /// The tree read once, loaded by the first search of a `Live` backend
    /// with the backend's options. An edit to the tree after that is not seen.
    corpus: Option<Corpus>,
    /// Milliseconds the corpus load took (0 before it, and for `Rg`).
    pub corpus_load_ms: f64,
    /// Scans that ended on the deadline or the byte cap, so their results may
    /// depend on the machine (a determinism warning).
    pub incomplete_scans: u32,
    pub totals: ScanTotals,
    /// The first error of an external backend (`rg` failed to run or exited
    /// with a status other than 0 or 1). Set, every later search returns
    /// nothing and the caller must discard the run.
    pub failure: Option<String>,
}

/// The options `Searcher::new` scans with: no match cap, the generous
/// deadline, everything else the library default.
pub fn scan_options() -> GrepOptions {
    GrepOptions {
        max_matches: SCAN_MATCHES,
        timeout: SCAN_DEADLINE,
        ..GrepOptions::default()
    }
}

/// A file read once: the source and its lines (no `\r`, no final empty line).
#[derive(Debug)]
pub struct FileText {
    pub source: String,
    pub lines: Vec<String>,
}

impl Searcher {
    pub fn new(root: &Path) -> Self {
        Self::with(root, Backend::Live(scan_options()))
    }

    pub fn with(root: &Path, backend: Backend) -> Self {
        Self {
            root: root.to_path_buf(),
            backend,
            names: scan_options(),
            scans: HashMap::new(),
            texts: HashMap::new(),
            corpus: None,
            corpus_load_ms: 0.0,
            incomplete_scans: 0,
            totals: ScanTotals::default(),
            failure: None,
        }
    }

    pub fn options(&self) -> &GrepOptions {
        match &self.backend {
            Backend::Live(options) => options,
            Backend::Rg(_) => &self.names,
        }
    }

    /// Bytes the corpus read (0 before the first live search, and for `Rg`).
    pub fn corpus_bytes(&self) -> u64 {
        self.corpus.as_ref().map_or(0, Corpus::bytes)
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
        self.search_spellings(term_needles(term, original))
    }

    fn search_spellings(&mut self, needles: Vec<String>) -> Rc<Hits> {
        let key = needles.join("\u{0}");
        if let Some(hits) = self.scans.get(&key) {
            return Rc::clone(hits);
        }
        if self.failure.is_some() {
            return Rc::new(Hits::default());
        }
        let mut matches: Vec<LiveMatch> = Vec::new();
        for spelling in &needles {
            match &self.backend {
                Backend::Live(options) => {
                    // The tree is read once, on the first search; every scan
                    // after it replays those bytes.
                    let corpus = self.corpus.get_or_insert_with(|| {
                        let started = Instant::now();
                        let corpus = Corpus::load(&self.root, options);
                        self.corpus_load_ms = started.elapsed().as_secs_f64() * 1000.0;
                        corpus
                    });
                    let result = corpus.scan(spelling, Instant::now() + SCAN_DEADLINE);
                    if matches!(result.stopped, Some("deadline" | "byte cap")) {
                        self.incomplete_scans += 1;
                    }
                    let totals = &mut self.totals;
                    totals.scans += 1;
                    totals.skipped_by_name += result.skipped_by_name;
                    totals.files_read += result.files_read;
                    totals.bytes_read += result.bytes_read;
                    totals.stopped += u64::from(result.stopped.is_some());
                    matches.extend(result.matches);
                }
                Backend::Rg(rg) => match rg.search(&self.root, spelling) {
                    Ok(found) => {
                        self.totals.scans += 1;
                        self.totals.rg_stdout_bytes += found.stdout_bytes;
                        self.totals.rg_stdout_lines += found.stdout_lines;
                        self.totals.rg_ignored_lines += found.ignored_lines;
                        matches.extend(found.matches);
                    }
                    Err(error) => {
                        // Not cached: the run is void, and says why.
                        self.failure = Some(format!("{error:#}"));
                        return Rc::new(Hits::default());
                    }
                },
            }
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
    fn scan_totals_add_up_and_a_skipped_file_is_counted_not_read() {
        let dir = temp_tree(
            "totals",
            &[
                ("a.rs", "fn x() { hotname(); }\n".into()),
                ("Cargo.lock", "name = \"other\"\n".into()),
            ],
        );
        let mut plain = Searcher::new(&dir);
        let plain_hits = plain.search("hotname");
        assert_eq!(plain_hits.files, ["a.rs"]);
        assert_eq!(plain.totals.scans, 1);
        assert_eq!(plain.totals.files_read, 2);
        assert_eq!(plain.totals.skipped_by_name, 0);
        // A cached search is not scanned again.
        plain.search("hotname");
        assert_eq!(plain.totals.scans, 1);

        let options = GrepOptions {
            skip_files: lci_maps::live_grep::LOCKFILES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ..scan_options()
        };
        let mut skipping = Searcher::with(&dir, Backend::Live(options));
        let hits = skipping.search("hotname");
        assert_eq!(hits.matches, plain_hits.matches);
        assert_eq!(skipping.totals.skipped_by_name, 1);
        assert_eq!(skipping.totals.files_read, 1);
        assert!(skipping.totals.bytes_read < plain.totals.bytes_read);
        assert_eq!(skipping.incomplete_scans, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tokens_are_chars_over_four_rounded_up() {
        assert_eq!(tokens_of(""), 0);
        assert_eq!(tokens_of("abcd"), 1);
        assert_eq!(tokens_of("abcde"), 2);
    }
}
