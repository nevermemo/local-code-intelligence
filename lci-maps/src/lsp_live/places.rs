//! The place table with the server's live outline (`[run_profile.lsp] places =
//! "outline"`): the index's table first, then what `textDocument/documentSymbol`
//! says about the files the walk touches.
//!
//! The outline of a file is learned once per map instance ([`SymbolPlaces::
//! learn`]; a second call for the same file replaces its entries, the later
//! outline being newer). A learned member takes the id of the index's own node
//! (same file, same name, nearest line, a function or method of either name for
//! the other) when the index has one, so that one symbol keeps one id across
//! the layers, a shifted line included; else it gets a minted id (the file's
//! real language, no container, the member's kind, equals told apart in
//! document order). Where the file is learned the outline's places win over
//! the index's, which may be older than the tree; the index fills what the
//! outline does not list (a kind this build has no node for).

use super::{Member, Place, SymbolPlaces};
use lci_core::{
    language,
    node::{NodeId, SymbolKey, SymbolKind, rust_module_path},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Mutex, PoisonError},
};

/// The node kind of an LSP `SymbolKind` number this build has a node for.
/// Fields, properties, variables and enum members have none until a later
/// slice adds them (K10b), so they get no entry.
fn node_kind(lsp_kind: u64) -> Option<SymbolKind> {
    match lsp_kind {
        12 => Some(SymbolKind::Function),
        6 | 9 => Some(SymbolKind::Method),
        5 | 23 => Some(SymbolKind::Struct),
        11 => Some(SymbolKind::Trait),
        10 => Some(SymbolKind::Enum),
        2..=4 => Some(SymbolKind::Module),
        14 => Some(SymbolKind::Const),
        _ => None,
    }
}

/// Functions and methods are the index's only kinds, and a server and the
/// index may disagree about which an associated function is.
fn callable(kind: SymbolKind) -> bool {
    matches!(kind, SymbolKind::Function | SymbolKind::Method)
}

/// One learned member.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    place: Place,
    end_line: u32,
    id: NodeId,
}

#[derive(Debug, Default)]
struct Learned {
    /// The files whose outline was asked, answered or not.
    files: HashSet<String>,
    by_node: HashMap<NodeId, Place>,
    by_place: HashMap<Place, NodeId>,
    /// Per file, in document order.
    by_file: HashMap<String, Vec<Entry>>,
}

/// The index's table plus the live outlines of the files the walk learned.
pub struct OutlinePlaces {
    /// The index's table ([`super::ScipPlaces`] in production).
    scip: Box<dyn SymbolPlaces>,
    state: Mutex<Learned>,
}

impl OutlinePlaces {
    pub fn over(scip: impl SymbolPlaces + 'static) -> Self {
        Self {
            scip: Box::new(scip),
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Learned> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The id of the member named `name` of `kind` at `line` of `path`: the
    /// index's node there that no earlier member took (the nearest line, a
    /// node of the member's own kind before the other callable kind), else a
    /// minted one.
    fn id_for(
        &self,
        path: &str,
        name: &str,
        kind: SymbolKind,
        line: u32,
        equals_before: u16,
        taken: &HashSet<NodeId>,
    ) -> Option<NodeId> {
        let nodes = self.scip.nodes_in(path);
        let indexed = nodes
            .iter()
            .filter(|(_, id)| !taken.contains(id))
            .filter_map(|(at, id)| match id {
                NodeId::Symbol(key)
                    if key.name == name
                        && (key.kind == kind || callable(key.kind) && callable(kind)) =>
                {
                    Some((key.kind != kind, at.abs_diff(line), id))
                }
                _ => None,
            })
            .min_by_key(|(other_kind, distance, _)| (*other_kind, *distance));
        if let Some((_, _, id)) = indexed {
            return Some(id.clone());
        }
        let language = language::for_path(Path::new(path))?.identifier();
        let mut key = SymbolKey {
            language: language.to_owned(),
            module_path: rust_module_path(path),
            container: Vec::new(),
            name: name.to_owned(),
            kind,
            disambiguator: equals_before,
        };
        // Never an id the index has for another node, nor one already given.
        loop {
            let id = NodeId::Symbol(key.clone());
            if !taken.contains(&id) && self.scip.place_of(&id).is_none() {
                return id.validate().is_ok().then_some(id);
            }
            key.disambiguator = key.disambiguator.checked_add(1)?;
        }
    }
}

impl SymbolPlaces for OutlinePlaces {
    fn place_of(&self, node: &NodeId) -> Option<Place> {
        self.state()
            .by_node
            .get(node)
            .cloned()
            .or_else(|| self.scip.place_of(node))
    }

    fn defined_at(&self, place: &Place) -> Option<NodeId> {
        self.state()
            .by_place
            .get(place)
            .cloned()
            .or_else(|| self.scip.defined_at(place))
    }

    fn defined_in(&self, path: &str, lines: (u32, u32), name: &str) -> Option<NodeId> {
        let state = self.state();
        let learned = state.by_file.get(path).and_then(|entries| {
            let named = |entry: &&Entry| entry.name == name;
            entries
                .iter()
                .filter(named)
                .find(|entry| entry.place.line == lines.0)
                .or_else(|| {
                    entries
                        .iter()
                        .filter(named)
                        .find(|entry| (lines.0..=lines.1).contains(&entry.place.line))
                })
                .map(|entry| entry.id.clone())
        });
        learned.or_else(|| self.scip.defined_in(path, lines, name))
    }

    fn enclosing(&self, path: &str, line: u32) -> Option<NodeId> {
        let state = self.state();
        let learned = state.by_file.get(path).and_then(|entries| {
            entries
                .iter()
                .filter(|entry| (entry.place.line..=entry.end_line).contains(&line))
                // The innermost: the latest start, then the smallest end.
                .max_by_key(|entry| {
                    (
                        entry.place.line,
                        entry.place.col,
                        std::cmp::Reverse(entry.end_line),
                    )
                })
                .map(|entry| entry.id.clone())
        });
        learned.or_else(|| self.scip.enclosing(path, line))
    }

    fn nodes_in(&self, path: &str) -> Vec<(u32, NodeId)> {
        self.scip.nodes_in(path)
    }

    fn learn(&self, path: &str, members: &[Member]) {
        let mut ordered: Vec<(&Member, SymbolKind)> = members
            .iter()
            .filter(|member| !member.name.is_empty() && member.place.path == path)
            .filter_map(|member| Some((member, node_kind(member.kind?)?)))
            .collect();
        ordered.sort_by(|a, b| (&a.0.place, &a.0.name).cmp(&(&b.0.place, &b.0.name)));
        let mut entries: Vec<Entry> = Vec::new();
        let mut taken: HashSet<NodeId> = HashSet::new();
        let mut places: HashSet<Place> = HashSet::new();
        let mut equals: HashMap<(&str, SymbolKind), u16> = HashMap::new();
        for (member, kind) in ordered {
            let before = equals.entry((member.name.as_str(), kind)).or_insert(0);
            let equals_before = *before;
            *before = before.saturating_add(1);
            if !places.insert(member.place.clone()) {
                continue;
            }
            let Some(id) = self.id_for(
                path,
                &member.name,
                kind,
                member.place.line,
                equals_before,
                &taken,
            ) else {
                continue;
            };
            taken.insert(id.clone());
            entries.push(Entry {
                name: member.name.clone(),
                place: member.place.clone(),
                end_line: member.end_line.max(member.place.line),
                id,
            });
        }
        let mut state = self.state();
        for old in state.by_file.remove(path).into_iter().flatten() {
            state.by_node.remove(&old.id);
            state.by_place.remove(&old.place);
        }
        state.files.insert(path.to_owned());
        for entry in &entries {
            state.by_node.insert(entry.id.clone(), entry.place.clone());
            state.by_place.insert(entry.place.clone(), entry.id.clone());
        }
        state.by_file.insert(path.to_owned(), entries);
    }

    fn outline_wanted(&self, path: &str) -> bool {
        !self.state().files.contains(path)
    }
}
