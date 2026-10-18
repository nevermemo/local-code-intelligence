//! The deterministic loop of arm A0 (grep-only), optionally behind the one
//! `lci` call of A1 or A2 (an [`Opening`]). No model, no gold: the only inputs
//! are the question and the tree. See `docs/spike/multimap/v2/sim-agent.md`.
//!
//! Arms A1 and A2 start with the opening call (and, for A2, a read of its
//! sites); the names in the text it showed join the frontier, and the chain
//! below then runs unchanged. One question runs as:
//! 1. probe: one grep per probed query term (a call each); a term's rarity is
//!    the number of files it occurs in;
//! 2. search: one grep listing the occurrences of the (up to three) rarest
//!    matching terms together, best files first;
//! 3. read: the top files, each as 40-line windows around the occurrences;
//! 4. follow: identifiers found in the text read, one at a time in the order
//!    the policy gives: a grep for the name, then a read of the first
//!    occurrence in text not read yet (two calls).
//!
//! Every tool output is charged tokens (characters / 4). A call that would go
//! over the call or token budget is not made and the run stops.

use super::follow::{Candidate, Frontier, Policy, identifiers_in, usable_identifier};
use super::score::GoldTarget;
use super::terms::{Hits, Searcher, original_spellings, pick_probe_terms, tokens_of};
use lci_core::lexical::query_terms;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

/// Occurrences shown by a search or follow grep.
const SEARCH_LISTING: usize = 50;
const FOLLOW_LISTING: usize = 20;
/// Files named in a probe's output.
const PROBE_LISTING: usize = 20;
pub const LISTING_LINE_CHARS: usize = 160;

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub policy: Policy,
    /// Tool calls allowed.
    pub max_calls: u32,
    /// Tokens of tool output allowed.
    pub max_tokens: u64,
    /// Query terms probed (the longest ones).
    pub probe_terms: usize,
    /// Rarest matching terms in the first search.
    pub or_terms: usize,
    /// Files read after the first search.
    pub read_files: usize,
    /// Windows read per file.
    pub windows_per_file: usize,
    /// Lines in a window.
    pub window_lines: u32,
    /// Names ranked per read.
    pub max_candidates: usize,
    /// Stop at the first read that hits the gold (gold-aware; a ceiling on
    /// calls to stop, not a gold-blind agent). Off: run to the budget.
    pub stop_at_gold: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            policy: Policy::Greedy,
            max_calls: 25,
            max_tokens: 20_000,
            probe_terms: 6,
            or_terms: 3,
            read_files: 3,
            windows_per_file: 3,
            window_lines: 40,
            max_candidates: 30,
            stop_at_gold: false,
        }
    }
}

impl SimConfig {
    pub fn to_json(&self) -> Value {
        json!({
            "policy": self.policy.name(),
            "max_calls": self.max_calls,
            "max_tokens": self.max_tokens,
            "probe_terms": self.probe_terms,
            "or_terms": self.or_terms,
            "read_files": self.read_files,
            "windows_per_file": self.windows_per_file,
            "window_lines": self.window_lines,
            "max_candidates": self.max_candidates,
            "stop_at_gold": self.stop_at_gold,
        })
    }
}

/// Lines `start..=end` (1-based) of a file the agent read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

/// The first call of arm A1 or A2, made before the A0 chain (built by
/// `arms`; the loop only charges and reads it).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Opening {
    /// `lci-search` (A1) or `lci-traverse` (A2).
    pub kind: &'static str,
    /// The query the tool was given.
    pub arg: String,
    /// What the tool printed, as the agent sees it (charged).
    pub output: String,
    /// The sites the output lists (path, 1-based line).
    pub listed: Vec<(String, u32)>,
    /// Text the output itself shows (A1: the snippet spans).
    pub windows: Vec<Window>,
    /// Files with lines to read as windows after the call, one read call per
    /// file, in order (A2: the sites of the results).
    pub reads: Vec<(String, Vec<u32>)>,
    /// The tool's raw JSON (A2), written to `raw/<id>.json` for `maps compare`.
    /// Not used by the loop.
    pub raw: Option<Value>,
    /// What the run files record about the call (status, stop reason, maps
    /// enabled, flags). Not used by the loop.
    pub info: Option<Value>,
}

/// One tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// 1-based position among the calls.
    pub call: u32,
    /// `lci-search` or `lci-traverse` (arms A1 and A2 only, always the first
    /// call), `site-read` (A2), then `probe`, `search`, `read`, `follow-grep`
    /// or `follow-read`.
    pub kind: &'static str,
    /// The term, name or path the call was about.
    pub arg: String,
    pub tokens: u64,
    pub cumulative_tokens: u64,
    pub depth: u32,
    /// Files holding the name (follow-grep calls only).
    pub rarity: Option<usize>,
    /// What a read call showed.
    pub windows: Vec<Window>,
    /// What the search call listed: (path, 1-based line), in listing order.
    pub listed: Vec<(String, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The call budget is spent.
    Calls,
    /// The next output would pass the token budget.
    Tokens,
    /// Nothing left to follow.
    Exhausted,
    /// No term of the question occurs in the tree.
    NoMatch,
    /// The question has no usable term.
    NoTerms,
    /// Stopped on a read that hit the gold (`stop_at_gold`).
    Gold,
}

impl Stop {
    pub fn name(self) -> &'static str {
        match self {
            Stop::Calls => "calls",
            Stop::Tokens => "tokens",
            Stop::Exhausted => "exhausted",
            Stop::NoMatch => "no-match",
            Stop::NoTerms => "no-terms",
            Stop::Gold => "gold",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trace {
    /// `query_terms` of the question.
    pub terms: Vec<String>,
    pub steps: Vec<Step>,
    pub stop: Stop,
}

impl Trace {
    pub fn tokens_read(&self) -> u64 {
        self.steps.last().map_or(0, |s| s.cumulative_tokens)
    }

    pub fn to_json(&self) -> Value {
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|s| {
                let mut step = json!({
                    "call": s.call, "kind": s.kind, "arg": s.arg,
                    "tokens": s.tokens, "cumulative_tokens": s.cumulative_tokens,
                    "depth": s.depth,
                });
                if let Some(rarity) = s.rarity {
                    step["rarity"] = json!(rarity);
                }
                if !s.windows.is_empty() {
                    step["windows"] = s
                        .windows
                        .iter()
                        .map(|w| json!({"path": w.path, "start": w.start, "end": w.end}))
                        .collect();
                }
                if !s.listed.is_empty() {
                    step["listed"] = s.listed.iter().map(|(p, l)| json!([p, l])).collect();
                }
                step
            })
            .collect();
        json!({ "terms": self.terms, "stop": self.stop.name(), "steps": steps })
    }
}

/// Runs the loop for one question: the A0 chain, behind `opening` (the
/// `lci` call of A1 or A2) when given. `stop_at` is only used with
/// `cfg.stop_at_gold`.
pub fn run_question(
    searcher: &mut Searcher,
    cfg: &SimConfig,
    question: &str,
    stop_at: Option<&GoldTarget>,
    opening: Option<&Opening>,
) -> Trace {
    let terms = query_terms(question);
    let mut run = Run {
        searcher,
        cfg,
        opening,
        stop_at: stop_at.filter(|_| cfg.stop_at_gold),
        steps: Vec::new(),
        tokens: 0,
        covered: Vec::new(),
        seen: HashSet::new(),
        originals: original_spellings(question),
    };
    let stop = match run.go(&terms) {
        Ok(()) => Stop::Exhausted,
        Err(stop) => stop,
    };
    Trace {
        terms,
        steps: run.steps,
        stop,
    }
}

struct Run<'a> {
    searcher: &'a mut Searcher,
    cfg: &'a SimConfig,
    opening: Option<&'a Opening>,
    stop_at: Option<&'a GoldTarget>,
    steps: Vec<Step>,
    tokens: u64,
    /// Windows read so far.
    covered: Vec<Window>,
    /// Lower-cased names already probed, queued or followed.
    seen: HashSet<String>,
    /// Tokens of the question as written, by lower-cased form.
    originals: HashMap<String, String>,
}

/// A 1-based inclusive line range.
type Lines = (u32, u32);

impl Run<'_> {
    fn go(&mut self, terms: &[String]) -> Result<(), Stop> {
        // The opening call (A1, A2) comes first; its names join the frontier.
        // The names of each read stay apart: one candidate limit and one
        // batch per read, as for a follow read.
        let mut reads = match self.opening {
            Some(opening) => self.open(opening)?,
            None => Vec::new(),
        };
        match self.start_chain(terms) {
            Ok(more) => reads.extend(more),
            // The grep chain found nothing, but the opening call showed text:
            // follow the names in it.
            Err(Stop::NoMatch | Stop::NoTerms) if reads.iter().any(|r| !r.is_empty()) => {}
            Err(stop) => return Err(stop),
        }
        // Each arm explores its own evidence first: the batches are in
        // priority order (the opening, its site reads, then the chain's
        // reads), and a name is kept by the first batch that has it. Greedy
        // pushes to the front, so it pushes in reverse and the opening ends
        // on top; bfs pushes in order. Either way the first read is the
        // opening's, and the policies part ways only on later reads.
        let mut batches: Vec<Vec<Candidate>> = reads
            .into_iter()
            .map(|names| self.batch(names, 1))
            .collect();
        if self.cfg.policy == Policy::Greedy {
            batches.reverse();
        }
        let mut frontier = Frontier::new(self.cfg.policy);
        for batch in batches {
            frontier.push_batch(batch);
        }

        // Follow names until the budget or the frontier runs out.
        while let Some(candidate) = frontier.pop() {
            let hits = self.searcher.search(&candidate.name);
            let Some(target) = hits
                .matches
                .iter()
                .find(|m| !self.is_covered(&m.path, m.line + 1))
            else {
                continue;
            };
            let listed: Vec<(String, u32)> = hits
                .matches
                .iter()
                .take(FOLLOW_LISTING)
                .map(|m| (m.path.clone(), m.line + 1))
                .collect();
            let output = self.listing_text(&listed);
            self.charge(
                "follow-grep",
                &candidate.name,
                &output,
                candidate.depth,
                Vec::new(),
                Vec::new(),
            )?;
            if let Some(step) = self.steps.last_mut() {
                step.rarity = Some(candidate.rarity);
            }
            let total = self.searcher.text(&target.path).lines.len() as u32;
            let windows = self.windows(total, &[target.line + 1]);
            let names = self.read("follow-read", &target.path, &windows, candidate.depth)?;
            frontier.push_batch(self.batch(names, candidate.depth + 1));
        }
        Ok(())
    }

    /// The opening call of A1 or A2, then the reads of its sites; returns the
    /// identifier names in the text shown, one list for the call's own text
    /// and one for each site read.
    fn open(&mut self, opening: &Opening) -> Result<Vec<Vec<String>>, Stop> {
        self.charge(
            opening.kind,
            &opening.arg,
            &opening.output,
            0,
            opening.windows.clone(),
            opening.listed.clone(),
        )?;
        let mut reads = vec![self.note_read(opening.windows.clone())?];
        let mut files = 0;
        for (path, lines) in &opening.reads {
            if files >= self.cfg.read_files {
                break;
            }
            let total = self.searcher.text(path).lines.len() as u32;
            // A site outside the file (or in a file that is not there) is
            // nothing to read, and costs no call.
            let inside: Vec<u32> = lines
                .iter()
                .copied()
                .filter(|line| (1..=total).contains(line))
                .collect();
            if inside.is_empty() {
                continue;
            }
            files += 1;
            let mut windows = self.windows(total, &inside);
            windows.truncate(self.cfg.windows_per_file);
            reads.push(self.read("site-read", path, &windows, 0)?);
        }
        Ok(reads)
    }

    /// Probe, first search and first reads of the A0 chain; returns the
    /// identifier names in the text of each read.
    fn start_chain(&mut self, terms: &[String]) -> Result<Vec<Vec<String>>, Stop> {
        if terms.is_empty() {
            return Err(Stop::NoTerms);
        }
        let mut found: Vec<(String, Rc<Hits>)> = Vec::new();
        for term in pick_probe_terms(terms, self.cfg.probe_terms) {
            let original = self.originals.get(&term).cloned();
            let hits = self.searcher.search_term(&term, original.as_deref());
            let output = probe_output(&term, &hits);
            self.charge("probe", &term, &output, 0, Vec::new(), Vec::new())?;
            self.seen.insert(term.to_lowercase());
            if !hits.files.is_empty() {
                found.push((term, hits));
            }
        }
        if found.is_empty() {
            return Err(Stop::NoMatch);
        }
        // Rarest first; equally rare, the longer term; then probe order.
        found.sort_by(|a, b| {
            a.1.files
                .len()
                .cmp(&b.1.files.len())
                .then(b.0.chars().count().cmp(&a.0.chars().count()))
        });
        found.truncate(self.cfg.or_terms);

        // The first search: the occurrences of the rarest terms together.
        let mut per_file: BTreeMap<&str, (usize, Vec<u32>)> = BTreeMap::new();
        for (_, hits) in &found {
            let mut in_file: Vec<&str> = Vec::new();
            for m in &hits.matches {
                let entry = per_file.entry(&m.path).or_default();
                if !in_file.contains(&m.path.as_str()) {
                    in_file.push(&m.path);
                    entry.0 += 1;
                }
                entry.1.push(m.line + 1);
            }
        }
        let mut ranked: Vec<(&str, usize, Vec<u32>)> = per_file
            .into_iter()
            .map(|(path, (terms_hit, mut lines))| {
                lines.sort_unstable();
                lines.dedup();
                (path, terms_hit, lines)
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(b.2.len().cmp(&a.2.len()))
                .then(a.0.cmp(b.0))
        });
        let listed: Vec<(String, u32)> = ranked
            .iter()
            .flat_map(|(path, _, lines)| lines.iter().map(move |l| ((*path).to_owned(), *l)))
            .take(SEARCH_LISTING)
            .collect();
        let query: Vec<&str> = found.iter().map(|(t, _)| t.as_str()).collect();
        let output = self.listing_text(&listed);
        self.charge("search", &query.join(" | "), &output, 0, Vec::new(), listed)?;

        // The first reads.
        let to_read: Vec<(String, Vec<u32>)> = ranked
            .iter()
            .take(self.cfg.read_files)
            .map(|(path, _, lines)| ((*path).to_owned(), lines.clone()))
            .collect();
        let mut reads: Vec<Vec<String>> = Vec::new();
        for (path, lines) in to_read {
            let total = self.searcher.text(&path).lines.len() as u32;
            // Text the opening call already showed is not read again; a file
            // with nothing new left costs no call (as follow targets do).
            let mut windows = self.windows(total, &lines);
            windows.retain(|&(start, end)| !self.is_range_covered(&path, start, end));
            windows.truncate(self.cfg.windows_per_file);
            if windows.is_empty() {
                continue;
            }
            reads.push(self.read("read", &path, &windows, 0)?);
        }
        Ok(reads)
    }

    /// Whether every line of `start..=end` of `path` was already shown.
    fn is_range_covered(&self, path: &str, start: u32, end: u32) -> bool {
        let mut next = start;
        let mut spans: Vec<(u32, u32)> = self
            .covered
            .iter()
            .filter(|w| w.path == path)
            .map(|w| (w.start, w.end))
            .collect();
        spans.sort_unstable();
        for (a, b) in spans {
            if a > next {
                break;
            }
            next = next.max(b.saturating_add(1));
            if next > end {
                return true;
            }
        }
        next > end
    }

    /// Records one call, or says why it cannot be made.
    fn charge(
        &mut self,
        kind: &'static str,
        arg: &str,
        output: &str,
        depth: u32,
        windows: Vec<Window>,
        listed: Vec<(String, u32)>,
    ) -> Result<(), Stop> {
        let cost = tokens_of(output);
        if self.steps.len() as u32 >= self.cfg.max_calls {
            return Err(Stop::Calls);
        }
        if self.tokens + cost > self.cfg.max_tokens {
            return Err(Stop::Tokens);
        }
        self.tokens += cost;
        self.steps.push(Step {
            call: self.steps.len() as u32 + 1,
            kind,
            arg: arg.to_owned(),
            tokens: cost,
            cumulative_tokens: self.tokens,
            depth,
            rarity: None,
            windows,
            listed,
        });
        Ok(())
    }

    /// One read call over the windows of a file; returns the identifier names
    /// in the text shown.
    fn read(
        &mut self,
        kind: &'static str,
        path: &str,
        windows: &[Lines],
        depth: u32,
    ) -> Result<Vec<String>, Stop> {
        let text = self.searcher.text(path);
        let mut output = String::new();
        for &(start, end) in windows {
            output.push_str(&format!("== {path} lines {start}-{end} ==\n"));
            let high = (end as usize).min(text.lines.len());
            let low = ((start - 1) as usize).min(high);
            for line in &text.lines[low..high] {
                output.push_str(line);
                output.push('\n');
            }
        }
        let read: Vec<Window> = windows
            .iter()
            .map(|&(start, end)| Window {
                path: path.to_owned(),
                start,
                end,
            })
            .collect();
        self.charge(kind, path, &output, depth, read.clone(), Vec::new())?;
        self.note_read(read)
    }

    /// Takes in text the agent has been shown: returns its identifier names,
    /// marks it covered, and ends the run if it hit the gold (`stop_at_gold`).
    fn note_read(&mut self, read: Vec<Window>) -> Result<Vec<String>, Stop> {
        let mut names = Vec::new();
        for w in &read {
            let text = self.searcher.text(&w.path);
            names.extend(identifiers_in(&w.path, &text.source, w.start, w.end));
        }
        let gold = self
            .stop_at
            .is_some_and(|target| read.iter().any(|w| target.hits(&w.path, w.start, w.end)));
        self.covered.extend(read);
        if gold { Err(Stop::Gold) } else { Ok(names) }
    }

    /// Windows of `window_lines` lines around each 1-based line, overlapping
    /// ones merged, in line order.
    fn windows(&self, total: u32, lines: &[u32]) -> Vec<Lines> {
        let before = (self.cfg.window_lines / 2).saturating_sub(1);
        let after = self.cfg.window_lines / 2;
        let mut sorted = lines.to_vec();
        sorted.sort_unstable();
        let mut out: Vec<Lines> = Vec::new();
        for line in sorted {
            let start = line.saturating_sub(before).max(1);
            let end = (line + after).min(total.max(1));
            match out.last_mut() {
                Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
                _ => out.push((start, end)),
            }
        }
        out
    }

    fn is_covered(&self, path: &str, line: u32) -> bool {
        self.covered
            .iter()
            .any(|w| w.path == path && w.start <= line && line <= w.end)
    }

    /// The names worth following among `names`, rarest first.
    fn batch(&mut self, names: Vec<String>, depth: u32) -> Vec<Candidate> {
        let mut fresh: Vec<String> = Vec::new();
        for name in names {
            let key = name.to_lowercase();
            // `seen` is case-insensitive, so `fresh` is too: `Foo` and `foo`
            // in one read take one candidate slot, not two.
            if usable_identifier(&name, self.searcher.options())
                && !self.seen.contains(&key)
                && !fresh.iter().any(|f| f.to_lowercase() == key)
            {
                fresh.push(name);
            }
            if fresh.len() >= self.cfg.max_candidates {
                break;
            }
        }
        let mut ranked: Vec<(usize, usize, String)> = Vec::new();
        for (order, name) in fresh.into_iter().enumerate() {
            let rarity = self.searcher.search(&name).files.len();
            self.seen.insert(name.to_lowercase());
            if rarity > 0 {
                ranked.push((rarity, order, name));
            }
        }
        ranked.sort();
        ranked
            .into_iter()
            .map(|(rarity, _, name)| Candidate {
                name,
                rarity,
                depth,
            })
            .collect()
    }

    /// What a grep prints: `path:line:text` for each occurrence.
    fn listing_text(&mut self, listed: &[(String, u32)]) -> String {
        let mut out = String::new();
        for (path, line) in listed {
            let text = self.searcher.text(path);
            let shown: String = text
                .lines
                .get(*line as usize - 1)
                .map(|l| l.trim().chars().take(LISTING_LINE_CHARS).collect())
                .unwrap_or_default();
            out.push_str(&format!("{path}:{line}:{shown}\n"));
        }
        out
    }
}

/// What a probe prints: the files holding the term with their occurrence
/// counts, the first few.
fn probe_output(term: &str, hits: &Hits) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for m in &hits.matches {
        match counts.iter_mut().find(|(p, _)| *p == m.path) {
            Some((_, n)) => *n += 1,
            None => counts.push((&m.path, 1)),
        }
    }
    let mut out = format!("{term}: {} files\n", counts.len());
    for (path, n) in counts.iter().take(PROBE_LISTING) {
        out.push_str(&format!("{path}:{n}\n"));
    }
    out
}
