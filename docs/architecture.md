# Architecture guide

`local-code-intelligence` (LCI) is a standalone local service: an MCP server and a plain REST mirror, plus a command
line, over one process. It owns its indexes and keeps persistent data in `data_dir`, outside every indexed repository.
Clients (Kilo, Copilot, Codex, any MCP-capable agent) are not special: client-specific behaviour does not belong in
the service. Behaviour and surfaces are described in [README.md](../README.md); this file describes the structure.
Facts here were checked against the code of the `spike/v2` branch on 2026-10-17; names are symbols, not line numbers.

## Crates

The Cargo workspace has four members: the root package `local-code-intelligence`, `lci-core`, `lci-maps` and `xtask`.

| Crate | Path | Holds | Depends on |
| --- | --- | --- | --- |
| `local-code-intelligence` (root) | `src/` | `app`, `store` (the only code that imports LanceDB and Arrow), `evaluate`, `server` (MCP), `rest`, `init` (`lci init`), `main` (CLI). Re-exports the `lci-core` modules from `src/lib.rs` so `crate::chunk`-style paths resolve. Cargo feature `map-equivalence` compiles test-only entry points (`App::fuse_through_maps`, `App::record_through_maps`) and the retrieval channels wrapped as maps (`src/app/search/channel_maps`). | `lci-core`, `lci-maps` |
| `lci-core` | `lci-core/src/` | Pure logic: `bundle`, `chunk`, `config` (with `config::layers` and `config::profile`), `endpoint`, `evaluation` (portable evaluation definitions and validation), `filter`, `language`, `lexical`, `lsp`, `manifest`, `models`, `node` (stable `NodeId`), `path`, `provider` (completion providers for System One), `workspace`, plus the Tree-sitter grammars | none of LanceDB, Arrow, axum, rmcp |
| `lci-maps` | `lci-maps/src/` | The maps, the registry and the traversal engine behind `search_code mode="traverse"`, `lci traverse` and the recipes: the `Map` trait, `Registry` (fail-open, fusion in today's arithmetic and order), the traversal engine (`traverse`, `walk`), the maps (SCIP graph and impact, config key, git history, live grep, LSP, search, symbol definitions, probe), the contract kit, the RunRecord writer | `lci-core` |
| `xtask` | `xtask/src/` | `cargo xtask` subcommands setup, build, test, acceptance, live-acceptance, env, lint-machine-paths, docs, gold, maps, mine-queries | `lci-core`, `lci-maps` |

The split exists for build time (commit `0729d9a`): the LanceDB/Arrow tree takes minutes to compile, so a change that stays inside
`lci-core` is checked in seconds with `cargo check -p lci-core` and `cargo test -p lci-core --lib <module>::` (see
[testing.md](development/testing.md)). A generic map can live in `lci-core` or `lci-maps`; anything stored in LanceDB
is implemented in the root crate (the retrieval channels wrapped as maps are in `src/app/search/channel_maps`).

## Runtime boundaries

| Module | Crate | Role |
| --- | --- | --- |
| `app` | root | `App` is the public facade: coordinates indexing, retrieval, navigation, watching and readiness. Children: `indexing`, `search` (its report's `freshness` block under `verify_on_read`), `navigation`, `readiness`, `compact`, `traverse`, `recipes` (see `src/app/AGENTS.md`) |
| `chunk`, `language` | core | `chunk` scans supported files into chunks (Tree-sitter for code, a heading splitter for Markdown); `language` is the registry of language adapters and their cache versions |
| `store` | root | Embedded LanceDB persistence; the only LanceDB and Arrow code |
| `models` | core | Embedding client (default port 8766) and fail-open reranker client (default port 8767) |
| `lexical`, `lsp` | core | Independent, fail-open retrieval channels (ripgrep with BM25 scoring; language-server symbols) |
| `filter` | core | Validates retrieval controls (`FilterRequest` to `EffectiveFilter`) and classifies source roles |
| `config` | core | Loads and validates settings; `config::layers` resolves default < file < local overlay < `LCI_*` environment < flags, and `lci config` prints each value with its source; `config::profile` is the `[run_profile]` section (`[run_profile.walk]`: `groups`, tool or claim, and `grep_corroboration`, which the traversal reads through `lci_maps::traverse::Grouping`, and `pricing`, rank or relative, which `app::traverse::run` passes to the hop price (`lci_maps::traverse::frontier::HopPrice`); `[run_profile.traverse]`: `map_freshness`, which `app::traverse::run` reads from the run profile it is given; `[run_profile.scip]`: `mode`, authoritative, cache or off, which `app::traverse` reads when it loads the SCIP index; `[run_profile.lsp]`: `sessions`, per call or shared, which `app::traverse::run` reads to pick the call's language-server manager, and `outline_cache` and `sync`, which `lsp::Manager::new` reads; `[run_profile.freshness]`: `verify_on_read`, which `app::traverse::run` reads from the run profile it is given) and `Config::run_profile()`, the hash of the result-affecting settings; `App::open` caches it (`App::profile_hash`) and `app::stamp` writes it as `profile_hash` into every MCP and REST result (`server::result`, `rest::respond`; the compact `search_code` writes it as its last key) |
| `evaluate` | root | Measures retrieval behaviour without influencing ranking (see Evaluation harness) |
| `traverse`, `recipes`, `compact` | root `app` | Graph traversal mode of `search_code` and `lci traverse` (`src/app/traverse/`: `mod` holds the types, `run` and registration; `index` the SCIP index and map loading; `seeds` the seed search; `rescore` the `GraphRescorer`; `response` the response builders; `first_stage`, `presets`, `chunks`); named recipes and `lci_compose`; the compact `search_code` presentation. Engine and maps: `lci-maps` |
| `server`, `rest`, `main` | root | MCP (rmcp, streamable HTTP), the REST/OpenAPI mirror on the same process and port, and the CLI |

## Ports and services

| Port | Service | Contacted by LCI |
| --- | --- | --- |
| 8768 (`DEFAULT_PORT`) | LCI itself: MCP, REST, `/health`, `/ready`, `/openapi.json` on loopback | serves |
| 8766 | OpenAI-compatible embedding service (`embedding_url`) | yes |
| 8767 | Reranker (`reranker_url`) | yes, fail-open |
| 8765 (`GENERATION_MODEL_PORT`) | A separate generation model | never: production code, tests and acceptance scripts must not contact or manage it |

`RESERVED_PORTS` is `8765..=8767`; a configured `port` inside it, and any model URL on 8765, is rejected at startup.
Tests never contact 8765 to 8767 (they use fakes under `tests/support`). The `[system_one]` model and the embedder and
reranker are each a URL, a model name and optionally the NAME of an environment variable holding an API key
(`lci-core` `endpoint`); a model service may be local, remote or hosted. Model-serving commands are not in the repository ([machine-values.md](development/machine-values.md)).

## Chunks and the manifest

- `Chunk` (`lci-core` `chunk`): `relative_file_path`, `language`, `start_line`, `end_line` (1-based, inclusive), `code`
  (trimmed), `content_hash` (SHA-256 hex of `code`, `workspace::hash`). `TARGET_BYTES = 6000`,
  `MAX_DECLARATION_BYTES = 24000`; `DOCUMENT_FORMAT_VERSION = "rust-chunks-v1"` is part of `embedding_identity`. Chunk
  sizes measured on this repository on 2026-10-11 (roadmap Snapshot, before the cleanup): median about 200 B, p90
  about 1.3 KB, p99 about 5 KB, max 17 KB (declarations are kept whole up to 24,000 B).
- Splitting: recursive at container, boundary and declaration node kinds (`ranges`); prefixes such as doc comments and
  attributes stay attached to the next child; a large leaf is never cut into text windows; chunks that are empty or only `{};` are skipped (`chunks_with_adapter`).
- `Scan` result: `chunks`, the per-file `files` map (becomes the manifest), `fingerprint`, and the counts `parsed`, `unchanged`, `removed`, `lossy`, `skipped`.
- Walker (`sources`): `ignore::WalkBuilder` with `.hidden(false)`, `.follow_links(false)`, `.require_git(false)`,
  `.lciignore` as a custom ignore file, `.git` skipped. Only files with a registered extension are read; non-UTF-8
  files are decoded lossily; Qt Linguist `.ts` files are skipped (`NonSource::QtLinguist`).
- Incremental scan (`scan_incremental`): a refresh does not use mtimes; every refresh re-reads and re-hashes every
  source file, and a file's cached chunks are reused when `content_hash`, `language` and
  `adapter_version` all match; output is sorted by `(path, start_line)`. The staleness `fingerprint` is SHA-256 over
  `path\0content_hash\0language\0cache_version` of every source file; skipped files are left out.
- Language registry (`language::ADAPTERS`): 18 extensions (rs, ts, tsx, js, jsx, py, cs, go, java, c, h, cpp, hpp, cc,
  cxx, hh, hxx, md) and 12 identifiers (`h` maps to `c`; hpp, cc, cxx, hh and hxx map to `cpp`; the registry never inspects
  content to choose a language). Each adapter has its own `cache_version` (for example `rust-chunks-v4`); lookup is
  `for_extension` and `for_path`. This table is the whole indexable set; `.md` files are indexed only with `[index].prose = true`. Each adapter has an extractor kind: Tree-sitter (a grammar plus a syntax description) or prose (`.md`: `chunk::markdown`, a hand-written heading splitter, no grammar and no language server). RST, AsciiDoc, text, TOML, YAML, JSON, build files and agent instruction files have no chunks.
- Chunk identity: no chunk-id column exists. Fusion keys a chunk by `(relative_file_path, start_line, content_hash)`;
  the vector cache by `content_hash`, or by `content_hash` plus path when the document template contains `{path}`
  (`vector_key`); the fused-order tie-break adds `end_line` (`fusion_order`: score, path, start, end, hash). An edit
  above a chunk changes its fusion key. Node ids for the traversal engine (`NodeId`, `chunk_ids`) are derived from the chunk lists and are not stored.
- Manifest (`lci-core` `manifest`): `version` (`VERSION = 2`), `workspace_id`, `workspace_path`, `fingerprint`, and
  `files: HashMap<path, CachedFile>` (`language`, `adapter_version`, `content_hash`, the full `Vec<Chunk>` including
  code). A manifest with another version, workspace id or path, a file whose language does not match its adapter, or a
  chunk whose hash does not match its code loads as empty and forces a full re-parse. `ManifestV1::migrate` is the
  precedent for a version bump. Stored as `<data_dir>/manifests/<workspace_id>.json`, written to a `.tmp` file and renamed.
  Per-file bundles of the multimap spike (`lci-core` `bundle`) live under `<data_dir>/bundles/<workspace>/` and are not used by the scan or search path.

## Indexes and channels

- **LanceDB store** (`src/store.rs`, opened at `<data_dir>/lancedb`). One table per workspace, `ws_<first 16 hex of
  the workspace id>`; the id is SHA-256 of the canonical path. A legacy name `ws_<64 hex>` is still read and dropped at
  the next commit. Utf8 columns `workspace_id`, `relative_file_path`, `language`, `code`, `content_hash`; UInt32
  `start_line`, `end_line`; `vector` is `FixedSizeList<Float32, dim>`. Schema metadata: `embedding_identity`,
  `workspace_path` (a mismatch is a table-name collision and fails loudly), `indexed_at` (unix seconds).
- **Commit and snapshots**: `Store::replace` rewrites the whole table with `CreateTableMode::Overwrite` on every index
  pass; unchanged chunks reuse cached vectors but every row is written again. `Store::snapshot` pins the current table
  revision (`checkout_current`), so a search keeps reading one revision while an index job commits.
- **Semantic search** (`Store::search`): cosine distance; first fetch `min(4 * limit, 160)` rows, then rows are
  filtered with `EffectiveFilter::matches`; only when the filter rejected rows and too few passed is the fetch repeated
  with four times as many rows, up to `MAX_FILTERED_SEMANTIC_ROWS = 10240`, after which a warning says matching chunks
  farther away were not considered. `Store::chunks` reads every row except `vector`; every search calls it.
- **Embedded text**: `document_text` fills `embedding_document_template` (default `{code}`; `{code}` and `{path}`); a
  non-default template changes `embedding_identity`. The query text fills `embedding_query_template`. `Hit` is the
  serialized result row: the flattened `Chunk` plus `file_path`, semantic/lexical/LSP ranks and scores,
  `lexical_match_count`, `fusion_score`, `retrieval_channels`, `reranker_score`, `source_role`, `source_role_prior`.
- **Lexical channel** (`lci-core` `lexical`): no persisted index. Each query runs a live `rg --json --line-number
  --ignore-case --fixed-strings` with one `--glob` per adapter and one `-e` per term. Terms come from `query_terms`:
  at most `MAX_QUERY_TERMS = 24`, stop words removed, identifiers split. A `TermMatcher` bitmask credits each line with
  the terms it really holds; `App::map_lexical_matches` maps lines onto chunks; `score_chunks` scores BM25 with
  `BM25_K1 = 1.2`, `BM25_B = 0.75` (chunk length is its line count, document frequency is counted over indexed files
  only). Terms in more than 25% of files are dropped (`MAX_DOCUMENT_FREQUENCY`) unless that would drop every credited
  term; at most 12 terms score, rarest first (`MAX_SCORING_TERMS`). Capped at `lexical_candidate_count` (40). `rg` is
  not given `--hidden` or `.lciignore`, so it skips hidden files the scanner indexes (those drop out because only lines
  inside indexed chunks count), and it reads the live tree while chunks come from the snapshot, so line ranges can drift under `manual` or `watch` freshness.
- **LSP channel** (`lci-core` `lsp`): answers are merged query by query across providers and first-seen order decides the LSP rank (`lsp_channel` in `src/app/search.rs`). `LspAdapter` is a trait-object registry; `Manager` holds 7 adapters (rust-analyzer,
  csharp-ls, typescript-language-server, pyright, gopls, jdtls, clangd), sessions keyed by `(provider, workspace_id)`,
  started on a detached task shared by concurrent callers and primed with one background `workspace/symbol`. Idle
  sessions are released after `lsp_idle_timeout_seconds` (1800) and a session that answers only transient errors for
  `lsp_stuck_seconds` (900) is replaced. Requests: `symbols` (`workspace/symbol`), `definition`, `references`, all
  returning `Location` (path, range, name, kind, language, provider). Search use: up to 8 symbol queries (a term plus
  `#`, plus a singular form for plurals), providers in parallel, each under `lsp_search_budget_ms` (1500); a provider
  over budget adds a note, a provider that fails adds a warning; locations map onto chunks of the same language that
  contain `start_line`; capped at `lsp_candidate_count` (40). Navigation tools (`symbols`, `definition`, `references`)
  are not fused, return raw `Location`s and need an index containing the file's language. The `lsp.live` map of a
  traversal (`lci-maps` `lsp_live`) asks a `Manager` it is given (`ManagerBackend::open(workspace, Arc<Manager>)`). Under
  `[run_profile.lsp] sessions = "per_call"` (default) `app::traverse::run` builds one per call, so the map owns it and its
  server dies with the map; under `"shared"` it uses `Call.lsp_sessions`, the `Arc<Manager>` that `App::language_servers()`
  hands out (the service's own, so a server started by navigation, search or a traverse call serves the others; the CLI
  `traverse` builds one per process and shuts it down after the walk), and the server's lifetime is the manager's (idle
  sweeper, `release_workspace`, shutdown). `Manager::probe_once` caches a successful version probe per provider for
  the manager's life. The recipes walk no `lsp.live` and pass no manager. `[run_profile.lsp] outline_cache` (read by
  `Manager::new` from the config it is built with: the App's, the CLI's, or `Call.lsp` for a per-call manager) gives the
  manager an outline cache, used only for adapters whose `LspAdapter::opens_documents()` is true (pyright, gopls,
  jdtls, clangd: the hook resends the disk text and the server takes it): `Manager::document_symbols` keys each
  non-empty answer by (provider, workspace id, path),
  `lsp::content_hash` of the file (`workspace::hash`, as the manifest) and the id of the server session that gave it,
  at most `MAX_CACHED_OUTLINES` (4096) entries. With `[run_profile.lsp] implementations = true`
  (default `false`) the map also asks `Manager::implementation` (`textDocument/implementation`) at each symbol node
  and offers `Implements` edges from an interface or trait member to its implementing members (the direction of
  `scip.implements`); it is the third request of a node, behind the same `max_external_calls` gate, and a failure of
  it is recorded without dropping the edges of the first two. With `[run_profile.lsp] call_hierarchy = true`
  (default `false`) it asks up to three more requests per symbol node, each behind the same gate and counted:
  `Manager::prepare_call_hierarchy` (`textDocument/prepareCallHierarchy`), then `Manager::incoming_calls` and
  `Manager::outgoing_calls` with the item the server gave, sent back verbatim (`HierarchyItem::raw`; the place of an
  item is its `selectionRange`, the name). The callers and callees are `CallsOrRefs` edges (witness `Exact`, no new
  edge kind), one per destination per expansion, seen at the call (`fromRanges`). Every error reply is recorded
  and keeps the other edges, which is also how a server without `callHierarchyProvider` (it answers -32601) is found:
  the capabilities are not read.
  The symbol-to-place table behind the map (`lsp_live::SymbolPlaces`) is `ScipPlaces`, built from the SCIP graph's
  nodes, under `[run_profile.lsp] places = "scip"` (default). With `places = "outline"` it is `OutlinePlaces` over it:
  `ask` outlines a symbol's file once per map instance (`Manager::document_symbols`, one counted external call;
  `SymbolPlaces::outline_wanted`), as it already does for a file or chunk node, and `SymbolPlaces::learn` takes the
  outline in. A member whose kind this build has a node for (`SymbolKind`: functions, methods, types, enums,
  modules, constants; LSP kinds 12, 6, 9, 5, 23, 11, 10, 2-4, 14) takes the SCIP node's id for the same file, name and
  nearest line, else (a Rust file, `OutlinePlaces::with_tree`) the canonical id of the `chunk::extract_for_path` declaration around the member, else a minted `SymbolKey` with the file's `language::for_path` identifier and the equal names
  numbered in document order; where a file is learned its places win over the index's, which may be older than the
  tree. `flatten_document_symbols` gives a nested `DocumentSymbol` the range from its name to its item's end, so
  `Member::end_line` is the last line of the item.
  it is recorded without dropping the edges of the first two.
  `[run_profile.lsp] sync = "watch"` (read by `Manager::new`; a per-call
  manager is built `without_sync`) gives each workspace with a session an `lsp::sync::WorkspaceSync`: a `notify`
  watcher of the root (polling at `watch_poll_milliseconds` when the platform watcher fails) whose events a task
  drains into batches, typed by the disk (created, changed, deleted) against a `(mtime, length)` table from one
  `chunk::source_files` walk (walked again only after an overflow). The manager's sink drops the files' kept outlines
  and sends each batch to the workspace's sessions as `workspace/didChangeWatchedFiles`; `Manager::client` drains
  before every request. The sync starts with the workspace's first session and stops with its last (`release`,
  `shutdown_all`, the idle sweeper). Lock order: drain lock, then the session registry, then `syncs`.
- **Model clients** (`lci-core` `models`): `embed` POSTs to `<embedding_url>/embeddings` and validates count, index,
  finiteness, non-zero vectors and one dimension; indexing retries up to 3 attempts (`MAX_ATTEMPTS`) with doubling
  backoff, `query` makes one attempt. Any refused connection trips a breaker for 30 s (`EMBEDDER_DOWN_WINDOW`): `query` and `embed_once` then skip at once
  with an error `is_service_unavailable` accepts, and index-time `embed` makes one probe attempt without retries or
  backoff (a failed probe re-arms the window, an answer closes the breaker). An HTTP 5xx does not trip it.
  `rerank` makes one attempt and returns `(index, score)` best first; after a
  refused connection a breaker skips the reranker for 30 s (`RERANKER_DOWN_WINDOW`). Batches the service rejects as
  oversized are retried one input at a time with prefixes halved down to 512 bytes.

## Indexing flow and freshness

`App::index` takes the per-workspace write lock and calls `index_locked`: (1) the process-wide `index_lock` and the
cross-process OS lock on `<data_dir>/index.lock`; (2) `manifest::try_load`, then `chunk::scan_incremental` on a blocking
thread; (3) open the previous snapshot, and when its identity matches, load `cached_vectors` and embed only the missing
keys (deduplicated, in batches of `embedding_batch_size`); cached vectors of another width than the service returns now
are embedded again; (4) `Store::replace`, then `manifest::save` (when embedding fails with an availability error per `models::is_service_unavailable` and the vector cache is empty under an unchanged identity, `Store::replace_vectorless` commits one placeholder vector per chunk and table metadata `vectors = "missing"` instead: `Snapshot.vectors_missing`, `embedding_dimension` null in `Status`, no vector read served); (5) return an `IndexReport` (counts, lossy, truncated
and skipped files, timing). `WorkspaceCoordination` holds the per-workspace `RwLock`, the `flight_error` shared by
concurrent first-index waiters, and the `last_stale_check` throttle. A search does not hold a lock while it reads: it
works on a pinned snapshot.

Freshness is `[index].freshness` (`IndexFreshness`), resolved per search in `resolve_index_for_search`:

| Case | Behaviour | `index.action` |
| --- | --- | --- |
| No snapshot | Index now, or wait for the job that holds the write lock; a failed automatic flight shares its error | `created` / `waited_for_existing_job` |
| `on-search` (default) | `refresh_stale_snapshot`: re-fingerprints at most every `stale_check_interval_seconds` (10); if stale, tries the write lock and refreshes incrementally; otherwise waits up to `wait_for_existing_job_seconds` (120) and re-checks. A failure keeps the old snapshot and warns | `reused` / `refreshed_incrementally` / `waited_for_existing_job` |
| `on-search`, vectorless snapshot (`vectors_missing`) | A third refresh trigger in `refresh_stale_snapshot`, gated by the same throttle (the fingerprint walk still runs) and fallback; skipped without re-indexing while the embedder connect breaker is open and for `EMBEDDER_DOWN_WINDOW` after a repair that ended still vectorless or failed (`WorkspaceCoordination.last_failed_repair`). A repair that finds the service still down keeps the vectorless snapshot; the search then warns that the index has no vectors | `refreshed_incrementally` / `reused` |
| `manual`, `watch` | Never refresh from a search, a vectorless snapshot included (filled by `index_workspace` or the watcher) | `reused` |
| Embedding identity changed, `on-search` | Treated as stale even when the check is throttled; re-embeds through `index_locked` and searches the new snapshot. If recovery fails, or the wait for another job times out, the search fails with a hard error (`stale_refresh_fallback`) | `refreshed_incrementally` / `waited_for_existing_job` / error |
| Embedding identity changed, `manual` or `watch` | Rejected: "embedding configuration changed; reindex workspace" | error |

In no mode are old vectors a fallback after an identity change. `watch_workspace` runs a polling task: it
re-fingerprints every `watch_poll_milliseconds` (2000), debounces (`watch_debounce_milliseconds`, 750) and calls
`App::index`; watch registrations die with the process. `index_status` re-fingerprints on every call.

## Data flow: search

`search_in_profile` (MCP and REST) and `search_with_options` (evaluate) go through `search_configured` (appends the
candidate dump when `LCI_CANDIDATE_DUMP` is set) to `search_once`. A `[search_profiles.<name>]` overrides only the channel
switches and `rerank_candidate_count`. Each optional channel fails independently and reports its fallback state.

| # | Step | Details |
| --- | --- | --- |
| 1 | Validate request | Query non-empty and at most 16000 bytes; `top_k` 1..=40 (default `default_top_k`, 8) |
| 2 | Validate filters | `FilterRequest` becomes `EffectiveFilter`: registered languages, include/exclude globs, source roles |
| 3 | Source roles | Classified from the path only; precedence generated > test > benchmark > example > documentation > configuration > source (7 roles) |
| 4 | Resolve index | Freshness policy above; the snapshot is pinned from here on |
| 5 | Filtered universe | `Store::chunks` then `hit_matches`; every chunk including its code is loaded on each search |
| 6 | Pick LSP adapters | Enabled adapters for the languages present in the universe |
| 7 | Fan out in parallel | `tokio::join!` of query embedding, `rg`, and LSP providers under the budget. A channel switched off in `[channels]` does no work and raises no warning |
| 8 | Candidates | Semantic: `Store::search(limit = semantic_candidate_count = 40)`; lexical: mapped onto the universe, top 40; LSP: mapped onto the universe, top 40. Errors become warnings and empty lists; budget overruns become notes |
| 9 | All channels empty | Empty report when filters were requested, or when semantic is off and a channel ran cleanly; otherwise fails with "every enabled retrieval channel failed or returned no candidates" |
| 10 | Weighted RRF | `fuse_hits`: each channel adds `weight / (rrf_k + rank)`, `rrf_k = 60`, weights all 1.0 (`FusionConfig`); summed in the fixed f32 order semantic, lexical, lsp |
| 11 | Source-role prior | Adds 0.004 for `source`, 0 for every other role, unweighted (a rank-1 RRF term is 1/61, about 0.0164) |
| 12 | Post-fusion filter and sort | Re-applies `hit_matches`, sorts by `fusion_order` |
| 13 | Optional trace | `CandidateTrace` rows record the whole pool before the cap |
| 14 | Per-file cap | `max_chunks_per_file = 2`: extra chunks of a file are demoted behind the others, never removed |
| 15 | Shortlist cut | Truncates to `rerank_candidate_count` (24); a note is added when a `top_k` the caller passed is larger than the window |
| 16 | Rerank (fail-open) | Document = path, newline, at most `rerank_document_max_bytes` (4096) of code. On error results stay in fusion order and a warning is appended; with the reranker switched off, fusion order with no warning. With `rescorer = "system_one"` the same documents (cut to 1500 characters, with path and start line; the candidate trace sizes that cut document) go to the typed `[system_one]` service in sequential batches (`batch_size` per request; `timeout_ms` applies per batch request, so when the batch size is smaller than the shortlist, the worst case before fail-open is (number of batches) x `timeout_ms`), one `noul` question per candidate, and the probabilities are the scores; a failed, malformed or partial answer is the same fail-open fallback (no reranker request) |
| 17 | Final filter, response | `hit_matches` once more; `SearchReport`: `results`, `warning` (counts as a fallback), `notes`, `timings` (7 fields: query embedding, LanceDB, lexical, LSP, fusion, reranking, total), `index {action, wait_ms}`, `filters`, `candidate_count`, `reranked`, optionally `candidates` and `rerank_outcome` |

The f32 summation order is part of the candidate-dump contract, and `replay` checks bit-exact equality. The compact
view (`compact_view`, a profile with `response = "compact"`) is applied to a finished report: presentation only.

## Data flow: traverse

`search_code` with `mode = "traverse"` (needs `[traverse] enabled`) and `lci traverse` run one function,
`app::traverse::run`. It loads the SCIP index (map `graph.scip`, cached by file stamp; `[run_profile.scip] mode` `authoritative` requires it, `cache` reads it when it is there, `off` never touches it and leaves the graph and the maps built from the index file empty, each saying so in a note), finds the start nodes
(`given`; `hybrid`: the top `seed_k` hits of the normal hybrid search mapped to SCIP symbols; `hybrid+bm25` or `bm25` with `[run_profile.seeds] bm25 = true`: the opt-in map `seeds.bm25`,
BM25 over the SCIP graph's node names, qualified paths and documentation, whose top `seed_k` nodes go after the hybrid hits or stand alone
when the search gave none; or `name_tokens`, the visible fallback), builds the steering ladder from the caller's rules, runs the `lci-maps` traversal under the
`[traverse]` caps (a request can only lower them), writes a trace under `data_dir`, and answers with the reached nodes,
totals and, only when work is left, a `handle` and `checkpoint`. The first stage (exact operators `exact.closure` and
`exact.pickaxe`, then seeds), named policies per question type and the steering ladder are described in
[engine.md](spike/multimap/engine.md). Default off: with `[traverse]`, `[recipes]` and `[compose]` disabled the tool
list, schemas and REST routes are exactly those of a server without them.

With `[run_profile.traverse] card_paths = true` (default `false`) every walked card of `results` and of `checkpoint.answer` also has `kind` and `path`, the
route from its start node with each hop's map, edge kind and witness (`app::traverse::response::add_paths`, read from the walk's state table; the engine keeps
`State::kind`, and a handle keeps it only under the key). With the key off the answer, the trace, the checkpoint and the handle are unchanged.

With `[run_profile.traverse] map_freshness = true` (default `false`) the answer also has `maps`: for each map of the call that
states a freshness, `{id, observed, stale, lag}` (`app::traverse::freshness`). The SCIP-built maps report `snapshot`;
`grep.live`, `lsp.live`, `git.history`, `exact.pickaxe` and `probe.compile` (which runs only under `rust_analyzer_trusted = true`) report `live` with `lag` 0 (they read current
bytes or git, placed by the index's positions for the first two). `lag` is the number of source files (language-adapter
files, prose left out) newer than the index file plus those changed (content hash), added or removed since the process
first saw the index's current stamp (modified time and length), each counted once; `stale` means `lag` above 0 and `null`
means not compared (no tree, unreadable tree, or an index file without a modified time). The tree is read and hashed
once per call with `chunk::source_hashes` (the index scan's walk and its read, skip and hash function, no chunk made);
`[run_profile.freshness] stat_gate = true` (default `false`, in the profile hash) puts a process-wide `chunk::StatGate` per
root in front of the read, which keeps `(modified time, length, hash)` per file and reads only files whose stamp is new or
changed. A file modified within 3 seconds of the read is not trusted (racy stamp; FAT, exFAT and SMB stamps are coarse) and is read again next call. Its gap, asserted by a test: a same-length edit that keeps a modified time older than 3 seconds before the previous read is not seen until the next edit.
Search's `verify_on_read` reads through the same function. `Expanded` carries an optional
`edge::Freshness` that a wrapper around each stating map records; the walk itself does not read it. Key off: no
wrapper, no tree read, no `maps` key.

`[run_profile.freshness] verify_on_read = true` (default `false`, in the profile hash) adds, on top of `map_freshness`, a check of
what the answer cites against the tree by content hash. It reuses the one read of the tree (`freshness::tree_view` returns the
lag with the file sets it came from; no second scan), reads and labels, and never refreshes the index or touches
`[index].freshness`. `SeedSearch::snapshot_hashes` (`AppSeeds` reads the manifest of the index snapshot) is read after the walk,
because the walk's first search may refresh the snapshot; the snapshot lag is the source files (the `tree_lag` set; manifest prose
entries are left out) that differ between the manifest and the tree. `seeds.hybrid`, `search.lexical` and `search.vector` are
then labelled with it (`Observer` does not wrap them; the label is filled at report time), `grep.live` and `lsp.live` say
`observed: live` with `places: "index"` and the SCIP lag (their places are the index's), and `response::add_file_changed` gives
every card `file_changed` from the baseline of its maps (the SCIP baseline for the maps built from the index and the placed
maps and the symbol cards, the manifest for the snapshot maps and the chunk cards; `null` where none applies). Key off: none of this, and no change to any answer.
`search::App::verify_on_read` does the same for a search report the caller asked for: `SearchReport.freshness` (`snapshot`, `channels` for the channels that ran, `hits` with `file_changed` per result) from the manifest of the snapshot the search ran on and one read of the tree's content hashes (`chunk::source_hashes`, gated by `stat_gate` when on), read after the search (a refresh in between is not seen) and timed in `timings.verify_ms`. The searches inside the crate that discard the block (`AppSeeds::search`, the evaluation runner) call the `_unverified` entry points, so a traversal reads the tree once.

## MCP, REST and CLI surfaces

- **MCP** (`src/server.rs`, `#[tool_router] impl McpServer`) is mounted at `/mcp` and at `/mcp/<profile>` for each search
  profile; `/mcp/<unknown>` is 404; each endpoint has its own session manager (keep-alive 1 h) and the same instructions.
  Tools are stateless request/response (`App` keeps no per-MCP-session state; traversal continuation uses a `handle`, a
  content hash of the snapshot kept as a file under the data directory, bound to the workspace and index).
  A profile may set `tools = "search"` (only `search_code`, `search_symbols`, `find_definition`, `find_references`) and
  `response = "compact"`. 16 tools: 10 always (`index_workspace`, `index_status`, `list_indexed_files`,
  `watch_workspace`, `unwatch_workspace`, `search_symbols`, `find_definition`, `find_references`, `search_code`,
  `service_status`), 5 with `[recipes] enabled` (`config_readers`, `impact_rings`, `flow_path`, `flow_from`,
  `config_neighbours`) and `lci_compose` with `[compose] enabled`. `search_code` takes `workspace_path`, `query`,
  `top_k`, `languages`, `include_paths`, `exclude_paths`, `source_roles` and, with `[traverse] enabled`, `mode`,
  `budget`, `steer`, `handle`. An argument a tool does not have is refused with a tool error naming it (REST JSON bodies: HTTP 422); `service_status` uses a strict empty argument object. The REST search query string uses a separate strict `SearchQuery` (HTTP 400 for unknown keys). Parameter semantics: README.
- **REST** (`src/rest.rs`) exposes each tool as `POST /v1/<tool>` (`GET /v1/service_status`), `search_code` takes
  `?search_profile=`, plus `POST /v1/release_workspace` (no MCP tool: releases the workspace's language servers),
  `GET /openapi.json` (hand-assembled), `/health`, `/ready`. REST, `/openapi.json`, `/health` and `/ready` check Host and
  Origin against loopback (`loopback_guard`); rmcp validates `/mcp` itself.
- **CLI** (`src/main.rs`): `serve`, `config`, `init`, `index`, `status`, `list-files`, `search`, `evaluate`, `compare`,
  `replay`, `traverse`, `recipe`, `symbols`, `definition`, `references`. Language servers are released on Ctrl+C or
  SIGTERM.

## Evaluation harness

- Definition: `EvaluationFile` (`lci-core` `evaluation`) holds `workspaces` (`key`, `required`) and `queries`; each
  `EvalQuery` has `query`, `workspace`, `top_k`, `filters`, `expected_paths`, `preferred_paths`, `disfavored_paths`,
  `expected_roles`, `required_snippet`, `tags`. Gold is path globs: no line ranges, no multi-step answers.
- Sets: `evaluations/core.toml` (14 queries, hand-written; workspaces `self` with 11 queries and optional `gust` with
  3) and `evaluations/mined/self.toml` (35 queries, generated, not for tuning ranking).
- Miner (`xtask/src/mine.rs`, `cargo xtask mine-queries`): one query per commit (subject plus first body paragraph);
  gold is the supported-extension files the commit touched that still exist at HEAD, renames followed; skips merges,
  reverts, noise and subjects under 4 words, and commits touching more than 10 files (`MAX_TOUCHED_FILES`). Tags
  `keyword`/`keyword-agnostic`, `gold-drift`, `short-query`/`long-query`.
- Runner (`src/evaluate/runner.rs`): calls `search_with_options`, with `trace_candidates` for `--dump-candidates`; keeps
  bounded evidence (`bounded_snippet`, 400 bytes). Metrics (`metrics.rs`): hit@1, hit@3, hit@8-or-window, reciprocal
  rank, graded nDCG@10 (gain 2 expected, 1 preferred), recall@3/@8, file accuracy @5/@10, role distribution, reranker
  state, aggregates by tag, per-stage latency percentiles.
- Compare (`compare.rs`): pairs queries by `(workspace, query text)`; mean paired delta of reciprocal rank, nDCG@10 and
  hit@1 with a seeded paired-bootstrap 95% CI (`DEFAULT_SEED = 0x4C434933`, `DEFAULT_RESAMPLES = 10000`) and W/L/T
  counts. Replay (`replay.rs`): offline; re-runs fusion, cap, window and rerank (`replace`/`vote`/`off`) from dumped
  ranks and scores after verifying the dump bit for bit; the fusion formula is hard-coded to three channels, so it
  cannot replay a deeper channel, other chunking or another embedding. RunRecord reading: `src/evaluate/run_record.rs`.
  Measurement method for the traversal work: [evaluation.md](spike/multimap/evaluation.md).

## Storage and freshness costs

Every commit overwrites the whole workspace table; every search loads every chunk row including code; there is no
vector index, scalar index or full-text index and no `optimize` or `cleanup_old_versions` call. `on-search` re-reads and
SHA-256-hashes every supported file at most every 10 s; `watch` polls every 2 s doing the same; `map_lsp_locations`
scans chunks linearly.

## Control-plane exclusion

Repository instructions and skill files are agent control-plane material. The checked-in `.lciignore` keeps `.agents/**`,
`.claude/**`, `.codex/**`, `.kilo/**`, `.github/{agents,instructions,skills}/**` and `test-results/**` out of this
repository's searchable code corpus while leaving them available to agents. Indexed content is data, never instructions.
