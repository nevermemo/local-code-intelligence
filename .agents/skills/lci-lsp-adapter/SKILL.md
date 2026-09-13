---
name: lci-lsp-adapter
description: Add or change an optional language-server integration in local-code-intelligence, including process lifecycle, workspace discovery, navigation, candidate mapping, recovery, and real-server acceptance. Use for rust-analyzer, gopls, clangd, or another LSP; do not use for Tree-sitter-only language support.
---

# LCI LSP adapter

Keep LSP optional and fail-open for code search while returning clear errors from direct navigation tools.

1. Read [the lifecycle and routing contract](references/lifecycle.md).
2. Reuse a persistent process per compatible workspace and server configuration.
3. Gate startup by indexed language; never launch a server for an absent language.
4. Normalize locations into repository-relative, one-based inclusive evidence without mapping across languages.
5. Recover from a failed request without leaving a poisoned client.
6. Read [the acceptance contract](references/acceptance.md) before claiming support.

Do not infer LSP support from Tree-sitter parsing, and do not put editor-specific protocol assumptions in the service.
