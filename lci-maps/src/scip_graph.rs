//! A typed graph over function and method symbols, loaded from a SCIP index
//! (slice M0.4 of the multimap spike v2; pre-registration in
//! `docs/spike/multimap/v2/m0.4-prereg.md`).
//!
//! Nodes are the symbols whose SCIP descriptor ends in `().` (functions and
//! methods). Edges:
//!
//! * `calls_or_refs`: a reference occurrence of a function or method symbol
//!   inside the innermost enclosing range of another such symbol. Edges are
//!   per (caller, callee) pair; the witness is the first occurrence by
//!   (file, line, column) and `sites` counts all of them.
//! * `contains`: the innermost strictly enclosing function range of a nested
//!   function (immediate parent only).
//!
//! An edge carries kind, provenance, confidence and a witness. There is no
//! stored rank weight: any ordering is computed by a scorer at walk time.
//!
//! Attribution follows the gold set's reader (`flow.md` section 4): the
//! innermost range among function, type and macro definitions, and a repair
//! for macro-expanded impl blocks (`#[async_trait]`, `#[tool_router]`) whose
//! functions carry the range of the whole block. The repair needs the source
//! text of the indexed tree; without it those functions keep their wide
//! range and the build report counts them.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, SnapshotView, SourceLabel,
    edge::{
        CONF_MAX, Edge as TypedEdge, Expanded, Site, WitnessClass, cap_edges, conf_from_unit,
        sort_edges,
    },
};
use lci_core::node::{NodeId, NodeKind, SymbolKey, SymbolKind, rust_module_path};
use protobuf::Message;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

/// `(start line, start column, end line, end column)`, zero-based as in SCIP.
pub type Range = (u32, u32, u32, u32);

/// SCIP `SymbolRole` bits used here.
const ROLE_DEFINITION: i32 = 1;
const ROLE_GENERATED: i32 = 16;

/// One occurrence, as read from the index.
#[derive(Debug, Clone)]
pub struct RawOccurrence {
    pub range: Range,
    pub symbol: String,
    pub roles: i32,
    pub enclosing: Option<Range>,
}

/// One indexed document.
#[derive(Debug, Clone)]
pub struct RawDocument {
    pub path: String,
    pub occurrences: Vec<RawOccurrence>,
}

/// Source text of the indexed tree, for the enclosing-range repair.
pub trait SourceText {
    fn read(&self, relative_path: &str) -> Option<Vec<u8>>;
}

/// Source text under a directory (a checkout of the indexed commit).
pub struct DirSource(pub PathBuf);

impl SourceText for DirSource {
    fn read(&self, relative_path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.0.join(relative_path)).ok()
    }
}

fn normalize_range(raw: &[i32]) -> Option<Range> {
    let at = |i: usize| u32::try_from(raw[i]).ok();
    match raw.len() {
        3 => Some((at(0)?, at(1)?, at(0)?, at(2)?)),
        4 => Some((at(0)?, at(1)?, at(2)?, at(3)?)),
        _ => None,
    }
}

/// Reads the documents and occurrences of a SCIP index file.
pub fn read_documents(path: &Path) -> Result<Vec<RawDocument>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let index = scip::types::Index::parse_from_bytes(&bytes)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    let mut documents = Vec::with_capacity(index.documents.len());
    for document in &index.documents {
        let mut occurrences = Vec::with_capacity(document.occurrences.len());
        for occurrence in &document.occurrences {
            let Some(range) = normalize_range(&occurrence.range) else {
                continue;
            };
            occurrences.push(RawOccurrence {
                range,
                symbol: occurrence.symbol.clone(),
                roles: occurrence.symbol_roles,
                enclosing: normalize_range(&occurrence.enclosing_range),
            });
        }
        documents.push(RawDocument {
            path: document.relative_path.replace('\\', "/"),
            occurrences,
        });
    }
    Ok(documents)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GraphEdgeKind {
    CallsOrRefs,
    Contains,
}

impl GraphEdgeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CallsOrRefs => "calls_or_refs",
            Self::Contains => "contains",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provenance {
    /// A reference occurrence in the SCIP index.
    Scip,
    /// Derived from nesting of enclosing ranges.
    Structure,
}

impl Provenance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scip => "scip",
            Self::Structure => "structure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub to: usize,
    pub kind: GraphEdgeKind,
    pub provenance: Provenance,
    /// 1.0, or 0.5 when every site of the edge is macro-generated.
    pub conf: f32,
    /// Reference occurrences behind the edge (1 for `contains`).
    pub sites: u32,
    pub witness: Witness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnKind {
    Function,
    Method,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub symbol: String,
    /// `Owner::name`, where the owner is the impl type, trait or module.
    pub name: String,
    pub kind: FnKind,
    pub file: String,
    /// The line of the name (0-based, as SCIP counts).
    pub line: u32,
    /// The column of the name (0-based).
    pub col: u32,
    /// The first and last line (0-based, inclusive) of the definition's
    /// enclosing range, after the repair; the name line alone when the index
    /// gave none.
    pub span: (u32, u32),
}

/// Counts from loading, for the evaluation's report.
#[derive(Debug, Clone, Default)]
pub struct BuildReport {
    pub documents: usize,
    pub definitions: usize,
    pub function_definitions: usize,
    /// Functions whose enclosing range was replaced by the repair.
    pub repaired: usize,
    /// Functions flagged for repair but left alone (no source, or the scan
    /// found no body).
    pub repair_skipped: usize,
    /// Reference occurrences to indexed symbols outside any definition.
    pub unattributed_sites: usize,
    pub attributed_sites: usize,
}

#[derive(Debug, Clone)]
pub struct ScipGraph {
    pub nodes: Vec<Node>,
    /// Out-edges per node, in source order of the witness.
    pub out: Vec<Vec<Edge>>,
    /// Distinct callers or parents per node.
    pub indegree: Vec<u32>,
    pub report: BuildReport,
    index: HashMap<String, usize>,
}

impl ScipGraph {
    /// Loads the index at `path`; `source` enables the enclosing-range repair.
    pub fn load(path: &Path, source: Option<&dyn SourceText>) -> Result<Self, String> {
        Ok(Self::build(&read_documents(path)?, source))
    }

    pub fn node_index(&self, symbol: &str) -> Option<usize> {
        self.index.get(symbol).copied()
    }

    pub fn edge_count(&self, kind: GraphEdgeKind) -> usize {
        self.out
            .iter()
            .flatten()
            .filter(|edge| edge.kind == kind)
            .count()
    }

    pub fn has_edge(&self, from: usize, to: usize, kind: GraphEdgeKind) -> bool {
        self.out[from]
            .iter()
            .any(|edge| edge.to == to && edge.kind == kind)
    }

    pub fn build(documents: &[RawDocument], source: Option<&dyn SourceText>) -> Self {
        let mut report = BuildReport {
            documents: documents.len(),
            ..BuildReport::default()
        };
        let mut defs = collect_definitions(documents);
        report.definitions = defs.len();
        report.function_definitions = defs.iter().filter(|d| d.is_function()).count();
        repair_enclosing(&mut defs, source, &mut report);

        // Innermost enclosing function, type or macro definition per file.
        let mut by_file: HashMap<&str, Vec<(Range, usize)>> = HashMap::new();
        for (i, def) in defs.iter().enumerate() {
            if let (Some(enc), true) = (def.enclosing, def.is_attributable()) {
                by_file.entry(def.path.as_str()).or_default().push((enc, i));
            }
        }
        let def_index: HashMap<&str, usize> = defs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.symbol.as_str(), i))
            .collect();

        let mut pairs: HashMap<(usize, usize), PairSites> = HashMap::new();
        for document in documents {
            let scope = by_file.get(document.path.as_str());
            for occ in &document.occurrences {
                if occ.roles & ROLE_DEFINITION != 0 || occ.symbol.starts_with("local ") {
                    continue;
                }
                let Some(&target) = def_index.get(occ.symbol.as_str()) else {
                    continue;
                };
                let caller = scope.and_then(|s| innermost(s, occ.range.0, occ.range.1));
                let Some(caller) = caller else {
                    report.unattributed_sites += 1;
                    continue;
                };
                report.attributed_sites += 1;
                if caller == target || !defs[caller].is_function() || !defs[target].is_function() {
                    continue;
                }
                let witness = Witness {
                    file: document.path.clone(),
                    line: occ.range.0,
                    col: occ.range.1,
                };
                let entry = pairs.entry((caller, target)).or_insert_with(|| PairSites {
                    sites: 0,
                    all_generated: true,
                    first: witness.clone(),
                });
                entry.sites += 1;
                entry.all_generated &= occ.roles & ROLE_GENERATED != 0;
                if (&witness.file, witness.line, witness.col)
                    < (&entry.first.file, entry.first.line, entry.first.col)
                {
                    entry.first = witness;
                }
            }
        }

        let mut nodes = Vec::new();
        let mut node_of_def: HashMap<usize, usize> = HashMap::new();
        for (i, def) in defs.iter().enumerate() {
            if def.is_function() {
                let (name, kind) = describe(&def.symbol);
                node_of_def.insert(i, nodes.len());
                nodes.push(Node {
                    symbol: def.symbol.clone(),
                    name,
                    kind,
                    file: def.path.clone(),
                    line: def.name_range.0,
                    col: def.name_range.1,
                    span: def
                        .enclosing
                        .map_or((def.name_range.0, def.name_range.0), |enc| (enc.0, enc.2)),
                });
            }
        }
        let mut out: Vec<Vec<Edge>> = vec![Vec::new(); nodes.len()];
        for ((caller, target), sites) in pairs {
            out[node_of_def[&caller]].push(Edge {
                to: node_of_def[&target],
                kind: GraphEdgeKind::CallsOrRefs,
                provenance: Provenance::Scip,
                conf: if sites.all_generated { 0.5 } else { 1.0 },
                sites: sites.sites,
                witness: sites.first,
            });
        }
        add_contains_edges(&defs, &node_of_def, &nodes, &mut out);
        for edges in &mut out {
            edges.sort_by(|a, b| {
                (
                    &a.witness.file,
                    a.witness.line,
                    a.witness.col,
                    a.to,
                    a.kind.as_str(),
                )
                    .cmp(&(
                        &b.witness.file,
                        b.witness.line,
                        b.witness.col,
                        b.to,
                        b.kind.as_str(),
                    ))
            });
        }
        let mut callers: Vec<HashSet<usize>> = vec![HashSet::new(); nodes.len()];
        for (from, edges) in out.iter().enumerate() {
            for edge in edges {
                callers[edge.to].insert(from);
            }
        }
        let indegree = callers.iter().map(|c| c.len() as u32).collect();
        let index = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.symbol.clone(), i))
            .collect();
        Self {
            nodes,
            out,
            indegree,
            report,
            index,
        }
    }
}

struct PairSites {
    sites: u32,
    all_generated: bool,
    first: Witness,
}

pub struct Def {
    pub symbol: String,
    pub path: String,
    pub name_range: Range,
    pub enclosing: Option<Range>,
}

impl Def {
    fn is_function(&self) -> bool {
        self.symbol.ends_with("().")
    }

    /// Functions, types (`#`) and macros (`!`) take references as callers.
    fn is_attributable(&self) -> bool {
        self.is_function() || self.symbol.ends_with('!') || self.symbol.ends_with('#')
    }
}

/// First definition of every non-local symbol, in index order.
pub(crate) fn collect_definitions(documents: &[RawDocument]) -> Vec<Def> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut defs = Vec::new();
    for document in documents {
        for occ in &document.occurrences {
            if occ.roles & ROLE_DEFINITION != 0
                && !occ.symbol.starts_with("local ")
                && seen.insert(occ.symbol.as_str())
            {
                defs.push(Def {
                    symbol: occ.symbol.clone(),
                    path: document.path.clone(),
                    name_range: occ.range,
                    enclosing: occ.enclosing,
                });
            }
        }
    }
    defs
}

fn start(r: Range) -> (u32, u32) {
    (r.0, r.1)
}

fn end(r: Range) -> (u32, u32) {
    (r.2, r.3)
}

fn contains_pos(r: Range, line: u32, col: u32) -> bool {
    start(r) <= (line, col) && (line, col) <= end(r)
}

/// The smallest range holding the position (by lines, then columns); the
/// first of equals wins.
pub(crate) fn innermost(scope: &[(Range, usize)], line: u32, col: u32) -> Option<usize> {
    let mut best: Option<((u32, i64), usize)> = None;
    for &(enc, def) in scope {
        if contains_pos(enc, line, col) {
            let size = (enc.2 - enc.0, i64::from(enc.3) - i64::from(enc.1));
            if best.is_none_or(|(b, _)| size < b) {
                best = Some((size, def));
            }
        }
    }
    best.map(|(_, def)| def)
}

/// A function whose range equals another function's range in the same file,
/// or strictly holds one, is a macro-expanded impl member; its range is
/// replaced by the span from its name line to the end of its body.
pub(crate) fn repair_enclosing(
    defs: &mut [Def],
    source: Option<&dyn SourceText>,
    report: &mut BuildReport,
) {
    let mut per_file: HashMap<String, Vec<(Range, usize)>> = HashMap::new();
    for (i, def) in defs.iter().enumerate() {
        if let (true, Some(enc)) = (def.is_function(), def.enclosing) {
            per_file.entry(def.path.clone()).or_default().push((enc, i));
        }
    }
    let mut flagged: Vec<usize> = Vec::new();
    for items in per_file.values() {
        for &(e1, s1) in items {
            for &(e2, s2) in items {
                if s1 != s2 && e1 != e2 && start(e1) <= start(e2) && end(e2) <= end(e1) {
                    flagged.push(s1);
                }
            }
        }
        let mut by_range: HashMap<Range, Vec<usize>> = HashMap::new();
        for &(e, s) in items {
            by_range.entry(e).or_default().push(s);
        }
        for group in by_range.values().filter(|g| g.len() > 1) {
            flagged.extend(group);
        }
    }
    flagged.sort_unstable();
    flagged.dedup();
    let mut cache: HashMap<String, Option<Vec<Vec<u8>>>> = HashMap::new();
    for i in flagged {
        let lines = cache.entry(defs[i].path.clone()).or_insert_with(|| {
            source
                .and_then(|s| s.read(&defs[i].path))
                .map(|bytes| bytes.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect())
        });
        let repaired = lines
            .as_deref()
            .and_then(|lines| body_range(lines, defs[i].name_range));
        match repaired {
            Some(new) if Some(new) != defs[i].enclosing => {
                defs[i].enclosing = Some(new);
                report.repaired += 1;
            }
            Some(_) => {}
            None => report.repair_skipped += 1,
        }
    }
}

/// The code bytes (outside comments, strings and char literals) after a
/// position, with their coordinates. Strings spanning lines are not tracked,
/// like the gold set's scanner.
pub(crate) struct Structural<'a> {
    pub(crate) lines: &'a [Vec<u8>],
    pub(crate) line: usize,
    pub(crate) col: usize,
    pub(crate) block_depth: u32,
}

fn char_literal_len(text: &[u8], i: usize) -> Option<usize> {
    match *text.get(i + 1)? {
        b'\\' => {
            let rest = text.get(i + 2..)?;
            let quote = rest.iter().position(|&b| b == b'\'')?;
            Some(2 + quote + 1)
        }
        b'\'' => None,
        _ => (text.get(i + 2) == Some(&b'\'')).then_some(3),
    }
}

fn raw_string_end(text: &[u8], i: usize) -> Option<usize> {
    // `r`, hashes, `"` ... `"`, same hashes; not preceded by an identifier byte.
    if i > 0 && (text[i - 1].is_ascii_alphanumeric() || text[i - 1] == b'_') {
        return None;
    }
    let hashes = text[i + 1..].iter().take_while(|&&b| b == b'#').count();
    if text.get(i + 1 + hashes) != Some(&b'"') {
        return None;
    }
    let body = i + 2 + hashes;
    let mut closing = vec![b'"'];
    closing.extend(std::iter::repeat_n(b'#', hashes));
    let found = text[body.min(text.len())..]
        .windows(closing.len())
        .position(|w| w == closing.as_slice());
    Some(found.map_or(text.len(), |p| body + p + closing.len()))
}

impl Iterator for Structural<'_> {
    type Item = (usize, usize, u8);

    fn next(&mut self) -> Option<Self::Item> {
        while self.line < self.lines.len() {
            let text = &self.lines[self.line];
            while self.col < text.len() {
                let i = self.col;
                let two = text.get(i..i + 2).unwrap_or(&[]);
                let ch = text[i];
                if self.block_depth > 0 {
                    if two == b"*/" {
                        self.block_depth -= 1;
                        self.col += 2;
                    } else if two == b"/*" {
                        self.block_depth += 1;
                        self.col += 2;
                    } else {
                        self.col += 1;
                    }
                    continue;
                }
                if two == b"//" {
                    break;
                }
                if two == b"/*" {
                    self.block_depth += 1;
                    self.col += 2;
                    continue;
                }
                if ch == b'"' {
                    let mut j = i + 1;
                    while j < text.len() && text[j] != b'"' {
                        j += if text[j] == b'\\' { 2 } else { 1 };
                    }
                    self.col = j + 1;
                    continue;
                }
                if ch == b'r'
                    && let Some(next) = raw_string_end(text, i)
                {
                    self.col = next;
                    continue;
                }
                if ch == b'\'' {
                    self.col += char_literal_len(text, i).unwrap_or(1);
                    continue;
                }
                self.col += 1;
                return Some((self.line, i, ch));
            }
            self.line += 1;
            self.col = 0;
        }
        None
    }
}

/// From the name's line to the closing brace (or `;`) of the item's body.
fn body_range(lines: &[Vec<u8>], name: Range) -> Option<Range> {
    let (name_line, name_col) = (name.2 as usize, name.3 as usize);
    let scan = Structural {
        lines,
        line: name_line,
        col: name_col,
        block_depth: 0,
    };
    let (mut paren, mut depth) = (0i32, 0i32);
    for (l, c, ch) in scan {
        let done = (name.2, 0, l as u32, c as u32 + 1);
        if depth == 0 {
            match ch {
                b'(' | b'[' => paren += 1,
                b')' | b']' => paren -= 1,
                b';' if paren == 0 => return Some(done),
                b'{' if paren == 0 => depth = 1,
                _ => {}
            }
        } else if ch == b'{' {
            depth += 1;
        } else if ch == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(done);
            }
        }
    }
    None
}

/// The immediate parent of each function whose range strictly holds the
/// function's range, as a `contains` edge.
fn add_contains_edges(
    defs: &[Def],
    node_of_def: &HashMap<usize, usize>,
    nodes: &[Node],
    out: &mut [Vec<Edge>],
) {
    let mut per_file: HashMap<&str, Vec<(Range, usize)>> = HashMap::new();
    for (i, def) in defs.iter().enumerate() {
        if let (true, Some(enc)) = (def.is_function(), def.enclosing) {
            per_file
                .entry(def.path.as_str())
                .or_default()
                .push((enc, i));
        }
    }
    for items in per_file.values() {
        for &(child_range, child) in items {
            let parent = items
                .iter()
                .filter(|&&(r, p)| {
                    p != child
                        && r != child_range
                        && start(r) <= start(child_range)
                        && end(child_range) <= end(r)
                })
                .min_by_key(|&&(r, _)| (r.2 - r.0, i64::from(r.3) - i64::from(r.1)));
            if let Some(&(_, parent)) = parent {
                let (from, to) = (node_of_def[&parent], node_of_def[&child]);
                out[from].push(Edge {
                    to,
                    kind: GraphEdgeKind::Contains,
                    provenance: Provenance::Structure,
                    conf: 1.0,
                    sites: 1,
                    witness: Witness {
                        file: nodes[to].file.clone(),
                        line: nodes[to].line,
                        col: defs[child].name_range.1,
                    },
                });
            }
        }
    }
}

/// `Owner::name` and the node kind of a function symbol. The owner is the
/// first type parameter of an `impl#[Type]` descriptor, else the enclosing
/// type or trait, else the last module.
fn describe(symbol: &str) -> (String, FnKind) {
    use scip::types::descriptor::Suffix;
    let fallback = || {
        let tail = symbol.rsplit('/').next().unwrap_or(symbol);
        (tail.trim_end_matches("().").to_string(), FnKind::Function)
    };
    let Ok(parsed) = scip::symbol::parse_symbol(symbol) else {
        return fallback();
    };
    let suffix = |d: &scip::types::Descriptor| d.suffix.enum_value().ok();
    let Some(last) = parsed.descriptors.last() else {
        return fallback();
    };
    let ds = &parsed.descriptors;
    let has_type = ds.iter().any(|d| suffix(d) == Some(Suffix::Type));
    let owner = ds
        .iter()
        .find(|d| suffix(d) == Some(Suffix::TypeParameter))
        .or_else(|| {
            ds.iter()
                .rev()
                .skip(1)
                .find(|d| suffix(d) == Some(Suffix::Type) && d.name != "impl")
        })
        .or_else(|| {
            ds.iter()
                .rev()
                .skip(1)
                .find(|d| suffix(d) == Some(Suffix::Namespace))
        });
    let name = match owner {
        Some(owner) => format!("{}::{}", owner.name, last.name),
        None => last.name.clone(),
    };
    let kind = if has_type {
        FnKind::Method
    } else {
        FnKind::Function
    };
    (name, kind)
}

// ------------------------------------------------------------- as a map

/// One graph edge with the node ids of its ends, for the typed [`TypedEdge`].
pub struct ScipEdgeRef<'a> {
    pub src: &'a NodeId,
    pub dst: &'a NodeId,
    pub edge: &'a Edge,
}

impl From<GraphEdgeKind> for EdgeKind {
    fn from(kind: GraphEdgeKind) -> Self {
        match kind {
            GraphEdgeKind::CallsOrRefs => Self::CallsOrRefs,
            GraphEdgeKind::Contains => Self::Contains,
        }
    }
}

/// A SCIP edge is compiler-grade evidence ([`WitnessClass::Compiler`]), with
/// the first site as its site. The site has the file path and no blob (the
/// graph holds no blob ids; an edge source fills it); `conf` is the graph's 1.0 or 0.5 in whole percent.
impl From<ScipEdgeRef<'_>> for TypedEdge {
    fn from(r: ScipEdgeRef<'_>) -> Self {
        Self {
            src: r.src.clone(),
            dst: r.dst.clone(),
            kind: r.edge.kind.into(),
            map: SCIP_MAP_ID,
            witness: WitnessClass::Compiler,
            conf: conf_from_unit(r.edge.conf),
            site: Some(Site {
                path: r.edge.witness.file.clone(),
                blob: String::new(),
                line: r.edge.witness.line,
                col: r.edge.witness.col,
            }),
        }
    }
}

/// The id of [`ScipMap`].
pub const SCIP_MAP_ID: MapId = "graph.scip";

/// A best-effort [`NodeId`] per graph node: Rust, the module path of the
/// file, no container (the graph does not keep the impl header), the last
/// segment of the display name, kind from [`FnKind`]. Two nodes with the
/// same key get distinct disambiguators in graph order. A caller with the
/// real declarations should pass its own ids to [`ScipMap::new`].
pub fn default_node_ids(graph: &ScipGraph) -> Vec<NodeId> {
    let mut seen: HashMap<SymbolKey, u16> = HashMap::new();
    graph
        .nodes
        .iter()
        .map(|node| {
            let mut key = SymbolKey {
                language: "rust".into(),
                module_path: rust_module_path(&node.file),
                container: Vec::new(),
                name: node.name.rsplit("::").next().unwrap_or(&node.name).into(),
                kind: match node.kind {
                    FnKind::Function => SymbolKind::Function,
                    FnKind::Method => SymbolKind::Method,
                },
                disambiguator: 0,
            };
            let count = seen.entry(key.clone()).or_insert(0);
            key.disambiguator = *count;
            *count += 1;
            NodeId::Symbol(key)
        })
        .collect()
}

/// The graph as a [`Map`]: `expand` returns the node's out-edges with
/// witness [`WitnessClass::Compiler`]; `query` answers a seeded request with
/// the best edge to each neighbour. No text.
pub struct ScipMap {
    graph: ScipGraph,
    ids: Vec<NodeId>,
    index: HashMap<NodeId, usize>,
    descriptor: MapDescriptor,
}

impl ScipMap {
    /// `ids[i]` is the id of `graph.nodes[i]`; they must be one per node and
    /// distinct.
    pub fn new(graph: ScipGraph, ids: Vec<NodeId>) -> Result<Self, String> {
        if ids.len() != graph.nodes.len() {
            return Err(format!(
                "{} ids for {} graph nodes",
                ids.len(),
                graph.nodes.len()
            ));
        }
        let index: HashMap<NodeId, usize> = ids.iter().cloned().zip(0..).collect();
        if index.len() != ids.len() {
            return Err("node ids are not distinct".into());
        }
        Ok(Self {
            graph,
            ids,
            index,
            descriptor: MapDescriptor {
                id: SCIP_MAP_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol],
                emits: vec![NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
        })
    }

    pub fn with_default_ids(graph: ScipGraph) -> Self {
        let ids = default_node_ids(&graph);
        Self::new(graph, ids).expect("default ids are distinct")
    }

    pub fn graph(&self) -> &ScipGraph {
        &self.graph
    }

    pub fn node_id(&self, node: usize) -> &NodeId {
        &self.ids[node]
    }

    /// The out-edges of graph node `from`, unsorted.
    fn typed_edges(&self, from: usize) -> Vec<TypedEdge> {
        self.graph.out[from]
            .iter()
            .map(|edge| {
                TypedEdge::from(ScipEdgeRef {
                    src: &self.ids[from],
                    dst: &self.ids[edge.to],
                    edge,
                })
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Map for ScipMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut edges = Vec::new();
        for (seed, _) in request.seeds {
            if let Some(&from) = self.index.get(seed) {
                edges.extend(self.typed_edges(from));
            }
        }
        sort_edges(&mut edges);
        let mut seen = HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.cost.nodes_touched = edges.len() as u64;
        response.hits = edges
            .into_iter()
            .zip(1..)
            .map(|(edge, rank)| MapHit {
                node: edge.dst,
                rank,
                score: Some(f32::from(edge.conf) / f32::from(CONF_MAX)),
                evidence: vec![Evidence {
                    via: Some(edge.src),
                    edge: edge.kind,
                    label: SourceLabel::Scip,
                    detail: None,
                }],
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, _budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        let Some(&from) = self.index.get(node) else {
            return Expanded::default();
        };
        let mut edges = self.typed_edges(from);
        cap_edges(&mut edges, EXPAND_LIMIT);
        let cost = Cost {
            nodes_touched: edges.len() as u64,
            ..Cost::default()
        };
        Expanded::new(edges, cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occ(range: Range, symbol: &str, roles: i32, enclosing: Option<Range>) -> RawOccurrence {
        RawOccurrence {
            range,
            symbol: symbol.to_string(),
            roles,
            enclosing,
        }
    }

    fn sym(path: &str) -> String {
        format!("rust-analyzer cargo demo 0.1.0 {path}")
    }

    /// a.rs: `fn alpha() { beta(); beta(); local; }`, `fn beta() { gamma(); }`,
    /// `fn gamma() {}` with a nested `fn inner() { gamma(); }` inside beta.
    fn demo_documents() -> Vec<RawDocument> {
        let (alpha, beta, gamma, inner) = (
            sym("a/alpha()."),
            sym("a/beta()."),
            sym("a/gamma()."),
            sym("a/beta()./inner()."),
        );
        vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 8), &alpha, 1, Some((0, 0, 0, 40))),
                occ((0, 13, 0, 17), &beta, 0, None),
                occ((0, 20, 0, 24), &beta, 0, None),
                occ((0, 27, 0, 28), "local 3", 0, None),
                occ((0, 31, 0, 36), &alpha, 0, None),
                occ((1, 3, 1, 7), &beta, 1, Some((1, 0, 4, 1))),
                occ((1, 12, 1, 17), &gamma, 0, None),
                occ((2, 7, 2, 12), &inner, 1, Some((2, 4, 3, 20))),
                occ((3, 8, 3, 13), &gamma, 0, None),
                occ((5, 3, 5, 8), &gamma, 1, Some((5, 0, 5, 12))),
                occ((9, 0, 9, 4), &gamma, 0, None),
            ],
        }]
    }

    #[test]
    fn edges_are_per_pair_with_first_witness_and_site_count() {
        let graph = ScipGraph::build(&demo_documents(), None);
        assert_eq!(graph.nodes.len(), 4);
        let alpha = graph.node_index(&sym("a/alpha().")).unwrap();
        let beta = graph.node_index(&sym("a/beta().")).unwrap();
        let gamma = graph.node_index(&sym("a/gamma().")).unwrap();
        let inner = graph.node_index(&sym("a/beta()./inner().")).unwrap();
        // alpha -> beta (2 sites), self reference and the local skipped.
        let edge = &graph.out[alpha][0];
        assert_eq!((edge.to, edge.sites), (beta, 2));
        assert_eq!(graph.out[alpha].len(), 1);
        assert_eq!(edge.witness.col, 13);
        assert_eq!(edge.provenance, Provenance::Scip);
        // the reference at line 3 is inside `inner`, the innermost range
        assert!(graph.has_edge(inner, gamma, GraphEdgeKind::CallsOrRefs));
        assert!(graph.has_edge(beta, gamma, GraphEdgeKind::CallsOrRefs));
        assert!(graph.has_edge(beta, inner, GraphEdgeKind::Contains));
        assert_eq!(graph.edge_count(GraphEdgeKind::Contains), 1);
        assert_eq!(graph.edge_count(GraphEdgeKind::CallsOrRefs), 3);
        // the reference on line 9 is outside every range
        assert_eq!(graph.report.unattributed_sites, 1);
        assert_eq!(graph.indegree[gamma], 2);
    }

    #[test]
    fn generated_only_edges_get_half_confidence() {
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 4), &sym("a/f()."), 1, Some((0, 0, 3, 1))),
                occ((5, 3, 5, 4), &sym("a/g()."), 1, Some((5, 0, 6, 1))),
                occ((1, 4, 1, 5), &sym("a/g()."), 16, None),
            ],
        }];
        let graph = ScipGraph::build(&docs, None);
        assert!((graph.out[0][0].conf - 0.5).abs() < f32::EPSILON);
    }

    struct Map(HashMap<String, Vec<u8>>);
    impl SourceText for Map {
        fn read(&self, path: &str) -> Option<Vec<u8>> {
            self.0.get(path).cloned()
        }
    }

    #[test]
    fn repair_narrows_a_block_range_shared_by_sibling_methods() {
        // Both methods carry the whole impl block's range, as under
        // `#[async_trait]`; the caller attribution needs the repair.
        let src = "impl T {\n    fn one(&self) {\n        two();\n    }\n    fn two(&self) {}\n}\n";
        let block = Some((0, 0, 5, 1));
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((1, 7, 1, 10), &sym("a/impl#[T]one()."), 1, block),
                occ((4, 7, 4, 10), &sym("a/impl#[T]two()."), 1, block),
                occ((2, 8, 2, 11), &sym("a/impl#[T]two()."), 0, None),
            ],
        }];
        let unrepaired = ScipGraph::build(&docs, None);
        assert_eq!(unrepaired.report.repair_skipped, 2);
        let source = Map(HashMap::from([(
            "a.rs".to_string(),
            src.as_bytes().to_vec(),
        )]));
        let graph = ScipGraph::build(&docs, Some(&source));
        assert_eq!(graph.report.repaired, 2);
        let one = graph.node_index(&sym("a/impl#[T]one().")).unwrap();
        let two = graph.node_index(&sym("a/impl#[T]two().")).unwrap();
        assert!(graph.has_edge(one, two, GraphEdgeKind::CallsOrRefs));
        assert!(!graph.has_edge(two, one, GraphEdgeKind::CallsOrRefs));
        assert_eq!(graph.nodes[one].name, "T::one");
        assert_eq!(graph.nodes[one].kind, FnKind::Method);
    }

    #[test]
    fn body_scan_skips_strings_comments_and_chars() {
        let src = "fn f() -> i32 { let s = \"}\"; // }\n let c = '}'; /* } */ 1 }\nfn g() {}\n";
        let lines: Vec<Vec<u8>> = src
            .as_bytes()
            .split(|&b| b == b'\n')
            .map(<[u8]>::to_vec)
            .collect();
        assert_eq!(body_range(&lines, (0, 3, 0, 4)), Some((0, 0, 1, 25)));
        let proto = vec![b"fn h(x: [u8; 2]);".to_vec()];
        assert_eq!(body_range(&proto, (0, 3, 0, 4)), Some((0, 0, 0, 17)));
    }

    #[test]
    fn display_names_follow_the_gold_convention() {
        let free = describe(&sym("rest/service_status()."));
        assert_eq!(
            (free.0.as_str(), free.1),
            ("rest::service_status", FnKind::Function)
        );
        let method = describe(&sym("lsp/clangd/impl#[ClangdServer][LspAdapter]enabled()."));
        assert_eq!(
            (method.0.as_str(), method.1),
            ("ClangdServer::enabled", FnKind::Method)
        );
        let decl = describe(&sym("lsp/adapter/LspAdapter#enabled()."));
        assert_eq!(
            (decl.0.as_str(), decl.1),
            ("LspAdapter::enabled", FnKind::Method)
        );
    }

    fn demo_map() -> ScipMap {
        ScipMap::with_default_ids(ScipGraph::build(&demo_documents(), None))
    }

    fn empty_view() -> SnapshotView<'static> {
        SnapshotView::new(crate::SnapshotRef::default(), Vec::new())
    }

    fn test_budget() -> Budget {
        Budget {
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
            max_external_calls: u32::MAX,
            max_nodes_touched: u64::MAX,
        }
    }

    /// The demo of slice S1: the typed edges of one symbol. Run with
    /// `cargo test -p lci-maps demo_prints -- --nocapture`.
    #[tokio::test]
    async fn expand_returns_the_graph_edges_with_compiler_witness() {
        use crate::Map as _;
        let map = demo_map();
        let beta = map.graph().node_index(&sym("a/beta().")).unwrap();
        let node = map.node_id(beta).clone();
        let view = empty_view();

        let edges =
            crate::kit_c3::check_expand(&map, &node, std::time::Duration::from_secs(5), &view)
                .await
                .expect("C7");
        for edge in &edges {
            println!(
                "{} -[{} {} conf {}]-> {}  at {:?}",
                edge.src,
                edge.kind.as_str(),
                edge.witness.as_str(),
                edge.conf,
                edge.dst,
                edge.site
            );
        }

        // One typed edge per graph edge, same ends and kinds, all Compiler.
        let mut want: Vec<_> = map.graph().out[beta]
            .iter()
            .map(|e| (map.node_id(e.to).clone(), EdgeKind::from(e.kind)))
            .collect();
        let mut got: Vec<_> = edges.iter().map(|e| (e.dst.clone(), e.kind)).collect();
        want.sort();
        got.sort();
        assert_eq!(got, want);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e.witness == WitnessClass::Compiler));
        assert!(edges.iter().all(|e| e.map == SCIP_MAP_ID && e.conf == 100));
        let call = edges
            .iter()
            .find(|e| e.kind == EdgeKind::CallsOrRefs)
            .unwrap();
        assert_eq!(
            call.site,
            Some(Site {
                path: "a.rs".into(),
                blob: String::new(),
                line: 1,
                col: 12
            })
        );
        // An unknown node has no edges.
        let unknown = NodeId::file("nowhere.rs").unwrap();
        assert!(
            map.expand(&unknown, test_budget(), &view)
                .await
                .edges
                .is_empty()
        );
    }

    #[tokio::test]
    async fn generated_only_edges_are_half_confidence_and_query_agrees_with_expand() {
        use crate::Map as _;
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 4), &sym("a/f()."), 1, Some((0, 0, 3, 1))),
                occ((5, 3, 5, 4), &sym("a/g()."), 1, Some((5, 0, 6, 1))),
                occ((1, 4, 1, 5), &sym("a/g()."), 16, None),
            ],
        }];
        let map = ScipMap::with_default_ids(ScipGraph::build(&docs, None));
        let f = map
            .node_id(map.graph().node_index(&sym("a/f().")).unwrap())
            .clone();
        let view = empty_view();
        let edges = map.expand(&f, test_budget(), &view).await.edges;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].conf, 50);

        // The seeded query names the same neighbours as `expand`.
        let seeds = [(f.clone(), 1.0)];
        let params = serde_json::Value::Null;
        let response = map
            .query(
                &crate::MapRequest {
                    text: None,
                    seeds: &seeds,
                    limit: 10,
                    budget: test_budget(),
                    params: &params,
                    as_of: None,
                },
                &view,
            )
            .await;
        let hit_nodes: Vec<_> = response.hits.iter().map(|h| h.node.clone()).collect();
        let edge_nodes: Vec<_> = edges.iter().map(|e| e.dst.clone()).collect();
        assert_eq!(hit_nodes, edge_nodes);
        assert_eq!(response.hits[0].evidence[0].edge, EdgeKind::CallsOrRefs);
    }

    #[test]
    fn map_construction_rejects_wrong_or_repeated_ids() {
        let graph = ScipGraph::build(&demo_documents(), None);
        assert!(ScipMap::new(graph.clone(), Vec::new()).is_err());
        let one = NodeId::file("a.rs").unwrap();
        assert!(ScipMap::new(graph, vec![one; 4]).is_err());
    }
}
