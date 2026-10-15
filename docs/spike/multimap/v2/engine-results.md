# Spike v2 engine results (2026-10-15)

Benches 1017 to 1020 on the frozen eval tree (`bfaff44`), debug builds of
`spike/v2`, hybrid seeds, the four default maps (seeds.hybrid,
seeds.scip_names, graph.scip, structure.contains) unless a row says more.
Every row is in `trend.md` with its wave label. Speed is judged by work
counts (pops, hops, reranker calls), not wall time: the benches ran four
sets at once.

## Method notes

- The traverse deadline is 60000 ms in every row here. Bench-1016 used the
  5 s default and, under parallel load, cut items silently (where-to 4,
  flow 5); those rows understate and are superseded by the repeats. Each row
  note now lists its stop reasons (`stops: bound=N deadline=N ...`).
- Learned-prior rows run on the held-out items only (30% of the ids, 131),
  fitted on the bench-1016 traces of the other 70%. The guessed control is a
  table with the same held-out ids and no prior values, so every hop falls
  back to the guess on exactly the same items.
- The held-out sets are small (where-to has 12 registration items there).
  Choosing a prior per set from these rows would select on the holdout; any
  such choice needs a fresh split before it counts.

## Score bound (Q1, B1)

- With the bound off, where-to and flow reach similar aggregate quality at
  15 to 20 times the pops (where-to reg_mrr .687 to .680; flow path_recall
  .328 to .324 and tau .933 to .872, over 30 and 26 items); impact
  decl_recall rose .067 to .079.
- B1 made the bound also wait for results below the top k that a later
  arrival could corroborate past the k-th. It changed no quality on impact,
  where-to or flow (pops equal, hops within 0.2%); on history with
  git.history it costs 5.7 times the pops (646 to 3695 on the holdout) with
  the same mrr.
- Diagnosis of the impact gap: in all 73 items whose top 10 differ between
  the bound on and off, the differing results are tied at the k-th ranked
  score (1.0 or 0.5). By ranked potential the bound misses nothing; by the
  gold it does (decl_recall .067 against .079), because longer walks fill
  the tie slots differently. Ranked potentials are products of a few
  discrete priors, so ties are common and their order (discovery order) is
  arbitrary. This is what Q5 (a fitted tie-break) addresses.

## Learned hop priors (S6f), held-out items

| set | guessed | learned |
| --- | --- | --- |
| history (+ git.history) | mrr .086, hit@10 .138, 9/29 non-empty | mrr .222, hit@10 .379, 18/29; pops 784 to 646 |
| impact | decl_f1 .036, file_f1 .354 | decl_f1 .046, file_f1 .479 |
| impact (+ scip.impact) | f1 .130, precision .111, recall .186 | f1 .152, precision .185, recall .153 |
| flow | path_recall .403 | .403 |
| where-to | reg_hit@3 .917, reg_mrr .722 | .750, .644 |

The normalised tables: impact prices `defined_in` at 1.0 and graph calls at
.088; history keeps calls at 1.0 with git touched .83 and cochange .76.

## Reranker frontier (W0c2b)

One batched reranker call per expansion rates the kept destinations (file
path, kind and name of a SCIP node; the path of a file; the id of any other
node; at most 1000 characters); each kept hop is multiplied by
`floor + (1 - floor) * s` with floor 0.25, at most 1, so the bound stays
sound.

All items, guessed priors (bench-1020 RR rows against bench-1019 B1 rows):

| set | without reranker | with reranker | pops |
| --- | --- | --- | --- |
| where-to | reg_mrr .687, set_f1 .242, precision .333 | .713, .304, .374 (reg_hit@3 .840 both) | 672 to 1003 |
| impact | decl_recall .067, decl_f1 .053, file_f1 .379, precision .052 | .079, .065, .405, .066 | 1593 to 2399 |
| impact (+ scip.impact) | decl_f1 .140, precision .129, ndcg@10 .211 | .155, .144, .187 | 1443 to 2399 |
| flow | path_recall .328, tau .933, precision .138 | .291, .333, .168 | 1073 to 1889 |

Held-out items, learned priors with and without the reranker:

| set | learned | learned + reranker |
| --- | --- | --- |
| where-to | reg_hit@3 .750, reg_mrr .644, precision .342 | .833, .799, .482 |
| impact | decl_recall .051, decl_f1 .046 | .069, .055 |
| flow | path_recall .403, tau 1.000 | .364, .667 (tau over 6 items) |

The reranker helps where-to and impact and hurts flow: it ranks by relevance
to the question, which scrambles the order along a call path (tau falls from
.933 to .333, though tau is defined on 15 items instead of 30) and loses path recall, though precision rises. On where-to it
also repairs the learned priors' loss. It costs 16 calls per item on average
(about 90 documents per impact item, no failures) and 50 to 76% more pops.
On history (with git.history) the reranker is the largest gain so far: over
all 110 items mrr .128 to .230, hit@10 .191 to .382, precision .063 to .102,
and fewer pops (16734 to 9733); with learned priors on the held-out items,
mrr .222 to .227 and hit@10 .379 to .517. These history rows are not clean
estimates: 52 of 110 reranker walks (9 of 29 with learned priors) stopped
on the 60 s deadline, against none in the control, so the pop count is cut
by the clock, not saved, and the quality is that of a deadline-limited,
partly failed policy. It is also the most expensive arm: 80 reranker calls
and about 670 rated documents per item, and 50 of 8825 calls failed while
four sets shared one reranker. History walks are also not reproducible
under load (git calls time out), so two arms that should walk alike can
differ (Q5 below).

## Fitted tie-break (Q5)

Pools of 40 results per item (bench-1021 Q5-pool, scored on the top 10) gave
the fit its tied pairs; weights were fitted per set on the train ids only
(pairwise logistic regression over results tied on ranked_potential). Share
of held-out tied pairs with the gold result first: impact 58.4% by discovery
order to 74.5% with the tie-break, where-to 78.4% to 80.6%, history 81.0% to
86.3%, flow 75.3% to 68.6% (worse). Discovery order is already a fair
tie-break.

At k 10 on the held-out items (guessed priors, Q5-none against Q5-tb):

| set | without | with the tie-break |
| --- | --- | --- |
| history (+ git.history) | mrr .086, hit@10 .138, precision .030 | .147, .241, .082 |
| impact | decl_recall .046, decl_f1 .036 | .043, .033 |
| where-to | reg_hit@3 .917, reg_mrr .722, set_f1 .300 | .833, .653, .348 |
| flow | path_recall .403, tau 1.000 | .403, .600 |

The pair gain on impact does not reach the top 10: at k 10 the bound ends the
walk before most tied candidates are found, so the tie-break can only reorder
the few that are. It stays opt-in; history is the only set where it helps, and
that choice was made on the holdout. The history pair also walked differently
(pops 4626 against 4506) although the tie-break did not change the walk then:
the git calls depend on load, so the history gain is not tie-break-only.

The bound at that commit was not sound for the final order with a tie-break:
it stopped while a state that could tie the k-th result, and win the tie, was
still reachable. The review fix 19e84c4 makes the comparison strict when a tie-break
is in use. Its cost (bench-1022, release build, held-out impact items): pops
rise from 515 to 7303 and 30 of 44 walks end on the pop budget instead of
the bound, with the same quality (decl_recall .046 against .045).

## Fusion and pass-through (Q2, Q4)

- RRF fusion (bench-1016, rows H1c-rrf in `trend.md`) is worse than the
  corroboration boost on every set, also against the bound-off boost walk of
  the same pops (where-to reg_hit@3 .480 against .840, impact decl_recall
  .056 against .079, flow path_recall .285 against .324); RRF switches the
  bound off. This is one RRF construction (fused over map ranks); the default
  stays `boost`.
- Pass-through of connective nodes changes no quality and cuts 19% of the
  history pops; pass-through pops were not counted then, so this is not yet
  a saving in total expansions (bench rows now report them). The default
  stays off.

## Boundary review (2026-10-15)

Two independent read-only reviews (Fable; GPT-6 Astra on the public tree)
found, and the fixes 19e84c4 and 6ffe410 address: the bound with a tie-break (above); a resumed walk
whose maps changed could undercount the corroboration a state can still gain;
prior tables accepted negative values, and a pair missing from a fitted table
fell back to the guess on the unfitted scale. 6ffe410 adds a salted, commit-grouped
hold-out split, an `UNRELIABLE` marker on rows with deadline stops or call
failures, total expansions, and `maps compare` (paired per-item deltas with a
bootstrap interval). Every comparison above used the same held-out ids, so
they are development data; preset choices wait for a fresh split.

The impact `grep_baseline` of M0b (decl_recall .988, stored per item by
`cargo xtask gold baseline`) is far above every engine row here, but it is
not a same-size competitor: it is handed the gold seed name
(`m1.4-prereg.md`, arm G) and returns every enclosing declaration with no
cap, while the engine is cut to 10. The nearer comparison is M1.4 arm N
(anchors from the question's names, then the exact closure) capped at the
same k; that row is planned. The impact gold is built from the same SCIP
closure, so N reproduces it nearly by construction (`m1.4-results.md`): a
typed impact query should route to the closure, and the walk's value on
impact is for questions that name no symbol.

## Seeds, one hop and the steering ceiling (bench-1022)

Release build of `40e495f`, all items, guessed priors, two sets at once, no
deadline stops or call failures in any row (waves `W-*` in `trend.md`).
Paired deltas are `maps compare` with a 95% bootstrap interval.

| set | walk (W-plain) | search only (W-search) | one hop + seeds (W-onehop) | oracle steer (W-oracle) |
| --- | --- | --- | --- | --- |
| where-to | reg_mrr .687, set_f1 .242, precision .333 | .837, .314, .343 | .837, .306, .310 | .900, .356, .661 |
| flow | path_recall .328, tau .933 (30), precision .138 | .219, -.273 (11), .220 | .467, .167 (44), .230 | .402, .855 (46), .186 |

- The walk does not return its own seeds (the hybrid search hits), and on
  where-to those are its best answers: search alone beats the walk (reg_mrr
  +.150 [+.027, +.293], set_f1 +.072 [+.033, +.117]). On flow one hop with
  the seeds beats the full walk (path_recall +.139 [+.072, +.208], precision
  +.092 [+.070, +.117]) at 1.7 times the answer tokens and with the order
  along the path lost (tau .167).
- Merging run files offline (no new engine run): seeds interleaved with the
  walk's answers, cut to 10, give flow path_recall .444 and tau .615 at the
  walk's own answer count (about 10 per item), and where-to reg_mrr .847,
  reg_hit@3 .960. Appended in full, flow reaches path_recall .547.
- The oracle steer (`maps oracle-policy`: per-item prune rules built from the
  gold, bench only) bounds what steering can add with these maps: where-to
  precision +.328 [+.239, +.417] and set_f1 +.114, flow path_recall +.074
  [+.037, +.115]. A gold-aware filter of the plain walk's 40-result pool
  (`maps oracle-filter`, no steering) reaches about the same: where-to
  precision .660, reg_mrr .840; flow path_recall .418, tau .842. Most of the
  ceiling is choosing among results the walk already finds (4 of 59 oracle
  where-to walks returned nothing), so a result
  filter can be tried before steering the walk itself.
- A larger pool (top 40, scored on 10) changes little (where-to reg_mrr .680, flow path_recall .324).

## Exact operators and the walk (offline merges)

The exact operators of BR3 against the walk's run files, merged per item
(exact answers first; no new engine run):

- Impact: the uncapped closure (arm N) has decl_recall .961; appending the
  walk with `scip.impact` and the reranker (RR-scipimpact) adds almost no
  first-ring declarations (.965) but raises second-ring recall .487 to .572,
  tests recall .446 to .505 and file recall .963 to .981. At an equal 20
  answers, interleaving loses first-ring recall (.953 to .893) for second-ring
  (.482 to .534): exact first, the walk after it.
- History: interleaving the git pickaxe baseline with the reranked walk
  (RR-git), cut to 10, raises hit@10 .645 to .727 and mrr .492 to .521. The
  walk rows stopped on the deadline on 52 of 110 items, so the gain is a lower
  bound for that arm.

On every set a first stage (search hits or an exact operator) gives the best
early answers and the walk adds answers the first stage does not reach. BR4
puts this in the engine: exact operators that abstain when the question names
nothing, the seeds kept in the answers, and a merge of the two lists.
