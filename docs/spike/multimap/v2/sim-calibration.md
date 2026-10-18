# Sim calibration: do the simulated agent's proxies track a judged outcome? (2026-10-18)

Roadmap item 8, the tier-2 half of d327 that is not confidence calibration (no component emits a probability yet, so
that half waits for one). Plan: private record `run/sim8/PLAN.md`, sha256 `0976b597...`, frozen before any run on a gold
set, any sample or any judge call; every later change is a disclosed deviation. Results: `run/sim8/RESULTS.md`
(final, includes the coordinator's spot check). Labels: [M] measured, [E] estimated or a design choice of the run. This is a
proxy-validity measurement, not an arm result: no number here says A1 or A2 is better than grep.

## Verdict: BETWEEN, "inconclusive at n 48"; the proxies stay uncalibrated

Neither the pass bar nor a kill bar was met, so the sim's proxies (`hit_at_12` primary; `calls_to_gold`,
`tokens_to_gold`, `hit_at_25`, `listed_at` secondary) are labelled **uncalibrated** in `sim-agent.md` and the roadmap.
`hit_at_12` differences between arms stay tier-1 numbers, and no arm claim is made from the sim without a tier-3 run.

## 1. Question and frozen inputs

Question (fixed before any run): do the proxies rank the six configurations (arms A0, A1, A2 times policies greedy,
bfs) the way a judged outcome does, where the judged outcome is "an agent given exactly this reading list names the gold
files"? Part A runs the sim on the existing sets (proxies only, no model). Part B judges a small subset with a cheap
model, blind to gold, scored by rule.

| input | identity [M] |
| --- | --- |
| Binaries | `lci` and `xtask` release builds of main at `fd2cd26` (the commit that merged the sim); one cargo build |
| Tree | `bfaff44` (`bfaff446...`), the gold's own root |
| Gold | `where-to` 59 items, sha256 `5f0f0e7f...`; `impact` 141 primary items (the file holds 161), `8c118213...`; `flow` 111 items, `2c3a514c...`; 311 items in all (the plan said 312). `gold_known` was true for every item, so none was excluded |
| SCIP (A2) | the warm4 index of the same tree, sha256 `81b83d2e...` |
| Services (A1, A2) | reranker ON (the shipped default, d382), embedder and reranker on loopback, the same config for every arm, policy and set; the tree was indexed beforehand (2,085 chunks, not stale) |
| Sim settings | defaults: 25 calls, 20,000 tokens, 6 probe terms, 3 read files, `--lci-top-k 10`, no `--stop-at-gold`; one `--wave` per (arm, policy) |

Determinism [M]: the six A0 repeats (both policies, three sets) and the four A1/A2 where-to repeats have byte-identical
`items.json` and `run.json`: 10 of 10. In every run `skipped_lci_failures`, `incomplete_scans`, `deadline_stops` and
`flagged_items` are 0 and every item is scored.

## 2. Part A: the sim on the existing sets (no model) [M]

`hit_at_12` per configuration, mean over all items of the set, 95% bootstrap interval (10,000 resamples). It is read from
the 12-call prefix of each 25-call trace, cut by script (0 mismatches against the recorded values).

| config | where-to (59) | impact (141) | flow (111) |
| --- | --- | --- | --- |
| A0 greedy | 0.153 [0.068, 0.254] | 0.383 [0.305, 0.468] | 0.360 [0.270, 0.450] |
| A0 bfs | 0.153 [0.068, 0.254] | 0.383 [0.305, 0.461] | 0.360 [0.270, 0.450] |
| A1 greedy | 0.831 [0.729, 0.915] | 0.972 [0.943, 0.993] | 0.973 [0.937, 1.000] |
| A1 bfs | 0.831 [0.729, 0.915] | 0.972 [0.943, 0.993] | 0.973 [0.937, 1.000] |
| A2 greedy | 0.831 [0.729, 0.915] | 0.908 [0.858, 0.950] | 0.811 [0.739, 0.883] |
| A2 bfs | 0.831 [0.729, 0.915] | 0.908 [0.858, 0.950] | 0.811 [0.739, 0.883] |

Greedy equals bfs on `hit_at_12` for every arm and every item: the two policies only part ways after the first reads
(call 10 for A0), so they differ in `hit_at_25` and `calls_to_gold`, not in the primary proxy. The six configurations
are therefore three distinct systems (A0, A1, A2) with tied pairs. Paired differences against A0 greedy (Holm across the
three sets, 0 only-in-A/B, 0 excluded, 0 skipped): A1 +0.678 [+0.542, +0.797] where-to, +0.589 [+0.504, +0.667] impact,
+0.613 [+0.523, +0.703] flow; A2 greedy +0.678 [+0.559, +0.797], +0.525 [+0.433, +0.617], +0.451 [+0.342, +0.559] (A2 bfs the same except flow [+0.342, +0.550]); Holm p 0.0003
(the bootstrap floor) for all. The secondary proxies are in the private `run/sim8/proxy-tables.md`. None of this is an arm
verdict; it is the proxy side of the calibration.

## 3. Slice 2: N3 descriptive check, no verdict (d329, never done before) [M]

The 40 N3 C0 tasks were written as where-to-shaped gold (declaration lines as spans; 39 tasks usable, n3-d02 has only
config-key items and no gold the sim can hit) and run through A0 (both policies, 6 runs, repeats identical). Against the
80 C0 agent transcripts:

* Sim A0 `hit_at_12` is 0.179 over the 39 tasks (config_readers 0.200, impact 0.300, flow_path 0.125, composite 0.091);
  a sim hit needs one gold span, while N3's recall needs all units, so it is far weaker than an N3 pass.
* Every sim run ended on a budget (22 on the token cap, 17 on the call cap, none by an answer rule), so a sim
  `tool_calls` mostly measures how fast the budget fills. C0 agents made 4, 8.5, 5 and 13 tool calls (median per family).
* Spearman of sim `tool_calls` against agent tool calls per task: +0.02 [-0.32, +0.35] (n 39); family-centred +0.36
  [+0.01, +0.66]. `calls_to_gold` has 7 to 11 hit tasks, too small to read. About 30 cells are shown, none corrected.
* First-grep overlap: the sim's first probe is a word of the agent's first Grep pattern in 0.75 of tasks, and its six
  probes cover 0.88 of those words. The shared case is trivial (96% of the agents' words occur in the question); the
  sim's longest-term rule picks a filler word where the anchor is short. 76% of the sim's read tokens go to
  `.md`, `.txt` and `.toml` files [M]; the causal reading of the weak correlation is not tested [E].

## 4. Part B: the judged subset [M]

**Sample.** 16 items per set, 48 in all, drawn with `random.Random(0x53494D38)` from sorted ids: 8 "proxies disagree"
items (the six configurations do not share one `hit_at_12`) and 8 "proxies agree" items per set (the specificity
stratum). Enough disagreeing items existed in every set (44 of 59, 87 of 141, 72 of 111). Nothing was selected by a
judged value. Identical reading lists of one item were judged once: **243 sheets** (147 configuration sheets plus 96
controls, two per item). A sheet is blind (random id, no arm, policy or set words) and holds the question and the
reading list exactly as the sim saw it.

**Judging.** J1 and J2 are claude-haiku-5-5 in different sheet orders; J3 is claude-sonnet-5-5 where the two differ by more
than 0.25. The judge returns the files that answer the question; `judge_file_recall` is the fraction of the item's gold
files named, scored by rule (scripted from the gold files; the frozen `xtask gold score` was not invoked). The judge's
self-grade `answerable` is descriptive only.

**Judge gate** (oracle controls at least 0.80, random controls at most 0.15):

| round | setup | oracle | random | result |
| --- | --- | --- | --- | --- |
| 1 | Haiku J1 + J2, original prompt | 0.720 | 0.000 | fail |
| 2 (the one allowed replacement) | same model, prompt v2 | 0.843 (final grade 0.849) | 0.000 | pass |

The replacement is a prompt replacement: v2 adds one sentence group telling the judge to name every file whose text shows
a relevant declaration, call site, registration or definition even if incomplete, only paths that appear in the text. A
trial with Sonnet as J1/J2 cost about 17 times Haiku and was abandoned after 8 calls. The prompt was changed after seeing
only the round-1 control scores; the whole judging was then redone. J3 was called on 10 sheets (7 configuration sheets);
mean |J1 - J2| is 0.036 on configuration sheets; no sheet was left ungraded. Spend: USD 1.55 under the tag `sim8-` (cap
15), including USD 0.14 for the abandoned trial.

## 5. Statistics and the verdict line by line [M]

| stat | value | bar | met |
| --- | --- | --- | --- |
| S1 pooled Kendall tau-b, 18 set-centred (set, config) cells | 0.228 [0.149, 0.774] | pass at least 0.5; kill below 0.2 | pass no; kill no |
| S2 pooled Spearman, delta against A0 greedy, 240 pairs, clustered by item | 0.253 [0.007, 0.485] | pass at least 0.3 with lower bound above 0 | no |
| S2 sign agreement, both deltas nonzero | 52/58 = 0.897 [0.788, 0.961] (Clopper-Pearson) | kill at or below 0.6 | kill no |
| Judge gate, final | oracle 0.849, random 0.000 | 0.80 / 0.15 | yes (after the replacement) |

Reading: the pass bar fails on both counts (tau 0.228 < 0.5; Spearman 0.253 < 0.3), no kill bar fires (tau 0.228 is above
0.2, sign agreement is far above 0.6, the gate passed), so the plan's BETWEEN branch applies: "inconclusive at n 48".
Per-set tau-b: where-to 0.784 [0.320, 1.000], impact 0.816 [0.816, 1.000], flow 0.333 [0.333, 0.816] (6 configurations,
16 items each). Per-set Spearman: where-to 0.255 [-0.158, 0.634], impact 0.214 [-0.309, 0.679], flow 0.310 [-0.075, 0.640]
(80 pairs each). The guessed expectations (tau 0.4 to 0.7, S2 0.3 to 0.5, oracle 0.9, random 0.05, where-to best and
impact worst) were not met in tau, S2 or the set order; they are reported, not acted on.

S3, descriptive (lower-is-better proxies sign-flipped; hit-only proxies on items where both configurations hit; `hit_at_25`
is a 25-call list read against the 12-call judgement; `calls_to_gold` is the full 25-call run's value, so its 94 pairs also count 8 sheets that reach gold only at calls 13 to 25, while `tokens_to_gold` and `listed_at` use hits within 12 calls): pooled tau-b / Spearman against `judge_file_recall`:
`hit_at_25` 0.379 [0.201, 0.743] / 0.251 [0.008, 0.482] (240 pairs); `calls_to_gold` 0.551 [0.328, 0.795] / 0.438
[0.175, 0.670] (94); `tokens_to_gold` 0.722 [0.222, 0.944] / 0.323 [-0.032, 0.627] (90); `listed_at` 0.873 [0.109, 0.909] /
0.463 [-0.057, 0.809] (76). `answerable` against `judge_file_recall`: Spearman 0.471 (n 147). Spearman among pairs
with equal window counts (length-bias check): 0.192 (n 88). Mean final grade: oracle 0.849, random 0.000, configuration
sheets 0.689. Where the proxy delta is zero (150 pairs) the judged delta has mean 0.163 and is nonzero in 51.

## 6. Caveats

* Three systems, not six: greedy and bfs tie on `hit_at_12` in all sampled items, so S1 has few distinct values and a
  lopsided interval around its point (0.228, [0.149, 0.774]).
* The pooled tau sits just above the kill line and its lower bound is below it, so the verdict is sensitive to the
  pooling rule. Per-set tau-b is 0.78, 0.82 and 0.33, high in where-to and impact, lower in flow.
* The proxy delta is binary, and all 90 nonzero proxy deltas are +1 (none -1), so a nonzero-stratum Spearman does not
  exist (a constant). What can be said: there the judged delta has mean 0.354, positive in 52 pairs, negative in 6, zero
  in 32. Sign agreement is high (0.897), but the judged grade varies a lot where the proxy sees nothing.
* `hit_at_12` is an any-hit rule (a window overlapping a gold span, or a spanless gold file named); the judged grade is
  a recall fraction, so for multi-file impact gold they measure different things.
* Listing-only paths (spot check of 10 sheets, 2026-10-18, no grade contradicted its sheet): gold files named only in grep
  listing lines (`path:count`) or in `lci` result lines without their text are the main source of judge disagreement and
  of proxy-judge gaps. `judge_file_recall` measures "files whose content the reading list showed"; `hit_at_12` credits a
  span hit however it was reached.
* A pass would have licensed only "the proxy tracks recognition from a reading list", not "agents solve tasks". The judge
  and the sim both key on identifiers (lexical circularity); the random controls (0.000) bound it.
* Small n: 48 items, 16 per set; per-set cells are descriptive. The set of sampled items is a stratified half-and-half
  draw, so its means differ from the full-set means in section 2.

## 7. What follows

The plan allows one follow-up: a new pre-registration with the sample doubled (96 items), never by adding items to this
one. It has not been started. The modelling choices `sim-agent.md` marks "to calibrate" (the `read_files` cap, the uncharged rarity
lookup, the 40-line window) stay as they are. Nothing here changes the arms' standing: the next evidence on A1 or A2 is a
tier-3 agent run.
