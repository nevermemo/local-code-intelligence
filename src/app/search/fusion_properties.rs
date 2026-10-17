//! N59 (b): fusion invariants of the search side. A seeded loop builds random
//! channel lists (with chunks that share a path and a start line, equal ranks
//! across channels, and zero weights) and checks `App::fuse_hits`, the fused
//! order, the per-file cap and the candidate trace against a reference written
//! out below. `src/evaluate/replay/fusion_properties.rs` checks the replay
//! twins (`fusion_score`, `fused_order`, `cap_by_file`) against the same
//! reference, so the two sides agree through it.

use super::{App, cap_chunks_per_file, fusion_order, trace_candidates};
use crate::{
    chunk::Chunk,
    config::FusionConfig,
    filter::SourceRole,
    store::{Hit, source_role_prior},
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
/// Positive weights and `rrf_k`, as configuration validation requires, so
/// every score is finite and non-negative.
const WEIGHTS: &[f32] = &[1.0, 0.5, 2.0, 0.25];
const RRF_KS: &[f32] = &[60.0, 1.0, 7.5];

type Key = (String, u32, String);

fn key(hit: &Hit) -> Key {
    (
        hit.chunk.relative_file_path.clone(),
        hit.chunk.start_line,
        hit.chunk.content_hash.clone(),
    )
}

/// A chunk; its end line follows from its content (here the hash), as for a
/// real chunk, so two chunks with one key never differ in their end line.
fn chunk_hit(path: &str, start_line: u32, content_hash: &str) -> Hit {
    let role = if path.starts_with("tests/") {
        SourceRole::Test
    } else {
        SourceRole::Source
    };
    Hit {
        file_path: format!("/workspace/{path}"),
        chunk: Chunk {
            relative_file_path: path.into(),
            language: "rust".into(),
            start_line,
            end_line: start_line + content_hash.len() as u32,
            code: format!("fn {content_hash}() {{}}"),
            content_hash: content_hash.into(),
        },
        semantic_score: None,
        semantic_rank: None,
        lexical_rank: None,
        lexical_match_count: 0,
        lexical_score: None,
        lsp_rank: None,
        fusion_score: 0.0,
        retrieval_channels: vec![],
        reranker_score: None,
        source_role: role,
        source_role_prior: source_role_prior(role),
    }
}

/// Distinct chunks: every path, start line 1 or 11, and a content hash, so
/// several chunks share a path and a start line (the N53 shape).
fn chunks(rng: &mut Rng) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for _ in 0..=rng.below(14) {
        let path = PATHS[rng.below(PATHS.len())];
        let start = [1, 11][rng.below(2)];
        let hash = HASHES[rng.below(HASHES.len())];
        if seen.insert((path, start, hash)) {
            out.push(chunk_hit(path, start, hash));
        }
    }
    out
}

/// One channel: a random subset of the chunks with ranks `1..=n`. With
/// `aligned`, a chunk's rank is its position in `pool`, so different channels
/// give equal ranks to different chunks and scores tie.
fn channel(rng: &mut Rng, pool: &[Hit], aligned: bool, tag: fn(&mut Hit, usize)) -> Vec<Hit> {
    let mut picked: Vec<usize> = (0..pool.len()).filter(|_| rng.below(2) == 0).collect();
    if !aligned {
        rng.shuffle(&mut picked);
    }
    picked
        .into_iter()
        .enumerate()
        .map(|(position, index)| {
            let mut hit = pool[index].clone();
            tag(&mut hit, position + 1);
            hit
        })
        .collect()
}

/// The fusion score written out: each channel's `weight / (rrf_k + rank)` in
/// the order semantic, lexical, language server, then the prior.
fn reference_score(rrf_k: f32, weights: &FusionConfig, hit: &Hit) -> f32 {
    let mut score = 0.0_f32;
    for (weight, rank) in [
        (weights.semantic_weight, hit.semantic_rank),
        (weights.lexical_weight, hit.lexical_rank),
        (weights.lsp_weight, hit.lsp_rank),
    ] {
        if let Some(rank) = rank {
            score += weight / (rrf_k + rank as f32);
        }
    }
    score + hit.source_role_prior
}

/// The cap written out: the items among the first `max` of their file, in
/// order, then the rest, in order; `max == 0` keeps everything in place.
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

/// The fused order written out: score descending, then path, start line, end
/// line and content hash. For finite non-negative scores the bit order is the
/// numeric order.
fn reference_order(a: &Hit, b: &Hit) -> Ordering {
    let order_key = |hit: &Hit| {
        (
            std::cmp::Reverse(hit.fusion_score.to_bits()),
            hit.chunk.relative_file_path.clone(),
            hit.chunk.start_line,
            hit.chunk.end_line,
            hit.chunk.content_hash.clone(),
        )
    };
    order_key(a).cmp(&order_key(b))
}

/// What identifies a fused hit and its score, for comparing orders.
fn identity(hit: &Hit) -> (Key, u32, u32) {
    (key(hit), hit.chunk.end_line, hit.fusion_score.to_bits())
}

#[test]
fn fusion_keys_scores_order_and_cap_hold_for_random_channels() {
    const SEEDS: u64 = 3000;
    // `same_place_ties`: equal-score neighbours with one path and start line
    // (the N53 shape), which only the end line and content hash order.
    let (mut same_place_ties, mut merged, mut demoted) = (0, 0, 0);
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        let pool = chunks(&mut rng);
        let aligned = rng.below(2) == 0;
        let semantic = channel(&mut rng, &pool, aligned, |hit, rank| {
            hit.semantic_rank = Some(rank);
            hit.semantic_score = Some(1.0 / rank as f32);
            hit.retrieval_channels.push("semantic".into());
        });
        let lexical = channel(&mut rng, &pool, aligned, |hit, rank| {
            hit.lexical_rank = Some(rank);
            hit.lexical_match_count = 1;
            hit.lexical_score = Some(2.0 / rank as f32);
            hit.retrieval_channels.push("lexical".into());
        });
        let lsp = channel(&mut rng, &pool, aligned, |hit, rank| {
            hit.lsp_rank = Some(rank);
            hit.retrieval_channels.push("lsp".into());
        });
        let rrf_k = RRF_KS[rng.below(RRF_KS.len())];
        let weights = FusionConfig {
            semantic_weight: WEIGHTS[rng.below(WEIGHTS.len())],
            lexical_weight: WEIGHTS[rng.below(WEIGHTS.len())],
            lsp_weight: WEIGHTS[rng.below(WEIGHTS.len())],
            max_chunks_per_file: rng.below(4) as u32,
        };
        let context = format!("seed {seed}, rrf_k {rrf_k}, weights {weights:?}");

        let mut fused = App::fuse_hits(
            rrf_k,
            &weights,
            semantic.clone(),
            lexical.clone(),
            lsp.clone(),
        );

        // Unique keys, and exactly the chunks some channel returned.
        let keys: Vec<Key> = fused.iter().map(key).collect();
        let unique: HashSet<&Key> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "{context}: duplicate key");
        let expected: HashSet<Key> = semantic
            .iter()
            .chain(&lexical)
            .chain(&lsp)
            .map(key)
            .collect();
        assert_eq!(unique, expected.iter().collect::<HashSet<_>>(), "{context}");

        // Every channel's rank is carried over, and the score is the weighted
        // sum, bit for bit.
        for hit in &fused {
            for (list, rank_of) in [
                (
                    &semantic,
                    (|h: &Hit| h.semantic_rank) as fn(&Hit) -> Option<usize>,
                ),
                (&lexical, |h: &Hit| h.lexical_rank),
                (&lsp, |h: &Hit| h.lsp_rank),
            ] {
                let input = list.iter().find(|h| key(h) == key(hit)).and_then(rank_of);
                assert_eq!(rank_of(hit), input, "{context}: {:?}", key(hit));
            }
            let channels: Vec<&str> = [
                ("semantic", hit.semantic_rank),
                ("lexical", hit.lexical_rank),
                ("lsp", hit.lsp_rank),
            ]
            .into_iter()
            .filter_map(|(name, rank)| rank.map(|_| name))
            .collect();
            assert_eq!(
                hit.retrieval_channels,
                channels,
                "{context}: {:?}",
                key(hit)
            );
            merged += usize::from(channels.len() > 1);
            assert_eq!(
                hit.fusion_score.to_bits(),
                reference_score(rrf_k, &weights, hit).to_bits(),
                "{context}: {:?} scored {} not {}",
                key(hit),
                hit.fusion_score,
                reference_score(rrf_k, &weights, hit)
            );
        }

        // The fused order is the reference order, total, and does not depend
        // on the input order.
        let mut expected_order = fused.clone();
        expected_order.sort_by(reference_order);
        fused.sort_by(fusion_order);
        assert_eq!(
            fused.iter().map(identity).collect::<Vec<_>>(),
            expected_order.iter().map(identity).collect::<Vec<_>>(),
            "{context}"
        );
        for pair in fused.windows(2) {
            assert_eq!(
                fusion_order(&pair[0], &pair[1]),
                Ordering::Less,
                "{context}: {:?} and {:?}",
                key(&pair[0]),
                key(&pair[1])
            );
            same_place_ties += usize::from(
                pair[0].fusion_score == pair[1].fusion_score
                    && pair[0].chunk.relative_file_path == pair[1].chunk.relative_file_path
                    && pair[0].chunk.start_line == pair[1].chunk.start_line,
            );
        }
        let (mut semantic, mut lexical, mut lsp) = (semantic, lexical, lsp);
        rng.shuffle(&mut semantic);
        rng.shuffle(&mut lexical);
        rng.shuffle(&mut lsp);
        let mut permuted = App::fuse_hits(rrf_k, &weights, semantic, lexical, lsp);
        permuted.sort_by(fusion_order);
        assert_eq!(
            fused.iter().map(identity).collect::<Vec<_>>(),
            permuted.iter().map(identity).collect::<Vec<_>>(),
            "{context}: the order depends on the input order"
        );

        // The cap is the stable partition of the reference, and the trace
        // records the same positions.
        let paths: Vec<&str> = fused
            .iter()
            .map(|hit| hit.chunk.relative_file_path.as_str())
            .collect();
        let max = weights.max_chunks_per_file;
        let order = reference_cap(&paths, max);
        demoted += order.iter().enumerate().filter(|(p, i)| p != *i).count();
        let capped = cap_chunks_per_file(fused.clone(), max);
        assert_eq!(
            capped.iter().map(identity).collect::<Vec<_>>(),
            order
                .iter()
                .map(|&i| identity(&fused[i]))
                .collect::<Vec<_>>(),
            "{context}: cap {max}"
        );
        let shortlist = rng.below(fused.len() + 2);
        let trace = trace_candidates(&fused, max, shortlist, 64);
        for (index, row) in trace.iter().enumerate() {
            let capped_rank = order.iter().position(|&i| i == index).unwrap() + 1;
            assert_eq!(row.fused_rank, index + 1, "{context}");
            assert_eq!(row.capped_rank, capped_rank, "{context}");
            assert_eq!(row.shortlisted, capped_rank <= shortlist, "{context}");
            assert_eq!(
                row.fusion_score.to_bits(),
                fused[index].fusion_score.to_bits(),
                "{context}"
            );
        }
    }
    assert!(
        same_place_ties > 0 && merged > 0 && demoted > 0,
        "same-place ties {same_place_ties}, merged {merged}, demoted {demoted}"
    );
}
