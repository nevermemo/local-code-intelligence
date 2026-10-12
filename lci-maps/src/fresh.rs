//! Freshness of stored maps (contract C3; spec
//! `docs/spike/multimap/specs/contract-kit.md`, C3).
//!
//! A stored map follows the core snapshot: the engine refreshes the core
//! index under `[index].freshness` first, then brings each stored map to
//! the snapshot the search answers from. A map is never
//! refreshed from a search under `manual` or `watch`; it may be built when
//! it has no data at all, as a search may create a missing index. A failed
//! build or refresh keeps the previous data and warns; it never fails the
//! search (decision M-8).

use crate::{Map, SnapshotRef};
use lci_core::manifest::CachedFile;
use std::{collections::HashMap, path::Path};

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
