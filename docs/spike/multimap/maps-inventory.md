# Maps inventory

Generated: 2026-10-12T04:04Z (`date -u`). Base commit: `7f9c636` (`origin/fix/verified-bugs`).

There is one row per candidate map. A *map* is an index over LCI's existing code unit: the
Tree-sitter `Chunk` (`lci-core/src/chunk.rs:43`, which holds path, language, line range, code
and content hash), keyed per file in the manifest (`lci-core/src/manifest.rs:12`, `:20`). A map
answers "given a set of chunks or a query, which chunks are next?" Some maps are keyed by
file rather than chunk. They resolve to chunks through the manifest's per-file chunk list.

**All costs below are estimates, not measurements.** They use this repository as the
reference size: 378 tracked files, 98 Rust files of about 2.1 MB in total, about 54k Rust
lines and 188 commits. That gives roughly 1-2k chunks, from `TARGET_BYTES = 6000` and
`MAX_DECLARATION_BYTES = 24000` at `lci-core/src/chunk.rs:10-11`. "Large repo" means about
100x that: around 10k files and 100k-200k chunks. Embedding throughput depends on the
external 8766 service and is the dominant cost for every vector row. Nothing in this table
was timed in this session.

Update model vocabulary:

- **per-file**: rebuilt only for files whose content hash changed, the same way the vector
  index refreshes today (`refreshed_incrementally`).
- **per-commit**: depends on git history, not the working tree. It updates when HEAD
  moves, never on a plain file save.
- **live**: there is no stored index. It is computed per query (the lexical channel today,
  and LSP today).
- **global**: a change in one file can change entries keyed by other files. It needs
  either a dirty-set recompute or an accepted staleness window.

## Inventory

| # | Map | Data source that already exists | Build cost (estimate) | Update on file change | Minimal MVP |
| --- | --- | --- | --- | --- | --- |
| 1 | Lexical | `rg --json --fixed-strings --ignore-case` per query (`lci-core/src/lexical.rs:512-532`); matches are mapped onto chunks in `src/app/search.rs` | None stored today. A query costs one ripgrep pass, roughly 10-100 ms here and seconds on a large repo. An optional stored inverted index (for example tantivy) would be about 10-30% of source size | live (nothing to update); a stored variant would be per-file | Keep it as is. Wrap it behind the map interface as a live map |
| 2a | Vector: chunk x code x default embedder | LanceDB table `ws_<16 hex>`, with columns built in `Store::replace` (`src/store.rs:220`, fields at `:236-255`) and `embedding_document_template = "{code}"` | Exists. With a 2560-dim f32 model (the default `qwen3-embedding-4b` is 2560-dim: an estimate from the model card, not checked here) a vector is about 10 KB, so 15-20 MB here and 1-2 GB on a large repo. Embedding time dominates | per-file (already implemented, keyed by content hash) | It already exists. Register it as map `vec.chunk.code` |
| 2b | Vector: chunk x path+code (another "purpose") | The same chunker, with a different `embedding_document_template` (for example the commented `title: {path} \| text: {code}` variant in `config.example.toml:51`) | Same size as 2a, and a second full embedding pass | per-file | A second LanceDB table under the same `data_dir`, with its own embedding identity in the table metadata |
| 2c | Vector: file-level or summary granularity | The manifest gives each file's chunks; a file is a concatenation of its chunks (truncated) | About 1/10 of 2a's rows (one per file). Here a few MB | per-file | Embed the head of each file plus its symbol names. One row per file |
| 2d | Vector: symbol-signature granularity | Tree-sitter declaration nodes already identified by the adapter (`is_declaration` in `lci-core/src/language.rs`) | 2-5x 2a's rows, but short texts, so cheaper per row. Estimate 1.5-3x the embedding time of 2a | per-file | Embed `kind name(signature)` for each declaration and link it to the enclosing chunk |
| 2e | Vector: second embedder | Any OpenAI-compatible model on 8766. The identity check already exists (`src/store.rs`, `embedding_identity` metadata) | Same as 2a per model | per-file | Out of scope until one model service can serve two models. It needs no product change, only a second identity |
| 3 | LSP symbols | `search_symbols` via language servers (`lci-core/src/lsp.rs`, `lci-core/src/lsp/*.rs`, `src/app/navigation.rs`); workspace/symbol is used as a channel in search | None stored today. A server start takes seconds to minutes (rust-analyzer on this repo is an estimated 10-60 s), and each query afterwards is about 10-500 ms | live | Snapshot `workspace/symbol` or `documentSymbol` per file into a symbol table (name, kind, file, range, chunk id); a few hundred KB here |
| 4 | LSP references | `find_references` (`src/server.rs`, `src/app/navigation.rs`) | Live calls of about 50 ms to 2 s each. Precomputing all references is expensive (one request per symbol), estimated minutes here | live, or per-file with global invalidation | Keep it live and use it as a traversal step from a chosen symbol, not as a prebuilt map |
| 5 | Call graph | Derivable from LSP `callHierarchy` (rust-analyzer supports it) or approximated from Tree-sitter call expressions plus name resolution through the symbol map (3) | Tree-sitter approximation: one parse pass (already done at chunk time), estimated <5 s here and an edge list under 1 MB. An LSP-exact graph costs minutes | per-file for outgoing edges; incoming edges are global | Tree-sitter `call_expression`/`method_call_expression` callee names resolved by name against the symbol map. It is imprecise but cheap |
| 6 | Module/import graph | Rust `mod`/`use` and the other languages' import statements, all visible in the Tree-sitter trees already parsed | <1 s here (it reuses the parse) and is tiny | per-file (edges from the file); the reverse index is global but cheap | Tree-sitter queries for `use_declaration`/`mod_item` (Rust) and `import` (TS/Py/Go/Java), with file-to-file edges |
| 7 | Language and extension map | `CachedFile.language` in the manifest (`lci-core/src/manifest.rs:12-17`) and the registry in `lci-core/src/language.rs` | Free, because it already exists | per-file | Expose the manifest's language field as a filter map. The `language` filter already does this (`lci-core/src/filter/request.rs`) |
| 8 | Agent and AI instruction files | `AGENTS.md`, `CLAUDE.md`, `.agents/skills/**`, `README.md`, `docs/**`. The checked-in `.lciignore` deliberately excludes the control plane from the code corpus | Small: about 240 Markdown files here (a few MB). Vectorising them costs about the same as 10-20% of 2a | per-file | A separate Markdown chunker (split on headings), with its own table or map so that it never competes with code ranking. Path and heading links to code mentions in backticks |
| 9 | Git co-change | `git log --name-only` / `--numstat`. `xtask/src/mine.rs` already parses logs, numstat and renames (`parse_log` `:205`, `parse_numstat` `:233`, `collect_renames` `:309`, `chase_renames` `:334`) | One `git log` pass here takes <1 s. Pairs are O(sum of files-per-commit²), so commits above N files need a cap. Storage is a sparse pair table, KB to MB | per-commit | Count file-pair co-occurrence over the last N commits with a commit-size cap, and rank partners by support and confidence. Reuse the parsing in mine.rs |
| 10 | Git recency | `git log -1 --format=%ct -- <file>` or a single log pass | <1 s here and tiny | per-commit (the working tree's mtime is a fallback for uncommitted files) | A last-touched timestamp per file used as a prior or filter, never as a hidden score (per the retrieval contract) |
| 11 | Git blame | `git blame --porcelain` per file | Expensive: an estimated seconds per large file, and minutes for a full repo here. Storage is line-range to commit | per-commit, per file | Lazy blame for a single chunk when asked by a traversal step, cached by (path, blob sha) |
| 12 | Commit messages | `git log` subjects and bodies; `xtask/src/mine.rs` already turns them into queries (`query_text` `:484`) | Text is tiny. Embedding about 200 messages here is trivial; a large repo might have 100k | per-commit | A commit -> touched-files map, plus lexical search over messages. Optionally vectors for message text |
| 13 | Test-to-code links | `#[test]`/`#[tokio::test]` functions (in about 40 files here, under `tests/cases/*.rs`, `lci-core/src/**/tests.rs` and inline `#[cfg(test)]` modules); the source-role classifier (`lci-core/src/filter/source_role.rs`) already tags test files | Name-based linking takes one pass and <1 s here. Linking by calls needs the call graph (5) | per-file for outgoing links | Test chunk -> symbols called in its body (via map 5), plus a module-path heuristic (`foo/tests.rs` -> `foo.rs`) |
| 14 | Config key -> reader sites | Config struct fields and `serde` names in `lci-core/src/config.rs` and `lci-core/src/config/layers.rs`; `config.example.toml` sections such as `[channels]`, `[fusion]` and `[index]`; `LCI_*` env names | Tiny. It is one Tree-sitter/regex pass over the config modules plus the field-access references | per-file, with global invalidation when a field is renamed | Pair TOML keys in `config.example.toml` with struct field names, then link to `config.<section>.<field>` access sites by lexical search |
| 15 | Error/log strings -> raise sites | String literals in `bail!`, `ensure!`, `anyhow!`, `warn!`/`tracing` calls and `Err(..)` (for example `src/store.rs:234` "embedding dimension changed; reindex with a stable model") | One parse pass, <1 s, KB of storage | per-file | Index string literals inside known macro calls. A message fragment maps to the raise-site chunk, then to its callers (map 5) |
| 16 | Entrypoints | MCP `#[tool]` handlers (`src/server.rs:89` onward), REST routes (`src/rest.rs`), clap subcommands and flags (`src/main.rs`), xtask commands (`xtask/src/main.rs`) | Tiny, from one parse pass | per-file | A list of (kind, name, handler symbol, chunk). Parse `#[tool]` names, the router `.route(..)` calls and clap derive attributes |
| 17 | Build graph | `Cargo.toml` workspace members (root, `lci-core`, `xtask`), `cargo metadata`, plus `[features]` and `#[cfg(..)]` attributes | `cargo metadata` takes about 1 s. Tiny | per-file (on a Cargo.toml change) | Crate -> files and crate -> crate edges from `cargo metadata`. Ignore features and cfg in the MVP |
| 18 | Trait/impl hierarchy | Tree-sitter `impl_item` (`impl Trait for Type`) and the equivalents in other languages; LSP `textDocument/implementation` for exact results | Tree-sitter: one pass, tiny | per-file, with a global reverse index | Edges `impl Trait for Type` -> trait name and type name, resolved by name through the symbol map |
| 19 | Doc-to-code links | Markdown in `docs/`, `AGENTS.md` and `README.md` that names paths (`src/app/search.rs`), `path:line` references and backticked identifiers | One regex pass over about 240 Markdown files, <1 s | per-file (for the doc). Links break when code moves, which can be checked against the symbol map | Extract backticked paths and identifiers, then resolve them against the manifest's file list and the symbol map |
| 20 | Ownership | There is no CODEOWNERS file here. A proxy is `git shortlog` per path, which yields personal names and must stay out of committed output | Cheap | per-commit | Skip for this repository (single owner). Keep it as a profile-gated map for multi-team repos, storing hashed or anonymised ids |
| 21 | Cross-language boundaries | FFI, `extern`, generated bindings, shell or Python scripts that call the binary (10 `.sh` files and 1 `.py` file are tracked here, all outside the code corpus) | Small | per-file | Skip in the MVP. This repo has almost no cross-language code; revisit it for TS<->Rust or C<->C++ repos |

## Notes for the repository profile

A cheap profile pass can choose which rows to build. It needs only the manifest, `git
rev-list --count HEAD` and `cargo metadata` where applicable:

- language mix (manifest languages): gates maps 3-6 and 18 per language server or grammar;
- file count and chunk count: gate the expensive vector rows (2b-2e) and blame (11);
- commit count and median files per commit: gate co-change (9). Below about 50 commits
  it is noise. This repository has 188;
- presence of instruction files, CODEOWNERS, `Cargo.toml`/`package.json` and test
  directories: gates maps 8, 20, 17 and 13.

A sensible MVP set for the spike is the maps that already exist or need only one cheap parse
or git pass: 1, 2a, 3 (live), 6, 7, 9, 13, 15 and 16. Everything else should wait until that
set shows a measurable gain on `eval-seed.json`.

Invariants carried from `AGENTS.md` that every row must respect: there is no additional
database or service (a new map is a LanceDB table or a file under `data_dir`); each map fails
open independently; filters apply before reranker input; and the freshness policy
(`on-search`/`manual`/`watch`) governs every per-file map the same way it governs vectors
today.
