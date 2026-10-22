# Evaluation method

Status: PLAN for the sections marked PLAN (simulated agent, walk-on-SCIP premise test, error-to-commit and trait-set plans, A/B method notes,
baselines policy); every other section states what exists on 2026-10-17. Measured results are not here: they are indexed in
[results.md](results.md). Scorer commands and file formats: [v2/gold-scorers.md](v2/gold-scorers.md). The engine under test:
[engine.md](engine.md).

## Sets
Gold files are private (outside the repository) except the two seed sets below. All sets are scored on their own; none is pooled with another.

| set | items | state (2026-10-17) |
| --- | --- | --- |
| impact | 141 primary (bands: unique 81, common 35) | frozen, tree `bfaff44` |
| flow | 111 (55 headline) | frozen |
| where-to | 59: registration 25, exemplar 20, convention 14 (the 30 tantivy items are a loss guard only) | frozen |
| history | 110 (families H1, H2, H3a, H3b) | frozen |
| instructions | count in the gold file (families I1, I2) | frozen |
| error | 34 | regression check, not discriminating (see Set portfolio) |
| tests-for-code | 494 (dev 167, test 327; 92 test tasks) | frozen; stage 2 and 2b done |
| docs-for-code | 548 | stage 1 frozen; stage 2 dev run unscored, test not run |
| issue-to-fix (I1) | 100-item pilot passed; full build stopped 2026-10-17 | 37 rows (20 dev, 17 test), no metric |
| masked region (R1) | 146 kept (dev 58, test 88) | frozen; stage 2 done |
| seed questions | 28 | public, `eval-seed.json` |
| bug localisation | 34 | public, `gold-bugloc.json` |
| external held-out (where-to exemplar, file) | SWE-PolyBench Verified (MIT, revision `b3fca77`): 382 rows, 382 items (leak 298, clean 84); LongCodeArena bug localization test split (Apache-2.0, revision `f2bb26a`): 150 rows (py, java, kt), 100 items (leak 85, clean 15), the 50 Kotlin rows dropped as `no_member` | built (K4a d484, K4b d499); data in the owner's gold directory, never in the repository; never used for tuning or for choosing defaults, cycle 2 reads it once, at the end, as a held-out check; the leak stratum is large (78 % and 85 %), so cycle 2 reads leak and clean separately |
| trait-dispatch / co-change | trait-dispatch not built (the trait-implementation set ran, d392) / co-change parked | see Set portfolio |

## Frozen tree rule
The gold of the first four sets was derived on commit `bfaff44`; the local tag `eval/bfaff44` names it and benches run against a worktree of it
(`cargo xtask maps bench --tree`). The tag is never moved. Frozen sets, preregistrations and result files are never edited; a new column is
computed from per-item outputs that already exist, and corrections are dated notes. External-repository sets pin one parent commit per item and
verify their frozen files by hash (`verify_freeze`) before and after every run.

## Scorers and reporting rules
- Scorers: `cargo xtask gold score|baseline|to-evaluate` and `cargo xtask maps bench` (writes `supplement.md`). Reporting rules (2026-10-16,
  additive) and the "Sets beside the frozen five" table: [v2/gold-scorers.md](v2/gold-scorers.md).
- Ceiling metrics: impact first-ring declaration recall, flow path recall and the error set's message-to-site question are near a ceiling. A delta
  on one is not evidence without the baseline column beside it (numbers in [v2/engine-results.md](v2/engine-results.md), "Reading rules").
- History rows H1, H3a and H3b are not used for decisions. Flow tables carry answer precision and tau beside path recall.
- Speed is judged by work counts (pops, hops, expansions, reranker calls), not wall time; wall time only for model latency.
- Every comparison states the paired bootstrap interval (10,000 resamples) and Holm correction across sets; a null or underpowered result is
  reported as "not resolved".

## Metric tiers (owner approved for recording, 2026-10-17)
Agent runs are the ground truth but too costly to run often; cheap proxies run constantly and are checked against agent runs now and then (the
offline/online pattern of search engines). Quality and cost are reported together so no single proxy is optimised blindly (Goodhart).

| tier | when | metrics |
| --- | --- | --- |
| 1 | every change; deterministic, nearly free | hit@k, recall@k, span hit, MRR on the frozen sets; speed proxies (pops, hops, expansions); cost-aware recall (recall per 1k tokens returned, or tokens read before the gold; plain recall@k rewards returning more); the simulated agent (below) |
| 2 | milestones; cheap model in the loop | confidence calibration (when LCI says 90%, is it right about 90% of the time? a trustworthy "this is it" lets agents stop searching); a small fixed subset judged by a cheap model, only where exact rules cannot decide |
| 3 | rare: before releases, after big changes | real agent A/B runs (N3 and N3b style); their main job is to validate the proxies (did tier-1 gains become agent token and success gains? pv2 passed once, Spearman +0.699). When a proxy stops tracking agents, fix the proxy |

Adoption cannot be proxied by retrieval scores (N3b: LCI was 2.7% of agent tool calls). A local-only log of real sessions (LCI call rate; an LCI
call followed by a grep for the same thing as a failure sign; returned files opened as a help sign) is DEFERRED until LCI is used in real
development sessions (owner, 2026-10-17). Until then tier-3 runs are the only adoption measure; occasional use of the CLI is recorded as
qualitative notes, never as metrics.

## Simulated agent (PLAN: design approved for recording 2026-10-17, not built)
A rule-based search loop with no LLM in the loop. Four rules replace an agent's decisions:
1. First search: identifier-like terms from the question (camel/snake split, stop list, rarest kept); the first grep is an OR over the top terms.
2. Read: the top N files by match count, as windows of about 40 lines around matches; every token read is counted.
3. Next search: follow the identifiers. Parse the code just read with tree-sitter (calls, types, imports) and grep their definitions and usages,
   breadth-first, under a budget of tool calls and tokens.
4. Stop at the first time the gold appears in something read. The score is calls and tokens until the gold, or a failure at the budget.

The LCI arm makes an LCI call first, reads the returned chunks, then falls back to the same grep chain; both arms use the same token accounting.
Guards for the known limits:
- It cannot paraphrase, so conceptual questions underrate grep. Guard: a cheap model proposes search patterns once per question, cached and
  replayed, so the simulator stays deterministic.
- Stopping at the gold makes absolute numbers meaningless; only differences between arms under the same policy count.
- Policy bias. Guard: run two or three policies (greedy, breadth-first) and trust a change only if it helps under all.
- Calibration step: compare once against the real N3/N3b transcripts (first grep terms, step counts; do the rankings agree?) before trusting it.

Prior art (preprints, summaries only): IR user simulation (sDCG, effort-vs-effect, simulators validated against real sessions); coding-agent
papers that measure tokens next to localisation (reads and searches are more than half of agent tool turns and nearly half of its tokens).

## Framing: Wikirace on a code graph
LCI's core loop is a Wikirace on a code graph with a (planned) System 1 racer and compiler-grade links (summary in the root [README](../../../README.md)).
Inspiration: an external Wikirace demo (vendor-reported 0.114 s against 8.566 s for a large model; no published source found for the hop-count
win). Mapping: pages are symbols, files, tests, docs and commits; links are edges (calls, implements, tested-by, mentioned-in, changed-in);
picking the next link is the walk policy; hops to the target are the pops/hops proxy.

Consequences for evaluation: (1) a racer is only as good as its links; without SCIP and doc edges the graph is sparse and traverse equalled
search, so building the edges is building the map; (2) the target is described, not named, so the racer must judge closeness and decide when to
stop, which is why confidence calibration is a tier-2 metric; (3) the N4/N4b local head losing means that head was too weak, not that System 1
does not fit; the planned racer is a local model with an allowed licence, chosen per role. No such rung is built: the ladder runs the agent's rules,
then the calling agent, and `steer.ladder` rejects `system_one` ([engine.md](engine.md), "Known gaps").

## Walk-on-SCIP premise test (PLAN: sanity adjustment 1, owner approved 2026-10-17)
Run right after doc types and Rust SCIP, before more walk features: the trait-implementation questions, and the R1 re-run with the walk enabled.
Why: traverse's first stage is hybrid search, LCI's proven asset (R1 span hit@10 .64 against BM25 over function units .19; I1 +.157 over rg); the
walk was never measured with SCIP on the R1 and external-repository trees (R1 stage 2 ran the walk on an empty SCIP file); on the frozen tree
`bfaff44`, which has a SCIP index, it was measured ([v2/engine-results.md](v2/engine-results.md)) and the gains were small (N2 stopped, N4 killed, S1). Re-aiming I1 and D1
at traverse only makes sense after the edges exist.

## Error-to-commit set (PLAN: sanity adjustment 5, owner 2026-10-17)
Re-aim the error set: "which code changes are responsible for this error". Steps: find where the error occurs (grep is fine), find related code,
find where that code changed in history, return those changes. Gold is SZZ-derived (blame the fix commit's lines at its parent) and needs
verification; first check that enough usable fix commits exist. Baselines: grep for the first step, `git bisect` where a reproducing test exists
(LCI's pitch is ranking culprits without building or running).

## Trait-implementor set and doc types (PLAN: sanity adjustments 5 and 6, owner 2026-10-17)
"Which types implement this trait" was never built (34 qualifying traits against a bar of 40, 33 of them in tantivy), so LCI was never measured on
it; it should be trivial from a code graph. Hypotheses to check: (1) no SCIP in the R1 and external-repository trees, so traverse falls back to chunks (seen in R1 stage
2); (2) the SCIP implementation relationship may not be ingested; (3) the LSP channel may not send `textDocument/implementation`; (4) Rust derive,
blanket and macro impls. Baseline beyond grep: an LSP-MCP tool (rust-analyzer `textDocument/implementation`, Serena-like). Doc file types come in
two parts: adapters for prose (.md .rst .adoc .txt) first; structured formats (.json .yaml .toml) start on the grep fallback with size and
generated-file filters (lockfiles, fixtures). Today: 18 extensions, of which only `.md` is a doc format (README table).

## Set portfolio (2026-10-16)
A premise check of the candidate sets, before further set work. Nothing frozen changed. The dated decisions are in
[../../decisions.md](../../decisions.md).
- Error set (34): kept as a regression check. Grep saturates it (hit@10 .971 with the stored fragment, .735 with the whole quoted message); `lci
  search` reaches .50 (.59 on the quoted message), an exact-message routing gap. Report the grep columns beside any search score; the set
  discriminates only on wrapped prefixes, example values and the entry-point chain.
- Trait-dispatch: not built (above; the lci tree has one qualifying trait, gust none); do not propose it again without a second repository that
  has traits.
- Co-change: parked 2026-10-16, dropped as a question set by the owner 2026-10-17. Prior co-change hit@10 is .456 and the same-directory baseline
  is .500, so a set would not discriminate.
- New sets tests-for-code and docs-for-code: frozen, beside the five and scored on their own. Docs-for-code (548 items) seed-only baselines reach
  recall@10 .386; BM25 on the commit message .730.
- Model-built sets enter only after their own premise check and freeze.
- Exact-message routing: killed by its pre-registered rule (Stage B not built). With the reranker on, search on the quoted message reached .647
  against grep .735, inside the .10 kill band; whole questions score .53 to .56. Agents grep a quoted message first, and grep plus search together
  already cover what routing would add. (Parked 2026-10-16 when pre-registered; the kill is the later update.)
- Second enumeration pass (models allowed) ranked docs-for-code > co-change for a diff > issue-to-fix > masked region questions > masked
  docs-to-code. RepoQA's needle-function task has the masked-region design but was not adopted (29.3% of its descriptions are leak-clean, against
  a 40% bar). Issue-to-fix overlaps the SWE-bench family but is built in-house (pinned public trees, public sets likely in training data); it
  borrows a post-cutoff stratum and file-level metrics. The issue-to-fix pilot and the masked-region build were running on 2026-10-16 (later
  state: [../../decisions.md](../../decisions.md)).

## Slice order and freshness (2026-10-16 direction; results are in results.md)
The 2026-10-16 slice sequence N0, N0b, N1, N2, N3, N4 is finished (the heading keeps its old name for links); nothing in this section is a run instruction. What to do next is in the
[roadmap](../../development/roadmap.md) only; the slice designs are decisions d346 to d351 in [../../decisions.md](../../decisions.md). Each slice
asked one question, and its outcome is in [results.md](results.md) ("Engine benches" for N0 and N0b, "Slices N1 to N4 and the model roles" for the rest):
- N0: first stage alone against first stage plus walk at a matched answer budget. Outcome: the walk is the default on flow and where-to, opt-in on
  impact and history (results.md, row N0).
- N0b: the same arms with and without a local judge as result filter. Outcome: no filter adopted (row N0b).
- N1: chained-query plans executed offline (Wizard of Oz). Outcome: killed; recipes are the agent-facing form (row N1).
- N2: an in-request edge bandit. Outcome: no verdict, then stopped; `edge_bandit` stays off (row N2).
- N3: a chained executor, or recipes only. Outcome: recipes survived the frozen rule (rows N3, N3 follow-ups); off-shape tasks were null (row N3b).
- N4: a result filter trained on offline labels. Outcome: killed (row N4).

Method facts that outlive the sequence:
- Freshness is out of scope for now (owner 2026-10-16): the spike works on frozen trees, so every N1 and N3 task runs read-only on the frozen
  tree. One review called a freshness contract a blocker for live use; that stays true for a release.
- Later F1 (verify on read): every answer carries the content hash of its file and is checked against the live file before it is returned; a stale
  answer is recomputed from live operators or marked stale; precomputed maps become accelerators trusted only on a hash match.
- Later F2: measure staleness after real edits (no benchmark has done so; all ran on a frozen tree), including whether the SCIP-based maps rebuild
  after an edit.
- Learned components (N4 included) train on patterns, never on node identities, and are tested on a later commit than the one they were trained
  on.

## Baselines policy (PLAN for the unbuilt arms)
Every claim is a measured difference against a baseline, never against an assumption:
- Lexical: ripgrep plus file reads; BM25 over files and over function units; for history questions a plain `git` arm as a separate arm. Record per
  question and arm: hops, files read, tokens read and spent, and whether the answer matched.
- Trait set: LSP-MCP baseline beside grep. Error-to-commit: `git bisect` where a reproducing test exists. Exact-message questions: grep columns
  beside any search score.

## Agent A/B method notes (D1 to D6: draft proposed 2026-10-12, adoption not recorded here)
DRAFT for the owner, nothing here is decided: every sentence, including the unbracketed policies (which stratum is primary, what voids a task,
which token comparison is primary), is proposed wording for the owner to move into the local pre-registration, rewrite or drop; bracketed values
are placeholders. The I-10 pre-registration is owner-local and untracked, and was not read for the draft. The six items are the ones the
independent sanity review asked to pre-register before the pilot (its report `reports/sanity-spike.md` on branch `cloud/sanity-spike`, section 4
and section 5 item 1). Where an item already exists in the local pre-registration (SC1's amendments, A24: a tool-usage rate, two sensitivities and
an embedder-noise rule; roadmap "Strategy checkpoint"), keep the existing wording and use these only for what they add. The labels D1 to D6 belong
to the draft; renumber them when moving them (for example as the next amendments after A24). SC2 is the sanity checkpoint after the pilot and
before the full pass. Outcome of the run: [results.md](results.md), row I-10. Arms: native tools only / plus language-server navigation / plus LCI
fast / plus LCI deep, native tools in every arm; tasks from 15 public repositories, 20 generated content repositories and the 45 replayed-commit
tasks of this repository.
- D1 Strata (the ceiling). The native-only arm runs every task [k] times before any treatment arm; a task is "baseline-hard" (the primary stratum)
  if it fails in at least [one] of [k] runs, the rest form "baseline-easy". The primary hypothesis is tested on baseline-hard; the all-task result
  is secondary. Strata are frozen and the list hashed into the pre-registration log before the first treatment run. No task is added, removed or
  re-stratified after any treatment result is seen; voids follow the pre-declared void rule, applied without knowledge of the arm. Reason:
  commit-mined tasks on small, well-named repositories are lexically easy, so every arm can tie at the ceiling (the gold file was already in the
  top-24 pool for 44 of 46 queries; 9 of 34 bug-localisation items leak a gold identifier); choosing the stratum from the baseline alone, before
  treatment data exists, keeps the selection blind.
- D2 Contamination. Each trial runs in a checkout of the base commit with no `.git` (or history truncated before the fix); tasks whose fix
  predates the evaluated model's training cutoff are tagged `pre-cutoff`, results are reported for `pre-cutoff` and `post-cutoff` apart, and
  mining prefers post-cutoff commits where a repository has them. A leak scan in the manner of contract C9
  ([specs/contract-kit.md](specs/contract-kit.md)) runs on everything the agent receives (task text, checkout, tool outputs) for the gold
  identifiers and fix text; a hit voids the task in every arm. Reason: public fixes are in training data and a checkout's `.git` can hold the
  answer; one audit found agents recovering gold from history in 18-25% of passing trials (unverified search summary, motivation only). C9 exists
  for map inputs; the agent's own inputs are the larger leak surface.
- D3 Minimum effect and power. Fix the minimum effect per primary metric before the pilot (success [+X points], tool calls [-Y%], tokens [-Z%]);
  the pilot runs [n] tasks in every arm, [k] runs each, and estimates the per-task between-run variance of each metric; the full count gives [80%]
  power at two-sided [0.05], paired by task. If it exceeds the budget, record before starting that the full pass is underpowered for that effect,
  and report a null on that metric as "not resolved", not "no effect". Reason: U1 (9 tasks, 3 runs per arm) was inconclusive by about 0.0006 on
  its bar; vendor-reported online effects are 0.3% to 2.6%; 15 repositories times 20 tasks is unlikely to resolve 2% and can resolve 10% (the
  review's inference, not a calculation); the pilot turns that into a number.
- D4 Tool-list equalisation. Every LCI arm exposes the search-only list (`search_code`, `search_symbols`, `find_definition`, `find_references`),
  not the maintenance tools; token and cost metrics are reported as measured and net of the tool-description tokens each arm carries (measured per
  run from the request), and the net one is the primary comparison. State of the list: [../../decisions.md](../../decisions.md) d221 records N31
  as a decision (profile key tools = "search", commit `0aa374c`). Reason: every arm beyond native tools carries more tool text before any search;
  the full list cost +7.2k tokens per run in arm D with 0 calls (49.2k against 42.0k for A) and the six maintenance tools are about 35% of the
  list. Without equalisation the token metric partly measures the tool list.
- D5 Uptake as a primary covariate. Report beside every outcome, for every arm and task, the share of runs that call the arm's added tool at least
  once; fix the cue policy before the pilot (a one-sentence cue naming the tools in every arm, native-only included, or no cue in any); label an
  arm "not exercised" below [p%] pilot uptake and report its outcome as the effect of carrying the tool list, not of using the tool. Reason: in
  U1, 0 of 27 uncued runs called any LCI tool; with a one-sentence cue 14 of 27 did (exploratory). An arm whose tool is never called measures the
  tool list; a cue in only some arms would confound arm with cue.
- D6 Arms, not iterations. Conclusions cover the four product arms only; no map, map version or combination of maps is ranked from A/B data.
  Ranking map iterations needs scripted, replayable agents with per-map leave-one-out and C7 controls, and is built only if the A/B says CONTINUE
  (overall or in a stratum). Reason: a free agent loop is not pairable by `compare` (the claim is made here; the draft's pointer to
  `architecture-map.md` section 9.5 does not carry it), and the A/B answers "does LCI help", not "which map helps"; writing the limit down in
  advance stops a post-hoc reading of arm differences as a map ranking.
- Feasibility (not one of the six): the task miner saw a deno index die at 36 minutes, and 16 of grpc's 20 tasks had unindexable gold when C/C++
  headers were not indexed (they are since commit `bbd3292`). A per-repository indexing dry run before a pilot shows which repositories a full
  pass can carry.

## Seed questions (28)
The questions live in [eval-seed.json](eval-seed.json) (ids q01 to q28); [eval-seed.md](eval-seed.md), kept frozen, is the readable form of the same set. Counts: easy 4,
medium 16, hard 8; `needs_model_service` is false for all. Generated 2026-10-12T04:22Z; revised 06:51Z (fact-check corrections to q07, q08, q11,
q13, q14, q15, q22 and q23; baseline expectations and the Baseline section added). Base commit `7f9c636` (`origin/fix/verified-bugs`, 188 commits
of history). Answers, co-change counts, recency and commit answers were verified there. The spike branch adds only docs under
`docs/spike/multimap/`, which no question references by line and which co-change counts exclude. History counts by revision: 188 at `7f9c636`, 191
at spike commit `842a5e9` (the count first written into q08), 192 at `b5e5851`; a shallow clone gives wrong counts, use a full clone.
- Purpose: multi-hop questions about this repository, each needing at least two kinds of map (lexical, LSP symbols/references, call graph, module
  graph, language map, agent instruction files, git history, test links, config keys, error/log strings, entrypoints, build graph, trait/impl
  hierarchy, doc-to-code); a plain grep of the question text should not be enough; every answer location was checked at the base commit by reading
  the file, a grep or `git log`, and the commands are listed per question.
- Map-kind coverage (questions per kind, 20 kinds; derivable from `maps` in the JSON): `call_graph` 22, `test_link` 21, `config_key` 11,
  `git_commit_message` 9, `error_string` 7, `agent_instructions` 5, `doc_to_code` 5, `entrypoint_xtask` 5, `module_graph` 5, `trait_impl` 5,
  `build_graph` 4, `entrypoint_mcp` 4, `language_map` 4, `lsp_symbol` 4, `entrypoint_cli` 3, `git_cochange` 3, `entrypoint_rest` 2, `git_blame` 1,
  `git_recency` 1, `lsp_reference` 1.
- Schema: top level `schema_version`, `generated`, `revised`, `base_commit`, `repository` (`self`), `history_counts`, `baseline_expectation_note`,
  `questions[]`. Each question has `id`, `question`, `maps` (map kinds needed), `hops` (expected traversal steps), `answer[]` (`path`, `symbol`,
  `lines`, `role`), `verification[]` (`method`: `grep` | `read` | `git_log`, plus `detail`), `needs_model_service`, `difficulty`,
  `baseline_expectation` (`guess`: `easy` | `needs-reading` | `needs-structure`, plus `basis`; the author's guess, not a measurement) and an
  optional `notes` string.
- Conventions: `path` is repo-relative; a commit answer holds the short sha in `path` and no `lines`. `lines` is one-based and inclusive and
  usually spans the whole item (function, impl, test); the specific line inside it is given in `role`, for example `caller (line 195 ...)`.
  Co-change answers leave `symbol` and `lines` empty and put `k of n commits` in `role`, counted without `--follow`. `verification` commands use
  short forms (`rg` for ripgrep); the co-change command is `git log --format=%h -- <file> | xargs -n1 git show --format= --name-only | sort | uniq
  -c | sort -rn`.
- Baseline: the chains are real, but a lexical search plus reading files can recover most of them (a distinctive string from the question, such as
  an error text, config key or hook name, usually lands on the first hop). The `maps` labels, written by the author who picked the questions, show
  which kinds of map could serve a hop, not that graph traversal is needed. So every evaluation includes an agent with ripgrep and file reads only
  (plain `git` CLI as a separate arm for history questions). Per question and arm record: hops (search or navigation calls); files read (count and
  paths); tokens read and spent; whether the answer matched (per answer row, using the line-drift rule below). "Multimap helps" is a measured
  difference against that baseline, not an assumption.
- Guess classes: `easy` = one grep plus reading one or two files answers it; `needs-reading` = lexical search finds the entry point but the answer
  needs following several references by reading; `needs-structure` = the working tree alone cannot answer it (git history counts, recency or
  commits), and with plain `git` allowed most of these probably drop to `needs-reading`. Guesses: easy 1, needs-reading 13, needs-structure 14;
  every needs-structure guess is there because of git history, none because of a code graph, which is the main reason to measure before building
  graph maps. The 14 history questions: q04, q07, q08, q09, q10, q11, q12, q16, q19, q20, q21, q25, q26, q27.
- Line drift: line numbers hold only at the base commit. On a later commit the symbol is the primary key (resolve it with
  `search_symbols`/`find_definition` or a grep for its `fn`/`const`/`impl` line; accept an answer at the same symbol even if lines moved); lines
  are a secondary check and a tie-breaker when a symbol name occurs more than once. Commit answers match by sha. Co-change counts only grow, so
  re-run the recorded command and compare rankings, not exact numbers. A renamed or removed symbol means re-verifying the question by hand. The
  JSON note "see eval-seed.md, Baseline" refers to the Baseline section of the frozen [eval-seed.md](eval-seed.md), summarised in the Baseline and
  guess-class items above.

## Bug-localisation set (34 items)
Data: [gold-bugloc.json](gold-bugloc.json). Generated 2026-10-12T08:55Z from `origin/fix/verified-bugs` at `bfbe51d`. Each item is a real bug
fixed on that branch, asked as a symptom the way an agent or user would meet it; the gold is where the fix landed; the tree to search is the fix's
parent. Fields: `id`, `row`, `fix_commit`, `base_commit`, `question`, `gold_files`, `gold_symbols`, `change_kind` (wrong-result, panic,
hang-or-timeout, error-reporting, validation, resource, other), `name_leak`, `notes`.
- Method: (1) candidates from `git log origin/fix/verified-bugs --format='%H %s' --grep='^N[0-9][0-9]*[a-z]*: '` gave 39 commits; for each, the
  matching row of `docs/development/roadmap.md` (on that branch) and the commit body were read; (2) kept only if the diff changes non-test Rust
  under `src/`, `lci-core/src/` or `xtask/src/` (test code = under a `tests` directory, or every changed line inside `#[cfg(test)]`, `mod tests`
  or a `#[test]` / `#[tokio::test]` function); (3) gold files are the changed non-test files that exist at `base_commit`; a file the fix creates
  is not gold (an agent cannot find it in the base tree) and is named in `notes`; (4) gold symbols are functions whose body or signature holds a
  changed non-test line (small brace-matching Rust scanner over the `-U0` hunks of both sides of the diff, kept only if the function exists at
  `base_commit`; form `Type::method` inside an `impl` (type = last path segment) or trait, `module::fn` for a free function, module = file stem or
  the directory for `mod.rs`); functions the fix adds are listed in `notes` ("Fix also adds"), and files with item-level changes only (struct
  fields, constants, `use` lines) are listed in `notes` and give no symbol; (5) the question is one or two sentences of symptom written from the
  row text and commit body, with checks asserted by the build script: no gold file name, no gold symbol as a whole word, no row id, no word "fix";
  (6) `name_leak` is true when an identifier token of the gold appears in the question: tokens come from gold paths (directories and stem, without
  `src`, `lci-core`, `xtask`) and gold symbols, split on `::`, `_`, `/`, `.`, `-` and camelCase, lower-cased, length 4 or more; a question word
  matches a token if it equals it or adds `s`, `es`, `d`, `ed` or `ing`; the leaked tokens are listed in `notes`. Generic leaks were reworded
  where cheap (for example "chunk" became "snippet"); the remaining 9 are domain words a reporter would use (`rust`, `language`, `java`, an LSP
  error name such as `RequestFailed`, a quoted error message such as "indexing failed while waiting").
- Counts: wrong-result 13, error-reporting 7, other 4, resource 3, panic 3, hang-or-timeout 2, validation 2, total 34. Gold files 65 (median 1.5
  per item; 17 items have one); gold symbols 90 (median 2, at most 10, `bug-006`, a language-server session change); `name_leak` 9 of 34; two
  items cover two rows each because one commit landed both (N75, N76 and N52, N51), and their questions describe both symptoms.
- Dropped (5 of 39): `55cdbd9` (N17), `f5648a0` (N46) and `49b2748` (N62, pins existing behaviour) are test-only changes; `c1d8d98` (N64-N66)
  matched the grep through its body and is CI and workflow only; `d7f77aa` (N3b) is feature work (a new release endpoint and shutdown on Ctrl+C)
  with mostly new symbols, so no failing behaviour to localise.
- Verification: a fresh subagent of a different model got 9 items (25% of 34 rounded up, drawn with `random.Random(20261012)`: bug-001, 002, 004,
  017, 018, 025, 027, 033, 034) and re-derived gold from `fix_commit`, `base_commit` and `question` alone, and judged leakage and answerability:
  gold files and symbols identical on 9 of 9 (21 symbols; 100% on this sample, the scanner is not verified on the other 25 items); leakage "none"
  for all 9; answerability 8 "yes", 1 "partly". The "partly" was bug-004: the question blamed declaration and usage lookups as well, but at
  `base_commit` those already returned an error for a crashed server and only the symbol lookup hid it; the question was narrowed to the symbol
  lookup and the gold is unchanged, because those two methods' bodies changed too (they gained the new warnings field). Noted, not changed: in
  bug-034 `src/store.rs` is a gold file without a gold symbol (the fix adds a method there); in bug-017 three changed functions are new and not
  gold. Nothing was dropped at verification.
- Caveats: one Rust repository, one branch of fixes, mostly found by review swarms rather than user reports, so the set is small and per-kind
  numbers are anecdotes; the questions were written knowing the fix and lean on the row text, which already describes the mechanism, while real
  reports are vaguer, so treat scores as an upper bound; each item has its own `base_commit` (34 distinct trees) and a runner must index each item
  at it (one tree for all puts the gold at wrong lines or in already-fixed code); several items bundle problems (`bug-007` three; `bug-010` and
  `bug-011` two rows each) and some fixes are wide (`bug-005`: 5 gold files, 8 symbols; `bug-006`: 10 symbols), so recall over the whole gold set
  is harsh and first-hit metrics kinder; symbol extraction is a scanner, not a compiler (macro-generated items and nested functions go to the
  outermost enclosing function; the verification pass is the check); a fix that changed several call sites of a bad pattern scores the sites an
  agent did not find as misses even though finding one has localised the bug.
- Scoring (for a benchmark runner): per item, check out `base_commit`, index it, ask `question`, record the ranked locations. File recall@k (share
  of `gold_files` in the top k distinct files, k = 1, 3, 5, 10) and file hit@k (at least one); symbol recall@k and hit@k, where a returned
  location matches a gold symbol if it lies inside that function's span at `base_commit` (match by span, not name text, so same-named methods are
  not credited by accident); hops (tool calls or traversal expansions until the first gold file and first gold symbol, capped at the budget,
  misses reported apart); tokens (read by the agent and spent by the engine until the first gold hit and in total, steering tokens included for
  steered runs). Report by `change_kind` and split by `name_leak` (leaky items are easier for lexical search), with the mean and a paired
  bootstrap interval against a plain text-search baseline on the same items.
