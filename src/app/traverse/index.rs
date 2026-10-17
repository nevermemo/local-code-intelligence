//! The loaded SCIP index and the caches of the maps built from it, shared
//! between calls (traversals and recipes).

use super::seeds::NameSeeds;
use anyhow::{Context, Result, anyhow};
use lci_core::node::NodeId;
use lci_maps::{
    Budget, Map, MapDescriptor, MapRequest, MapResponse, SnapshotView,
    config_key::{ConfigKeyMap, ConfigKeyOptions},
    contains::ContainsMap,
    impact::{Decl, ImpactIndex},
    scip_graph::{DirSource, ScipMap, SourceText},
    scip_impact::{ImpactMap, ImpactOptions},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::SystemTime,
};

/// The SCIP index and the name index over it, shared between calls.
pub(super) struct Loaded {
    pub(super) stamp: (Option<SystemTime>, u64),
    pub(super) scip: Arc<ScipMap>,
    pub(super) names: Arc<NameSeeds>,
    /// The containment of the graph's files and symbols (map
    /// `structure.contains`); a call adds the lines of its chunk seeds.
    pub(super) contains: ContainsMap,
    /// The graph nodes of each file (path as the index writes it, `/`
    /// separated), in graph order.
    pub(super) by_file: HashMap<String, Vec<usize>>,
}

impl Loaded {
    pub(super) fn new(stamp: (Option<SystemTime>, u64), scip: ScipMap) -> Self {
        let names = NameSeeds::new(&scip);
        let contains = ContainsMap::new(&scip);
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, node) in scip.graph().nodes.iter().enumerate() {
            by_file
                .entry(node.file.replace('\\', "/"))
                .or_default()
                .push(i);
        }
        Self {
            stamp,
            scip: Arc::new(scip),
            names: Arc::new(names),
            contains,
            by_file,
        }
    }
}

type Cache = Mutex<HashMap<(PathBuf, Option<PathBuf>), Arc<Loaded>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub(super) async fn load(index: &Path, root: Option<&Path>) -> Result<Arc<Loaded>> {
    let meta = std::fs::metadata(index).with_context(|| {
        format!(
            "no SCIP index at {}; write one with `rust-analyzer scip` in the workspace or set traverse.scip_index",
            index.display()
        )
    })?;
    let stamp = (meta.modified().ok(), meta.len());
    let key = (index.to_path_buf(), root.map(Path::to_path_buf));
    if let Some(found) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .filter(|loaded| loaded.stamp == stamp)
    {
        return Ok(found.clone());
    }
    let (path, root_dir) = (index.to_path_buf(), root.map(Path::to_path_buf));
    let loaded = tokio::task::spawn_blocking(move || -> Result<Loaded> {
        let source = root_dir.map(DirSource);
        let graph = lci_maps::scip_graph::ScipGraph::load(
            &path,
            source.as_ref().map(|s| s as &dyn SourceText),
        )
        .map_err(|error| anyhow!(error))?;
        Ok(Loaded::new(stamp, ScipMap::with_default_ids(graph)))
    })
    .await
    .context("load the SCIP index")??;
    let loaded = Arc::new(loaded);
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, loaded.clone());
    Ok(loaded)
}

type Stamp = (Option<SystemTime>, u64);

/// The maps built from a SCIP index and a tree, one per (index, tree), each
/// valid for the stamp of the index it was built from.
struct StampCache<M>(Mutex<HashMap<(PathBuf, PathBuf), Built<M>>>);

/// A map with the stamp of the index it was built from.
type Built<M> = (Stamp, Arc<M>);

impl<M> Default for StampCache<M> {
    fn default() -> Self {
        Self(Mutex::default())
    }
}

impl<M> StampCache<M> {
    fn get(&self, index: &Path, root: &Path, stamp: Stamp) -> Option<Arc<M>> {
        let cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (built_at, map) = cache.get(&(index.to_path_buf(), root.to_path_buf()))?;
        (*built_at == stamp).then(|| map.clone())
    }

    fn put(&self, index: &Path, root: &Path, stamp: Stamp, map: M) -> Arc<M> {
        let map = Arc::new(map);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (index.to_path_buf(), root.to_path_buf()),
                (stamp, map.clone()),
            );
        map
    }
}

/// The `config.key` map of a SCIP index and a tree, built once per index
/// stamp (a tree edit after that is not seen; the map's freshness is
/// `global`). Declarations take the ids of the SCIP graph.
pub(super) async fn config_key_map(
    index: &Path,
    root: &Path,
    loaded: &Loaded,
) -> Result<Arc<ConfigKeyMap>> {
    static CACHE: OnceLock<StampCache<ConfigKeyMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(map) = cache.get(index, root, loaded.stamp) {
        return Ok(map);
    }
    let (index_buf, root_buf, scip) =
        (index.to_path_buf(), root.to_path_buf(), loaded.scip.clone());
    let map = tokio::task::spawn_blocking(move || -> Result<ConfigKeyMap> {
        let (index, root) = (index_buf, root_buf);
        let source = DirSource(root.clone());
        let ix = ImpactIndex::load(&index, Some(&source)).map_err(|error| anyhow!(error))?;
        let resolve = |decl: &Decl| {
            scip.graph()
                .node_index(&decl.symbol)
                .map(|node| scip.node_id(node).clone())
        };
        ConfigKeyMap::extract(&ix, &root, &source, &resolve, ConfigKeyOptions::default())
            .map_err(|error| anyhow!(error))
    })
    .await
    .context("build config.key")??;
    Ok(cache.put(index, root, loaded.stamp, map))
}

/// The `scip.impact` map of a SCIP index and a tree, built once per index
/// stamp. Units the SCIP graph holds take its node ids.
pub(super) async fn impact_map(
    index: &Path,
    root: &Path,
    loaded: &Loaded,
) -> Result<Arc<ImpactMap>> {
    static CACHE: OnceLock<StampCache<ImpactMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(map) = cache.get(index, root, loaded.stamp) {
        return Ok(map);
    }
    let (index_buf, root_buf, scip) =
        (index.to_path_buf(), root.to_path_buf(), loaded.scip.clone());
    let map = tokio::task::spawn_blocking(move || -> Result<ImpactMap> {
        let (index, root) = (index_buf, root_buf);
        let source = DirSource(root);
        let ix = ImpactIndex::load(&index, Some(&source)).map_err(|error| anyhow!(error))?;
        let resolve = |symbol: &str| {
            scip.graph()
                .node_index(symbol)
                .map(|node| scip.node_id(node).clone())
        };
        Ok(ImpactMap::new(ix, &resolve, ImpactOptions::default()))
    })
    .await
    .context("build scip.impact")??;
    Ok(cache.put(index, root, loaded.stamp, map))
}

/// What a recipe reads of a SCIP index and its tree: the call graph
/// (`graph.scip`) and the impact map (`scip.impact`, whose index serves
/// `exact.closure`). Both are cached by the stamp of the index, as `run` uses
/// them, so a recipe and a traversal over the same index share one load.
pub(crate) struct OpenedIndex {
    pub scip: Arc<ScipMap>,
    pub impact: Arc<ImpactMap>,
}

/// Opens (or takes from the cache) the maps of `index` over `root`.
pub(crate) async fn open_index(index: &Path, root: &Path) -> Result<OpenedIndex> {
    let loaded = load(index, Some(root)).await?;
    let impact = impact_map(index, root, &loaded).await?;
    Ok(OpenedIndex {
        scip: loaded.scip.clone(),
        impact,
    })
}

/// The `config.key` map of `index` over `root` (built once per index stamp; it
/// runs the key extractor, which needs `rg` on `PATH`).
pub(crate) async fn open_config_key(index: &Path, root: &Path) -> Result<Arc<ConfigKeyMap>> {
    let loaded = load(index, Some(root)).await?;
    config_key_map(index, root, &loaded).await
}

/// A map shared between calls, registered by value.
pub(super) struct Shared<M>(pub(super) Arc<M>);

#[lci_maps::async_trait]
impl<M: Map> Map for Shared<M> {
    fn describe(&self) -> &MapDescriptor {
        self.0.describe()
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        self.0.query(request, view).await
    }

    async fn expand(
        &self,
        node: &NodeId,
        budget: Budget,
        view: &SnapshotView<'_>,
    ) -> lci_maps::edge::Expanded {
        self.0.expand(node, budget, view).await
    }

    fn card(&self) -> lci_maps::edge::MapCard {
        self.0.card()
    }
}
