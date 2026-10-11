# Architecture guide

`local-code-intelligence` is a standalone MCP service. It owns its indexes and
keeps persistent data outside indexed repositories. Clients may be Kilo,
Copilot, Codex, or any other MCP-capable agent; client-specific behavior does
not belong in the service.

## Crates

The Cargo workspace has three members: the root package
`local-code-intelligence`, `lci-core`, and `xtask` (build, test and acceptance
commands). `lci-core` holds the pure logic -- `chunk`, `config`, `filter`,
`language`, `lexical`, `lsp`, `manifest`, `models` and `workspace`, with the
Tree-sitter grammars -- and depends on none of LanceDB, Arrow, axum or rmcp. The
root crate holds what needs the embedded database or a protocol server: `app`,
`store` (the only code that imports LanceDB and Arrow), `evaluate`, `server`,
`rest` and `main`, and re-exports the `lci-core` modules from `src/lib.rs` so
existing `crate::chunk`-style paths keep resolving. The split exists for build
time (commit `0729d9a`): the LanceDB/Arrow tree takes minutes to compile, so a
change that stays inside `lci-core` can be checked in seconds with
`cargo check -p lci-core` and `cargo test -p lci-core --lib <module>::` (see the
[testing guide](development/testing.md)).

## Runtime boundaries

- `app` coordinates indexing, retrieval, navigation, watching, and readiness.
- `chunk` scans supported source and produces Tree-sitter chunks.
- `language` is the registry for language adapters and cache compatibility.
- `store` owns embedded LanceDB persistence.
- `models` calls the embedding service on port 8766 and the fail-open reranker
  on port 8767.
- `lexical` and `lsp` are independent, fail-open retrieval channels.
- `filter` validates retrieval controls and classifies source roles.
- `evaluate` measures retrieval behavior without influencing ranking.
- `server` and `main` expose the MCP, REST/OpenAPI, and command-line interfaces; `rest` mirrors the MCP tools as plain JSON-over-HTTP on the same process and port.

Port 8765 hosts the separate generation model. Production code, tests, and
acceptance scripts in this repository must not contact or manage it.

## Data flow

Search validates filters, prepares a compatible index when one is missing,
runs semantic, lexical, and applicable LSP retrieval, fuses candidates, applies
the small source-role prior, and reranks a bounded shortlist. Each optional
channel fails independently and reports its fallback state.

Repository instructions and skill files are agent control-plane material. The
checked-in `.lciignore` keeps them out of this repository's searchable code
corpus while leaving them available to agents.
