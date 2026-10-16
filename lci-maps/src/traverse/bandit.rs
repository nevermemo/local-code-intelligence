//! The in-request edge bandit (`[traverse] edge_bandit`, off by default; spike
//! slice N2).
//!
//! Inside one request, the walk learns which kinds of edge find answers and
//! orders its frontier toward them. Nothing carries over between requests: a
//! [`Bandit`] is born with the run and dies with it (a resumed call starts a
//! new one; its settings are kept in the handle and a resume with other
//! settings is refused, see [`Settings::fingerprint`]).
//!
//! The hit signal is a [`Signal`] (`[traverse] bandit_signal`); the weight
//! update below does not depend on which one is used, only on the hit or miss
//! each rated node gets.
//!
//! * `topk` (the default) is a **diagnostic**: when an expansion adopts a node
//!   for the first time, the node is a *hit* for the kind of edge that found
//!   it if its rank (`State::ranked_potential`) would place it in the current
//!   top `k` results ([`TopK`]); every adopted node is a *try*. `k` is the
//!   walk's answer cutoff (`Traversal::top_k`, else
//!   [`stop::ANSWER_K`](super::stop::ANSWER_K)). Nothing is recorded while
//!   fewer than `k` results exist (everything would place, which only measures
//!   the fill phase). That rank is built from the same kind priors the weights
//!   reshuffle, so `topk` reinforces itself: it shows what the walk already
//!   believes, not whether a kind pays.
//! * `rerank` is independent of the walk's own score: every `every` expansions,
//!   a reranker (its own rescorer, not `rerank_frontier`'s) rates up to `sample`
//!   of the nodes adopted since, and a score at or above `threshold` is a hit.
//!   The calls are counted (`totals.bandit_calls`, `totals.bandit_docs`) and
//!   capped by `model_calls` together with the frontier reranker's.
//! * `fixed` and `random` are cost-free baselines with no stats: per-kind
//!   weights from a table, or drawn from a seed. They show how much of an
//!   effect is just reordering.
//!
//! A kind's smoothed hit rate is `(hits + 1) / (tries + 2)`; its weight is that
//! rate over the mean rate of the kinds tried so far, raised to `rate`
//! (`[traverse] bandit_rate`, 1 by default; 0 switches the effect off) and
//! clamped to `[WEIGHT_MIN, WEIGHT_MAX]`. A kind not tried yet has the rate of
//! no evidence, `1/2`, so it is tried early when the tried kinds pay little.
//!
//! The weight multiplies a child's *frontier priority* only. The potential,
//! and so every score the answers are ranked by and the score bound is
//! computed from, is unchanged. The weights an expansion uses are those of the
//! stats at its start. A route that relaxes a queued node keeps its weight
//! (the priority is scaled by the potential ratio), and a node that is queued
//! later (unblocked, rescaled below a relaxed parent, redirected) gets the
//! weight of the edge kind that found it in this call. The one exception is a
//! resume: the states the earlier call held back at its depth limit are queued
//! at their potential (their stats are gone), and the priorities the handle
//! holds are the weighted ones the earlier call left.

use crate::EdgeKind;
pub use lci_core::config::TraverseBanditSignal as Signal;
use lci_core::node::NodeId;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The lowest weight a kind can get.
pub const WEIGHT_MIN: f64 = 0.5;
/// The highest weight a kind can get.
pub const WEIGHT_MAX: f64 = 2.0;
/// The default `bandit_rate`.
pub const DEFAULT_RATE: f64 = 1.0;
/// The largest `bandit_rate` the configuration accepts.
pub const MAX_RATE: f64 = 4.0;

/// The rate a traversal uses: a value outside `0..=MAX_RATE`, or not a number,
/// is the default.
pub fn sane_rate(rate: f64) -> f64 {
    if rate.is_finite() && (0.0..=MAX_RATE).contains(&rate) {
        rate
    } else {
        DEFAULT_RATE
    }
}

/// The settings of the signals other than `topk`.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// `rerank`: a score at or above this is a hit.
    pub threshold: f64,
    /// `rerank`: a rating round after every this many expansions (at least 1).
    pub every: u32,
    /// `rerank`: the most nodes rated per round (at least 1).
    pub sample: u32,
    /// `random`: the seed of the weights.
    pub seed: u64,
    /// `fixed`: the weight of each kind (a kind not named has 1).
    pub fixed: BTreeMap<EdgeKind, f64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            every: 3,
            sample: 4,
            seed: 0,
            fixed: BTreeMap::new(),
        }
    }
}

impl Options {
    /// The options with their numbers made usable: a threshold that is not a
    /// number is 0.5 (else clamped to `0..=1`), `every` and `sample` at least 1,
    /// a fixed weight that is not a number 1 (else clamped to the weight range).
    pub fn sane(mut self) -> Self {
        self.threshold = if self.threshold.is_finite() {
            self.threshold.clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.every = self.every.max(1);
        self.sample = self.sample.max(1);
        for weight in self.fixed.values_mut() {
            *weight = if weight.is_finite() {
                weight.clamp(WEIGHT_MIN, WEIGHT_MAX)
            } else {
                1.0
            };
        }
        self
    }
}

/// What the traversal was asked for, as one object: the rate, the signal and
/// the options.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub rate: f64,
    pub signal: Signal,
    pub options: Options,
}

impl Settings {
    /// Text that names everything that decides the weights, and nothing that
    /// does not apply to the signal: a handle keeps it so that a resume with
    /// other settings is refused
    /// ([`Snapshot::check_bandit`](super::handle::Snapshot::check_bandit)).
    /// `rescorer` is the identity of the `rerank` signal's rescorer (none: it has
    /// none); other signals ignore it.
    pub fn fingerprint(&self, rescorer: Option<&str>) -> String {
        let rate = sane_rate(self.rate);
        let o = self.options.clone().sane();
        match self.signal {
            Signal::Topk => format!("topk;rate={rate}"),
            Signal::Rerank => format!(
                "rerank;rate={rate};threshold={};every={};sample={};rescorer={}",
                o.threshold,
                o.every,
                o.sample,
                rescorer.unwrap_or("none")
            ),
            Signal::Fixed => {
                let table: Vec<String> = o
                    .fixed
                    .iter()
                    .map(|(kind, weight)| format!("{}={weight}", kind.as_str()))
                    .collect();
                format!("fixed;{}", table.join(","))
            }
            Signal::Random => format!("random;seed={}", o.seed),
        }
    }
}

/// What one kind of edge has done in the request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindStat {
    pub tries: u32,
    pub hits: u32,
}

impl KindStat {
    /// `(hits + 1) / (tries + 2)`: 1/2 with no evidence.
    pub fn smoothed(self) -> f64 {
        (f64::from(self.hits) + 1.0) / (f64::from(self.tries) + 2.0)
    }
}

/// The weight of a kind with `stat` among kinds whose mean smoothed rate is
/// `mean`: `(rate / mean) ^ rate`, clamped to `[WEIGHT_MIN, WEIGHT_MAX]`. A mean
/// that is not positive and finite gives 1.
pub fn weight_of(stat: KindStat, mean: f64, rate: f64) -> f64 {
    if !(mean.is_finite() && mean > 0.0) {
        return 1.0;
    }
    (stat.smoothed() / mean)
        .powf(rate)
        .clamp(WEIGHT_MIN, WEIGHT_MAX)
}

/// The `random` signal's weight of `kind`: a number in `[WEIGHT_MIN,
/// WEIGHT_MAX)` that depends on the seed and the kind only.
pub fn random_weight(seed: u64, kind: EdgeKind) -> f64 {
    // FNV-1a of the kind's name, mixed with the seed by splitmix64.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in kind.as_str().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let mut z = seed.wrapping_add(hash).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    let unit = (z >> 11) as f64 / (1u64 << 53) as f64;
    WEIGHT_MIN + (WEIGHT_MAX - WEIGHT_MIN) * unit
}

/// The top `k` ranks seen so far, best first.
#[derive(Debug, Clone)]
pub struct TopK {
    k: usize,
    ranks: Vec<f64>,
}

impl TopK {
    /// The best `k` (at least 1) of `ranks`.
    pub fn new(k: usize, ranks: impl IntoIterator<Item = f64>) -> Self {
        let mut top = Self {
            k: k.max(1),
            ranks: ranks.into_iter().collect(),
        };
        top.ranks.sort_by(|a, b| b.total_cmp(a));
        top.ranks.truncate(top.k);
        top
    }

    /// Whether `k` results exist.
    pub fn is_full(&self) -> bool {
        self.ranks.len() >= self.k
    }

    /// Whether a result of `rank` would be in the top `k`: fewer than `k`
    /// results exist, or it ranks strictly above the `k`-th (a tie goes to the
    /// earlier result, as discovery order does).
    pub fn places(&self, rank: f64) -> bool {
        !self.is_full() || rank.total_cmp(&self.ranks[self.k - 1]).is_gt()
    }

    /// Adds a result of `rank`.
    pub fn insert(&mut self, rank: f64) {
        let at = self.ranks.partition_point(|r| r.total_cmp(&rank).is_ge());
        self.ranks.insert(at, rank);
        self.ranks.truncate(self.k);
    }
}

/// A kind's tries, hits and weight at the end of a request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KindReport {
    pub tries: u32,
    pub hits: u32,
    pub weight: f64,
}

/// The bandit of a request, as reported.
#[derive(Debug, Clone, PartialEq)]
pub struct BanditReport {
    pub rate: f64,
    pub signal: Signal,
    pub kinds: BTreeMap<EdgeKind, KindReport>,
    /// `rerank`: rating calls that failed, and rounds the `model_calls` cap
    /// skipped, in this call.
    pub failures: u32,
    pub skipped: u32,
}

impl BanditReport {
    /// `{"rate": r, "signal": s, "kinds": {kind: {"tries", "hits", "weight"}}}`,
    /// plus `failures` and `skipped` under `rerank`.
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "rate": self.rate,
            "signal": self.signal.as_str(),
            "kinds": self
                .kinds
                .iter()
                .map(|(kind, r)| (
                    kind.as_str().to_owned(),
                    json!({"tries": r.tries, "hits": r.hits, "weight": r.weight}),
                ))
                .collect::<serde_json::Map<String, Value>>(),
        });
        if let (Signal::Rerank, Some(object)) = (self.signal, value.as_object_mut()) {
            object.insert("failures".into(), json!(self.failures));
            object.insert("skipped".into(), json!(self.skipped));
        }
        value
    }
}

/// The tries and hits per kind of edge in one request.
#[derive(Debug, Clone)]
pub struct Bandit {
    rate: f64,
    signal: Signal,
    options: Options,
    stats: BTreeMap<EdgeKind, KindStat>,
    /// Kinds that found a node, for the signals that keep no stats.
    seen: BTreeSet<EdgeKind>,
    failures: u32,
    skipped: u32,
}

impl Bandit {
    pub fn new(settings: &Settings) -> Self {
        Self {
            rate: sane_rate(settings.rate),
            signal: settings.signal,
            options: settings.options.clone().sane(),
            stats: BTreeMap::new(),
            seen: BTreeSet::new(),
            failures: 0,
            skipped: 0,
        }
    }

    pub fn signal(&self) -> Signal {
        self.signal
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    pub fn stat(&self, kind: EdgeKind) -> KindStat {
        self.stats.get(&kind).copied().unwrap_or_default()
    }

    /// Whether the signal reads the walk's own top `k` list.
    pub fn wants_top(&self) -> bool {
        self.signal == Signal::Topk
    }

    /// Whether a node of `rank` is a hit under the `topk` signal, given the
    /// results so far; none when the signal is another or the list is not
    /// full yet (nothing is recorded then). Independent of the weight update.
    pub fn hit(&self, top: Option<&TopK>, rank: f64) -> Option<bool> {
        match (self.signal, top) {
            (Signal::Topk, Some(top)) if top.is_full() => Some(top.places(rank)),
            _ => None,
        }
    }

    /// Whether a rated score is a hit under the `rerank` signal.
    pub fn score_hit(&self, score: f64) -> bool {
        score >= self.options.threshold
    }

    /// Notes that `kind` found a node (for the signals with no stats).
    pub fn observe(&mut self, kind: EdgeKind) {
        self.seen.insert(kind);
    }

    /// Notes one rated node found by `kind`.
    pub fn record(&mut self, kind: EdgeKind, hit: bool) {
        let stat = self.stats.entry(kind).or_default();
        stat.tries = stat.tries.saturating_add(1);
        stat.hits = stat.hits.saturating_add(u32::from(hit));
    }

    /// A rating call failed.
    pub fn note_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    /// A rating round was skipped by the `model_calls` cap.
    pub fn note_skipped(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }

    /// The mean smoothed rate of the kinds tried so far; none before the first
    /// try.
    fn mean(&self) -> Option<f64> {
        let tried: Vec<f64> = self
            .stats
            .values()
            .filter(|s| s.tries > 0)
            .map(|s| s.smoothed())
            .collect();
        (!tried.is_empty()).then(|| tried.iter().sum::<f64>() / tried.len() as f64)
    }

    /// The weight of `kind` now; 1 before any kind was rated under the signals
    /// that learn.
    pub fn weight(&self, kind: EdgeKind) -> f64 {
        match self.signal {
            Signal::Topk | Signal::Rerank => match self.mean() {
                Some(mean) => weight_of(self.stat(kind), mean, self.rate),
                None => 1.0,
            },
            Signal::Fixed => self.options.fixed.get(&kind).copied().unwrap_or(1.0),
            Signal::Random => random_weight(self.options.seed, kind),
        }
    }

    pub fn report(&self) -> BanditReport {
        let mut kinds: BTreeMap<EdgeKind, KindReport> = self
            .stats
            .iter()
            .map(|(&kind, s)| {
                (
                    kind,
                    KindReport {
                        tries: s.tries,
                        hits: s.hits,
                        weight: self.weight(kind),
                    },
                )
            })
            .collect();
        if matches!(self.signal, Signal::Fixed | Signal::Random) {
            for &kind in self.seen.iter().chain(self.options.fixed.keys()) {
                kinds.entry(kind).or_insert(KindReport {
                    tries: 0,
                    hits: 0,
                    weight: self.weight(kind),
                });
            }
        }
        BanditReport {
            rate: self.rate,
            signal: self.signal,
            kinds,
            failures: self.failures,
            skipped: self.skipped,
        }
    }
}

/// The nodes adopted since the last rating round, with the kind of edge that
/// found each, for the `rerank` signal.
pub type Pending = Vec<(NodeId, EdgeKind)>;

#[cfg(test)]
mod tests {
    use super::*;

    const CALLS: EdgeKind = EdgeKind::CallsOrRefs;
    const TESTS: EdgeKind = EdgeKind::Tests;

    fn settings(signal: Signal) -> Settings {
        Settings {
            rate: 1.0,
            signal,
            options: Options::default(),
        }
    }

    fn bandit(signal: Signal) -> Bandit {
        Bandit::new(&settings(signal))
    }

    #[test]
    fn smoothing_gives_one_half_with_no_evidence() {
        assert_eq!(KindStat::default().smoothed(), 0.5);
        assert_eq!(KindStat { tries: 2, hits: 2 }.smoothed(), 0.75);
        assert_eq!(KindStat { tries: 2, hits: 0 }.smoothed(), 0.25);
    }

    #[test]
    fn weight_is_the_rate_over_the_mean() {
        // Rates 0.75 and 0.25 have the mean 0.5: weights 1.5 and 0.5.
        let paying = KindStat { tries: 2, hits: 2 };
        let idle = KindStat { tries: 2, hits: 0 };
        assert!((weight_of(paying, 0.5, 1.0) - 1.5).abs() < 1e-12);
        assert!((weight_of(idle, 0.5, 1.0) - 0.5).abs() < 1e-12);
        // A rate of 0 switches the effect off; 2 squares the ratio.
        assert_eq!(weight_of(paying, 0.5, 0.0), 1.0);
        assert!((weight_of(paying, 0.5, 2.0) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn weight_is_clamped() {
        let paying = KindStat {
            tries: 1000,
            hits: 1000,
        };
        let idle = KindStat {
            tries: 1000,
            hits: 0,
        };
        assert_eq!(weight_of(paying, 0.1, 1.0), WEIGHT_MAX);
        assert_eq!(weight_of(idle, 0.9, 1.0), WEIGHT_MIN);
        assert_eq!(weight_of(paying, 0.1, MAX_RATE), WEIGHT_MAX);
    }

    #[test]
    fn a_bad_mean_gives_weight_one() {
        let stat = KindStat { tries: 3, hits: 1 };
        for mean in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(weight_of(stat, mean, 1.0), 1.0);
        }
    }

    #[test]
    fn sane_rate_falls_back_to_the_default() {
        assert_eq!(sane_rate(0.0), 0.0);
        assert_eq!(sane_rate(2.5), 2.5);
        for bad in [-0.1, MAX_RATE + 0.1, f64::NAN, f64::INFINITY] {
            assert_eq!(sane_rate(bad), DEFAULT_RATE);
        }
    }

    #[test]
    fn no_kind_tried_means_weight_one() {
        let bandit = bandit(Signal::Topk);
        assert_eq!(bandit.weight(CALLS), 1.0);
        assert!(bandit.report().kinds.is_empty());
    }

    #[test]
    fn weights_follow_the_hit_rates_and_stay_in_range() {
        let mut bandit = bandit(Signal::Topk);
        for _ in 0..30 {
            bandit.record(CALLS, true);
            bandit.record(TESTS, false);
        }
        let (a, b) = (bandit.weight(CALLS), bandit.weight(TESTS));
        assert!(a > 1.0 && b < 1.0, "{a} {b}");
        assert!((WEIGHT_MIN..=WEIGHT_MAX).contains(&a));
        assert!((WEIGHT_MIN..=WEIGHT_MAX).contains(&b));
        // A kind not tried yet has the rate of no evidence (1/2), above the
        // mean of these two kinds' rates only if they pay little: here the
        // mean is about 0.5, so it stays near 1.
        let fresh = bandit.weight(EdgeKind::Imports);
        assert!((fresh - 1.0).abs() < 0.1, "{fresh}");
        let report = bandit.report();
        assert_eq!(report.kinds[&CALLS].tries, 30);
        assert_eq!(report.kinds[&CALLS].hits, 30);
        assert_eq!(report.kinds[&TESTS].hits, 0);
        assert_eq!(report.kinds[&CALLS].weight, a);
    }

    #[test]
    fn an_untried_kind_is_tried_early_when_the_tried_kinds_pay_little() {
        let mut bandit = bandit(Signal::Topk);
        for _ in 0..20 {
            bandit.record(CALLS, false);
        }
        assert!(bandit.weight(EdgeKind::Imports) > 1.0);
        assert_eq!(bandit.weight(EdgeKind::Imports), WEIGHT_MAX);
    }

    #[test]
    fn top_k_places_until_full_then_needs_a_strictly_better_rank() {
        let mut top = TopK::new(2, [0.5, 0.9, 0.1]);
        // Kept: 0.9, 0.5.
        assert!(top.is_full());
        assert!(top.places(0.6));
        assert!(!top.places(0.5), "a tie with the k-th does not place");
        assert!(!top.places(0.2));
        top.insert(0.6);
        assert!(!top.places(0.6));
        assert!(top.places(0.7));

        let mut empty = TopK::new(2, []);
        assert!(!empty.is_full());
        assert!(empty.places(0.0));
        empty.insert(0.3);
        assert!(empty.places(0.0));
        empty.insert(0.2);
        assert!(empty.is_full());
        assert!(!empty.places(0.2));
        // k of 0 counts as 1.
        assert!(TopK::new(0, []).places(1.0));
    }

    #[test]
    fn topk_records_nothing_while_the_list_is_not_full() {
        let b = bandit(Signal::Topk);
        let part = TopK::new(2, [0.5]);
        assert_eq!(b.hit(Some(&part), 0.9), None);
        let full = TopK::new(2, [0.5, 0.4]);
        assert_eq!(b.hit(Some(&full), 0.9), Some(true));
        assert_eq!(b.hit(Some(&full), 0.1), Some(false));
        assert_eq!(b.hit(None, 0.9), None);
        // The other signals read no list.
        assert_eq!(bandit(Signal::Rerank).hit(Some(&full), 0.9), None);
    }

    #[test]
    fn a_score_at_the_threshold_is_a_hit() {
        let b = bandit(Signal::Rerank);
        assert!(b.score_hit(0.5));
        assert!(b.score_hit(0.9));
        assert!(!b.score_hit(0.49));
    }

    #[test]
    fn random_weights_are_in_range_and_depend_on_the_seed_and_kind_only() {
        for seed in [0, 1, 7, u64::MAX] {
            for kind in EdgeKind::ALL {
                let w = random_weight(seed, kind);
                assert!((WEIGHT_MIN..WEIGHT_MAX).contains(&w), "{seed} {kind:?} {w}");
                assert_eq!(w, random_weight(seed, kind));
            }
        }
        let a: Vec<f64> = EdgeKind::ALL.iter().map(|&k| random_weight(1, k)).collect();
        let b: Vec<f64> = EdgeKind::ALL.iter().map(|&k| random_weight(2, k)).collect();
        assert_ne!(a, b);
        assert!(
            a.windows(2).any(|w| w[0] != w[1]),
            "kinds do not all get one weight"
        );
    }

    #[test]
    fn fixed_and_random_keep_no_stats_and_report_their_weights() {
        let mut options = Options::default();
        options.fixed.insert(CALLS, 1.5);
        let mut fixed = Bandit::new(&Settings {
            rate: 1.0,
            signal: Signal::Fixed,
            options: options.clone(),
        });
        fixed.observe(TESTS);
        assert_eq!(fixed.weight(CALLS), 1.5);
        assert_eq!(fixed.weight(TESTS), 1.0);
        let report = fixed.report();
        assert_eq!(report.kinds[&CALLS].weight, 1.5);
        assert_eq!(report.kinds[&TESTS].tries, 0);

        options.seed = 5;
        let mut random = Bandit::new(&Settings {
            rate: 1.0,
            signal: Signal::Random,
            options,
        });
        random.observe(TESTS);
        assert_eq!(random.weight(TESTS), random_weight(5, TESTS));
        assert_eq!(random.report().kinds[&TESTS].weight, random_weight(5, TESTS));
    }

    #[test]
    fn the_fingerprint_names_what_decides_the_weights() {
        let mut options = Options::default();
        options.fixed.insert(CALLS, 1.5);
        let make = |signal, rate, options: &Options| {
            Settings {
                rate,
                signal,
                options: options.clone(),
            }
            .fingerprint(None)
        };
        let base = make(Signal::Topk, 1.0, &options);
        assert_ne!(base, make(Signal::Topk, 2.0, &options));
        assert_ne!(base, make(Signal::Rerank, 1.0, &options));
        // Options that do not apply to the signal do not change it.
        let mut other = options.clone();
        other.threshold = 0.9;
        other.seed = 3;
        assert_eq!(base, make(Signal::Topk, 1.0, &other));
        assert_ne!(
            make(Signal::Rerank, 1.0, &options),
            make(Signal::Rerank, 1.0, &other)
        );
        assert_ne!(
            make(Signal::Random, 1.0, &options),
            make(Signal::Random, 1.0, &other)
        );
        other.fixed.insert(CALLS, 1.25);
        assert_ne!(
            make(Signal::Fixed, 1.0, &options),
            make(Signal::Fixed, 1.0, &other)
        );
    }

    #[test]
    fn the_report_names_kinds_in_json() {
        let mut bandit = bandit(Signal::Topk);
        bandit.record(CALLS, true);
        let value = bandit.report().to_json();
        assert_eq!(value["rate"], 1.0);
        assert_eq!(value["signal"], "topk");
        assert_eq!(value["kinds"][CALLS.as_str()]["tries"], 1);
        assert_eq!(value["kinds"][CALLS.as_str()]["hits"], 1);
        assert_eq!(value["kinds"][CALLS.as_str()]["weight"], 1.0);
        assert!(value.get("failures").is_none());
        let mut rerank = Bandit::new(&settings(Signal::Rerank));
        rerank.note_failure();
        rerank.note_skipped();
        let value = rerank.report().to_json();
        assert_eq!(value["failures"], 1);
        assert_eq!(value["skipped"], 1);
    }
}
