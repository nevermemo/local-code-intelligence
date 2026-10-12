---
name: lci-lsp-adapter
description: Add or change an optional language-server integration in local-code-intelligence, including process lifecycle, workspace discovery, navigation, candidate mapping, recovery, and real-server acceptance. Use for rust-analyzer, gopls, clangd, or another LSP; do not use for Tree-sitter-only language support.
---

# LCI LSP adapter

Keep LSP optional and fail-open for code search while returning clear errors from direct navigation tools.

Where things live: one adapter module per server under `lci-core/src/lsp/` (`rust.rs`, `csharp.rs`, `typescript.rs`, `python.rs`, `go.rs`, `java.rs`, `clangd.rs`) behind the shared trait in `adapter.rs` and the JSON-RPC transport in `transport.rs`; registration in `Manager::new` (`lci-core/src/lsp.rs`); the per-server `[<section>]` config in `lci-core/src/config.rs` (rust-analyzer: top-level `rust_analyzer_*` keys); the real-server acceptance specs under `xtask/src/acceptance/`.

1. Read [the lifecycle and routing contract](references/lifecycle.md).
2. Reuse a persistent process per compatible workspace and server configuration.
3. Gate startup by indexed language; never launch a server for an absent language.
4. Normalize locations into repository-relative, one-based inclusive evidence without mapping across languages.
5. Recover from a failed request without leaving a poisoned client.
6. Read [the acceptance contract](references/acceptance.md) before claiming support.

Fast loop: `cargo test -p lci-core --lib lsp::` for adapter logic, `cargo xtask test --suite Lsp` for the fake-server navigation cases, `cargo xtask test --suite Full` once per completed slice. Real-server evidence: `cargo xtask acceptance <lang>-lsp` / `<lang>-missing` / `<lang>-recovery`; each prints `PREREQUISITE_UNAVAILABLE` and skips when its executable is absent, which is a limitation to report, not a pass.

Do not infer LSP support from Tree-sitter parsing, and do not put editor-specific protocol assumptions in the service.
