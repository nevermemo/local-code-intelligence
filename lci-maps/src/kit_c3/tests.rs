//! The C3 check passes `sym.defs` and a live map, and rejects one
//! known-bad map per rule it enforces.

use super::*;
use crate::{
    Contribution, MapDescriptor, NodeKind, SourceLabel,
    kit::FixedMap,
    scratch::Scratch,
    symdefs::{self, SymDefs},
};
use lci_core::{
    bundle::BundleStore,
    chunk::{Scan, scan_incremental},
};
use std::{collections::HashMap, path::Path};

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// One tree state: its own directory, scan and snapshot.
struct Tree {
    root: Scratch,
    scan: Scan,
    snapshot: SnapshotRef,
}

impl Tree {
    fn new(indexed_at: &str, files: &[(&str, &str)]) -> Self {
        let root = Scratch::new("c3-tree").unwrap();
        for (name, content) in files {
            write(root.path(), name, content);
        }
        let scan = scan_incremental(root.path(), &HashMap::new()).unwrap();
        let snapshot = SnapshotRef {
            indexed_at: indexed_at.into(),
            fingerprint: scan.fingerprint.clone(),
        };
        Self {
            root,
            scan,
            snapshot,
        }
    }

    fn state(&self) -> State<'_> {
        State {
            target: Target {
                snapshot: &self.snapshot,
                root: self.root.path(),
                files: &self.scan.files,
            },
            chunks: &self.scan.chunks,
        }
    }
}

struct Fixture {
    old: Tree,
    new: Tree,
    data: Scratch,
    missing: Scratch,
    broken_snapshot: SnapshotRef,
}

impl Fixture {
    fn new() -> Self {
        let old = Tree::new(
            "t1",
            &[
                ("src/engine.rs", "pub fn alpha() -> u8 { 1 }\n"),
                ("src/other.rs", "pub struct Beta;\n"),
            ],
        );
        let new = Tree::new(
            "t2",
            &[
                (
                    "src/engine.rs",
                    "pub fn alpha() -> u8 { 1 }\n\npub fn beta() -> u8 { 2 }\n",
                ),
                ("src/other.rs", "pub struct Beta;\n"),
            ],
        );
        Self {
            old,
            new,
            data: Scratch::new("c3-data").unwrap(),
            // Exists, but holds none of the files: a refresh cannot read them.
            missing: Scratch::new("c3-missing").unwrap(),
            broken_snapshot: SnapshotRef {
                indexed_at: "t2-broken".into(),
                fingerprint: "unreadable".into(),
            },
        }
    }

    fn store(&self) -> BundleStore {
        BundleStore::new(self.data.path(), &"ab".repeat(32))
    }

    fn case(&self) -> FreshnessCase<'_> {
        FreshnessCase {
            old: self.old.state(),
            new: self.new.state(),
            broken: Target {
                snapshot: &self.broken_snapshot,
                root: self.missing.path(),
                files: &self.new.scan.files,
            },
            text: "alpha beta",
            budget: Duration::from_secs(5),
        }
    }
}

#[tokio::test]
async fn sym_defs_passes_c3() {
    let fixture = Fixture::new();
    let subject = || -> Box<dyn Refreshable> { Box::new(SymDefs::new(fixture.store())) };
    let bumped =
        || -> Box<dyn Refreshable> { Box::new(SymDefs::new(fixture.store()).with_version(2)) };
    check_freshness(&subject, &bumped, &fixture.case())
        .await
        .unwrap();
}

/// Wraps `sym.defs` and breaks one C3 rule.
struct Broken {
    inner: SymDefs,
    store: BundleStore,
    flaw: Flaw,
}

#[derive(Clone, Copy, PartialEq)]
enum Flaw {
    /// Says it is fresh, as of the searched snapshot, whatever its data.
    HidesStaleness,
    /// Drops its data when a refresh fails.
    DropsDataOnFailure,
    /// Claims to have no data, so every search rebuilds it (a refresh from
    /// a search under `manual` and `watch`).
    RefreshesFromEverySearch,
    /// Refuses to build after a version bump (a hard error).
    FailsOnBump,
}

#[async_trait::async_trait]
impl Map for Broken {
    fn describe(&self) -> &crate::MapDescriptor {
        self.inner.describe()
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = self.inner.query(request, view).await;
        if self.flaw == Flaw::HidesStaleness {
            response.freshness.stale = false;
            response.freshness.snapshot = view.snapshot.clone();
        }
        response
    }
}

impl Refreshable for Broken {
    fn data_snapshot(&self) -> Option<&SnapshotRef> {
        match self.flaw {
            Flaw::RefreshesFromEverySearch => None,
            _ => self.inner.data_snapshot(),
        }
    }

    fn refresh(&mut self, target: &Target<'_>) -> Result<(), String> {
        if self.flaw == Flaw::FailsOnBump && self.inner.describe().version > 1 {
            return Err("the map's identity changed".into());
        }
        let result = self.inner.refresh(target);
        if result.is_err() && self.flaw == Flaw::DropsDataOnFailure {
            self.inner = SymDefs::new(self.store.clone());
        }
        result
    }
}

#[tokio::test]
async fn c3_rejects_each_known_bad_stored_map() {
    let fixture = Fixture::new();
    for (flaw, expected) in [
        (Flaw::HidesStaleness, "answered as of"),
        (Flaw::DropsDataOnFailure, "failed refresh"),
        (Flaw::RefreshesFromEverySearch, "search after the edit"),
        (Flaw::FailsOnBump, "version bump"),
    ] {
        let make = |version: u32| -> Box<dyn Refreshable> {
            Box::new(Broken {
                inner: SymDefs::new(fixture.store()).with_version(version),
                store: fixture.store(),
                flaw,
            })
        };
        let subject = || make(1);
        let bumped = || make(2);
        let failure = check_freshness(&subject, &bumped, &fixture.case())
            .await
            .expect_err("a known-bad map must fail C3");
        assert_eq!(failure.contract, "C3", "{failure:?}");
        assert!(
            failure.detail.contains(expected),
            "{expected:?} not in {failure:?}"
        );
    }
}

#[tokio::test]
async fn c3_refuses_a_fixture_whose_edit_does_not_change_the_answer() {
    let fixture = Fixture::new();
    let subject = || -> Box<dyn Refreshable> { Box::new(SymDefs::new(fixture.store())) };
    let mut case = fixture.case();
    case.text = "alpha";
    let failure = check_freshness(&subject, &subject, &case)
        .await
        .unwrap_err();
    assert_eq!(failure.contract, "kit");
}

/// A live map that says so: observed the live tree, as of the view.
struct LiveMap(MapDescriptor);

#[async_trait::async_trait]
impl Map for LiveMap {
    fn describe(&self) -> &MapDescriptor {
        &self.0
    }

    async fn query(&self, _request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.0, view, MapStatus::Ok);
        response.freshness.observed = Observed::LiveTree;
        response
    }
}

fn live_descriptor() -> MapDescriptor {
    MapDescriptor {
        id: "live",
        version: 1,
        contribution: Contribution::RankList,
        accepts_text: true,
        accepts: vec![],
        emits: vec![NodeKind::Chunk],
        freshness: FreshnessModel::Live,
        source_labels: vec![SourceLabel::Lexical],
        needs: vec![],
    }
}

#[tokio::test]
async fn c3_passes_a_live_map_that_reads_the_live_tree_and_rejects_one_that_claims_a_snapshot() {
    let fixture = Fixture::new();
    let view = fixture.old.state().view();
    let good = || -> Box<dyn Map> { Box::new(LiveMap(live_descriptor())) };
    check_live_freshness(&good, &view, "alpha", Duration::from_secs(5))
        .await
        .unwrap();
    // A live map built on `MapResponse::empty` reports `observed = snapshot`.
    let bad = || -> Box<dyn Map> {
        Box::new(FixedMap {
            descriptor: live_descriptor(),
            hits: vec![],
        })
    };
    let failure = check_live_freshness(&bad, &view, "alpha", Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(failure.contract, "C3");
    // A stored map is not checked as a live one.
    let stored = || -> Box<dyn Map> {
        Box::new(FixedMap {
            descriptor: symdefs::descriptor(),
            hits: vec![],
        })
    };
    let failure = check_live_freshness(&stored, &view, "alpha", Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(failure.contract, "kit");
}
