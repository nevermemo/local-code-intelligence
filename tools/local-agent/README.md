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
| `limits.modelRequestTimeoutMs` | Per-model-request timeout (default 600000, sized for the local xhigh-reasoning model) |
| `limits.mcpRequestTimeoutMs` | Per-MCP-request timeout (default 60000) |
| `limits.checkTimeoutMs` / `limits.gitTimeoutMs` | Per-child-process timeouts for `run_check` and `git_diff` (defaults 600000 / 60000) |
| `limits.taskDeadlineMs` | Overall task deadline, independent of the turn budget (default 1800000) |
| `limits.retryBudget` | Retries allowed per failure class; absent classes keep the defaults |
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
- The checkpoint file lives beside the audit log (under `test-results` by
  default) and is never committed.

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

Recovery flags:

| Flag | Meaning |
| --- | --- |
| `--task-id <id>` | Persistent task id (`[A-Za-z0-9._-]+`). Defaults to the contract id, else a stable hash of the task text |
| `--state <path>` | Checkpoint file. Defaults to `<audit dir>/state/<task-id>.json` |
| `--resume` | Resume from the existing checkpoint; the task id and task text must match |
| `--stop-after-first-patch` | Stop as soon as one patch is accepted |
| `--stop-after-first-failed-check` | Stop as soon as one check fails |

Exit codes let the coordinator branch without parsing text:

| Code | Statuses |
| --- | --- |
| 0 | `awaiting_review`, `stopped_after_patch`, `stopped_after_failed_check` |
| 1 | `failed` |
| 2 | `no_changes` |
| 3 | `cancelled`, `deadline_exceeded` |

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
  "schema": "local-agent.task/v1",
  "id": "slice-001",
  "objective": "Add the contract loader to agent.mjs",
  "allowedFiles": ["tools/local-agent/agent.mjs"],
  "acceptance": "CLI accepts --contract and rejects invalid contracts",
  "prohibited": ["no new dependencies", "no schema changes"],
  "checkSuites": ["Unit"]
}
```

## Reliability, cancellation, and recovery

A run is recoverable: every outbound request is bounded, the whole task is
bounded, cancellation is graceful, and progress is checkpointed. The harness
currently accepts implementation tasks that are expected to edit at least one
allowed file; use LCI or the coordinator directly for read-only investigations.

- **Timeouts.** Each model request, each MCP request, and each child process
  (`run_check`, `git_diff`) is bounded by its own policy timeout. No single
  request may outlive the overall task deadline.
- **Task deadline.** `limits.taskDeadlineMs` bounds the whole run independently
  of `maxTurns`. The deadline is checked before every model turn and every tool
  call, so no new work starts after it passes.
- **Cancellation.** `SIGINT` / `SIGTERM` abort in-flight model, MCP, and child
  process work; the loop stops at the next safe point, writes a checkpoint with
  status `cancelled`, and exits 3. A cancellation is never reported as a
  timeout.

### Checkpoint (`local-agent.state/v1`)

The checkpoint is deliberately small. The growing conversation is **not**
persisted: only the task identity, the tool surface, the accepted change
summary, and the accepted patch-chain identity are, so resumption stays predictable.

```json
{
  "schema": "local-agent.state/v1",
  "task_id": "python-decorators-01",
  "status": "awaiting_review",
  "turn": 6,
  "patches_applied": 2,
  "checks_run": [{ "suite": "Indexing", "result": "passed" }],
  "files_changed": ["src/chunk.rs", "tests/cases/indexing.rs"],
  "last_failure": null
}
```

The record also carries `task_hash`, `contract_id`, `contract_hash`, `tools`,
`file_hashes`, `diff_id`, and `retries_used`. `file_hashes` records the current
SHA-256 of every file changed through an accepted patch. On resume, every
recorded file must still exist and match its hash. `diff_id` is an audit identity
for the accepted patch chain; it is not a Git working-tree hash.

A checkpoint is written after every accepted patch, after every check, and on
every terminal status. `--resume` reloads it and refuses to continue when the
task id, task text, or contract differs from the one the checkpoint describes.
A resumed run starts a new model conversation seeded with a compact resume
summary; it does not restore the prior transcript or KV cache. It continues the
same turn, patch, check, and diff counters. Resume fails with `state_drift` when
a recorded file is missing or changed outside the harness.

MCP initialization is attempted once per process before the model loop. An
initialization failure is reported immediately and is not retried; the MCP retry
budget applies to tool requests made after a session has initialized.

### Failure classes and retry budgets

Failures are classified so a transport fault is never confused with a rejected
patch or a failing check. Each class has its own narrow budget
(`limits.retryBudget`), and only transport-shaped codes are retried — a missing
API key or a contract-denied path is a decision, not a transient fault.

| Class | Default budget | Typical codes |
| --- | --- | --- |
| `model_failure` | 1 | `model_timeout`, `model_error`, `model_bad_json`, `no_api_key` |
| `mcp_failure` | 1 | `mcp_timeout`, `mcp_error`, `mcp_tool_error`, `mcp_init_failed` |
| `patch_rejected` | 2 | `patch_mismatch`, `bad_patch`, `patch_too_large`, `contract_path_denied` |
| `check_failed` | 1 | `check_failed` |
| `coordinator_cancelled` | 0 | `cancelled` |
| `deadline_exceeded` | 0 | `deadline_exceeded` |
| `harness_failure` | 0 | `git_timeout`, `unknown_tool`, policy and path errors |

Recovery is narrow:

```
Transport timeout
  -> retry the same request once

No model response, no saved change
  -> retry with one short correction prompt, then report no_changes

Patch precondition mismatch
  -> return the current nearby source (numbered lines) to the model
  -> request a fresh exact patch

Focused check failure
  -> provide only the relevant failure output
  -> request a correction

Repeated identical failure
  -> stop and return control to the coordinator
```

A repeated identical failing tool call (same tool, same arguments, same error
code) stops the run immediately with `repeated: true` on the failure, instead of
burning the remaining budget. A final response that requests no file change is
not an accepted result: the harness sends one short correction prompt and then
reports `no_changes` (exit 2).

Retries, checkpoints, cancellation requests, repeated failures, and the terminal
status are recorded in the JSONL audit log as `kind: "lifecycle"` records —
identifiers, classes, and codes only, never task text, source, or model output.

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

# Bounded first slice: stop as soon as one patch is accepted.
node agent.mjs --contract contract.json --policy policy.json \
  --task-id python-decorators-01 --stop-after-first-patch

# Resume the same task from its checkpoint after an interruption.
node agent.mjs --contract contract.json --policy policy.json \
  --task-id python-decorators-01 --resume
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
