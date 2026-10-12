---
name: lci-release-validation
description: Validate local-code-intelligence before a release, milestone commit, or distribution by checking build quality, MCP behavior, retrieval evidence, Windows operation, artifacts, and documented limitations. Use for final acceptance and release preparation, not routine edits.
---

# LCI release validation

Validate the changed surface and report evidence classes separately.

1. Read [the validation matrix](references/validation-matrix.md).
2. Inspect the intended diff and repository status before running broad checks.
3. Run the deterministic gate once on the completed change: `cargo xtask test --suite Full` (machine-path lint, fmt check, workspace tests, Clippy with `-D warnings`).
4. Run only the live acceptance relevant to the changed behavior plus required regression suites: individual `cargo xtask acceptance <name>` subcommands, or `cargo xtask live-acceptance [--only <steps>] [--skip-build]` for the whole per-step-timed matrix (needs 8766/8767 and the language servers on the current shell's `PATH`; logs under `test-results/live-acceptance-logs/`).
5. Confirm temporary artifacts, reports, models, data directories, `config.toml`, and editor settings are not staged.
6. Record unavailable external prerequisites (`PREREQUISITE_UNAVAILABLE`) as limitations rather than weakening acceptance.

Publishing, tagging, opening a pull request, and pushing to `main` require the human's explicit approval (AGENTS.md "Approval rules"); pushing a feature branch does not. Do not contact port 8765.
