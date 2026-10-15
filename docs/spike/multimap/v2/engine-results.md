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

- With the bound off, where-to and flow keep their quality at 16 to 21 times
  the pops; impact decl_recall rose .067 to .079.
- B1 made the bound also wait for results below the top k that a later
  arrival could corroborate past the k-th. It changed nothing on impact,
  where-to or flow (identical rows); on history with git.history it costs
  5.7 times the pops (646 to 3695 on the holdout) with the same mrr.
- Diagnosis of the impact gap: in all 73 items whose top 10 differ between
  the bound on and off, the differing results are tied at the k-th ranked
  score (1.0 or 0.5). The bound does not miss better answers; longer walks
  fill the tie slots differently. Ranked potentials are products of a few
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

One batched reranker call per expansion rates the kept destinations (path
plus SCIP signature, at most 1000 characters); each kept hop is multiplied by
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
| flow | path_recall .403, tau 1.000 | .364, .667 |

The reranker helps where-to and impact and hurts flow: it ranks by relevance
to the question, which scrambles the order along a call path (tau falls from
.933 to .333, though tau is defined on 15 items instead of 30) and loses path recall, though precision rises. On where-to it
also repairs the learned priors' loss. It costs 16 calls per item on average
(about 90 documents per impact item, no failures) and 50 to 76% more pops.
On history (with git.history) the reranker is the largest gain so far: over
all 110 items mrr .128 to .230, hit@10 .191 to .382, precision .063 to .102,
with 42% fewer pops (16734 to 9733); with learned priors on the held-out
items, mrr .222 to .227 and hit@10 .379 to .517. It is also the most
expensive arm: 80 reranker calls and about 670 rated documents per item, and
50 of 8825 calls failed (timeouts while four sets shared one reranker).

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
that choice was made on the holdout.

## Fusion and pass-through (Q2, Q4)

- RRF fusion is worse than the corroboration boost on every set; the default
  stays `boost`.
- Pass-through of connective nodes changes no quality and saves 19% of the
  history pops; the default stays off.
