# local-code-intelligence

Cross-platform (Windows, macOS, Linux) Rust, TypeScript, JavaScript, Python, C#, Go, and Java code retrieval for any Streamable HTTP MCP client. It combines Tree-sitter syntax chunks, an owned embedded LanceDB index, local Qwen embeddings, ripgrep lexical search, optional persistent Rust and C# language-server retrieval, Reciprocal Rank Fusion, and fail-open neural reranking.

Supported source extensions and result language identifiers are exact and case-sensitive:

| Extension | Language identifier | Parser |
| --- | --- | --- |
| `.rs` | `rust` | Tree-sitter Rust |
| `.ts` | `typescript` | Tree-sitter TypeScript |
| `.tsx` | `tsx` | Tree-sitter TSX |
| `.js` | `javascript` | Tree-sitter JavaScript |
| `.jsx` | `jsx` | Tree-sitter JavaScript/JSX |
| `.py` | `python` | Tree-sitter Python |
| `.cs` | `csharp` | Tree-sitter C# |
| `.go` | `go` | Tree-sitter Go |
| `.java` | `java` | Tree-sitter Java |

All nine languages always have Tree-sitter syntax indexing and retrieval regardless of LSP configuration. Navigation (`search_symbols`/`find_definition`/`find_references`) is additionally available behind an optional, independently-configured language server per language family: Rust uses `rust-analyzer`; C# uses standalone `csharp-ls`; TypeScript/TSX/JavaScript/JSX share one `typescript-language-server` instance; Python uses `pyright` (`pyright-langserver`). Go and Java have syntax indexing and retrieval only; neither has a navigation server integration yet. None of these are bundled or installed by LCI, and a disabled or unavailable one never affects syntax retrieval for its language — only navigation on that language's files fails with a clear tooling error. `workspace/symbol` search (`search_symbols`) is honestly limited for TypeScript/Python: since LCI never writes a `tsconfig.json`/`pyrightconfig.json` into a user's repository, a freshly spawned server has no project context until `find_definition`/`find_references` open a file, so `search_symbols` results depend on what's already been opened in that server session.

## Documentation

- [Architecture guide](docs/architecture.md): runtime boundaries, data flow, and the agent control-plane exclusion.
- [Testing guide](docs/development/testing.md): focused suite selection and the full deterministic gate.
- [Live acceptance CI](docs/development/live-acceptance-ci.md): running `cargo xtask acceptance`/`evaluate` against real services on every push, via a self-hosted runner.

## Build and run

Build prerequisites are Rust stable 1.91 or newer, a C/C++ toolchain (MSVC + Windows SDK on Windows; a system compiler on macOS/Linux), CMake, and `protoc`. `cargo xtask setup` auto-downloads and verifies the official `protoc` release on Windows; on macOS/Linux it checks `PATH` for `protoc`/`cmake` and prints an install hint (`brew install protobuf cmake`, `apt install protobuf-compiler cmake`, etc.) if either is missing. Ripgrep is optional and fails open when unavailable. Rust navigation additionally needs the optional `rust-analyzer` and `rust-src` Rustup components. C# navigation optionally needs a standalone `csharp-ls` executable. TypeScript/JavaScript navigation optionally needs `typescript-language-server` (`npm install -g typescript-language-server typescript`; the workspace itself also needs its own `typescript` dependency, same as any real TS/JS project). Python navigation optionally needs `pyright` (`npm install -g pyright`, providing `pyright-langserver`). None of these are bundled or installed by LCI. No Python environment or database server is required to build or run LCI itself.

All dev/build/test/acceptance tasks run through the `xtask` crate (`cargo xtask <command>`) instead of shell scripts, so the workflow is identical on Windows, macOS, and Linux:

```bash
cd local-code-intelligence
cargo xtask setup
rustup component add rust-analyzer rust-src
cargo xtask build --test
./target/debug/local-code-intelligence serve   # local-code-intelligence.exe on Windows
```

The first build compiles LanceDB and its dependencies and can take several minutes. Subsequent builds reuse Cargo's cache. When iterating on chunking, language adapters, filters, or LSP adapters, `cargo check -p lci-core` / `cargo test -p lci-core --lib` skip that dependency tree entirely and finish in seconds -- see `docs/development/testing.md` for the full picture, including an optional personal (not project) sccache/linker setup that speeds up full-workspace builds too.

Build note: LanceDB 0.38.0 requires its `remote` Cargo feature to compile because its job module references an HTTP error variant without a feature guard. This project enables that compile-time feature but uses only local embedded database paths.

The executable serves:

- MCP: `http://127.0.0.1:8768/mcp`
- Health: `http://127.0.0.1:8768/health`
- Readiness: `http://127.0.0.1:8768/ready`

Smoke test from another terminal:

```bash
curl http://127.0.0.1:8768/health
```

The server binds only to loopback. Its MCP transport uses the official `rmcp` SDK with host/origin validation and supports session-based older clients as well as the SDK's current protocol. `/health` is a cheap process-liveness check and never contacts model services. `/ready` and the MCP `service_status` tool return the same bounded dependency report. They return HTTP 200 when a new semantic index can be created, even if optional channels are degraded, and `/ready` returns HTTP 503 when a required dependency is unavailable. Ctrl+C stops the server.

## Configuration

Defaults work with the services specified for this project. Copy `config.example.toml` to `config.toml` to override them, then pass `--config config.toml` before or after a subcommand. Configuration files are read only when explicitly supplied. Unknown fields are errors.

| Setting | Default |
| --- | --- |
| `embedding_url` | `http://localhost:8766/v1` |
| `embedding_model` | `qwen3-embedding-4b` |
| `reranker_url` | `http://localhost:8767/rerank` |
| `reranker_model` | `qwen3-reranker-4b` |
| `data_dir` | OS-native local data dir + `local-code-intelligence`: `%LOCALAPPDATA%\local-code-intelligence` (Windows), `~/Library/Application Support/local-code-intelligence` (macOS), `$XDG_DATA_HOME` or `~/.local/share/local-code-intelligence` (Linux) |
| `default_top_k` | 8 (allowed: 1–40) |
| `embedding_batch_size` | 8 |
| `embedding_timeout_seconds` | 120 per batch |
| `reranker_timeout_seconds` | 120 |
| `readiness_timeout_seconds` | 5 (allowed: 1–30) |
| `ripgrep_path` | `rg` (on Windows, also discovers common VS Code-bundled copies) |
| `semantic_candidate_count` | 40 |
| `lexical_candidate_count` | 40 |
| `rerank_candidate_count` | 24 |
| `rrf_k` | 60 |
| `watch_poll_milliseconds` | 2000 |
| `watch_debounce_milliseconds` | 750 |
| `rust_analyzer_path` | `rust-analyzer` |
| `[csharp].path` | unset (disabled) |
| `[csharp].args` | `[]` |
| `[csharp].disabled` | `false` |
| `[typescript].path` | unset (disabled) |
| `[typescript].args` | `[]` (appended after the `--stdio` flag LCI always supplies) |
| `[typescript].disabled` | `false` |
| `[python].path` | unset (disabled) |
| `[python].args` | `[]` (appended after the `--stdio` flag LCI always supplies) |
| `[python].disabled` | `false` |
| `lsp_timeout_seconds` | 60 |
| `lsp_candidate_count` | 40 |
| `index.freshness` | `on-search` (allowed: `manual`, `on-search`, `watch`) |
| `index.stale_check_interval_seconds` | 10 |
| `index.wait_for_existing_job_seconds` | 120 |

Persistent data lives in `data_dir\lancedb` and `data_dir\manifests`, outside indexed repositories. Canonicalization resolves path aliases and symlinks. A SHA-256 of the canonical workspace path identifies a workspace; each workspace has a separate LanceDB table and parse manifest. Manifest schema v2 records language and adapter compatibility per file, so changing one language parser does not invalidate unaffected languages. Compatible schema-v1 Rust manifests are migrated without reparsing unchanged files. Data directories and indexed workspaces cannot contain one another. Moving a repository creates a new workspace identity. This milestone has no automatic index cleanup command.

The generation service at port 8765 is independent and is never called, inspected, proxied, or managed by this application.

## MCP tools

All tools return structured JSON, also available as MCP text content. Tool failures set MCP `isError`.

| Tool | Arguments | Result |
| --- | --- | --- |
| `index_workspace` | `workspace_path` | Canonical workspace/ID, file/chunk counts, parsed/unchanged/removed files, newly embedded/reused chunks, total time |
| `index_status` | `workspace_path` | Whether indexed/indexing/stale/watched, chunk count, embedding dimension, configuration compatibility, last successful indexing time |
| `list_indexed_files` | `workspace_path` | Every file the persistent index has cached chunks for, with language and chunk count, from the last successful `index_workspace` |
| `watch_workspace` | `workspace_path` | Start debounced polling and automatic reindexing for an indexed workspace |
| `unwatch_workspace` | `workspace_path` | Stop automatic reindexing for a workspace |
| `search_symbols` | `workspace_path`, `query` | Workspace symbols from applicable enabled Rust/C# language servers, with provider/language metadata |
| `find_definition` | workspace path, relative file path, one-based line, zero-based UTF-16 character | Definition locations routed by `.rs`/`.cs` extension |
| `find_references` | the definition arguments plus optional `include_declaration` | Reference locations routed by `.rs`/`.cs` extension |
| `search_code` | `workspace_path`, `query`, optional `top_k` | Ranked source chunks, scores, timings, fallback warning, and index lifecycle metadata |
| `service_status` | none | The same required/optional readiness report as `/ready` |

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

`search_code` automatically creates a missing workspace index, waits for its commit, and continues the requested search in the same call. Concurrent first searches for one workspace share that indexing job; different callers receive results from the committed snapshot without duplicate document embeddings. A compatible persisted index is reused immediately after restart.

Once an index exists, the `[index]` `freshness` policy (default `on-search`) decides whether a search checks for staleness. Under `on-search`, the first search that needs it compares the workspace's file manifest and content hashes against disk, throttled by `stale_check_interval_seconds` so repeated searches in an unchanged workspace reuse the current snapshot without rescanning. When changed, new, or removed files are detected (or the embedding configuration changed), the search incrementally refreshes the index — reusing cached vectors for unchanged content hashes — before continuing. If another refresh is already running, the search waits up to `wait_for_existing_job_seconds` and then reuses that job's result; if the embedding service is unavailable, the previous snapshot is kept and the search still returns results with a warning explaining the refresh could not complete. `freshness = "manual"` never refreshes automatically; use `index_workspace` for an explicit refresh. `freshness = "watch"` leaves refresh to `watch_workspace`'s background poller instead of checking on every search, which suits long-running editor sessions.

The search result's separate `index` object explains first-search latency. `action: created` means that request performed initial indexing, `waited_for_existing_job` means another concurrent request performed it, `refreshed_incrementally` means the search found and applied a staleness refresh before continuing, and `reused` means a compatible, current-enough snapshot was already available. `wait_ms` covers index creation, refresh, or waiting and is zero or near zero for plain reuse; it does not overload retrieval-stage timing fields. Indexing is synchronous and finishes when the new index is persisted, so allow a long client tool timeout for a first search, an automatic refresh, or an explicit indexing pass. Index writes are serialized in the running service, with per-workspace read/write coordination. This version is intended for one running service per data directory; multiple clients should connect to that service.

Readiness requires a writable application data directory, accessible embedded LanceDB storage, a reachable embedding endpoint, and the configured embedding model in its OpenAI-compatible model listing. Reranking, ripgrep, rust-analyzer, csharp-ls, typescript-language-server, and pyright are optional fail-open components. Readiness reports each language server independently; an absent one never disables syntax retrieval for its language. Dependency probes use `readiness_timeout_seconds`; readiness never downloads, starts, stops, or manages dependencies, and it never inspects or contacts the independent generation service on port 8765.

Watching is opt-in for each server run. It polls only after `watch_workspace`, debounces a detected source change, and calls the same safe indexing path. Watch registrations are not restored after a process restart; persistent indexes and manifests are. A failed automatic update is logged and retried after another poll while the prior LanceDB snapshot remains searchable.

The first applicable LSP request for a workspace starts one long-lived provider process per language server; later calls reuse it. A failed request discards the process and retries once with a fresh provider. `search_code` queries each applicable, enabled provider independently only when filtered indexed chunks contain a language it navigates, and maps locations only to same-language chunks. A missing or disabled optional server leaves semantic and lexical retrieval available. Direct navigation rejects unsupported extensions before startup and returns an MCP error when the selected provider is disabled or unavailable. Definition/reference input lines are one-based; character offsets follow LSP and are zero-based UTF-16 units.

## Direct CLI and acceptance test

The CLI calls the same application logic and is useful for testing without configuring an editor. Use it while the server is stopped to avoid concurrent processes writing the same data directory.

```bash
lci=./target/debug/local-code-intelligence   # local-code-intelligence.exe on Windows
gust=/path/to/gpu-dialect-v0
"$lci" index "$gust"
"$lci" status "$gust"
"$lci" list-files "$gust"
"$lci" search "$gust" 'lower syn AST expressions into generated Slang compute shader code'
"$lci" symbols "$gust" 'emit_expression#'
"$lci" definition "$gust" 'crates/gust-macros/src/slang/mod.rs' 266 24
"$lci" references "$gust" 'crates/gust-macros/src/slang/mod.rs' 581 8 --include-declaration
"$lci" index "$gust"
```

The second indexing pass should report zero new embeddings if source and model configuration are unchanged. GUST's actual translator is under `crates/gust-macros/src/slang`; inspect the returned source, not just path names. `cargo xtask acceptance core` saves the index, query, and repeat-index reports and checks that a majority of top-eight results are actual Slang translator implementation chunks, including the expression translator.

Search accepts repeatable `--language`, `--include-path`, `--exclude-path`, and `--source-role` filters. Path filters are case-sensitive repository-relative globs: `*` and `?` stay within one path component, while `**` crosses directories. Absolute paths, backslashes, traversal components, empty patterns, and malformed globs are rejected. An explicitly empty include list is a valid filter that returns no results. Requested filters are applied to every retrieval channel and reported back as effective filters; no result may bypass them.

Each result includes a deterministic source role. Classification precedence is generated, test, benchmark, example, documentation, configuration, then source. Production source receives a small transparent `0.004` fusion prior; all other roles receive zero. The role and prior are included in result metadata, and explicit role filters remain authoritative.

Run the portable retrieval evaluation with the same application search path:

```bash
"$lci" evaluate ./evaluations/core.toml \
  --workspace "self=$PWD" \
  --workspace "gust=/path/to/gpu-dialect-v0" \
  --output ./test-results/evaluation.json
```

Workspace mappings are supplied at runtime, so checked-in definitions contain no machine-specific absolute paths. The GUST workspace is optional; its queries are explicitly skipped when no mapping is supplied. Without `--output`, JSON is written to stdout. A required missing workspace, search error, expected-path miss, wrong expected role, or missing required snippet produces a failed query and a nonzero process exit after the JSON report is emitted. Valid optional skips do not fail the run.

Reports contain bounded path, line-range, role, rank, score, and preview evidence; per-query hit@1, hit@3, hit@8, reciprocal rank, preference/disfavor counts, role distribution, reranker state, timings, lifecycle, and effective filters; and aggregate hit rates, MRR, median/p95 latency, fallback count, and created/reused/waited index counts. Generated reports belong under `test-results` and are not committed. Evaluation reuses compatible persisted indexes and preserves the existing first-search lifecycle behavior. It calls only the configured embedding and reranking services, whose defaults are ports 8766 and 8767; application paths never contact port 8765.

`cargo xtask acceptance lsp` independently checks workspace-symbol, definition, and reference navigation against the same translator and saves each normalized response in `test-results`.

With the real embedding and reranking services running on ports 8766 and 8767, the mixed-language acceptance creates a disposable fixture and data directory under `test-results`, uses the actual debug binary, and never contacts port 8765:

```bash
cargo xtask acceptance multilingual
```

It verifies all supported language IDs, semantic and lexical retrieval with live reranking, production TypeScript, Python, and C# ranking over test decoys, zero-work unchanged indexing, one-file invalidation, deletion, restart reuse, and prior-snapshot retrieval after a deliberately unreachable embedding endpoint causes an update to fail. Reports are written as `test-results/multilingual-*.json`.

C# language-server acceptance is split across three more subcommands: `cargo xtask acceptance csharp-lsp` (real `dotnet` fixture + `csharp-ls`, plus `--probe-only` for a standalone JSON-RPC protocol probe with no LCI binary involved), `cargo xtask acceptance csharp-missing` (provider-isolation/degradation when `csharp-ls` is unavailable), and `cargo xtask acceptance csharp-recovery` (persistent process reuse and forced-kill recovery). Each requires `dotnet` and `csharp-ls` on `PATH` and skips gracefully (printing `PREREQUISITE_UNAVAILABLE`) when they're missing.

TypeScript/JavaScript and Python follow the same three-subcommand pattern: `cargo xtask acceptance typescript-lsp` / `typescript-missing` / `typescript-recovery` against a real npm-scaffolded fixture and `typescript-language-server` (`--typescript-language-server <path>` overrides discovery), and `cargo xtask acceptance python-lsp` / `python-missing` / `python-recovery` against a real Python package fixture and `pyright` (`--pyright <path>` overrides discovery). Both skip gracefully when their server isn't found on `PATH`.

## Retrieval and persistence behavior

1. Scan exact lowercase `.rs`, `.ts`, `.tsx`, `.js`, `.jsx`, `.py`, `.cs`, `.go`, and `.java` extensions recursively with `.gitignore`, nested ignores, and standard `ignore` crate rules. The checked-in `.lciignore` is honored as an additional ignore file and keeps agent control-plane material (`.agents`, `.github/agents`, `.github/instructions`, `.github/skills`, `.codex`, `.claude`) and generated `test-results` out of this repository's searchable corpus while leaving those files available to agents on disk. Symlinks are not followed and `.git` is skipped. Hidden source files are eligible when not ignored. Read/traversal errors abort the update and preserve the previous index.
2. Select a static language adapter and Tree-sitter grammar by extension. Rust retains its original declaration boundaries. TypeScript/JavaScript chunks preserve imports, executable top-level statements, exports, comments/decorators, functions, classes and methods, interfaces and signatures, type aliases, enums, and variable-assigned arrow functions. Python chunks preserve imports, assignments, executable top-level statements, synchronous functions, asynchronous functions, classes, methods, decorators, and associated leading comments. C# chunks preserve using directives, namespaces, top-level statements, types, members, attributes, XML documentation comments, and local functions. Go chunks preserve top-level functions, methods, type/const/var declarations, and associated leading comments. Java chunks preserve packages, imports, classes, interfaces, enums, records, methods, constructors, fields, annotations, and Javadoc comments. Oversized syntax splits only at named syntax boundaries toward 6,000-byte groups, with declarations preserved up to 24,000 bytes. Large indivisible leaves stay whole rather than being truncated; parser error recovery remains searchable.
3. Hash each complete source file and persist its parsed chunks, language identifier, and adapter version in the external manifest. A later manual or watched pass reparses only changed/new files or files whose selected adapter version changed, including after restart. Fingerprints include path, content, language, and adapter version. A missing, incompatible, or malformed manifest safely causes a full parse.
4. Hash each exact source chunk. Reuse vectors from the current workspace snapshot when content hashes and the stored embedding URL/model/document-format identity match. Parser adapter versions are intentionally decoupled from embedding compatibility, so unchanged chunk text keeps its vector. Duplicate new chunks are embedded once. Documents are embedded as source, without a query instruction.
5. Build a complete workspace snapshot in memory and commit a LanceDB table overwrite after embedding succeeds. Save the matching parse manifest only after that commit. Removed chunks disappear from the current snapshot; line-only changes update locations without re-embedding unchanged chunks. Previous Lance versions may remain on disk. This is a straightforward small/medium-repository design, not a streaming indexer for huge monorepos.
6. Embed queries with exactly:

```text
Instruct: Given a code search query, retrieve relevant code passages that answer the query
Query: <query>
```

7. Retrieve semantic candidates with LanceDB cosine distance while ripgrep independently searches useful query terms across all supported extensions. For each language present in the filtered index, its enabled language server (rust-analyzer, csharp-ls, typescript-language-server, pyright) also searches workspace symbols. Map lexical lines to any indexed language and LSP locations only to same-language chunks.
8. Deduplicate and fuse semantic, lexical, and LSP ranks with Reciprocal Rank Fusion. Results expose channel ranks, lexical match count, fusion score, and retrieval channels.
9. Rerank the configured fused shortlist. Semantic, lexical, and Rust LSP channels fail independently; available channels continue. If reranking fails, return fusion order with a warning.

When `ripgrep_path` is the default `rg` or `rg.exe`, resolution checks `PATH` first. On Windows, if `rg` isn't on `PATH`, it also checks common per-user and system-wide VS Code, VS Code Insiders, and VSCodium installations, including versioned application directories, since Windows users are less likely to have a system-wide ripgrep install than macOS/Linux users with a package manager. Any other configured value is treated as an explicit command or path and is used unchanged. If ripgrep cannot be started, lexical retrieval fails open and the warning explains how to set `ripgrep_path`.

Embedding response indices are validated and reordered; vectors must have consistent dimensions, finite values, and nonzero norm. Changing an embedding URL/model requires reindexing. Replacing a model behind an unchanged name/URL is not detectable automatically; use a distinct configured model name or a new data directory for that change.

JSON timing fields measure query embedding, LanceDB search, lexical search, Rust LSP search, fusion, reranking, and total retrieval in milliseconds. Total also includes validation and index access/wait time. The same timings appear in tracing logs on stderr; CLI JSON remains on stdout. Set `RUST_LOG=local_code_intelligence=debug` to adjust logging.

## Module boundaries and checks

- `config` / `workspace`: configuration, canonical identity, external storage boundary.
- `language` / `chunk` / `manifest`: static language adapters, ignore-aware incremental scan, Tree-sitter chunks, and persistent per-file parse reuse.
- `models`: embedding and reranking HTTP clients.
- `lexical`: ripgrep-based lexical candidate channel.
- `lsp`: the shared `LspAdapter` trait and JSON-RPC transport, plus one adapter module per optional language server (rust-analyzer, csharp-ls, typescript-language-server, pyright) and normalized navigation locations.
- `store`: embedded LanceDB schema, snapshot writes, cosine candidates.
- `filter`: retrieval filter validation and deterministic source-role classification.
- `evaluate`: portable evaluation definitions, execution, and metrics; it observes retrieval without changing ranking.
- `app`: indexing/cache coordination and retrieval pipeline.
- `server`: MCP tools and HTTP transport. `main`: CLI/server startup.

There is no BM25, editor-specific integration, Docker setup, generation proxy, or web frontend yet. Later candidate sources can join the same fusion boundary before reranking.

```bash
cargo xtask build --test
cargo xtask test --suite Full
```

`cargo xtask test --suite Full` runs `cargo fmt --all -- --check`, `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings` in sequence, setting `PROTOC` from `.tools/protoc` when the auto-downloaded copy is present. For focused iteration, `cargo xtask test --suite <name>` runs the matching test modules: `Unit`, `Chunk`, `Filter`, `Evaluation`, `Indexing`, `Readiness`, `Mcp`, `Watching`, `Lsp`, or `Full`. The integration suites filter the single `tests/integration.rs` binary by the module names registered there (`evaluation`, `indexing`, `mcp`, `navigation`, `readiness`, `watching`); [docs/development/testing.md](docs/development/testing.md) maps each change to its suite.

Automated tests use a local mock model HTTP service and real embedded LanceDB. They exercise Rust boundary regression, all seven language adapters, schema-v1 migration, per-language parse compatibility, persistent parse/vector reuse across reopen, mixed-language metadata and retrieval, TS/JS-only LSP gating, stale detection, deleted files, failed update preservation, watched refresh, embedding configuration changes, reranker failure/invalid replies, query instruction formatting, ignore handling, and actual Streamable HTTP initialization/tool discovery. Live GUST and multilingual acceptance use the real local Qwen services separately.
