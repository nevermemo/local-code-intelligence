# Results index

Status: INDEX of frozen results, as of 2026-10-17. One row per arm or run: date, question, verdict, headline number (with
its interval and source), file. Nothing here is a merge or a re-scoring; the numbers are copied from the file in the last
column. Method, sets and tiers: [evaluation.md](evaluation.md). Decisions drawn from these results:
[../../decisions.md](../../decisions.md). Evidence labels in the frozen files: [M] measured, [I] inferred, [G] guess.

Notes that apply to every row:
- A file name starting with `v2/` or `s1-` is in this repository and frozen (never edited). "private record X" names a
  record held outside the repository, which is not reproducible from here.
- The frozen files cite files that were retired in the 2026-10-17 cleanup (`next-slices.md`, `gold-bugloc.md`,
  `ab-prereg-addendum-draft.md`, `eval-seed.md`, `recycled-research.md`, `maps-inventory.md`, `architecture-map.md`). Their
  content moved: slices, sets and A/B method to [evaluation.md](evaluation.md); lessons to [lessons.md](lessons.md);
  engine facts to [engine.md](engine.md) and [../../architecture.md](../../architecture.md).
- Ceiling metrics and the reading rules for every table: [evaluation.md](evaluation.md), "Scorers and reporting rules".

## Identity and baselines (frozen eval tree `bfaff44`)
| id | date | question | verdict | headline | file |
| --- | --- | --- | --- | --- | --- |
| S1 stability | 2026-10-12 | does dropping `start_line` from chunk identity keep ids? | kept (one repository, 200 commit pairs) | 470,887 unchanged chunks: NodeId kept 100.00%, legacy key 98.49% (47.81% in files whose chunks changed or moved) | [s1-stability.md](s1-stability.md) |
| M0.3 | 2026-10-14 (inferred from the bootstrap seed; the file has no date) | lexical-only LCI on impact, flow, where-to | baseline; grep far ahead on impact | impact decl_f1 0.153 [0.131, 0.175] against grep 0.817; flow headline path_recall 0.104 [0.054, 0.197]; where-to reg_hit@3 0.720, equal to BM25 | [v2/m0.3-results.md](v2/m0.3-results.md) |
| M0.4 | 2026-10-14 | does a scored walk order hops better than random on flow? | beats random at hit@1 only away from name shortcuts | hit@1 rules 0.693 against random 0.395; no-shortcut 0.510 against about 0.407; BFS reaches path recall 0.990 inside 200 pops | [v2/m0.4-results.md](v2/m0.4-results.md) |
| M0b | 2026-10-14 | fused and reranked LCI against lexical | reranked helps where-to, not impact or flow | impact decl_f1 L 0.153, F 0.145, FR 0.164, grep 0.817; where-to reg_hit@3 FR minus L +0.200; convention set_recall FR minus F +0.111 [+0.020, +0.222] | [v2/m0b-results.md](v2/m0b-results.md) |
| M1.2 | 2026-10-14 | do `reads_key` translation edges help config-key impact? | kind does not improve impact as registered | O+T minus O decl_recall +0.067 [+0.028, +0.114] but precision 0.833 (prereg bar 0.99); PT-d not met; flow reader precision 0.861 | [v2/m1.2-results.md](v2/m1.2-results.md) |
| M1.3 | 2026-10-14 | does the compiler probe equal the M0.5 mutations? | equal (plumbing test) | 20 of 20 [Wilson 0.839, 1.000]; warm `cargo check` p50 4.20 s (M0.5 gold 4.31 s) | [v2/m1.3-results.md](v2/m1.3-results.md) |
| M1.4 | 2026-10-14 | `impact(x)` as a search anchor plus exact closure | anchor from the hit loses to grep; from the question beats it (same-oracle caveat) | C decl_f1 0.700 against grep 0.817 (-0.116 [-0.190, -0.043]); N 0.967 (+0.150 [+0.101, +0.203]); ceiling O 0.991 | [v2/m1.4-results.md](v2/m1.4-results.md) |

## Engine benches (all in [v2/engine-results.md](v2/engine-results.md), by section)
| id | date | question | verdict | headline | section |
| --- | --- | --- | --- | --- | --- |
| Q1/B1 score bound | 2026-10-15 | does the bound change quality? | bound off costs 15 to 20 times the pops for no quality | where-to reg_mrr .687 to .680, flow path_recall .328 to .324; impact decl_recall .067 to .079 (ties) | Score bound |
| S6f hop priors | 2026-10-15 | do learned priors beat guessed (held-out)? | helps history and impact, not flow | history mrr .086 to .222, hit@10 .138 to .379 (18 of 29 non-empty); where-to reg_mrr .722 to .644 | Learned hop priors |
| W0c2b reranker frontier | 2026-10-15 | does a reranker on the frontier help? | helps where-to and impact, hurts flow | where-to set_f1 .242 to .304; flow tau .933 (30 items) to .333 (15 items, not paired); history mrr .128 to .230 (52 of 110 hit the deadline) | Reranker frontier |
| Q5 fitted tie-break | 2026-10-15 | does a fitted tie-break beat discovery order? | no; discovery order is a fair tie-break | gold first among held-out tied pairs: impact 58.4% to 74.5%, flow 75.3% to 68.6% | Fitted tie-break |
| Q2/Q4 | 2026-10-15 | RRF fusion; pass-through of connective nodes | RRF worse (default `boost`); pass-through off | where-to reg_hit@3 .480 against .840; pass-through cuts 19% of history pops | Fusion and pass-through |
| bench-1022 | 2026-10-15 | seeds, one hop, gold-aware steering | search alone beats the walk on where-to | where-to reg_mrr walk .687, search .837, oracle steer .900; flow path_recall walk .328, one hop .467 | Seeds, one hop |
| offline merges | 2026-10-15 | exact operators plus the walk, merged per item | exact first, walk after | impact decl_recall .961 (closure); second ring .487 to .572 with the reranked walk | Exact operators and the walk |
| bench-1023 | 2026-10-15 | first stage in the engine | F-append best or tied on recall (about twice the answers) | flow path_recall .328 to .547 [+.168, +.272]; impact decl_recall .067 to .874; history hit@10 .191 to .664 | First stage in the engine |
| N0 (1024, 1024b) | 2026-10-16 | does the walk add anything after the first stage? | walk default on flow and on where-to at 20 answers; opt-in on impact and history | flow +.294 at 20, +.131 at 10; where-to +.069 at 20, +.012 (n.s.) at 10; impact -.043 with the walk; history +.018 (Holm .27); agreement check killed (AUC .584 impact, .472 history) | N0 |
| N0b | 2026-10-16 | does a local judge as result filter change N0? | no filter adopted (judge cygnet:12b) | test K=10: flow append +.031 (Holm .78); where-to walk under filter -.063 (Holm .023); dev tie-break +.103 shrank to +.018 (n.s.) | N0b |
| S1 shipped defaults | 2026-10-16 | do the defaults reproduce the winning arms? | yes | all primaries identical; history -.0182 hit@10 against F-append (limit .02), 1 expansion against 153 | Shipped defaults (S1); private record next-iter/s1/RESULTS.md |
| pv (proxy validity) | 2026-10-16 | do bench deltas predict graded answer quality (60 items)? | KILLED, underpowered | pooled Spearman +0.019 [-0.192, +0.307]; 11 metric and 8 answer deltas nonzero | private record proxy-validity/RESULTS.md |
| pv2 | 2026-10-16 | same, 181 items over where-to, flow, impact | passed (kill rule: below .2) | Spearman +0.699 [+0.596, +0.784]; flow +0.50, impact +0.59, where-to +0.155 (undetermined); cost $25.41 | Do bench metric differences predict answer quality |
| tie-break premise | 2026-10-16 | would a decision model help at the cut? | headroom only on flow at 10 | oracle headroom flow@10 +0.1913 [+0.1499, +0.2338]; where-to@10 +0.0022; impact and history@10 0; cygnet:12b dev flow@10 +0.1025 [+0.0393, +0.1664] | private record tiebreak/RESULTS.md |

## Slices N1 to N4 and the model roles
| id | date | question | verdict | headline | file |
| --- | --- | --- | --- | --- | --- |
| S8j roles | 2026-10-16 | which decision-model roles work alone? | judge v1 kept (plus `role=test` label); judge v2, both stoppers, router and prefilter killed. The per-role model screen was NOT run (roles side quest closed except judge v1, owner 2026-10-16/17) | judge v1 no-origin flow path_recall .606 against reranker .505 (+.102 [+.054, +.150]); router TF-IDF .946 against model .909 | [v2/s8j-roles-results.md](v2/s8j-roles-results.md) |
| N1 | 2026-10-16 | will agents write good chained-query plans? | KILLED: schema stays internal, recipes are the agent-facing form | 12 of 54 units (22%) executable and on target (needs 27); 85% of plans run; uncued LCI use 4 of 54 runs | [v2/n1-results.md](v2/n1-results.md) |
| N2 | 2026-10-16 | does an in-request edge bandit help the walk? | no verdict (+.03 not detectable), then STOPPED; `edge_bandit` stays off | flow@20 bandit minus F-append -0.025, upper +0.001; hit rates 6% and 13% | [v2/n2-results.md](v2/n2-results.md) |
| N3 | 2026-10-16 | do recipe tools cut agent round trips (40 recipe-shaped tasks)? | recipes SURVIVE the frozen rule (opt-in; a floor test) | round trips 0.469 of plain (upper 0.526) and 0.633 of composer control (upper 0.707); recall non-inferior | [v2/n3-results.md](v2/n3-results.md) |
| N3 follow-ups | 2026-10-16 | re-judge; read-only Bash control | same verdict | recall T minus C0 +0.124 (lower +0.078) re-judged; against C0B round trips 0.444 (upper 0.499) | [v2/n3-results.md](v2/n3-results.md) |
| N3b | 2026-10-16 | do recipes help off-shape tasks (160 runs)? | NULL: 0 recipe calls in 80 runs | round trips 1.044; recall -0.015; LCI 33 of 1,245 tool calls (2.7%); input tokens +11.6% | [v2/n3-results.md](v2/n3-results.md) |
| N4 | 2026-10-16 | a filter trained on offline labels | KILLED (cheap head and LoRA fine-tune) | LoRA where-to set_f1@10 .222 tuned against .244 untuned and .307 first stage (-.085 [-.125, -.048], Holm p .001) | private record n4/ and n4b/; summary in [evaluation.md](evaluation.md) |

## Gold sets and set premises (private records under the sets directory)
| id | date | question | verdict | headline | record |
| --- | --- | --- | --- | --- | --- |
| tests-for-code 1 | 2026-10-16 | stage-1 gold and heuristics (494 items) | frozen | baselines recall@10: union .297, rg -w stem .255, name siblings .113; 65% of gold tests reached by no heuristic | private record tests-for-code/README.md |
| tests-for-code 2 | 2026-10-16 | LCI arm on the 327 test items | seed-only clears rg stem on hit@10, not union | A-seed hit@10 .544 (mode L), A-desc .765, A-both .792; union .459, rg stem .382; tier-H A-seed .786 | private record tests-for-code/stage2/RESULTS.md |
| tests-for-code 2b | 2026-10-16 | is the description win LCI's? | K1 fires: description arms not attributable to LCI | A-desc .718 [.633, .799] against BM25(description) .608 [.516, .696] (task-macro); paired +.111 [.017, .208] | private record tests-for-code/stage2b/RESULTS.md |
| docs-for-code 1 | 2026-10-16 | stage-1 gold and heuristics (548 external items) | frozen | seed-only baselines reach recall@10 .386; BM25 on the commit message .730 | private record docs-for-code/README.md |
| docs-for-code 2 | 2026-10-17 | LCI arm, dev only | paused, not frozen, no scores | 1,128 dev searches, 0 errors, 2 degraded records not re-run; test not run | private record docs-for-code/stage2/RESULTS.md |
| I1 pilot (issue-to-fix) | 2026-10-16 to 2026-10-17 | 100-item pilot, six kill bars | all six pass; full build stopped | clean+piece recall@10: rg .286, BM25 .350, LCI .444; tier H (n 40) LCI .633, minus rg +.196 [.029, .371] | private record issue-to-fix/RESULTS.md |
| I1 full build | 2026-10-17 | issue-to-fix on the full set | STOPPED by owner decision; record only, no test metric | 37 finished rows (20 dev, 17 test) in 5 repositories; ollama, osu, bevy not started; deno index killed after about 2 h; the set may be re-aimed at `traverse` | private record issue-to-fix/full/RESULTS.md |
| R1 stage 1 | 2026-10-17 | masked-region concept questions (146 kept) | frozen; no kill bar fired | span hit@10 LCI .64 [.56, .72] against BM25 over function units .19 and BM25 files .09; file hit@10 rg .16, BM25 files .25 | private record r1-masked/RESULTS.md |
| R1 stage 2 | 2026-10-17 | LCI search and traverse arms | traverse arms equal search: the SCIP file was empty, so the walk was NOT run | test span hit@10 A0 .602 [.50, .70]; one-per-file cap -.091 [-.159, -.023]; walk-only arm 0.000 | private record r1-masked/stage2/RESULTS.md |
| exact-route | 2026-10-16 | route quoted messages to grep first | KILLED by its pre-registered rule; Stage B not built | search on the quoted message .647 against grep .735 (hit@10, n 34) | private record exact-route/RESULTS.md |
| RepoQA | 2026-10-16 | adopt the needle-function task? | not adopted | 29.3% of descriptions leak-clean (bar 40%); rg floor on the clean subset .69 | private record repoqa/RESULTS.md |
| enum / enum2 | 2026-10-16 | which new sets are worth building | ranked lists; see Set portfolio | docs-for-code first, then co-change for a diff, issue-to-fix, masked region | private record enum2/RANKED2.md |

## Agent A/B
| id | date | question | verdict | headline | record |
| --- | --- | --- | --- | --- | --- |
| U1 (0.6B pass) | 2026-10-11 | do agents use LCI and save effort (9 tasks)? | inconclusive (state 6) | uncued use 0 of 27 runs, cued 14 of 27; calls dM1 -0.2224 [-0.438, -0.038] against bar -0.223; tokens dM2 +0.318 | private record ab/U1-RESULTS.md |
| I-10, pooled_q1 | 2026-10-14 | does an agent do better with LCI than with native tools? | CONTINUE (necessary condition met; later, smaller A/B only) | 1,208 runs incl. arm C; D minus A calls -0.229 [-0.328, -0.106] (20.5% fewer), tokens +0.350 [+0.235, +0.507], success -0.026 [-0.077, +0.030] | private record ab-analysis-1015/pooled_q1/RESULTS.md |
| I-10, pooled_q2 | 2026-10-14 | same, run set without arm C | INCONCLUSIVE (extend external tasks toward 20 per repository) | 1,122 runs, arms A and D only; the decision of record between q1 and q2 is NOT documented | private record ab-analysis-1015/pooled_q2/RESULTS.md |
