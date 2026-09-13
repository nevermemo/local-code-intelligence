# local-code-intelligence

Windows-native Rust, TypeScript, JavaScript, and Python code retrieval for any Streamable HTTP MCP client. It combines Tree-sitter syntax chunks, an owned embedded LanceDB index, local Qwen embeddings, ripgrep lexical search, optional persistent rust-analyzer retrieval, Reciprocal Rank Fusion, and fail-open neural reranking.

Supported source extensions and result language identifiers are exact and case-sensitive:

| Extension | Language identifier | Parser |
| --- | --- | --- |
| `.rs` | `rust` | Tree-sitter Rust |
| `.ts` | `typescript` | Tree-sitter TypeScript |
| `.tsx` | `tsx` | Tree-sitter TSX |
| `.js` | `javascript` | Tree-sitter JavaScript |
| `.jsx` | `jsx` | Tree-sitter JavaScript/JSX |
| `.py` | `python` | Tree-sitter Python |

TypeScript, JavaScript, and Python have Tree-sitter syntax indexing and retrieval, but no language-server integration. Rust remains the only language supported by symbol, definition, and reference navigation.

## Build and run

Prerequisites: Rust stable 1.91 or newer, ripgrep (`rg`) on `PATH`, the MSVC C++ build tools/Windows SDK, and CMake. Rust navigation additionally needs the `rust-analyzer` and `rust-src` Rustup components. LanceDB also needs `protoc` at build time; the setup script downloads the official Windows binary into `.tools` inside this project. No Python environment or database server is required.

```powershell
cd C:\Users\micro\Desktop\local-code-intelligence
.\scripts\Setup-BuildTools.ps1
rustup component add rust-analyzer rust-src
.\scripts\Build.ps1 -Test
.\target\debug\local-code-intelligence.exe serve
```

The first build compiles LanceDB and its dependencies and can take several minutes. Subsequent builds reuse Cargo's cache.

Build note: LanceDB 0.38.0 requires its `remote` Cargo feature to compile because its job module references an HTTP error variant without a feature guard. This project enables that compile-time feature but uses only local embedded database paths.

The executable serves:

- MCP: `http://127.0.0.1:8768/mcp`
- Health: `http://127.0.0.1:8768/health`

Smoke test from another PowerShell window:

```powershell
Invoke-RestMethod http://127.0.0.1:8768/health
```

The server binds only to loopback. Its MCP transport uses the official `rmcp` SDK with host/origin validation and supports session-based older clients as well as the SDK's current protocol. `/health` reports process liveness; it does not claim that either model service is ready. Ctrl+C stops the server.

## Configuration

Defaults work with the services specified for this project. Copy `config.example.toml` to `config.toml` to override them, then pass `--config .\config.toml` before or after a subcommand. Configuration files are read only when explicitly supplied. Unknown fields are errors.

| Setting | Default |
| --- | --- |
| `embedding_url` | `http://localhost:8766/v1` |
| `embedding_model` | `qwen3-embedding-4b` |
| `reranker_url` | `http://localhost:8767/rerank` |
| `reranker_model` | `qwen3-reranker-4b` |
| `data_dir` | `%LOCALAPPDATA%\local-code-intelligence` on Windows |
| `default_top_k` | 8 (allowed: 1–40) |
| `embedding_batch_size` | 8 |
| `embedding_timeout_seconds` | 120 per batch |
| `reranker_timeout_seconds` | 120 |
| `ripgrep_path` | `rg` (on Windows, also discovers common VS Code-bundled copies) |
| `semantic_candidate_count` | 40 |
| `lexical_candidate_count` | 40 |
| `rerank_candidate_count` | 24 |
| `rrf_k` | 60 |
| `watch_poll_milliseconds` | 2000 |
| `watch_debounce_milliseconds` | 750 |
| `rust_analyzer_path` | `rust-analyzer` |
| `lsp_timeout_seconds` | 60 |
| `lsp_candidate_count` | 40 |

Persistent data lives in `data_dir\lancedb` and `data_dir\manifests`, outside indexed repositories. Canonicalization resolves path aliases and symlinks. A SHA-256 of the canonical workspace path identifies a workspace; each workspace has a separate LanceDB table and parse manifest. Manifest schema v2 records language and adapter compatibility per file, so changing one language parser does not invalidate unaffected languages. Compatible schema-v1 Rust manifests are migrated without reparsing unchanged files. Data directories and indexed workspaces cannot contain one another. Moving a repository creates a new workspace identity. This milestone has no automatic index cleanup command.

The generation service at port 8765 is independent and is never called, proxied, or managed by this application.

## MCP tools

All tools return structured JSON, also available as MCP text content. Tool failures set MCP `isError`.

| Tool | Arguments | Result |
| --- | --- | --- |
| `index_workspace` | `workspace_path` | Canonical workspace/ID, file/chunk counts, parsed/unchanged/removed files, newly embedded/reused chunks, total time |
| `index_status` | `workspace_path` | Whether indexed/indexing/stale/watched, chunk count, embedding dimension, configuration compatibility, last successful indexing time |
| `watch_workspace` | `workspace_path` | Start debounced polling and automatic reindexing for an indexed workspace |
| `unwatch_workspace` | `workspace_path` | Stop automatic reindexing for a workspace |
| `search_symbols` | `workspace_path`, `query` | Workspace symbols from rust-analyzer |
| `find_definition` | workspace path, relative file path, one-based line, zero-based UTF-16 character | Definition locations from rust-analyzer |
| `find_references` | the definition arguments plus optional `include_declaration` | Reference locations from rust-analyzer |
| `search_code` | `workspace_path`, `query`, optional `top_k` | Ranked source chunks with absolute/relative paths, inclusive one-based lines, semantic/reranker scores, timings and fallback warning |

Example arguments:

```json
{"workspace_path":"C:\\Users\\micro\\Desktop\\gpu-dialect-v0"}
```

```json
{
  "workspace_path": "C:\\Users\\micro\\Desktop\\gpu-dialect-v0",
  "query": "lower syn AST expressions into generated Slang compute shader code",
  "top_k": 8
}
```

Indexing is synchronous: its tool call finishes when the new index is persisted. Allow a long client tool timeout for the first indexing pass. Index writes are serialized in the running service, with per-workspace read/write coordination. This version is intended for one running service per data directory; multiple clients should connect to that service.

Watching is opt-in for each server run. It polls only after `watch_workspace`, debounces a detected source change, and calls the same safe indexing path. Watch registrations are not restored after a process restart; persistent indexes and manifests are. A failed automatic update is logged and retried after another poll while the prior LanceDB snapshot remains searchable.

The first Rust LSP request for a workspace starts one long-lived rust-analyzer process and waits briefly for crate discovery. Later MCP calls and mixed-language searches reuse it. A failed request discards the process and retries once with a fresh analyzer. `search_code` uses Rust LSP as an independent fail-open candidate channel only when the index contains Rust chunks; a TypeScript/JavaScript-only index does not start rust-analyzer or emit an LSP warning. LSP locations map only to Rust chunks. Direct navigation rejects recognized non-Rust paths before starting rust-analyzer and returns an MCP error when Rust tooling is unavailable. Definition/reference input lines are one-based; character offsets follow LSP and are zero-based UTF-16 units.

## Direct CLI and acceptance test

The CLI calls the same application logic and is useful for testing without configuring an editor. Use it while the server is stopped to avoid concurrent processes writing the same data directory.

```powershell
$lci = '.\target\debug\local-code-intelligence.exe'
$gust = 'C:\Users\micro\Desktop\gpu-dialect-v0'
& $lci index $gust
& $lci status $gust
& $lci search $gust 'lower syn AST expressions into generated Slang compute shader code'
& $lci symbols $gust 'emit_expression#'
& $lci definition $gust 'crates/gust-macros/src/slang/mod.rs' 266 24
& $lci references $gust 'crates/gust-macros/src/slang/mod.rs' 581 8 --include-declaration
& $lci index $gust
```

The second indexing pass should report zero new embeddings if source and model configuration are unchanged. GUST's actual translator is under `crates/gust-macros/src/slang`; inspect the returned source, not just path names. `scripts/Acceptance.ps1` saves the index, query, and repeat-index reports and checks that a majority of top-eight results are actual Slang translator implementation chunks, including the expression translator.

`scripts/Acceptance-Lsp.ps1` independently checks workspace-symbol, definition, and reference navigation against the same translator and saves each normalized response in `test-results`.

With the real embedding and reranking services running on ports 8766 and 8767, the mixed-language acceptance creates a disposable fixture and data directory under `test-results`, uses the actual debug binary, and never contacts port 8765:

```powershell
.\scripts\Acceptance-Multilingual.ps1
```

It verifies all supported language IDs, semantic and lexical retrieval with live reranking, production TypeScript ranking over a test decoy, zero-work unchanged indexing, one-file TypeScript invalidation, JavaScript deletion, and prior-snapshot retrieval after a deliberately unreachable embedding endpoint causes an update to fail. Reports are written as `test-results\multilingual-*.json`.

## Retrieval and persistence behavior

1. Scan exact lowercase `.rs`, `.ts`, `.tsx`, `.js`, `.jsx`, and `.py` extensions recursively with `.gitignore`, nested ignores, and standard `ignore` crate rules. Symlinks are not followed and `.git` is skipped. Hidden source files are eligible when not ignored. Read/traversal errors abort the update and preserve the previous index.
2. Select a static language adapter and Tree-sitter grammar by extension. Rust retains its original declaration boundaries. TypeScript/JavaScript chunks preserve imports, executable top-level statements, exports, comments/decorators, functions, classes and methods, interfaces and signatures, type aliases, enums, and variable-assigned arrow functions. Python chunks preserve imports, assignments, executable top-level statements, synchronous functions, asynchronous functions, classes, methods, decorators, and associated leading comments. Oversized syntax splits only at named syntax boundaries toward 6,000-byte groups, with declarations preserved up to 24,000 bytes. Large indivisible leaves stay whole rather than being truncated; parser error recovery remains searchable.
3. Hash each complete source file and persist its parsed chunks, language identifier, and adapter version in the external manifest. A later manual or watched pass reparses only changed/new files or files whose selected adapter version changed, including after restart. Fingerprints include path, content, language, and adapter version. A missing, incompatible, or malformed manifest safely causes a full parse.
4. Hash each exact source chunk. Reuse vectors from the current workspace snapshot when content hashes and the stored embedding URL/model/document-format identity match. Parser adapter versions are intentionally decoupled from embedding compatibility, so unchanged chunk text keeps its vector. Duplicate new chunks are embedded once. Documents are embedded as source, without a query instruction.
5. Build a complete workspace snapshot in memory and commit a LanceDB table overwrite after embedding succeeds. Save the matching parse manifest only after that commit. Removed chunks disappear from the current snapshot; line-only changes update locations without re-embedding unchanged chunks. Previous Lance versions may remain on disk. This is a straightforward small/medium-repository design, not a streaming indexer for huge monorepos.
6. Embed queries with exactly:

```text
Instruct: Given a code search query, retrieve relevant code passages that answer the query
Query: <query>
```

7. Retrieve semantic candidates with LanceDB cosine distance while ripgrep independently searches useful query terms across all supported extensions. In indexes containing Rust, rust-analyzer also searches workspace symbols. Map lexical lines to any indexed language and LSP locations only to Rust chunks.
8. Deduplicate and fuse semantic, lexical, and LSP ranks with Reciprocal Rank Fusion. Results expose channel ranks, lexical match count, fusion score, and retrieval channels.
9. Rerank the configured fused shortlist. Semantic, lexical, and Rust LSP channels fail independently; available channels continue. If reranking fails, return fusion order with a warning.

When `ripgrep_path` is the default `rg` or `rg.exe`, Windows resolution checks `PATH` first and then common per-user and system-wide VS Code, VS Code Insiders, and VSCodium installations, including versioned application directories. Any other configured value is treated as an explicit command or path and is used unchanged. If ripgrep cannot be started, lexical retrieval fails open and the warning explains how to set `ripgrep_path`.

Embedding response indices are validated and reordered; vectors must have consistent dimensions, finite values, and nonzero norm. Changing an embedding URL/model requires reindexing. Replacing a model behind an unchanged name/URL is not detectable automatically; use a distinct configured model name or a new data directory for that change.

JSON timing fields measure query embedding, LanceDB search, lexical search, Rust LSP search, fusion, reranking, and total retrieval in milliseconds. Total also includes validation and index access/wait time. The same timings appear in tracing logs on stderr; CLI JSON remains on stdout. Set `RUST_LOG=local_code_intelligence=debug` to adjust logging.

## Module boundaries and checks

- `config` / `workspace`: configuration, canonical identity, external storage boundary.
- `language` / `chunk` / `manifest`: static language adapters, ignore-aware incremental scan, Tree-sitter chunks, and persistent per-file parse reuse.
- `models`: embedding and reranking HTTP clients.
- `lsp`: persistent rust-analyzer JSON-RPC processes and normalized navigation locations.
- `store`: embedded LanceDB schema, snapshot writes, cosine candidates.
- `app`: indexing/cache coordination and retrieval pipeline.
- `server`: MCP tools and HTTP transport. `main`: CLI/server startup.

There is no BM25, editor-specific integration, Docker setup, generation proxy, or web frontend yet. Later candidate sources can join the same fusion boundary before reranking.

```powershell
.\scripts\Build.ps1 -Test
cargo fmt --all --check
$env:PROTOC = Join-Path (Get-Location) '.tools\protoc\bin\protoc.exe'
cargo clippy --locked --all-targets -j 8 -- -D warnings
```

Automated tests use a local mock model HTTP service and real embedded LanceDB. They exercise Rust boundary regression, all six language adapters, schema-v1 migration, per-language parse compatibility, persistent parse/vector reuse across reopen, mixed-language metadata and retrieval, TS/JS-only LSP gating, stale detection, deleted files, failed update preservation, watched refresh, embedding configuration changes, reranker failure/invalid replies, query instruction formatting, ignore handling, and actual Streamable HTTP initialization/tool discovery. Live GUST and multilingual acceptance use the real local Qwen services separately.
