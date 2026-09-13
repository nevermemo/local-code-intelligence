# Local Qwen harness (`tools/local-agent`)

Dependency-free Node 24 harness that drives the local Qwen model against this
repository through a small, policy-bounded tool set. `agent.mjs` implements the
full harness: policy loading and validation, the bounded tool set, the
OpenAI-compatible model loop, the LCI MCP client, the JSONL audit log, and the
dry-run path.

## Requirements

- Node 24, no npm dependencies (no `package.json`, no `node_modules`).
- Qwen generation service at `http://127.0.0.1:8765/v1` (configurable).
- LCI MCP service at `http://127.0.0.1:8768/mcp` (configurable).

## Policy

`policy.example.json` is the reference policy. All paths are relative to
`repositoryRoot` and use forward slashes.

| Field | Meaning |
| --- | --- |
| `repositoryRoot` | Repository root, relative to the policy file |
| `read` / `write` / `deny` | Globs for readable, writable, and forbidden paths |
| `model.name` / `model.endpoint` / `model.apiKeyEnv` | Qwen model (e.g. `qwen3.8-27b`), OpenAI-compatible endpoint, environment variable holding the key |
| `lci.endpoint` | LCI MCP endpoint |
| `limits.maxTurns` | Maximum model turns per task |
| `limits.maxReadLines` | Maximum lines per `read_file` call |
| `limits.maxPatchBytes` | Maximum bytes per `apply_patch` (measured over `oldText` + `newText`) |
| `limits.maxToolResultBytes` | Maximum bytes returned per tool result |
| `checks.script` / `checks.suites` | `scripts/Test.ps1` and the named suites accepted by `run_check` |
| `checks.pathRules` | Ordered `{glob, suites, reason}` rules: the deterministic change-aware planner matches each changed repository-relative path against these globs to derive the focused suites |
| `checks.finalSuite` | Suite (e.g. `full`) the planner always defers; it is not run during a slice and the coordinator runs it once after all slices are accepted |
| `audit.jsonlPath` | JSONL audit log (under `test-results`, never committed) |

Deny wins: a path matching `deny` is neither readable nor writable, even when
`read` or `write` also matches it.

## Security model

- No delete, no whole-file replacement, no arbitrary shell, no Git writes.
- `apply_patch` is exact-text only: `oldText` must occur exactly
  `expectedReplacements` times and every occurrence is replaced with `newText`
  (an empty `newText` removes the matched text). `git_diff` is read-only.
  Commits are made by the coordinator outside the harness.
- `run_check` executes only named `Test.ps1` suites declared in the policy.
- Every tool call and model turn is appended to the JSONL audit path.

## Tools

| Tool | Notes |
| --- | --- |
| `lci_index_status` / `lci_search_code` | Via the LCI MCP endpoint; retrieval reuses the index |
| `list_files` | Directory listing under a relative path |
| `read_file` | Bounded by `maxReadLines` |
| `apply_patch` | Exact-text replace (`oldText` -> `newText`), bounded by `maxPatchBytes` |
| `git_diff` | Read-only |
| `run_check` | Named `Test.ps1` suite only |
| `plan_verification` | Deterministic change-aware planner: input is the changed repository-relative paths; output is `{focused, deferred, unmatchedPaths}`; performs no filesystem, process, MCP, model, or Git action |

## CLI

`--policy <path>` is required for every non-help invocation, and exactly one
of the three mutually exclusive input modes is required: `--task <text>`,
`--task-file <path>`, or `--contract <json>`.

```
node agent.mjs --task "..." --policy policy.json
node agent.mjs --task-file task.md --policy policy.json --dry-run
node agent.mjs --contract contract.json --policy policy.json
```

`--dry-run` resolves the policy, validates the task or contract, and lists the
planned tool calls without contacting the model or writing files.

### Task contract (`--contract`)

`--contract <json>` is the third input mode: a single JSON document of type
`local-agent.task/v1` that fully specifies one bounded slice.

| Field | Meaning |
| --- | --- |
| `id` | Stable contract identifier (recorded in the audit log as `contractId`) |
| `objective` | One-sentence objective for the slice |
| `allowedFiles` | Exact relative paths the slice may write |
| `acceptance` | What a correct result must satisfy |
| `prohibited` | What the slice must not do |
| `checkSuites` | Named `Test.ps1` suites the slice may run via `run_check` |

The base policy remains the outer authority: the contract narrows the exact
writable files and the runnable named suites at the tool layer, and cannot
widen anything the policy denies. `plan_verification` derives focused suites
from the policy `pathRules` and then contract-filters them: a suite may be
focused only if the contract's `checkSuites` also authorizes it, so a contract
can narrow the planner output but never widen it.
Invalid contracts (unknown type, missing or malformed fields, files outside
the policy `write` globs, suites not declared in `checks.suites`) fail before
any model or MCP contact and before any mutation.

Audit records carry `contractId` only; contract contents are never written to
the audit log.

## Verification planning

`plan_verification` is the deterministic change-aware verification planner. It
maps the changed files of a slice to the smallest set of named suites that
could be affected, so a slice runs only the focused checks instead of repeated
broad tests.

- Input: the slice's changed repository-relative paths (forward slashes,
  relative to `repositoryRoot`).
- Matching: each path is tested against the ordered `checks.pathRules` globs;
  the union of the matched rules' `suites` (with the rule `reason` as
  justification) is the focused candidate set.
- Contract filtering: focused suites are intersected with the active task
  contract's `checkSuites`; the contract can narrow the result but never
  widen it.
- `deferred`: the `checks.finalSuite` suite is always deferred by the planner,
  never focused.
- `unmatchedPaths`: changed paths no `pathRules` glob matches; the planner
  reports them as-is instead of widening the scope.
- Output: `{focused, deferred, unmatchedPaths}`. The tool performs no
  filesystem, process, MCP, model, or Git action; it only plans, it never
  executes checks.
- No automatic escalation: the planner never adds suites on its own. Focused
  stays focused — there is no automatic escalation to `Full` (or any other
  suite) while a slice is in progress. `Full` runs exactly once, after all
  accepted slices, when the coordinator runs `checks.finalSuite`.

Compact valid example:

```json
{
  "type": "local-agent.task/v1",
  "id": "slice-001",
  "objective": "Add the contract loader to agent.mjs",
  "allowedFiles": ["tools/local-agent/agent.mjs"],
  "acceptance": "CLI accepts --contract and rejects invalid contracts",
  "prohibited": ["no new dependencies", "no schema changes"],
  "checkSuites": ["Unit"]
}
```

## Commands

All commands run from `tools/local-agent`.

```bash
# Show help (no policy or task required).
node agent.mjs --help

# Dry-run: resolve and validate the policy and task, list the planned tool
# surface, and exit without contacting the model or writing files.
node agent.mjs --task "Implement the policy loader" --policy policy.example.json --dry-run

# Run the offline unit tests (node:test; no network, no model, no LCI).
node --test test/agent.test.mjs

# Bounded task run: one task, one policy, real model + LCI endpoints.
node agent.mjs --task-file task.md --policy policy.json

# Contract run: one task contract, one policy, real model + LCI endpoints.
node agent.mjs --contract contract.json --policy policy.json
```

A bounded task run is a single task with an explicit allowed-file list, one
small exact-text `apply_patch`, and one focused `run_check` suite proportional
to the change. The model and LCI endpoints must be reachable for a non-dry-run
run; the dry-run and test commands above are fully offline.

## Workflow

1. Contract: the coordinator defines one task contract (`local-agent.task/v1`)
   per slice and runs it with `--contract`.
2. Implementation: Qwen edits within the contract's `allowedFiles`.
3. Planning: Qwen calls `plan_verification` once with the changed
   repository-relative paths and runs at most the returned
   contract-authorized focused suites — one `run_check` per focused suite.
   The `checks.finalSuite` and any unmatched or deferred paths never trigger
   extra suites; there is no automatic `Full` escalation during a slice.
4. Coordinator review: the coordinator reviews the saved diff once against the
   contract's `acceptance` and `prohibited`; a valid saved diff counts even
   without prose.
5. Correction: if the review rejects the diff, the coordinator issues a
   correction contract with the specific findings; the subagent fixes only
   those findings.
6. Broad verification: after all slices are accepted, the coordinator runs one
   full check — `checks.finalSuite` (`Test.ps1 -Suite Full`) — exactly once.
7. Commit: the coordinator commits outside the harness.

Routine indexing and broad baseline tests are not startup chores: the LCI
index is reused via `search_code` / `index_status`, and the full suite runs
once at the end, not before or between slices.

## Smoke (Windows PowerShell)

```powershell
Set-Location tools\local-agent
node .\agent.mjs --help
node .\agent.mjs --task "Implement the policy loader" --policy .\policy.example.json --dry-run
node --test .\test\agent.test.mjs
node .\agent.mjs --task-file .\task.md --policy .\policy.json
node .\agent.mjs --contract .\contract.json --policy .\policy.json
```

## Audit

The harness appends one JSON line per tool call and model turn to
`audit.jsonlPath` (default `test-results/local-agent/audit.jsonl`). Records
carry metadata only (tool name, ok, error class, byte counts, duration) and
never task text, model output, source, or patch content. Contract runs record
`contractId` only; contract contents are never written to the audit log.
Generated reports under `test-results` are never committed.
