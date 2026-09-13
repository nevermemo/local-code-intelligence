---
name: lci-language-adapter
description: Add or change programming-language support in local-code-intelligence, including extensions, Tree-sitter parsing, chunking, cache compatibility, retrieval metadata, and acceptance. Use for language grammar or syntax-indexing work; use lci-lsp-adapter separately for language servers.
---

# LCI language adapter

Implement one language slice at a time without changing unrelated adapters.

1. Read [the adapter contract](references/adapter-contract.md).
2. Read [the chunking contract](references/chunking-contract.md) before changing syntax boundaries.
3. Update only the affected adapter compatibility version when parsed output can change.
4. Preserve exact source, stable hashes, one-based inclusive lines, and embedding reuse for unchanged chunk text.
5. Read [the acceptance contract](references/acceptance.md) before writing tests or claiming support.

Do not claim language-server support from Tree-sitter support. Do not add client-specific behavior or contact port 8765.
