# N2 results: does an in-request edge bandit help the walk? (2026-10-16)

Study of the in-request edge bandit (`next-slices.md`, N2). Inside one request, a reranker rates a few newly found
nodes every few expansions, a tracker counts hits per kind of edge, and the walk shifts queue priority toward the kinds
that paid. Nothing carries over between requests. The code is on this branch behind `[traverse] edge_bandit` (default
off; with it off the walk, the surface and the golden snapshot are unchanged). Everything below ran on the frozen tree
(bfaff44) and the S8j split, with the pre-registration frozen before any number existed and three dated amendments
written before the TEST items ran.

## Verdict

- **Formal outcome (pre-registered power rule): "+.03 is not detectable at the available n; no verdict".** The kill
  rule's gain clause could not be tested, so no formal kill was applied.
- **Coordinator decision: N2 is stopped and the bandit stays off.** Every confidence interval of A1 minus F-append has
  an upper bound far below the +.03 target (flow@10 [0, 0], flow@20 upper bound +0.001, where-to@20 upper bound
  +0.003), so a +.03 gain is excluded by the intervals even though the power rule forbids a formal test. The no-loss
  clause also fails literally on flow at K=20 (point estimate -0.025 against a limit of -0.02).
- N2b (a bandit driven by the judge's verdicts) is **not triggered**: N0b's judge gave no gain to build on.

## Design

- Arms, all on F-append (`first_stage = "append"`, `first_k = 10`, `seed_k = 8`, exact seeds, top-k 10), identical except
  for the bandit flags:

| arm | what |
| --- | --- |
| A0 | F-append, bandit off |
| A1 | bandit, rerank signal: every 3 expansions rate at most 4 new nodes, score >= 0.5 is a hit, rate 1.0, weight clamped to 0.5..2, smoothing `(hits + 1) / (tries + 2)` |
| A2 | cost-matched fixed reweighting: per-kind weights learned from DEV and frozen (no model calls) |
| A3 | cost-matched random reweighting: seeded per-kind weights in 0.5..2, mean over 3 seeds (no model calls) |
| A4 | the bar: the winning N0b configuration, which is F-append with no judge, i.e. A0 |

- Hit signal: the frozen reranker, through the bandit's own rescorer, independent of the walk's score and of
  `rerank_frontier` (off). The code also has a `topk` signal ("would place in the current top k"); it rewards the
  priors it reweights, so it is a diagnostic only and not an arm.
- Sets: only where N0b keeps the walk. After the N0b verdict that is flow (K=10 and K=20) and where-to (K=20). Impact
  and history were out. Primaries: flow `path_recall`, where-to `set_f1`.
- Items: S8j TEST, flow 50 (all defined), where-to 34 (20 with a defined `set_f1`). DEV was used for the fixed table
  and the variance only; pooling DEV and TEST was not allowed.
- Kill rule (frozen, from `next-slices.md`): gain of at least +.03 on flow path recall or impact second-ring recall
  with a Holm-adjusted paired interval excluding zero, no set losing more than .02 on its primary, physical cost
  (expansions, model calls, tokens) within 1.5x of F-append. Power first: if +.03 is not detectable, report that.
- Statistics: paired by item, 95% percentile bootstrap, 10000 resamples. Holm p-values over the three reported cells
  are descriptive.
- Cost accounting (coordinator decision before any number): F-append makes no model calls inside the walk, so 1.5 x 0
  is unsatisfiable. Model calls are judged as reranker documents per item end to end (first-stage 24 + bandit +
  frontier), limit 1.5 x 24. The literal reading is reported and does not gate.

## DEV phase and diagnosis

The DEV split gave the fixed table and the power check. At the code defaults the signal was nearly dead: 18 hits in
617 tries on flow (3%).

- Power step 2 (A1 minus A0 on DEV, SD inflated to its one-sided 80% upper bound): flow `path_recall` SD 0.18, so at
  TEST n = 50 the detectable difference is 0.079 (Holm) or 0.072 (unadjusted), well above .03: **not testable**. Impact
  `r2_decl_recall` was testable (MDE 0.004) but left the kept sets with the N0b verdict. So no gain metric could be
  tested.
- Amendment 2, a threshold x sample sweep on DEV flow and impact (threshold 0.5, 0.3, 0.2, 0.1; sample 4 and 8). The
  hit rate rose from 3% to 13%, but no configuration reached +.02 on either gain metric (flow deltas between -0.021 and
  +0.001, impact +0.001 everywhere), and every sample-8 configuration broke the 1.5x document limit. Recorded as "no
  usable DEV signal"; the code defaults stayed frozen for TEST.
- A diagnosis of why the same ten flow items changed in every configuration (DEV only, no code changed). The bandit
  is not inert and not broken; it is not learning from hits:
  - **The rater sees names only.** The rescorer's document is the file path and the symbol name, no body, against a
    multi-hop question, so scores sit below the thresholds. 48 of 61 DEV flow walks had zero hits in the whole walk.
  - **Low hit rates, so the weights follow try counts.** A kind's rate is `(hits + 1) / (tries + 2)` and the mean is
    taken over the tried kinds only. An untried kind has rate 1/2, a tried kind with no hits has less, so with zero
    hits everywhere the kind with the fewest tries gets the largest weight (often the 2.0 clamp). The first
    non-neutral weight was 2.0 in 26 of 57 items. That is a novelty ordering, not a learned one. With the rate set to
    0 (weights all 1, rating rounds still running) the result lists and pop order equalled A0 on 9 of 9 control
    items, which rules out side effects of the rating rounds and of the flag plumbing.
  - **Few rounds, late.** Walks average about 9.4 expansions; the first rating round fires after expansion 3, so the
    first weighted hops come at expansion 4 or later, with 12 rated nodes at most.
  - **Answers are decided by ties.** In all 61 A0 and all 61 A1 DEV flow walks the stop was the score bound with the k-th
    result tied with the best frontier entry, so the top 10 is "the first 10 tied nodes discovered". Re-ordering the
    queue only picks a different slice of the tie, which moved path recall both ways (6 items down, 4 up).
  - Where hits did occur, the walk moved back toward A0.

## TEST results

All 12 + 8 bench runs exited 0; no item was flagged (no deadline, no rerank or bandit failure).

Arm means (n = items with the primary defined in all arms; A3 is the mean of three seeds):

| cell | n | A0 | A1 | A2 | A3 |
| --- | --- | --- | --- | --- | --- |
| flow@10 `path_recall` | 50 | 0.3235 | 0.3235 | 0.3302 | 0.3213 |
| flow@20 `path_recall` | 50 | 0.4919 | 0.4672 | 0.4772 | 0.4564 |
| where-to@20 `set_f1` | 20 | 0.3453 | 0.3439 | 0.3494 | 0.3513 |

Paired differences, 95% intervals (items up / down / level):

| comparison | cell | mean delta | 95% CI | up/down/level |
| --- | --- | --- | --- | --- |
| A1 - A0 | flow@10 | +0.0000 | [0.0000, 0.0000] | 0/0/50 |
| A1 - A0 | flow@20 | -0.0247 | [-0.0547, +0.0010] | 1/5/44 |
| A1 - A0 | where-to@20 | -0.0015 | [-0.0077, +0.0033] | 1/1/18 |
| A1 - A2 (learning vs frozen table) | flow@20 | -0.0100 | [-0.0450, +0.0250] | 2/4/44 |
| A1 - A3 (learning vs random) | flow@20 | +0.0108 | [-0.0277, +0.0532] | 10/7/33 |
| A2 - A0 | flow@20 | -0.0147 | [-0.0523, +0.0237] | 3/6/41 |
| A3 - A0 | flow@20 | -0.0354 | [-0.0747, -0.0013] | 2/12/36 |
| A3 - A0 | where-to@20 | +0.0059 | [+0.0013, +0.0118] | 5/0/15 |

- At K=10 A1 equals A0 on every flow item and every where-to item by construction (exact answers and seeds come first
  in the list), so those cells carry no information. K=20 equals the uncut run.
- The only interval that excludes zero on the favorable side is random reweighting on where-to@20 (+0.006, a
  non-gain metric); on flow no arm beats A0 and no interval excludes zero favorably. A1 is never significantly better
  than the fixed table or the random reweighting on any cell.
- Rating signal on TEST: hit rate 6.4% on flow (37 of 579 tries), 12.8% on where-to (49 of 383). On where-to the kind order
  flipped between DEV and TEST: DEV gave `calls_or_refs` the lowest weight, TEST had it with the highest hit rate (17%
  against 10% for `contains`), so the ranking of kinds is not stable at these counts.
- Trend row (flow, reported not gated): the K=20 loss sits in the middle bins (2 to 3 rounds: -0.043; pops 8 or fewer:
  -0.039); walks with 4 or more rounds or over 12 pops show no change. No pattern that rounds help.
- Cost, A1 against A0 (per-item means): expansions 1.06x (flow) and 0.97x (where-to); tokens 1.03x and 1.02x; reranker
  documents end to end 35.6 against 24 (1.48x) and 35.3 against 24 (1.47x). Within 1.5x on every component, narrowly
  on documents. Walk-only model calls: A0 0, A1 about 3.1 (the literal reading is unsatisfiable and does not gate).
- Attribution label (reported, not a gate): "reweighting effect" by the label's letter, driven only by the where-to
  cell above; reweighting alone moves answers, learning adds nothing measurable.

## Caveats

- **Cost matching was not achieved cleanly.** The first A2 and A3 runs fell outside 0.9x to 1.1x of A1's expansions on
  both sets. Per the pre-registration they were re-run once with `max_pops` set to A1's mean (11). The cap cut the
  longer walks, so the re-runs sit below A1 (flow A2 0.78x, A3 0.85x; where-to about 0.9x), and the comparison "bandit
  against baselines at matched expansions" is only approximate. The cost favors the baselines on documents (they make
  no model calls) but not on expansions. The sensitivity is real: with the first, uncapped A3 (1.58x expansions on one
  seed) the A1 minus A3 difference on flow@20 flips sign (-0.032, Holm p 0.008) against the capped run (+0.011, n.s.);
  extra expansions alone move flow@20 by about 0.03.
- **Serving drift.** A0 is this run's re-run, not the earlier F-append raw outputs: services moved between days and two
  of 61 DEV flow items differed in seed order at ranks 3 to 7 with no metric change. Arms are compared only within the
  run; the old binary run today also gives the new output, so the difference is not the code.
- **Bench preflight bug.** The bench passed the `--edge-bandit` and `--bandit-*` flags on its preflight probe calls,
  where the rerank signal cannot start ("needs the application"), so every map was marked unavailable and the run
  would have produced nothing. The workaround was a thin wrapper binary (`lci-wrap`) that drops the bandit flags on
  probe calls (calls with `--seeds=` or a `--maps` list without `seeds.hybrid`) and forwards the per-item walks
  unchanged; every bandit arm ran through it (A0 ran the binary directly). The bench was a debug build. The wrapper
  is not part of the repository; the bench's probe call should not receive the flags.
- The yield-based stop (by-product) was not evaluated: A1 fails the no-loss clause, and the pre-registration asks for
  it only if A1 is not killed on loss.
- Not shown: that a kind-learning signal cannot pay. This test fed the bandit a near-constant zero signal, so what it
  measured was try-count novelty plus tie-order luck. Not varied: rating text, the untried-kind bonus, rounds earlier
  than expansion 4, and the metric's tie handling.

## Consequences

- The bandit code stays in the tree behind `edge_bandit` (default off, golden snapshot unchanged) as a measured
  negative; nothing ships on by default. No tool description changes.
- If N2 is ever revisited it is a redesign, not a fix: rate real node text (a symbol body or chunk) so hit rates reach
  20 to 50%, drop the untried-kind bonus so zero-hit rounds leave weights at 1, and use a tie-aware metric (for
  example rank by potential, then by reranker score) before any recall difference can be read.
- Since the walk's stop is a bound tie, any ordering change reads as noise on path recall at this n; the same holds
  for the other order-only ideas. That points at the stop rule and the tie-break, not at edge kinds.
