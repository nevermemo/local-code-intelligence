# Validation matrix

Choose rows affected by the change; a release candidate should cover the full matrix.

| Surface | Deterministic evidence | Live evidence |
| --- | --- | --- |
| Build quality | format check, workspace tests, Clippy with warnings denied | release binary starts on Windows |
| MCP/HTTP | initialization, tool discovery, structured errors, `/health`, `/ready` tests | real client connects to `127.0.0.1:8768/mcp` |
| Persistence | temporary LanceDB, manifest compatibility, restart reuse, failed-update preservation | external data directory survives process restart |
| Retrieval | mock semantic/lexical/LSP/fusion/reranker and filter coverage | 8766/8767 evaluation with auditable source results |
| Languages | parser/chunk/update/delete/restart tests per adapter | production-versus-decoy query per added language |
| LSP | protocol/lifecycle/location tests | real server symbol, definition, and references |
| Windows tools | resolver unit/integration tests | resolved `rg.exe`, optional language-server executable |
| Packaging | staged-file and artifact scan | clean extraction and smoke test when distributing |

Always report:

- Commit or diff tested and exact configuration overrides.
- Counts and outcomes, not merely “tests pass.”
- Which services were real, mocked, unavailable, or deliberately not contacted.
- Index lifecycle actions and embedding reuse where indexing is exercised.
- Retrieval metrics and fallbacks where ranking is exercised.
- Remaining limitations and uncommitted files.

Generated `test-results`, disposable fixtures, `.tools` downloads, `target`, model weights, local configuration, and editor settings do not belong in a release commit. The application may use ports 8766, 8767, and 8768 according to their documented roles; port 8765 remains external and untouched.
