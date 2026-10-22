//! The three retrieval channels (vector, lexical, LSP symbols) and the
//! source-role prior wrapped as [`Map`]s, for the T0 bit-for-bit check
//! against [`App::fuse_hits`] (History: spike slice S2; spec
//! `docs/spike/multimap/specs/map-trait.md`, 3.5). Each map calls the code the production search calls
//! (`Models::query` and `Store::search`, `lexical::search` and
//! [`App::map_lexical_matches`], [`App::provider_symbols`] and
//! [`App::map_lsp_locations`], `Hit::source_role_prior`) and reports with
//! today's warning and note texts. The production search does not use them:
//! they exist so the registry can be checked against [`App::fuse_hits`] bit
//! for bit (T0) and so the contract kit can run against real channels.
//!
//! They live in the root crate, not in `lci-maps`, because the code they
//! wrap does (a deviation from map-trait.md 3.1, which placed the lexical and
//! LSP maps in `lci-maps`; moving that code would change the production
//! path).

mod record;

use super::{App, lsp_failure_warning, lsp_symbol_queries};
use crate::{
    config::Config,
    filter::EffectiveFilter,
    lexical,
    store::{Hit, Snapshot, Store},
    workspace::Workspace,
};
use lci_core::node::NodeKind;
use lci_maps::{
    Contribution, Cost, EdgeKind, Evidence, Freshness, FreshnessModel, Map, MapDescriptor, MapHit,
    MapRequest, MapResponse, MapStatus, Need, Observed, Registry, SnapshotView, SourceLabel,
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub(crate) const SEMANTIC: &str = "vec.chunk.code";
pub(crate) const LEXICAL: &str = "lexical";
pub(crate) const LSP: &str = "lsp.symbols";
pub(crate) const SOURCE_ROLE: &str = "prior.source_role";

fn descriptor(
    id: &'static str,
    contribution: Contribution,
    freshness: FreshnessModel,
    label: SourceLabel,
    needs: Vec<Need>,
) -> MapDescriptor {
    MapDescriptor {
        id,
        version: 1,
        contribution,
        accepts_text: contribution == Contribution::RankList,
        accepts: if contribution == Contribution::Prior {
            vec![NodeKind::Chunk]
        } else {
            vec![]
        },
        emits: vec![NodeKind::Chunk],
        freshness,
        source_labels: vec![label],
        needs,
    }
}

pub(crate) fn semantic_descriptor() -> MapDescriptor {
    descriptor(
        SEMANTIC,
        Contribution::RankList,
        FreshnessModel::PerFile,
        SourceLabel::Vector,
        vec![Need::Embedding],
    )
}

pub(crate) fn lexical_descriptor() -> MapDescriptor {
    descriptor(
        LEXICAL,
        Contribution::RankList,
        FreshnessModel::Live,
        SourceLabel::Lexical,
        vec![Need::Ripgrep],
    )
}

pub(crate) fn lsp_descriptor() -> MapDescriptor {
    descriptor(
        LSP,
        Contribution::RankList,
        FreshnessModel::Live,
        SourceLabel::Lsp,
        vec![Need::Lsp],
    )
}

pub(crate) fn source_role_descriptor() -> MapDescriptor {
    descriptor(
        SOURCE_ROLE,
        Contribution::Prior,
        FreshnessModel::Live,
        SourceLabel::Prior,
        vec![],
    )
}

/// Which legacy channel a list of hits came from: the rank and score
/// fields it set on each [`Hit`].
#[derive(Clone, Copy)]
pub(crate) enum Channel {
    Semantic,
    Lexical,
    Lsp,
}

impl Channel {
    fn rank(self, hit: &Hit) -> Option<usize> {
        match self {
            Self::Semantic => hit.semantic_rank,
            Self::Lexical => hit.lexical_rank,
            Self::Lsp => hit.lsp_rank,
        }
    }

    fn score(self, hit: &Hit) -> Option<f32> {
        match self {
            Self::Semantic => hit.semantic_score,
            Self::Lexical => hit.lexical_score,
            Self::Lsp => None,
        }
    }

    fn label(self) -> SourceLabel {
        match self {
            Self::Semantic => SourceLabel::Vector,
            Self::Lexical => SourceLabel::Lexical,
            Self::Lsp => SourceLabel::Lsp,
        }
    }
}

/// A legacy channel's list as map hits: each hit's NodeId from the view,
/// its rank exactly as fusion consumed it. A hit that is not a chunk of the
/// view is left out (none is, when the list came from the same snapshot and
/// filter as the view).
///
/// A chunk listed twice (two stored rows with one path, lines and hash) is
/// kept once, at its first rank; `fuse_hits` would merge the two rows into
/// one entry too, but scored by the later one for the semantic channel and
/// by both for the others. No real channel returns a chunk twice.
pub(crate) fn map_hits(channel: Channel, hits: &[Hit], view: &SnapshotView<'_>) -> Vec<MapHit> {
    let mut seen = std::collections::HashSet::new();
    hits.iter()
        .filter_map(|hit| {
            let node = view.locate(&hit.chunk)?.clone();
            if !seen.insert(node.clone()) {
                return None;
            }
            let rank = u32::try_from(channel.rank(hit)?).ok()?;
            let detail = matches!(channel, Channel::Lexical)
                .then(|| format!("{} credited lines", hit.lexical_match_count));
            Some(MapHit {
                node,
                rank,
                score: channel.score(hit),
                evidence: vec![Evidence {
                    via: None,
                    edge: EdgeKind::QueryMatch,
                    label: channel.label(),
                    detail,
                }],
            })
        })
        .collect()
}

fn response(
    descriptor: &MapDescriptor,
    view: &SnapshotView<'_>,
    hits: Vec<MapHit>,
    status: MapStatus,
    started: Instant,
    external_calls: u32,
) -> MapResponse {
    let mut response = MapResponse::empty(descriptor, view, status);
    response.hits = hits;
    response.cost = Cost {
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        external_calls,
        nodes_touched: response.hits.len() as u64,
        ..Cost::default()
    };
    response.freshness = Freshness {
        observed: if descriptor.freshness == FreshnessModel::Live {
            Observed::LiveTree
        } else {
            Observed::Snapshot
        },
        ..response.freshness
    };
    response
}

/// A legacy channel's list recorded once and returned on every call: the
/// fusion unit cases' inputs, run through the registry by T0.
#[cfg(test)]
pub(crate) struct RecordedChannel {
    descriptor: MapDescriptor,
    channel: Channel,
    hits: Vec<Hit>,
}

#[cfg(test)]
impl RecordedChannel {
    pub(crate) fn new(channel: Channel, hits: Vec<Hit>) -> Self {
        let descriptor = match channel {
            Channel::Semantic => semantic_descriptor(),
            Channel::Lexical => lexical_descriptor(),
            Channel::Lsp => lsp_descriptor(),
        };
        Self {
            descriptor,
            channel,
            hits,
        }
    }
}

#[cfg(test)]
#[lci_maps::async_trait]
impl Map for RecordedChannel {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, _: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let hits = map_hits(self.channel, &self.hits, view);
        response(
            &self.descriptor,
            view,
            hits,
            MapStatus::Ok,
            Instant::now(),
            0,
        )
    }
}

/// `vec.chunk.code`: the query embedding, then the LanceDB cosine search
/// over the snapshot with the run's filter.
pub(crate) struct SemanticMap<'a> {
    pub(crate) app: &'a App,
    pub(crate) snapshot: &'a Snapshot,
    pub(crate) workspace: &'a Workspace,
    pub(crate) filters: &'a EffectiveFilter,
    /// How many snapshot chunks pass `filters` (the universe's length), so
    /// the search stops once it has found them all.
    pub(crate) matching: usize,
    pub(crate) descriptor: MapDescriptor,
}

#[lci_maps::async_trait]
impl Map for SemanticMap<'_> {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let fail = |warning: String, calls| {
            response(
                &self.descriptor,
                view,
                vec![],
                MapStatus::Failed { warning },
                started,
                calls,
            )
        };
        let Some(text) = request.text else {
            return fail("no query text".into(), 0);
        };
        let vector = match self.app.models.query(text).await {
            Ok(vector) => vector,
            Err(error) => {
                return fail(
                    format!(
                        "Query embedding unavailable; results from the other enabled channels used: {error:#}"
                    ),
                    1,
                );
            }
        };
        match Store::search_code_lane(
            self.snapshot,
            self.workspace,
            &vector,
            request.limit,
            self.filters,
            self.matching,
        )
        .await
        {
            Ok(found) => response(
                &self.descriptor,
                view,
                map_hits(Channel::Semantic, &found.hits, view),
                MapStatus::Ok,
                started,
                1,
            ),
            Err(error) => fail(format!("Semantic retrieval unavailable: {error:#}"), 1),
        }
    }
}

/// `lexical`: ripgrep over the live tree, mapped onto the snapshot's chunks
/// and scored as the production channel does.
pub(crate) struct LexicalMap<'a> {
    pub(crate) config: &'a Config,
    pub(crate) workspace: &'a Workspace,
    pub(crate) universe: &'a [Hit],
    pub(crate) descriptor: MapDescriptor,
}

#[lci_maps::async_trait]
impl Map for LexicalMap<'_> {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let Some(text) = request.text else {
            return response(
                &self.descriptor,
                view,
                vec![],
                MapStatus::Failed {
                    warning: "no query text".into(),
                },
                started,
                0,
            );
        };
        match lexical::search(&self.config.ripgrep_path, &self.workspace.path, text).await {
            Ok(search) => {
                let hits = App::map_lexical_matches(self.universe, search, request.limit);
                response(
                    &self.descriptor,
                    view,
                    map_hits(Channel::Lexical, &hits, view),
                    MapStatus::Ok,
                    started,
                    1,
                )
            }
            Err(error) => response(
                &self.descriptor,
                view,
                vec![],
                MapStatus::Failed {
                    warning: format!("Lexical retrieval unavailable: {error:#}"),
                },
                started,
                1,
            ),
        }
    }
}

/// `lsp.symbols`: `workspace/symbol` queries to every applicable language
/// server under `lsp_search_budget_ms`, merged query by query and mapped
/// onto the snapshot's chunks. Provider selection, the symbol queries, the
/// budgeted fan-out and merge, and the note and warning texts are the
/// functions the LSP arm of `search_once` calls.
pub(crate) struct LspMap<'a> {
    pub(crate) app: &'a App,
    pub(crate) workspace: &'a Workspace,
    pub(crate) universe: &'a [Hit],
    pub(crate) descriptor: MapDescriptor,
}

#[lci_maps::async_trait]
impl Map for LspMap<'_> {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let providers = self.app.applicable_lsp_adapters(self.universe);
        if providers.is_empty() {
            return response(
                &self.descriptor,
                view,
                vec![],
                MapStatus::Skipped {
                    reason: "no enabled language server applies".into(),
                },
                started,
                0,
            );
        }
        let symbol_queries = lsp_symbol_queries(request.text.unwrap_or_default());
        let channel = self
            .app
            .lsp_channel(&providers, self.workspace, &symbol_queries)
            .await;
        let calls = (providers.len() * symbol_queries.len()) as u32;
        let hits = App::map_lsp_locations(self.universe, channel.locations, request.limit);
        // The notes `search_once` reports, in its order: what the tree barrier
        // did, then the providers past the budget. A map carries one note, so
        // they are joined with "; " (N110).
        let mut notes = crate::app::navigation::barrier_notes(
            &channel.restarted,
            &channel.kept,
            &channel.reused,
        );
        if !channel.over_budget.is_empty() {
            notes.push(
                self.app
                    .lsp_over_budget_note(&channel.over_budget, &self.workspace.id)
                    .await,
            );
        }
        let note = (!notes.is_empty()).then(|| notes.join("; "));
        let warning =
            (!channel.failures.is_empty()).then(|| lsp_failure_warning(&channel.failures));
        let status = lsp_status(
            note,
            warning,
            channel.failures.len() == providers.len(),
            hits.is_empty(),
        );
        response(
            &self.descriptor,
            view,
            map_hits(Channel::Lsp, &hits, view),
            status,
            started,
            calls,
        )
    }
}

/// The LSP map's status. `search_once` fuses whatever was gathered, also
/// from a provider that failed after answering some queries; so only an
/// answer with no hit, from providers that all failed, is a failure.
pub(crate) fn lsp_status(
    note: Option<String>,
    warning: Option<String>,
    all_failed: bool,
    no_hits: bool,
) -> MapStatus {
    match (note, warning) {
        (None, None) => MapStatus::Ok,
        (None, Some(warning)) if all_failed && no_hits => MapStatus::Failed { warning },
        (note, warning) => MapStatus::Partial { note, warning },
    }
}

/// `prior.source_role`: for every seed (every fused node), the prior of its
/// chunk's source role, added unweighted after the rank lists.
pub(crate) struct SourceRolePrior<'a> {
    /// The hits the view was built from, in the view's order.
    pub(crate) universe: &'a [Hit],
    pub(crate) descriptor: MapDescriptor,
}

#[lci_maps::async_trait]
impl Map for SourceRolePrior<'_> {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let positions: HashMap<&lci_core::node::NodeId, usize> =
            view.ids().iter().zip(0..).collect();
        let hits = request
            .seeds
            .iter()
            .filter_map(|(node, _)| positions.get(node).map(|&at| (node, &self.universe[at])))
            .enumerate()
            .map(|(rank, (node, hit))| MapHit {
                node: node.clone(),
                rank: rank as u32 + 1,
                score: Some(hit.source_role_prior),
                evidence: vec![Evidence {
                    via: None,
                    edge: EdgeKind::Prior,
                    label: SourceLabel::Prior,
                    detail: None,
                }],
            })
            .collect();
        response(&self.descriptor, view, hits, MapStatus::Ok, started, 0)
    }
}

/// A registry with the three channel maps and the prior, with today's
/// weights, candidate counts and `[channels]` switches. A switched-off
/// channel is registered but disabled, so it is skipped silently.
pub(crate) fn legacy_registry<'a>(
    config: &Config,
    channels: crate::config::ChannelsConfig,
    semantic: Box<dyn Map + 'a>,
    lexical: Box<dyn Map + 'a>,
    lsp: Box<dyn Map + 'a>,
    prior: Box<dyn Map + 'a>,
) -> Registry<'a> {
    let weights = &config.fusion;
    let mut registry = Registry::new(config.rrf_k);
    for (map, weight, limit) in [
        (
            semantic,
            weights.semantic_weight,
            config.semantic_candidate_count,
        ),
        (
            lexical,
            weights.lexical_weight,
            config.lexical_candidate_count,
        ),
        (lsp, weights.lsp_weight, config.lsp_candidate_count),
        (prior, 1.0, usize::MAX),
    ] {
        // Contract C5: `Registry::run` checks every answer against the
        // map's labels (merge condition M6), so no wrapper is needed.
        registry
            .register(map, weight, limit)
            .expect("the legacy maps have distinct ids and no filter");
    }
    assert!(registry.set_enabled(SEMANTIC, channels.semantic));
    assert!(registry.set_enabled(LEXICAL, channels.lexical));
    assert!(registry.set_enabled(LSP, channels.lsp));
    registry
}

/// One fused candidate of [`App::fuse_through_maps`].
#[derive(Debug, Clone, PartialEq)]
pub struct MapFusionRow {
    pub node: String,
    pub relative_file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub fusion_score: f32,
}

/// What [`App::fuse_through_maps`] returns: the fused pool in fused order,
/// and the warnings and notes the maps raised, in fusion order.
#[derive(Debug, Clone)]
pub struct MapFusion {
    pub rows: Vec<MapFusionRow>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

/// What a registry run of the legacy maps ran over, handed to the
/// callbacks of [`App::with_legacy_run`].
pub(super) struct LegacySetup<'s, 'a> {
    pub view: &'s SnapshotView<'a>,
    pub universe: &'s [Hit],
    pub snapshot: &'s Snapshot,
    pub workspace: &'s Workspace,
    /// The `[channels]` switches the run uses: the configured ones, with the
    /// semantic channel off on a vectorless snapshot, as `search_once`
    /// adjusts them.
    pub channels: crate::config::ChannelsConfig,
    /// Whether the run turned the semantic channel off for that reason, so
    /// that it raises the production search's warning.
    pub vectorless: bool,
}

impl App {
    /// Spike slice S2, for tests only: the fused pool of `query` over the
    /// workspace's current snapshot (no filter), computed by the map
    /// registry with the four wrapped legacy maps instead of `fuse_hits`. No
    /// index work: the workspace must already be indexed. The production
    /// search never calls this.
    #[doc(hidden)]
    pub async fn fuse_through_maps(
        &self,
        path: &std::path::Path,
        query: &str,
    ) -> anyhow::Result<MapFusion> {
        self.with_legacy_run(path, query, |_| (), |(), setup, run| map_fusion(setup, run))
            .await
    }

    /// Runs the legacy maps through the registry as
    /// [`Self::fuse_through_maps`] describes, calling `before` once the
    /// snapshot and its universe are resolved and `after` with the run.
    pub(super) async fn with_legacy_run<B, T>(
        &self,
        path: &std::path::Path,
        query: &str,
        before: impl FnOnce(&LegacySetup<'_, '_>) -> B,
        after: impl FnOnce(B, &LegacySetup<'_, '_>, &lci_maps::Run) -> T,
    ) -> anyhow::Result<T> {
        use anyhow::Context;
        let filters = EffectiveFilter::from_request(&crate::filter::FilterRequest::default())?;
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let _read = coordination.lock.read().await;
        let snapshot = self
            .store
            .snapshot(&workspace)
            .await?
            .context("workspace is not indexed")?;
        let mut chunks = Store::chunks(&snapshot, &workspace).await?;
        chunks.retain(|hit| super::hit_matches(&filters, hit));
        // The production search's code lane: the prose chunks belong to the
        // docs lane, which has no map, so they are not in the universe.
        let (universe, prose) = super::split_prose(&filters, chunks);
        let code_filters = if prose.is_empty() {
            filters.clone()
        } else {
            super::lane_filter(&filters, &universe)
        };
        let view = SnapshotView::new(
            lci_maps::SnapshotRef {
                indexed_at: snapshot.indexed_at.clone(),
                fingerprint: String::new(),
            },
            universe.iter().map(|hit| &hit.chunk).collect(),
        );
        // A vectorless snapshot (built while the embedding service was
        // unreachable) turns the semantic channel off, as in `search_once`:
        // the query is not embedded and no vector read is tried.
        let mut channels = self.config.channels;
        let vectorless = channels.semantic && snapshot.vectors_missing;
        if vectorless {
            channels.semantic = false;
        }
        let registry = legacy_registry(
            &self.config,
            channels,
            Box::new(SemanticMap {
                app: self,
                snapshot: &snapshot,
                workspace: &workspace,
                filters: &code_filters,
                matching: universe.len(),
                descriptor: semantic_descriptor(),
            }),
            Box::new(LexicalMap {
                config: &self.config,
                workspace: &workspace,
                universe: &universe,
                descriptor: lexical_descriptor(),
            }),
            Box::new(LspMap {
                app: self,
                workspace: &workspace,
                universe: &universe,
                descriptor: lsp_descriptor(),
            }),
            Box::new(SourceRolePrior {
                universe: &universe,
                descriptor: source_role_descriptor(),
            }),
        );
        let setup = LegacySetup {
            view: &view,
            universe: &universe,
            snapshot: &snapshot,
            workspace: &workspace,
            channels,
            vectorless,
        };
        let prepared = before(&setup);
        let run = registry.run(Some(query), LEGACY_RUN_BUDGET, &view).await;
        Ok(after(prepared, &setup, &run))
    }
}

/// Each map's budget in a registry run of the legacy maps.
pub(super) const LEGACY_RUN_BUDGET: Duration = Duration::from_secs(600);

fn map_fusion(setup: &LegacySetup<'_, '_>, run: &lci_maps::Run) -> MapFusion {
    let rows = run
        .fused
        .iter()
        .map(|fused| {
            let chunk = setup
                .view
                .chunk(&fused.node)
                .expect("fused nodes are chunks");
            MapFusionRow {
                node: fused.node.to_string(),
                relative_file_path: chunk.relative_file_path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                fusion_score: fused.score,
            }
        })
        .collect();
    MapFusion {
        rows,
        // The production search raises the vectorless warning first.
        warnings: setup
            .vectorless
            .then(|| super::VECTORLESS_WARNING.to_owned())
            .into_iter()
            .chain(run.warnings().into_iter().map(|(_, w)| w.to_owned()))
            .collect(),
        notes: run.notes().into_iter().map(|(_, n)| n.to_owned()).collect(),
    }
}
