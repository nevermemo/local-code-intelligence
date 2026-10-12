//! Contract C3, the freshness model (spec
//! `docs/spike/multimap/specs/contract-kit.md`, C3), for stored maps driven
//! by [`prepare`] and for live maps.
//!
//! A stored map is checked on a fixture with two snapshots, "old" (indexed)
//! and "new" (after an edit), plus a "broken" target whose refresh must
//! fail. Under every `[index].freshness` mode: a map with no data is built;
//! after the edit, under `on-search` the core refreshes and the map is
//! refreshed to give the new snapshot's answer, while under `manual` and
//! `watch` the core keeps the old snapshot and the map answers from it;
//! when the core moved without a search (an explicit index, the watcher),
//! `manual` and `watch` leave the map stale and it says so. A failed
//! refresh keeps the previous data and warns. A version bump is a rebuild
//! or a stale map, never a hard error. Every answer names the snapshot it
//! came from.

use crate::{
    Budget, FreshnessModel, Map, MapHit, MapRequest, MapResponse, MapStatus, Observed, SnapshotRef,
    SnapshotView,
    fresh::{IndexFreshness, Prepared, Refreshable, Target, prepare},
    kit::ContractFailure,
};
use lci_core::chunk::Chunk;
use std::time::{Duration, Instant};

fn broke(detail: impl Into<String>) -> ContractFailure {
    ContractFailure {
        contract: "C3",
        detail: detail.into(),
    }
}

/// A snapshot of the fixture: what a stored map is brought to, and the
/// chunks a search over it sees (the view's universe).
#[derive(Clone, Copy)]
pub struct State<'s> {
    pub target: Target<'s>,
    pub chunks: &'s [Chunk],
}

impl<'s> State<'s> {
    fn view(&self) -> SnapshotView<'s> {
        SnapshotView::new(self.target.snapshot.clone(), self.chunks.iter().collect())
    }
}

/// The C3 fixture for a stored map.
pub struct FreshnessCase<'s> {
    /// Indexed before the edit.
    pub old: State<'s>,
    /// The tree after the edit, as the core indexes it.
    pub new: State<'s>,
    /// The new snapshot with a tree the map cannot read: a refresh to it
    /// must fail.
    pub broken: Target<'s>,
    /// A query whose answer differs between `old` and `new`.
    pub text: &'s str,
    pub budget: Duration,
}

/// Builds a fresh, never-built instance of a stored map.
pub type StoredFactory<'f> = &'f dyn Fn() -> Box<dyn Refreshable>;

async fn ask(
    map: &dyn Refreshable,
    text: &str,
    budget: Duration,
    view: &SnapshotView<'_>,
) -> MapResponse {
    let params = serde_json::Value::Null;
    let request = MapRequest {
        text: Some(text),
        seeds: &[],
        limit: 40,
        budget: Budget {
            deadline: Instant::now() + budget,
            max_external_calls: u32::MAX,
            max_nodes_touched: u64::MAX,
        },
        params: &params,
        as_of: None,
    };
    map.query(&request, view).await
}

fn expect_prepared(label: &str, got: &Prepared, want: &[Prepared]) -> Result<(), ContractFailure> {
    if want.contains(got) {
        Ok(())
    } else {
        Err(broke(format!(
            "{label}: prepare gave {got:?}, expected one of {want:?}"
        )))
    }
}

/// The hits' nodes and ranks (evidence aside).
fn ranked(hits: &[MapHit]) -> Vec<(String, u32)> {
    hits.iter()
        .map(|hit| (hit.node.to_string(), hit.rank))
        .collect()
}

/// The answer must be a usable answer from `snapshot`: not failed, from
/// the map's freshness model, observed on a snapshot, every hit a chunk of
/// the view, and `stale` exactly when `snapshot` is not the view's.
fn expect_answer(
    label: &str,
    map: &dyn Refreshable,
    response: &MapResponse,
    snapshot: &SnapshotRef,
    view: &SnapshotView<'_>,
) -> Result<(), ContractFailure> {
    let descriptor = map.describe();
    if matches!(
        response.status,
        MapStatus::Failed { .. } | MapStatus::Skipped { .. }
    ) {
        return Err(broke(format!(
            "{label}: {} answered {:?}",
            descriptor.id, response.status
        )));
    }
    let freshness = &response.freshness;
    if freshness.model != descriptor.freshness || freshness.observed != Observed::Snapshot {
        return Err(broke(format!(
            "{label}: freshness {:?} observed {:?}; a stored map with model {:?} answers from a snapshot",
            freshness.model, freshness.observed, descriptor.freshness
        )));
    }
    if freshness.snapshot != *snapshot {
        return Err(broke(format!(
            "{label}: answered as of {:?}, but its data is {:?}",
            freshness.snapshot, snapshot
        )));
    }
    let stale = *snapshot != view.snapshot;
    if freshness.stale != stale {
        return Err(broke(format!(
            "{label}: stale = {}, but its data is {:?} and the search is on {:?}",
            freshness.stale, snapshot, view.snapshot
        )));
    }
    if let Some(hit) = response.hits.iter().find(|hit| !view.contains(&hit.node)) {
        return Err(broke(format!(
            "{label}: {} is not a chunk of the searched snapshot",
            hit.node
        )));
    }
    Ok(())
}

fn expect_same_hits(
    label: &str,
    got: &MapResponse,
    want: &MapResponse,
) -> Result<(), ContractFailure> {
    if got.hits == want.hits {
        Ok(())
    } else {
        Err(broke(format!(
            "{label}: answered {:?}, a fresh build of that snapshot answers {:?}",
            ranked(&got.hits),
            ranked(&want.hits)
        )))
    }
}

/// The answer of a fresh build of `state`.
async fn reference(
    subject: StoredFactory<'_>,
    state: State<'_>,
    case: &FreshnessCase<'_>,
) -> Result<MapResponse, ContractFailure> {
    let mut map = subject();
    let prepared = prepare(map.as_mut(), IndexFreshness::OnSearch, &state.target);
    expect_prepared("reference build", &prepared, &[Prepared::Built])?;
    let view = state.view();
    let response = ask(map.as_ref(), case.text, case.budget, &view).await;
    expect_answer(
        "reference build",
        map.as_ref(),
        &response,
        state.target.snapshot,
        &view,
    )?;
    Ok(response)
}

/// Contract C3 for a stored map. `subject` builds a never-built instance;
/// `bumped` builds one with a higher version over the same stored data.
pub async fn check_freshness(
    subject: StoredFactory<'_>,
    bumped: StoredFactory<'_>,
    case: &FreshnessCase<'_>,
) -> Result<(), ContractFailure> {
    let (old_view, new_view) = (case.old.view(), case.new.view());
    let old_snapshot = case.old.target.snapshot;
    let new_snapshot = case.new.target.snapshot;
    if old_snapshot == new_snapshot || case.broken.snapshot == old_snapshot {
        return Err(ContractFailure {
            contract: "kit",
            detail: "the fixture needs distinct old, new and broken snapshots".into(),
        });
    }

    // Reference answers: a fresh build of each snapshot.
    let want_old = reference(subject, case.old, case).await?;
    let want_new = reference(subject, case.new, case).await?;
    if want_old.hits == want_new.hits {
        return Err(ContractFailure {
            contract: "kit",
            detail: "the fixture's edit must change the answer to its query".into(),
        });
    }

    for mode in [
        IndexFreshness::Manual,
        IndexFreshness::Watch,
        IndexFreshness::OnSearch,
    ] {
        let label = |what: &str| format!("{mode:?}: {what}");
        let mut map = subject();
        // A search may create a missing index, in every mode.
        let prepared = prepare(map.as_mut(), mode, &case.old.target);
        expect_prepared(&label("first search"), &prepared, &[Prepared::Built])?;
        let response = ask(map.as_ref(), case.text, case.budget, &old_view).await;
        expect_answer(
            &label("first search"),
            map.as_ref(),
            &response,
            old_snapshot,
            &old_view,
        )?;
        expect_same_hits(&label("first search"), &response, &want_old)?;

        // The tree is edited and searched. Under on-search the core
        // refreshes and so does the map; otherwise both keep the old
        // snapshot.
        let (core, view, want, expected) = if mode == IndexFreshness::OnSearch {
            (case.new, &new_view, &want_new, Prepared::Refreshed)
        } else {
            (case.old, &old_view, &want_old, Prepared::Current)
        };
        let prepared = prepare(map.as_mut(), mode, &core.target);
        expect_prepared(&label("search after the edit"), &prepared, &[expected])?;
        let response = ask(map.as_ref(), case.text, case.budget, view).await;
        expect_answer(
            &label("search after the edit"),
            map.as_ref(),
            &response,
            core.target.snapshot,
            view,
        )?;
        expect_same_hits(&label("search after the edit"), &response, want)?;

        if mode != IndexFreshness::OnSearch {
            // The core moved to the new snapshot without a search (an
            // explicit index, or the watcher): a search must not refresh
            // the map; it answers from the old data and says it is stale.
            let prepared = prepare(map.as_mut(), mode, &case.new.target);
            expect_prepared(
                &label("core refreshed outside a search"),
                &prepared,
                &[Prepared::Stale],
            )?;
            if map.data_snapshot() != Some(old_snapshot) {
                return Err(broke(label("a search refreshed the map")));
            }
            let response = ask(map.as_ref(), case.text, case.budget, &new_view).await;
            expect_answer(
                &label("core refreshed outside a search"),
                map.as_ref(),
                &response,
                old_snapshot,
                &new_view,
            )?;
        }
    }

    // A failed refresh keeps the previous data and warns; the previous
    // snapshot stays searchable.
    let mut map = subject();
    prepare(map.as_mut(), IndexFreshness::OnSearch, &case.old.target);
    let prepared = prepare(map.as_mut(), IndexFreshness::OnSearch, &case.broken);
    let id = map.describe().id;
    match &prepared {
        Prepared::Failed { warning } if warning.contains(id) => {}
        other => {
            return Err(broke(format!(
                "failed refresh: prepare gave {other:?}, expected a failure naming {id}"
            )));
        }
    }
    if map.data_snapshot() != Some(old_snapshot) {
        return Err(broke(format!(
            "failed refresh: the map's data is now {:?}, expected the previous {old_snapshot:?}",
            map.data_snapshot()
        )));
    }
    let response = ask(map.as_ref(), case.text, case.budget, &old_view).await;
    expect_answer(
        "failed refresh",
        map.as_ref(),
        &response,
        old_snapshot,
        &old_view,
    )?;
    expect_same_hits("failed refresh", &response, &want_old)?;

    // A version bump over data stored by the previous version: rebuilt or
    // stale, never a hard error.
    let mut previous = subject();
    prepare(previous.as_mut(), IndexFreshness::Manual, &case.old.target);
    let mut map = bumped();
    if map.describe().version <= previous.describe().version {
        return Err(ContractFailure {
            contract: "kit",
            detail: "the bumped map must have a higher version".into(),
        });
    }
    let prepared = prepare(map.as_mut(), IndexFreshness::Manual, &case.old.target);
    expect_prepared(
        "version bump",
        &prepared,
        &[Prepared::Built, Prepared::Current, Prepared::Stale],
    )?;
    let response = ask(map.as_ref(), case.text, case.budget, &old_view).await;
    let snapshot = map.data_snapshot().cloned();
    let Some(snapshot) = snapshot else {
        return Err(broke("version bump: the map has no data after the search"));
    };
    expect_answer(
        "version bump",
        map.as_ref(),
        &response,
        &snapshot,
        &old_view,
    )?;
    Ok(())
}

/// Contract C3 for a live map: whatever the mode (a live map has no data
/// to refresh), its answer says it read the live tree, as of the view's
/// snapshot, and is never stale.
pub async fn check_live_freshness(
    subject: &dyn Fn() -> Box<dyn Map>,
    view: &SnapshotView<'_>,
    text: &str,
    budget: Duration,
) -> Result<(), ContractFailure> {
    let map = subject();
    let descriptor = map.describe();
    if descriptor.freshness != FreshnessModel::Live {
        return Err(ContractFailure {
            contract: "kit",
            detail: format!("{} is not a live map", descriptor.id),
        });
    }
    let params = serde_json::Value::Null;
    let request = MapRequest {
        text: Some(text),
        seeds: &[],
        limit: 40,
        budget: Budget {
            deadline: Instant::now() + budget,
            max_external_calls: u32::MAX,
            max_nodes_touched: u64::MAX,
        },
        params: &params,
        as_of: None,
    };
    let response = map.query(&request, view).await;
    let freshness = &response.freshness;
    if freshness.model != descriptor.freshness {
        return Err(broke(format!(
            "{} answered with model {:?}, its descriptor says {:?}",
            descriptor.id, freshness.model, descriptor.freshness
        )));
    }
    if freshness.observed != Observed::LiveTree
        || freshness.snapshot != view.snapshot
        || freshness.stale
    {
        return Err(broke(format!(
            "{} is live but answered observed {:?}, snapshot {:?}, stale {}; expected the live tree on {:?}",
            descriptor.id, freshness.observed, freshness.snapshot, freshness.stale, view.snapshot
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
