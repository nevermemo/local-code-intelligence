# A/B pilot pre-registration: addendum draft (six items)

**DRAFT for the owner. Nothing here is decided.** Written 2026-10-12 (`date -u` 18:37Z) by
the spike-trim run. The agent A/B's pre-registration (I-10) lives in the owner's local
untracked notes, not in this repository, and was not read for this draft. This file proposes
wording for the six items the independent sanity review asks to pre-register before the
pilot (`reports/sanity-spike.md` on `cloud/sanity-spike`, section 4 and section 5 item 1),
each with its reason, for the owner to move into the local pre-registration, rewrite or
drop. Numbers and choices in square brackets are placeholders for the owner. Every
sentence, including the unbracketed policies (which stratum is primary, what voids a task,
which token comparison is primary), is proposed wording for the owner to change, not a
decision.

If an item already exists in the local pre-registration (SC1's amendments, A24, adopted a
tool-usage rate, two sensitivities and an embedder-noise rule; `docs/development/roadmap.md`,
"Strategy checkpoint"), keep the existing wording and use this one only for what it adds.
The labels D1 to D6 are for this draft only; renumber them when moving them (for example as
the next amendments after A24).

Context the items rest on (Read, `docs/development/roadmap.md` at `66f999d`): the A/B arms
are native tools only / plus language-server navigation / plus LCI fast / plus LCI deep, the
native tools in every arm; tasks from 15 public repositories, 20 generated content
repositories and the 45 replayed-commit tasks of this repository (line 77). SC2 is the
sanity checkpoint after the pilot and before the full pass (line 82).

## D1. Strata rule (the ceiling)

Proposed wording:

> Before any treatment arm runs, the native-tools-only arm runs every task [k] times. A task
> belongs to the primary stratum "baseline-hard" if that arm fails it in at least [one] of
> the [k] runs; the remaining tasks form "baseline-easy". The primary hypothesis is tested
> on baseline-hard; the all-task result is reported as secondary. The stratum of every task
> is frozen, and the list hashed into the pre-registration log, before the first treatment
> run. No task is added, removed or re-stratified after any treatment result is seen; voids
> follow the pre-declared void rule, applied without knowledge of the arm.

Reason: commit-mined tasks on small, well-named repositories are lexically easy, so every
arm can tie at the ceiling. On this repository the gold file was already in the top-24 pool
for 44 of 46 queries (roadmap, line 76), and 9 of 34 bug-localisation items leak a gold
identifier into the question (`docs/spike/multimap/gold-bugloc.md` line 56). Selecting the
stratum from the baseline alone, before treatment data exists, keeps the selection blind.

## D2. Contamination control

Proposed wording:

> Each trial runs in a checkout of the task's base commit with no `.git` directory (or with
> history truncated before the fix commit). Tasks whose fix commit predates the evaluated
> model's training cutoff are tagged `pre-cutoff`; results are reported for `pre-cutoff`
> and `post-cutoff` tasks separately, and mining prefers post-cutoff commits where the
> repository has them. A leak scan in the manner of contract C9 (`docs/spike/multimap/specs/
> contract-kit.md`, C9) runs on everything the agent receives (task text, checkout, tool
> outputs) for the gold identifiers and fix text; a hit voids that task in every arm.

Reason: public fix commits are in frontier models' training data, and a checkout's `.git`
can hold the answer. One audit found agents recovering gold from history in 18-25% of
passing trials (search summary in the review, unverified; carries no weight beyond
motivating the control). C9 already exists for map inputs; the agent's own inputs are the
larger leak surface.

## D3. Minimum effect of interest and pilot-based power

Proposed wording:

> The minimum effect of interest for each primary metric is fixed before the pilot:
> task success [+X points absolute], tool calls [-Y%], tokens [-Z%]. The pilot runs [n]
> tasks in every arm, [k] runs each, and estimates the per-task between-run variance of
> each metric. The full task count is derived from that variance for [80%] power at a
> two-sided [0.05] level, paired by task, for the minimum effect. If the derived count
> exceeds the feasible budget, the pre-registration records that the full pass is
> underpowered for that effect before it starts, and a null result on that metric is then
> reported as "not resolved", not as "no effect".

Reason: U1, with 9 tasks and 3 runs per arm, was inconclusive by about 0.0006 on its bar
(roadmap, line 204). Vendor-reported online effects of semantic search for agents are 0.3%
to 2.6% (search summary in the review). With agent-run variance, 15 repositories times 20
tasks is unlikely to resolve a 2% effect and can resolve a 10% one (the review's inference,
not a calculation); the pilot is where that becomes a number.

## D4. Tool-list equalisation

Proposed wording:

> Every LCI arm exposes the search-only tool list (`search_code`, `search_symbols`,
> `find_definition`, `find_references`; roadmap row N31), not the maintenance tools. Token
> and cost metrics are reported twice: as measured, and net of the tool-description tokens
> each arm carries (measured per run from the request). The primary token comparison is the
> net one.

Reason: every arm beyond native tools carries more tool text before any search happens. The
full LCI list cost +7.2k tokens per run in the D arm with 0 calls (49.2k against 42.0k for
A), and the six maintenance tools are about 35% of the list (N31, roadmap line 297). Without
equalisation the token metric partly measures the tool list. N31 is not yet implemented
(its row is open), so this item depends on it or on an equivalent per-arm tool filter.

## D5. Uptake as a primary covariate

Proposed wording:

> For every arm and task, the share of runs in which the arm's added tool is called at least
> once is reported beside every outcome, as a primary covariate. The cue policy is fixed
> before the pilot: either [a one-sentence cue naming the available tools in every arm,
> native-only included] or [no cue in any arm]. An arm whose added tool is called in fewer
> than [p%] of pilot runs is labelled "not exercised", and its outcome is reported as the
> effect of carrying the tool list, not of using the tool.

Reason: in U1, 0 of 27 uncued runs called any LCI tool; with a one-sentence cue 14 of 27 did
(exploratory; roadmap line 204). An arm whose tool is never called measures the tool list.
A cue in only some arms would confound the arm with the cue.

## D6. Arms, not iterations

Proposed wording:

> The A/B's conclusions are limited to the four product arms. No map, map version or
> combination of maps is ranked from A/B data. Ranking map iterations needs scripted,
> replayable agents with per-map leave-one-out and contract C7 controls, and is built only
> if the A/B says CONTINUE (overall or in a stratum).

Reason: a free agent loop is not pairable by `compare` (`docs/spike/multimap/
architecture-map.md`, section 9.5), and the A/B answers "does LCI help", not "which map
helps". Writing the limit down in advance stops a post-hoc reading of arm differences as a
map ranking (C7: `docs/spike/multimap/specs/contract-kit.md`).

## Related, not one of the six

Feasibility at 15 public repositories times 20 tasks is itself unmeasured: the A/B task
miner already saw a deno index die at 36 minutes (roadmap, line 662), and 16 of grpc's 20
tasks have unindexable gold (C/C++ headers not indexed; roadmap, line 93). A per-repository
indexing dry run before the pilot would show which repositories the full pass can carry.
