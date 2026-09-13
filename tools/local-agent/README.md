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

## CLI

`--policy <path>` is required for every non-help invocation, and exactly one
of `--task <text>` / `--task-file <path>` is required.

```
node agent.mjs --task "..." --policy policy.json
node agent.mjs --task-file task.md --policy policy.json --dry-run
```

`--dry-run` resolves the policy, validates the task, and lists the planned
tool calls without contacting the model or writing files.

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
```

A bounded task run is a single task with an explicit allowed-file list, one
small exact-text `apply_patch`, and one focused `run_check` suite proportional
to the change. The model and LCI endpoints must be reachable for a non-dry-run
run; the dry-run and test commands above are fully offline.

## Workflow

1. Bounded assignment: the coordinator gives the subagent one task with an
   explicit allowed-file list.
2. LCI retrieval: `search_code` / `index_status` only; routine reindexing is
   forbidden.
3. Small patch: one exact-text `apply_patch` within the byte limit.
4. Focused check: one named `run_check` suite proportional to the change.
5. Coordinator diff review: the saved diff is reviewed against the assignment;
   a valid saved diff counts even without prose.
6. Correction: findings are fixed in a bounded correction pass.
7. Final: one full check (`Test.ps1 -Suite Full`) and a coordinator commit.

## Smoke (Windows PowerShell)

```powershell
Set-Location tools\local-agent
node .\agent.mjs --help
node .\agent.mjs --task "Implement the policy loader" --policy .\policy.example.json --dry-run
node --test .\test\agent.test.mjs
node .\agent.mjs --task-file .\task.md --policy .\policy.json
```

## Audit

The harness appends one JSON line per tool call and model turn to
`audit.jsonlPath` (default `test-results/local-agent/audit.jsonl`). Records
carry metadata only (tool name, ok, error class, byte counts, duration) and
never task text, model output, source, or patch content. Generated reports
under `test-results` are never committed.
