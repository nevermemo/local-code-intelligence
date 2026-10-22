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
//! label. Every call reads and hashes the tree's source files, unless
//! `[run_profile.freshness] stat_gate` is on (below). No chunk is made.
//!
//! `[run_profile.freshness] stat_gate` (slice K38), default off, makes that one
//! read cheap: the process keeps, per root, each source file's modified time,
//! length and content hash ([`chunk::StatGate`]), lists the tree without reading
//! it, and reads and hashes only the files whose modified time or length is new
//! or changed (a file without a modified time is always read). The answers are
//! the full read's, through the same hash function, except for one gap: a
//! same-length edit that keeps a modified time older than 3 seconds before the
//! previous read of the file (a deliberate `set_modified`, or a clock that went
//! back) is not seen until the file's next edit. A file modified within 3
//! seconds of a read is read again (coarse-stamp filesystems: FAT, exFAT, SMB).
//! The table is per process, rebuilt on the first call, and also
//! serves a search's `verify_on_read`. Key off: every file is read, as before.
//!
//! The maps themselves do not see the tree, so [`Observer`] wraps each map
//! that states a freshness: it sets [`Expanded::freshness`] unless the map did,
//! and keeps the last one it saw per map for the response. With the key off
//! nothing is wrapped.
//!
//! `[run_profile.freshness] verify_on_read` (slice K2b), together with
//! `map_freshness`, uses the same one read of the tree and adds two things:
//!
//! * the maps that read the index snapshot (`seeds.hybrid`, `search.lexical`,
//!   `search.vector`) are labelled with how many source files differ between
//!   the snapshot's manifest and the tree (read after the walk, because the
//!   walk's first search may refresh the snapshot; prose files of the manifest
//!   are not counted, the tree scan leaves prose out). `grep.live`
//!   reads the tree and `lsp.live` queries a language server (a fresh one reads
//!   the disk; a kept one is bounded by its tree stamp's coverage, and the
//!   walk says so in a note), and both place their answers by the SCIP index
//!   (the line of a symbol), so they say `observed: live` with the SCIP index's lag
//!   and `places: "index"`: they are stale when the index's places are;
//! * each card says `file_changed`: whether the content hash of its file now
//!   differs from the hash the card's maps read (the SCIP baseline for the
//!   maps built from the index or placed by it, and for a symbol card, whose
//!   site is the index's; the manifest for the snapshot maps and for a chunk
//!   card; `true` when either differs), `null` when no reference applies (a
//!   commit, a file that is not a source file) or no tree was read.
//!
//! It reads and labels. It never refreshes the index. Two limits: a card whose
//! file was newer than the index file when its stamp was first seen has no
//! baseline hash, so it counts as changed (`file_changed: true`) whatever its
//! content is; and the manifest is read after the walk, so a refresh of the
//! snapshot that lands between a map's read and that manifest read (a watched
//! index, `[index].freshness = "watch"`) makes the answer look as current as
//! the new snapshot.

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
    str::FromStr,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::SystemTime,
};

use super::seeds::{SeedSearch, is_source_path, snapshot_differing};

type Stamp = (Option<SystemTime>, u64);
type Files = HashMap<String, CachedFile>;

/// The maps built from the SCIP index and the tree: stale when the tree
/// differs from the baseline of the index's stamp.
const SCIP_BUILT: [MapId; 9] = [
    "graph.scip",
    "structure.contains",
    "seeds.scip_names",
    "seeds.bm25",
    "seeds.repo_map",
    "exact.closure",
    "config.key",
    "scip.impact",
    "scip.implements",
];

/// The maps that read the working tree or git when they answer, or (`lsp.live`)
/// query a language server that does.
const LIVE: [MapId; 6] = [
    "grep.live",
    "grep.structural",
    "lsp.live",
    "git.history",
    "exact.pickaxe",
    "probe.compile",
];

/// The maps that answer from the index snapshot (the stored chunks): with
/// `verify_on_read` they say how far the snapshot is behind the tree.
const SNAPSHOT_BACKED: [MapId; 3] = ["seeds.hybrid", "search.lexical", "search.vector"];

/// The live maps whose places (the line of a symbol) come from the SCIP
/// index: with `verify_on_read` they carry the index's lag.
const PLACED_BY_INDEX: [MapId; 3] = ["grep.live", "grep.structural", "lsp.live"];

/// The baselines: one per (index, tree), valid for the stamp it was taken at.
type Baselines = Mutex<HashMap<(PathBuf, PathBuf), (Stamp, Arc<Files>)>>;

fn baselines() -> &'static Baselines {
    static BASELINES: OnceLock<Baselines> = OnceLock::new();
    BASELINES.get_or_init(Default::default)
}

/// The modified time of each file of a [`Files`] (`None`: the platform gave
/// none), taken by the same listing that hashed it.
type Modified = HashMap<String, Option<SystemTime>>;

/// The files the language adapters index, prose left out, hashed without
/// parsing or keeping any chunk (one [`chunk::source_hashes`] call; with
/// `stat_gate` only the files whose modified time or length changed are read),
/// and their modified times.
fn source_hashes(root: &Path, stat_gate: bool) -> Result<(Files, Modified)> {
    let hashed = chunk::source_hashes(root, false, stat_gate)?;
    let mut files = Files::with_capacity(hashed.files.len());
    let mut modified = Modified::with_capacity(hashed.files.len());
    for file in hashed.files {
        modified.insert(file.relative.clone(), file.modified);
        files.insert(
            file.relative,
            CachedFile {
                language: file.language.into(),
                adapter_version: file.adapter_version.into(),
                content_hash: file.content_hash,
                chunks: Vec::new(),
            },
        );
    }
    Ok((files, modified))
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
fn not_newer(now: &Files, modified: &Modified, index_modified: SystemTime) -> Files {
    now.iter()
        .filter(|(path, _)| match modified.get(*path) {
            Some(Some(modified)) => *modified <= index_modified,
            _ => true,
        })
        .map(|(path, file)| (path.clone(), file.clone()))
        .collect()
}

/// How many source files differ from the index file: the files newer than it
/// and those changed since this process first saw its stamp (see the module
/// comment). The first call that sees a stamp takes the baseline. `None` when
/// the stamp has no modified time (not compared). Every call reads and hashes
/// the tree's source files; with `stat_gate` only those whose modified time or
/// length changed.
#[cfg(test)]
pub(super) async fn tree_lag(
    index: &Path,
    root: &Path,
    stamp: Stamp,
    stat_gate: bool,
) -> Result<Option<u32>> {
    Ok(tree_view(index, root, stamp, false, stat_gate)
        .await?
        .and_then(|view| view.lag))
}

/// What one read of the tree gave: the lag against the index's baseline and
/// the file sets it was computed from, which `verify_on_read` reuses (no second
/// scan).
pub(super) struct TreeView {
    /// The lag [`tree_lag`] reports; `None` when the stamp has no modified
    /// time (not compared with the index file).
    pub(super) lag: Option<u32>,
    /// The tree's source files now (path and content hash).
    now: Arc<Files>,
    /// The index's baseline for this stamp; `None` when there is no stamp.
    baseline: Option<Arc<Files>>,
}

/// [`tree_lag`] with the sets behind it. `want_now`: read the tree even when
/// the stamp has no modified time (the snapshot check needs `now`, the lag is
/// then `None`); without it such a call reads nothing and gives `None`.
/// `stat_gate`: `[run_profile.freshness] stat_gate`, whether the read keeps
/// each file's `(modified time, length)` and hash in the process and reads only
/// the files that changed (see the module comment for its gap).
pub(super) async fn tree_view(
    index: &Path,
    root: &Path,
    stamp: Stamp,
    want_now: bool,
    stat_gate: bool,
) -> Result<Option<TreeView>> {
    let Some(index_modified) = stamp.0 else {
        if !want_now {
            return Ok(None);
        }
        let root = root.to_path_buf();
        let (now, _) = tokio::task::spawn_blocking(move || source_hashes(&root, stat_gate))
            .await
            .context("read the tree")??;
        return Ok(Some(TreeView {
            lag: None,
            now: Arc::new(now),
            baseline: None,
        }));
    };
    let key = (index.to_path_buf(), root.to_path_buf());
    let known = baselines()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .filter(|(taken, _)| *taken == stamp)
        .map(|(_, files)| files.clone());
    let root = root.to_path_buf();
    let (then, now, lag) =
        tokio::task::spawn_blocking(move || -> Result<(Arc<Files>, Files, u32)> {
            let (now, modified) = source_hashes(&root, stat_gate)?;
            match known {
                Some(then) => {
                    let lag = files_differing(&then, &now);
                    Ok((then, now, lag))
                }
                None => {
                    let then = Arc::new(not_newer(&now, &modified, index_modified));
                    let lag = files_differing(&then, &now);
                    Ok((then, now, lag))
                }
            }
        })
        .await
        .context("read the tree")??;
    let mut taken = baselines().lock().unwrap_or_else(PoisonError::into_inner);
    if taken.get(&key).is_none_or(|(at, _)| *at != stamp) {
        taken.insert(key, (stamp, then.clone()));
    }
    Ok(Some(TreeView {
        lag: Some(lag),
        now: Arc::new(now),
        baseline: Some(then),
    }))
}

/// What the manifest of the index snapshot said, read after the walk.
struct Manifest {
    hashes: HashMap<String, String>,
    /// The files that differ from the tree; `None` when no tree was read.
    lag: Option<u32>,
}

/// `verify_on_read` state of one call.
struct Verify {
    /// The one read of the tree; `None` when it could not be made.
    view: Option<TreeView>,
    /// Set once, after the walk. `Some(None)`: the search has no manifest.
    manifest: OnceLock<Option<Manifest>>,
}

impl Verify {
    /// The snapshot's lag, `None` when it was not compared.
    fn snapshot_lag(&self) -> Option<u32> {
        self.manifest.get()?.as_ref()?.lag
    }

    /// Whether the file now differs from the hash the baseline has, `None`
    /// when there is no baseline.
    fn against_baseline(&self, path: &str) -> Option<bool> {
        let view = self.view.as_ref()?;
        let then = view.baseline.as_ref()?;
        match (then.get(path), view.now.get(path)) {
            (Some(then), Some(now)) => Some(then.content_hash != now.content_hash),
            // Added, or newer than the index (counted from the first call), or
            // left the source set: it differs from what the index saw.
            (None, Some(_)) | (Some(_), None) => Some(true),
            (None, None) => None,
        }
    }

    /// The same against the snapshot's manifest.
    fn against_manifest(&self, path: &str) -> Option<bool> {
        let view = self.view.as_ref()?;
        let manifest = self.manifest.get()?.as_ref()?;
        match (manifest.hashes.get(path), view.now.get(path)) {
            (Some(then), Some(now)) => Some(*then != now.content_hash),
            (None, Some(_)) => Some(true),
            // A manifest entry the tree scan does not have: removed when it is
            // a source path; a prose file is not compared.
            (Some(_), None) => is_source_path(path).then_some(true),
            (None, None) => None,
        }
    }
}

/// The label of a map that states its freshness: `lag` is the tree's
/// difference from the index's baseline (`None` when not compared).
fn label(id: &str, lag: Option<u32>, verify: Option<&Verify>) -> Option<Freshness> {
    if SCIP_BUILT.contains(&id) {
        Some(Freshness::snapshot(lag))
    } else if verify.is_some() && PLACED_BY_INDEX.contains(&id) {
        // The tree is read, but the places are the index's: stale when the
        // index is.
        Some(Freshness {
            observed: Observed::LiveTree,
            stale: matches!(lag, Some(1..)),
            lag,
        })
    } else if LIVE.contains(&id) {
        Some(Freshness::live())
    } else if let (true, Some(verify)) = (SNAPSHOT_BACKED.contains(&id), verify) {
        Some(Freshness::snapshot(verify.snapshot_lag()))
    } else {
        None
    }
}

/// The maps a card names: those whose edges reached it and the map that made
/// it (`via`, which is what a start node has).
fn card_maps(card: &Value) -> Vec<&str> {
    let mut maps: Vec<&str> = card["maps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    maps.extend(card["via"].as_str());
    maps
}

/// The file a card cites: its site, else the path of its node.
fn card_path(card: &Value) -> Option<String> {
    if let Some(path) = card["site"]["path"].as_str() {
        return Some(path.to_owned());
    }
    let node = NodeId::from_str(card["node"].as_str()?).ok()?;
    node.path().map(|path| path.replace('\\', "/"))
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
    /// `[run_profile.freshness] verify_on_read`; `None` when it is off.
    verify: Option<Verify>,
    seen: Arc<Mutex<HashMap<MapId, Freshness>>>,
}

impl Observer {
    pub(super) fn off() -> Self {
        Self {
            lag: None,
            verify: None,
            seen: Arc::default(),
        }
    }

    pub(super) fn on(lag: Option<u32>) -> Self {
        Self {
            lag: Some(lag),
            verify: None,
            seen: Arc::default(),
        }
    }

    /// With `verify_on_read` on: `view` is the read of the tree that `lag` came
    /// from (`None` when none could be made).
    pub(super) fn verifying(mut self, view: Option<TreeView>) -> Self {
        self.verify = Some(Verify {
            view,
            manifest: OnceLock::new(),
        });
        self
    }

    /// Whether `verify_on_read` is on for this call.
    pub(super) fn verifies(&self) -> bool {
        self.lag.is_some() && self.verify.is_some()
    }

    /// Reads the index snapshot's manifest, for the maps that answer from it.
    /// Called after the walk: the walk's first search may have refreshed the
    /// snapshot, and the tree does not change inside a call. Nothing here
    /// refreshes anything.
    pub(super) async fn settle(&self, search: Option<&dyn SeedSearch>) {
        let Some(verify) = &self.verify else {
            return;
        };
        let hashes = match search {
            Some(search) => search.snapshot_hashes().await,
            None => None,
        };
        let manifest = hashes.map(|hashes| {
            let lag = verify.view.as_ref().map(|view| {
                let now: HashMap<&str, &str> = view
                    .now
                    .iter()
                    .map(|(path, file)| (path.as_str(), file.content_hash.as_str()))
                    .collect();
                snapshot_differing(&hashes, &now)
            });
            Manifest { hashes, lag }
        });
        let _ = verify.manifest.set(manifest);
    }

    /// `map`, wrapped when it states a freshness. The snapshot-backed maps are
    /// not wrapped under `verify_on_read`: their lag is known after the walk.
    pub(super) fn wrap<'a>(&self, map: Box<dyn Map + 'a>) -> Box<dyn Map + 'a> {
        let Some(lag) = self.lag else {
            return map;
        };
        let id = map.describe().id;
        if self.verify.is_some() && SNAPSHOT_BACKED.contains(&id) {
            return map;
        }
        match label(id, lag, self.verify.as_ref()) {
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
    /// the call knows it. `None` with the key off. With `verify_on_read` the
    /// live maps that place by the index say `places: "index"`.
    pub(super) fn report<'i>(&self, ids: impl IntoIterator<Item = &'i str>) -> Option<Value> {
        let lag = self.lag?;
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let entries: Vec<Value> = ids
            .into_iter()
            .filter_map(|id| {
                let freshness = seen
                    .get(id)
                    .cloned()
                    .or_else(|| label(id, lag, self.verify.as_ref()))?;
                let mut entry = json!({
                    "id": id,
                    "observed": observed_name(&freshness.observed),
                    "stale": freshness.stale,
                    "lag": freshness.lag,
                });
                if self.verify.is_some() && PLACED_BY_INDEX.contains(&id) {
                    entry["places"] = json!("index");
                }
                Some(entry)
            })
            .collect();
        Some(Value::Array(entries))
    }

    /// Whether the file `card` cites differs now from what its maps read:
    /// `Some(true)` or `Some(false)`, `None` when no reference applies (see the
    /// module comment). `None` too with the key off.
    pub(super) fn file_changed(&self, card: &Value) -> Option<bool> {
        let verify = self.verify.as_ref()?;
        let maps = card_maps(card);
        let path = card_path(card)?;
        // A symbol card is placed by the SCIP index whichever map reached it, a
        // chunk card by the snapshot (a start node names no map).
        let node = card["node"]
            .as_str()
            .and_then(|node| NodeId::from_str(node).ok());
        let by_index = matches!(node, Some(NodeId::Symbol(_)))
            || maps
                .iter()
                .any(|id| SCIP_BUILT.contains(id) || PLACED_BY_INDEX.contains(id));
        let by_snapshot = matches!(node, Some(NodeId::Chunk { .. }))
            || maps.iter().any(|id| SNAPSHOT_BACKED.contains(id));
        let verdicts = [
            by_index.then(|| verify.against_baseline(&path)).flatten(),
            by_snapshot
                .then(|| verify.against_manifest(&path))
                .flatten(),
        ];
        if verdicts.contains(&Some(true)) {
            Some(true)
        } else if verdicts.contains(&Some(false)) {
            Some(false)
        } else {
            None
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::traverse::seeds::SeedHit;

    fn file(hash: &str) -> CachedFile {
        CachedFile {
            language: "rust".into(),
            adapter_version: "0".into(),
            content_hash: hash.into(),
            chunks: Vec::new(),
        }
    }

    fn files(entries: &[(&str, &str)]) -> Files {
        entries
            .iter()
            .map(|(path, hash)| ((*path).to_owned(), file(hash)))
            .collect()
    }

    fn manifest(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(path, hash)| ((*path).to_owned(), (*hash).to_owned()))
            .collect()
    }

    /// A search with a manifest and nothing else.
    struct Manifested(Option<HashMap<String, String>>);

    #[lci_maps::async_trait]
    impl SeedSearch for Manifested {
        async fn search(&self, _question: &str, _k: usize) -> Result<Vec<SeedHit>> {
            Ok(Vec::new())
        }

        async fn snapshot_hashes(&self) -> Option<HashMap<String, String>> {
            self.0.clone()
        }
    }

    #[test]
    fn the_seed_maps_built_from_the_graph_state_a_snapshot_freshness() {
        // `seeds.repo_map` reads the same graph as `seeds.bm25`: stale when the
        // tree differs from the index's baseline, and not a live map.
        for id in ["seeds.bm25", "seeds.repo_map"] {
            assert!(SCIP_BUILT.contains(&id) && !LIVE.contains(&id), "{id}");
            assert_eq!(label(id, Some(0), None), Some(Freshness::snapshot(Some(0))));
            assert_eq!(label(id, Some(2), None), Some(Freshness::snapshot(Some(2))));
        }
    }

    #[test]
    fn grep_structural_states_a_live_freshness_like_grep_live() {
        for id in ["grep.live", "grep.structural"] {
            assert_eq!(label(id, Some(0), None), Some(Freshness::live()), "{id}");
            assert_eq!(label(id, None, None), Some(Freshness::live()), "{id}");
        }
        assert!(PLACED_BY_INDEX.contains(&"grep.structural"));
    }

    #[test]
    fn the_snapshot_lag_counts_source_files_only() {
        let now = files(&[("a.rs", "2"), ("b.rs", "1"), ("c.rs", "1")]);
        // a.rs changed, c.rs is not in the manifest, gone.rs was removed; the
        // README (prose) the tree scan does not list is not a difference.
        let snapshot = manifest(&[
            ("a.rs", "1"),
            ("b.rs", "1"),
            ("gone.rs", "1"),
            ("README.md", "1"),
        ]);
        let tree = |files: &Files| -> Vec<(String, String)> {
            files
                .iter()
                .map(|(path, file)| (path.clone(), file.content_hash.clone()))
                .collect()
        };
        let now_pairs = tree(&now);
        let now_refs: HashMap<&str, &str> = now_pairs
            .iter()
            .map(|(path, hash)| (path.as_str(), hash.as_str()))
            .collect();
        assert_eq!(snapshot_differing(&snapshot, &now_refs), 3);
        let same = manifest(&[("a.rs", "2"), ("b.rs", "1"), ("c.rs", "1")]);
        assert_eq!(snapshot_differing(&same, &now_refs), 0);
    }

    fn view(now: &[(&str, &str)], baseline: Option<&[(&str, &str)]>) -> TreeView {
        TreeView {
            lag: baseline.map(|then| files_differing(&files(then), &files(now))),
            now: Arc::new(files(now)),
            baseline: baseline.map(|then| Arc::new(files(then))),
        }
    }

    fn card(node: &str, maps: &[&str], path: Option<&str>) -> Value {
        let mut card = json!({"node": node, "maps": maps});
        if let Some(path) = path {
            card["site"] = json!({"path": path, "line": 1, "col": 1});
        }
        card
    }

    #[tokio::test]
    async fn a_card_changed_when_the_reference_of_its_maps_differs() {
        // The tree now; the SCIP index saw a.rs as 1 and b.rs as 1 (c.rs is new);
        // the snapshot saw a.rs as 2, b.rs as 0 (c.rs is not in it).
        let now = [("a.rs", "2"), ("b.rs", "1"), ("c.rs", "1")];
        let observer = Observer::on(Some(2))
            .verifying(Some(view(&now, Some(&[("a.rs", "1"), ("b.rs", "1")]))));
        let search = Manifested(Some(manifest(&[("a.rs", "2"), ("b.rs", "0")])));
        observer.settle(Some(&search)).await;
        let said =
            |maps: &[&str], path: &str| observer.file_changed(&card("file:x", maps, Some(path)));
        // The maps built from the index, and the live maps that place by it:
        // the index's baseline.
        assert_eq!(said(&["graph.scip"], "a.rs"), Some(true));
        assert_eq!(said(&["graph.scip"], "b.rs"), Some(false));
        assert_eq!(said(&["grep.live"], "b.rs"), Some(false));
        assert_eq!(
            said(&["lsp.live"], "c.rs"),
            Some(true),
            "newer than the index"
        );
        // The snapshot maps: the manifest.
        assert_eq!(said(&["search.lexical"], "a.rs"), Some(false));
        assert_eq!(said(&["search.vector"], "b.rs"), Some(true));
        assert_eq!(
            said(&["seeds.hybrid"], "c.rs"),
            Some(true),
            "not indexed yet"
        );
        // Both: either difference counts.
        assert_eq!(said(&["graph.scip", "search.lexical"], "b.rs"), Some(true));
        assert_eq!(said(&["graph.scip", "search.lexical"], "a.rs"), Some(true));
        // No reference: a map that reads history, a file that is not a source.
        assert_eq!(said(&["git.history"], "a.rs"), None);
        assert_eq!(said(&["graph.scip"], "README.md"), None);
        assert_eq!(
            observer.file_changed(&card("commit:abc", &["git.history"], None)),
            None
        );
        // A start node names its map in `via`.
        let seed = json!({"node": "file:b.rs", "maps": [], "via": "seeds.scip_names"});
        assert_eq!(observer.file_changed(&seed), Some(false));
        // A symbol start node names no map; its site is the index's.
        assert_eq!(
            observer.file_changed(&card("sym:rust:a.rs::f/fn", &[], Some("a.rs"))),
            Some(true)
        );
        // A card with no site is located by its node.
        assert_eq!(
            observer.file_changed(&card("file:a.rs", &["structure.contains"], None)),
            Some(true)
        );
        // The snapshot is two files behind the tree: b.rs differs, c.rs is
        // missing.
        assert_eq!(observer.verify.as_ref().unwrap().snapshot_lag(), Some(2));
    }

    #[tokio::test]
    async fn without_a_tree_or_a_manifest_the_snapshot_lag_and_the_cards_are_not_compared() {
        // No tree could be read: nothing is compared, the labels say null.
        let observer = Observer::on(None).verifying(None);
        observer
            .settle(Some(&Manifested(Some(manifest(&[("a.rs", "1")])))))
            .await;
        assert_eq!(observer.verify.as_ref().unwrap().snapshot_lag(), None);
        assert_eq!(
            observer.file_changed(&card("file:a.rs", &["graph.scip"], Some("a.rs"))),
            None
        );
        let report = observer.report(["seeds.hybrid", "grep.live"]).unwrap();
        assert_eq!(
            report[0],
            json!({"id": "seeds.hybrid", "observed": "snapshot", "stale": false, "lag": null})
        );
        assert_eq!(
            report[1],
            json!({"id": "grep.live", "observed": "live", "stale": false, "lag": null, "places": "index"})
        );
        // A search with no manifest (or none given): the tree is read, the
        // snapshot is not compared.
        let now = [("a.rs", "1")];
        let observer = Observer::on(Some(0)).verifying(Some(view(&now, Some(&now))));
        observer.settle(Some(&Manifested(None))).await;
        assert_eq!(observer.verify.as_ref().unwrap().snapshot_lag(), None);
        let observer = Observer::on(Some(0)).verifying(Some(view(&now, Some(&now))));
        observer.settle(None).await;
        assert_eq!(observer.verify.as_ref().unwrap().snapshot_lag(), None);
    }

    #[tokio::test]
    async fn without_verify_on_read_the_labels_are_as_before_and_nothing_is_added() {
        let observer = Observer::on(Some(2));
        assert!(!observer.verifies());
        observer
            .settle(Some(&Manifested(Some(manifest(&[("a.rs", "1")])))))
            .await;
        let report = observer
            .report(["seeds.hybrid", "search.lexical", "grep.live", "graph.scip"])
            .unwrap();
        // The snapshot maps state nothing; grep.live says live, with the four
        // fields and no `places`.
        assert_eq!(
            report,
            json!([
                {"id": "grep.live", "observed": "live", "stale": false, "lag": 0},
                {"id": "graph.scip", "observed": "snapshot", "stale": true, "lag": 2},
            ])
        );
        assert_eq!(
            observer.file_changed(&card("file:a.rs", &["graph.scip"], Some("a.rs"))),
            None
        );
    }
}
