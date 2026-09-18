---
name: LCI Rust implementation
description: Architectural and implementation rules for Rust source in local-code-intelligence.
applyTo: "{src/**/*.rs,lci-core/src/**/*.rs}"
---

- Keep client-neutral behavior in the Rust service. Do not import editor, Copilot, Kilo, or generation-model concerns.
- Use the existing module boundaries: configuration/workspace identity, language/chunk/manifest, models, store, LSP, filtering/evaluation, application coordination, and MCP/HTTP transport.
- The workspace is split into two crates along a dependency line, not a feature line: `lci-core` (chunk, language, filter, lexical, manifest, workspace, config, models, lsp) has zero dependency on LanceDB/arrow and compiles/tests independently in seconds; the root crate (app, evaluate, server, store, main) owns storage and orchestration. Put new pure-logic code (a language adapter, a filter rule, an LSP adapter) in `lci-core` unless it genuinely needs `Store`/`App`. Never add a `lci-core -> local-code-intelligence` dependency edge.
- Preserve external storage boundaries and per-workspace coordination. Never hold a workspace read guard while entering an indexing path that needs its write guard.
- Validate inputs at the shared application boundary so MCP and CLI behavior agree.
- Preserve structured JSON fields and fail-open behavior for optional retrieval channels. Return actionable errors for required indexing failures.
- Prefer direct, typed code over new frameworks or abstraction layers. Add a dependency only when the standard library and existing dependencies do not provide a clear implementation.
- Keep tracing on stderr and structured CLI JSON on stdout.
