//! `[run_profile.traverse] map_freshness`: what each map of a call says about how current
//! its answer is (slice K2a-2).
//!
//! The comparison is the tree against the index file. A SCIP index carries no
//! fingerprint of its tree, so the baseline is built from two facts: the
//! files that are not newer than the index file (modified time at or before
//! the index's) are taken to be as the index saw them, and their content
//! hashes are recorded when this process first sees the index's current stamp
//! (modified time and length). A file newer than the index file is left out
//! of the baseline and counts from the first call. Later calls count a
//! baseline file whose hash changed (a touch without a content change does
//! not count), a file added or removed, and the files left out, each file
//! once however often it is edited. A rewritten index has a new stamp, hence a
//! new baseline. Not seen: a change that left the file older than the index
//! file, and an edit made after the first call that saw the stamp to a file
//! that was newer than the index (it counted from the start). An index file
//! without a modified time is not compared (`lag` is null). K20b owns the full
//! label. Every call reads and hashes the tree's source files.
//!
//! The maps themselves do not see the tree, so [`Observer`] wraps each map
//! that states a freshness: it sets [`Expanded::freshness`] unless the map did,
//! and keeps the last one it saw per map for the response. With the key off
//! nothing is wrapped.

use anyhow::{Context, Result};
use lci_core::manifest::CachedFile;
use lci_core::{chunk, node::NodeId};
use lci_maps::{
    Budget, Map, MapDescriptor, MapId, MapRequest, MapResponse, Observed, SnapshotView,
    edge::{Expanded, Freshness, MapCard},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::SystemTime,
};

type Stamp = (Option<SystemTime>, u64);
type Files = HashMap<String, CachedFile>;

/// The maps built from the SCIP index and the tree: stale when the tree
/// differs from the baseline of the index's stamp.
const SCIP_BUILT: [MapId; 7] = [
    "graph.scip",
    "structure.contains",
    "seeds.scip_names",
    "exact.closure",
    "config.key",
    "scip.impact",
    "scip.implements",
];

/// The maps that read the working tree or git when they answer.
const LIVE: [MapId; 5] = [
    "grep.live",
    "lsp.live",
    "git.history",
    "exact.pickaxe",
    "probe.compile",
];

/// The baselines: one per (index, tree), valid for the stamp it was taken at.
type Baselines = Mutex<HashMap<(PathBuf, PathBuf), (Stamp, Arc<Files>)>>;

fn baselines() -> &'static Baselines {
    static BASELINES: OnceLock<Baselines> = OnceLock::new();
    BASELINES.get_or_init(Default::default)
}

/// The files the language adapters index, prose left out, with the chunks
/// dropped: only the content hashes are kept, and a scan against these files
/// parses just the files that changed.
fn source_files(root: &Path, previous: &Files) -> Result<Files> {
    let scan = chunk::scan_incremental_with(root, previous, false)?;
    Ok(scan
        .files
        .into_iter()
        .map(|(path, file)| {
            (
                path,
                CachedFile {
                    chunks: Vec::new(),
                    ..file
                },
            )
        })
        .collect())
}

/// How many files differ between two sets: changed, added or removed.
fn files_differing(then: &Files, now: &Files) -> u32 {
    let changed = now
        .iter()
        .filter(|(path, file)| {
            then.get(*path)
                .is_none_or(|old| old.content_hash != file.content_hash)
        })
        .count();
    let removed = then.keys().filter(|path| !now.contains_key(*path)).count();
    u32::try_from(changed + removed).unwrap_or(u32::MAX)
}

/// The files of `now` that are not newer than `index_modified` (a file whose
/// modified time cannot be read counts as not newer): the baseline.
fn not_newer(root: &Path, now: &Files, index_modified: SystemTime) -> Files {
    now.iter()
        .filter(|(path, _)| {
            match std::fs::metadata(root.join(path)).and_then(|meta| meta.modified()) {
                Ok(modified) => modified <= index_modified,
                Err(_) => true,
            }
        })
        .map(|(path, file)| (path.clone(), file.clone()))
        .collect()
}

/// How many source files differ from the index file: the files newer than it
/// and those changed since this process first saw its stamp (see the module
/// comment). The first call that sees a stamp takes the baseline. `None` when
/// the stamp has no modified time (not compared). Every call reads and hashes
/// the tree's source files; only changed ones are parsed.
pub(super) async fn tree_lag(index: &Path, root: &Path, stamp: Stamp) -> Result<Option<u32>> {
    let Some(index_modified) = stamp.0 else {
        return Ok(None);
    };
    let key = (index.to_path_buf(), root.to_path_buf());
    let known = baselines()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .filter(|(taken, _)| *taken == stamp)
        .map(|(_, files)| files.clone());
    let root = root.to_path_buf();
    let (then, lag) = tokio::task::spawn_blocking(move || -> Result<(Arc<Files>, u32)> {
        match known {
            Some(then) => {
                let now = source_files(&root, &then)?;
                let lag = files_differing(&then, &now);
                Ok((then, lag))
            }
            None => {
                let now = source_files(&root, &Files::new())?;
                let then = Arc::new(not_newer(&root, &now, index_modified));
                let lag = files_differing(&then, &now);
                Ok((then, lag))
            }
        }
    })
    .await
    .context("read the tree")??;
    let mut taken = baselines().lock().unwrap_or_else(PoisonError::into_inner);
    if taken.get(&key).is_none_or(|(at, _)| *at != stamp) {
        taken.insert(key, (stamp, then));
    }
    Ok(Some(lag))
}

/// The label of a map that states its freshness: `lag` is the tree's
/// difference from the index's baseline (`None` when not compared).
fn label(id: &str, lag: Option<u32>) -> Option<Freshness> {
    if SCIP_BUILT.contains(&id) {
        Some(Freshness::snapshot(lag))
    } else if LIVE.contains(&id) {
        Some(Freshness::live())
    } else {
        None
    }
}

/// What the response says of one map's observation.
fn observed_name(observed: &Observed) -> String {
    match observed {
        Observed::Snapshot => "snapshot".to_owned(),
        Observed::LiveTree => "live".to_owned(),
        Observed::AsOf(point) => format!("as_of:{point}"),
    }
}

/// The freshness of a call's maps. Off: wraps nothing and reports nothing.
pub(super) struct Observer {
    /// `None`: the key is off. `Some(None)`: on, the tree was not compared.
    lag: Option<Option<u32>>,
    seen: Arc<Mutex<HashMap<MapId, Freshness>>>,
}

impl Observer {
    pub(super) fn off() -> Self {
        Self {
            lag: None,
            seen: Arc::default(),
        }
    }

    pub(super) fn on(lag: Option<u32>) -> Self {
        Self {
            lag: Some(lag),
            seen: Arc::default(),
        }
    }

    /// `map`, wrapped when it states a freshness.
    pub(super) fn wrap<'a>(&self, map: Box<dyn Map + 'a>) -> Box<dyn Map + 'a> {
        let Some(lag) = self.lag else {
            return map;
        };
        match label(map.describe().id, lag) {
            Some(fallback) => Box::new(Observing {
                inner: map,
                fallback,
                seen: self.seen.clone(),
            }),
            None => map,
        }
    }

    /// `maps`: the freshness of each of the map `ids` (in this order) that states one,
    /// as the last expansion said it or, for a map that was not expanded, as
    /// the call knows it. `None` with the key off.
    pub(super) fn report<'i>(&self, ids: impl IntoIterator<Item = &'i str>) -> Option<Value> {
        let lag = self.lag?;
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let entries: Vec<Value> = ids
            .into_iter()
            .filter_map(|id| {
                let freshness = seen.get(id).cloned().or_else(|| label(id, lag))?;
                Some(json!({
                    "id": id,
                    "observed": observed_name(&freshness.observed),
                    "stale": freshness.stale,
                    "lag": freshness.lag,
                }))
            })
            .collect();
        Some(Value::Array(entries))
    }
}

/// A map that records the freshness of its expansions.
struct Observing<'a> {
    inner: Box<dyn Map + 'a>,
    fallback: Freshness,
    seen: Arc<Mutex<HashMap<MapId, Freshness>>>,
}

#[lci_maps::async_trait]
impl Map for Observing<'_> {
    fn describe(&self) -> &MapDescriptor {
        self.inner.describe()
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        self.inner.query(request, view).await
    }

    async fn expand(&self, node: &NodeId, budget: Budget, view: &SnapshotView<'_>) -> Expanded {
        let mut expanded = self.inner.expand(node, budget, view).await;
        let stated = expanded
            .freshness
            .get_or_insert_with(|| self.fallback.clone())
            .clone();
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.inner.describe().id, stated);
        expanded
    }

    fn card(&self) -> MapCard {
        self.inner.card()
    }
}
