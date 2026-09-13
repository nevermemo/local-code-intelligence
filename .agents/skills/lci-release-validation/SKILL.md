---
name: lci-release-validation
description: Validate local-code-intelligence before a release, milestone commit, or distribution by checking build quality, MCP behavior, retrieval evidence, Windows operation, artifacts, and documented limitations. Use for final acceptance and release preparation, not routine edits.
---

# LCI release validation

Validate the changed surface and report evidence classes separately.

1. Read [the validation matrix](references/validation-matrix.md).
2. Inspect the intended diff and repository status before running broad checks.
3. Run deterministic formatting, tests, and Clippy once on the completed change.
4. Run only live acceptance relevant to changed behavior, plus required regression suites.
5. Confirm temporary artifacts, reports, models, data directories, and editor settings are not staged.
6. Record unavailable external prerequisites as limitations rather than weakening acceptance.

Do not publish, deploy, tag, or push unless the user explicitly requests it. Do not contact port 8765.
