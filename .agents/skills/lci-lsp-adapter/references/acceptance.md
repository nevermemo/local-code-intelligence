# LSP acceptance

Separate mock protocol tests from real-server evidence.

Automated coverage (fake LSP in `tests/support`, `tests/cases/navigation.rs`, `lci-core` `lsp::` unit tests) should include:

- Initialization, request/response correlation, notifications, stderr, timeouts, process exit, retry, and shutdown.
- Workspace gating and absence of server startup for unrelated-language repositories or filtered searches.
- Symbol, definition, and reference location normalization.
- Rejection of unsupported source files before process startup.
- Candidate mapping to same-language indexed chunks only.
- Search fallback when the executable or request is unavailable.
- Direct-navigation error behavior.

Real acceptance must use the actual language-server executable against a known workspace and verify at least one workspace symbol, definition, and reference. The three xtask subcommands per language are the standard form: `cargo xtask acceptance <lang>-lsp` (full flow), `<lang>-missing` (provider isolation when the executable is absent), `<lang>-recovery` (persistent process reuse and forced-kill recovery). They start fixture servers on port 8769 and write normalized responses under `test-results/`. Record the executable and version, and distinguish unavailable tooling (`PREREQUISITE_UNAVAILABLE`) from a code failure. Do not claim support from a mocked server alone.

Known, documented limitations are not failures: `workspace/symbol` for TypeScript, Python, and C/C++ only sees files the server has been told about, and a fresh `clangd` process does not find cross-file references. The acceptance specs encode these as informational (TypeScript, C/C++) or skipped (Python) stages; do not "fix" them by weakening an assertion for another language.
