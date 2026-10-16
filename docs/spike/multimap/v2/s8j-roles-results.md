# S8j results: model roles tested one at a time (2026-10-16)

Step 1 of the roles side quest: test each candidate "decision model" role alone, with a hosted
reference model standing in for the decision model, before spending time on which local model
should fill the role. Labels [M] measured, [I] inferred. Everything here is offline replay or
bench work on the frozen eval tree; no role is wired into the engine. Numbers are aggregates
over gold sets; the gold questions themselves are not reproduced.

Result in one line: **one role survives, the judge (v1 questions) plus a `role=test` label.**
The judge v2 questions, both stopper forms, the router and the file prefilter were all killed by
rules written down before the runs.

## Setup

- **Roles.** Judge (score each candidate result for the question), judge v2 (richer structured
  questions plus a labeller), stopper (decide when to stop expanding), router (pick the question
  type), file prefilter (score the walk's first-stage files before the walk and steer it).
- **Split.** A fixed dev/test split of the gold items (dev used to pick settings, logged; test
  used only to confirm a dev pick). Items dev/test: where-to 25/34, flow 61/50, impact 79/82,
  history 58/52 (441 in all). The router was scored on all 441 questions; the prefilter and
  judge v2 on dev.
- **Judge protocol.** Candidate cards are built without gold: the node site plus the public
  snippet from the eval tree (or the commit message), and nothing else. Cards carry no `origin`
  field (see the confound below), candidates are batched 40 per call in a seeded shuffle, and
  the card builder and wording are frozen and hashed. The default gate is the model's own
  decision (score >= 0.5); hand-tuned cut-offs are not used in headline arms.
- **Pre-registered kill rules.**

| role | killed if |
| --- | --- |
| judge v2 | gain over judge v1 under +0.03, or labels agree under 90% with a path heuristic |
| stopper | under 20% of pops saved at matched recall, or fixed-N is as good |
| file prefilter | under 50% of the oracle-pool gain, or under 0.03 over the post-walk judge filter |
| router | a classical (TF-IDF) router reaches 90% accuracy, unless the model wins by 5 points |

- **Reference arms.** Local reranker and local models (clef, jevk5, decider:4b), the plain walk,
  and gold-aware oracle arms as probes (bench only, never used for scoring).

## Results

| role | verdict | key numbers [M] |
| --- | --- | --- |
| judge v1 | **KEEP** | no-origin flow rank path_recall .606 against the local reranker .505 (+.102 [+.054, +.150], 33 items better, 6 worse); where-to file view rank reg_mrr .940 against .837 without a judge (+.027 to +.200) |
| judge v2 | KILLED | gain under +0.03 on every headline metric (below) |
| labeller | partly kept | `role=test` kept; `is_test` dropped; `role` is a label, not a filter |
| stopper | KILLED (both forms) | extra pops saved over fixed-N at matched recall: negative on 3 of 4 sets ("enough?"), near zero or reversed on test (judge-driven) |
| router | KILLED | TF-IDF nearest centroid .946 against the model router .909 |
| file prefilter | KILLED | below the post-walk judge filter by .18 to .23 on where-to; negative on flow even with oracle files |

### Judge v1 (keep)

- Origin confound. The hosted judge's cards had an `origin` field that the reranker's text did
  not. Rerunning with no origin on both sides kept the lead: flow rank path_recall .606 against
  .505. Adding origin back moved the judge by only +.007 (not significant). All later judge
  arms are no-origin. This comparison was run before the dev/test split existed.
- The model-decided gate (score >= 0.5) was as good as the tuned best: where-to file view
  precision .648 against .653 at the best cut-off (oracle .702); flow precision .468, recall
  .538. Within-item AUC of gold regions against other files: .93 (where-to file view, batched
  .933 against .897 single-call), flow .78.
- Local models on the same task: jevk5 rank matches the local reranker within noise on
  where-to, flow and history. On impact both reorder exact-origin answers badly (decl_recall
  .87 to .54 and .49), so **a judge must never reorder exact-origin answers.** Clef's own
  decision fails as a gate (where-to reg_mrr .08 to .52, flow recall .07 to .09), so weaker
  models need rank mode or a calibration step.

### Judge v2 and labeller (killed, except one label)

- Dev, paired v2 minus v1: where-to rank 0/0/0 (all ties on reg_mrr, set_f1, precision); flow
  rank path_recall -.005 (ns); with the model's own gate, where-to reg_mrr +.045 (ns, one item)
  and flow recall +.013 (ns). AUC moved where-to relevance .936 to .953 and fit .938 to .951;
  flow flat (.772 to .778). The gain stayed under +0.03, so the judge stays v1.
- Labeller agreement with a path heuristic on dev (where-to, flow): `role=test` .929 and .974,
  kept. `is_test` .739 and .952, dropped (most false positives are files with inline test
  modules). `role=unrelated` fires on 11% of where-to gold and 35% of flow gold, so `role` is
  a label to show, never a filter.

### Stopper (killed)

- Premise holds (hops replay): an oracle stop saves 73% (flow), 79% (impact), 98% (history)
  and 92% (where-to) of pops. The smallest fixed N whose mean recall reaches the oracle's saves
  4%, 0%, 65% and 26%.
  On where-to the saving is mostly "seeds are already complete".
- "Enough?" question, dev, extra pops saved over fixed-N at matched recall (own decision,
  p >= .5): where-to +14.8% (n 18), flow -23.0%, impact -31.1%, history -66.2%. A completeness
  score was never better.
- Judge-driven stop (stop when the judge's best candidate crosses a threshold), best rule per
  set on dev: where-to +17.9%, flow +2.6%, impact +1.5%, history -12.7%. The pre-registered
  confirmation on test **reversed** the only sizeable gain: where-to +17.9% on dev became
  -21.3% on test. Max-hit and multi-hit rules were all negative.
- Caveat: 40% to 60% of the replayed states were truncated at 40k characters (seeds and the
  most recent expansions kept), which could understate a stopper. Both forms are dead on the
  rule "fixed-N is as good or better"; a rerun with untruncated states is not done.

### Router (killed)

Question-type routing over all 441 gold questions: pure-Python TF-IDF nearest centroid,
leave-one-out accuracy .946; the hosted model's choice .909 (22 impact questions sent to
where-to). The classical bar of .90 is passed, so the model router is killed. Caveat: the gold
questions are templated, so this says only that routing by question type is easy, not that
routing is solved on free-form questions.

### File prefilter (killed)

The judge scores the distinct files of the first-stage hits before the walk, and the scores
become per-item steering policy (keep the top 3, 5 or 10 files; keep the model's own picks, at
most 3; or boost them by 2x instead of pruning). Oracle-pool is a gold-aware probe (not an optimal-policy ceiling): the
gold regions intersected with the same seed-file pool, plus anchor files. Paired dev items: where-to
25, flow 61, impact 68. First stage off (the walk alone), headline metric per set:

| set (metric) | oracle-pool gain over plain | best judge arm | recovery of oracle-pool | vs post-walk judge filter |
| --- | --- | --- | --- | --- |
| where-to (reg_mrr, n 11 with registrations) | +.318 | own picks: +.227 (top 3: +.182) | .71 (top 3: .57) | -.182 (top 3: -.227) |
| flow (path_recall) | -.084 | top 3 boost: -.003 (prune arms -.08 to -.10) | not defined (probe negative) | -.30 to -.40 |
| impact (decl_recall) | +.033 | top 10: +.011 | .35 | not applicable |

- Where-to: the prefilter recovers more than half of the oracle-pool gain, but the plain walk
  followed by the judge filter reaches reg_mrr .909 (+.409 over the plain walk), far above
  either, so the second kill clause (under 0.03 over the post-walk filter) is hit. Boost-only
  arms gain nothing (top 3 boost -.015, own boost -.045).
- Flow: every arm is negative, and so is the oracle-pool probe (-.084; the oracle over full
  regions is +.089). Restricting the walk to first-stage files removes the files the walk
  would have reached.
- Impact: effects near zero, even for the probe (+.033).
- First stage append (exact answers and seeds merged with the walk): no gain on where-to or
  impact (oracle probe also 0.0); flow negative for every arm including the probe (-.109).
- Caveat: the pool is small (distinct files of the seed hits), so the probe is low by
  construction, and where-to has only 11 registration items on dev. Neither changes the
  verdict, since the post-walk judge filter beats every prefilter arm using the same scores.

## Placement notes for local models [M]

- GGUF models can run on the integrated GPU through Vulkan. jevk5:4b gave the same scores as
  on the discrete GPU (.7953 against .7951) at about 7x the warm latency (0.38 s against
  0.055 s per call).
- ONNX models (decider, kev, jeeves, nimble, clef, decima) cannot use Vulkan; with the CUDA
  device hidden they fall back to CPU (decima: 0.16 to 0.19 s warm).
- decider:4b with 32k context and a batch of 6 candidates spilled out of the discrete GPU's
  memory: F32 ONNX weights plus the context, not a placement tax. Off the discrete GPU those
  bytes land in system RAM. Batch size and context need a budget guard per model.

## What survives and what is next

- **Survives:** the v1 judge (batched 40, no origin, rank mode or the model's own gate) as a
  post-walk filter or ranker that never reorders exact-origin answers, and the `role=test`
  label.
- **Dead for now:** judge v2 questions, any stopper (fixed-N is enough), a model router (the
  classical one is enough), and a file prefilter before the walk.
- **Next, per-role model screen.** Test each candidate decision model per role alone on dev
  (mostly cached replays): the clef family, kev:9b, jeeves, decider:4b and 2b, nimble, winnow
  and cygnet, jevk5, decima. Take the top two per role to a few full configs on all items:
  best per role, one model for all roles, all-local. Local models need a per-model capability
  entry (context per row, batch size), token-budgeted batching and per-model calibration.
  TODO: screen results.
- **Experiment: the hosted model ranks the candidate models** per role before any measurement
  (cards from public model pages, three shuffles). Pre-registered prediction, nearly the same
  for all three roles: clef first, kev:9b second, then jeeves, winnow:12b, nimble and cygnet,
  jevk5 and decider:4b, winnow:e4b, decider:2b, decima last. The ranking ignores each model's
  context limit for batched judging, so it is role-blind. It is compared with the measured
  screen by rank correlation once the screen exists. TODO: Spearman against the measured
  order.
