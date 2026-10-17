//! Metric primitives shared by the five gold-set scorers. Every function is
//! pure; ranks are one-based; an empty denominator is documented per function.

/// Fraction of returned entries that are correct. No entries returned scores
/// 0.0 (an empty answer is not a perfect one).
pub fn precision(correct: usize, returned: usize) -> f64 {
    if returned == 0 {
        0.0
    } else {
        correct as f64 / returned as f64
    }
}

/// Fraction of gold elements found. An empty gold set scores 1.0 (nothing was
/// missed).
pub fn recall(found: usize, gold: usize) -> f64 {
    if gold == 0 {
        1.0
    } else {
        found as f64 / gold as f64
    }
}

/// Harmonic mean of precision and recall; 0.0 when both are 0.
pub fn f1(precision: f64, recall: f64) -> f64 {
    if precision + recall <= 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

/// 1.0 when the first relevant answer sits at rank `k` or better.
pub fn hit_at_k(first_rank: Option<usize>, k: usize) -> f64 {
    match first_rank {
        Some(r) if r >= 1 && r <= k => 1.0,
        _ => 0.0,
    }
}

/// Reciprocal rank of the first relevant answer; ranks beyond `cut` score 0.
pub fn reciprocal_rank(first_rank: Option<usize>, cut: usize) -> f64 {
    match first_rank {
        Some(r) if r >= 1 && r <= cut => 1.0 / r as f64,
        _ => 0.0,
    }
}

/// Binary-relevance nDCG@k. `relevance[i]` says whether the answer at rank
/// `i + 1` earns gain; `n_relevant` is the number of gain-earning answers an
/// ideal ranking could contain. 0.0 when `n_relevant` is 0.
pub fn ndcg_at_k(relevance: &[bool], n_relevant: usize, k: usize) -> f64 {
    let gain = |i: usize| 1.0 / ((i + 2) as f64).log2();
    let dcg: f64 = relevance
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, rel)| **rel)
        .map(|(i, _)| gain(i))
        .sum();
    let ideal: f64 = (0..n_relevant.min(k)).map(gain).sum();
    if ideal <= 0.0 { 0.0 } else { dcg / ideal }
}

/// Kendall tau between the gold order and the answer order of the same items
/// (no ties): (concordant - discordant) / pairs. `None` below two items.
/// `answer_order` must be a permutation of `gold_order`.
pub fn kendall_tau<T: PartialEq>(gold_order: &[T], answer_order: &[T]) -> Option<f64> {
    let n = gold_order.len();
    if n < 2 {
        return None;
    }
    let pos = |list: &[T], x: &T| list.iter().position(|y| y == x);
    let (mut concordant, mut discordant) = (0i64, 0i64);
    for i in 0..n {
        for j in (i + 1)..n {
            let (ga, gb) = (i as i64, j as i64);
            let (aa, ab) = (
                pos(answer_order, &gold_order[i])? as i64,
                pos(answer_order, &gold_order[j])? as i64,
            );
            let d = (ga - gb) * (aa - ab);
            if d > 0 {
                concordant += 1;
            } else if d < 0 {
                discordant += 1;
            }
        }
    }
    Some((concordant - discordant) as f64 / (n * (n - 1) / 2) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn precision_counts_returned_entries() {
        assert!(close(precision(3, 4), 0.75));
        assert!(close(precision(0, 0), 0.0));
    }

    #[test]
    fn recall_counts_gold_elements() {
        assert!(close(recall(1, 4), 0.25));
        assert!(close(recall(0, 0), 1.0));
    }

    #[test]
    fn f1_is_the_harmonic_mean() {
        assert!(close(f1(0.5, 1.0), 2.0 / 3.0));
        assert!(close(f1(0.0, 0.0), 0.0));
    }

    #[test]
    fn hit_at_k_respects_the_cutoff() {
        assert!(close(hit_at_k(Some(3), 3), 1.0));
        assert!(close(hit_at_k(Some(4), 3), 0.0));
        assert!(close(hit_at_k(None, 3), 0.0));
    }

    #[test]
    fn reciprocal_rank_is_cut_off() {
        assert!(close(reciprocal_rank(Some(4), 10), 0.25));
        assert!(close(reciprocal_rank(Some(11), 10), 0.0));
        assert!(close(reciprocal_rank(None, 10), 0.0));
    }

    #[test]
    fn ndcg_rewards_early_gain() {
        assert!(close(ndcg_at_k(&[true, true], 2, 10), 1.0));
        let late = ndcg_at_k(&[false, true], 1, 10);
        assert!(close(late, 1.0 / 3f64.log2()));
        assert!(close(ndcg_at_k(&[true], 0, 10), 0.0));
        assert!(close(ndcg_at_k(&[false, false, true], 1, 2), 0.0));
    }

    #[test]
    fn kendall_tau_spans_minus_one_to_one() {
        let gold = ["a", "b", "c", "d"];
        assert!(close(kendall_tau(&gold, &gold).unwrap(), 1.0));
        assert!(close(
            kendall_tau(&gold, &["d", "c", "b", "a"]).unwrap(),
            -1.0
        ));
        assert!(kendall_tau(&["a"], &["a"]).is_none());
    }
}
