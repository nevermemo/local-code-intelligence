//! Scoring rules of the spike bench (`docs/spike/multimap/bench.md`,
//! section 5): everything is computed from ranked evidence paths, so two
//! builds whose report schemas differ are scored the same way.

use std::collections::BTreeSet;

/// Ranked distinct files: the first occurrence of each path, in rank order,
/// at most `limit` of them.
pub fn distinct_files<S: AsRef<str>>(paths: &[S], limit: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    paths
        .iter()
        .map(AsRef::as_ref)
        .filter(|path| seen.insert(*path))
        .take(limit)
        .map(str::to_owned)
        .collect()
}

/// Per-task retrieval scores over the top 10 distinct files.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskScore {
    pub rr: f64,
    pub hit1: bool,
    pub hit5: bool,
    pub hit10: bool,
    pub recall10: f64,
}

pub fn score_task(files: &[String], gold: &[String]) -> TaskScore {
    let files = &files[..files.len().min(10)];
    let first = files.iter().position(|file| gold.contains(file));
    let found = gold.iter().filter(|g| files.contains(g)).count();
    TaskScore {
        rr: first.map_or(0.0, |index| 1.0 / (index + 1) as f64),
        hit1: first.is_some_and(|index| index < 1),
        hit5: first.is_some_and(|index| index < 5),
        hit10: first.is_some(),
        recall10: if gold.is_empty() {
            0.0
        } else {
            found as f64 / gold.len() as f64
        },
    }
}

/// Symbol lookup over the top 5 distinct files: `(hit5, precision5)`.
pub fn score_lookup(files: &[String], gold: &[String]) -> (bool, f64) {
    let files = &files[..files.len().min(5)];
    let correct = files.iter().filter(|file| gold.contains(file)).count();
    let precision = if files.is_empty() {
        0.0
    } else {
        correct as f64 / files.len() as f64
    };
    (correct > 0, precision)
}

/// Nearest-rank percentile of `values` (`p` in 0..=100); `None` when empty.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

/// SplitMix64: a tiny deterministic generator, identical on every platform.
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`), by rejection so there is no modulo bias.
    pub fn below(&mut self, n: usize) -> usize {
        let n = n as u64;
        let zone = u64::MAX - u64::MAX % n;
        loop {
            let value = self.next_u64();
            if value < zone {
                return (value % n) as usize;
            }
        }
    }
}

/// Mean of `deltas` and its 95% percentile interval from a paired bootstrap
/// (`resamples` resamples of the pairs, generator seeded with `seed`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub mean: f64,
    pub low: f64,
    pub high: f64,
}

pub fn bootstrap(deltas: &[f64], resamples: usize, seed: u64) -> Option<Interval> {
    if deltas.is_empty() || resamples == 0 {
        return None;
    }
    let n = deltas.len();
    let mean = deltas.iter().sum::<f64>() / n as f64;
    let mut rng = SplitMix64::new(seed);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..n).map(|_| deltas[rng.below(n)]).sum::<f64>() / n as f64)
        .collect();
    means.sort_by(f64::total_cmp);
    let low = means[((0.025 * resamples as f64).floor() as usize).min(resamples - 1)];
    let high = means[((0.975 * resamples as f64).ceil() as usize).clamp(1, resamples) - 1];
    Some(Interval { mean, low, high })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn distinct_files_keeps_first_occurrence_in_rank_order() {
        assert_eq!(
            distinct_files(&["a", "b", "a", "c", "b", "d"], 3),
            owned(&["a", "b", "c"])
        );
    }

    #[test]
    fn task_scores_use_file_ranks() {
        let files = owned(&["x", "y", "gold2", "z", "gold1"]);
        let score = score_task(&files, &owned(&["gold1", "gold2", "gold3"]));
        assert_eq!(score.rr, 1.0 / 3.0);
        assert!(!score.hit1 && score.hit5 && score.hit10);
        assert!((score.recall10 - 2.0 / 3.0).abs() < 1e-12);
        let miss = score_task(&files, &owned(&["nowhere"]));
        assert_eq!((miss.rr, miss.hit10, miss.recall10), (0.0, false, 0.0));
        // Only the top 10 count.
        let long: Vec<String> = (0..11).map(|i| format!("f{i}")).collect();
        assert!(!score_task(&long, &owned(&["f10"])).hit10);
    }

    #[test]
    fn lookup_scores_the_top_five() {
        let files = owned(&["a", "gold", "b", "c", "d", "gold2"]);
        let (hit, precision) = score_lookup(&files, &owned(&["gold", "gold2"]));
        assert!(hit);
        assert!((precision - 0.2).abs() < 1e-12);
        assert_eq!(score_lookup(&[], &owned(&["gold"])), (false, 0.0));
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let values = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&values, 50.0), Some(3.0));
        assert_eq!(percentile(&values, 95.0), Some(5.0));
        assert_eq!(percentile(&values, 0.0), Some(1.0));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn bootstrap_is_seeded_and_brackets_the_mean() {
        let deltas: Vec<f64> = (0..40).map(|i| f64::from(i % 5) * 0.1 - 0.1).collect();
        let a = bootstrap(&deltas, 2000, 42).unwrap();
        assert_eq!(a, bootstrap(&deltas, 2000, 42).unwrap());
        assert!(a.low <= a.mean && a.mean <= a.high);
        assert!(a.low > 0.0, "a clearly positive shift excludes zero: {a:?}");
        let ties = bootstrap(&[0.0; 30], 1000, 42).unwrap();
        assert_eq!((ties.low, ties.high), (0.0, 0.0));
        let symmetric: Vec<f64> = (0..60)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        let s = bootstrap(&symmetric, 2000, 42).unwrap();
        assert!(s.low < 0.0 && s.high > 0.0);
        assert_eq!(bootstrap(&[], 10, 1), None);
    }

    #[test]
    fn splitmix_matches_its_reference_sequence() {
        // First outputs of SplitMix64 seeded with 0 (published test vector).
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(rng.next_u64(), 0x6e78_9e6a_a1b9_65f4);
    }
}
