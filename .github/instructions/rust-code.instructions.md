---
name: LCI Rust implementation
description: Architectural and implementation rules for Rust source in local-code-intelligence.
applyTo: "{src/**/*.rs,lci-core/src/**/*.rs}"
---

- Keep client-neutral behavior in the Rust service. Do not import editor, Copilot, Kilo, or generation-model concerns. Keep tracing on stderr and structured CLI JSON on stdout.
- Use the existing module boundaries: configuration/workspace identity, language/chunk/manifest, models, store, LSP, filtering/evaluation, application coordination, and MCP/HTTP transport.
- The workspace has four crates, split along dependency lines: `lci-core` (chunk, language, filter, lexical, manifest, workspace, config, models, lsp; no LanceDB/arrow, compiles and tests in seconds), `lci-maps`, `xtask`, and the root crate (app, evaluate, init, rest, server, store, main). Put new pure-logic code (a language adapter, a filter rule, an LSP adapter) in `lci-core` unless it needs `Store`/`App`. Never add a `lci-core -> local-code-intelligence` dependency edge.
- Preserve external storage boundaries and per-workspace coordination. Never hold a workspace read guard while entering an indexing path that needs its write guard.
- Validate inputs at the shared application boundary so MCP and CLI behavior agree. Preserve structured JSON fields and fail-open behavior for optional retrieval channels; return actionable errors for required indexing failures.
- Prefer direct, typed code over new frameworks or abstraction layers. Add a dependency only when the standard library and existing dependencies do not provide a clear implementation.
