//! A typed graph over function and method symbols, loaded from a SCIP index;
//! the `graph.scip` map and the traversal engine read it (History: slice M0.4
//! of the multimap spike v2; pre-registration in
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
//! * `implements`: from a trait-impl member (`impl#[Type][Trait]name().`) to
//!   the trait's method declaration (`Trait#name().`), when both are nodes.
//!   rust-analyzer writes no SCIP relationships, so the trait is read from
//!   the `impl ... Trait for Type` header: the first type reference occurrence
//!   between the generics and `for` (see [`derive_impls`]). The edge needs the
//!   source text of the indexed tree; without it none is made and the build
//!   report counts the documents. These edges are not callers: they do not
//!   count in [`ScipGraph::indegree`]. Limits of the header read: a header
//!   after an attribute on the same line (`#[cfg(x)] impl Tr for Foo {`) is
//!   not read (`BuildReport::impl_after_attribute` counts such lines); a
//!   negative impl (`impl !Tr for Foo`) gives no fact (`impl_negative`);
//!   columns are UTF-8 byte columns, as rust-analyzer writes them
//!   (`PositionEncoding::Utf8`) and as the scanner counts them, so a header
//!   line with non-ASCII text needs no conversion.
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
        CONF_MAX, Edge as TypedEdge, Expanded, MapCard, Site, WitnessClass, cap_edges,
        conf_from_unit, sort_edges,
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

/// The documentation of the function and method symbols of an index, by
/// symbol: the `SymbolInformation.documentation` lines joined by `\n`, cut at
/// [`MAX_DOC_BYTES`]. A symbol with none is not in the table.
pub type SymbolDocs = HashMap<String, String>;

/// Longest documentation text kept for a symbol, in bytes (cut on a character
/// boundary).
pub const MAX_DOC_BYTES: usize = 1024;

/// Reads the documents and occurrences of a SCIP index file. The documentation
/// of the symbols is not read; see [`read_symbol_docs`].
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

/// The documentation of the function and method symbols (the symbols a graph
/// node can be) of a SCIP index file, each cut at [`MAX_DOC_BYTES`] before it
/// is copied. It parses the file again and only the entity seeds
/// (`[run_profile.seeds] bm25`) ask for it: the graph keeps no documentation.
pub fn read_symbol_docs(path: &Path) -> Result<SymbolDocs, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let index = scip::types::Index::parse_from_bytes(&bytes)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    let mut docs = SymbolDocs::new();
    for info in index
        .documents
        .iter()
        .flat_map(|document| &document.symbols)
    {
        if !info.symbol.ends_with("().")
            || info.symbol.starts_with("local ")
            || docs.contains_key(&info.symbol)
        {
            continue;
        }
        let mut text = String::new();
        for line in &info.documentation {
            if text.len() >= MAX_DOC_BYTES {
                break;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(cut_to_boundary(line, MAX_DOC_BYTES));
        }
        let text = cut_to_boundary(text.trim(), MAX_DOC_BYTES);
        if !text.is_empty() {
            docs.insert(info.symbol.clone(), text.to_owned());
        }
    }
    Ok(docs)
}

/// The names of a symbol's path, outermost first: the package, then each
/// descriptor (modules, types, the member). The `impl` marker of a trait-impl
/// member is left out. Empty for a symbol that does not parse.
pub fn symbol_path_names(symbol: &str) -> Vec<String> {
    let Ok(parsed) = scip::symbol::parse_symbol(symbol) else {
        return Vec::new();
    };
    std::iter::once(parsed.package.name.clone())
        .chain(parsed.descriptors.iter().map(|d| d.name.clone()))
        .filter(|name| !name.is_empty() && name != "impl")
        .collect()
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut_to_boundary(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GraphEdgeKind {
    CallsOrRefs,
    Contains,
    /// From a trait-impl member to the trait's method declaration.
    Implements,
}

impl GraphEdgeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CallsOrRefs => "calls_or_refs",
            Self::Contains => "contains",
            Self::Implements => "implements",
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
    /// Trait-impl headers (`impl ... for ...`) found in the source text.
    pub impl_headers: usize,
    /// Of those, headers with a trait reference occurrence.
    pub impl_resolved: usize,
    /// Headers with no trait occurrence whose text holds a `$` (macro
    /// metavariable).
    pub impl_unresolved_macro: usize,
    /// Headers with no trait occurrence and no `$`.
    pub impl_unresolved_other: usize,
    /// Documents whose source text was not available (their headers are not
    /// read).
    pub impl_no_source: usize,
    /// Resolved headers whose trait has no definition in the index.
    pub impl_external_trait: usize,
    /// `implements` edges made (member to trait method declaration).
    pub impl_edges: usize,
    /// Members of a workspace trait's impl block that gave no edge: the trait
    /// method is no node (an associated type, a macro-written declaration) or
    /// the member's symbol names another trait than the header.
    pub impl_members_unlinked: usize,
    /// Members of an external trait's impl block (no edge: the declaration is
    /// no node).
    pub impl_members_external: usize,
    /// Negative impls (`impl !Trait for Type`), not in `impl_headers`: given no
    /// fact (the type does not implement the trait).
    pub impl_negative: usize,
    /// Lines that start with an attribute and carry an `impl` after it
    /// (`#[cfg(x)] impl ..`, trait or inherent): the header is not read.
    pub impl_after_attribute: usize,
    /// Nodes inside a trait-impl block whose symbol is no impl member
    /// (`impl#[Type][Trait]name().`) the scan could read.
    pub impl_members_unparsed: usize,
}

#[derive(Debug, Clone)]
pub struct ScipGraph {
    pub nodes: Vec<Node>,
    /// Out-edges per node, in source order of the witness.
    pub out: Vec<Vec<Edge>>,
    /// Distinct callers or parents per node (`implements` edges not
    /// counted).
    pub indegree: Vec<u32>,
    /// The trait-impl headers read from the source text.
    pub impls: Vec<ImplFact>,
    pub report: BuildReport,
    index: HashMap<String, usize>,
}

impl ScipGraph {
    /// A graph with no nodes and no edges (`[run_profile.scip] mode` `off`, or
    /// `cache` with no index file): nothing resolves in it.
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            out: Vec::new(),
            indegree: Vec::new(),
            impls: Vec::new(),
            report: BuildReport::default(),
            index: HashMap::new(),
        }
    }

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
        Self::build_with(documents, source, true)
    }

    /// [`ScipGraph::build`], with the trait-impl derivation switched off for
    /// a timing of the load without it.
    pub(crate) fn build_with(
        documents: &[RawDocument],
        source: Option<&dyn SourceText>,
        derive: bool,
    ) -> Self {
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
        let index: HashMap<String, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.symbol.clone(), i))
            .collect();
        let impls = if derive {
            derive_impls(documents, source, &mut report)
        } else {
            Vec::new()
        };
        add_implements_edges(&impls, &nodes, &index, &mut out, &mut report);
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
            for edge in edges.iter().filter(|e| e.kind != GraphEdgeKind::Implements) {
                callers[edge.to].insert(from);
            }
        }
        let indegree = callers.iter().map(|c| c.len() as u32).collect();
        Self {
            nodes,
            out,
            indegree,
            impls,
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
pub(crate) struct Structural<'a, L: AsRef<[u8]> = Vec<u8>> {
    pub(crate) lines: &'a [L],
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

impl<L: AsRef<[u8]>> Iterator for Structural<'_, L> {
    type Item = (usize, usize, u8);

    fn next(&mut self) -> Option<Self::Item> {
        while self.line < self.lines.len() {
            let text = self.lines[self.line].as_ref();
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

// ------------------------------------------------- trait-impl headers

/// Lines a trait-impl header may span before the scan gives up on it.
const HEADER_LINES: usize = 15;

/// Where an [`ImplFact`] was read from. Only the header text so far;
/// rust-analyzer writes no SCIP relationships (an open issue upstream), and
/// a second source would be a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplSource {
    Header,
}

/// One `impl Trait for Type` header, with the trait read from the reference
/// occurrence of the trait's name in the header.
#[derive(Debug, Clone)]
pub struct ImplFact {
    /// The first type reference after `for` (`None` for a blanket impl over
    /// `T`, a primitive, a tuple or a reference the index does not resolve).
    pub type_sym: Option<String>,
    /// The first type reference between the generics and `for`; ends in `#`.
    pub trait_sym: String,
    pub path: String,
    /// The 0-based line of the `impl` keyword.
    pub header_line: u32,
    /// The trait's occurrence in the header (the evidence of the facts).
    pub site: Witness,
    /// From the `impl` keyword to the closing brace (to the header's end when
    /// there is no body).
    pub block: Range,
    /// The trait has no definition in the index (another crate's trait).
    pub external: bool,
    /// The trait occurrence is macro-generated.
    pub generated: bool,
    pub source: ImplSource,
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The column after the `impl` keyword when the line starts an `impl` item:
/// optional `pub` (with a `(..)` restriction), `default` and `unsafe`, then
/// `impl` followed by a space or `<`.
fn impl_keyword_end(text: &[u8]) -> Option<usize> {
    let skip_ws = |mut i: usize| {
        while text.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    let mut i = skip_ws(0);
    // Most lines start with something else: leave at the first byte.
    if !matches!(text.get(i), Some(b'i' | b'p' | b'd' | b'u')) {
        return None;
    }
    while i < text.len() {
        let word_end = i + text[i..].iter().take_while(|&&b| is_ident(b)).count();
        match &text[i..word_end] {
            b"impl" => {
                return matches!(text.get(word_end), Some(b'<' | b' ' | b'\t')).then_some(word_end);
            }
            b"default" | b"unsafe" => i = skip_ws(word_end),
            b"pub" => {
                i = skip_ws(word_end);
                if text.get(i) == Some(&b'(') {
                    i = skip_ws(i + text[i..].iter().position(|&b| b == b')')? + 1);
                }
            }
            _ => return None,
        }
    }
    None
}

/// Whether the line starts with an attribute and holds an `impl` item after
/// it (`#[cfg(x)] impl Tr for Foo {`): a header the scan does not read.
fn attribute_then_impl(text: &[u8]) -> bool {
    let rest = text.trim_ascii_start();
    rest.starts_with(b"#[")
        && [&b"] impl "[..], b"] unsafe impl ", b"] impl<"]
            .iter()
            .any(|needle| rest.windows(needle.len()).any(|w| w == *needle))
}

/// A trait-impl header found in the source text.
struct HeaderScan {
    generics_end: (u32, u32),
    for_at: (u32, u32),
    /// The `where` after the type, else the end of the header.
    type_end: (u32, u32),
    /// The `{` or `;` that ends the header.
    end: (u32, u32),
    /// The end of the closing brace of the body.
    block_end: Option<(u32, u32)>,
    dollar: bool,
    /// `impl !Trait for Type`.
    negative: bool,
}

/// Scans the item that starts at `line` (after the `impl` keyword, at byte
/// `after_impl`) as an `impl .. for ..` header: `None` for an inherent impl
/// (no `for` before `where`, `{` or `;`), for `for<` (a higher-ranked bound),
/// or a header the scan cannot end.
fn scan_header<L: AsRef<[u8]>>(lines: &[L], line: usize, after_impl: usize) -> Option<HeaderScan> {
    let mut stream = Structural {
        lines,
        line,
        col: after_impl,
        block_depth: 0,
    };
    let mut code: Vec<(u32, u32, u8)> = Vec::new();
    let mut paren = 0i32;
    let mut end = None;
    for (l, c, ch) in stream.by_ref() {
        if l > line + HEADER_LINES {
            return None;
        }
        match ch {
            b'(' | b'[' => paren += 1,
            b')' | b']' => paren -= 1,
            b'{' | b';' if paren <= 0 => {
                end = Some((l as u32, c as u32, ch));
                break;
            }
            _ => {}
        }
        // The scan yields no newline: keep the words of two lines apart.
        if code.last().is_some_and(|&(last, _, _)| last != l as u32) {
            code.push((l as u32, 0, b' '));
        }
        code.push((l as u32, c as u32, ch));
    }
    let (end_line, end_col, terminator) = end?;
    let at = |i: usize| code.get(i).map_or((end_line, end_col), |&(l, c, _)| (l, c));
    let skip_ws = |mut i: usize| {
        while code.get(i).is_some_and(|c| c.2.is_ascii_whitespace()) {
            i += 1;
        }
        i
    };
    // The generics list, `<..>`, where `->` does not close a level.
    let mut p = skip_ws(0);
    if code.get(p).map(|c| c.2) == Some(b'<') {
        let mut depth = 0i32;
        let mut closed = None;
        for q in p..code.len() {
            match code[q].2 {
                b'<' => depth += 1,
                b'>' if q > p && code[q - 1].2 != b'-' => {
                    depth -= 1;
                    if depth == 0 {
                        closed = Some(q + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        p = closed?;
    }
    let generics_end = p;
    let mut for_idx = None;
    let mut q = generics_end;
    while q < code.len() {
        if !is_ident(code[q].2) {
            q += 1;
            continue;
        }
        let start_of_word = q;
        while q < code.len() && is_ident(code[q].2) {
            q += 1;
        }
        let word: Vec<u8> = code[start_of_word..q].iter().map(|c| c.2).collect();
        if word == b"where" {
            break;
        }
        if word == b"for" && code.get(skip_ws(q)).map(|c| c.2) != Some(b'<') {
            for_idx = Some(start_of_word);
            break;
        }
    }
    let for_idx = for_idx?;
    // The type ends at a `where` clause.
    let mut type_end = code.len();
    let mut q = for_idx + 3;
    while q < code.len() {
        if !is_ident(code[q].2) {
            q += 1;
            continue;
        }
        let start_of_word = q;
        while q < code.len() && is_ident(code[q].2) {
            q += 1;
        }
        if code[start_of_word..q].iter().map(|c| c.2).eq(*b"where") {
            type_end = start_of_word;
            break;
        }
    }
    let mut block_end = None;
    if terminator == b'{' {
        let mut depth = 1i32;
        for (l, c, ch) in stream {
            match ch {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = Some((l as u32, c as u32 + 1));
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    Some(HeaderScan {
        generics_end: at(generics_end),
        for_at: at(for_idx),
        type_end: at(type_end),
        end: (end_line, end_col),
        block_end,
        dollar: code.iter().any(|c| c.2 == b'$'),
        negative: code.get(skip_ws(generics_end)).map(|c| c.2) == Some(b'!'),
    })
}

/// The trait-impl facts of every document: for each line that starts an
/// `impl .. for ..` item, the trait is the first reference occurrence of a
/// type (symbol ends in `#`) between the generics and `for`, the type the
/// first one after `for`. Columns are compared as bytes, like the repair's.
///
/// `source` gives the text; without it (or for a document it cannot read)
/// nothing is read and `report.impl_no_source` counts the document.
pub fn derive_impls(
    documents: &[RawDocument],
    source: Option<&dyn SourceText>,
    report: &mut BuildReport,
) -> Vec<ImplFact> {
    let mut facts = Vec::new();
    let Some(source) = source else {
        report.impl_no_source += documents.len();
        return facts;
    };
    let mut defined: HashSet<&str> = HashSet::new();
    for document in documents {
        let bytes = source.read(&document.path);
        if bytes.is_none() {
            report.impl_no_source += 1;
        }
        let mut candidates: Vec<(usize, usize)> = Vec::new();
        for (i, text) in bytes
            .iter()
            .flat_map(|b| b.split(|&c| c == b'\n'))
            .enumerate()
        {
            match impl_keyword_end(text) {
                Some(end) => candidates.push((i, end)),
                None if attribute_then_impl(text) => report.impl_after_attribute += 1,
                None => {}
            }
        }
        let lines: Vec<&[u8]> = if candidates.is_empty() {
            Vec::new()
        } else {
            bytes
                .iter()
                .flat_map(|b| b.split(|&c| c == b'\n'))
                .map(|text| text.strip_suffix(b"\r").unwrap_or(text))
                .collect()
        };
        let mut wanted = vec![false; lines.len() + HEADER_LINES + 1];
        for &(l, _) in &candidates {
            wanted[l..=l + HEADER_LINES].fill(true);
        }
        let mut by_line: HashMap<u32, Vec<(u32, &RawOccurrence)>> = HashMap::new();
        for occ in &document.occurrences {
            if !occ.symbol.ends_with('#') {
                continue;
            }
            if occ.roles & ROLE_DEFINITION != 0 {
                defined.insert(occ.symbol.as_str());
            } else if occ.range.0 == occ.range.2
                && wanted.get(occ.range.0 as usize).copied().unwrap_or(false)
                && !occ.symbol.starts_with("local ")
            {
                by_line
                    .entry(occ.range.0)
                    .or_default()
                    .push((occ.range.1, occ));
            }
        }
        for list in by_line.values_mut() {
            list.sort_by_key(|&(col, _)| col);
        }
        for (line, after_impl) in candidates {
            let Some(scan) = scan_header(&lines, line, after_impl) else {
                continue;
            };
            if scan.negative {
                report.impl_negative += 1;
                continue;
            }
            report.impl_headers += 1;
            let after_for = (scan.for_at.0, scan.for_at.1 + 3);
            let (mut trait_occ, mut type_occ) = (None, None);
            for l in line as u32..=scan.end.0 {
                for &(col, occ) in by_line.get(&l).into_iter().flatten() {
                    let at = (l, col);
                    if trait_occ.is_none() && scan.generics_end <= at && at < scan.for_at {
                        trait_occ = Some(occ);
                    }
                    if type_occ.is_none() && after_for <= at && at < scan.type_end {
                        type_occ = Some(occ);
                    }
                }
            }
            let Some(trait_occ) = trait_occ else {
                if scan.dollar {
                    report.impl_unresolved_macro += 1;
                } else {
                    report.impl_unresolved_other += 1;
                }
                continue;
            };
            report.impl_resolved += 1;
            facts.push(ImplFact {
                type_sym: type_occ.map(|o| o.symbol.clone()),
                trait_sym: trait_occ.symbol.clone(),
                path: document.path.clone(),
                header_line: line as u32,
                site: Witness {
                    file: document.path.clone(),
                    line: trait_occ.range.0,
                    col: trait_occ.range.1,
                },
                block: (
                    line as u32,
                    0,
                    scan.block_end.unwrap_or(scan.end).0,
                    scan.block_end.unwrap_or((scan.end.0, scan.end.1 + 1)).1,
                ),
                external: false,
                generated: trait_occ.roles & ROLE_GENERATED != 0,
                source: ImplSource::Header,
            });
        }
    }
    // A trait with no definition in the index belongs to another crate.
    let external: Vec<bool> = facts
        .iter()
        .map(|f| !defined.contains(f.trait_sym.as_str()))
        .collect();
    report.impl_external_trait = external.iter().filter(|&&e| e).count();
    for (fact, external) in facts.iter_mut().zip(external) {
        fact.external = external;
    }
    facts
}

/// `(type, trait, member name)` of an impl-member symbol
/// `...impl#[Type][Trait]name().`; the two groups are bare or in backticks. The
/// innermost (last) `impl#[` counts: a nested impl inside a method gives its own
/// member symbol `...impl#[A][T]m().impl#[B][U]n().`.
fn impl_member_parts(symbol: &str) -> Option<(&str, &str, &str)> {
    let mut rest = &symbol[symbol.rfind("impl#[")? + 5..];
    let mut groups = [""; 2];
    for group in &mut groups {
        let inner = rest.strip_prefix('[')?;
        let (text, after) = match inner.strip_prefix('`') {
            Some(quoted) => {
                let close = quoted.find("`]")?;
                (&quoted[..close], &quoted[close + 2..])
            }
            None => {
                let close = inner.find(']')?;
                (&inner[..close], &inner[close + 1..])
            }
        };
        *group = text;
        rest = after;
    }
    let name = rest.strip_suffix("().")?;
    (!name.is_empty() && name.bytes().all(is_ident)).then_some((groups[0], groups[1], name))
}

/// The bare name of a trait group: `Service<Req>` and `fmt::Display` give
/// `Service` and `Display`.
fn group_trait_name(group: &str) -> &str {
    let name = group.split('<').next().unwrap_or(group);
    name.rsplit("::").next().unwrap_or(name).trim()
}

/// The last name of a global type symbol (`.../Display#` gives `Display`).
fn symbol_type_name(symbol: &str) -> &str {
    let descriptors = symbol.splitn(5, ' ').last().unwrap_or(symbol);
    let name = descriptors.strip_suffix('#').unwrap_or(descriptors);
    let at = name.rfind(['/', '#', '.', ':']).map_or(0, |i| i + 1);
    name[at..].trim_matches('`')
}

/// An `implements` edge from each member of a trait-impl block to the
/// trait's method declaration, when the declaration is a node. The member
/// belongs to the innermost impl block holding its name; its symbol must name
/// the same trait as the header (a guard against nested impls and macros).
fn add_implements_edges(
    impls: &[ImplFact],
    nodes: &[Node],
    index: &HashMap<String, usize>,
    out: &mut [Vec<Edge>],
    report: &mut BuildReport,
) {
    let mut by_path: HashMap<&str, Vec<&ImplFact>> = HashMap::new();
    for fact in impls {
        by_path.entry(fact.path.as_str()).or_default().push(fact);
    }
    for (member, node) in nodes.iter().enumerate() {
        let Some(facts) = by_path.get(node.file.as_str()) else {
            continue;
        };
        let at = (node.line, node.col);
        let block = facts
            .iter()
            .filter(|f| start(f.block) <= at && at <= end(f.block))
            .min_by_key(|f| {
                (
                    f.block.2 - f.block.0,
                    i64::from(f.block.3) - i64::from(f.block.1),
                )
            });
        let Some(fact) = block else {
            continue;
        };
        let Some((_, trait_group, name)) = impl_member_parts(&node.symbol) else {
            report.impl_members_unparsed += 1;
            continue;
        };
        let decl = format!("{}{name}().", fact.trait_sym);
        let to = (group_trait_name(trait_group) == symbol_type_name(&fact.trait_sym))
            .then(|| index.get(&decl))
            .flatten();
        let Some(&to) = to else {
            if fact.external {
                report.impl_members_external += 1;
            } else {
                report.impl_members_unlinked += 1;
            }
            continue;
        };
        report.impl_edges += 1;
        out[member].push(Edge {
            to,
            kind: GraphEdgeKind::Implements,
            provenance: Provenance::Scip,
            conf: if fact.generated { 0.5 } else { 1.0 },
            sites: 1,
            witness: fact.site.clone(),
        });
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
            GraphEdgeKind::Implements => Self::Implements,
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

    fn card(&self) -> MapCard {
        MapCard {
            dependent: true,
            ..MapCard::from_descriptor(self.describe())
        }
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

    // ---- trait-impl headers ------------------------------------------------

    fn one_file(path: &str, text: &str) -> Map {
        Map(HashMap::from([(
            path.to_string(),
            text.as_bytes().to_vec(),
        )]))
    }

    fn def(range: Range, symbol: &str, enclosing: Range) -> RawOccurrence {
        occ(range, symbol, ROLE_DEFINITION, Some(enclosing))
    }

    fn reference(range: Range, symbol: &str) -> RawOccurrence {
        occ(range, symbol, 0, None)
    }

    fn one_document(occurrences: Vec<RawOccurrence>) -> Vec<RawDocument> {
        vec![RawDocument {
            path: "a.rs".into(),
            occurrences,
        }]
    }

    /// A trait with a method, a struct, and `impl Tr for Foo` with the method.
    const PLAIN: &str = "pub trait Tr {\n    fn m(&self);\n}\npub struct Foo;\nimpl Tr for Foo {\n    fn m(&self) {}\n}\n";

    fn plain_documents() -> Vec<RawDocument> {
        one_document(vec![
            def((0, 10, 0, 12), &sym("a/Tr#"), (0, 0, 2, 1)),
            def((1, 7, 1, 8), &sym("a/Tr#m()."), (1, 4, 1, 16)),
            def((3, 11, 3, 14), &sym("a/Foo#"), (3, 0, 3, 15)),
            reference((4, 5, 4, 7), &sym("a/Tr#")),
            reference((4, 12, 4, 15), &sym("a/Foo#")),
            def((5, 7, 5, 8), &sym("a/impl#[Foo][Tr]m()."), (5, 4, 5, 18)),
        ])
    }

    #[test]
    fn a_plain_header_gives_a_fact_and_a_member_edge_to_the_declaration() {
        let source = one_file("a.rs", PLAIN);
        let graph = ScipGraph::build(&plain_documents(), Some(&source));
        assert_eq!(graph.impls.len(), 1);
        let fact = &graph.impls[0];
        assert_eq!(fact.trait_sym, sym("a/Tr#"));
        assert_eq!(fact.type_sym.as_deref(), Some(sym("a/Foo#").as_str()));
        assert_eq!((fact.header_line, fact.source), (4, ImplSource::Header));
        assert_eq!(fact.block, (4, 0, 6, 1));
        assert_eq!((fact.site.line, fact.site.col), (4, 5));
        assert!(!fact.generated);
        let member = graph.node_index(&sym("a/impl#[Foo][Tr]m().")).unwrap();
        let decl = graph.node_index(&sym("a/Tr#m().")).unwrap();
        assert!(graph.has_edge(member, decl, GraphEdgeKind::Implements));
        let edge = &graph.out[member][0];
        assert_eq!(edge.provenance, Provenance::Scip);
        assert!((edge.conf - 1.0).abs() < f32::EPSILON);
        assert_eq!(
            (
                edge.witness.file.as_str(),
                edge.witness.line,
                edge.witness.col
            ),
            ("a.rs", 4, 5)
        );
        assert_eq!(graph.edge_count(GraphEdgeKind::Implements), 1);
        // An `implements` edge is no caller.
        assert_eq!(graph.indegree[decl], 0);
        let r = &graph.report;
        assert_eq!(
            (
                r.impl_headers,
                r.impl_resolved,
                r.impl_external_trait,
                r.impl_edges
            ),
            (1, 1, 0, 1)
        );
        assert_eq!(
            (
                r.impl_unresolved_macro,
                r.impl_unresolved_other,
                r.impl_no_source
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn a_generic_bound_is_not_the_trait_and_a_where_clause_is_not_the_type() {
        let src =
            "impl<T: Bound> From<X<T>> for Y<T> {}\nimpl<T> Tr for T\nwhere\n    T: Bar,\n{\n}\n";
        let docs = one_document(vec![
            reference((0, 8, 0, 13), &sym("a/Bound#")),
            reference((0, 15, 0, 19), &sym("core/From#")),
            reference((0, 20, 0, 21), &sym("a/X#")),
            reference((0, 30, 0, 31), &sym("a/Y#")),
            reference((1, 8, 1, 10), &sym("a/Tr#")),
            reference((3, 7, 3, 10), &sym("a/Bar#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert_eq!(graph.impls.len(), 2);
        assert_eq!(graph.impls[0].trait_sym, sym("core/From#"));
        assert_eq!(
            graph.impls[0].type_sym.as_deref(),
            Some(sym("a/Y#").as_str())
        );
        assert_eq!(graph.impls[1].trait_sym, sym("a/Tr#"));
        // `T` has no type occurrence; `Bar` after `where` is not the type.
        assert_eq!(graph.impls[1].type_sym, None);
        assert_eq!(graph.impls[1].block, (1, 0, 5, 1));
        assert_eq!(graph.report.impl_headers, 2);
    }

    #[test]
    fn an_inherent_impl_gives_no_fact_and_no_edge() {
        let src = "pub struct Foo;\nimpl Foo {\n    fn m(&self) {}\n}\nimpl<T> Foo where T: for<'a> Fn(&'a u8) {}\n";
        let docs = one_document(vec![
            def((0, 11, 0, 14), &sym("a/Foo#"), (0, 0, 0, 15)),
            reference((1, 5, 1, 8), &sym("a/Foo#")),
            def((2, 7, 2, 8), &sym("a/impl#[Foo]m()."), (2, 4, 2, 18)),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert!(graph.impls.is_empty());
        assert_eq!(graph.report.impl_headers, 0);
        assert_eq!(graph.edge_count(GraphEdgeKind::Implements), 0);
    }

    #[test]
    fn a_higher_ranked_bound_is_not_the_for_of_the_header() {
        let src = "impl<F> Tr for F where F: for<'a> Fn(&'a u8) {}\nimpl<T: for<'a> Bar<'a>> Tr for T {}\n";
        let docs = one_document(vec![
            reference((0, 8, 0, 10), &sym("a/Tr#")),
            reference((0, 34, 0, 36), &sym("core/Fn#")),
            reference((1, 16, 1, 19), &sym("a/Bar#")),
            reference((1, 25, 1, 27), &sym("a/Tr#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert_eq!(graph.impls.len(), 2);
        for fact in &graph.impls {
            assert_eq!(fact.trait_sym, sym("a/Tr#"));
            assert_eq!(fact.type_sym, None);
        }
        assert_eq!(graph.impls[1].site.col, 25);
    }

    #[test]
    fn a_macro_header_is_counted_and_gives_nothing() {
        let src = "macro_rules! m {\n    ($t:ty) => {\n        impl $tr for $t {}\n    };\n}\n";
        let graph = ScipGraph::build(&one_document(Vec::new()), Some(&one_file("a.rs", src)));
        assert!(graph.impls.is_empty());
        let r = &graph.report;
        assert_eq!(
            (r.impl_headers, r.impl_resolved, r.impl_unresolved_macro),
            (1, 0, 1)
        );
        assert_eq!(r.impl_unresolved_other, 0);
    }

    #[test]
    fn without_source_nothing_is_read_and_the_documents_are_counted() {
        let docs = plain_documents();
        let none = ScipGraph::build(&docs, None);
        assert!(none.impls.is_empty());
        assert_eq!(none.report.impl_no_source, 1);
        assert_eq!(none.edge_count(GraphEdgeKind::Implements), 0);
        // A source that lacks the document counts it too.
        let other = one_file("b.rs", PLAIN);
        let missing = ScipGraph::build(&docs, Some(&other));
        assert!(missing.impls.is_empty());
        assert_eq!(missing.report.impl_no_source, 1);
    }

    #[test]
    fn a_memberless_marker_impl_is_a_fact_with_no_edge() {
        let src = "pub trait Marker {}\npub struct Foo;\nimpl Marker for Foo {}\nunsafe impl Marker for Foo {}\npub(crate) unsafe impl Marker for Foo {}\n";
        let docs = one_document(vec![
            def((0, 10, 0, 16), &sym("a/Marker#"), (0, 0, 0, 19)),
            reference((2, 5, 2, 11), &sym("a/Marker#")),
            reference((2, 16, 2, 19), &sym("a/Foo#")),
            reference((3, 12, 3, 18), &sym("a/Marker#")),
            reference((3, 23, 3, 26), &sym("a/Foo#")),
            reference((4, 23, 4, 29), &sym("a/Marker#")),
            reference((4, 34, 4, 37), &sym("a/Foo#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert_eq!(graph.impls.len(), 3);
        assert_eq!(graph.report.impl_edges, 0);
        assert_eq!(graph.report.impl_members_unlinked, 0);
        assert_eq!(graph.report.impl_external_trait, 0);
    }

    #[test]
    fn an_external_trait_is_counted_and_its_members_make_no_edge() {
        let src = "pub struct Foo;\nimpl Default for Foo {\n    fn default() -> Self { Foo }\n}\n";
        let docs = one_document(vec![
            def((0, 11, 0, 14), &sym("a/Foo#"), (0, 0, 0, 15)),
            reference(
                (1, 5, 1, 12),
                "rust-analyzer cargo core 0.0 default/Default#",
            ),
            reference((1, 17, 1, 20), &sym("a/Foo#")),
            def(
                (2, 7, 2, 14),
                &sym("a/impl#[Foo][Default]default()."),
                (2, 4, 2, 32),
            ),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert_eq!(graph.impls.len(), 1);
        assert_eq!(graph.report.impl_external_trait, 1);
        assert_eq!(graph.report.impl_members_external, 1);
        assert_eq!(graph.report.impl_members_unlinked, 0);
        assert_eq!(graph.edge_count(GraphEdgeKind::Implements), 0);
    }

    #[test]
    fn a_member_naming_another_trait_than_its_header_makes_no_edge() {
        let mut docs = plain_documents();
        docs[0].occurrences[5] = def((5, 7, 5, 8), &sym("a/impl#[Foo][Other]m()."), (5, 4, 5, 18));
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", PLAIN)));
        assert_eq!(graph.edge_count(GraphEdgeKind::Implements), 0);
        assert_eq!(graph.report.impl_members_unlinked, 1);
    }

    #[test]
    fn macro_expanded_members_with_a_shared_range_still_link_after_the_repair() {
        // `#[async_trait]` gives every member the range of the whole block.
        let src = "pub trait Tr {\n    fn one(&self);\n    fn two(&self);\n}\nimpl Tr for Foo {\n    fn one(&self) { self.two(); }\n    fn two(&self) {}\n}\n";
        let block = (4, 0, 7, 1);
        let docs = one_document(vec![
            def((0, 10, 0, 12), &sym("a/Tr#"), (0, 0, 3, 1)),
            def((1, 7, 1, 10), &sym("a/Tr#one()."), (1, 4, 1, 18)),
            def((2, 7, 2, 10), &sym("a/Tr#two()."), (2, 4, 2, 18)),
            reference((4, 5, 4, 7), &sym("a/Tr#")),
            reference((4, 12, 4, 15), &sym("a/Foo#")),
            def((5, 7, 5, 10), &sym("a/impl#[Foo][Tr]one()."), block),
            def((6, 7, 6, 10), &sym("a/impl#[Foo][Tr]two()."), block),
            reference((5, 25, 5, 28), &sym("a/impl#[Foo][Tr]two().")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        let node = |s: &str| graph.node_index(&sym(s)).unwrap();
        assert!(graph.has_edge(
            node("a/impl#[Foo][Tr]one()."),
            node("a/Tr#one()."),
            GraphEdgeKind::Implements
        ));
        assert!(graph.has_edge(
            node("a/impl#[Foo][Tr]two()."),
            node("a/Tr#two()."),
            GraphEdgeKind::Implements
        ));
        assert!(graph.has_edge(
            node("a/impl#[Foo][Tr]one()."),
            node("a/impl#[Foo][Tr]two()."),
            GraphEdgeKind::CallsOrRefs
        ));
        assert_eq!(graph.report.impl_edges, 2);
    }

    #[test]
    fn a_negative_impl_gives_no_fact_and_is_counted() {
        let src = "pub trait Tr {}\npub struct Foo;\nimpl !Tr for Foo {}\nimpl<T> !Sync for Wrapper<T> {}\n";
        let docs = one_document(vec![
            def((0, 10, 0, 12), &sym("a/Tr#"), (0, 0, 0, 15)),
            reference((2, 6, 2, 8), &sym("a/Tr#")),
            reference((2, 13, 2, 16), &sym("a/Foo#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert!(graph.impls.is_empty());
        assert_eq!(graph.report.impl_negative, 2);
        assert_eq!(graph.report.impl_headers, 0);
        assert_eq!(graph.edge_count(GraphEdgeKind::Implements), 0);
    }

    #[test]
    fn comments_between_the_tokens_of_a_header_are_skipped() {
        let src = "pub struct Foo;\nimpl /* c */ Tr // x\n for Foo {}\n";
        let docs = one_document(vec![
            reference((1, 13, 1, 15), &sym("a/Tr#")),
            reference((2, 5, 2, 8), &sym("a/Foo#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert_eq!(graph.impls.len(), 1);
        assert_eq!(graph.impls[0].trait_sym, sym("a/Tr#"));
        assert_eq!(
            graph.impls[0].type_sym.as_deref(),
            Some(sym("a/Foo#").as_str())
        );
        assert_eq!(graph.impls[0].header_line, 1);
    }

    #[test]
    fn an_impl_of_a_trait_object_is_inherent_and_gives_nothing() {
        let src = "impl dyn Tr {}\nimpl dyn Tr + Send {\n    fn m(&self) {}\n}\n";
        let docs = one_document(vec![
            reference((0, 9, 0, 11), &sym("a/Tr#")),
            reference((1, 9, 1, 11), &sym("a/Tr#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert!(graph.impls.is_empty());
        assert_eq!(graph.report.impl_headers, 0);
    }

    #[test]
    fn a_reference_type_with_a_lifetime_and_mut_is_read_to_its_type() {
        let src = "impl<'a> Tr for &'a mut Foo {}\n";
        let docs = one_document(vec![
            reference((0, 9, 0, 11), &sym("a/Tr#")),
            reference((0, 24, 0, 27), &sym("a/Foo#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert_eq!(graph.impls.len(), 1);
        assert_eq!(graph.impls[0].trait_sym, sym("a/Tr#"));
        assert_eq!(
            graph.impls[0].type_sym.as_deref(),
            Some(sym("a/Foo#").as_str())
        );
    }

    #[test]
    fn a_header_after_an_attribute_on_its_line_is_missed_and_counted() {
        let src = "#[cfg(x)] impl Tr for Foo {}\n#[derive(Debug)]\nstruct S;\n#[cfg(y)]\nimpl Tr for S {}\n";
        let docs = one_document(vec![
            reference((0, 15, 0, 17), &sym("a/Tr#")),
            reference((0, 22, 0, 25), &sym("a/Foo#")),
            reference((4, 5, 4, 7), &sym("a/Tr#")),
            reference((4, 12, 4, 13), &sym("a/S#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        // Only the header on its own line is read.
        assert_eq!(graph.impls.len(), 1);
        assert_eq!(graph.impls[0].header_line, 4);
        assert_eq!(graph.report.impl_after_attribute, 1);
    }

    #[test]
    fn columns_are_utf8_bytes_so_a_non_ascii_comment_in_a_header_is_harmless() {
        // `é` is two bytes: `Foo` starts at byte 21, character 20.
        let src = "impl Tr for /* é */ Foo {}\n";
        let docs = one_document(vec![
            reference((0, 5, 0, 7), &sym("a/Tr#")),
            reference((0, 21, 0, 24), &sym("a/Foo#")),
        ]);
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", src)));
        assert_eq!(graph.impls.len(), 1);
        assert_eq!(
            graph.impls[0].type_sym.as_deref(),
            Some(sym("a/Foo#").as_str())
        );
    }

    #[test]
    fn a_nested_impl_member_is_read_at_its_innermost_impl() {
        let nested = sym("a/impl#[Foo][Tr]m().impl#[Bar][Other]n().");
        assert_eq!(impl_member_parts(&nested), Some(("Bar", "Other", "n")));
    }

    #[test]
    fn a_node_in_an_impl_block_that_is_no_member_is_counted() {
        let mut docs = plain_documents();
        docs[0].occurrences.push(def(
            (5, 12, 5, 17),
            &sym("a/impl#[Foo][Tr]m().inner()."),
            (5, 10, 5, 20),
        ));
        let graph = ScipGraph::build(&docs, Some(&one_file("a.rs", PLAIN)));
        assert_eq!(graph.report.impl_edges, 1);
        assert_eq!(graph.report.impl_members_unparsed, 1);
    }

    #[test]
    fn member_symbols_and_trait_names_are_read_with_or_without_backticks() {
        let plain = sym("a/impl#[Foo][Tr]m().");
        assert_eq!(impl_member_parts(&plain), Some(("Foo", "Tr", "m")));
        let ticks = sym("a/impl#[`Wrapper<R>`][`Service<Req>`]poll_ready().");
        let parts = impl_member_parts(&ticks).unwrap();
        assert_eq!(parts, ("Wrapper<R>", "Service<Req>", "poll_ready"));
        assert_eq!(group_trait_name(parts.1), "Service");
        assert_eq!(group_trait_name("fmt::Display"), "Display");
        // Inherent members, associated types and nested items are no trait members.
        assert_eq!(impl_member_parts(&sym("a/impl#[Foo]m().")), None);
        assert_eq!(impl_member_parts(&sym("a/impl#[Foo][Tr]Item#")), None);
        assert_eq!(
            impl_member_parts(&sym("a/impl#[Foo][Tr]m().inner().")),
            None
        );
        assert_eq!(symbol_type_name(&sym("a/b/Display#")), "Display");
        assert_eq!(
            symbol_type_name("rust-analyzer cargo core 0.0 fmt/Display#"),
            "Display"
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

    /// Measures the kill rule of the trait-impl derivation on a real index;
    /// prints the build report counters, the distinct type-to-trait pairs, a
    /// comparison with a sample of hand-checked pairs and the load time with
    /// and without the derivation. Not part of the suite:
    ///
    /// `LCI_KILL_SCIP=<index.scip> LCI_KILL_TREE=<tree> [LCI_KILL_SAMPLE=<sample.json>]
    /// [LCI_KILL_TIMING=1] cargo test -p lci-maps kill_rule -- --ignored --nocapture`
    ///
    /// The sample is a JSON array of `{file, line, trait, type}` (line
    /// 0-based, as the Python reference writes it). With `LCI_KILL_TIMING` set
    /// the graph is built twice more, with and without the derivation.
    #[test]
    #[ignore = "reads a real SCIP index and its tree (env LCI_KILL_SCIP, LCI_KILL_TREE)"]
    fn kill_rule_report() {
        use std::time::Instant;
        let var = |name: &str| std::env::var(name).ok();
        let (Some(scip), Some(tree)) = (var("LCI_KILL_SCIP"), var("LCI_KILL_TREE")) else {
            panic!("set LCI_KILL_SCIP and LCI_KILL_TREE");
        };
        let source = DirSource(PathBuf::from(&tree));
        let started = Instant::now();
        let documents = read_documents(Path::new(&scip)).expect("read the index");
        println!("read_documents: {:.2} s", started.elapsed().as_secs_f64());

        let started = Instant::now();
        let mut report = BuildReport::default();
        let facts = derive_impls(&documents, Some(&source), &mut report);
        println!(
            "derive_impls alone: {:.2} s",
            started.elapsed().as_secs_f64()
        );
        let r = &report;
        println!(
            "impl_headers {} resolved {} unresolved_macro {} unresolved_other {} no_source {} external_trait {}",
            r.impl_headers,
            r.impl_resolved,
            r.impl_unresolved_macro,
            r.impl_unresolved_other,
            r.impl_no_source,
            r.impl_external_trait
        );
        let pairs: HashSet<(&str, &str)> = facts
            .iter()
            .filter_map(|f| Some((f.type_sym.as_deref()?, f.trait_sym.as_str())))
            .collect();
        println!(
            "facts {} distinct type->trait {} facts without type {}",
            facts.len(),
            pairs.len(),
            facts.iter().filter(|f| f.type_sym.is_none()).count()
        );

        println!(
            "impl_negative {} impl_after_attribute {}",
            r.impl_negative, r.impl_after_attribute
        );
        {
            // Headers whose first line holds non-ASCII bytes: columns are
            // UTF-8 bytes, so these resolve like the others.
            let mut by_path: HashMap<&str, Vec<&ImplFact>> = HashMap::new();
            for fact in &facts {
                by_path.entry(fact.path.as_str()).or_default().push(fact);
            }
            let (mut non_ascii, mut with_type) = (0, 0);
            for (path, list) in by_path {
                let Some(bytes) = source.read(path) else {
                    continue;
                };
                let lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
                for fact in list {
                    if lines
                        .get(fact.header_line as usize)
                        .is_some_and(|l| !l.is_ascii())
                    {
                        non_ascii += 1;
                        with_type += usize::from(fact.type_sym.is_some());
                    }
                }
            }
            println!(
                "resolved headers whose first line holds non-ASCII bytes: {non_ascii} ({with_type} with a type)"
            );
        }

        if let Some(sample) = var("LCI_KILL_SAMPLE") {
            let text = std::fs::read_to_string(sample).expect("read the sample");
            let rows: Vec<serde_json::Value> = serde_json::from_str(&text).expect("sample json");
            let mut same = 0;
            for row in &rows {
                let (file, line) = (row["file"].as_str().unwrap(), row["line"].as_u64().unwrap());
                let found = facts
                    .iter()
                    .find(|f| f.path == file && u64::from(f.header_line) == line);
                let ok = found.is_some_and(|f| {
                    Some(f.trait_sym.as_str()) == row["trait"].as_str()
                        && f.type_sym.as_deref() == row["type"].as_str()
                });
                if !ok {
                    println!("MISMATCH {file}:{} ours {found:?} want {row}", line + 1);
                }
                same += usize::from(ok);
            }
            println!("sample: {same}/{} pairs reproduced", rows.len());
        }

        if let Some(rows) = var("LCI_KILL_ROWS") {
            // Every header row of the Python reference (`impl_edges.py`):
            // where do the two derivations differ?
            let text = std::fs::read_to_string(rows).expect("read the rows");
            let rows: Vec<serde_json::Value> = serde_json::from_str(&text).expect("rows json");
            let (mut same, mut ours_missing, mut trait_differs, mut type_differs) = (0, 0, 0, 0);
            let (mut py_unresolved, mut ours_extra) = (0, 0);
            let mut seen = HashSet::new();
            for row in &rows {
                let (file, line) = (row["file"].as_str().unwrap(), row["line"].as_u64().unwrap());
                seen.insert((file.to_string(), line));
                let found = facts
                    .iter()
                    .find(|f| f.path == file && u64::from(f.header_line) == line);
                let Some(want_trait) = row["trait"].as_str() else {
                    py_unresolved += 1;
                    ours_extra += usize::from(found.is_some());
                    continue;
                };
                let Some(f) = found else {
                    ours_missing += 1;
                    println!("ROW-MISSING {file}:{} {}", line + 1, row["text"]);
                    continue;
                };
                if f.trait_sym != want_trait {
                    trait_differs += 1;
                    println!("ROW-TRAIT {file}:{} {}", line + 1, row["text"]);
                } else if f.type_sym.as_deref() != row["type"].as_str() {
                    type_differs += 1;
                    println!(
                        "ROW-TYPE {file}:{} ours {:?} python {} | {}",
                        line + 1,
                        f.type_sym,
                        row["type"],
                        row["text"]
                    );
                } else {
                    same += 1;
                }
            }
            let ours_only = facts
                .iter()
                .filter(|f| !seen.contains(&(f.path.clone(), u64::from(f.header_line))))
                .count();
            println!(
                "rows {}: same {same}, python-unresolved {py_unresolved} (ours resolved {ours_extra}), ours missing {ours_missing}, trait differs {trait_differs}, type differs {type_differs}; facts without a python row {ours_only}",
                rows.len()
            );
        }

        let started = Instant::now();
        let graph = ScipGraph::build(&documents, Some(&source));
        println!(
            "build with the derivation:{:.2} s; implements edges {} (members: external trait {}, other unlinked {})",
            started.elapsed().as_secs_f64(),
            graph.edge_count(GraphEdgeKind::Implements),
            graph.report.impl_members_external,
            graph.report.impl_members_unlinked
        );
        if var("LCI_KILL_TIMING").is_some() {
            for round in 0..2 {
                let started = Instant::now();
                let without = ScipGraph::build_with(&documents, Some(&source), false);
                let off = started.elapsed().as_secs_f64();
                let started = Instant::now();
                let with = ScipGraph::build_with(&documents, Some(&source), true);
                let on = started.elapsed().as_secs_f64();
                println!(
                    "round {round}: build without {off:.2} s, with {on:.2} s ({:+.1} %); nodes {}/{}",
                    (on / off - 1.0) * 100.0,
                    without.nodes.len(),
                    with.nodes.len()
                );
            }
        }
    }

    #[test]
    fn an_empty_graph_has_no_nodes_and_resolves_no_symbol() {
        let graph = ScipGraph::empty();
        assert!(graph.nodes.is_empty() && graph.out.is_empty() && graph.impls.is_empty());
        assert_eq!(graph.node_index(&sym("a/alpha().")), None);
        for kind in [
            GraphEdgeKind::CallsOrRefs,
            GraphEdgeKind::Contains,
            GraphEdgeKind::Implements,
        ] {
            assert_eq!(graph.edge_count(kind), 0);
        }
        assert_eq!(graph.report.documents, 0);
        let map = ScipMap::with_default_ids(graph);
        assert!(map.graph().nodes.is_empty());
    }

    /// An index file with one document: `alpha` carries two documentation
    /// lines, `beta` a long one, `gamma` none, and a type symbol has some too.
    #[test]
    fn read_symbol_docs_keeps_function_documentation_cut_at_the_limit() {
        use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};

        let (alpha, beta, gamma) = (sym("a/alpha()."), sym("a/beta()."), sym("a/gamma()."));
        let ty = sym("a/Thing#");
        let mut document = Document::new();
        document.relative_path = "a.rs".into();
        for (i, symbol) in [&alpha, &beta, &gamma].into_iter().enumerate() {
            let mut occurrence = Occurrence::new();
            occurrence.range = vec![i as i32, 3, 8];
            occurrence.symbol = symbol.clone();
            occurrence.symbol_roles = SymbolRole::Definition as i32;
            document.occurrences.push(occurrence);
        }
        let long = "é".repeat(MAX_DOC_BYTES);
        for (symbol, lines) in [
            (
                &alpha,
                vec![
                    "```rust\nfn alpha()\n```".to_owned(),
                    "Parses the config.".to_owned(),
                ],
            ),
            (&beta, vec![long.clone()]),
            (&gamma, vec!["  ".to_owned()]),
            (&ty, vec!["A type, not a node.".to_owned()]),
        ] {
            let mut info = SymbolInformation::new();
            info.symbol = symbol.clone();
            info.documentation = lines;
            document.symbols.push(info);
        }
        let mut index = Index::new();
        index.documents.push(document);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.scip");
        std::fs::write(&path, index.write_to_bytes().unwrap()).unwrap();

        // The graph and `read_documents` carry no documentation: a load
        // without the entity seeds does not read or keep any.
        let documents = read_documents(&path).unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].occurrences.len(), 3);
        assert_eq!(ScipGraph::load(&path, None).unwrap().nodes.len(), 3);

        // Function symbols with a non-blank line only, the lines joined.
        let docs = read_symbol_docs(&path).unwrap();
        assert_eq!(docs.len(), 2, "{docs:?}");
        assert!(!docs.contains_key(&ty));
        assert_eq!(docs[&alpha], "```rust\nfn alpha()\n```\nParses the config.");
        // Cut at MAX_DOC_BYTES on a character boundary (`é` is two bytes).
        let beta_doc = &docs[&beta];
        assert_eq!(beta_doc.len(), MAX_DOC_BYTES);
        assert!(beta_doc.chars().all(|c| c == 'é'));
        assert!(!docs.contains_key(&gamma));
        assert!(read_symbol_docs(&dir.path().join("missing.scip")).is_err());
        // The path names of a symbol: the package, then the descriptors.
        assert_eq!(
            symbol_path_names(&sym("a/Thing#run().")),
            ["demo", "a", "Thing", "run"]
        );
        assert!(symbol_path_names("not a symbol").is_empty());
    }

    #[test]
    fn map_construction_rejects_wrong_or_repeated_ids() {
        let graph = ScipGraph::build(&demo_documents(), None);
        assert!(ScipMap::new(graph.clone(), Vec::new()).is_err());
        let one = NodeId::file("a.rs").unwrap();
        assert!(ScipMap::new(graph, vec![one; 4]).is_err());
    }
}
