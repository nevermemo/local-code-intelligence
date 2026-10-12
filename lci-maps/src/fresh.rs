//! Freshness of stored maps (contract C3; spec
//! `docs/spike/multimap/specs/contract-kit.md`, C3).
//!
//! A stored map follows the core snapshot: the engine refreshes the core
//! index under `[index].freshness` first, then brings each stored map to
//! the snapshot the search answers from with [`prepare`]. A map is never
//! refreshed from a search under `manual` or `watch`; it may be built when
//! it has no data at all, as a search may create a missing index. A failed
//! build or refresh keeps the previous data and warns; it never fails the
//! search (decision M-8).

use crate::{Map, SnapshotRef};
pub use lci_core::config::IndexFreshness;
use lci_core::manifest::CachedFile;
use std::{collections::HashMap, panic::AssertUnwindSafe, path::Path};

/// The snapshot a stored map is brought to: the core snapshot's identity,
/// the workspace root and the snapshot's files (the manifest's).
#[derive(Debug, Clone, Copy)]
pub struct Target<'t> {
    pub snapshot: &'t SnapshotRef,
    pub root: &'t Path,
    pub files: &'t HashMap<String, CachedFile>,
}

/// A map with stored data that the engine refreshes.
pub trait Refreshable: Map {
    /// The snapshot its data was built for; `None` before the first build.
    fn data_snapshot(&self) -> Option<&SnapshotRef>;

    /// Brings the data to `target`, incrementally where it can. On an
    /// error (its text is returned) the previous data must stay as it was.
    fn refresh(&mut self, target: &Target<'_>) -> Result<(), String>;
}

/// What [`prepare`] did to a stored map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    /// Its data was already for the target snapshot.
    Current,
    /// It had no data and was built (any mode).
    Built,
    /// Its data was for another snapshot and was refreshed (`on-search`).
    Refreshed,
    /// Its data is for another snapshot and the mode forbids a refresh
    /// from a search (`manual`, `watch`): it answers from that data and
    /// says it is stale.
    Stale,
    /// The build or refresh failed (an error or a panic); the previous data,
    /// if any, is kept. Reported as a warning naming the map.
    Failed { warning: String },
}

/// Brings `map` to `target` as `mode` allows (contract C3), for a search.
/// The explicit index command and the watcher refresh stored maps through
/// their own path, which is S4's (decision K-7).
pub fn prepare(map: &mut dyn Refreshable, mode: IndexFreshness, target: &Target<'_>) -> Prepared {
    let done = match map.data_snapshot() {
        Some(snapshot) if snapshot == target.snapshot => return Prepared::Current,
        None => Prepared::Built,
        Some(_) if mode == IndexFreshness::OnSearch => Prepared::Refreshed,
        Some(_) => return Prepared::Stale,
    };
    let id = map.describe().id;
    let kept = if done == Prepared::Built {
        "it has no data yet"
    } else {
        "it answers from its previous data"
    };
    match std::panic::catch_unwind(AssertUnwindSafe(|| map.refresh(target))) {
        Ok(Ok(())) => done,
        Ok(Err(error)) => Prepared::Failed {
            warning: format!("map {id} could not be refreshed: {error}; {kept}"),
        },
        Err(_) => Prepared::Failed {
            warning: format!("map {id} panicked while refreshing; {kept}"),
        },
    }
}
