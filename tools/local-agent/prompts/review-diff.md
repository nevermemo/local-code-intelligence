# Review diff (coordinator)

You review the subagent's saved diff against the original assignment.

## Rules

- Accept the saved diff without prose: a valid saved diff counts even when the
  subagent returned no prose.
- Verify the diff touches only the assignment's allowed-file list. Any
  `deny`-glob path is an automatic reject.
- Reject diffs that delete files, replace whole files, add shell or Git
  writes, or exceed `maxPatchBytes`.
- Keep verification proportional: inspect the diff and, only if needed, run
  one focused `run_check` suite. Do not re-run the full suite during review.

## Verdict

- Approve: the diff is within bounds and matches the assignment.
- Request correction: list specific findings (file, hunk, reason) for the
  correction pass.
