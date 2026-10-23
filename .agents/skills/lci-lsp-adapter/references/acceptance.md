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

## Acceptance subcommand options

Each language's three subcommands run against a real fixture and the real server. The server executable is found on `PATH` unless an option names it:

- `csharp`: a real `dotnet` fixture and `csharp-ls`; `csharp-lsp --probe-only` is a standalone JSON-RPC protocol probe with no LCI binary involved. Needs `dotnet` and `csharp-ls` on `PATH`, or `$LCI_ACCEPTANCE_CSHARP_LS` naming the executable (for a `dotnet tool` install that is not on `PATH`).
- `typescript`: a real npm-scaffolded fixture and `typescript-language-server` (`--typescript-language-server <path>`).
- `python`: a real Python package fixture and `pyright` (`--pyright <path>`).
- `go`: a real `go.mod`-rooted module and `gopls` (`--gopls <path>`).
- `java`: a real Eclipse `.project`/`.classpath` fixture and jdtls (`--java <jdtls install directory>`, a directory and not an executable, matching `[java].path`; `$LCI_ACCEPTANCE_JDTLS` sets the same directory for repeated local runs).
- `c` and `cpp`: real standalone C/C++ fixtures and one shared `clangd` (`--clangd <path>`, for each).

All of them skip gracefully when their server is not found. Prerequisites per tool: `docs/development/live-acceptance-ci.md` "Prerequisites".

The `version_arg` of an acceptance spec (`None` for pyright-langserver, `Some("version")` for gopls) must agree with the adapter's readiness probe: both say what the binary answers to.
