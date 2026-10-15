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

use crate::{
    Budget, SnapshotRef,
    fresh::Target,
    kit::ContractFailure,
    scratch::Scratch,
    symdefs::SymDefs,
    traverse::source::{EdgeSource, Expansion, FusedEdge},
};
use lci_core::{
    bundle::{BundleStore, Declarations, Producer, StoredDeclaration},
    chunk::scan_incremental,
    manifest::CachedFile,
    node::NodeId,
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
pub fn first_difference(incremental: &[String], full: &[String]) -> Option<String> {
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

/// One fused edge in canonical text form: every field a traversal reads
/// (ends, kind, fused confidence, provenance, the best edge with its site),
/// no cost (a measurement, not part of the answer).
pub fn canonical_fused(fused: &FusedEdge) -> String {
    let best = &fused.best;
    format!(
        "{} -{}-> {} conf={} maps={:?} witnesses={:?} offers={:?} best={}/{:?}/{} site={:?}",
        best.src,
        best.kind.as_str(),
        best.dst,
        fused.conf,
        fused.maps,
        fused.witnesses,
        fused.offers,
        best.map,
        best.witness,
        best.conf,
        best.site,
    )
}

/// What an expansion says, in canonical form and in the source's order: its
/// edges, then the calls it reports (map, edge count, whether it failed, and
/// the tokens it is charged).
fn canonical_expansion(expansion: &Expansion) -> Vec<String> {
    expansion
        .edges
        .iter()
        .map(|fused| format!("edge {}", canonical_fused(fused)))
        .chain(expansion.calls.iter().map(|call| {
            format!(
                "call {} n={} failed={} tokens={}",
                call.map,
                call.n_edges,
                call.failure.is_some(),
                call.cost.est_tokens
            )
        }))
        .collect()
}

/// What an equivalence audit of two edge sources compared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceReport {
    pub nodes: usize,
    /// Fused edges compared, per source.
    pub edges: usize,
}

/// Contract S5, the equivalence auditor: for every node of `nodes`, the two
/// sources must give the same edges (as a set), in the same order, and report
/// the same calls with the same token cost. The time a call took is a
/// measurement and is left out, and so are its external calls: a map that
/// caches (`git.history` runs git once more on a cold cache) reports a count
/// that depends on when it was asked, and a pre-merged row keeps the cold
/// one. Used for the pre-merged source against the
/// on-the-fly one on fixtures (spine brief 1.3); the first difference fails.
/// Whatever switches the caller wants audited are applied to both sources
/// before the call.
pub async fn audit_edge_sources(
    left: &dyn EdgeSource,
    right: &dyn EdgeSource,
    nodes: &[NodeId],
    per_node: Duration,
) -> Result<SourceReport, ContractFailure> {
    let broke = |detail: String| ContractFailure {
        contract: "S5",
        detail,
    };
    let budget = || Budget {
        deadline: Instant::now() + per_node,
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    };
    let mut report = SourceReport::default();
    for node in nodes {
        let left = left.edges(node, budget()).await;
        let right = right.edges(node, budget()).await;
        let (a, b) = (canonical_expansion(&left), canonical_expansion(&right));
        let (mut sorted_a, mut sorted_b) = (a.clone(), b.clone());
        sorted_a.sort();
        sorted_b.sort();
        if let Some(difference) = first_difference(&sorted_a, &sorted_b) {
            return Err(broke(format!(
                "node {node}: the sources differ at {difference} (left, right)"
            )));
        }
        if let Some(at) = a.iter().zip(&b).position(|(x, y)| x != y) {
            return Err(broke(format!(
                "node {node}: same entries, different order from position {at}: {:?} vs {:?}",
                a[at], b[at]
            )));
        }
        report.nodes += 1;
        report.edges += left.edges.len();
    }
    Ok(report)
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
