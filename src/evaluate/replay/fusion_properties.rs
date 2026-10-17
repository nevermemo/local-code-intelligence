//! N59 (b): fusion invariants of the replay twins. A seeded loop builds random
//! candidate rows and checks `fusion_score`, `fused_order` and `cap_by_file`
//! against the reference that `src/app/search/fusion_properties.rs` checks the
//! search side against (weighted sum in channel order, then the prior; score
//! descending, then path, start line, end line and content hash; a stable
//! partition), and that `verify_dump` accepts the rows a search would dump.

use super::{Scored, cap_by_file, fused_order, fusion_score, verify_dump};
use crate::{
    app::CandidateTrace,
    config::{Config, FusionConfig},
    evaluate::types::CandidateArm,
    filter::SourceRole,
};
use std::{cmp::Ordering, collections::HashSet};

/// SplitMix64: a small deterministic generator, so a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

const PATHS: &[&str] = &["a.rs", "b.rs", "src/c.rs", "tests/d.rs"];
const HASHES: &[&str] = &["h0", "h1", "h22"];
/// Positive weights and `rrf_k`, as `ReplayArm` validation and the
/// configuration require, so every score is finite and non-negative.
const WEIGHTS: &[f32] = &[1.0, 0.5, 2.0, 0.25];
const RRF_KS: &[f32] = &[60.0, 1.0, 7.5];

fn row(path: &str, start_line: u32, content_hash: &str) -> CandidateTrace {
    let role = if path.starts_with("tests/") {
        SourceRole::Test
    } else {
        SourceRole::Source
    };
    CandidateTrace {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line,
        end_line: start_line + content_hash.len() as u32,
        content_hash: content_hash.into(),
        source_role: role,
        semantic_rank: None,
        semantic_score: None,
        lexical_rank: None,
        lexical_score: None,
        lexical_match_count: 0,
        lsp_rank: None,
        prior: crate::store::source_role_prior(role),
        fusion_score: 0.0,
        fused_rank: 0,
        capped_rank: 0,
        shortlisted: false,
        rerank_rank: None,
        reranker_score: None,
        document_bytes: None,
        est_tokens: None,
        snippet: String::new(),
    }
}

/// Distinct chunks, each with random channel ranks; with `aligned`, a channel
/// ranks the chunks it returns in pool order, so scores tie across chunks.
fn rows(rng: &mut Rng) -> Vec<CandidateTrace> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for _ in 0..=rng.below(14) {
        let path = PATHS[rng.below(PATHS.len())];
        let start = [1, 11][rng.below(2)];
        let hash = HASHES[rng.below(HASHES.len())];
        if seen.insert((path, start, hash)) {
            out.push(row(path, start, hash));
        }
    }
    let aligned = rng.below(2) == 0;
    for set in [
        (|r: &mut CandidateTrace, rank| r.semantic_rank = Some(rank))
            as fn(&mut CandidateTrace, usize),
        |r, rank| r.lexical_rank = Some(rank),
        |r, rank| r.lsp_rank = Some(rank),
    ] {
        let mut picked: Vec<usize> = (0..out.len()).filter(|_| rng.below(2) == 0).collect();
        if !aligned {
            rng.shuffle(&mut picked);
        }
        for (position, index) in picked.into_iter().enumerate() {
            set(&mut out[index], position + 1);
        }
    }
    // A chunk reaches the dump only through some channel.
    out.retain(|r| r.semantic_rank.is_some() || r.lexical_rank.is_some() || r.lsp_rank.is_some());
    out
}

fn reference_score(rrf_k: f32, weights: &FusionConfig, row: &CandidateTrace) -> f32 {
    let mut score = 0.0_f32;
    for (weight, rank) in [
        (weights.semantic_weight, row.semantic_rank),
        (weights.lexical_weight, row.lexical_rank),
        (weights.lsp_weight, row.lsp_rank),
    ] {
        if let Some(rank) = rank {
            score += weight / (rrf_k + rank as f32);
        }
    }
    score + row.prior
}

fn reference_order(a: &(f32, &CandidateTrace), b: &(f32, &CandidateTrace)) -> Ordering {
    let key = |(score, row): &(f32, &CandidateTrace)| {
        (
            std::cmp::Reverse(score.to_bits()),
            row.relative_file_path.clone(),
            row.start_line,
            row.end_line,
            row.content_hash.clone(),
        )
    };
    // Scores are finite and non-negative (see `WEIGHTS`), where the bit
    // order is the numeric order.
    key(a).cmp(&key(b))
}

fn reference_cap(paths: &[&str], max: u32) -> Vec<usize> {
    let fits: Vec<bool> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            max == 0 || paths[..index].iter().filter(|p| *p == path).count() < max as usize
        })
        .collect();
    let head = (0..paths.len()).filter(|&index| fits[index]);
    let tail = (0..paths.len()).filter(|&index| !fits[index]);
    head.chain(tail).collect()
}

fn identity(item: &Scored<'_>) -> (String, u32, u32, String, u32) {
    (
        item.row.relative_file_path.clone(),
        item.row.start_line,
        item.row.end_line,
        item.row.content_hash.clone(),
        item.score.to_bits(),
    )
}

#[test]
fn replay_fusion_matches_the_reference_for_random_rows() {
    const SEEDS: u64 = 3000;
    // Equal-score neighbours with one path and start line (the N53 shape).
    let (mut same_place_ties, mut demoted) = (0, 0);
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        let mut rows = rows(&mut rng);
        let rrf_k = RRF_KS[rng.below(RRF_KS.len())];
        let weights = FusionConfig {
            semantic_weight: WEIGHTS[rng.below(WEIGHTS.len())],
            lexical_weight: WEIGHTS[rng.below(WEIGHTS.len())],
            lsp_weight: WEIGHTS[rng.below(WEIGHTS.len())],
            max_chunks_per_file: rng.below(4) as u32,
        };
        let context = format!("seed {seed}, rrf_k {rrf_k}, weights {weights:?}");

        // The score is the weighted sum, bit for bit.
        for row in &rows {
            assert_eq!(
                fusion_score(row, rrf_k, &weights, row.prior).to_bits(),
                reference_score(rrf_k, &weights, row).to_bits(),
                "{context}"
            );
        }

        // The fused order is the reference order from any input order.
        let mut expected: Vec<(f32, &CandidateTrace)> = rows
            .iter()
            .map(|row| (reference_score(rrf_k, &weights, row), row))
            .collect();
        expected.sort_by(reference_order);
        let expected: Vec<_> = expected
            .iter()
            .map(|&(score, row)| identity(&Scored { row, score }))
            .collect();
        for _ in 0..3 {
            let mut pool: Vec<Scored<'_>> = rows
                .iter()
                .map(|row| Scored {
                    row,
                    score: fusion_score(row, rrf_k, &weights, row.prior),
                })
                .collect();
            rng.shuffle(&mut pool);
            pool.sort_by(fused_order);
            assert_eq!(
                pool.iter().map(identity).collect::<Vec<_>>(),
                expected,
                "{context}"
            );
        }
        same_place_ties += expected
            .windows(2)
            .filter(|pair| {
                pair[0].4 == pair[1].4 && pair[0].0 == pair[1].0 && pair[0].1 == pair[1].1
            })
            .count();

        // The cap is the reference's stable partition.
        let max = weights.max_chunks_per_file;
        let paths: Vec<&str> = expected.iter().map(|item| item.0.as_str()).collect();
        let order = reference_cap(&paths, max);
        demoted += order.iter().enumerate().filter(|(p, i)| p != *i).count();
        assert_eq!(
            cap_by_file(expected.clone(), max, |item| item.0.as_str()),
            order
                .iter()
                .map(|&i| expected[i].clone())
                .collect::<Vec<_>>(),
            "{context}: cap {max}"
        );

        // A dump written the way a search writes it (fused order, the
        // reference cap's positions, the shortlist cut) verifies.
        rows.sort_by(|a, b| {
            reference_order(
                &(reference_score(rrf_k, &weights, a), a),
                &(reference_score(rrf_k, &weights, b), b),
            )
        });
        let shortlist = rng.below(rows.len() + 2);
        for (index, row) in rows.iter_mut().enumerate() {
            row.fusion_score = reference_score(rrf_k, &weights, row);
            row.fused_rank = index + 1;
            row.capped_rank = order.iter().position(|&i| i == index).unwrap() + 1;
            row.shortlisted = row.capped_rank <= shortlist;
        }
        let mut arm = CandidateArm::new(&Config::default(), 10);
        arm.rrf_k = rrf_k;
        arm.fusion = weights;
        arm.rerank_candidate_count = shortlist;
        if let Err(refusal) = verify_dump(&rows, &arm) {
            panic!("{context}: refused: {}", refusal.detail);
        }
    }
    assert!(
        same_place_ties > 0 && demoted > 0,
        "same-place ties {same_place_ties}, demoted {demoted}"
    );
}
