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
3. Check services without touching 8765: `curl http://127.0.0.1:8768/ready` (or the embedding service's `/v1/models`). Note which of 8766/8767 are up; do not start anything unless the human already permitted it. On the current development machine the two 4B pooling services together use roughly 17 GB of 48 GB RAM and the reranker shares the iGPU with everything else, so never add a third model process without asking, and keep heavy compiles away from timed live runs.
4. If the running `local-code-intelligence serve` was started from `target/debug`, note that rebuilding will fail on Windows until it is restarted from a copied binary (see `AGENTS.md` "Running LCI locally"). Ask before stopping it.
5. Confirm the stop condition with the human if the mandate did not state one.

## The loop

Repeat until the stop rule fires or the roadmap has no ready item.

1. **Pick.** Open `docs/development/roadmap.md`. Take the highest-priority item whose status marks it ready and unblocked. Mark it in progress with today's date. If the roadmap is missing or has no ready item, stop and ask rather than invent a backlog.
2. **Plan.** Produce a slice plan (template in [references/templates.md](references/templates.md)): goal, allowed files, acceptance criteria, tests to add, which gate suites apply, whether a live evaluation is needed (yes for anything touching ranking, candidates, reranker input, priors, roles, embedding identity, or latency), and the revert condition. A slice is one logical change; if the plan needs more than roughly ten files or two subsystems, split it into separate roadmap items.
3. **Implement.** Brief an implementer with the plan, the allowed-file list, the relevant contract files, and whether the `lci` MCP tools are connected. Inspect the saved diff for file scope and intent before anything else; a valid diff counts even without prose. Fix small gaps yourself or send one focused continuation; do not start a second implementer on the same files.
4. **Gate.** Run the focused suite first (`cargo xtask test --suite <name>` or `cargo test -p lci-core --lib <module>::`), then `cargo xtask test --suite Full` once the slice is complete. See "When a gate fails".
5. **Evaluate (when the plan says so).** Follow `lci-retrieval-evaluation/references/evaluation-workflow.md` exactly: frozen `lci-base`/`lci-new` binaries, scratch configs with short separate data directories, sequential runs, reranked and reranker-unreachable pairs for prior/role/fusion changes, reports under `test-results/`. Do not compile or run tests while a timed run is in progress. Decide with the workflow's decision rule; if the change does not hold up, revert it and record why in the roadmap.
6. **Review.** Brief a reviewer who did not write the code with the diff, the slice plan, and the evidence. It checks correctness, `AGENTS.md` boundaries (ports, data location, fail-open, freshness semantics), test honesty (no weakened or vacuous tests; suites that still match tests), documentation drift (README tables, `config.example.toml`, skill references, MCP tool descriptions), and scope creep. Address every finding or record why not; a disputed finding goes to the human, not silently dropped.
7. **Commit and push.** One logical change per commit, message per the template: imperative subject under about 70 characters, a body saying what changed and why, the evidence (test counts, suites, live metrics before/after, report names), and limitations. The client adds the co-author trailer. Stage only the slice's files; never `git add -A` with scratch material present (`reports/`, `research_notes/`, `test-results/` are not slice files). Push the feature branch.
8. **Record.** Update the roadmap item: status, commit hash, evidence summary, follow-ups discovered. Update any docs the reviewer flagged in the same commit as the code, or in an immediately following one if the review found them afterwards.
9. **Check usage and continue.** Read the usage figure. Below 90%: run an explorer in the background if none is running and pick the next item. At or above 90%: stop (see below).

## When a gate fails

- Read the failure before touching anything. Fix the code if the test is right; fix the test only if the test asserted something the slice legitimately changed, and say so in the commit body.
- If the fix is not obvious within one focused attempt, revert the slice (`git checkout -- <files>` or `git revert` if already committed), mark the roadmap item blocked with the failure, and move on. A revert is a normal outcome, not a failure of the run.
- A `--suite Full` failure caused by an unrelated pre-existing problem is still a stop: fix it as its own slice first, or ask.
- Never pass a gate by `#[ignore]`, loosening an assertion, raising a timeout to hide a hang, or filtering a suite down to zero tests.

## Stop and ask

Stop the loop, leave the tree clean, and ask the human when:

- the next step is on the `AGENTS.md` approval list (downloads, pull requests, pushing `main`, tags, publishing, boundary changes, deleting user data, starting or stopping services);
- a roadmap item needs a product decision the roadmap does not already settle (new dependency, new config surface with unclear default, user-visible behavior change);
- evidence conflicts (tests green but live evaluation regresses, or reviewer and implementer disagree on a contract);
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
- Reporting "tests pass" without counts, or "evaluated" without the report name and the baseline it was compared with.
