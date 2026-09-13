# Language acceptance

A language-support claim requires evidence for the affected language:

1. Exact extension discovery and correct rejection of lookalikes or unsupported case variants.
2. `.gitignore`, nested ignore, hidden-file, symlink, and external-data boundaries through the shared scanner.
3. Sensible Tree-sitter chunks with exact source and line ranges.
4. Correct language and source-role metadata.
5. Initial indexing with real document embeddings in live acceptance.
6. A zero-work repeat with zero new embeddings.
7. A one-file update that reparses only the changed file and reuses unchanged vectors.
8. Deleted-file reconciliation.
9. Previous-snapshot preservation after a failed update.
10. Compatible reuse after application restart.
11. Semantic and lexical retrieval plus reranking and reranker fail-open behavior.
12. A production implementation ranked against a realistic test, example, or documentation decoy.
13. README, MCP, CLI, filter, and evaluation-definition updates.

Use mocks for deterministic automated coverage and the configured 8766/8767 services for one bounded live acceptance. Report those evidence classes separately. LSP evidence belongs to `$lci-lsp-adapter`.
