# Templates

Copy, fill, and keep each one short. Angle brackets mark fields.

## Slice plan (coordinator or planner writes it; implementer and reviewer receive it)

```text
Roadmap item: <id or title>
Goal: <one sentence; the observable behavior that changes>
Not in scope: <what this slice deliberately leaves alone>
Allowed files: <explicit list; tests included>
Contracts to read first: <AGENTS.md sections, skill references, README section>
Acceptance:
  - <behavior 1, how it is observed>
  - <behavior 2>
Tests to add or change: <file and case names>
Gate: cargo xtask test --suite <focused>; then --suite Full
Live evaluation: <none | evaluation-workflow.md steps 3-9, rerank on/off, workspaces>
Docs to update: <README section, config.example.toml, skill reference, none>
Revert condition: <what result means the slice is reverted, not fixed>
```

## Implementer brief

```text
You are implementing one bounded slice of local-code-intelligence (Rust).
Read AGENTS.md "Hard rules" first; then the slice plan below.
Edit only these files: <list>. Do not commit, push, run live evaluations,
or start/stop services. Do not touch ports 8765-8767.
lci MCP tools: <connected | not connected; use Grep/Read>.
Fast check you may run: <cargo check -p lci-core | cargo xtask test --suite X | none>.
Return: files changed, what each change does, tests added, anything you
could not finish. A saved diff is the deliverable; keep prose short.

<slice plan>
```

## Reviewer brief

```text
You are reviewing a diff you did not write, on local-code-intelligence.
Read AGENTS.md "Hard rules" and the slice plan below. Then read the diff
(git diff <base>..<head> or the file list) and the evidence.
Report findings as file:line, severity (blocker | should-fix | nit), and
a one-line reason. Check in this order:
  1. Correctness and error handling against the plan's acceptance criteria.
  2. Boundaries: ports, data outside workspaces, fail-open channels,
     freshness semantics, no client-specific code, no new service.
  3. Tests: honest (no weakened assertions, no #[ignore], no vacuous
     suite filters), cover the changed behavior and its failure path.
  4. Doc drift: README tables and prose, config.example.toml, skill
     references, MCP tool descriptions, evaluations/core.toml comments.
  5. Scope: files outside the allowed list, unrelated refactors.
Say explicitly if you found nothing in a category. Do not edit files.

<slice plan>
<evidence: test counts, suite names, live metrics, report file names>
```

## Explorer brief

```text
Background research on local-code-intelligence. No time pressure; finding
nothing is a valid result. Do not edit source files, do not start or stop
services, do not touch ports 8765-8767.
Question: <one question, e.g. "Would an in-process lexical engine beat
shelling out to ripgrep for our candidate channel, given how
lci-core/src/lexical.rs maps matches to chunks?">
Evidence to consult: <files, test-results reports, research notes>.
Return: a proposal with expected benefit, cost, risk, how it would be
evaluated (which metrics, which workflow), and the evidence - or
"nothing worth proposing" with one line on why.
```

## Five-line implementer and reviewer briefs

When the slice plan is already a file, point to it and restate nothing (the long briefs above are the fallback when it is not). `<plans_dir>` is the plans directory (resolve it with `cargo xtask env plans_dir`); name it by role in a brief, never by path.

```text
Implementer: slice <ID>. Read <plans_dir>/<plan file>, AGENTS.md "Hard rules", <skill reference>.
Edit only the plan's allowed files. No commit, push, live runs or services; do not touch ports 8765-8767.
lci MCP: <connected | not connected; use Grep/Read>. Fast check allowed: <command | none>.
Return: files changed, what each does, tests added, what is unfinished.
The diff is the deliverable; keep prose short.
```

```text
Reviewer: slice <ID>. Read <plans_dir>/<plan file>, then `git diff <base>..<head>` and <evidence paths>. Do not edit.
Check in order: correctness against the acceptance; AGENTS.md "Hard rules"; test honesty; doc drift (README, config.example.toml, skill references, roadmap decision line if a default moved); scope.
Report file:line, severity (blocker | should-fix | nit), one line each; say "none" for an empty category.
lci MCP: <connected | not connected; use Grep/Read>.
Do not run live evaluations or start services.
```

## Independent review brief (a pre-registration, design or plan, before anything runs)

```text
You are the independent reviewer of <document path>. You have no stake in it.
Read-only: edit nothing, start no service, contact no port 8765-8767. Web research is allowed; cite URLs.
Read <document>, <inputs it leans on: evidence, contracts, roadmap items, code>.
Report, with file:line for every claim and [unverified] for anything you could not open:
  0. Verdict, one paragraph: GO | GO WITH FIXES | STOP.
  1. What the result can and cannot decide; would it change a decision?
  2. Blockers B1..: fix before anything runs (what, evidence, smallest fix).
  3. Should-fix S1..: adopt before the full pass (each becomes one amendment-log line).
  4. Nits.
  5. Direction of bias as written; a cheaper design that answers the same question, if any.
```

The author answers every B and S item in the document's amendment log (adopted, adapted or rejected, with the reason). After the fixes the reviewer re-checks the blockers against the revised body and the log against the body, and gives a verdict again.

## Sanity checkpoint brief

Source of the checkpoints: roadmap "Strategy checkpoint" (keep the list below in sync with it). A fresh reviewer with no stake (stronger model, read-only, web research allowed for prior art) is briefed with:

```text
Sanity checkpoint <SC1..SC6 | recurring: renewal, every fifth slice, surprising result, owner request>, <date>.
Edit nothing, start no service. Read the roadmap "Strategy checkpoint", the items under test, <pre-registration, results, logs>, and `git log` since <last checkpoint>.
Answer with evidence (file:line or URL; [unverified] if you could not open it):
  1. Is the goal still sound? What is measured and what is only assumed?
  2. Does this already exist elsewhere (prior art, what vendors do)?
  3. What single measurement would change the decision?
  4. The checkpoint's own questions: <from the table>.
  5. Verdict: continue | narrow | pivot | stop, the next action, and who decides.
One page.
```

| Checkpoint | When | Its own questions |
| --- | --- | --- |
| SC1 | before the A/B binary freeze (mandatory; PREREGISTRATION step 1c) | question fit, neutrality of the plumbing slices, scope, consistency, residual risks |
| SC2 | after the A/B pilot, before the full pass | transcript audit (allowlists, path violations, void rate), noise (A1 repeat rule), harness validity |
| SC3 | at the A/B interim look (about 115 code tasks) | data quality and the futility decision |
| SC4 | after the A/B results, before any decision | an independent re-analysis from the raw logs; an interpretation check (what the result does and does not license) |
| SC5 | before pre-registering phase 2 (cross-map verbs and interfaces) and before any training or distillation on the RTX 5090 | phase 2's premise and its pre-registered measurement; the cost of being wrong |
| SC6 | after A7 (portable configuration) implementation and docs are in place | measured first: fresh agents (clean worktree with no local files; the owner's machine) find or create every path, port and URL through the documented resolvers without guessing; then neutrality evidence, docs consistency, nothing machine-specific committed |

The coordinator records the verdict and what changed in the roadmap "Strategy checkpoint".

## Pre-registration

Write it before anything runs and before looking at any result; freeze it; change it only through the amendment log. Select on one half of the queries and confirm on the other ([evaluation workflow](../../lci-retrieval-evaluation/references/evaluation-workflow.md)).

```text
# Pre-registration: <the question as one sentence> (roadmap <ID>)
Written <date> by <role>, before any run. The last section is the amendment log.
1. Question and hypotheses: <the one question; H0 and H1; what each outcome would change>.
2. Arms: <name | binary commit | config file and switches | what differs from the baseline | frozen artefacts>.
3. Metrics: <one primary; secondary; how each is computed and from which file>.
4. Decision rules, fixed now: adopt if <primary delta >= X, 95% interval excludes 0, no required-query or first-rank regression>;
   reject if <...>; inconclusive and what follows if <...>; stop rule <budget, usage, time>.
5. Data and procedure: <queries or tasks, halves or strata, run order, repeats and the variance rule>.
6. Safety and cost: <ports, data outside workspaces, services touched, time and token budget>.
7. Analysis plan: <script, seeds, intervals, multiple-comparison handling>.
8. Build steps in order: <numbered, with the check that proves each>.
9. Amendment log (append-only; a threshold changes only here):
   | # | Date | By | Change or review item | Adopted / adapted / rejected | Reason | Threshold changed? |
```

## Commit message

Observed conventions in `git log`: imperative subject, body with what and why, verified-before claims, bulleted changes, a Tests line with counts, an Evaluation or Live block with before/after numbers and report names, then the client-supplied co-author trailer.

```text
<Imperative subject, what changes, under ~70 characters>

<Why: the defect or gap, how it was observed or reproduced.>

- <change 1 and its reason>
- <change 2>

Tests: <N new unit tests in lci-core::module>, <M integration cases in
tests/cases/<file>.rs>; cargo xtask test --suite Full green.
Evaluation (<services used>, <workspaces>, before/after on frozen binaries;
reports in test-results, not committed):
- reranker unavailable: hit@1 <a> -> <b>, MRR <a> -> <b>, ...
- reranker on: ...
Limitations: <what was not verified, prerequisites unavailable>.
```

## Roadmap status entry

Use the roadmap's own status vocabulary and layout. Each item touched in a slice records, at minimum:

```text
status: <ready | in-progress (YYYY-MM-DD) | done (commit <hash>) | blocked (reason)>
evidence: <suite counts; report names; metric deltas>
follow-ups: <new items discovered, added to the backlog with a link back>
```

## End-of-run report

```text
Branch: <name>, pushed to origin at <hash>.
Commits this run:
- <hash> <subject>
Evaluations: <report pair names and the deltas that decided each slice>
Blocked or reverted: <item, reason>
Pending approval: <downloads, PR, main push, boundary questions>
Explorer proposals added to the roadmap: <titles>
Stopped because: <usage at N% | roadmap empty | approval needed>
```

## Run checkpoint file

Current state only, at most about 7 KB (the post-compaction hook prints it whole), re-read whole after every compaction. Delete a finished item at the next refresh (its record is the roadmap, a commit or the agent's memory); refresh after every push. The post-compaction check asserts these strings, so keep them verbatim: `Active run: yes|no`, `Last pushed: <7 hex characters>`, `Standing mandate`, `Approvals`, `90%`.

```text
# Run checkpoint -- <date time>. Last pushed: <hash> (<branch>, <PR>). Tree: <clean | uncommitted: files>
Active run: <yes (since <date>; what) | no>
FIRST AFTER ANY COMPACTION: run the post-compaction check script; call get_usage; read this file; read the roadmap "In progress" section and `git status`.

## Standing mandate
<what the owner asked for; pause at 90% of the 5-hour window, stop at 90% of the weekly window; pointer to the memory file holding the preferences>

## Approvals
- <date>: "<verbatim scope>". Does not cover: <...>
Not approved: <one line>

## Now
- <roadmap ID> | <who, model> | <allowed files> | <next step when it finishes>
Background tasks: <id | purpose | what to do when it finishes>

## Services
| port | what runs | started by (script or task id) | restore |

## Key paths
<frozen binaries and trees (commit), scratch configs and data_dirs, plan files, evidence directories, the archive of the previous checkpoint>

## Open questions
1. <question for the owner; the answer goes to the roadmap or memory, then the line is deleted>
```
