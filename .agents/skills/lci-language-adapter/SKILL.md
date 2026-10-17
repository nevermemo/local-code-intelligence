---
name: lci-language-adapter
description: Add or change programming-language support in local-code-intelligence, including extensions, Tree-sitter parsing, chunking, cache compatibility, retrieval metadata, and acceptance. Use for language grammar or syntax-indexing work; use lci-lsp-adapter separately for language servers.
---

# LCI language adapter

Implement one language slice at a time without changing unrelated adapters.
Adapter logic lives in `lci-core` (`lci-core/src/language.rs` registry,
`lci-core/src/chunk.rs` chunking) and must not depend on LanceDB.

1. Read [the adapter contract](references/adapter-contract.md).
2. Read [the chunking contract](references/chunking-contract.md) before changing syntax boundaries.
3. Bump only the affected adapter compatibility version when parsed output can change.
4. Preserve exact source, stable hashes, one-based inclusive lines, and embedding reuse for unchanged chunk text.
5. Read [the acceptance contract](references/acceptance.md) before writing tests or claiming support.

Fast loop: `cargo check -p lci-core`, then `cargo test -p lci-core --lib chunk::` or `language::`; then `cargo xtask test --suite Chunk` and, once per completed slice, `cargo xtask test --suite Full`. Live evidence needs 8766/8767 up: `cargo xtask acceptance multilingual`, plus the language's `cargo xtask acceptance <lang>-lsp` / `-missing` / `-recovery` triple from `lci-lsp-adapter`.

Do not claim language-server support from Tree-sitter support. Do not add client-specific behavior or contact port 8765.
