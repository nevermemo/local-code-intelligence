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
- Tau is reported as the aggregate mean over the items where an arm defines it,
  with the item count in parentheses, so two arms in one row can rest on
  different items. A paired tau (items defined in both arms) is labelled paired.

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
| flow | path_recall .403, tau 1.000 | .364, .667 (aggregate tau over 6 items) |

The reranker helps where-to and impact and hurts flow: it ranks by relevance
to the question, which scrambles the order along a call path (aggregate tau falls from .933 over 30 items to .333 over 15; the two means rest on different items and are not a paired change) and loses path recall, though precision rises. On where-to it
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

## Seeds, one hop and a gold-aware steering probe (bench-1022)

Release build of `40e495f`, all items, guessed priors, two sets at once, no
deadline stops or call failures in any row (waves `W-*` in `trend.md`).
Paired deltas are `maps compare` with a 95% bootstrap interval.

| set | walk (W-plain) | search only (W-search) | one hop + seeds (W-onehop) | gold-aware steer (W-oracle) |
| --- | --- | --- | --- | --- |
| where-to | reg_mrr .687, set_f1 .242, precision .333 | .837, .314, .343 | .837, .306, .310 | .900, .356, .661 |
| flow | path_recall .328, tau .933 (30), precision .138 | .219, -.273 (11), .366 | .467, .167 (44), .230 | .402, .855 (46), .186 |

- The walk does not return its own seeds (the hybrid search hits), and on
  where-to those are its best answers: search alone beats the walk (reg_mrr
  +.150 [+.027, +.293], set_f1 +.072 [+.033, +.117]). On flow one hop with
  the seeds beats the full walk (path_recall +.139 [+.072, +.208], precision
  +.092 [+.070, +.117]) at 1.7 times the answer tokens and with the order
  along the path lost (tau .167).
- Merging run files offline (no new engine run): seeds interleaved with the
  walk's answers, cut to 10, give flow path_recall .444 and tau .615 at the
  walk's own answer count (about 10 per item), and where-to reg_mrr .847,
  reg_hit@3 .960. Appended in full, flow reaches path_recall .547 (about 20 answers per item
  instead of 10, so part of that is answer budget; see the N0 section).
- The gold-aware steer (`maps oracle-policy`: per-item prune rules built from
  the gold, bench only) is a probe, not an optimal-policy ceiling: it prunes
  outside the gold files and so may remove useful intermediate routes. With
  these maps it gains: where-to precision +.328 [+.239, +.417] and set_f1 +.114, flow path_recall +.074
  [+.037, +.115]. A gold-aware filter of the plain walk's 40-result pool
  (`maps oracle-filter`, no steering) reaches about the same: where-to
  precision .660, reg_mrr .840; flow path_recall .418, tau .842. Most of the
  probe's gain is choosing among results the walk already finds (4 of 59
  gold-aware where-to walks returned nothing), so a result
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
  walk rows stopped on the deadline on 52 of 110 items, so this is a
  deadline-limited observation, not a lower bound: further exploration could
  also displace good answers.

On every set a first stage (search hits or an exact operator) gives the best
early answers and the walk adds answers the first stage does not reach. BR4
puts this in the engine: exact operators that abstain when the question names
nothing, the seeds kept in the answers, and a merge of the two lists.

## First stage in the engine (bench-1023)

BR4b's first stage on the release build at `b5462ad`: arm F-off is the walk
alone; F-append lists the first stage (exact answers on impact and history,
the search seeds on every set, `first_k = 10`) and then the walk; F-interleave
alternates them; F-exactseed is F-append with the walk also starting from the
exact answers. Impact selects `exact.closure`, history `git.history` and
`exact.pickaxe`. Deltas are against F-off, 95% paired bootstrap, no item
stopped on the deadline.

| set | metric | F-off (about 10 answers) | F-append (about 20) | F-interleave (about 10) | F-exactseed (about 20) |
| --- | --- | --- | --- | --- | --- |
| where-to (59) | reg_mrr (25) | .687 | .837 [+.027, +.293] | .847 [+.040, +.300] | - |
| | set_f1 (34) | .242 | .297 [+.023, +.091] | .260 (n.s.) | - |
| | precision | .333 | .287 [-.088, -.009] | .358 (n.s.) | - |
| flow (111) | path_recall | .328 | .547 [+.168, +.272] | .398 [+.008, +.134] | - |
| | precision | .138 | .216 [+.060, +.098] | .300 [+.136, +.191] | - |
| | tau (paired: 30 / 16 items) | .933 (.875 on the 16) | .756 [-.356, -.044] | .792 (n.s.) | - |
| impact (141) | decl_recall | .067 | .874 | .711 | .869 |
| | r2_decl_recall | .046 | .461 | .364 | .464 |
| | tests_recall (100) | .006 | .387 | .291 | .387 |
| | precision | .052 | .342 | .428 | .331 |
| history (110) | mrr | .128 | .501 [+.281, +.465] | .483 | .501 |
| | hit@10 | .191 | .664 [+.373, +.573] | .582 | .664 |

Every impact and history interval excludes zero by a wide margin.

Answer budget: F-append (and F-exactseed) return the first stage's cards (up to
`first_k = 10`) plus the walk's top 10, about 20 answers per item, against about
10 for F-off and F-interleave, and the bench scores every returned answer. Part
of the F-append gain over those arms, above all on the recall metrics (flow
path_recall, impact second ring, history hit@10), is answer budget and not
walk. The matched rerun is the N0 section below. Findings:

- A plain repeat of F-off (arm R-off) gave the same score on every item of
  every set: this repeat showed no metric differences at this deadline. That
  shows repeatable scores on one repeat, not identical ranked evidence, and it
  says nothing about the noise of agent runs, which is still to be measured.
- Append is the default to beat (at about twice the answers of F-off and
  F-interleave; see N0 below): it is best or tied on recall everywhere
  (flow path_recall +.219, impact second-ring +.415, history hit@10 +.473)
  and loses only flow order (tau) and where-to precision. Interleave buys
  precision (flow .300, impact .428) for recall.
- `exact_seeds` adds nothing: on impact it costs 2.1 more expansions per item
  for the same scores, and on history its answers are identical to append's
  on all 110 items (pickaxe answers are commits, which the walk does not
  start from).
- Against the offline merges above, the in-engine first stage is below them on
  impact second ring (.461 vs .572) and history hit@10 (.664 vs .727); those
  merges used the reranked walk and wider maps, these arms the default maps
  without the reranker frontier.

## N0: first stage alone against first stage plus walk (bench-1024, bench-1024b)

Question: once the first stage has answered, does the walk add anything? Two
runs, the second pre-registered to remove a confound of the first. Same frozen
eval tree and release build (`b5462ad`) as bench-1023; paired by item, 95%
paired bootstrap (10000 resamples over items).

### First run, unmatched (bench-1024)

F-first is the first stage alone with its natural answer count (8.0 cards per
item on where-to and flow, 9.8 on impact, 9.7 on history); it was compared
with bench-1023's F-append (18 to 20 cards). F-append scored higher on the
recall metrics (flow path_recall .219 to .547, +.328 [+.269, +.388]; impact
second ring +.015 [+.009, +.022]; history hit@10 +.018 [.000, +.046]) and
lower on precision (flow -.150, impact decl_f1 -.361), and on where-to set_f1
it scored lower (.314 to .297, -.017 [-.029, -.007]). This mixes "walk against
no walk" with "about 10 against about 20 answers", and the bench scores every
returned answer, so a reviewer rated it confounded. It is kept only as the
reference for the matched run.

### Matched rerun (bench-1024b), pre-registered

The arms, the primaries (one per set: where-to set_f1, flow path_recall, impact
r2_decl_recall, history hit@10) and the decision rule were written before any
bench-1024b arm was scored (`prereg.md` of that run).

- F-append: bench-1023's arm as it is (first stage capped at `first_k = 10`,
  then the walk's top 10), reused and not re-run. The matched comparison is
  therefore against a 10-card first stage plus the walk; a walk appended to a
  20-card first stage was not run.
- F-first20: the first stage alone at 20 cards. A live run of the same binary
  with `first_stage = "append"`, `first_k = 20`, `answer_seeds = true`,
  `seed_k = 20` and `max_pops = 1`; every walk-origin card is then dropped, so
  the answers are the exact operators' cards (impact, history) followed by the
  search seeds. `seed_k = 20` is needed because the seeds are the walk's start
  nodes; with 8 seeds where-to and flow could not reach 20 first-stage cards.
- Both arms are scored twice: on at most 20 answers (headline) and cut at 10
  (robustness view). Delta is F-append minus F-first20.
- Rule: the walk stays in the default path for a set only when delta is
  positive, the primary is Holm-significant over the four primaries (alpha .05)
  and its unadjusted 95% interval excludes zero; otherwise the walk is opt-in.
  Bootstrap p is floored at 2/10001, so the smallest Holm p here is .0008.
  Secondaries are descriptive, with no verdicts.

Prefix check (after the run): the first cards of F-first20 equal bench-1024's
F-first cards on all 141 impact, 110 history and 59 where-to items, and on 108
of 111 flow items (3 differ). So lengthening the first stage kept the earlier
answers in place except on 3 flow items.

Invalid first where-to run: its preflight call for `seeds.hybrid` timed out
after 60 s (attributed to GPU contention), so the run used stand-in seeds
(3 first-stage cards per item, first cards equal to F-first on 0 of 59 items,
reg_hit@3 .24). It was moved aside as invalid and rerun; the rerun gave 20
cards per item and the 59 of 59 prefix match above. Nothing from the invalid
run is scored in the tables.

View at 20 answers (both arms scored on at most 20; delta = F-append minus
F-first20):

| set | primary (n) | F-first20 | F-append | delta [95% CI] | Holm p | verdict |
| --- | --- | --- | --- | --- | --- | --- |
| where-to | set_f1 (34) | .227 | .297 | +.069 [+.031, +.108] | .0008 | walk stays default |
| flow | path_recall (111) | .253 | .547 | +.294 [+.236, +.355] | .0008 | walk stays default |
| impact | r2_decl_recall (141) | .504 | .461 | -.043 [-.061, -.025] | .0008 | walk opt-in |
| history | hit@10 (110) | .645 | .664 | +.018 [.000, +.046] | .268 | walk opt-in |

View cut at 10 answers:

| set | primary (n) | F-first20 | F-append | delta [95% CI] | Holm p | verdict |
| --- | --- | --- | --- | --- | --- | --- |
| where-to | set_f1 (34) | .295 | .307 | +.012 [.000, +.022] | .130 | walk opt-in |
| flow | path_recall (111) | .229 | .360 | +.131 [+.097, +.168] | .0008 | walk stays default |
| impact | r2_decl_recall (141) | .446 | .447 | +.001 [.000, +.002] | 1.0 | walk opt-in |
| history | hit@10 (110) | .645 | .645 | .000 (all ties) | 1.0 | walk opt-in |

Reading:

- Flow: the walk adds path recall in both views (61 items better, none worse
  at 20), so it stays default. Where-to: the walk wins at 20 but only just at
  10 (the interval barely clears zero and Holm does not), so where-to is
  budget-dependent: default when 20 answers are returned, opt-in at 10.
- Impact: with 20 first-stage cards the first stage alone is better on the
  primary (-.043; 47 items worse, 13 better), and on decl_recall (.964 against
  .874), decl_f1 (.692 against .462) and precision (.592 against .342); at 10
  the two arms are nearly the same answers. History: no difference the data can
  separate at 20 (2 items better, none worse) and none at 10. Both are opt-in.
- Tie sensitivity (added after the run, from the rebuilt walk pools): many
  walk candidates tie on potential, and the product breaks ties in discovery
  order. Under random tie-breaking the expected flow gain at 10 answers is
  +.056 [+.040, +.072] (Holm .0008), not +.131: quote flow at 10 as +.06 to
  +.13, with more than half of the observed gain coming from the order in
  which the walk discovers its ties (first-found ties neighbour the best
  seeds). The 20-answer numbers are robust (flow +.273 expected against +.294
  observed; where-to unchanged), and every per-set decision above survives.
  Tie order alone moves flow by about ±.02, so later judge or filter gains
  smaller than that are not counted as wins.
- Secondaries at 20 (descriptive, unadjusted): where-to precision .166 to .287,
  flow precision .184 to .216, history mrr .499 to .501, history precision .281
  to .170; reg_hit@3 and reg_mrr on where-to are identical at both cuts (25
  items, all ties).
- Flow tau, aggregate and paired. Aggregate: F-first20 -.389 over 12 items;
  F-append .379 over 59 (bench-1023 row). Paired over the 12 items where
  F-first20 defines it: -.389 to -.333 at 20 (+.056 [-.167, +.278]); at 10,
  paired over 11 items, -.273 to -.515 (-.242 [-.545, .000]). Tau rests on
  a few items and decides nothing here.

Caveats:

- Answer counts: F-first20 returns 20.0 cards per item on every set (minimum
  19, on one flow item). F-append returns 18.0 on where-to and flow, 19.8 on
  impact and 19.7 on history. On where-to and flow F-first20 therefore has two
  more scored slots, so the F-append wins there are conservative. On impact the
  counts are nearly equal, so the first stage's win is not an answer-count
  effect.
- Scored list: history hit@10 reads commits (accepted commits among the
  distinct commits returned), so it is effectively a comparison of commit
  lists. F-first20 returns 12.2 commits per item and F-append 7.4; the rest of
  F-append's cards are symbols, chunks and files that this metric does not
  score. A walk contribution outside commits is invisible to it.
- Cost, per item (cost proxies, not wall time). F-append: pops 11.4, 9.7, 11.3
  and 153.4, physical map calls 18.1, 16.2, 17.9 and 386.9, for where-to, flow,
  impact and history; external calls are 0 on the first three sets and 88.9 on
  history. F-first20 (live, walk capped at one pop): physical map calls 1.3,
  1.8, 1.3 and 2.6, history git calls 7.2 and none elsewhere.

Agreement check: dropped on where-to and flow (no exact operator, so the
overlap between exact answers and seeds is undefined). Kept on impact and
history, with the label now measured against F-first20. On impact the walk adds
a gold item on 0 of 141 items, so the AUC is undefined (first run: .584, with
8 positives). On history it adds one on 5 of 110 items and the file-overlap AUC
is .472 [.181, .748], again below the .65 rule: killed as a skip-the-walk
signal, as in the first run.

Decision from this run: the walk is default on flow (both views) and on
where-to at 20 answers (opt-in at 10, so where-to is budget-dependent); it is
opt-in on impact, where the first stage alone is better at 20, and on history.
This compares the walk with no result filter; whether a local judge changes it
is N0b.

## N0b: a local judge as a result filter

Question: does a local judge, used as a result filter, change the N0 decisions
(first stage alone against first stage plus walk)? Pre-registered before any
judged test result; confirmatory on the S8j test split only. Five arms, all
offline reorderings or filters of the same raw engine outputs (the engine and
its cost are unchanged by the judge):

- A1 first stage alone (20 cards), A2 first stage plus judge, A3 first stage
  plus walk (append, 10 plus the walk's top 10), A4 append plus judge, A5
  append plus a judge that reorders only the tie group at the K=10 cut (flow
  only).
- Judge: cygnet:12b, Q8_0 GGUF, Apache-2.0 (a Gemma 4 12B derivative), rank
  mode, one candidate per request, card text only (the gold is never sent),
  exact-origin answers never reordered. It was picked by a rule fixed before
  the numbers were used: among the licence-eligible local models, the highest
  mean gain over the no-judge null on the dev items at K=10, where a set loss
  worse than -.02 disqualifies. The gate modes (keep or drop) all lost on dev
  and were disqualified; cygnet:12b in rank mode won with a mean dev gain of
  +.071 (flow path_recall +.143, where-to set_f1 -.002).
- Test split, n per set (items with the primary defined): where-to 20 (of 34),
  flow 50, impact 73 (of 82), history 52. Where-to is at the edge of the
  n >= 20 floor, so a where-to result there is "no evidence", not "no effect".
- K = 10 is the primary view; K = 20 is the robustness view. Same primaries and
  Holm correction (over the four sets, inside each comparison) as N0; 95%
  paired bootstrap, 10000 resamples. Comparisons: C1 A2 minus A1 (filter on the
  first stage), C2 A4 minus A3 (filter on the append), C3 A4 minus A2 (walk
  under the filter), C4 A3 minus A1 (walk without a filter, the N0 question).
- Decision chain per set: the walk stays only if C3 is positive and
  Holm-significant; the filter becomes the default only if it is positive,
  Holm-significant and (on flow at 10) above the tie band below; a significant
  harm is reported.

### Primaries at K=10, test split

| arm | where-to set_f1 (20) | flow path_recall (50) | impact r2_decl_recall (73) | history hit@10 (52) |
| --- | --- | --- | --- | --- |
| A1 first stage | .349 | .197 | .460 | .615 |
| A2 first stage + judge | .377 | .169 | .457 | .615 |
| A3 append | .363 | .324 (E .254) | .461 | .615 |
| A4 append + judge | .315 | .355 | .460 | .615 |
| oracle filter, first stage (ceiling) | .430 | .215 | .466 | .615 |
| oracle filter, append (ceiling) | .386 | .492 | .473 | .615 |

E is the expected score under random tie order (no-judge append only; the first
stage has no ties). The oracle rows reorder by the gold labels and are ceilings,
not arms.

### Verdict per set at K=10 (delta [95% CI], Holm p)

| set (n) | C1 filter on first stage | C2 filter on append | C3 walk under the filter | C4 walk, no filter | default after the chain |
| --- | --- | --- | --- | --- | --- |
| where-to (20) | +.028 [-.024, +.077], .83 | -.048 [-.095, -.006], .10 | -.063 [-.120, -.013], .023 | +.014 [+.005, +.024], .0008 | A1; walk opt-in, filter off |
| flow (50) | -.027 [-.063, .000], .23 | +.031 [-.025, +.086], .78 | +.186 [+.120, +.257], .0008 | +.127 [+.077, +.179], .0008 | A3; walk stays, filter off |
| impact (73) | -.003 [-.019, +.008], 1.0 | -.002 [-.005, .000], 1.0 | +.003 [-.008, +.019], 1.0 | +.002 [.000, +.005], 1.0 | A1; walk opt-in, filter off |
| history (52) | .000 [.000, .000], 1.0 | .000 [.000, .000], 1.0 | .000 [.000, .000], 1.0 | .000 [.000, .000], 1.0 | A1; walk opt-in, filter off |

Reading:

- No set adopts the filter. The pick's dev gain on flow (+.143, on the screen's
  pools) did not carry over: on test, A4 minus A3 is +.031, numerically above
  the .02 tie band but not significant. The screen is a weak predictor here.
- Where-to: with the filter on, the walk pool is significantly worse than the
  filtered first stage (C3 -.063), so the walk is opt-in at 10 as in N0. The
  filter on the append (C2 -.048, unadjusted p .026, Holm .10) is not a
  significant harm after Holm; 9 of the 12 changed items lose.
- Flow: the walk stays under the filter (C3 +.186, 22 items up, 1 down).
- Impact and history: the judge moves nothing. Every impact difference is under
  .003, and history has no judged change at K=10 (at K=20 its cards change in 10
  of 52 items for A4, which cannot change the score). A judge gain above .01 on
  either set would have been checked as a bug; none occurred.
- What the judge did: at K=10 it reordered the card list in nearly every item
  (where-to 34 of 34, flow 47 to 50 of 50, impact 53 to 55 of 73), changed the
  card set in 32 to 54 items, and moved the primary in only 1 to 15 items per
  set (where-to 12 of 20, mostly down for A4: 3 up, 9 down).
- The oracle ceilings reproduce the N0 harness exactly. Headroom is real only
  on flow with the walk (append .324 to .492); a filter reaching it would need
  a much better ranking than this judge gives.

K = 20: in rank mode the judge reorders and drops nothing from the 20 cards, so
it cannot change the scored set. A2 equals A1 and A4 equals A3 exactly on all
four sets (checked, not tested). The walk effects repeat N0: where-to +.069
[+.028, +.114] and flow +.277 [+.185, +.374] significant (Holm .0008), impact
-.034 [-.060, -.009] significant (Holm .012, the walk lowers recall), history
+.019 [.000, +.058] not significant.

### Ties

Tie order is part of the K=10 flow score, as the N0 tie note says. Test split,
flow at 10: A3 scores .324 as observed against .254 expected under random tie
order. The no-judge walk gain (C4) is +.127 observed and +.057 against that
expectation, so more than half of the K=10 flow gain is tie order. At K=20 the
walk gains are robust to ties (flow +.277 observed, +.260 against the expected
score; where-to +.069 either way; impact -.034 and -.037).

Tie band rule, fixed before the judged results: a judge gain counts as a win
only if it also exceeds the tie band of its cell (flow at 10 about .02, flow at
20 about .016, other cells about .002) as well as passing the registered test.
Judge arms cannot be redrawn under random ties without new verdicts, so they are
reported as observed only. C2 on flow is +.031, above the band but with Holm .78,
so it is not a win under either criterion.

### A5: a tie-only judge (flow, K=10)

Offline, on dev, the tie group at the cut (about 11 candidates per item) was
the only place where a judge could matter on flow at 10: the oracle order
inside the ties reaches .55 against .36 observed (all items), and the dev
tie-break with cygnet:12b gained +.103 [+.039, +.166]. A5 applies that on test,
reusing the A4 verdicts (no extra model calls).

| judge | A3 | A4 | A5 | A5 minus A3 [95% CI], Holm | A5 minus A4 [95% CI], Holm | judge calls per item, A5 against A4 |
| --- | --- | --- | --- | --- | --- | --- |
| cygnet:12b | .324 | .355 | .342 | +.018 [-.027, +.064], .82 | -.013 [-.057, +.030], .82 | 9.1 against 14.1 |

- The dev promise (+.103) shrank to +.018 on test, not significant and below the
  band: not a win. A5 is indistinguishable from the full-pool judge (A4) at
  about two thirds of the calls.
- The dev numbers were selected on dev (the screen models were chosen there), so
  shrinkage was expected, and it was large.

### Hosted reference, not a candidate

A hosted model (jev-latest) ran the same arms A2, A4 and A5 as a descriptive
reference for what a good judge can do. It is never shipped, is not eligible
for the pick, and sits outside the decision chain and every Holm family (its
Holm column is inside this reference set only). It does not change any verdict.

| comparison, K=10 | where-to (20) | flow (50) | impact (73) |
| --- | --- | --- | --- |
| ref A2 first stage + judge | .418 | .181 | .462 |
| ref A4 append + judge | .368 | .395 | .458 |
| ref C1 A2 minus A1 | +.070 [+.025, +.117] | -.016 [-.042, +.003] | +.003 [-.002, +.009] |
| ref C2 A4 minus A3 | +.005 [-.037, +.050] | +.071 [+.021, +.124] | -.003 [-.007, .000] |
| ref C3 A4 minus A2 | -.050 [-.095, -.008] | +.214 [+.141, +.290] | -.004 [-.010, .000] |
| gap, cygnet minus ref, A2 | -.041 [-.091, +.001] | -.012 [-.040, +.010] | -.006 [-.021, +.004] |
| gap, cygnet minus ref, A4 | -.054 [-.114, +.004] | -.040 [-.086, +.001] | +.001 [.000, +.004] |

- The reference helps where a filter should: on the first stage for where-to
  (+.070) and on the append for flow (+.071, above the band). A5 with the
  reference is +.046 [+.002, +.092] over A3 (unadjusted p .040, Holm .081), also
  not a win.
- The local pick trails the reference by .04 to .05 on where-to and flow
  (intervals touch zero, n = 20 and 50) and matches it on impact. History is
  identical for both (no judged change).

### Physical cost

- cygnet:12b: 3246 live requests, none failed or lost, 2.9 M server input
  tokens, no output tokens, about 0.13 to 0.14 s p50 per request, 576 s for the
  whole local run (concurrency 1, no throttle). The harness's character-based
  token estimate undercounts the server's count by about 1.9 times for this
  model.
- Reference (hosted): 3256 requests, none failed, 2.1 M input and 114 k output
  tokens, 0.26 s p50, 507 s at 2 workers.
- Engine cost per item is the same with and without a judge: the walk costs
  about 10 to 11 pops and 16 to 18 map calls on where-to, flow and impact
  against 0 pops and 1.4 to 1.8 map calls for the first stage alone. History:
  153 pops, 385 map calls and 90 external calls against 2.6 map calls and 7.4
  external calls.
- The pick produced no significant gain, so it has no cost per point. For the
  reference, A4 on flow costs about 8.8 k judge input tokens per item for +.071
  (an estimate from request counts).
- A5: 9.1 judge calls per item (9.4 per tied item) against 14.1 for the
  full-pool judge, with no extra model calls.

### Deviations and caveats

- A5 is not a strict subset of the A4 pool: 45 of the 508 tie-group members
  (9%) were found by the walk but never returned, so they have no verdict and
  cannot be placed. A5 reorders only the returned, judged members (453 of 463
  returned members had a verdict). It is a conservative, cache-only version of
  the dev tie-break, which also scored members the walk had not returned.
- RAM guard breach: free memory fell to 2.6 to 2.8 GB for a few minutes while
  the judge model was resident, against the run's 6 GB rule, and recovered to
  6.5 GB or more within minutes. The run was not paused, because the model has
  to stay resident; nothing was paging (checked). The VRAM guard (hard-coded 2.0
  GB free in the harness) was lowered to 1.0 GB to match the run's rule; free
  VRAM stayed at about 1.3 GB.
- Process: the pick comparisons were run minutes before the rule text was
  written into the pick record. The rule was not changed after the numbers were
  seen.
- Report-only additions made after results existed: the table of items whose
  cards changed against items whose primary changed, and the per-item card lists
  kept for a later agent-proxy measurement. They change no rule, test, Holm
  family or number. They arrived mid-run, not before results, and the
  registration says so.
- The GPU throttle was off for this run (the registration text still reads 0.6).
  The call cap (3256, the registered estimate) was exactly enough.
- Screen against live: the dev screen sent batches of 40 candidates on larger
  pools; the live run sends one candidate per request on the real arm pools.
  Per-request latency in the screen was queue time; live latency is 0.14 s.
- Not run: the all-items descriptive judge run, K=20 gate comparisons (the pick
  is a rank mode), and the decima:agent candidate (its screen entry was
  incomplete). History has no judge-sensitive cards at K=10.

Decision from this run: no local judge filter becomes a default on any set. The
N0 defaults stand under the filter: the walk stays on flow, is opt-in on
where-to at 10 (default at 20, as in N0), impact and history. A judge pays only
where the walk's tie order is the problem (flow at 10), and there the local
pick's test gain is +.03 (+.02 for the cheaper tie-only judge), below what the
tests can separate from noise.

## Shipped defaults (S1)

The N0 and N0b decisions are the engine's defaults now (code defaults in `lci-core/src/config.rs`, the per-question-type
values in the preset files `src/app/traverse/presets/*.json`, both restated in `config.example.toml`):

| setting | default | source |
| --- | --- | --- |
| `[traverse] first_stage` | `"append"` (was `"off"`) | N0: the gains come from the first stage |
| `[traverse] answer_seeds` | `true` (was `false`) | the start nodes are the first-stage answers on where-to and flow; the winning arms all had it |
| `[traverse] exact_seeds` | `true` (was `false`) | neutral in bench-1023 (no score change); the exact cards are exact answers |
| policy `impact` | walk off, `exact.closure` added to the maps, `first_k` 20, `seed_k` 20 | N0 @20: first stage alone better (-.043 with the walk) |
| policy `history` (new preset) | walk off, `git.history` and `exact.pickaxe` added, `first_k` 20, `seed_k` 20 | N0: walk +.018, Holm .27 |
| policy `flow` | walk on | N0, N0b: +.294 at 20, +.131 at 10 |
| policy `where-to` | walk when `first_k + top_k >= 20` | N0: +.069 at 20, +.012 (n.s.) at 10 |
| no policy, or `explore` | `[traverse]` keys alone: first stage, then the walk | unchanged walk |
| `edge_bandit`, `[recipes] flow_judge`, `[recipes] enabled`, `[compose] enabled` | off | N2, N0b measured off; the recipe and compose verdict is N3b's |

"Walk off" means the first stage alone answers: the call takes one pop (so the response still has a handle a caller may
continue) and the walk's cards are not returned; the bench arm F-first20 ran the same way (`max_pops = 1`, walk cards dropped).
"Answers" for the where-to rule is the number of cards a call asks for, `first_k + top_k` (N0's 20 is 10 + 10); over MCP the
default `top_k` is 8 (18 answers), a point measured later in d389 (16 answers without `steer`, default kept): the threshold sits at the measured point (20 answers), so a where-to call over MCP with the default `top_k` does not walk, and gets its walk from `top_k` 10. Whether the default `top_k` or the threshold should move is open. A `[traverse.policies.<name>]` table overrides any of the
built-in values; `lci traverse --first-k` wins over the policy's own value. The recipes pin the first stage off, so
`flow_from` still reads the walk's own cards.

Verification (S1): the bench was re-run on the frozen tree with these defaults and no arm flags (only `--policy <set>`
for the question type) and compared item by item with the winning arms of the earlier run, at 20 and at 10 answers.
where-to (59 items) and flow (111) against F-append, impact (141) against F-first20: every primary metric is identical
(set F1 .2966 at 20 and .3069 at 10, reg hit@3 .920, path recall .5466 at 20 and .3600 at 10, tau .3785, impact second
ring .5043 at 20 and .4462 at 10, decl F1 .6924 and .8152); the only differences are precision +.0017 on one where-to
item at 10 and -.0002 on one flow item. history (110 items) is identical to F-first20 (hit@10 .6455, mrr .4988 at 20 and .4918 at 10, hit@3 .5091, precision .2809 at 20); against F-append it is -.0182 on hit@10 (interval -.0455 to 0, two items), -.0023 on mrr and -.0091 on hit@3, inside the .02 allowed, at 1 expansion per item against 153.


## Do bench metric differences predict answer quality? (proxy validity, pv2)

Pre-registered check, run 2026-10-16. For one arm pair (first stage with walk cards dropped against first stage plus
walk, both cut at 20 cards), a reader model answered each bench question from each arm's cards only, three
repetitions, and two blind judges (a third on disagreement) graded the answers against gold. The statistic is the
pooled Spearman correlation between the per-item metric difference and the per-item graded answer difference;
the kill rule was a point estimate below .2.

- Not killed: Spearman +0.699 (95% interval +0.596 to +0.784, 181 items over where-to, flow and impact). Per set:
  flow +0.50 (78 items), impact +0.59 (77), where-to +0.155 (26, interval -0.29 to +0.55, undetermined).
- Signs agree on 98 of 112 items where both differences are nonzero. Mean graded difference is +0.184 when the
  metric rises, +0.037 when it is unchanged and -0.118 when it falls.
- Reliability of the graded outcome over three repetitions is about .90, so +0.70 is close to the ceiling.
- Limits: pooling across sets adds between-set structure (flow differences are all positive, impact mostly
  negative); the metric detects losses better than gains (rises on impact and where-to showed about zero graded
  gain); the reader sees cards only, so validity for a live agent with tools is probably lower; one arm pair
  only. History had too few nonzero differences to test.
- Use: bench differences on flow and impact are evidence about answer quality for a card reader. A where-to
  difference, and any gain claimed on impact, still needs an agent arm before a default changes.
- Cost: $25.41 of a $30 cap.

## Reading rules for the tables above (2026-10-16, additive)

Nothing in this file is rescored or edited; these are footers that apply to every table here and to later ones. The full
list, with the new columns and the deferred items, is in `gold-scorers.md` ("Reporting rules"); `cargo xtask maps bench`
now also writes a `supplement.md` per run with the secondary columns and the ceiling footers.

- Ceiling metrics: impact first-ring declaration recall (a word-boundary grep plus the enclosing declaration reaches .995 on
  unique names, .79 on some, .42 on common names), flow path recall (breadth-first expansion reaches 97 to 100% of the gold
  paths at 200 pops), and the error set's message-to-site question (a fixed-string search gets .971). A walk, filter or
  steering delta on one of these is not evidence of a gain. The impact rows to read are the `common` band, `r2_decl_recall`
  and `tests_recall`.
- Flow rows: read `path_recall` with answer precision and tau; recall alone rises with the answer count.
- History: the H3a, H3b and H1 rows are not used for decisions (H3 floors near zero by construction; H1 is circular with the
  exact pickaxe operator). No walk, bandit or filter decision above rests on them.
- where-to on tantivy (30 items) is a loss guard only.
