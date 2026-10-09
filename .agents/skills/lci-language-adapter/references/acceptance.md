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
14. LSP navigation available via the language-server adapter contract, passing `lci-lsp-adapter`'s own acceptance bar (real workspace symbol/definition/reference evidence, provider isolation, and recovery).

## Where each class of evidence comes from

| Items | Evidence | Command |
| --- | --- | --- |
| 1-4, 6-10 (mocked) | `lci-core` unit tests plus `tests/cases/indexing.rs` with the fake model service | `cargo test -p lci-core --lib chunk::`, `cargo xtask test --suite Indexing` |
| 5-11 (live) | mixed-language fixture indexing, update, delete, restart, failed-update retention, live reranking | `cargo xtask acceptance multilingual` (8766/8767 up; reports `test-results/multilingual-*.json`) |
| 12 | a scratch decoy evaluation TOML kept outside the repo, run with and without the reranker | `lci-retrieval-evaluation/references/evaluation-workflow.md` |
| 13 | README language table and chunk description, `config.example.toml` (if an LSP section exists), `evaluations/core.toml` chunking query, MCP tool descriptions | review the diff against `.github/instructions/documentation.instructions.md` |
| 14 | real server symbol/definition/reference, provider isolation, recovery | `cargo xtask acceptance <lang>-lsp`, `<lang>-missing`, `<lang>-recovery` |

Use mocks for deterministic automated coverage and the configured 8766/8767 services for one bounded live acceptance. Report those evidence classes separately.

LSP evidence belongs to `lci-lsp-adapter`; its acceptance bar is the source of truth for item 14's evidence, but the requirement itself is not optional or deferrable. A language-support claim is incomplete without it: do not stop at syntax-only support, do not merge or announce a language as "added" before its navigation adapter passes, and do not treat "no language-server adapter yet" as an acceptable final state. If a real blocker makes navigation genuinely infeasible for a language (no LSP implementation exists anywhere for it, for example), say so explicitly to the human and get their agreement before treating item 14 as satisfied by anything less than passing evidence. Do not decide that unilaterally.
