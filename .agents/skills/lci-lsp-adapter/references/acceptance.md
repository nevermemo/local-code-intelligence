# LSP acceptance

Separate mock protocol tests from real-server evidence.

Automated coverage should include:

- Initialization, request/response correlation, notifications, stderr, timeouts, process exit, retry, and shutdown.
- Workspace gating and absence of server startup for unrelated-language repositories or filtered searches.
- Symbol, definition, and reference location normalization.
- Rejection of unsupported source files before process startup.
- Candidate mapping to same-language indexed chunks only.
- Search fallback when the executable or request is unavailable.
- Direct-navigation error behavior.

Real acceptance must use the actual language-server executable against a known workspace and verify at least one workspace symbol, definition, and reference. Record the executable/version and distinguish unavailable tooling from a code failure. Do not claim support from a mocked server alone.
