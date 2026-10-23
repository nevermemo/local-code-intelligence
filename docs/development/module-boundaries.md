# Module boundaries

This file is the home of the per-module boundary list and the checks that enforce it.

## Module boundaries and checks

- `config` / `workspace`: configuration, canonical identity, external storage boundary.
- `language` / `chunk` / `manifest`: static language adapters, ignore-aware incremental scan, Tree-sitter chunks (Markdown: heading sections), persistent per-file parse reuse.
- `models`: embedding and reranking HTTP clients. `lexical`: the ripgrep lexical channel.
- `lsp`: the shared `LspAdapter` trait and JSON-RPC transport, one adapter module per optional language server, normalized navigation locations.
- `store`: embedded LanceDB schema, snapshot writes, cosine candidates. `filter`: filter validation and source-role classification.
- `evaluate`: portable evaluation definitions, execution and metrics; it observes retrieval without changing ranking.
- `app`: indexing/cache coordination and the retrieval pipeline (also `traverse`, `recipes`, `compact`).
- `server`: MCP tools and HTTP transport. `rest`: the JSON-over-HTTP/OpenAPI mirror. `main`: CLI/server startup. The `lci-maps` crate holds the multimap (traversal) map definitions; the full module and crate picture is [docs/architecture.md](../architecture.md). Later candidate sources can join the same fusion boundary before reranking.

Checks: `cargo xtask build --test`, then `cargo xtask test --suite Full` once per completed change. `Full` runs `cargo xtask lint-machine-paths`, `cargo xtask docs check`, `cargo fmt --all -- --check`, `cargo test --workspace --locked --features local-code-intelligence/map-equivalence` and `cargo clippy --workspace --all-targets --locked --features local-code-intelligence/map-equivalence -- -D warnings` in sequence, setting `PROTOC` from `.tools/protoc` when the auto-downloaded copy is present. Focused suites (`Unit`, `Chunk`, `Filter`, `Config`, `Evaluation`, `Indexing`, `Readiness`, `Mcp`, `Watching`, `Lsp`, `Freshness`) and the change-to-suite map are in [docs/development/testing.md](testing.md); every focused suite fails if its filter matches no tests.

Automated tests use a local mock model HTTP service and real embedded LanceDB. They cover Rust boundary regression, all eighteen language adapters (one per extension), schema-v1 migration, per-language parse compatibility, persistent parse/vector reuse across reopen, mixed-language metadata and retrieval, TS/JS-only LSP gating, stale detection, deleted files, failed-update preservation, watched refresh, embedding configuration changes, reranker failure and invalid replies, query instruction formatting, ignore handling, actual Streamable HTTP initialization and tool discovery, and the REST/OpenAPI mirror's success and error responses. Live self-workspace and multilingual acceptance use the real local model services separately.
