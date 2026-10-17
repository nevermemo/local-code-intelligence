# Claude Code entry point

Read `AGENTS.md` first. It holds the hard rules, approval rules, live-service rules, and working method, and it is client-neutral. This file adds only what is specific to Claude Code.

## Skills

Load the matching project skill (under `.agents/skills/`) before starting. The list of skills and when each applies is in `AGENTS.md` "Project skills"; the language-support bar is in `AGENTS.md` "Working method" ("Language support"). For `lci-autonomous-improvement`, the role mapping is below. Fast build/test loop: `docs/development/testing.md`.

## Dogfooding with the `lci` MCP server

- Follow `AGENTS.md` "Working method" for `lci` MCP dogfooding and connection recovery.
- Claude Code cannot re-dial a failed MCP connection; ask the human to reconnect it. Say once that you are using Grep/Read meanwhile.
- Subagents launched with the Agent tool inherit the same rule; tell them whether `lci` is connected so they do not rediscover it.

## Role mapping for `lci-autonomous-improvement`

| Role | Claude Code mapping |
| --- | --- |
| coordinator | The main session. Owns the roadmap, git, gates, usage checks, and every approval request. |
| planner, reviewer | Agent tool with `model: "fable"`. The reviewer must not be the subagent that wrote the diff. |
| implementer | Agent tool with `model: "sonnet"`. One bounded edit, an allowed-file list, no commits, no test runs unless the slice says so. |
| explorer | Occasional Agent tool with `model: "sonnet"` and `run_in_background: true`. Hunts for optimisations or researches an option with no time pressure; "found nothing" is a valid report. |

## Usage check

After each slice, load `mcp__ccd_session_mgmt__get_usage` (ToolSearch `select:mcp__ccd_session_mgmt__get_usage`) and call it. What to do with the numbers is the skill's rule: `lci-autonomous-improvement` "Stopping at the usage limit".

## Commits and approval

- Commit attribution: Claude Code supplies the `Co-Authored-By` trailer through the session's attribution reminder. Do not hand-write a different one, and do not strip it.
- Approval: `AGENTS.md` "Approval rules" (from the human in chat only).

# Compact instructions

Durable state already lives in `docs/development/roadmap.md`, `docs/decisions.md`, git history, `test-results/`, AGENTS.md, the skills, and the agent's memory files, so a summary only needs what those do not hold:

- Keep: the user's standing mandate and stop rule, approvals granted in this session (verbatim scope), unanswered questions to the user, the branch and last pushed commit, the in-progress roadmap item and its uncommitted files, running background tasks and agents (purpose, what to do when they finish), frozen-binary and scratch config/data paths in use, service state (which models run where, anything temporarily swapped that must be restored), and decisions made but not yet written to the roadmap.
- Drop: tool output, file contents, diffs, evaluation tables already recorded in the roadmap or commit messages, and finished discussions.
- After compacting, re-read the roadmap's "Now and next" section and `git status` before acting.

Mechanics for long runs (see the skill's "Compaction strategy"): the owner may set `autoCompactWindow` (e.g. 300000) plus hooks in the gitignored `.claude/settings.local.json` -- a `SessionStart` hook with matcher `compact` that prints the run checkpoint, the roadmap's "Now and next" section and git state, and a `PostToolUse` hook (`if: "Bash(git push*)"`) reminding the agent to refresh the checkpoint after every push. The agent cannot trigger compaction itself; it keeps the checkpoint current so any compaction is cheap. Keep the checkpoint outside the repository, at the checkpoint path (`cargo xtask env checkpoint`; its durable location is recorded in the agent's memory), never in the repo.
