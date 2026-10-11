---
name: lci-autonomous-improvement
description: Operate a long, unattended improvement run on local-code-intelligence - pick roadmap items, plan bounded slices, implement, gate, evaluate, review independently, commit on a feature branch, update the roadmap, and stop at the usage limit or when an approval is needed. Use when the human asks for autonomous multi-hour work; load the task-specific skill for each slice as well.
---

# LCI autonomous improvement

The operating manual for a run that continues without the human for hours. `AGENTS.md` still applies in full; this skill adds the loop, the roles, and the stop rules. Load the matching task skill (`lci-language-adapter`, `lci-lsp-adapter`, `lci-retrieval-evaluation`, `lci-release-validation`) for each slice.

## Non-negotiables

- Work on a feature branch, never `main`. Commit after every passing slice; push the branch after every commit.
- Every slice ends in one of two states: committed and green, or reverted. Never leave half-done work in the tree between slices.
- Never weaken, delete, or skip a test to pass a gate.
- Never tune production ranking to evaluation paths, queries, or fixtures.
- Stop and ask for anything on the approval list in `AGENTS.md`. Approval comes from the human in chat only.
- Check usage after every slice; stop starting new work at 90% of the 5-hour or weekly window, finishing or reverting the current slice first.

## Roles

One session coordinates; everything else is delegated to a fresh subagent with complete context, because subagents are stateless.

| Role | Does | Does not |
| --- | --- | --- |
| coordinator | Owns the roadmap, git, gates, live evaluations, usage checks, and every approval request. Writes the slice plan and the briefs. | Implement large edits itself when an implementer can; skip review. |
| planner | Turns a roadmap item into a slice plan: scope, allowed files, acceptance, eval gate, risks. Reads code, writes no code. | Expand scope beyond the item. |
| implementer | One bounded edit against an allowed-file list, returns a diff summary. | Commit, push, edit files outside the list, run live evaluations, change tests it was not asked to change. |
| reviewer | Reads the diff cold (did not write it) for correctness, boundary violations, test honesty, and doc drift. Returns findings with file:line. | Rewrite the change; approve its own work. |
| explorer | Background research or optimisation hunting with no time pressure: compares options, gathers evidence, proposes roadmap items. "Found nothing" is a valid result. | Edit source; claim results without evidence. |

A client entry point maps these roles to its own subagent mechanism; `AGENTS.md` and this file stay client-neutral.

## Before the first slice

1. Read `AGENTS.md`, `docs/development/roadmap.md`, `git log --oneline -n 15`, `git status --short`, and `ls test-results`.
2. Confirm you are on a non-`main` branch with `origin` configured; create `feature/<topic>` if not.
3. Check services without touching 8765: `curl http://127.0.0.1:8768/ready` (or the embedding service's `/v1/models`). Note which of 8766/8767 are up; do not start anything unless the human already permitted it. On the current development machine 8766 is the vLLM embedder in WSL and 8767 the llama.cpp reranker (roadmap V5); the reranker alone holds roughly 8-12 GB and both models share the iGPU, so never run a third model process (roadmap V1, R4); live numbers are in the roadmap Snapshot, and the embedder's `/v1/models` shows which model is loaded now. Keep heavy compiles away from timed live runs.
4. If the running `local-code-intelligence serve` was started from `target/debug`, note that rebuilding will fail on Windows until it is restarted from a copied binary (see `AGENTS.md` "Running LCI locally"). Ask before stopping it.
5. Confirm the stop condition with the human if the mandate did not state one.

## The loop

Repeat until the stop rule fires or the roadmap has no ready item.

1. **Pick.** Open `docs/development/roadmap.md`. Take the highest-priority item whose status marks it ready and unblocked. Mark it in progress with today's date. If the roadmap is missing or has no ready item, stop and ask rather than invent a backlog.
2. **Plan.** Produce a slice plan (template in [references/templates.md](references/templates.md)): goal, allowed files, acceptance criteria, tests to add, which gate suites apply, whether a live evaluation is needed (yes for anything touching ranking, candidates, reranker input, priors, roles, embedding identity, or latency), and the revert condition. A slice is one logical change; if the plan needs more than roughly ten files or two subsystems, split it into separate roadmap items. Keep the plan in a file so briefs can point to it (the five-line briefs in the templates restate nothing).
3. **Implement.** Brief an implementer with the plan, the allowed-file list, the relevant contract files, and whether the `lci` MCP tools are connected. Inspect the saved diff for file scope and intent before anything else; a valid diff counts even without prose. Fix small gaps yourself or send one focused continuation; do not start a second implementer on the same files.
4. **Gate.** Run the focused suite first (`cargo xtask test --suite <name>` or `cargo test -p lci-core --lib <module>::`), then `cargo xtask test --suite Full` once the slice is complete. See "When a gate fails".
5. **Evaluate (when the plan says so).** Follow `lci-retrieval-evaluation/references/evaluation-workflow.md` exactly: frozen `lci-base`/`lci-new` binaries, scratch configs with short separate data directories, sequential runs, reranked and reranker-unreachable pairs for prior/role/fusion changes, reports under `test-results/`. Do not compile or run tests while a timed run is in progress. Decide with the workflow's decision rule; if the change does not hold up, revert it and record why in the roadmap.
6. **Review.** Brief a reviewer who did not write the code with the diff, the slice plan, and the evidence. It checks correctness, `AGENTS.md` boundaries (ports, data location, fail-open, freshness semantics), test honesty (no weakened or vacuous tests; suites that still match tests), documentation drift (README tables, `config.example.toml`, skill references, MCP tool descriptions), and scope creep. Address every finding or record why not; a disputed finding goes to the human, not silently dropped -- unless the human has delegated disputes, in which case pick one side (at random if both are defensible), log the dispute and the pick in the roadmap, and continue.
7. **Commit and push.** One logical change per commit, message per the template: imperative subject under about 70 characters, a body saying what changed and why, the evidence (test counts, suites, live metrics before/after, report names), and limitations. The client adds the co-author trailer. Stage only the slice's files; never `git add -A` with scratch material present (`reports/`, `research_notes/`, `test-results/` are not slice files). Push the feature branch.
8. **Record.** Update the roadmap item: status, commit hash, evidence summary, follow-ups discovered. Update any docs the reviewer flagged in the same commit as the code, or in an immediately following one if the review found them afterwards.
9. **Check usage and continue.** Every fifth slice, run the triage pass below first. Read the usage figure. Below 90%: run an explorer in the background if none is running and pick the next item. At or above 90%: stop (see below).

## Triage pass

Every five slices, and at the start of every roadmap renewal, revisit what did not land:

1. List roadmap items marked `blocked` or reverted, and any test that fails, is ignored, or flaked during the run.
2. Have a reviewer (fresh subagent) decide each one: **retry** with a new plan (the cause is understood now, or a dependency has landed), **edit** the scope or acceptance into something achievable, or **remove** it (obsolete, superseded, or not worth its cost).
3. Removing or rewriting a test is allowed only when the review shows the test itself is wrong or obsolete (it asserts behavior the product no longer has, or duplicates another test), never to get a failing slice through. Record the reason in the commit body.
4. Record every decision in the roadmap: retried and edited items get a new status and a note; removed items move to a "Dropped" list with the reason, so the history stays readable.
5. Keep the roadmap itself true: refresh the Snapshot's date and facts against `git log`; reconcile each item's header with its table row (the row is the only status); rewrite "In progress" and "Next up" to what is actually in flight, one line per slice named by its item ID plus a letter, never a tier name; move a finished pre-registration and its RESULT into its item rather than leaving it under "In progress".

## Measure, decide, sanity-check

Measurements guide opinions and opinions guide measurements. Keep the two alternating instead of letting either run alone:

- **Measure the premise first.** Before a large design or ideation effort (more than about twenty agents or a day of work), name the measurement that would show its premise is wrong, and run that measurement first when it is cheaper than the effort. A design built on an unmeasured premise is a hypothesis, not a plan.
- **Falsify cheaply.** Prefer small tests with a pre-registered threshold and kill rule over generating and scoring many ideas. Model-assigned scores drift upward and do not replace a measurement; use them to choose what to measure next. Write the pre-registration and get its independent review before anything runs ([Pre-registration and Independent review brief](references/templates.md)).
- **Measure what users get.** Retrieval metrics are proxies. When a decision depends on whether agents benefit, measure agent outcomes (success, tool calls, tokens, wall time) with the agent's native tools in every arm.
- **Sanity checks at phase boundaries:** at every roadmap renewal, before a multi-day effort starts, after a surprising result, and whenever the human asks. A fresh reviewer with no stake (the stronger model, read-only, web research allowed for prior art) answers: is the goal still sound; what is measured and what is assumed; does this already exist elsewhere; what single measurement would change the decision; continue, narrow, pivot or stop. Record the verdict and the decision in the roadmap. The brief and the checkpoint list (SC1-SC5, mirroring the roadmap's "Strategy checkpoint") are in [references/templates.md](references/templates.md).
- **Build in pause points.** Split long multi-agent runs into segments that pass their state through files, so a sanity check or a usage check can stop or re-pace the run between segments without losing work.

## Token efficiency

Long runs are budget-bound; spend tokens on decisions, not on re-reading.

- Short term (each step): cap tool output (`tail`, `grep`, `--stat`, a summary script over JSON reports) instead of printing whole logs, diffs, or reports; read only the section of a file you need; never re-read a file you just edited or already hold; keep status messages to a line or two.
- Briefs: point at the roadmap item and contract files by path instead of restating them; state only what the agent cannot discover (decisions, constraints, prior evidence). Send follow-ups to the agent that already holds the context rather than starting a fresh one.
- Model choice: implementers and explorers on the cheaper coding model; planners and reviewers on the stronger one, and only where a second opinion changes the outcome (non-trivial or ranking-affecting diffs). Skip a separate review for docs-only or mechanical slices.
- Long term (across the run and context compactions): keep durable state in `docs/development/roadmap.md`, commit messages, and `test-results/` reports, so recovery needs three short reads, not a transcript. Write measured results into the roadmap once; reference them afterwards.
- Avoid repeated work: reuse frozen binaries and existing reports as baselines; do not rerun a live evaluation whose inputs have not changed; batch independent tool calls.
- Do not load heavyweight helper skills or tools for edits whose keys you already know; write the file directly.

## Compaction strategy

Context compaction is routine in a long run; plan for it instead of fearing it.

1. Durable first: anything worth keeping is written down before it can be lost. Results and decisions go to the roadmap or commit messages; facts about the machine go to the agent's memory; volatile run state (in-flight agents and tasks, frozen binaries, scratch configs, service state, approvals, pending questions, next steps) goes to a short run checkpoint file outside the repository (current state only, at most 12 KB, in the section order of the "Run checkpoint file" template; delete a finished item at the next refresh).
2. Checkpoint at every push: after each `git push`, refresh the run checkpoint and the roadmap status. A push is the natural boundary where the tree is clean and the state is easiest to describe.
3. Compact early: keep the live context small (roughly a third of the window) so every turn is cheap; the checkpoint makes early compaction nearly lossless.
4. Re-inject on resume: after a compaction, start from the checkpoint, the roadmap's In progress section, and `git status`/`git log -3` (automate this where the client supports a post-compaction hook) before acting.
   Pair it with an alignment check kept outside the repository (its location in the agent's memory or checkpoint): it hashes the always-loaded instruction files and the active run's files against a reviewed baseline, checks push state, services and the checkpoint's size and required lines, and prints only problems. Keep its watch list current (core files always; run files only while a run is active), and re-baseline only after a reviewed change.
5. Summaries keep only what files do not: the standing mandate, approvals with scope, unanswered questions, and anything decided since the last checkpoint.

## When a gate fails

- Read the failure before touching anything. Fix the code if the test is right; fix the test only if the test asserted something the slice legitimately changed, and say so in the commit body.
- If the fix is not obvious within one focused attempt, revert the slice (`git checkout -- <files>` or `git revert` if already committed), mark the roadmap item blocked with the failure, and move on. A revert is a normal outcome, not a failure of the run.
- A `--suite Full` failure caused by an unrelated pre-existing problem is still a stop: fix it as its own slice first, or ask.
- Never pass a gate by `#[ignore]`, loosening an assertion, raising a timeout to hide a hang, or filtering a suite down to zero tests.

## Stop and ask

Stop the loop, leave the tree clean, and ask the human when:

- the next step is on the `AGENTS.md` approval list (downloads, pull requests, pushing `main`, tags, publishing, boundary changes, deleting user data, starting or stopping services);
- a roadmap item needs a product decision the roadmap does not already settle (new dependency, new config surface with unclear default, user-visible behavior change);
- evidence conflicts (tests green but live evaluation regresses, or reviewer and implementer disagree on a contract) and the human has not delegated disputes;
- 8766/8767 are down and you have no permitted way to start them;
- anything would touch 8765.

Explorer findings that need a decision become roadmap proposals, not stop events.

## Explorer briefs

Explorers run occasionally in the background and must not block the loop. Typical topics from the standing mandate: alternative reranker models or strategies (gating, cutoffs, bounded input), embedder options and document representations, LSP channel improvements, whether shelling out to ripgrep beats an in-process lexical engine, and random optimisation hunting in a named module. Each brief states the question, where evidence may come from (code, `test-results/`, cited research notes), and that the output is a short evidence-backed proposal or "nothing found". Explorers never edit source and never start services. Fold accepted proposals into the roadmap as new items with their evidence attached.

## Stopping at the usage limit

1. Finish the current slice to a committed green state, or revert it.
2. Update the roadmap (in-progress item back to ready or marked blocked, with notes).
3. Push the feature branch.
4. Report: commits made (hash and subject), evaluations run with before/after numbers, items blocked and why, explorer proposals pending, anything awaiting approval.

## Recovery after a crash or context loss

State lives in three places, not in your memory: `docs/development/roadmap.md` (what is in progress and why), `git log` and `git status` on the feature branch (what is committed and what is dirty), and `test-results/` (what was measured). Re-read all three first. If the tree is dirty, decide from the roadmap and the diff whether the dirty work is the in-progress slice; finish it through the normal gate/review/commit path or revert it. Never commit dirty work you cannot explain.

## Anti-patterns

- Tuning priors, thresholds, or queries to `evaluations/core.toml` or any fixture path.
- Broad refactors, renames, or formatting sweeps bundled into a feature slice.
- Editing files outside the slice's allowed list "while there".
- Compiling, testing, or indexing during a timed live evaluation.
- Running a second LCI process on the server's data directory while either indexes.
- Running a persistent server or a timed comparison from `target/debug` instead of a copied binary.
- Two implementers on the shared registry, scanner, manifest, or `app` modules at once.
- Starting, stopping, or probing model services without permission; any blanket process kill.
- Treating a one-query win as evidence; treating a one-query change on an eleven-query set as signal.
- Designing on top of an unmeasured premise because the measurement is harder than another round of ideas.
- Reporting "tests pass" without counts, or "evaluated" without the report name and the baseline it was compared with.
