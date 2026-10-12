//! Contract C4, the full-rebuild equivalence auditor (spec
//! `docs/spike/multimap/specs/contract-kit.md`, C4).
//!
//! The auditor walks a sequence of tree states. At each one it updates the
//! subject incrementally from its own previous state and builds it again
//! from nothing, and compares the two on a canonical serialisation; the
//! first difference fails the contract with the first differing entry. A
//! step may also bump the adapter version: the subject must then treat its
//! stored state as produced by the previous adapter and not reuse it.
//!
//! The sequence comes from the caller: a small synthetic edit script in the
//! unit suite, a replay of this repository's commits in the slow suite
//! (`tests.rs`, `c4_commit_replay`).

use crate::{SnapshotRef, fresh::Target, kit::ContractFailure, scratch::Scratch, symdefs::SymDefs};
use lci_core::{
    bundle::{BundleStore, Declarations, Producer, StoredDeclaration},
    chunk::scan_incremental,
    manifest::CachedFile,
};
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

/// A stored map under audit.
pub trait Audited {
    /// Updates the subject incrementally to the tree at `root`, from its
    /// own previous state, and returns its data in canonical form (sorted).
    fn incremental(&mut self, root: &Path) -> Result<Vec<String>, String>;

    /// Builds the subject from nothing for the tree at `root` and returns
    /// its data in canonical form (sorted).
    fn full(&self, root: &Path) -> Result<Vec<String>, String>;

    /// From now on the stored state counts as produced by the previous
    /// adapter version: an incremental update must not reuse it.
    fn bump_adapter(&mut self) -> Result<(), String>;
}

/// One state of the audited sequence.
#[derive(Debug, Clone)]
pub struct Step {
    pub label: String,
    /// Bump the adapter version before this step's update.
    pub bump_adapter: bool,
}

/// What one audited step compared.
#[derive(Debug, Clone)]
pub struct StepReport {
    pub label: String,
    pub entries: usize,
    pub elapsed: Duration,
}

/// Contract C4 over `steps`: `apply(i)` puts the tree at `root` into state
/// `i`, then the incremental and full data must be equal. Returns what each
/// step compared, or the first difference.
pub fn audit(
    subject: &mut dyn Audited,
    root: &Path,
    steps: &[Step],
    mut apply: impl FnMut(usize) -> Result<(), String>,
) -> Result<Vec<StepReport>, ContractFailure> {
    let broke = |detail: String| ContractFailure {
        contract: "C4",
        detail,
    };
    let mut report = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let started = Instant::now();
        apply(index).map_err(|error| ContractFailure {
            contract: "kit",
            detail: format!("step {}: could not apply: {error}", step.label),
        })?;
        if step.bump_adapter {
            subject
                .bump_adapter()
                .map_err(|error| broke(format!("step {}: adapter bump: {error}", step.label)))?;
        }
        let incremental = subject.incremental(root).map_err(|error| {
            broke(format!(
                "step {}: incremental update failed: {error}",
                step.label
            ))
        })?;
        let full = subject
            .full(root)
            .map_err(|error| broke(format!("step {}: full build failed: {error}", step.label)))?;
        if let Some(difference) = first_difference(&incremental, &full) {
            return Err(broke(format!(
                "step {}: incremental differs from full at {difference}",
                step.label
            )));
        }
        report.push(StepReport {
            label: step.label.clone(),
            entries: full.len(),
            elapsed: started.elapsed(),
        });
    }
    Ok(report)
}

/// The first entry in one sorted list and not the other, described.
fn first_difference(incremental: &[String], full: &[String]) -> Option<String> {
    let (mut i, mut f) = (0, 0);
    loop {
        match (incremental.get(i), full.get(f)) {
            (None, None) => return None,
            (Some(a), Some(b)) if a == b => {
                i += 1;
                f += 1;
            }
            (Some(a), Some(b)) if a < b => return Some(format!("{a:?} (incremental only)")),
            (Some(_), Some(b)) | (None, Some(b)) => return Some(format!("{b:?} (full only)")),
            (Some(a), None) => return Some(format!("{a:?} (incremental only)")),
        }
    }
}

/// The suffix the C4 adapter bump gives the stored state's adapter version.
pub const PRIOR: &str = "~prior";

/// `sym.defs` under audit: its incremental state is the previous scan's
/// files (the manifest) and the bundle store; a full build starts from an
/// empty manifest and an empty store in a scratch directory.
pub struct SymDefsAudit<P: Producer<Payload = Vec<StoredDeclaration>> + Clone = Declarations> {
    producer: P,
    data: Scratch,
    store: BundleStore,
    files: HashMap<String, CachedFile>,
    steps: u64,
    /// (reused, extracted) bundles of every incremental update.
    pub syncs: Vec<(usize, usize)>,
}

const WORKSPACE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

impl SymDefsAudit<Declarations> {
    pub fn new() -> std::io::Result<Self> {
        Self::with_producer(Declarations)
    }
}

impl<P: Producer<Payload = Vec<StoredDeclaration>> + Clone> SymDefsAudit<P> {
    pub fn with_producer(producer: P) -> std::io::Result<Self> {
        let data = Scratch::new("c4")?;
        let store = BundleStore::new(data.path(), WORKSPACE);
        Ok(Self {
            producer,
            data,
            store,
            files: HashMap::new(),
            steps: 0,
            syncs: Vec::new(),
        })
    }

    /// The store the incremental side keeps its bundles in.
    pub fn store(&self) -> &BundleStore {
        &self.store
    }

    fn build(
        producer: P,
        store: BundleStore,
        root: &Path,
        files: &HashMap<String, CachedFile>,
        snapshot: SnapshotRef,
    ) -> Result<(Vec<String>, (usize, usize)), String> {
        let mut map = SymDefs::with_producer(store, producer);
        map.build(&Target {
            snapshot: &snapshot,
            root,
            files,
        })?;
        let data = map.data().expect("built");
        Ok((data.canonical(), map.last_sync.expect("built")))
    }
}

impl<P: Producer<Payload = Vec<StoredDeclaration>> + Clone> Audited for SymDefsAudit<P> {
    fn incremental(&mut self, root: &Path) -> Result<Vec<String>, String> {
        let scan = scan_incremental(root, &self.files).map_err(|error| format!("{error:#}"))?;
        self.steps += 1;
        let snapshot = SnapshotRef {
            indexed_at: format!("step {}", self.steps),
            fingerprint: scan.fingerprint.clone(),
        };
        let (canonical, sync) = Self::build(
            self.producer.clone(),
            self.store.clone(),
            root,
            &scan.files,
            snapshot,
        )?;
        self.files = scan.files;
        self.syncs.push(sync);
        Ok(canonical)
    }

    fn full(&self, root: &Path) -> Result<Vec<String>, String> {
        let scratch = Scratch::new("c4-full").map_err(|error| error.to_string())?;
        let scan = scan_incremental(root, &HashMap::new()).map_err(|error| format!("{error:#}"))?;
        let snapshot = SnapshotRef {
            indexed_at: "full".into(),
            fingerprint: scan.fingerprint.clone(),
        };
        let store = BundleStore::new(scratch.path(), WORKSPACE);
        Self::build(self.producer.clone(), store, root, &scan.files, snapshot)
            .map(|(canonical, _)| canonical)
    }

    /// Relabels the manifest and the bundles as produced by the previous
    /// adapter (`<version>~prior`) and makes that state detectably
    /// different from what the current adapter produces: each file with
    /// more than one chunk loses its last one, each stored declaration's
    /// name gets the suffix `_prior`, and the bundle under the current key
    /// is removed. A subject that reused any of it would differ from the
    /// full build. (Today's scan re-parses any file whose adapter version
    /// differs, so the chunk perturbation guards a future scan, not this
    /// one; the bundle side is what a key without the adapter version
    /// trips on.)
    fn bump_adapter(&mut self) -> Result<(), String> {
        for file in self.files.values_mut() {
            let current = self.producer.key(file);
            file.adapter_version.push_str(PRIOR);
            if file.chunks.len() > 1 {
                file.chunks.pop();
            }
            if !self.producer.applies(&file.language) {
                continue;
            }
            let prior = self.producer.key(file);
            let Some(mut stored) = self
                .store
                .get::<Vec<StoredDeclaration>>(&current)
                .map_err(|error| format!("{error:#}"))?
            else {
                continue;
            };
            for declaration in &mut stored {
                declaration.name.push_str("_prior");
            }
            if prior != current {
                let _ = std::fs::remove_file(self.store.path_of(&current));
            }
            self.store
                .put(&prior, &stored)
                .map_err(|error| format!("{error:#}"))?;
        }
        Ok(())
    }
}

impl<P: Producer<Payload = Vec<StoredDeclaration>> + Clone> std::fmt::Debug for SymDefsAudit<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymDefsAudit")
            .field("producer", &self.producer.id())
            .field("data", &self.data)
            .field("files", &self.files.len())
            .finish()
    }
}

#[cfg(test)]
mod tests;
