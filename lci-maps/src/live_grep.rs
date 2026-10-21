//! The `grep.live` map (multimap wave W0a): a search over the CURRENT bytes of
//! a tree. No index and no cache, so an edit is seen by the next call.
//!
//! # What it does
//!
//! From a symbol, file or chunk node it takes one identifier (a symbol's
//! name; the stem of a file's name, a chunk standing for its file) and looks
//! for it in every text file under the root that `.gitignore`, `.ignore` and
//! the git exclude file leave in, in path order. Each occurrence becomes
//! evidence for two kinds of edge, both [`EdgeKind::Mentions`]:
//!
//! * to the file the occurrence is in (always);
//! * to the symbol that encloses the line, when the map was given a
//!   [`SymbolLocator`] (the SCIP graph's definition spans, through
//!   [`ScipSpans`]). An occurrence inside the node's own definition is
//!   dropped; for a file node, an occurrence in the file itself.
//!
//! Each edge carries one [`Site`] (the first occurrence of the best class)
//! and a count-based confidence. Whole-identifier matches (no identifier
//! character on either side) have witness [`WitnessClass::Exact`]; a match
//! inside a longer identifier has [`WitnessClass::Text`]. Exact is no
//! proof of a reference: a word in a comment or a string counts, and M1.2
//! measured bare-name text at 0.034 precision for the weakest class.
//!
//! # Caps and what they do not guarantee
//!
//! At most [`GrepOptions::max_matches`] occurrences per call (exact first),
//! files larger than `max_file_bytes` or not UTF-8 or with a NUL in their
//! first 8 KiB are skipped, the scan stops at `max_scan_bytes` or at the
//! earlier of the call's budget deadline and `timeout`. A scan that stops
//! early is reported as a partial answer by `query` (`expand` has no status
//! and returns what it found). Files whose name is exactly one of
//! `skip_files` are never read and take no match slot; the list is empty by
//! default, and a caller opts in to skipping package-manager lockfiles with
//! [`LOCKFILES`]. Names shorter than `min_name_len` and a stoplist of
//! ubiquitous names (`new`, `default`, ...) produce no edges: they would
//! only return the cap's worth of noise. Which `max_matches` occurrences are
//! kept is a cap in walk (path) order, not a rank. [`GrepOptions::roles`]
//! narrows the walk to files of given [`SourceRole`]s: a file of another role
//! is never yielded, so it is neither read nor counted.
//!
//! # Freshness
//!
//! The model is [`FreshnessModel::Live`]. Every site's `blob` is `live:` and
//! the hex of the first 16 bytes of the SHA-256 of the file's current bytes,
//! so it can never equal a snapshot blob. The pre-merged source keeps a blob
//! a map set and treats a row that asked a `Live` map as always stale
//! ([`crate::traverse::merged::MergedAdjacency::update`]).

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, Freshness, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, Observed, SnapshotView,
    SourceLabel,
    edge::{Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges},
    scip_graph::ScipMap,
};
use ignore::WalkBuilder;
use lci_core::{
    filter::{SourceRole, classify_source_role},
    node::{NodeId, NodeKind},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// The id of [`LiveGrep`].
pub const GREP_LIVE_MAP_ID: MapId = "grep.live";

/// Bytes of a file's start that are searched for a NUL (the binary test).
const BINARY_PROBE: usize = 8192;
/// Observed call times kept for the card's median.
const LOG_LEN: usize = 64;
/// Confidence of a file or symbol whose best occurrence is whole-identifier,
/// and of one that has only substring occurrences, before the count bonus.
const CONF_EXACT: u8 = 60;
const CONF_TEXT: u8 = 20;

/// Names that match everywhere, in any tree: they would fill the cap with
/// noise. A switch of [`GrepOptions`], not a fact about the tree.
const COMMON_NAMES: &[&str] = &[
    "default", "from", "into", "drop", "clone", "hash", "next", "main", "test", "tests", "build",
    "load", "read", "write", "init", "name", "path", "call", "with", "apply", "parse", "find",
    "self", "mod", "lib", "index", "none", "some", "true", "false", "type", "impl", "args", "data",
    "item", "node", "text", "value", "values", "error", "result", "state", "config", "output",
];

/// Package-manager lockfiles: generated, often large, and full of package
/// names that match an identifier without saying anything about the code.
/// Exact names only; there is no `*lock*` rule. Not applied by default: a
/// caller opts in with `skip_files: LOCKFILES.iter().map(..).collect()`.
pub const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "composer.lock",
    "Cargo.lock",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Pipfile.lock",
    "poetry.lock",
    "uv.lock",
    "bun.lock",
    "bun.lockb",
];

#[derive(Debug, Clone)]
pub struct GrepOptions {
    /// Most occurrences one call keeps (exact ones first).
    pub max_matches: usize,
    /// Files above this size are not read.
    pub max_file_bytes: u64,
    /// The scan stops after reading this many bytes.
    pub max_scan_bytes: u64,
    /// The longest one scan runs (the call's own deadline also applies).
    pub timeout: Duration,
    /// Names shorter than this (in bytes) make no edges.
    pub min_name_len: usize,
    /// Names that make no edges.
    pub skip_names: BTreeSet<String>,
    /// Directory names the walk never enters, besides what ignore files and
    /// hidden directories already exclude.
    pub skip_dirs: Vec<String>,
    /// File names (the whole name, not a pattern) the walk never reads.
    /// Empty by default; see [`LOCKFILES`].
    pub skip_files: Vec<String>,
    /// When set, the walk yields only files whose [`classify_source_role`]
    /// (of the path relative to the root) is in the list. `None` (the
    /// default) yields every file.
    pub roles: Option<Vec<SourceRole>>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            max_matches: 50,
            max_file_bytes: 1 << 20,
            max_scan_bytes: 256 << 20,
            timeout: Duration::from_secs(3),
            min_name_len: 4,
            skip_names: COMMON_NAMES.iter().map(|s| (*s).to_owned()).collect(),
            skip_dirs: vec!["target".into(), "node_modules".into()],
            skip_files: Vec::new(),
            roles: None,
        }
    }
}

/// One occurrence of the needle in the current bytes of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveMatch {
    /// Relative to the root, `/` separated.
    pub path: String,
    /// 0-based line and byte column, as SCIP counts.
    pub line: u32,
    pub col: u32,
    /// Whole identifier (no identifier character on either side).
    pub exact: bool,
    /// `live:` and the hex of the first 16 bytes of the SHA-256 of the file.
    pub blob: String,
}

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanResult {
    /// Exact occurrences first, then substring ones, each in walk order;
    /// at most `max_matches` in all.
    pub matches: Vec<LiveMatch>,
    pub files_read: u64,
    pub bytes_read: u64,
    pub skipped_binary: u64,
    pub skipped_large: u64,
    /// Files not read because their name is in `skip_files`.
    pub skipped_by_name: u64,
    /// Why the scan ended before the walk did, if it did.
    pub stopped: Option<&'static str>,
    pub elapsed_ms: f64,
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether `text` is a name worth searching: identifier characters only.
fn is_identifier(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(is_ident)
}

/// The SHA-256 of `bytes`, first 16 bytes in hex, behind `live:`.
fn live_blob(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .take(16)
        .fold(String::from("live:"), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

/// The walk of `scan` and [`Corpus::load`]: one configuration, so the two
/// cannot see different files.
fn walker(root: &Path, options: &GrepOptions) -> ignore::Walk {
    let skip_dirs: HashSet<String> = options.skip_dirs.iter().cloned().collect();
    let roles = options.roles.clone();
    let base = root.to_path_buf();
    WalkBuilder::new(root)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                return !skip_dirs.contains(entry.file_name().to_string_lossy().as_ref());
            }
            match &roles {
                Some(roles) if entry.file_type().is_some_and(|t| t.is_file()) => {
                    relative_path(&base, entry.path())
                        .is_some_and(|path| roles.contains(&classify_source_role(&path)))
                }
                _ => true,
            }
        })
        .build()
}

/// Whether the first [`BINARY_PROBE`] bytes of `bytes` hold a NUL.
fn has_nul_in_probe(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_PROBE)].contains(&0)
}

/// `path` relative to `root`, `/` separated; `None` outside the root.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// Every occurrence of `needle` in a file's text, pushed to `exact` or `text`
/// (each kept to `cap`). Nothing happens when the file does not hold the
/// needle or `path` gives none; `path` is asked only once the needle is found.
fn collect_matches(
    content: &str,
    needle: &str,
    cap: usize,
    path: impl FnOnce() -> Option<String>,
    exact: &mut Vec<LiveMatch>,
    text: &mut Vec<LiveMatch>,
) {
    if !content.contains(needle) {
        return;
    }
    let Some(path) = path() else { return };
    let blob = live_blob(content.as_bytes());
    for (line_no, line) in content.split('\n').enumerate() {
        for (col, _) in line.match_indices(needle) {
            let raw = line.as_bytes();
            let before = col == 0 || !is_ident(raw[col - 1]);
            let after = raw.get(col + needle.len()).is_none_or(|&b| !is_ident(b));
            let found = LiveMatch {
                path: path.clone(),
                line: line_no as u32,
                col: col as u32,
                exact: before && after,
                blob: blob.clone(),
            };
            let bucket = if found.exact { &mut *exact } else { &mut *text };
            if bucket.len() < cap {
                bucket.push(found);
            }
        }
    }
}

/// Searches the current bytes of the tree at `root` for `needle`. Blocking.
/// Deterministic for a given tree: the walk is in path order. [`Corpus`] does
/// the same over bytes read once.
pub fn scan(root: &Path, needle: &str, options: &GrepOptions, deadline: Instant) -> ScanResult {
    let started = Instant::now();
    let mut result = ScanResult::default();
    if needle.is_empty() || options.max_matches == 0 {
        return result;
    }
    let skip_files: HashSet<String> = options.skip_files.iter().cloned().collect();
    let cap = options.max_matches;
    let (mut exact, mut text) = (Vec::new(), Vec::new());
    for entry in walker(root, options) {
        if Instant::now() >= deadline {
            result.stopped = Some("deadline");
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if skip_files.contains(entry.file_name().to_string_lossy().as_ref()) {
            result.skipped_by_name += 1;
            continue;
        }
        let size = entry.metadata().map_or(0, |m| m.len());
        if size > options.max_file_bytes {
            result.skipped_large += 1;
            continue;
        }
        if result.bytes_read + size > options.max_scan_bytes {
            result.stopped = Some("byte cap");
            break;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        result.files_read += 1;
        result.bytes_read += bytes.len() as u64;
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) if !has_nul_in_probe(&bytes) => content,
            _ => {
                result.skipped_binary += 1;
                continue;
            }
        };
        collect_matches(
            content,
            needle,
            cap,
            || relative_path(root, entry.path()),
            &mut exact,
            &mut text,
        );
        if exact.len() >= cap {
            result.stopped = Some("match cap");
            break;
        }
    }
    exact.append(&mut text);
    exact.truncate(cap);
    result.matches = exact;
    result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    result
}

/// What a [`Corpus`] holds of one file.
enum Stored {
    /// Its name is in `skip_files`: counted, never read.
    ByName,
    /// Over `max_file_bytes`: counted, never read.
    Large,
    /// The read failed: a scan passes over it without counting it.
    Unread,
    /// Read, but not UTF-8 or with a NUL in the probe; only the length is kept.
    Binary(u64),
    /// Read and searchable.
    Text(String),
}

/// One file of the walk.
struct Record {
    /// `None` when the entry lies outside the root (a hit in such a file is
    /// passed over, as `scan` does); never so for a walk from the root.
    path: Option<String>,
    /// The size the walk's metadata gave (0 when it gave none).
    size: u64,
    stored: Stored,
}

/// The files of a tree read once, for many scans of the same bytes.
/// [`Corpus::scan`] gives what [`scan`] gives for the tree as it was at
/// [`Corpus::load`], every field but `elapsed_ms`, without walking a
/// directory or reading a file. An edit after the load is not seen.
pub struct Corpus {
    options: GrepOptions,
    records: Vec<Record>,
    bytes: u64,
}

impl Corpus {
    /// Walks `root` as [`scan`] does and reads every file `scan` would read:
    /// not one whose name is in `options.skip_files` or whose size is above
    /// `options.max_file_bytes`. `scan` stops at `max_scan_bytes`; the corpus
    /// reads the whole walk and applies that cap when it scans.
    pub fn load(root: &Path, options: &GrepOptions) -> Corpus {
        let skip_files: HashSet<&str> = options.skip_files.iter().map(String::as_str).collect();
        let mut records = Vec::new();
        let mut total = 0;
        for entry in walker(root, options) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let size = entry.metadata().map_or(0, |m| m.len());
            let stored = if skip_files.contains(entry.file_name().to_string_lossy().as_ref()) {
                Stored::ByName
            } else if size > options.max_file_bytes {
                Stored::Large
            } else if let Ok(bytes) = std::fs::read(entry.path()) {
                let len = bytes.len() as u64;
                total += len;
                match String::from_utf8(bytes) {
                    Ok(content) if !has_nul_in_probe(content.as_bytes()) => Stored::Text(content),
                    _ => Stored::Binary(len),
                }
            } else {
                Stored::Unread
            };
            records.push(Record {
                path: relative_path(root, entry.path()),
                size,
                stored,
            });
        }
        Corpus {
            options: options.clone(),
            records,
            bytes: total,
        }
    }

    /// The options the corpus was loaded (and scans) with.
    pub fn options(&self) -> &GrepOptions {
        &self.options
    }

    /// Files in the walk.
    pub fn files(&self) -> usize {
        self.records.len()
    }

    /// Bytes read at load, binary files included (their text is not kept).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// [`scan`] over the stored files. The deadline is checked once per file,
    /// not per directory entry, so a deadline that passes during the last
    /// non-file entries of the walk is not reported.
    pub fn scan(&self, needle: &str, deadline: Instant) -> ScanResult {
        let started = Instant::now();
        let mut result = ScanResult::default();
        let options = &self.options;
        if needle.is_empty() || options.max_matches == 0 {
            return result;
        }
        let cap = options.max_matches;
        let (mut exact, mut text) = (Vec::new(), Vec::new());
        for record in &self.records {
            if Instant::now() >= deadline {
                result.stopped = Some("deadline");
                break;
            }
            match record.stored {
                Stored::ByName => {
                    result.skipped_by_name += 1;
                    continue;
                }
                Stored::Large => {
                    result.skipped_large += 1;
                    continue;
                }
                _ => {}
            }
            if result.bytes_read + record.size > options.max_scan_bytes {
                result.stopped = Some("byte cap");
                break;
            }
            let content = match &record.stored {
                Stored::Text(content) => content,
                Stored::Binary(len) => {
                    result.files_read += 1;
                    result.bytes_read += len;
                    result.skipped_binary += 1;
                    continue;
                }
                _ => continue,
            };
            result.files_read += 1;
            result.bytes_read += content.len() as u64;
            collect_matches(
                content,
                needle,
                cap,
                || record.path.clone(),
                &mut exact,
                &mut text,
            );
            if exact.len() >= cap {
                result.stopped = Some("match cap");
                break;
            }
        }
        exact.append(&mut text);
        exact.truncate(cap);
        result.matches = exact;
        result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        result
    }
}

/// Finds the symbol that encloses a line of a file, if it knows one.
pub trait SymbolLocator: Send + Sync {
    /// `line` is 0-based, `path` relative to the root with `/`.
    fn symbol_at(&self, path: &str, line: u32) -> Option<NodeId>;
}

/// The definition spans of a [`ScipMap`], read through its public accessors:
/// the innermost definition whose span holds the line.
#[derive(Debug, Clone, Default)]
pub struct ScipSpans {
    /// Per file: (first line, last line, node id), 0-based inclusive.
    by_file: HashMap<String, Vec<(u32, u32, NodeId)>>,
}

impl ScipSpans {
    pub fn from_scip(map: &ScipMap) -> Self {
        let mut by_file: HashMap<String, Vec<(u32, u32, NodeId)>> = HashMap::new();
        for (i, node) in map.graph().nodes.iter().enumerate() {
            by_file
                .entry(node.file.replace('\\', "/"))
                .or_default()
                .push((node.span.0, node.span.1, map.node_id(i).clone()));
        }
        Self { by_file }
    }

    /// Files with at least one span.
    pub fn files(&self) -> usize {
        self.by_file.len()
    }
}

impl SymbolLocator for ScipSpans {
    fn symbol_at(&self, path: &str, line: u32) -> Option<NodeId> {
        self.by_file
            .get(path)?
            .iter()
            .filter(|(from, to, _)| (*from..=*to).contains(&line))
            .min_by(|a, b| {
                (a.1 - a.0, std::cmp::Reverse(a.0), &a.2).cmp(&(
                    b.1 - b.0,
                    std::cmp::Reverse(b.0),
                    &b.2,
                ))
            })
            .map(|(_, _, id)| id.clone())
    }
}

/// The last call times of a map, for the median its card reports.
#[derive(Debug, Default)]
pub struct CostLog {
    times: Mutex<VecDeque<f64>>,
}

impl CostLog {
    pub fn record(&self, elapsed_ms: f64) {
        let mut times = self.times.lock().unwrap_or_else(PoisonError::into_inner);
        if times.len() == LOG_LEN {
            times.pop_front();
        }
        times.push_back(elapsed_ms);
    }

    /// The median (upper, for an even count) of the calls kept.
    pub fn p50(&self) -> Option<f64> {
        let mut sorted: Vec<f64> = self
            .times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .copied()
            .collect();
        sorted.sort_by(f64::total_cmp);
        sorted.get(sorted.len() / 2).copied()
    }

    pub fn len(&self) -> usize {
        self.times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The identifier to look for from `node`, if the node names one.
pub(crate) fn needle_of(node: &NodeId) -> Option<String> {
    let name = match node {
        NodeId::Symbol(key) => key.name.clone(),
        NodeId::File { path } | NodeId::Chunk { path, .. } => {
            let file = path.rsplit('/').next().unwrap_or(path);
            file.split('.').next().unwrap_or(file).to_owned()
        }
        _ => return None,
    };
    is_identifier(&name).then_some(name)
}

/// `grep.live`; see the module notes.
pub struct LiveGrep {
    descriptor: MapDescriptor,
    root: PathBuf,
    options: GrepOptions,
    locator: Option<Arc<dyn SymbolLocator>>,
    log: Arc<CostLog>,
}

impl LiveGrep {
    pub fn new(root: impl Into<PathBuf>, locator: Option<Arc<dyn SymbolLocator>>) -> Self {
        Self::with_options(root, locator, GrepOptions::default())
    }

    pub fn with_options(
        root: impl Into<PathBuf>,
        locator: Option<Arc<dyn SymbolLocator>>,
        options: GrepOptions,
    ) -> Self {
        Self {
            descriptor: MapDescriptor {
                id: GREP_LIVE_MAP_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol, NodeKind::File, NodeKind::Chunk],
                emits: vec![NodeKind::File, NodeKind::Symbol],
                freshness: FreshnessModel::Live,
                source_labels: vec![SourceLabel::Lexical],
                needs: Vec::new(),
            },
            root: root.into(),
            options,
            locator,
            log: Arc::new(CostLog::default()),
        }
    }

    pub fn options(&self) -> &GrepOptions {
        &self.options
    }

    /// The call times kept so far.
    pub fn log(&self) -> &CostLog {
        &self.log
    }

    /// Records `runs` scans for a name that occurs nowhere (the whole tree
    /// is read: the dearest case), so the card has a measured median before
    /// the first real call. Returns that median.
    pub async fn calibrate(&self, runs: usize) -> Option<f64> {
        for _ in 0..runs {
            let (root, options) = (self.root.clone(), self.options.clone());
            let deadline = Instant::now() + options.timeout;
            let measured = tokio::task::spawn_blocking(move || {
                scan(&root, "lci_calibration_probe_zz", &options, deadline)
            })
            .await;
            if let Ok(measured) = measured {
                self.log.record(measured.elapsed_ms);
            }
        }
        self.log.p50()
    }

    /// Scans for what `node` names. `None` when the node names nothing worth
    /// searching.
    async fn search(&self, node: &NodeId, budget: Budget) -> Option<ScanResult> {
        let needle = needle_of(node)?;
        if needle.len() < self.options.min_name_len || self.options.skip_names.contains(&needle) {
            return None;
        }
        let deadline = budget.deadline.min(Instant::now() + self.options.timeout);
        let (root, options) = (self.root.clone(), self.options.clone());
        let result = tokio::task::spawn_blocking(move || scan(&root, &needle, &options, deadline))
            .await
            .ok()?;
        self.log.record(result.elapsed_ms);
        Some(result)
    }

    /// The edges leaving `node` for a finished scan, sorted and capped.
    fn edges_of(&self, node: &NodeId, found: &ScanResult) -> Vec<Edge> {
        #[derive(Default)]
        struct Group {
            exact: Vec<usize>,
            text: Vec<usize>,
        }
        let own_path = node.path();
        let mut files: BTreeMap<&str, Group> = BTreeMap::new();
        let mut symbols: BTreeMap<NodeId, Group> = BTreeMap::new();
        for (i, hit) in found.matches.iter().enumerate() {
            let enclosing = self
                .locator
                .as_ref()
                .and_then(|l| l.symbol_at(&hit.path, hit.line));
            if enclosing.as_ref() == Some(node) || own_path == Some(hit.path.as_str()) {
                continue;
            }
            let bucket = |g: &mut Group| {
                if hit.exact {
                    g.exact.push(i);
                } else {
                    g.text.push(i);
                }
            };
            bucket(files.entry(&hit.path).or_default());
            if let Some(symbol) = enclosing {
                bucket(symbols.entry(symbol).or_default());
            }
        }
        let edge = |dst: NodeId, group: &Group| -> Edge {
            let (first, count, witness, base, step) = match group.exact.first() {
                Some(&first) => (first, group.exact.len(), WitnessClass::Exact, CONF_EXACT, 5),
                None => (
                    group.text[0],
                    group.text.len(),
                    WitnessClass::Text,
                    CONF_TEXT,
                    3,
                ),
            };
            let hit = &found.matches[first];
            Edge {
                src: node.clone(),
                dst,
                kind: EdgeKind::Mentions,
                map: GREP_LIVE_MAP_ID,
                witness,
                conf: base + (count.min(4) as u8 - 1) * step,
                site: Some(Site {
                    path: hit.path.clone(),
                    blob: hit.blob.clone(),
                    line: hit.line,
                    col: hit.col,
                }),
            }
        };
        let mut edges: Vec<Edge> = files
            .iter()
            .filter_map(|(path, group)| Some(edge(NodeId::file(*path).ok()?, group)))
            .chain(symbols.iter().map(|(id, group)| edge(id.clone(), group)))
            .collect();
        cap_edges(&mut edges, EXPAND_LIMIT);
        edges
    }
}

#[crate::async_trait]
impl Map for LiveGrep {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        let mut edges = Vec::new();
        let mut stopped = None;
        for (seed, _) in request.seeds {
            if let Some(found) = self.search(seed, request.budget).await {
                response.cost.elapsed_ms += found.elapsed_ms;
                response.cost.bytes_read += found.bytes_read;
                response.cost.nodes_touched += found.files_read;
                stopped = stopped.or(found.stopped.filter(|why| *why != "match cap"));
                edges.extend(self.edges_of(seed, &found));
            }
        }
        cap_edges(&mut edges, usize::MAX);
        let mut seen = HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        if let Some(why) = stopped {
            response.status = MapStatus::Partial {
                note: Some(format!("the scan stopped early: {why}")),
                warning: None,
            };
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
                    label: SourceLabel::Lexical,
                    detail: edge.site.map(|s| format!("{}:{}", s.path, s.line + 1)),
                }],
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        match self.search(node, budget).await {
            Some(found) => {
                let cost = Cost {
                    elapsed_ms: found.elapsed_ms,
                    nodes_touched: found.files_read,
                    bytes_read: found.bytes_read,
                    ..Cost::default()
                };
                Expanded::new(self.edges_of(node, &found), cost)
            }
            None => Expanded::default(),
        }
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.log.p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: Some(Duration::ZERO),
            dependent: false,
            good_for: vec![QuestionType::WhereTo, QuestionType::Impact],
        }
    }
}

#[cfg(test)]
mod tests;
