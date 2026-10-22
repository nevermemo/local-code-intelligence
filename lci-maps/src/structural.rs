//! The `grep.structural` map (cycle 1, slice K19): from a symbol, the places
//! where its name occurs in a given syntactic role, found by a text search
//! for the candidate files and confirmed by an ast-grep pattern.
//!
//! # What it does
//!
//! ripgrep names the files that hold the seed's name as a whole word (the
//! [`Candidates`] step); each of those files is parsed once by the chunker's
//! own tree-sitter grammar and searched with the patterns of
//! [`patterns`] (an `impl`, `extends` or `implements` header that names the
//! seed, a call of the seed). A word in a comment or a string matches no
//! pattern, which is the difference from `grep.live`. The cost is one parse
//! per candidate file, so it is bounded by the candidate list, not by the
//! tree, and no server runs.
//!
//! This module holds the matcher, [`scan_files`] over a list of candidate
//! files with its caps and counters, and the map that wraps it,
//! [`Structural`].
//!
//! # What an edge says
//!
//! Witness [`WitnessClass::Exact`] and label [`SourceLabel::Tags`] (the
//! syntax tree, not a compiler): the name is literal in the matched syntax.
//!
//! * [`EdgeKind::Implements`], confidence 70, from a header match
//!   (`impl Trait for Type`, `class C extends Base`, `class C implements
//!   Base`, `class C(Base)`, `class C : Base`): from the seed to the other
//!   party of the header, the symbol of that name when it is declared in the
//!   same file, else the file of the match. Either direction is found, so a
//!   trait seed reaches its implementors and a type seed reaches its traits.
//! * [`EdgeKind::CallsOrRefs`], confidence 55, from a call match: from the
//!   seed (the callee) to the function that encloses the call, else the file.
//!   The receiver's type is not known, so any `x.name()` counts.
//!
//! The site is the match's zero-based line and column, with the live blob of
//! the file. What is not seen: `#[derive]` and macro-generated impls (no
//! header exists in the text), Go and duck-typed languages' interfaces (no
//! header names them), and the trait a bare name stands for: `Display` in a
//! header matches every `Display`, of whichever crate.
//!
//! # Caps
//!
//! At most [`StructuralOptions::max_candidates`] files are looked at (path
//! order), a file above `max_file_bytes` is skipped and counted, the scan
//! stops at the deadline and after `cap` hits. A name that is not a plain
//! identifier makes no scan.

mod lang;
mod patterns;

pub use lang::Lang;
pub use patterns::{Compiled, LANGUAGES, Role, Template, compile, instantiate, patterns};

use crate::{
    Budget, Contribution, Cost, EdgeKind, Evidence, Freshness, FreshnessModel, Map, MapDescriptor,
    MapHit, MapId, MapRequest, MapResponse, MapStatus, Need, Observed, SnapshotView, SourceLabel,
    edge::{Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges},
    impact::code_names,
    live_grep::{CostLog, needle_of},
    lsp_live::SymbolPlaces,
};
use ast_grep_core::{AstGrep, tree_sitter::StrDoc};
use lci_core::{
    language::ADAPTERS,
    lexical,
    node::{NodeId, NodeKind},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// The id of the map.
pub const STRUCTURAL_ID: MapId = "grep.structural";

/// Most files one scan reads: the candidate list is cut here, in path order.
pub const MAX_CANDIDATES: usize = 200;

/// Limits of one scan.
#[derive(Debug, Clone)]
pub struct StructuralOptions {
    /// Candidate files looked at, in path order.
    pub max_candidates: usize,
    /// Files above this size are not read.
    pub max_file_bytes: u64,
    /// The longest one scan runs (the call's own deadline also applies).
    pub timeout: Duration,
    /// Most hits one scan keeps.
    pub cap: usize,
}

impl Default for StructuralOptions {
    fn default() -> Self {
        Self {
            max_candidates: MAX_CANDIDATES,
            max_file_bytes: 1 << 20,
            timeout: Duration::from_secs(3),
            cap: crate::EXPAND_LIMIT,
        }
    }
}

/// The step that proposes files: those that hold `name` as a whole word.
#[crate::async_trait]
pub trait Candidates: Send + Sync {
    /// Paths relative to `root`, `/` separated, distinct, in path order, of
    /// the files with one of `extensions` that hold `name` as a whole word.
    async fn files_with_word(
        &self,
        root: &Path,
        name: &str,
        extensions: &[&str],
    ) -> Result<Vec<String>, String>;
}

/// ripgrep, through `lci_core::lexical::search_literals`
/// (`rg --fixed-strings --word-regexp`).
pub struct Ripgrep {
    path: String,
}

impl Ripgrep {
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
}

/// Lines per file asked of ripgrep: only the first whole-identifier line of a
/// file is needed, but ripgrep's word boundary is looser than the identifier
/// test, so a few lines are asked.
const LINES_PER_FILE: u32 = 5;

#[crate::async_trait]
impl Candidates for Ripgrep {
    async fn files_with_word(
        &self,
        root: &Path,
        name: &str,
        extensions: &[&str],
    ) -> Result<Vec<String>, String> {
        let found = lexical::search_literals(&self.path, root, &[name.to_owned()], LINES_PER_FILE)
            .await
            .map_err(|error| error.to_string())?;
        let mut files: Vec<String> = found
            .matches
            .into_iter()
            .map(|hit| hit.relative_path)
            .filter(|path| {
                let extension = path.rsplit('.').next().unwrap_or_default();
                extensions
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(extension))
            })
            .collect();
        if files.is_empty()
            && let Some(warning) = found.warning
        {
            return Err(warning);
        }
        files.sort();
        files.dedup();
        Ok(files)
    }
}

/// The test double: a fixed list, whatever the name.
pub struct Listed(pub Vec<String>);

#[crate::async_trait]
impl Candidates for Listed {
    async fn files_with_word(
        &self,
        _root: &Path,
        _name: &str,
        _extensions: &[&str],
    ) -> Result<Vec<String>, String> {
        Ok(self.0.clone())
    }
}

/// The file extensions of a language identifier.
pub fn extensions_of(identifier: &str) -> Vec<&'static str> {
    ADAPTERS
        .iter()
        .filter(|adapter| adapter.identifier() == identifier && !adapter.is_prose())
        .map(|adapter| adapter.glob().trim_start_matches("*."))
        .collect()
}

/// The extensions of every language that has patterns.
pub fn all_extensions() -> Vec<&'static str> {
    LANGUAGES
        .iter()
        .flat_map(|identifier| extensions_of(identifier))
        .collect()
}

/// One syntactic match of the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Relative to the root, `/` separated.
    pub path: String,
    pub role: Role,
    /// 0-based line and byte column of the matched node's start, as SCIP
    /// counts.
    pub line: u32,
    pub col: u32,
    /// `live:` and the hex of the first 16 bytes of the SHA-256 of the file's
    /// bytes, as `grep.live` writes it: a blob no snapshot holds.
    pub blob: String,
    /// For a header: the bare name of the other party (the trait or type, the
    /// subclass), read from the pattern's `$T` or `$C`.
    pub other: Option<String>,
}

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// In candidate order, then source order; at most `cap`.
    pub hits: Vec<Hit>,
    /// Candidate files taken from the list (after `max_candidates`).
    pub candidates: u64,
    pub files_parsed: u64,
    pub bytes_parsed: u64,
    pub skipped_large: u64,
    /// Templates that did not compile, once each.
    pub dropped: Vec<String>,
    /// Why the scan ended before the list did, if it did.
    pub stopped: Option<&'static str>,
}

/// The bare name of a type or trait as written in a header: generic
/// arguments, references, paths and qualifiers dropped (`&mut a::Wrapper<T>`
/// gives `Wrapper`). `None` when nothing identifier-like is left.
pub fn bare_name(text: &str) -> Option<String> {
    let head = text.split('<').next().unwrap_or(text);
    let last = head.rsplit(['.', ':']).next().unwrap_or(head);
    let name = last
        .trim()
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .next()
        .unwrap_or_default();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The SHA-256 of `bytes`, first 16 bytes in hex, behind `live:` (the form of
/// `grep.live`'s blobs).
fn live_blob(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(16)
        .fold(String::from("live:"), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

/// Whether `text` is a name worth searching: identifier characters only.
pub fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Parses each of the `candidates` (paths relative to `root`) whose language
/// is `language` (every language with patterns when `None`) and keeps the
/// matches of the patterns for `name`. Blocking. A file that cannot be read,
/// is not UTF-8, or has no grammar is passed over without a count.
/// Deterministic for given bytes: candidate order, then source order.
pub fn scan_files(
    root: &Path,
    candidates: &[String],
    name: &str,
    language: Option<&str>,
    options: &StructuralOptions,
    deadline: Instant,
) -> Scan {
    let mut scan = Scan::default();
    if !is_identifier(name) || options.cap == 0 {
        return scan;
    }
    let taken = candidates.len().min(options.max_candidates);
    scan.candidates = taken as u64;
    let mut compiled: HashMap<&'static str, Compiled> = HashMap::new();
    let mut seen_dropped: HashSet<String> = HashSet::new();
    for path in &candidates[..taken] {
        if Instant::now() >= deadline {
            scan.stopped = Some("deadline");
            break;
        }
        let Some(lang) = Lang::for_path(Path::new(path)) else {
            continue;
        };
        if language.is_some_and(|wanted| wanted != lang.identifier()) {
            continue;
        }
        let full = root.join(path);
        if std::fs::metadata(&full).map_or(0, |meta| meta.len()) > options.max_file_bytes {
            scan.skipped_large += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let set = compiled
            .entry(lang.identifier())
            .or_insert_with(|| Compiled::build(&lang, name));
        for dropped in &set.dropped {
            if seen_dropped.insert(dropped.clone()) {
                scan.dropped.push(dropped.clone());
            }
        }
        scan.files_parsed += 1;
        scan.bytes_parsed += bytes.len() as u64;
        let Ok(tree) = AstGrep::<StrDoc<Lang>>::try_new(text, lang) else {
            continue;
        };
        let root_node = tree.root();
        let blob = live_blob(&bytes);
        let mut found: Vec<Hit> = Vec::new();
        for (role, set) in [(Role::Header, &set.header), (Role::Call, &set.call)] {
            for pattern in set {
                for matched in root_node.find_all(pattern) {
                    let (line, col) = matched.get_node().start_pos().byte_point();
                    let env = matched.get_env();
                    // The other party: the pattern's `$T` or `$C`, else (a C++
                    // base clause, which cannot hold the class) the name of
                    // the class around the match.
                    let other = if role == Role::Header {
                        ["T", "C"]
                            .iter()
                            .find_map(|var| env.get_match(var).map(|node| node.text()))
                            .or_else(|| {
                                matched
                                    .get_node()
                                    .parent()
                                    .and_then(|parent| parent.field("name"))
                                    .map(|name| name.text())
                            })
                            .and_then(|other| bare_name(&other))
                    } else {
                        None
                    };
                    found.push(Hit {
                        path: path.clone(),
                        role,
                        line: line as u32,
                        col: col as u32,
                        blob: blob.clone(),
                        other,
                    });
                }
            }
        }
        found.sort_by(|a, b| {
            (a.line, a.col, a.role, &a.other).cmp(&(b.line, b.col, b.role, &b.other))
        });
        found.dedup();
        for hit in found {
            if scan.hits.len() >= options.cap {
                scan.stopped = Some("match cap");
                return scan;
            }
            scan.hits.push(hit);
        }
    }
    scan
}

/// The edge confidence of a header match: the trait or base name is literal
/// in the header.
const CONF_HEADER: u8 = 70;
/// The edge confidence of a call match: the name only, the receiver's type is
/// not known.
const CONF_CALL: u8 = 55;
/// A name that occurs nowhere, for [`Structural::calibrate`].
const CALIBRATION_NAME: &str = "lci_calibration_probe_zz";

/// Sums of what the scans of one map did, for a report that prices the map.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub scans: u64,
    pub candidates: u64,
    pub files_parsed: u64,
    pub bytes_parsed: u64,
    pub matches: u64,
    pub skipped_large: u64,
}

/// `grep.structural`; see the module notes.
pub struct Structural {
    descriptor: MapDescriptor,
    root: PathBuf,
    candidates: Box<dyn Candidates>,
    places: Arc<dyn SymbolPlaces>,
    options: StructuralOptions,
    log: CostLog,
    totals: Mutex<Totals>,
    notes: Vec<String>,
}

impl Structural {
    pub fn new(
        root: impl Into<PathBuf>,
        candidates: Box<dyn Candidates>,
        places: Arc<dyn SymbolPlaces>,
        options: StructuralOptions,
    ) -> Self {
        // The table is checked against every grammar once, here, so a card
        // can say what a scan would not do.
        let notes = LANGUAGES
            .iter()
            .filter_map(|identifier| Lang::for_identifier(identifier))
            .flat_map(|lang| Compiled::build(&lang, "Sample").dropped)
            .collect();
        Self {
            descriptor: MapDescriptor {
                id: STRUCTURAL_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: true,
                accepts: vec![NodeKind::Symbol],
                emits: vec![NodeKind::Symbol, NodeKind::File],
                freshness: FreshnessModel::Live,
                source_labels: vec![SourceLabel::Tags],
                needs: vec![Need::Ripgrep],
            },
            root: root.into(),
            candidates,
            places,
            options,
            log: CostLog::default(),
            totals: Mutex::new(Totals::default()),
            notes,
        }
    }

    pub fn options(&self) -> &StructuralOptions {
        &self.options
    }

    /// The call times kept so far.
    pub fn log(&self) -> &CostLog {
        &self.log
    }

    /// What the scans of this map did so far.
    pub fn totals(&self) -> Totals {
        *self.totals.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Templates of the table that did not compile for their grammar; empty
    /// when the table is sound (a test pins that it is).
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Records `runs` expansions of a name that occurs nowhere (ripgrep reads
    /// the tree, the dearest candidate step; nothing is parsed), so the card
    /// has a measured median before the first real call. Returns it.
    pub async fn calibrate(&self, runs: usize) -> Option<f64> {
        for _ in 0..runs {
            let deadline = Instant::now() + self.options.timeout;
            let _ = self.search(CALIBRATION_NAME, None, deadline).await;
        }
        self.log.p50()
    }

    /// The candidate step, then the blocking scan, for one name in one
    /// language (every language with patterns when `None`). `Err` when the
    /// candidate step failed (ripgrep missing, or failed with no match).
    async fn search(
        &self,
        name: &str,
        language: Option<&str>,
        deadline: Instant,
    ) -> Result<Scan, String> {
        let started = Instant::now();
        let extensions = match language {
            Some(identifier) => extensions_of(identifier),
            None => all_extensions(),
        };
        // A name that is no identifier, and a language with no pattern (or no
        // grammar): nothing to confirm, ripgrep is not started.
        let has_patterns = LANGUAGES.iter().any(|identifier| {
            language.is_none_or(|wanted| wanted == *identifier)
                && [Role::Header, Role::Call]
                    .iter()
                    .any(|role| !patterns(identifier, *role).is_empty())
        });
        if !is_identifier(name) || extensions.is_empty() || !has_patterns {
            return Ok(Scan::default());
        }
        let files = self
            .candidates
            .files_with_word(&self.root, name, &extensions)
            .await?;
        let (root, options) = (self.root.clone(), self.options.clone());
        let (name, language) = (name.to_owned(), language.map(str::to_owned));
        let scan = tokio::task::spawn_blocking(move || {
            scan_files(
                &root,
                &files,
                &name,
                language.as_deref(),
                &options,
                deadline,
            )
        })
        .await
        .map_err(|error| error.to_string())?;
        self.log.record(started.elapsed().as_secs_f64() * 1000.0);
        let mut totals = self.totals.lock().unwrap_or_else(PoisonError::into_inner);
        totals.scans += 1;
        totals.candidates += scan.candidates;
        totals.files_parsed += scan.files_parsed;
        totals.bytes_parsed += scan.bytes_parsed;
        totals.matches += scan.hits.len() as u64;
        totals.skipped_large += scan.skipped_large;
        Ok(scan)
    }

    fn deadline_of(&self, budget: Budget) -> Instant {
        budget.deadline.min(Instant::now() + self.options.timeout)
    }

    /// The edges a finished scan gives, one per destination and kind (the
    /// first site), sorted and capped. `seed` is the node the name came from;
    /// a name read from a question has none, and its edges leave the file of
    /// the match.
    fn edges_of(&self, seed: Option<&NodeId>, scan: &Scan) -> Vec<Edge> {
        let mut edges: Vec<Edge> = Vec::new();
        for hit in &scan.hits {
            let enclosing = self.places.enclosing(&hit.path, hit.line);
            let (kind, conf, placed) = match hit.role {
                Role::Header => {
                    let other = hit
                        .other
                        .as_deref()
                        .and_then(|name| self.places.defined_in(&hit.path, (0, u32::MAX), name));
                    (EdgeKind::Implements, CONF_HEADER, other)
                }
                // A call inside the seed's own definition is no news.
                Role::Call if enclosing.as_ref() == seed && seed.is_some() => continue,
                Role::Call => (EdgeKind::CallsOrRefs, CONF_CALL, enclosing),
            };
            let Ok(file) = NodeId::file(hit.path.clone()) else {
                continue;
            };
            let dst = placed.unwrap_or_else(|| file.clone());
            let src = seed.cloned().unwrap_or(file);
            if dst == src || edges.iter().any(|e| e.dst == dst && e.kind == kind) {
                continue;
            }
            edges.push(Edge {
                src,
                dst,
                kind,
                map: STRUCTURAL_ID,
                witness: WitnessClass::Exact,
                conf,
                site: Some(Site {
                    path: hit.path.clone(),
                    blob: hit.blob.clone(),
                    line: hit.line,
                    col: hit.col,
                }),
            });
        }
        cap_edges(&mut edges, self.options.cap);
        edges
    }
}

/// The note of a scan that stopped early (the cap of hits is no early stop),
/// with its counters.
fn partial_note(scan: &Scan) -> Option<String> {
    let why = scan.stopped.filter(|why| *why != "match cap")?;
    Some(format!(
        "the scan stopped early: {why} (candidates {}, files_parsed {}, bytes_parsed {}, matches {}, skipped_large {})",
        scan.candidates,
        scan.files_parsed,
        scan.bytes_parsed,
        scan.hits.len(),
        scan.skipped_large
    ))
}

#[crate::async_trait]
impl Map for Structural {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        // What to look for: each seed symbol's name (in its language) and the
        // last segment of each path the text writes in code spans.
        let mut asked: Vec<(String, Option<String>, Option<&NodeId>)> = Vec::new();
        for (seed, _) in request.seeds {
            if let (NodeId::Symbol(key), Some(name)) = (seed, needle_of(seed)) {
                asked.push((name, Some(key.language.clone()), Some(seed)));
            }
        }
        if let Some(text) = request.text {
            for path in code_names(text).0 {
                if let Some(name) = path.into_iter().next_back() {
                    asked.push((name, None, None));
                }
            }
        }
        let deadline = self.deadline_of(request.budget);
        let (mut edges, mut note, mut warning) = (Vec::new(), None, None);
        for (name, language, seed) in &asked {
            match self.search(name, language.as_deref(), deadline).await {
                Ok(scan) => {
                    response.cost.nodes_touched += scan.files_parsed;
                    response.cost.bytes_read += scan.bytes_parsed;
                    response.cost.external_calls += 1;
                    note = note.or_else(|| partial_note(&scan));
                    edges.extend(self.edges_of(*seed, &scan));
                }
                Err(error) => warning = warning.or(Some(error)),
            }
        }
        cap_edges(&mut edges, usize::MAX);
        let mut seen = HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        if note.is_some() || warning.is_some() {
            response.status = MapStatus::Partial { note, warning };
        }
        response.freshness = Freshness {
            observed: Observed::LiveTree,
            ..response.freshness
        };
        response.hits = edges
            .into_iter()
            .zip(1..)
            .map(|(edge, rank)| MapHit {
                node: edge.dst,
                rank,
                score: Some(f32::from(edge.conf) / 100.0),
                evidence: vec![Evidence {
                    via: Some(edge.src),
                    edge: edge.kind,
                    label: SourceLabel::Tags,
                    detail: edge
                        .site
                        .map(|site| format!("{}:{}", site.path, site.line + 1)),
                }],
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        let (NodeId::Symbol(key), Some(name)) = (node, needle_of(node)) else {
            return Expanded::default();
        };
        let started = Instant::now();
        match self
            .search(&name, Some(&key.language), self.deadline_of(budget))
            .await
        {
            Ok(scan) => {
                let cost = Cost {
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    external_calls: 1,
                    nodes_touched: scan.files_parsed,
                    bytes_read: scan.bytes_parsed,
                    ..Cost::default()
                };
                Expanded::new(self.edges_of(Some(node), &scan), cost)
            }
            Err(_) => Expanded::default(),
        }
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.log.p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: Some(Duration::ZERO),
            dependent: false,
            good_for: vec![QuestionType::Impact, QuestionType::WhereTo],
        }
    }
}

#[cfg(test)]
mod tests;
