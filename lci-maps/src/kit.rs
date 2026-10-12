//! The map contract kit, skeleton (contract-kit.md; slice S2 implements
//! contracts C1 and C6). One generic harness checks any [`Map`] against a
//! fixture: a snapshot view, a query text and a control map that returns
//! candidates (the C1 carve-out: the comparison with the map removed is only
//! meaningful while another map answers).
//!
//! A kit that passes a known-bad map is a kit bug: the kit's own tests run a
//! blocking map (C1) and a nondeterministic map (C6) and expect the
//! matching contract id.

use crate::{
    Fused, Map, MapDescriptor, MapHit, MapRequest, MapResponse, MapStatus, Registry, Run,
    SnapshotView,
};
use lci_core::node::NodeId;
use std::time::{Duration, Instant};

/// A contract the map broke, with the first difference found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractFailure {
    pub contract: &'static str,
    pub detail: String,
}

fn broke(contract: &'static str, detail: impl Into<String>) -> ContractFailure {
    ContractFailure {
        contract,
        detail: detail.into(),
    }
}

/// Builds a fresh instance of a map; the registry owns what it runs.
pub type Factory<'f, 'a> = &'f (dyn Fn() -> Box<dyn Map + 'a> + 'f);

/// What every contract check runs against.
pub struct Fixture<'v, 'a> {
    pub view: &'v SnapshotView<'a>,
    pub text: &'v str,
    /// Each map's budget per call.
    pub budget: Duration,
    /// How far past the budget a whole run may end before C1 counts the map
    /// as having stalled the search.
    pub slack: Duration,
    pub rrf_k: f32,
}

/// The ways the C1 fault injector makes a map fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Failed,
    Panic,
    /// Sleeps past the deadline having emitted nothing.
    PastDeadline,
    /// Emits a node that is not in the universe.
    ForeignNode,
    /// Emits a NaN score.
    NotANumber,
}

impl Fault {
    pub const ALL: [Fault; 5] = [
        Self::Failed,
        Self::Panic,
        Self::PastDeadline,
        Self::ForeignNode,
        Self::NotANumber,
    ];

    /// A deadline is a partial answer (a note); everything else is a
    /// failure (a warning).
    fn is_note(self) -> bool {
        self == Self::PastDeadline
    }
}

/// Wraps a map and replaces its answer with a fault; keeps its descriptor.
pub struct FaultInjector<'a> {
    pub inner: Box<dyn Map + 'a>,
    pub fault: Fault,
}

#[async_trait::async_trait]
impl Map for FaultInjector<'_> {
    fn describe(&self) -> &MapDescriptor {
        self.inner.describe()
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let first = || {
            request
                .seeds
                .first()
                .map(|(node, _)| node.clone())
                .or_else(|| view.ids().first().cloned())
        };
        let hit = |node: NodeId, score| MapHit {
            node,
            rank: 1,
            score: Some(score),
            evidence: Vec::new(),
        };
        let mut response = MapResponse::empty(self.describe(), view, MapStatus::Ok);
        match self.fault {
            Fault::Failed => {
                response.status = MapStatus::Failed {
                    warning: format!("{} failed (injected)", self.describe().id),
                };
            }
            Fault::Panic => panic!("injected panic in {}", self.describe().id),
            Fault::PastDeadline => {
                let wait = request
                    .budget
                    .deadline
                    .saturating_duration_since(Instant::now());
                tokio::time::sleep(wait * 2 + Duration::from_millis(50)).await;
            }
            Fault::ForeignNode => {
                let node = NodeId::file("kit/not-in-the-universe").expect("valid id");
                response.hits.push(hit(node, 1.0));
            }
            Fault::NotANumber => {
                if let Some(node) = first() {
                    response.hits.push(hit(node, f32::NAN));
                }
            }
        }
        response
    }
}

/// A map that returns the same hits on every call (a known-good map for
/// the kit's own tests and for fixtures).
pub struct FixedMap {
    pub descriptor: MapDescriptor,
    pub hits: Vec<MapHit>,
}

#[async_trait::async_trait]
impl Map for FixedMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.hits = match self.descriptor.contribution {
            // A prior answers for the seeds it knows.
            crate::Contribution::Prior => self
                .hits
                .iter()
                .filter(|hit| request.seeds.iter().any(|(node, _)| *node == hit.node))
                .cloned()
                .collect(),
            _ => self.hits.clone(),
        };
        response
    }
}

async fn run_maps<'a>(
    maps: Vec<(Box<dyn Map + 'a>, bool)>,
    fixture: &Fixture<'_, 'a>,
) -> Result<(Run, Duration), ContractFailure> {
    let mut registry = Registry::new(fixture.rrf_k);
    let mut disabled = Vec::new();
    for (map, enabled) in maps {
        let id = map.describe().id;
        registry
            .register(map, 1.0, 40)
            .map_err(|error| broke("kit", error.to_string()))?;
        if !enabled {
            disabled.push(id);
        }
    }
    for id in disabled {
        registry.set_enabled(id, false);
    }
    let started = Instant::now();
    let run = registry
        .run(Some(fixture.text), fixture.budget, fixture.view)
        .await;
    Ok((run, started.elapsed()))
}

/// Node, score bits and parts of every fused entry, in order.
fn same_pool(
    contract: &'static str,
    label: &str,
    got: &[Fused],
    want: &[Fused],
) -> Result<(), ContractFailure> {
    if got.len() != want.len() {
        return Err(broke(
            contract,
            format!(
                "{label}: {} fused nodes, expected {}",
                got.len(),
                want.len()
            ),
        ));
    }
    for (position, (a, b)) in got.iter().zip(want).enumerate() {
        if a.node != b.node || a.score.to_bits() != b.score.to_bits() || a.parts != b.parts {
            return Err(broke(
                contract,
                format!(
                    "{label}: position {position} is {} at {} ({:?}), expected {} at {} ({:?})",
                    a.node, a.score, a.parts, b.node, b.score, b.parts
                ),
            ));
        }
    }
    Ok(())
}

/// Contract C1: for every injected fault, and for the map switched off,
/// the run fuses exactly what it fuses without the map; a failure adds one
/// warning naming the map, a missed deadline one note, a switched-off map
/// nothing; and no run, including one with the map as it is, ends later
/// than the budget plus the slack.
pub async fn check_fail_open<'a>(
    subject: Factory<'_, 'a>,
    control: Factory<'_, 'a>,
    fixture: &Fixture<'_, 'a>,
) -> Result<(), ContractFailure> {
    let id = subject().describe().id;
    let limit = fixture.budget + fixture.slack;
    let (baseline, _) = run_maps(vec![(control(), true)], fixture).await?;
    if baseline.fused.is_empty() {
        return Err(broke("kit", "the control map returned no candidates"));
    }
    let attributed = |pairs: Vec<(&str, &str)>| pairs.iter().filter(|(map, _)| *map == id).count();
    let others = |run: &Run| {
        run.warnings().iter().filter(|(map, _)| *map != id).count()
            + run.notes().iter().filter(|(map, _)| *map != id).count()
    };
    let (as_is, elapsed) = run_maps(vec![(subject(), true), (control(), true)], fixture).await?;
    if elapsed > limit {
        return Err(broke(
            "C1",
            format!("{id} as it is kept the run for {elapsed:?}, past {limit:?}"),
        ));
    }
    if let MapStatus::Failed { .. } = as_is
        .calls
        .iter()
        .find(|c| c.id == id)
        .map(|c| &c.response.status)
        .expect("called")
    {
        same_pool(
            "C1",
            &format!("{id} failing by itself"),
            &as_is.fused,
            &baseline.fused,
        )?;
    }
    let mut runs = vec![("switched off", None)];
    runs.extend(Fault::ALL.map(|fault| ("injected", Some(fault))));
    for (label, fault) in runs {
        let (map, enabled): (Box<dyn Map + 'a>, bool) = match fault {
            Some(fault) => (
                Box::new(FaultInjector {
                    inner: subject(),
                    fault,
                }),
                true,
            ),
            None => (subject(), false),
        };
        let label = format!("{label} {fault:?}");
        let (run, elapsed) = run_maps(vec![(map, enabled), (control(), true)], fixture).await?;
        if elapsed > limit {
            return Err(broke(
                "C1",
                format!("{label}: the run took {elapsed:?}, past {limit:?}"),
            ));
        }
        same_pool("C1", &label, &run.fused, &baseline.fused)?;
        let (warnings, notes) = (attributed(run.warnings()), attributed(run.notes()));
        let expected = match fault {
            None => (0, 0),
            Some(fault) if fault.is_note() => (0, 1),
            Some(_) => (1, 0),
        };
        if (warnings, notes) != expected || others(&run) != others(&baseline) {
            return Err(broke(
                "C1",
                format!(
                    "{label}: {warnings} warnings and {notes} notes name {id} (expected {expected:?}); \
                     warnings {:?}, notes {:?}",
                    run.warnings(),
                    run.notes()
                ),
            ));
        }
    }
    Ok(())
}

/// The map's own call in a run, as it can be compared across runs.
fn subject_answer(run: &Run, id: &str) -> Option<(Vec<MapHit>, MapStatus)> {
    run.calls
        .iter()
        .find(|call| call.id == id)
        .map(|call| (call.response.hits.clone(), call.response.status.clone()))
}

/// Contract C6: the same fixture gives the same answer from the map and the
/// same fused pool (nodes, score bits, order), run after run and whatever
/// the registration order.
pub async fn check_determinism<'a>(
    subject: Factory<'_, 'a>,
    control: Factory<'_, 'a>,
    fixture: &Fixture<'_, 'a>,
) -> Result<(), ContractFailure> {
    let id = subject().describe().id;
    let (first, _) = run_maps(vec![(subject(), true), (control(), true)], fixture).await?;
    for (label, maps) in [
        ("second run", vec![(subject(), true), (control(), true)]),
        (
            "reversed registration",
            vec![(control(), true), (subject(), true)],
        ),
    ] {
        let (run, _) = run_maps(maps, fixture).await?;
        same_pool("C6", label, &run.fused, &first.fused)?;
        if subject_answer(&run, id) != subject_answer(&first, id) {
            return Err(broke("C6", format!("{label}: {id} answered differently")));
        }
    }
    Ok(())
}
