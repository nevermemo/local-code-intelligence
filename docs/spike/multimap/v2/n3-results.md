# N3 results: do server-side recipes cut agent round trips? (2026-10-16)

Agent A/B for the recipe tools built in N3 (`next-slices.md`, N3; N1 killed the chained-query schema, so only
recipes were built). Agents (Sonnet 5.5 through the SDK runner) answered 40 composite questions about the frozen
tree (bfaff44) under five tool setups. Answers were judged blind (two Sonnet judges, Fable on disagreement) against
gold. The kill rule, the scoring script, the prompts and the task files were frozen before the main run; a pilot
set n and exposed one harness defect (below).

## Verdict: recipes survive

"Survive" means not killed by the frozen rule. It does not mean shown useful in general (see "What is not
supported").

**Arms** (all with Read, Grep and Glob; 2 repetitions per task):

| arm | adds | runs |
| --- | --- | --- |
| C0 | the 4 generic lci search and navigation tools; prompt names lci | 80 |
| C1 | C0 plus `lci_compose` (up to six operator steps with `$sN` references): the thin programmatic-composition control | 80 |
| T | the recipe tools; prompt names lci | 80 |
| U | the recipe tools attached, prompt never mentions lci (uncued) | 72 |
| S | the recipe tools plus a short skill that says when to use them | 40 |

**Tasks.** 40 composite tasks in four families, one per recipe (config readers, impact rings, call paths, config
neighbourhoods), fixed before the run and read-only on the frozen tree. They are recipe-shaped by design, and the
gold was derived from the operators the recipes compose (`config.key`, `scip.impact`, `graph.scip`, SCIP treated as
the truth) and then hand-checked. Items only Grep can find are neutral, so the control's one structural advantage
earns no credit. The round-trip primary is therefore close to tautological for T: one call returns the set that
generated the gold. This is a floor test, not a discovery. The informative part is that C1 had the same operators
behind `lci_compose` and still needed more round trips and recalled less, and that recall did not drop.

**Frozen kill rule** (seed 20261016, 10,000 task-bootstrap draws, one-sided 95% bounds, Holm over the two primaries,
applied against both controls): keep the recipes only if (1) agent round trips fall to at most 0.75 of the control
with the upper bound below 1, and (2) gold recall is non-inferior with margin 0.10. Binary task success is reported
and not gated. (The margin was amended from the early draft's 5-point success rule before the pilot.)

| measure | T vs C0 | T vs C1 |
| --- | --- | --- |
| round-trip ratio (upper95) | **0.469** (0.526) | **0.633** (0.707) |
| recall difference (lower95) | +0.097 (+0.054) | +0.028 (+0.003) |
| rule | pass (needs <= 0.75; recall lower bound > -0.10) | pass |

Holm-adjusted p < 0.0001 on both primaries. `recipes_survive` is true against both controls.

Per run means:

| arm | round trips | tool calls | operator calls | tokens | cost | recall | success |
| --- | --- | --- | --- | --- | --- | --- | --- |
| C0 | 4.34 | 8.4 | 0 | 76.6k | $0.065 | 0.897 | 0.700 |
| C1 | 3.15 | 5.7 | 20.5 | 55.4k | $0.051 | 0.967 | 0.887 |
| T | 1.68 | 2.9 | 43.5 | 38.4k | $0.040 | 0.995 | 0.975 |
| U | 1.81 | 3.0 | 49.8 | 42.1k | $0.043 | 0.987 | 0.944 |
| S | 1.85 | 3.1 | 41.0 | 43.1k | $0.043 | 0.961 | 0.925 |

Success, T minus C0: +0.275 (lower95 +0.175); T minus C1: +0.0875 (lower95 +0.025). Round trips fell, not work:
operator calls moved server-side (T 43.5 per run against C1's 20.5). T sits at the floor: 36 of 80 runs used
exactly one round trip. The one family with a large gap is the composite family (C0 about 6.5 round trips against
T about 2.25); in the call-path family C0 recall is already 1.0.

Round trips count assistant responses with at least one tool call (parallel calls count once, sub-agents excluded).
All 352 runs finished with subtype success; there were no zero-round-trip runs. Wall time is reported only
(T about 11 s, C0 about 20 s) and is not a claim.

## The supported claim, and what is not supported

Supported (the review's scope, exactly): on 40 recipe-shaped questions about this repository at bfaff44 (config
readers, impact rings, call paths, config neighbourhoods), Sonnet 5.5 SDK agents with Read, Grep and Glob plus the
recipe tools finish in about half the agent round trips of the same agent without them (0.47x) and about two thirds
of an agent given a generic operator composer (0.63x), with gold recall at least as high (judged by two blind
Sonnet judges against SCIP-derived gold) and lower token cost. With the recipe tools present and no prompt cue, the
agents called them in every run.

Not supported: benefit on questions that are not shaped like a recipe; benefit for an agent that has Bash (rg,
git); another repository or language; another agent family; latency. The 0.47 ratio is specific to this tool
surface: C0 had no Bash, so it could not pipe searches or use git.

## Uptake (descriptive only)

| arm | any lci tool | recipe tool |
| --- | --- | --- |
| U (72 runs) | 72/72 (Wilson 95% 0.949 to 1.0) | 72/72 |
| S (40 runs) | 40/40 | 40/40 |
| T (80 runs) | 80/80 | 80/80 |
| C0 (cued) | 21/80 (0.179 to 0.368) | n/a |
| C1 (cued) | 64/80 | `lci_compose` 50/80 |

U ran 36 of the 40 tasks (72 runs, 352 in total): the frozen guard rejects an uncued prompt that names lci, and four
frozen task texts do. U is not a kill-rule contrast. Its 72/72 is a match between the recipe tool descriptions and
the question shapes (the descriptions paraphrase the task shapes), not evidence that agents reach for lci unprompted.
N1's uncued arm had only the generic search and navigation tools and its uptake was 4 of 54. The honest uptake test
is the same arm with the recipe descriptions cut to one line (follow-up 6). S (skill) was not better than T
(recall 0.961 against 0.995, 40 runs, descriptive).

Stop reasons in T: complete 164, ambiguous anchor 4 (all impact tasks, 4 of 20 runs), empty 1. No recipe defect
behind them; the threshold for concern was 50%.

## Search and navigation tools: cold-start diagnosis and fix

The pilot put the generic search and navigation tools on hold because they returned empty answers. They are not
broken. The language server starts lazily at the first navigation call, the server waited only 10 s for it, and then
answered empty until indexing finished (10 to 140 s depending on load; reproduced with three cold servers and under
N0b load). Fix, configuration and harness only: the idle timeout set to 0 in the arm configs, a warm-up gate before
each arm, and a four-tool navigation check before each arm. Pilot transcripts: 23 of 26 navigation calls non-empty,
but the first call per server fell in the cold window, so pilot effect sizes are not comparable and were not used
for the verdict. Main run: 64 of 67 navigation calls non-empty, 3 empty (two qualified-name queries, one
find_references), none in a cold window. C0 used lci in only 21 of 80 runs (27 calls), so the cold start did not
shape C0's numbers. Known tool limits seen: symbol search does not list struct fields, and find_references on a
macro-routed method gives 0.

## Judging and its limits

J1 and J2 agreed on 344 of 352 sheets (97.7%); 8 went to the third judge, none unresolved. Limits, none of which
can flip the round-trip primary:

- Judges never saw the neutral items. Real config readers that the gold marks neutral were graded as contradicting
  the gold in a few T and U answers (one config task); every arm is exposed, but the sheet departs from the gold.
- Answer format favours the recipes: Grep output yields file and line, recipe output yields names. Some C0 fails
  cite a test file and line without the test function name, where the question says "name the declarations". Part of
  the success gap is format, not finding.
- A "one of" gold group was shown as two separate items with no "either" note.
- Blinding is partial: tool names were masked, but recipe vocabulary ("ring 1", "witness", "SCIP") survives in
  answers (T 45 and U 41 mention SCIP, C0 none). Direction of bias unclear.
- Estimated: fixing the first three and re-judging would narrow the success gap by a few points.

## Hygiene and run notes

- Path leak (accepted): a machine-specific directory name appeared in some tool results (C0 21 of 80 runs, C1 20,
  T 2, U 2 of 72, S 16 of 40). It follows only a search tool call; recipe tools and `lci_compose` do not leak.
- Scheduling: RAM limits forced one arm server at a time (two at first), so arms ran at different wall-clock times
  on the same day, same model id and same binary in the pilot and main freezes. The first runner was stopped after
  about 130 runs and in-flight jobs re-ran from scratch. Pairing is by task id.
- Cost: about $21.84 for the step: SDK runs $17.26, judges $4.59, under the $40 cap.
- Harness tests: 33 pass; the freeze check was ok after the run.

## Consequences

- The recipe tools are kept behind their flags (`[recipes]`, `[compose]`, both off by default; the flags-off tool
  list, routes and OpenAPI document are pinned to the pre-N3 surface by a golden file).
- The claim stays scoped as above until an off-shape test exists. Recipes stop at trait methods, string-literal
  TOML keys and history.

## Follow-ups, in order of value

1. Off-shape tasks with gold not derived from SCIP operators (where-to and history families, tests-for-code, commit
   replay), scored with the same rule. A null or negative result there is the real scope statement.
2. C0 plus Bash (rg, git), 40 tasks x 2 repetitions. **Running now; result will be appended here.**
3. Re-judge the 352 existing sheets with neutral items shown, the "one of" group collapsed, and "file:line without
   a name counts as named"; report both verdicts. **Running now; result will be appended here.**
4. A second repository with a fresh SCIP index and gold written by someone who has not seen the recipes.
5. Another agent family on the same 40 tasks.
6. Uptake with one-line recipe descriptions (the facts-only rule stands; the question is how much of 72/72 is
   description length).
7. Per-family rows and wall time in the write-up.
