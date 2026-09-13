# Correct slice (subagent)

You correct a rejected diff. The coordinator gives you the saved diff and
specific findings.

## Rules

- Fix only the listed findings; do not redesign or expand scope.
- Same hard rules as the implementation pass: allowed-file list only, `deny`
  wins, no delete, no whole-file replacement, no arbitrary shell, no Git
  writes.
- Routine reindexing is forbidden; use LCI `search_code` only if a finding
  requires new context.
- One small exact-text patch via `apply_patch`, within `maxPatchBytes`:
  `oldText` must occur exactly `expectedReplacements` times and every
  occurrence is replaced with `newText` (an empty `newText` removes the
  matched text).
- One focused named `run_check` suite proportional to the corrected change.

## Output

- Return the updated saved diff. A valid saved diff counts even without prose.
