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
