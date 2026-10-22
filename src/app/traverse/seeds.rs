//! Start nodes: the name-token stand-in, the hybrid search seam, the mapping of
//! search hits to graph nodes, and the descriptor-only seed maps.

use super::first_stage::{EXACT_CLOSURE_ID, EXACT_PICKAXE_ID};
use super::{
    Call, SEEDS_BM25, SEEDS_HYBRID, SEEDS_LITERAL, SEEDS_MAP, SEEDS_REPO_MAP, chunks, index::Loaded,
};
use crate::{app::App, config::TraverseConfig, filter::FilterRequest, models::Models};
use anyhow::{Context, Result, anyhow};
use lci_core::{
    lexical,
    node::{NodeId, NodeKind},
};
use lci_maps::{
    Contribution, FreshnessModel, Map, MapDescriptor, MapId, MapRequest, MapResponse, MapStatus,
    SnapshotView, SourceLabel,
    contains::CONTAINS_MAP_ID,
    impact::code_names,
    lsp_live::SymbolPlaces,
    scip_graph::{ScipMap, SymbolDocs, symbol_path_names},
    scip_implements::{ImplementsMap, SCIP_IMPLEMENTS_ID, TargetKind},
    search_maps::ChunkSearch,
    traverse::Seed,
    walk::tokenize,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

/// Start nodes the name-token stand-in takes from the question.
const SEED_COUNT: usize = 3;
/// Most symbols one hit of the hybrid search may start from (a chunk that
/// holds more definitions keeps the ones the question's words name best).
const MAX_SYMBOLS_PER_HIT: usize = 3;

/// Matches the question's words against symbol names: the stand-in for the
/// lexical and vector channels until they are wrapped as maps.
pub(super) struct NameSeeds {
    names: Vec<(NodeId, HashSet<String>)>,
}

impl NameSeeds {
    pub(super) fn new(scip: &ScipMap) -> Self {
        let graph = scip.graph();
        Self {
            names: (0..graph.nodes.len())
                .map(|i| {
                    (
                        scip.node_id(i).clone(),
                        tokenize(&graph.nodes[i].name).into_iter().collect(),
                    )
                })
                .collect(),
        }
    }

    /// The best `limit` nodes by the number of the text's words in their
    /// name, with that number.
    fn matches(&self, text: &str, limit: usize) -> Vec<(NodeId, usize)> {
        let words = tokenize(text);
        let mut scored: Vec<(usize, &NodeId)> = self
            .names
            .iter()
            .map(|(id, tokens)| (words.iter().filter(|w| tokens.contains(*w)).count(), id))
            .filter(|(n, _)| *n > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(n, id)| (id.clone(), n))
            .collect()
    }
}

/// BM25 over the SCIP graph's entities (`seeds.bm25`): each function or method
/// node is a short document of three fields, the tokens of its name, of its
/// qualified path (the symbol's package, modules, types and member, and the
/// file's path segments) and of its documentation. The scorer is the one the
/// ripgrep channel uses ([`lexical::score_chunks`]): a field that holds a
/// query term is one credited line of the node, so a term's frequency is the
/// number of fields (1 to 3) that hold it, its document frequency the number
/// of nodes, and a node's length its token count.
pub(super) struct Bm25Seeds {
    ids: Vec<NodeId>,
    /// Per node: the graph's `Owner::name`, for the seed's details.
    names: Vec<String>,
    /// Per node: the name, path and documentation tokens.
    fields: Vec<[Vec<String>; 3]>,
    /// Per node: its token count, at least 1.
    lengths: Vec<u32>,
}

/// A token worth indexing: one a query term could match (three characters or
/// more, no stop word) that is not a number.
fn indexable(token: &str) -> bool {
    lexical::usable_term(token) && !token.chars().all(|c| c.is_ascii_digit())
}

fn index_tokens(text: &str) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|token| indexable(token))
        .collect()
}

impl Bm25Seeds {
    /// The index over the graph's nodes; `docs` is the documentation of the
    /// SCIP index by symbol (empty when it could not be read).
    pub(super) fn new(scip: &ScipMap, docs: &SymbolDocs) -> Self {
        let graph = scip.graph();
        let mut ids = Vec::with_capacity(graph.nodes.len());
        let mut names = Vec::with_capacity(graph.nodes.len());
        let mut fields = Vec::with_capacity(graph.nodes.len());
        let mut lengths = Vec::with_capacity(graph.nodes.len());
        for (i, node) in graph.nodes.iter().enumerate() {
            let name = index_tokens(&node.name);
            let mut path: Vec<String> = Vec::new();
            for part in symbol_path_names(&node.symbol) {
                path.extend(index_tokens(&part));
            }
            let file = node
                .file
                .rsplit_once('.')
                .map_or(&*node.file, |(stem, _)| stem);
            for part in file.split(['/', '\\']) {
                path.extend(index_tokens(part));
            }
            let doc = docs
                .get(&node.symbol)
                .map(|text| index_tokens(text))
                .unwrap_or_default();
            lengths.push(
                u32::try_from(name.len() + path.len() + doc.len()).map_or(u32::MAX, |n| n.max(1)),
            );
            ids.push(scip.node_id(i).clone());
            names.push(node.name.clone());
            fields.push([name, path, doc]);
        }
        Self {
            ids,
            names,
            fields,
            lengths,
        }
    }

    /// Bit `i` is set when query term `i` is held by one of `tokens`: a term
    /// of at most [`lexical::WORD_MATCH_MAX_CHARS`] characters must be a whole
    /// token, a longer one may be inside a token (the rule the ripgrep
    /// channel's matcher applies to a line).
    fn term_mask(terms: &[String], tokens: &[String]) -> u32 {
        terms
            .iter()
            .enumerate()
            .filter(|(_, term)| {
                if term.chars().count() <= lexical::WORD_MATCH_MAX_CHARS {
                    tokens.iter().any(|token| token == *term)
                } else {
                    tokens.iter().any(|token| token.contains(term.as_str()))
                }
            })
            .fold(0, |mask, (bit, _)| mask | 1 << bit)
    }

    /// The best `limit` nodes for the question, best first, with their BM25
    /// score; those that hold none of its scoring terms are not returned.
    pub(super) fn matches(&self, question: &str, limit: usize) -> Vec<(NodeId, String, f64)> {
        let terms = lexical::query_terms(question);
        if terms.is_empty() || self.ids.is_empty() {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for (node, fields) in self.fields.iter().enumerate() {
            for tokens in fields {
                let term_mask = Self::term_mask(&terms, tokens);
                if term_mask != 0 {
                    lines.push(lexical::CreditedLine {
                        file: node,
                        chunk: node,
                        term_mask,
                    });
                }
            }
        }
        let mut scored = lexical::score_chunks(terms.len(), self.ids.len(), &self.lengths, &lines);
        // Equal scores keep the order of the node ids, not of the graph.
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| self.ids[a.chunk].cmp(&self.ids[b.chunk]))
        });
        scored
            .into_iter()
            .take(limit)
            .map(|scored| {
                (
                    self.ids[scored.chunk].clone(),
                    self.names[scored.chunk].clone(),
                    scored.score,
                )
            })
            .collect()
    }
}

/// One hit of the hybrid search, as the start-node mapping needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedHit {
    /// The chunk's file, relative to the workspace.
    pub path: String,
    /// The chunk's lines, 1-based and inclusive (as the index stores them).
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: String,
    /// The chunk's ordinal among the chunks of its file with the same
    /// content hash (a part of its node id); `None` when the index's chunk
    /// list could not be read, which reads as 0.
    pub ordinal: Option<u32>,
    /// Position in the search result, 1-based.
    pub rank: usize,
    /// The retrievers that returned the chunk (`semantic`, `lexical`, `lsp`).
    pub retrievers: Vec<String>,
    pub semantic_rank: Option<usize>,
    pub lexical_rank: Option<usize>,
    pub lsp_rank: Option<usize>,
    pub fusion_score: f32,
    pub reranker_score: Option<f32>,
}

impl SeedHit {
    /// The score the search ordered the hit by: the reranker's when it ran
    /// for the hit, else the fusion score, with the name of which it is.
    fn score(&self) -> (f32, &'static str) {
        match self.reranker_score {
            Some(score) => (score, "reranker"),
            None => (self.fusion_score, "fusion"),
        }
    }
}

/// The search that turns a question into [`SeedHit`]s.
#[lci_maps::async_trait]
pub trait SeedSearch: Send + Sync {
    /// The best `k` hits of the question, best first.
    async fn search(&self, question: &str, k: usize) -> Result<Vec<SeedHit>>;

    /// The chunks of the workspace's index, for the maps `search.lexical` and
    /// `search.vector`; `None` when this search has no index to offer.
    fn chunk_search(&self) -> Option<Arc<dyn ChunkSearch + '_>> {
        None
    }

    /// Why this search is known to be unavailable (the application did not
    /// open); `None` for a search that can run. A map that needs the
    /// application besides the hybrid search (`seeds.literal`) treats `Some`
    /// like no search at all.
    fn unavailable(&self) -> Option<&str> {
        None
    }

    /// The reranker client, for `[traverse] rerank_frontier`; `None` when this
    /// search has none.
    fn reranker(&self) -> Option<&Models> {
        None
    }

    /// The reranker's model and endpoint as text a handle keeps, so a resume
    /// can refuse another reranker; `None` when this search cannot say. The
    /// text names no path and no secret.
    fn reranker_identity(&self) -> Option<String> {
        None
    }

    /// The content hash of every file of the index snapshot's manifest, by
    /// path, for `[run_profile.freshness] verify_on_read`; `None` when this
    /// search has no index snapshot (or an empty one). Reads only.
    async fn snapshot_hashes(&self) -> Option<HashMap<String, String>> {
        None
    }

    /// The lines of the workspace's source files that hold one of `names` as a
    /// whole identifier (exact case), at most `max_per_file` per file, for
    /// `seeds.literal`; `None` when this search cannot run ripgrep.
    async fn literal_search(
        &self,
        _names: &[String],
        _max_per_file: u32,
    ) -> Option<Result<lexical::LiteralSearch>> {
        None
    }
}

/// The search `search_code` runs in its normal mode, over one workspace and
/// with the retrievers and reranker of one profile.
pub struct AppSeeds<'a> {
    pub app: &'a App,
    pub workspace: &'a Path,
    pub profile: Option<&'a str>,
}

#[lci_maps::async_trait]
impl SeedSearch for AppSeeds<'_> {
    fn chunk_search(&self) -> Option<Arc<dyn ChunkSearch + '_>> {
        Some(Arc::new(chunks::AppChunks::new(self.app, self.workspace)))
    }

    fn reranker(&self) -> Option<&Models> {
        Some(&self.app.models)
    }

    fn reranker_identity(&self) -> Option<String> {
        let config = &self.app.config;
        // The endpoint is kept as a hash: the URL may carry a host or a token
        // that does not belong in a handle.
        Some(format!(
            "{} @ u{}",
            config.reranker_model,
            lci_maps::traverse::trace::hash16(config.reranker_url.as_bytes())
        ))
    }

    async fn literal_search(
        &self,
        names: &[String],
        max_per_file: u32,
    ) -> Option<Result<lexical::LiteralSearch>> {
        let resolved =
            crate::workspace::Workspace::resolve(self.workspace, &self.app.config.data_dir);
        Some(match resolved {
            Ok(workspace) => {
                lexical::search_literals(
                    &self.app.config.ripgrep_path,
                    &workspace.path,
                    names,
                    max_per_file,
                )
                .await
            }
            Err(error) => Err(error),
        })
    }

    async fn snapshot_hashes(&self) -> Option<HashMap<String, String>> {
        let data_dir = self.app.config.data_dir.clone();
        let workspace = crate::workspace::Workspace::resolve(self.workspace, &data_dir).ok()?;
        let manifest =
            tokio::task::spawn_blocking(move || crate::manifest::try_load(&data_dir, &workspace))
                .await
                .ok()?
                .ok()?;
        (!manifest.files.is_empty()).then(|| {
            manifest
                .files
                .into_iter()
                .map(|(path, file)| (path, file.content_hash))
                .collect()
        })
    }

    async fn search(&self, question: &str, k: usize) -> Result<Vec<SeedHit>> {
        let report = self
            .app
            .search_in_profile_unverified(
                self.workspace,
                question,
                Some(k),
                FilterRequest::default(),
                self.profile,
            )
            .await?;
        if report.results.is_empty() {
            return Ok(Vec::new());
        }
        // The ordinal of a chunk in its node id needs the chunk list of its
        // file, which only the index's manifest has.
        let data_dir = self.app.config.data_dir.clone();
        let workspace = report.workspace.clone();
        let manifest =
            tokio::task::spawn_blocking(move || crate::manifest::try_load(&data_dir, &workspace))
                .await
                .context("read the chunk list of the index")?
                .ok();
        Ok(report
            .results
            .iter()
            .zip(1..)
            .map(|(hit, rank)| {
                let chunk = &hit.chunk;
                let ordinal = manifest.as_ref().and_then(|manifest| {
                    let file = manifest.files.get(&chunk.relative_file_path)?;
                    let at = file.chunks.iter().position(|c| {
                        (c.start_line, c.end_line, &c.content_hash)
                            == (chunk.start_line, chunk.end_line, &chunk.content_hash)
                    })?;
                    match lci_core::node::chunk_ids(&file.chunks).swap_remove(at) {
                        NodeId::Chunk { ordinal, .. } => Some(ordinal),
                        _ => None,
                    }
                });
                SeedHit {
                    path: chunk.relative_file_path.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    content_hash: chunk.content_hash.clone(),
                    ordinal,
                    rank,
                    retrievers: hit.retrieval_channels.clone(),
                    semantic_rank: hit.semantic_rank,
                    lexical_rank: hit.lexical_rank,
                    lsp_rank: hit.lsp_rank,
                    fusion_score: hit.fusion_score,
                    reranker_score: hit.reranker_score,
                }
            })
            .collect())
    }
}

/// Whether the language adapters that index code (not prose) own this path.
pub(crate) fn is_source_path(path: &str) -> bool {
    lci_core::language::for_path(Path::new(path)).is_some_and(|adapter| !adapter.is_prose())
}

/// How many source files differ between an index snapshot's manifest
/// (`path -> content hash`) and the tree's source files now (`path -> content
/// hash`): changed, added or removed. The manifest's prose entries are left
/// out (the tree scan leaves prose out); a manifest entry the tree no longer
/// has counts when it is a source path. Shared by the traverse labels and the
/// search report of `[run_profile.freshness] verify_on_read`.
pub(crate) fn snapshot_differing(
    manifest: &HashMap<String, String>,
    now: &HashMap<&str, &str>,
) -> u32 {
    let changed = now
        .iter()
        .filter(|(path, hash)| manifest.get(**path).map(String::as_str) != Some(**hash))
        .count();
    let removed = manifest
        .keys()
        .filter(|path| !now.contains_key(path.as_str()) && is_source_path(path))
        .count();
    u32::try_from(changed + removed).unwrap_or(u32::MAX)
}

/// A search that is known to be unavailable: it fails with the reason, which
/// the response names as the reason for the stand-in.
pub struct NoSearch(pub String);

#[lci_maps::async_trait]
impl SeedSearch for NoSearch {
    async fn search(&self, _question: &str, _k: usize) -> Result<Vec<SeedHit>> {
        Err(anyhow!("{}", self.0))
    }

    fn unavailable(&self) -> Option<&str> {
        Some(&self.0)
    }
}

/// A start node and what is known of where it came from.
pub(super) struct MappedSeed {
    pub(super) node: NodeId,
    /// The `seed_hits` record of the response and the trace.
    pub(super) info: Value,
}

/// Where in the graph a hit's chunk is.
enum Place {
    /// Functions whose name is on a line of the chunk, at most
    /// [`MAX_SYMBOLS_PER_HIT`] of `total`.
    Defined { nodes: Vec<usize>, total: usize },
    /// The chunk lies inside this function (the innermost around it).
    Inside(usize),
    /// No function of the graph: the chunk stays a chunk node.
    Nowhere,
}

/// Finds the graph nodes a hit's chunk defines or lies in. Lines: the chunk's
/// are 1-based, the graph's 0-based; a chunk holds a function when the line
/// of the function's name is one of the chunk's lines, and lies in a function
/// when the function's whole range covers the chunk's lines. A chunk that
/// does neither (the gap between two functions, a type, a constant, a file
/// the index does not list) is not attributed to its neighbour.
fn place(loaded: &Loaded, hit: &SeedHit, words: &[String]) -> Place {
    let graph = loaded.scip.graph();
    let Some(nodes) = loaded.by_file.get(&hit.path.replace('\\', "/")) else {
        return Place::Nowhere;
    };
    let first = hit.start_line.saturating_sub(1);
    let last = hit.end_line.max(hit.start_line).saturating_sub(1);
    let mut defined: Vec<usize> = nodes
        .iter()
        .copied()
        .filter(|&i| (first..=last).contains(&graph.nodes[i].line))
        .collect();
    if !defined.is_empty() {
        let total = defined.len();
        if total > MAX_SYMBOLS_PER_HIT {
            let overlap = |i: usize| {
                let tokens = &loaded.names.names[i].1;
                words.iter().filter(|w| tokens.contains(*w)).count()
            };
            defined.sort_by_key(|&i| (std::cmp::Reverse(overlap(i)), graph.nodes[i].line, i));
            defined.truncate(MAX_SYMBOLS_PER_HIT);
        }
        defined.sort_by_key(|&i| (graph.nodes[i].line, i));
        return Place::Defined {
            nodes: defined,
            total,
        };
    }
    let around = nodes
        .iter()
        .copied()
        .filter(|&i| {
            let (from, to) = graph.nodes[i].span;
            from <= first && last <= to
        })
        .min_by_key(|&i| {
            let (from, to) = graph.nodes[i].span;
            (to - from, std::cmp::Reverse(from), i)
        });
    around.map_or(Place::Nowhere, Place::Inside)
}

/// Maps the hits of the hybrid search to start nodes, best hit first: a
/// symbol node of the SCIP graph where the hit's chunk defines or lies in a
/// function (see [`place`]), else the chunk's own node. A node that several
/// hits map to is one seed that lists the ranks of all of them; its other
/// fields are those of its best hit. A hit whose file or hash cannot make a
/// node id is dropped.
pub(super) fn map_hits(loaded: &Loaded, question: &str, hits: &[SeedHit]) -> Vec<MappedSeed> {
    let words = tokenize(question);
    let graph = loaded.scip.graph();
    let mut seeds: Vec<MappedSeed> = Vec::new();
    let mut at: HashMap<NodeId, usize> = HashMap::new();
    for hit in hits {
        let (score, score_kind) = hit.score();
        // (node, how, definitions in the chunk, graph index)
        let candidates: Vec<(NodeId, &'static str, Option<usize>, Option<usize>)> =
            match place(loaded, hit, &words) {
                Place::Defined { nodes, total } => nodes
                    .into_iter()
                    .map(|i| {
                        (
                            loaded.scip.node_id(i).clone(),
                            "defined",
                            Some(total),
                            Some(i),
                        )
                    })
                    .collect(),
                Place::Inside(i) => vec![(loaded.scip.node_id(i).clone(), "inside", None, Some(i))],
                Place::Nowhere => {
                    let chunk = NodeId::chunk(
                        hit.path.clone(),
                        hit.content_hash.clone(),
                        hit.ordinal.unwrap_or(0),
                    )
                    .or_else(|_| NodeId::file(hit.path.clone()));
                    match chunk {
                        Ok(node) => vec![(node, "chunk", None, None)],
                        Err(_) => Vec::new(),
                    }
                }
            };
        for (node, how, symbols_in_chunk, graph_index) in candidates {
            if let Some(&known) = at.get(&node) {
                if let Some(list) = seeds[known].info["also_hit_ranks"].as_array_mut() {
                    list.push(json!(hit.rank));
                }
                continue;
            }
            let symbol = node.kind() == NodeKind::Symbol;
            let mut info = json!({
                "node": node.to_string(),
                "kind": if symbol { "symbol" } else if node.kind() == NodeKind::Chunk { "chunk" } else { "file" },
                "how": how,
                "path": hit.path,
                "lines": [hit.start_line, hit.end_line],
                "hit_rank": hit.rank,
                "retrievers": hit.retrievers,
                "channel_ranks": {
                    "semantic": hit.semantic_rank,
                    "lexical": hit.lexical_rank,
                    "lsp": hit.lsp_rank,
                },
                "score": score,
                "score_kind": score_kind,
                "also_hit_ranks": [],
            });
            if let Some(object) = info.as_object_mut() {
                if let Some(total) = symbols_in_chunk {
                    object.insert("symbols_in_chunk".into(), json!(total));
                }
                if let Some(index) = graph_index {
                    object.insert("symbol".into(), json!(graph.nodes[index].name));
                }
            }
            at.insert(node.clone(), seeds.len());
            seeds.push(MappedSeed { node, info });
        }
    }
    seeds
}

/// Most names of the question (its code spans' last segments) `seeds.literal`
/// searches for.
const MAX_LITERAL_NAMES: usize = 4;
/// Most lines of one file ripgrep reports per search (`--max-count`).
const MAX_LITERAL_PER_FILE: u32 = 20;
/// Most definition seeds one name gives.
const MAX_LITERAL_PER_NAME: usize = 3;
/// Shortest name worth a search, in characters.
const MIN_LITERAL_CHARS: usize = 2;
/// The note of a `seeds.literal` call whose question has no name to search for.
const NO_LITERAL_NAMES_NOTE: &str = "seeds.literal: the question names nothing in code spans (`Name` or `a::Name`), so there is nothing to search for";
/// The note of a `seeds.literal` call with no search that can run ripgrep.
const NO_RIPGREP_NOTE: &str =
    "seeds.literal needs the application's ripgrep (give --root with a question)";
/// The ripgrep note of a call whose search is missing or known to be
/// unavailable (with the reason), else `None`.
fn no_ripgrep_note(search: Option<&dyn SeedSearch>) -> Option<String> {
    match search {
        None => Some(NO_RIPGREP_NOTE.to_owned()),
        Some(search) => search
            .unavailable()
            .map(|reason| format!("{NO_RIPGREP_NOTE}: the search is unavailable: {reason}")),
    }
}
/// Words that declare the name that follows them. `impl .. for <name>` counts
/// by holding `impl` before the name.
const DECL_KEYWORDS: [&str; 17] = [
    "fn",
    "trait",
    "struct",
    "enum",
    "impl",
    "class",
    "interface",
    "type",
    "mod",
    "def",
    "func",
    "union",
    "record",
    "protocol",
    "function",
    "object",
    "delegate",
];

/// The names `seeds.literal` searches for: the last segment of each path in
/// the question's code spans, at least [`MIN_LITERAL_CHARS`] characters, each
/// once, in question order, at most [`MAX_LITERAL_NAMES`].
pub(super) fn literal_needles(question: &str) -> Vec<String> {
    let (paths, _rejected) = code_names(question);
    let mut needles: Vec<String> = Vec::new();
    for path in paths {
        let Some(last) = path.last() else { continue };
        if last.chars().count() >= MIN_LITERAL_CHARS && !needles.contains(last) {
            needles.push(last.clone());
        }
        if needles.len() == MAX_LITERAL_NAMES {
            break;
        }
    }
    needles
}

/// Whether a declaration word precedes the first whole-identifier occurrence
/// of `name` in `text` (and the line is not a comment).
pub(super) fn declares(text: &str, name: &str) -> bool {
    let trimmed = text.trim_start();
    if ["//", "/*", "*", "#", "--"]
        .iter()
        .any(|comment| trimmed.starts_with(comment))
    {
        return false;
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let Some(start) = text
        .match_indices(name)
        .map(|(start, _)| start)
        .find(|&start| {
            !text[..start].chars().next_back().is_some_and(word)
                && !text[start + name.len()..].chars().next().is_some_and(word)
        })
    else {
        return false;
    };
    // The word right before the name declares it (`trait Name`, `pub struct
    // Name`), or the name follows the `for` of an `impl .. for Name`. A name
    // in a signature (`fn draw(shape: Shape)`) or a constant is a use.
    let before: Vec<&str> = text[..start]
        .split(|c: char| !word(c))
        .filter(|token| !token.is_empty())
        .collect();
    // A Go method, `func (r T) Name`: the receiver sits between the word and
    // the name.
    let prefix = text[..start].trim();
    if prefix.ends_with(')') && (prefix.starts_with("func (") || prefix.starts_with("func(")) {
        return prefix.matches('(').count() == 1;
    }
    match before.last() {
        Some(&last) if DECL_KEYWORDS.contains(&last) => true,
        Some(&"for") => before.contains(&"impl"),
        // C#: `delegate <return type> Name(..)`.
        Some(_) => before.contains(&"delegate") && !text[..start].contains('('),
        _ => false,
    }
}

/// One node `seeds.literal` found, before the cut.
struct LiteralHit {
    node: NodeId,
    how: &'static str,
    name: usize,
    path: String,
    line: u32,
    count: u32,
}

/// Start nodes from the exact occurrences of the names the question puts in
/// code spans (`seeds.literal`): ripgrep finds the lines that hold a name as a
/// whole identifier, and each line is placed in the SCIP graph. A line that is
/// the definition of a symbol named so gives that symbol (`definition`); else
/// the innermost symbol around it (`occurrence`, counted per node); else, when
/// the line declares the name ([`DECL_KEYWORDS`] before it), the file's node
/// (`file`: a type or trait the graph lacks, which `structure.contains` or an
/// outline expands); else the line is dropped. Order: definitions (name order,
/// then path; at most [`MAX_LITERAL_PER_NAME`] per name), occurrences (most
/// lines first), files (by path), with the nodes of `known` left out and the
/// list cut at `seed_k`. Fail-open: a search that is not available or fails
/// gives no seeds and a note.
pub(super) async fn literal_seeds(
    limits: &TraverseConfig,
    call: &Call<'_>,
    known: &HashSet<NodeId>,
    places: &dyn SymbolPlaces,
) -> Result<(Vec<Seed>, Vec<Value>, Vec<String>)> {
    let mut notes = Vec::new();
    // A call with no search, or one known to be unavailable, says so first: a
    // question with no code span must not hide a missing ripgrep.
    if let Some(note) = no_ripgrep_note(call.seed_search) {
        notes.push(note);
        return Ok((Vec::new(), Vec::new(), notes));
    }
    let Some(seed_search) = call.seed_search else {
        return Ok((Vec::new(), Vec::new(), notes));
    };
    let needles = literal_needles(call.question);
    if needles.is_empty() {
        notes.push(NO_LITERAL_NAMES_NOTE.to_owned());
        return Ok((Vec::new(), Vec::new(), notes));
    }
    let search = seed_search
        .literal_search(&needles, MAX_LITERAL_PER_FILE)
        .await;
    let found = match search {
        None => {
            notes.push(NO_RIPGREP_NOTE.to_owned());
            return Ok((Vec::new(), Vec::new(), notes));
        }
        Some(Err(error)) => {
            notes.push(format!("seeds.literal: the search failed: {error:#}"));
            return Ok((Vec::new(), Vec::new(), notes));
        }
        Some(Ok(found)) => found,
    };
    notes.extend(found.warning.clone());
    let mut hits: Vec<LiteralHit> = Vec::new();
    let mut at: HashMap<NodeId, usize> = HashMap::new();
    let mut dropped = 0usize;
    for line in &found.matches {
        let Some(name) = needles.get(line.name) else {
            continue;
        };
        let line0 = line.line.saturating_sub(1);
        let (node, how) =
            if let Some(node) = places.defined_in(&line.relative_path, (line0, line0), name) {
                (node, "definition")
            } else if let Some(node) = places.enclosing(&line.relative_path, line0) {
                (node, "occurrence")
            } else if is_source_path(&line.relative_path) && declares(&line.text, name) {
                match NodeId::file(line.relative_path.clone()) {
                    Ok(node) => (node, "file"),
                    Err(_) => {
                        dropped += 1;
                        continue;
                    }
                }
            } else {
                dropped += 1;
                continue;
            };
        match at.get(&node) {
            Some(&index) => {
                let hit = &mut hits[index];
                hit.count += 1;
                if hit.how == "occurrence" && how == "definition" {
                    // The detail shows the definition's line, not the first
                    // use that came before it.
                    hit.how = "definition";
                    hit.name = line.name;
                    hit.path.clone_from(&line.relative_path);
                    hit.line = line.line;
                }
            }
            None => {
                at.insert(node.clone(), hits.len());
                hits.push(LiteralHit {
                    node,
                    how,
                    name: line.name,
                    path: line.relative_path.clone(),
                    line: line.line,
                    count: 1,
                });
            }
        }
    }
    let mut definitions: Vec<&LiteralHit> =
        hits.iter().filter(|hit| hit.how == "definition").collect();
    definitions.sort_by_key(|hit| hit.name);
    let mut per_name = vec![0usize; needles.len()];
    definitions.retain(|hit| {
        per_name[hit.name] += 1;
        per_name[hit.name] <= MAX_LITERAL_PER_NAME
    });
    let mut occurrences: Vec<&LiteralHit> =
        hits.iter().filter(|hit| hit.how == "occurrence").collect();
    occurrences.sort_by_key(|hit| std::cmp::Reverse(hit.count));
    let files = hits.iter().filter(|hit| hit.how == "file");
    let (mut seeds, mut details) = (Vec::new(), Vec::new());
    let mut counts = [0usize; 3];
    for hit in definitions
        .into_iter()
        .chain(occurrences)
        .chain(files)
        .filter(|hit| !known.contains(&hit.node))
        .take(limits.seed_k as usize)
    {
        counts[match hit.how {
            "definition" => 0,
            "occurrence" => 1,
            _ => 2,
        }] += 1;
        details.push(json!({
            "node": hit.node.to_string(),
            "kind": if hit.node.kind() == NodeKind::Symbol { "symbol" } else { "file" },
            "how": hit.how,
            "name": needles[hit.name],
            "path": hit.path,
            "line": hit.line,
            "lines": [hit.line, hit.line],
            "retrievers": [SEEDS_LITERAL],
            "score": hit.count,
            "score_kind": "occurrences",
        }));
        seeds.push(Seed {
            node: hit.node.clone(),
            map: Some(SEEDS_LITERAL),
        });
    }
    let files_with_lines = found
        .matches
        .iter()
        .map(|line| line.relative_path.as_str())
        .collect::<HashSet<_>>()
        .len();
    notes.push(format!(
        "seeds.literal: searched {}; {} lines in {files_with_lines} files held a name as a whole word; {} start nodes ({} definition, {} occurrence, {} file); {dropped} lines were in no known symbol and declared nothing",
        needles.join(", "),
        found.matches.len(),
        seeds.len(),
        counts[0],
        counts[1],
        counts[2],
    ));
    Ok((seeds, details, notes))
}

/// A start-node or first-stage source as a registered map: its descriptor
/// only. The walk never queries it; registering it puts its id in the
/// selectors, the maps list and the trace (`seeds.hybrid`, `seeds.scip_names`,
/// `seeds.bm25`, `exact.closure`, `exact.pickaxe`).
pub(super) struct SeedMap {
    descriptor: MapDescriptor,
}

impl SeedMap {
    fn new(id: MapId, emits: Vec<NodeKind>, labels: Vec<SourceLabel>) -> Self {
        Self {
            descriptor: MapDescriptor {
                id,
                version: 1,
                contribution: Contribution::RankList,
                accepts_text: true,
                accepts: Vec::new(),
                emits,
                freshness: FreshnessModel::PerFile,
                source_labels: labels,
                needs: Vec::new(),
            },
        }
    }

    /// The hits of the hybrid search.
    pub(super) fn hybrid() -> Self {
        Self::new(
            SEEDS_HYBRID,
            vec![NodeKind::Symbol, NodeKind::Chunk, NodeKind::File],
            vec![SourceLabel::Vector, SourceLabel::Lexical, SourceLabel::Lsp],
        )
    }

    /// The symbols whose names hold the question's words.
    pub(super) fn name_tokens() -> Self {
        Self::new(
            SEEDS_MAP,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Lexical],
        )
    }

    /// The symbols whose name, path and documentation hold the question's
    /// rare words (BM25 over the SCIP graph's entities).
    pub(super) fn bm25() -> Self {
        Self::new(
            SEEDS_BM25,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Lexical],
        )
    }

    /// The symbols the SCIP graph's calls lead to from the entities the
    /// question's words name (personalized PageRank over the graph).
    pub(super) fn repo_map() -> Self {
        Self::new(
            SEEDS_REPO_MAP,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Prior, SourceLabel::Lexical],
        )
    }

    /// The nodes of the lines that hold a name of the question's code spans
    /// as a whole word (ripgrep, placed in the SCIP graph).
    pub(super) fn literal() -> Self {
        Self::new(
            SEEDS_LITERAL,
            vec![NodeKind::Symbol, NodeKind::File],
            vec![SourceLabel::Lexical],
        )
    }

    /// The declarations that reference the symbol the question names.
    pub(super) fn exact_closure() -> Self {
        Self::new(
            EXACT_CLOSURE_ID,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Scip],
        )
    }

    /// The commits `git log -G/-S/--grep` finds for a history question.
    pub(super) fn exact_pickaxe() -> Self {
        Self::new(
            EXACT_PICKAXE_ID,
            vec![NodeKind::Commit],
            vec![SourceLabel::Git],
        )
    }
}

#[lci_maps::async_trait]
impl Map for SeedMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, _request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        MapResponse::empty(&self.descriptor, view, MapStatus::Ok)
    }
}

fn parse_seeds(seeds: &[String]) -> Result<Vec<NodeId>> {
    seeds
        .iter()
        .map(|text| NodeId::from_str(text).map_err(|e| anyhow!("seed {text:?}: {e}")))
        .collect()
}

/// The start nodes of a call and where they came from.
#[derive(Default)]
pub(super) struct Seeds {
    pub(super) nodes: Vec<Seed>,
    /// Where the start nodes came from: `given`, `hybrid`, `name_tokens`,
    /// `hybrid+bm25` (the hybrid search's nodes, then those of `seeds.bm25`),
    /// `bm25` (`seeds.bm25` answered where the hybrid search did not),
    /// `hybrid+repo_map`, `hybrid+bm25+repo_map`, `bm25+repo_map`, `repo_map`
    /// (the nodes of `seeds.repo_map` after those, or alone), each also with
    /// `+literal` (see [`join_source`]),
    /// `traits` (the traits and trait methods the question's paths name, with
    /// `scip.implements` on), `traits+hybrid`, `traits+hybrid+bm25`,
    /// `traits+bm25`, `traits+name_tokens` or
    /// `traits+exact` (those, with the other source's nodes after them),
    /// `exact` (the first stage's cards, set by `run`) or `none`; empty on a
    /// continuation.
    pub(super) source: &'static str,
    /// How many start nodes are the traits and trait methods the question's
    /// paths name; `Some` only when the call walks `scip.implements` (0 when
    /// no path named one), so a bench can tell them from fallbacks.
    pub(super) trait_seeds: Option<usize>,
    /// The given start nodes (`--seeds`) that are traits or trait methods of
    /// `scip.implements`: walked as trait seeds, not counted in `trait_seeds`.
    pub(super) given_traits: Vec<NodeId>,
    /// One record per start node: the retrievers that found it and its score.
    pub(super) details: Vec<Value>,
    /// Why the stand-in replaced the hybrid search, if it did.
    pub(super) fallback: Option<String>,
    pub(super) notes: Vec<String>,
    /// How long the hybrid search ran (zero when it did not). The walk's
    /// deadline does not run while it does: the search is bounded by the
    /// embedding and reranker timeouts, not by `deadline_ms`.
    pub(super) search_time: Duration,
}

impl Seeds {
    /// The start nodes that are the traits and trait methods the question
    /// named ([`add_trait_seeds`] puts them first, `trait_seeds` of them), and
    /// the given ones that are traits or trait methods ([`Self::given_traits`]);
    /// none on a continuation.
    pub(super) fn trait_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        let count = self.trait_seeds.unwrap_or(0);
        self.nodes
            .iter()
            .take(count)
            .map(|seed| seed.node.clone())
            .chain(self.given_traits.iter().cloned())
    }
}

/// The `seed_source` after `part` (`repo_map` or `literal`) added nodes to the
/// start nodes of `current`: one of a closed list of strings, in the order the
/// sources run (hybrid, `bm25`, `repo_map`, `literal`). `current` empty (no
/// earlier source answered) gives `part` alone.
pub(super) fn join_source(current: &'static str, part: &'static str) -> &'static str {
    match (current, part) {
        ("", "repo_map") => "repo_map",
        ("hybrid", "repo_map") => "hybrid+repo_map",
        ("bm25", "repo_map") => "bm25+repo_map",
        ("hybrid+bm25", "repo_map") => "hybrid+bm25+repo_map",
        ("", "literal") => "literal",
        ("hybrid", "literal") => "hybrid+literal",
        ("bm25", "literal") => "bm25+literal",
        ("hybrid+bm25", "literal") => "hybrid+bm25+literal",
        ("repo_map", "literal") => "repo_map+literal",
        ("hybrid+repo_map", "literal") => "hybrid+repo_map+literal",
        ("bm25+repo_map", "literal") => "bm25+repo_map+literal",
        ("hybrid+bm25+repo_map", "literal") => "hybrid+bm25+repo_map+literal",
        _ => part,
    }
}

/// The note of `seeds.repo_map` on a graph with no nodes.
const REPO_MAP_EMPTY_NOTE: &str =
    "seeds.repo_map: the SCIP graph is empty ([run_profile.scip] mode or no index): no start nodes";
/// The note of `seeds.repo_map` when no entity matches the question's words.
const REPO_MAP_NO_MATCH_NOTE: &str = "seeds.repo_map: no entity matches the question's words, so the PageRank has nothing to personalise on: no start nodes";

/// Finds the start nodes: the caller's, else the hybrid search's, then (when
/// `seeds.bm25` is wanted) the entities it adds after them, else (when none of
/// those answered) the name-token stand-in, and the stand-in alone when it is
/// the only seeds map wanted.
pub(super) async fn find_seeds(
    limits: &TraverseConfig,
    call: &Call<'_>,
    loaded: &Loaded,
    wanted: &[MapId],
) -> Result<Seeds> {
    let explicit = parse_seeds(call.seeds)?;
    if !explicit.is_empty() {
        return Ok(Seeds {
            nodes: explicit
                .into_iter()
                .map(|node| Seed { node, map: None })
                .collect(),
            source: "given",
            ..Seeds::default()
        });
    }
    let mut found = Seeds::default();
    let mut reason = None;
    let mut hybrid_answered = false;
    if wanted.contains(&SEEDS_HYBRID) {
        match call.seed_search {
            None => {
                reason = Some(
                    "no search is available for this call (it has no workspace to search)"
                        .to_owned(),
                );
            }
            Some(_) if call.question.trim().is_empty() => {
                reason = Some("the question is empty".to_owned());
            }
            Some(search) => {
                let started = Instant::now();
                let hits = search.search(call.question, limits.seed_k as usize).await;
                found.search_time = started.elapsed();
                match hits {
                    Err(error) => reason = Some(format!("the hybrid search failed: {error:#}")),
                    Ok(hits) if hits.is_empty() => {
                        reason = Some("the hybrid search returned no hits".to_owned());
                    }
                    Ok(hits) => {
                        let mapped = map_hits(loaded, call.question, &hits);
                        let kinds = |kind: &str| {
                            mapped
                                .iter()
                                .filter(|seed| seed.info["kind"] == kind)
                                .count()
                        };
                        let chunks = kinds("chunk") + kinds("file");
                        if chunks > 0 {
                            found.notes.push(if wanted.contains(&CONTAINS_MAP_ID) {
                            format!(
                                "{chunks} of {} start nodes are chunks or files that the SCIP graph does not hold; {CONTAINS_MAP_ID} expands them to the symbols they contain",
                                mapped.len()
                            )
                        } else {
                            format!(
                                "{chunks} of {} start nodes are chunks or files that the SCIP graph does not hold; nothing expands them (add {CONTAINS_MAP_ID} to the maps)",
                                mapped.len()
                            )
                        });
                        }
                        found.source = "hybrid";
                        for seed in mapped {
                            found.nodes.push(Seed {
                                node: seed.node,
                                map: Some(SEEDS_HYBRID),
                            });
                            found.details.push(seed.info);
                        }
                        hybrid_answered = true;
                    }
                }
            }
        }
    }
    // `seeds.bm25` (only in `wanted` under `[run_profile.seeds] bm25`): the
    // entities whose name, path and documentation hold the question's rare
    // words, after the hybrid search's nodes.
    if wanted.contains(&SEEDS_BM25) && !call.question.trim().is_empty() {
        let known: HashSet<NodeId> = found.nodes.iter().map(|seed| seed.node.clone()).collect();
        let mut added = 0;
        let index = loaded.bm25().await;
        for (node, name, score) in index.matches(call.question, limits.seed_k as usize) {
            if known.contains(&node) {
                continue;
            }
            found.details.push(json!({
                "node": node.to_string(),
                "kind": "symbol",
                "how": "bm25",
                "symbol": name,
                "retrievers": ["bm25"],
                "score": score,
                "score_kind": "bm25",
            }));
            found.nodes.push(Seed {
                node,
                map: Some(SEEDS_BM25),
            });
            added += 1;
        }
        if added > 0 {
            if hybrid_answered {
                found.source = "hybrid+bm25";
            } else {
                found.source = "bm25";
                if let Some(reason) = reason.take() {
                    found.notes.push(format!(
                        "seed_source is bm25, the entity index over symbol names, paths and documentation: {reason}"
                    ));
                    found.fallback = Some(reason);
                }
            }
        }
    }
    // `seeds.repo_map` (only in `wanted` under `[run_profile.seeds]
    // repo_map`): personalized PageRank over the SCIP graph, the jump on the
    // entities `seeds.bm25`'s index finds for the question, after the earlier
    // sources' nodes. The index is the entity index of `seeds.bm25`, built
    // whether or not that map is wanted.
    if wanted.contains(&SEEDS_REPO_MAP) && !call.question.trim().is_empty() {
        let repo = loaded.repo_map().await;
        if repo.is_empty() {
            found.notes.push(REPO_MAP_EMPTY_NOTE.to_owned());
        } else {
            let known: HashSet<NodeId> = found.nodes.iter().map(|seed| seed.node.clone()).collect();
            let index = loaded.bm25().await;
            let (question, limit) = (call.question.to_owned(), limits.seed_k as usize);
            let (ranked, stats) = {
                let repo = repo.clone();
                tokio::task::spawn_blocking(move || repo.matches(&index, &question, limit))
                    .await
                    .expect("the PageRank does not panic")
            };
            if stats.personalised == 0 {
                found.notes.push(REPO_MAP_NO_MATCH_NOTE.to_owned());
            }
            let mut added = 0;
            for (node, name, score) in ranked {
                if known.contains(&node) {
                    continue;
                }
                found.details.push(json!({
                    "node": node.to_string(),
                    "kind": "symbol",
                    "how": "repo_map",
                    "symbol": name,
                    "retrievers": ["repo_map"],
                    "score": score,
                    "score_kind": "pagerank",
                    "personalised": stats.personalised,
                    "iterations": stats.iterations,
                }));
                found.nodes.push(Seed {
                    node,
                    map: Some(SEEDS_REPO_MAP),
                });
                added += 1;
            }
            if added > 0 {
                if found.source.is_empty()
                    && let Some(reason) = reason.take()
                {
                    found.notes.push(format!(
                        "seed_source is repo_map, the PageRank over the SCIP graph from the entities the question's words name: {reason}"
                    ));
                    found.fallback = Some(reason);
                }
                found.source = join_source(found.source, "repo_map");
            }
        }
    }
    // `seeds.literal` (only in `wanted` under `[run_profile.seeds] literal`):
    // the nodes of the lines that hold a name of the question's code spans as
    // a whole word, after the hybrid search's and `seeds.bm25`'s nodes. Not
    // started for a question with no such name.
    if wanted.contains(&SEEDS_LITERAL)
        && let Some(note) = no_ripgrep_note(call.seed_search)
    {
        // No search to run ripgrep: said first, whatever the question names.
        found.notes.push(note);
    } else if wanted.contains(&SEEDS_LITERAL) && !literal_needles(call.question).is_empty() {
        let known: HashSet<NodeId> = found.nodes.iter().map(|seed| seed.node.clone()).collect();
        let places = loaded.places().await;
        let (seeds, details, mut notes) = literal_seeds(limits, call, &known, &*places).await?;
        found.notes.append(&mut notes);
        if !seeds.is_empty() {
            found.nodes.extend(seeds);
            found.details.extend(details);
            if found.source.is_empty()
                && let Some(reason) = reason.take()
            {
                found.notes.push(format!(
                    "seed_source is literal, the exact occurrences of the names in code spans: {reason}"
                ));
                found.fallback = Some(reason);
            }
            found.source = join_source(found.source, "literal");
        }
    } else if wanted.contains(&SEEDS_LITERAL) {
        found.notes.push(NO_LITERAL_NAMES_NOTE.to_owned());
    }
    if hybrid_answered {
        return Ok(found);
    }
    if !found.nodes.is_empty() {
        // `seeds.bm25`, `seeds.repo_map` or `seeds.literal` answered where the
        // hybrid search did not; the stand-in is reached only when none did.
    } else if wanted.contains(&SEEDS_MAP) {
        found.source = "name_tokens";
        for (node, matched) in loaded.names.matches(call.question, SEED_COUNT) {
            found.details.push(json!({
                "node": node.to_string(),
                "kind": "symbol",
                "how": "name",
                "retrievers": ["name_tokens"],
                "score": matched,
                "score_kind": "words_in_name",
            }));
            found.nodes.push(Seed {
                node,
                map: Some(SEEDS_MAP),
            });
        }
        if let Some(reason) = reason {
            found.notes.push(format!(
                "seed_source is name_tokens, the stand-in that matches the question's words against symbol names: {reason}"
            ));
            found.fallback = Some(reason);
        }
    } else if let Some(reason) = reason {
        found.source = "hybrid";
        found.fallback = Some(reason);
    }
    if found.source.is_empty() {
        found.source = "none";
    }
    Ok(found)
}

/// Most start nodes the trait paths of one question give.
const MAX_TRAIT_SEEDS: usize = 6;

/// How the traits and trait methods a question names join the start nodes of
/// the other sources (the hybrid search or the stand-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraitSeedMode {
    /// `Replace` when at least one start node is an exact match (the
    /// qualifiers of the written path are on the symbol's path), `Prepend`
    /// when the paths matched by their last segment alone.
    #[default]
    Auto,
    /// The trait start nodes are the only ones from the question's own
    /// sources (the first stage may still add its cards after them).
    Replace,
    /// The trait start nodes go first, the other sources' nodes after them.
    Prepend,
}

impl FromStr for TraitSeedMode {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "auto" => Ok(Self::Auto),
            "replace" => Ok(Self::Replace),
            "prepend" => Ok(Self::Prepend),
            other => Err(anyhow!(
                "trait seeds mode {other:?}: use auto, replace or prepend"
            )),
        }
    }
}

/// Puts the traits and trait methods the question names among the start
/// nodes, when the call walks `scip.implements`: a path in a code span of the
/// question (`tauri::Manager`, `Runtime::run`) that names a trait or a trait
/// method declaration of the map ([`ImplementsMap::find`]) starts the walk at
/// that node, from which the map's edges reach the implementing types or
/// members. Expanding such a start node follows every one of those
/// `scip.implements` edges (no gate, no beam) and ranks them ahead of its other
/// edges ([`lci_maps::traverse::frontier::hop_from`]; [`Seeds::trait_nodes`]
/// names the nodes), so no implementor is lost to the beam or to a reference
/// that prices higher. They go in front of the start nodes of the other sources
/// (`Prepend`) or take their place (`Replace`; `Auto` replaces when a path is
/// an exact match). Call it only when `scip.implements` is on; a call without
/// that map never gets here, so its start nodes are those of [`find_seeds`]
/// alone. The seeds are not credited to a map (`Seed::map` is `None`, as for
/// given seeds): no map ranks them, the question's own words named them.
///
/// Sets [`Seeds::trait_seeds`] (0 when nothing was added). When the caller
/// gave `--seeds`, none is added; the given start nodes that are targets of the
/// map are walked as trait seeds ([`Seeds::given_traits`]). Nothing changes
/// when no path names a target, or on a continuation (which does not look for
/// seeds). The source becomes `traits`, or `traits+<source>` when the other
/// start nodes stay. Returns the note the response carries, if seeds were
/// added.
pub(super) fn add_trait_seeds(
    found: &mut Seeds,
    map: &ImplementsMap,
    question: &str,
    mode: TraitSeedMode,
) -> Option<String> {
    found.trait_seeds = Some(0);
    if found.source == "given" {
        found.given_traits = found
            .nodes
            .iter()
            .filter(|seed| map.targets().iter().any(|t| t.node == seed.node))
            .map(|seed| seed.node.clone())
            .collect();
        return None;
    }
    let (names, _) = code_names(question);
    let named: Vec<_> = map
        .find(&names, MAX_TRAIT_SEEDS)
        .into_iter()
        .take(MAX_TRAIT_SEEDS)
        .collect();
    if named.is_empty() {
        return None;
    }
    let exact = named.iter().filter(|hit| hit.exact).count();
    let replace = match mode {
        TraitSeedMode::Replace => true,
        TraitSeedMode::Prepend => false,
        TraitSeedMode::Auto => exact > 0,
    };
    let mut nodes: Vec<Seed> = Vec::new();
    let mut details: Vec<Value> = Vec::new();
    for hit in &named {
        nodes.push(Seed {
            node: hit.target.node.clone(),
            map: None,
        });
        details.push(json!({
            "node": hit.target.node.to_string(),
            "kind": "symbol",
            "how": match hit.target.kind {
                TargetKind::Trait => "trait",
                TargetKind::Method => "trait_method",
            },
            "written": hit.path,
            "retrievers": [SCIP_IMPLEMENTS_ID],
            "score": u32::from(hit.exact),
            "score_kind": if hit.exact { "path_match" } else { "name_match_only" },
        }));
    }
    let count = named.len();
    let loose = count - exact;
    let before = found.source;
    let other = found.nodes.len();
    let what = if replace {
        found.nodes.clear();
        found.details.clear();
        found.source = "traits";
        if other == 0 {
            "no other source gave start nodes".to_owned()
        } else {
            format!("they replace the {other} start nodes of seed_source {before}")
        }
    } else {
        let aligned = found.details.len() == found.nodes.len();
        let taken: HashSet<NodeId> = nodes.iter().map(|seed| seed.node.clone()).collect();
        for (i, seed) in std::mem::take(&mut found.nodes).into_iter().enumerate() {
            if taken.contains(&seed.node) {
                continue;
            }
            if aligned {
                details.push(found.details[i].clone());
            }
            nodes.push(seed);
        }
        if !aligned {
            details.append(&mut found.details);
        }
        found.source = match before {
            "hybrid" => "traits+hybrid",
            "hybrid+bm25" => "traits+hybrid+bm25",
            "bm25" => "traits+bm25",
            "hybrid+literal" => "traits+hybrid+literal",
            "hybrid+bm25+literal" => "traits+hybrid+bm25+literal",
            "bm25+literal" => "traits+bm25+literal",
            "hybrid+repo_map" => "traits+hybrid+repo_map",
            "hybrid+bm25+repo_map" => "traits+hybrid+bm25+repo_map",
            "bm25+repo_map" => "traits+bm25+repo_map",
            "repo_map" => "traits+repo_map",
            "hybrid+repo_map+literal" => "traits+hybrid+repo_map+literal",
            "hybrid+bm25+repo_map+literal" => "traits+hybrid+bm25+repo_map+literal",
            "bm25+repo_map+literal" => "traits+bm25+repo_map+literal",
            "repo_map+literal" => "traits+repo_map+literal",
            "literal" => "traits+literal",
            "name_tokens" => "traits+name_tokens",
            "exact" => "traits+exact",
            _ => "traits",
        };
        if other == 0 {
            "no other source gave start nodes".to_owned()
        } else {
            format!("the {other} start nodes of seed_source {before} follow them")
        }
    };
    found.nodes = nodes;
    found.details = details;
    found.trait_seeds = Some(count);
    Some(format!(
        "{count} start nodes are the traits and trait methods the question's paths name ({loose} of them by the last segment alone); {what}; {SCIP_IMPLEMENTS_ID} follows their edges to the implementing types and members"
    ))
}

/// Adds `trait_seeds` (how many start nodes are the traits and trait methods
/// the question named) to a response of a call that walks `scip.implements`;
/// a call without it has no such field.
pub(super) fn add_trait_seed_count(response: &mut Value, found: &Seeds) {
    if let (Some(count), Some(object)) = (found.trait_seeds, response.as_object_mut()) {
        object.insert("trait_seeds".into(), json!(count));
    }
}
