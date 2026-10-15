//! The exact one-hop reverse closure of a SCIP symbol, the anchor rules that
//! choose the symbol from a query and recorded retrieval hits, and the control
//! arms of slice M1.4 of the multimap spike v2 (pre-registration in
//! `docs/spike/multimap/v2/m1.4-prereg.md`).
//!
//! Units follow the `impact` gold's declaration rule: functions and methods,
//! structs, enums, traits, type aliases and constants or statics. Enum
//! variants, fields and associated types are not units: a reference inside one
//! belongs to the enclosing unit. A reference occurrence is attributed to the
//! innermost unit whose enclosing range holds it.

use crate::scip_graph::{
    Def, DirSource, GraphEdgeKind, Range, RawDocument, ScipGraph, SourceText, Structural,
    collect_definitions, innermost, read_documents, repair_enclosing,
};
use crate::walk::SplitMix;
use protobuf::Message;
use scip::types::descriptor::Suffix;
use scip::types::symbol_information::Kind;
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::Path,
    process::Command,
};

const ROLE_DEFINITION: i32 = 1;

/// One answer: the name line (1-based) of a declaration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Decl {
    pub path: String,
    pub line: u32,
    pub name: String,
    pub symbol: String,
}

/// A symbol's descriptor names, as the question would write them.
#[derive(Debug, Clone)]
pub struct Chain {
    /// Module names, owner (impl type or enclosing type), then the name.
    pub names: Vec<String>,
    /// The owner, when the symbol is a member of a type.
    pub owner: Option<String>,
}

/// Descriptor names of a symbol. The `impl` descriptor and its type
/// parameters are replaced by the first type parameter (the implementing
/// type). `None` for symbols that do not parse.
pub fn chain_of(symbol: &str) -> Option<Chain> {
    let parsed = scip::symbol::parse_symbol(symbol).ok()?;
    let suffix = |d: &scip::types::Descriptor| d.suffix.enum_value().ok();
    let ds = &parsed.descriptors;
    let mut names: Vec<String> = Vec::new();
    let mut is_type: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < ds.len() {
        let d = &ds[i];
        if suffix(d) == Some(Suffix::Type) && d.name == "impl" {
            if let Some(tp) = ds
                .get(i + 1)
                .filter(|n| suffix(n) == Some(Suffix::TypeParameter))
            {
                names.push(tp.name.clone());
                is_type.push(true);
                i += 1;
                while ds
                    .get(i + 1)
                    .is_some_and(|n| suffix(n) == Some(Suffix::TypeParameter))
                {
                    i += 1;
                }
            }
            i += 1;
            continue;
        }
        names.push(d.name.clone());
        is_type.push(suffix(d) == Some(Suffix::Type));
        i += 1;
    }
    if names.is_empty() {
        return None;
    }
    let owner =
        (names.len() >= 2 && is_type[names.len() - 2]).then(|| names[names.len() - 2].clone());
    Some(Chain { names, owner })
}

/// Code spans of a query that look like paths: `A`, `a::B`, a trailing `()`
/// and generic arguments removed; leading `crate`, `self`, `super` dropped.
/// Spans of any other form are not names. Returns (names, rejected spans).
pub fn code_names(query: &str) -> (Vec<Vec<String>>, usize) {
    let parts: Vec<&str> = query.split('`').collect();
    let mut names: Vec<Vec<String>> = Vec::new();
    let mut rejected = 0;
    let spans = parts.len() / 2;
    for k in 0..spans {
        let raw = parts[2 * k + 1];
        if raw.is_empty() {
            continue;
        }
        let mut text = raw.trim().to_string();
        // Generic arguments (nesting aware), then a trailing `()`.
        let mut depth = 0u32;
        text = text
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
        let text = text
            .trim()
            .strip_suffix("()")
            .unwrap_or(text.trim())
            .to_string();
        let segs: Vec<&str> = text.split("::").collect();
        let ident = |s: &str| {
            !s.is_empty()
                && !s.starts_with(|c: char| c.is_ascii_digit())
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        if segs.is_empty() || !segs.iter().all(|s| ident(s)) {
            rejected += 1;
            continue;
        }
        let mut segs: Vec<String> = segs.iter().map(|s| (*s).to_string()).collect();
        while segs.len() > 1 && matches!(segs[0].as_str(), "crate" | "self" | "super") {
            segs.remove(0);
        }
        if !names.contains(&segs) {
            names.push(segs);
        }
    }
    (names, rejected)
}

/// A recorded retrieval chunk.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub rank: u32,
    pub path: String,
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
}

/// An anchor choice: the definition index, the rank of the chunk it came
/// from (C only) and the number of name segments that matched.
#[derive(Debug, Clone)]
pub struct Anchor {
    pub def: usize,
    pub chunk_rank: Option<u32>,
    pub segments: usize,
}

/// One declaration of a [`RankedClosure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedDecl {
    /// Index into [`ImpactIndex::defs`].
    pub def: usize,
    /// Non-definition occurrences of the anchor symbol inside this declaration.
    pub held: usize,
    pub decl: Decl,
}

/// What arm N makes of one question: see [`ImpactIndex::ranked_closure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedClosure {
    /// Code names the question gave.
    pub names: usize,
    /// Whether a name matched a symbol of the index.
    pub anchored: bool,
    /// The anchor's closure, ordered; empty unless `anchored`.
    pub decls: Vec<RankedDecl>,
}

fn is_unit_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Function
            | Kind::Method
            | Kind::TraitMethod
            | Kind::StaticMethod
            | Kind::Struct
            | Kind::Class
            | Kind::Enum
            | Kind::Trait
            | Kind::Interface
            | Kind::TypeAlias
            | Kind::Constant
            | Kind::StaticVariable
            | Kind::StaticProperty
    )
}

/// The SCIP index, indexed for closures and anchors.
pub struct ImpactIndex {
    pub docs: Vec<RawDocument>,
    pub defs: Vec<Def>,
    pub kinds: Vec<Option<Kind>>,
    pub unit: Vec<bool>,
    pub chains: Vec<Option<Chain>>,
    def_of: HashMap<String, usize>,
    /// Per file: units with an enclosing range.
    scope: HashMap<String, Vec<(Range, usize)>>,
    /// Non-definition occurrences of non-local symbols: (document, line, col).
    refs: HashMap<String, Vec<(usize, u32, u32)>>,
    by_file: HashMap<String, Vec<usize>>,
    pub repaired: usize,
    pub unparsed_symbols: usize,
}

impl ImpactIndex {
    pub fn load(path: &Path, source: Option<&dyn SourceText>) -> Result<Self, String> {
        let docs = read_documents(path)?;
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let index = scip::types::Index::parse_from_bytes(&bytes)
            .map_err(|e| format!("parse {}: {e}", path.display()))?;
        let mut kind_of: HashMap<String, Kind> = HashMap::new();
        for document in &index.documents {
            for info in &document.symbols {
                if let Ok(kind) = info.kind.enum_value() {
                    kind_of.entry(info.symbol.clone()).or_insert(kind);
                }
            }
        }
        Ok(Self::build(docs, &kind_of, source))
    }

    pub fn build(
        docs: Vec<RawDocument>,
        kind_of: &HashMap<String, Kind>,
        source: Option<&dyn SourceText>,
    ) -> Self {
        let mut defs = collect_definitions(&docs);
        let mut report = crate::scip_graph::BuildReport::default();
        repair_enclosing(&mut defs, source, &mut report);
        let kinds: Vec<Option<Kind>> = defs
            .iter()
            .map(|d| kind_of.get(&d.symbol).copied())
            .collect();
        let unit: Vec<bool> = defs
            .iter()
            .zip(&kinds)
            .map(|(d, k)| d.enclosing.is_some() && k.is_some_and(is_unit_kind))
            .collect();
        let chains: Vec<Option<Chain>> = defs.iter().map(|d| chain_of(&d.symbol)).collect();
        let unparsed_symbols = chains.iter().filter(|c| c.is_none()).count();
        let mut scope: HashMap<String, Vec<(Range, usize)>> = HashMap::new();
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, d) in defs.iter().enumerate() {
            by_file.entry(d.path.clone()).or_default().push(i);
            if let (true, Some(enc)) = (unit[i], d.enclosing) {
                scope.entry(d.path.clone()).or_default().push((enc, i));
            }
        }
        let def_of: HashMap<String, usize> = defs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.symbol.clone(), i))
            .collect();
        let mut refs: HashMap<String, Vec<(usize, u32, u32)>> = HashMap::new();
        for (di, doc) in docs.iter().enumerate() {
            for occ in &doc.occurrences {
                if occ.roles & ROLE_DEFINITION != 0 || occ.symbol.starts_with("local ") {
                    continue;
                }
                refs.entry(occ.symbol.clone())
                    .or_default()
                    .push((di, occ.range.0, occ.range.1));
            }
        }
        Self {
            docs,
            defs,
            kinds,
            unit,
            chains,
            def_of,
            scope,
            refs,
            by_file,
            repaired: report.repaired,
            unparsed_symbols,
        }
    }

    pub fn def_index(&self, symbol: &str) -> Option<usize> {
        self.def_of.get(symbol).copied()
    }

    /// Definitions of a file (indices into `defs`).
    pub fn defs_in_file(&self, path: &str) -> &[usize] {
        self.by_file.get(path).map_or(&[][..], Vec::as_slice)
    }

    /// Paths of the indexed documents.
    pub fn doc_paths(&self) -> BTreeSet<&str> {
        self.docs.iter().map(|d| d.path.as_str()).collect()
    }

    /// Non-definition occurrences of a symbol: (path, line, col), zero-based.
    pub fn occurrences(&self, symbol: &str) -> Vec<(String, u32, u32)> {
        self.refs
            .get(symbol)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|&(di, line, col)| (self.docs[di].path.clone(), line, col))
            .collect()
    }

    pub fn ref_count(&self, def: usize) -> usize {
        self.refs.get(&self.defs[def].symbol).map_or(0, Vec::len)
    }

    /// The unit holding a position of a file.
    pub fn unit_at(&self, path: &str, line: u32, col: u32) -> Option<usize> {
        let scope = self.scope.get(path)?;
        innermost(scope, line, col)
    }

    /// `Owner::name` for members, else the name.
    pub fn display_name(&self, def: usize) -> String {
        match &self.chains[def] {
            Some(c) => match &c.owner {
                Some(o) => format!("{o}::{}", c.names[c.names.len() - 1]),
                None => c.names[c.names.len() - 1].clone(),
            },
            None => self.defs[def].symbol.clone(),
        }
    }

    pub fn decl(&self, def: usize) -> Decl {
        let d = &self.defs[def];
        Decl {
            path: d.path.clone(),
            line: d.name_range.0 + 1,
            name: self.display_name(def),
            symbol: d.symbol.clone(),
        }
    }

    pub fn unit_defs(&self) -> Vec<usize> {
        (0..self.defs.len()).filter(|&i| self.unit[i]).collect()
    }

    /// The exact one-hop reverse closure: the units holding a non-definition
    /// occurrence of the symbol. Sorted by (path, line, symbol).
    pub fn closure(&self, symbol: &str) -> Vec<usize> {
        let mut set: BTreeSet<(String, u32, String, usize)> = BTreeSet::new();
        for &(di, line, col) in self.refs.get(symbol).map_or(&[][..], Vec::as_slice) {
            let path = &self.docs[di].path;
            if let Some(u) = self.unit_at(path, line, col) {
                let d = &self.defs[u];
                set.insert((d.path.clone(), d.name_range.0, d.symbol.clone(), u));
            }
        }
        set.into_iter().map(|t| t.3).collect()
    }

    pub fn matches(&self, def: usize, name: &[String]) -> bool {
        self.chains[def].as_ref().is_some_and(|c| {
            c.names.len() >= name.len() && c.names[c.names.len() - name.len()..] == *name
        })
    }

    /// Arm C: a matching symbol defined inside one of the first `k` chunks;
    /// best-ranked chunk, then most segments, earliest line, symbol order.
    pub fn anchor_c(&self, names: &[Vec<String>], evidence: &[Chunk], k: u32) -> Option<Anchor> {
        let mut chunks: Vec<&Chunk> = evidence
            .iter()
            .filter(|c| c.rank >= 1 && c.rank <= k)
            .collect();
        chunks.sort_by_key(|c| c.rank);
        for chunk in chunks {
            let mut best: Option<(CKey<'_>, usize, usize)> = None;
            for &def in self.by_file.get(&chunk.path).map_or(&[][..], Vec::as_slice) {
                let d = &self.defs[def];
                let line1 = d.name_range.0 + 1;
                if line1 < chunk.start_line || line1 > chunk.end_line {
                    continue;
                }
                let segs = names
                    .iter()
                    .filter(|n| self.matches(def, n))
                    .map(Vec::len)
                    .max();
                let Some(segs) = segs else { continue };
                let key = (std::cmp::Reverse(segs), d.name_range.0, d.symbol.as_str());
                if best.as_ref().is_none_or(|(b, _, _)| key < *b) {
                    best = Some((key, def, segs));
                }
            }
            if let Some((_, def, segments)) = best {
                return Some(Anchor {
                    def,
                    chunk_rank: Some(chunk.rank),
                    segments,
                });
            }
        }
        None
    }

    /// Arm N: every matching symbol of the index; most non-definition
    /// occurrences, then most segments, earliest line, symbol order.
    pub fn anchor_n(&self, names: &[Vec<String>]) -> Option<Anchor> {
        let mut best: Option<(NKey<'_>, usize, usize)> = None;
        for def in 0..self.defs.len() {
            let segs = names
                .iter()
                .filter(|n| self.matches(def, n))
                .map(Vec::len)
                .max();
            let Some(segs) = segs else { continue };
            let d = &self.defs[def];
            let key = (
                std::cmp::Reverse(self.ref_count(def)),
                std::cmp::Reverse(segs),
                d.name_range.0,
                d.symbol.as_str(),
            );
            if best.as_ref().is_none_or(|(b, _, _)| key < *b) {
                best = Some((key, def, segs));
            }
        }
        best.map(|(_, def, segments)| Anchor {
            def,
            chunk_rank: None,
            segments,
        })
    }

    /// Arm N for one question: the code names of the question, the anchor
    /// ([`ImpactIndex::anchor_n`]) and its one-hop reverse closure, ordered by
    /// the anchor's occurrences each declaration holds (most first), then path,
    /// line and symbol. The whole closure is returned; a caller cuts it.
    pub fn ranked_closure(&self, question: &str) -> RankedClosure {
        let (names, _) = code_names(question);
        let Some(anchor) = self.anchor_n(&names) else {
            return RankedClosure {
                names: names.len(),
                anchored: false,
                decls: Vec::new(),
            };
        };
        let symbol = &self.defs[anchor.def].symbol;
        let mut held: HashMap<usize, usize> = HashMap::new();
        for (path, line, col) in self.occurrences(symbol) {
            if let Some(unit) = self.unit_at(&path, line, col) {
                *held.entry(unit).or_default() += 1;
            }
        }
        let mut decls: Vec<RankedDecl> = self
            .closure(symbol)
            .into_iter()
            .map(|def| RankedDecl {
                def,
                held: held.get(&def).copied().unwrap_or(0),
                decl: self.decl(def),
            })
            .collect();
        decls.sort_by(|a, b| {
            b.held.cmp(&a.held).then_with(|| {
                (&a.decl.path, a.decl.line, &a.decl.symbol).cmp(&(
                    &b.decl.path,
                    b.decl.line,
                    &b.decl.symbol,
                ))
            })
        });
        RankedClosure {
            names: names.len(),
            anchored: true,
            decls,
        }
    }
}

type CKey<'a> = (std::cmp::Reverse<usize>, u32, &'a str);
type NKey<'a> = (
    std::cmp::Reverse<usize>,
    std::cmp::Reverse<usize>,
    u32,
    &'a str,
);
/// Lines of a source file and the set of its code byte positions.
type FileData = (Vec<Vec<u8>>, HashSet<(usize, usize)>);
type InEdge<'a> = (usize, (&'a str, u32, u32));

/// Source files read once, with their code bytes (outside comments, strings
/// and character literals; multi-line strings are not tracked).
pub struct SourceCache<'a> {
    source: &'a dyn SourceText,
    files: HashMap<String, Option<FileData>>,
}

impl<'a> SourceCache<'a> {
    pub fn new(source: &'a dyn SourceText) -> Self {
        Self {
            source,
            files: HashMap::new(),
        }
    }

    fn file(&mut self, path: &str) -> Option<&FileData> {
        if !self.files.contains_key(path) {
            let entry = self.source.read(path).map(|bytes| {
                let lines: Vec<Vec<u8>> =
                    bytes.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
                let code: HashSet<(usize, usize)> = Structural {
                    lines: &lines,
                    line: 0,
                    col: 0,
                    block_depth: 0,
                }
                .map(|(l, c, _)| (l, c))
                .collect();
                (lines, code)
            });
            self.files.insert(path.to_string(), entry);
        }
        self.files.get(path).and_then(Option::as_ref)
    }

    /// The lines of a file, split at line feeds (a carriage return may remain).
    pub fn lines(&mut self, path: &str) -> Option<&Vec<Vec<u8>>> {
        self.file(path).map(|(lines, _)| lines)
    }

    /// Whether the position (zero-based) is a code byte.
    pub fn is_code(&mut self, path: &str, line: usize, col: usize) -> bool {
        self.file(path)
            .is_some_and(|(_, code)| code.contains(&(line, col)))
    }

    /// Whether the file holds the word as a whole identifier.
    pub fn has_word(&mut self, path: &str, word: &str) -> bool {
        let Some((lines, _)) = self.file(path) else {
            return false;
        };
        let w = word.as_bytes();
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        lines.iter().any(|line| {
            line.windows(w.len()).enumerate().any(|(i, win)| {
                win == w
                    && (i == 0 || !ident(line[i - 1]))
                    && line.get(i + w.len()).is_none_or(|&b| !ident(b))
            })
        })
    }

    /// Whether the unit carries a test attribute (`#[test]`, `#[tokio::test`)
    /// between its range start and name line, or within 12 lines above it.
    pub fn is_test(&mut self, def: &Def) -> bool {
        let name_line = def.name_range.0 as usize;
        let from_enc = def.enclosing.map_or(name_line, |e| e.0 as usize);
        let from = from_enc.min(name_line.saturating_sub(12));
        let Some((lines, _)) = self.file(&def.path) else {
            return false;
        };
        (from..=name_line).filter_map(|l| lines.get(l)).any(|line| {
            let t = String::from_utf8_lossy(line);
            t.contains("#[test]") || t.contains("#[tokio::test")
        })
    }
}

/// Hits of `rg -w` for a name: (path, line, col), zero-based, in rg's sorted order.
pub fn rg_hits(root: &Path, name: &str) -> Result<Vec<(String, usize, usize)>, String> {
    let out = Command::new("rg")
        .current_dir(root)
        .args([
            "--vimgrep",
            "-w",
            "-F",
            "--no-config",
            "--color",
            "never",
            "--sort",
            "path",
            "--path-separator",
            "/",
            "-g",
            "*.rs",
            "-e",
            name,
            ".",
        ])
        .output()
        .map_err(|e| format!("rg: {e}"))?;
    if !out.status.success() && out.status.code() != Some(1) {
        return Err(format!(
            "rg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut hits = Vec::new();
    for line in text.lines() {
        let mut it = line.splitn(4, ':');
        let (Some(p), Some(l), Some(c)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(l), Ok(c)) = (l.parse::<usize>(), c.parse::<usize>()) else {
            continue;
        };
        hits.push((p.trim_start_matches("./").to_string(), l - 1, c - 1));
    }
    Ok(hits)
}

/// Arm V: `rg -w` of the anchor's bare name, hits in code only, owner check
/// for members, enclosing unit by position only; anchor's own unit removed.
pub fn arm_v(
    ix: &ImpactIndex,
    root: &Path,
    cache: &mut SourceCache<'_>,
    anchor: usize,
) -> Result<Vec<usize>, String> {
    let Some(chain) = &ix.chains[anchor] else {
        return Ok(Vec::new());
    };
    let name = &chain.names[chain.names.len() - 1];
    let mut set: BTreeSet<(String, u32, String, usize)> = BTreeSet::new();
    for (path, line, col) in rg_hits(root, name)? {
        if !cache.is_code(&path, line, col) {
            continue;
        }
        if let Some(owner) = &chain.owner
            && !cache.has_word(&path, owner)
        {
            continue;
        }
        let Some(u) = ix.unit_at(&path, line as u32, col as u32) else {
            continue;
        };
        if u == anchor {
            continue;
        }
        let d = &ix.defs[u];
        set.insert((d.path.clone(), d.name_range.0, d.symbol.clone(), u));
    }
    Ok(set.into_iter().map(|t| t.3).collect())
}

/// Arm R: `n` units drawn uniformly without replacement.
pub fn arm_r(units: &[usize], n: usize, seed: u64) -> Vec<usize> {
    let mut pool = units.to_vec();
    let mut rng = SplitMix(seed);
    let n = n.min(pool.len());
    for i in 0..n {
        let j = i + (rng.next_u64() % (pool.len() - i) as u64) as usize;
        pool.swap(i, j);
    }
    let mut out: Vec<usize> = pool[..n].to_vec();
    out.sort_unstable();
    out
}

/// Arm B: the first `n` nodes of an undirected breadth-first walk over the
/// `calls_or_refs` edges of the M0.4 graph, from the unit holding `start`.
pub fn arm_b(graph: &ScipGraph, start_symbol: &str, n: usize) -> Option<Vec<String>> {
    let start = graph.node_index(start_symbol)?;
    let mut incoming: Vec<Vec<InEdge<'_>>> = vec![Vec::new(); graph.nodes.len()];
    for (from, edges) in graph.out.iter().enumerate() {
        for e in edges
            .iter()
            .filter(|e| e.kind == GraphEdgeKind::CallsOrRefs)
        {
            incoming[e.to].push((
                from,
                (e.witness.file.as_str(), e.witness.line, e.witness.col),
            ));
        }
    }
    for list in &mut incoming {
        list.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    }
    let mut seen = vec![false; graph.nodes.len()];
    seen[start] = true;
    let mut queue = VecDeque::from([start]);
    let mut out = Vec::new();
    while let Some(u) = queue.pop_front() {
        if out.len() >= n {
            break;
        }
        let outs = graph.out[u]
            .iter()
            .filter(|e| e.kind == GraphEdgeKind::CallsOrRefs)
            .map(|e| e.to);
        let ins = incoming[u].iter().map(|t| t.0);
        for v in outs.chain(ins) {
            if !seen[v] {
                seen[v] = true;
                if out.len() < n {
                    out.push(graph.nodes[v].symbol.clone());
                }
                queue.push_back(v);
            }
        }
    }
    Some(out)
}

/// Arms OD and CD: from the one-hop closure as hop 1, walk the `calls_or_refs`
/// edges backwards to `max_depth` hops; return the symbols of every reached
/// node (the caller keeps the test functions).
pub fn reverse_reach(graph: &ScipGraph, hop1: &[String], max_depth: usize) -> Vec<String> {
    let mut callers: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
    for (from, edges) in graph.out.iter().enumerate() {
        for e in edges
            .iter()
            .filter(|e| e.kind == GraphEdgeKind::CallsOrRefs)
        {
            callers[e.to].push(from);
        }
    }
    let mut seen = vec![false; graph.nodes.len()];
    let mut frontier: Vec<usize> = Vec::new();
    for s in hop1 {
        if let Some(i) = graph.node_index(s)
            && !seen[i]
        {
            seen[i] = true;
            frontier.push(i);
        }
    }
    let mut reached = frontier.clone();
    for _ in 1..max_depth {
        let mut next = Vec::new();
        for &u in &frontier {
            for &c in &callers[u] {
                if !seen[c] {
                    seen[c] = true;
                    next.push(c);
                }
            }
        }
        reached.extend(&next);
        frontier = next;
    }
    reached
        .into_iter()
        .map(|i| graph.nodes[i].symbol.clone())
        .collect()
}

/// Convenience for the driver: source text under a checkout.
pub fn dir_source(root: &Path) -> DirSource {
    DirSource(root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scip_graph::RawOccurrence;

    fn sym(path: &str) -> String {
        format!("rust-analyzer cargo demo 0.1.0 {path}")
    }

    fn occ(range: Range, symbol: &str, roles: i32, enclosing: Option<Range>) -> RawOccurrence {
        RawOccurrence {
            range,
            symbol: symbol.to_string(),
            roles,
            enclosing,
        }
    }

    /// a.rs: `struct Foo` (line 0), `impl Foo { fn run() }` (line 1), `fn use_foo() { Foo::run(); }`
    /// (line 2), `fn deep() { use_foo(); }` (line 3), `enum E { V(Foo) }` (line 4).
    fn demo() -> ImpactIndex {
        let foo = sym("a/Foo#");
        let run = sym("a/impl#[Foo]run().");
        let use_foo = sym("a/use_foo().");
        let deep = sym("a/deep().");
        let e = sym("a/E#");
        let v = sym("a/E#V#");
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 7, 0, 10), &foo, 1, Some((0, 0, 0, 14))),
                occ((1, 14, 1, 17), &run, 1, Some((1, 11, 1, 30))),
                occ((2, 3, 2, 10), &use_foo, 1, Some((2, 0, 2, 40))),
                occ((2, 15, 2, 18), &foo, 0, None),
                occ((2, 20, 2, 23), &run, 0, None),
                occ((3, 3, 3, 7), &deep, 1, Some((3, 0, 3, 30))),
                occ((3, 12, 3, 19), &use_foo, 0, None),
                occ((4, 5, 4, 6), &e, 1, Some((4, 0, 4, 20))),
                occ((4, 9, 4, 10), &v, 1, Some((4, 9, 4, 15))),
                occ((4, 11, 4, 14), &foo, 0, None),
            ],
        }];
        let kinds: HashMap<String, Kind> = [
            (foo, Kind::Struct),
            (run, Kind::Method),
            (use_foo, Kind::Function),
            (deep, Kind::Function),
            (e, Kind::Enum),
            (v, Kind::EnumMember),
        ]
        .into_iter()
        .collect();
        ImpactIndex::build(docs, &kinds, None)
    }

    #[test]
    fn closure_attributes_to_innermost_unit_and_skips_variants() {
        let ix = demo();
        let names: Vec<String> = ix
            .closure(&sym("a/Foo#"))
            .into_iter()
            .map(|d| ix.display_name(d))
            .collect();
        // use_foo holds a reference; the reference in variant V belongs to enum E.
        assert_eq!(names, vec!["use_foo".to_string(), "E".to_string()]);
        let run: Vec<String> = ix
            .closure(&sym("a/impl#[Foo]run()."))
            .into_iter()
            .map(|d| ix.display_name(d))
            .collect();
        assert_eq!(run, vec!["use_foo".to_string()]);
    }

    #[test]
    fn chain_and_names_match_owner_forms() {
        let c = chain_of(&sym("a/impl#[Foo]run().")).unwrap();
        assert_eq!(c.names, vec!["a", "Foo", "run"]);
        assert_eq!(c.owner.as_deref(), Some("Foo"));
        let ix = demo();
        let (names, rejected) =
            code_names("What breaks if `Foo::run()` or `crate::a::Foo<T>` or `x + y` change?");
        assert_eq!(rejected, 1);
        assert_eq!(names, vec![vec!["Foo", "run"], vec!["a", "Foo"]]);
        let run = ix.def_index(&sym("a/impl#[Foo]run().")).unwrap();
        assert!(ix.matches(run, &["Foo".into(), "run".into()]));
        assert!(ix.matches(run, &["run".into()]));
        assert!(!ix.matches(run, &["Bar".into(), "run".into()]));
    }

    #[test]
    fn anchor_c_needs_a_hit_and_n_does_not() {
        let ix = demo();
        let (names, _) = code_names("`Foo`");
        let miss = vec![Chunk {
            rank: 1,
            path: "b.rs".into(),
            start_line: 1,
            end_line: 9,
        }];
        assert!(ix.anchor_c(&names, &miss, 5).is_none());
        let hit = vec![
            Chunk {
                rank: 2,
                path: "a.rs".into(),
                start_line: 1,
                end_line: 2,
            },
            Chunk {
                rank: 1,
                path: "b.rs".into(),
                start_line: 1,
                end_line: 9,
            },
        ];
        let a = ix.anchor_c(&names, &hit, 5).unwrap();
        assert_eq!(ix.defs[a.def].symbol, sym("a/Foo#"));
        assert_eq!(a.chunk_rank, Some(2));
        assert!(ix.anchor_c(&names, &hit, 1).is_none());
        assert_eq!(
            ix.defs[ix.anchor_n(&names).unwrap().def].symbol,
            sym("a/Foo#")
        );
    }

    #[test]
    fn random_is_deterministic_and_without_repeats() {
        let units: Vec<usize> = (0..50).collect();
        let a = arm_r(&units, 7, 3);
        assert_eq!(a, arm_r(&units, 7, 3));
        assert_eq!(a.len(), 7);
        assert_eq!(a.iter().collect::<BTreeSet<_>>().len(), 7);
        assert_ne!(a, arm_r(&units, 7, 4));
    }

    /// a.rs: `struct Foo` (line 1), `fn alpha` uses Foo once (line 2), `fn beta`
    /// twice (line 3), `fn gamma` once (line 4).
    fn held_demo() -> ImpactIndex {
        let (foo, alpha, beta, gamma) = (
            sym("a/Foo#"),
            sym("a/alpha()."),
            sym("a/beta()."),
            sym("a/gamma()."),
        );
        let docs = vec![RawDocument {
            path: "a.rs".into(),
            occurrences: vec![
                occ((0, 7, 0, 10), &foo, 1, Some((0, 0, 0, 14))),
                occ((1, 3, 1, 8), &alpha, 1, Some((1, 0, 1, 30))),
                occ((1, 12, 1, 15), &foo, 0, None),
                occ((2, 3, 2, 7), &beta, 1, Some((2, 0, 2, 40))),
                occ((2, 12, 2, 15), &foo, 0, None),
                occ((2, 20, 2, 23), &foo, 0, None),
                occ((3, 3, 3, 8), &gamma, 1, Some((3, 0, 3, 30))),
                occ((3, 12, 3, 15), &foo, 0, None),
            ],
        }];
        let kinds: HashMap<String, Kind> = [
            (foo, Kind::Struct),
            (alpha, Kind::Function),
            (beta, Kind::Function),
            (gamma, Kind::Function),
        ]
        .into_iter()
        .collect();
        ImpactIndex::build(docs, &kinds, None)
    }

    #[test]
    fn ranked_closure_orders_by_references_held_then_position() {
        let ix = held_demo();
        let ranked = ix.ranked_closure("What breaks if `Foo` changes?");
        assert!(ranked.anchored);
        assert_eq!(ranked.names, 1);
        // beta holds two references, alpha and gamma one each (path, line order).
        let order: Vec<(u32, usize)> = ranked.decls.iter().map(|r| (r.decl.line, r.held)).collect();
        assert_eq!(order, [(3, 2), (2, 1), (4, 1)]);
        assert!(ranked.decls.iter().all(|r| r.decl == ix.decl(r.def)));
        assert!(ranked.decls.iter().all(|r| r.decl.path == "a.rs"));
    }

    #[test]
    fn ranked_closure_without_a_name_or_an_anchor_is_empty() {
        let ix = held_demo();
        let none = ix.ranked_closure("What breaks if the parser changes?");
        assert_eq!((none.names, none.anchored, none.decls.len()), (0, false, 0));
        let unknown = ix.ranked_closure("What breaks if `Nowhere` changes?");
        assert_eq!((unknown.names, unknown.anchored), (1, false));
        assert!(unknown.decls.is_empty());
    }
}
