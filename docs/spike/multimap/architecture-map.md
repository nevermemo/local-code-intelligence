# Retrieval architecture map

Generated: 2026-10-12T04:07Z
Base commit: 7f9c636 (`origin/fix/verified-bugs`; the spike branch adds only docs)

This file describes the retrieval pipeline as it exists today. It is groundwork for the
modular multimap traversal spike. All `path:line` references point at definition lines in
the source tree at that commit.

## 1. Crate layout

| Crate | Path | Holds | Must not depend on |
| --- | --- | --- | --- |
| `local-code-intelligence` (root) | `src/` | `app` (indexing, search, navigation, readiness), `store` (the only LanceDB/Arrow code), `evaluate`, `server` (MCP), `rest`, `main` (CLI). Re-exports the `lci-core` modules from `src/lib.rs`. | none |
| `lci-core` | `lci-core/src/` | Pure logic: `chunk`, `config`, `filter`, `language`, `lexical`, `lsp`, `manifest`, `models`, `workspace`, plus the Tree-sitter grammars | LanceDB, Arrow, axum, rmcp |
| `xtask` | `xtask/src/` | `cargo xtask` build/test/lint/env/acceptance and `mine-queries` (`xtask/src/mine.rs`) | `lci-core` (on purpose: it would compile every grammar, see `xtask/src/mine.rs:45-52`) |

The crates are split for build time (`docs/architecture.md:8-22`). That split constrains the
design: a generic `Map` trait can live in `lci-core`, but any map stored in LanceDB has to be
implemented in the root crate.

## 2. Code units: chunks

| Item | Location | Notes |
| --- | --- | --- |
| `Chunk` struct | `lci-core/src/chunk.rs:43` | Fields: `relative_file_path`, `language`, `start_line`, `end_line` (both 1-based, inclusive), `code` (trimmed), `content_hash` (SHA-256 hex of `code`, from `workspace::hash` at `lci-core/src/workspace.rs:6`). |
| Size limits | `lci-core/src/chunk.rs:10-11` | `TARGET_BYTES = 6000`, `MAX_DECLARATION_BYTES = 24000`. |
| Format version | `lci-core/src/chunk.rs:12` | `DOCUMENT_FORMAT_VERSION = "rust-chunks-v1"`. It is part of `embedding_identity`. |
| Recursive splitter | `lci-core/src/chunk.rs:54` (`ranges`) | Splits at container, boundary and declaration node kinds. Prefixes such as doc comments and attributes stay attached to the next child. A large leaf is never cut into text windows. |
| Per-file chunking | `lci-core/src/chunk.rs:144` (`chunks_with_adapter`) | Skips chunks that are empty or contain only `{};`. |
| `Scan` result | `lci-core/src/chunk.rs:15` | `chunks`, the per-file `files` map (becomes the manifest), `fingerprint`, and the counts `parsed`, `unchanged`, `removed`, `lossy` and `skipped`. |
| Walker | `lci-core/src/chunk.rs:204` (`sources`) | Uses `ignore::WalkBuilder` with `.hidden(false)`, `.follow_links(false)`, `.require_git(false)` and `.add_custom_ignore_filename(".lciignore")`, and skips `.git` (`:207-213`). Only files with a registered extension are read (`:219`). Non-UTF-8 files are decoded lossily (`:192`). Qt Linguist `.ts` files are skipped (`:229`). |
| Incremental scan | `lci-core/src/chunk.rs:270` (`scan_incremental`) | Reuses a file's cached chunks when `content_hash`, `language` and `adapter_version` all match (`:300-308`). Otherwise the file is re-parsed. Output is sorted by `(path, start_line)`. |
| Staleness fingerprint | `lci-core/src/chunk.rs:251`, `:261` | SHA-256 over `path\0content_hash\0language\0cache_version` for every source file. Skipped files are left out. |
| Language registry | `lci-core/src/language.rs:468` (`ADAPTERS`) | 12 extensions, 11 identifiers (`hpp` maps to `cpp`). Each entry has its own `cache_version` (for example `rust-chunks-v2`). `LanguageAdapter` is at `:39`. Lookup goes through `for_extension` (`:555`) and `for_path` (`:561`). |
| Checked-in `.lciignore` | `.lciignore` | Excludes `.agents/**`, `.claude/**`, `.github/{agents,instructions,skills}/**`, `.codex/**`, `.kilo/**` and `test-results/**`. |

**Chunk identity.** No chunk-id column or field exists. The code uses three keys:

- Fusion keys a chunk by `(relative_file_path, start_line, content_hash)` (`src/app/search.rs:1304`).
- The vector cache keys a chunk by `content_hash`, or by `content_hash` plus the path when the
  document template contains `{path}` (`src/store.rs:93`).
- The fused-order tiebreak adds `end_line` (`src/app/search.rs:337`).

Two of these keys include `start_line`. An edit above a chunk therefore changes its fusion
key even though its content hash stays the same.

**Only code is a unit today.** Markdown, TOML, YAML, JSON, build files and agent instruction
files have no chunks, because the walker drops them at `chunk.rs:219`.

## 3. Manifest

| Item | Location | Notes |
| --- | --- | --- |
| `Manifest` | `lci-core/src/manifest.rs:20` | `version` (`VERSION = 2`, `:9`), `workspace_id`, `workspace_path`, `fingerprint`, and `files: HashMap<path, CachedFile>`. |
| `CachedFile` | `lci-core/src/manifest.rs:12` | `language`, `adapter_version`, `content_hash`, and the file's full `Vec<Chunk>` including code. |
| Compatibility check | `lci-core/src/manifest.rs:39` | Checks version, workspace id and path, and that every file's language matches its adapter and every chunk's hash matches its code. An incompatible or unparseable manifest loads as empty, which forces a full re-parse (`:128`). |
| v1 migration | `lci-core/src/manifest.rs:57`, `:73` | `ManifestV1::migrate` is the precedent for any future version bump. |
| Storage | `lci-core/src/manifest.rs:117`, `:164` | `<data_dir>/manifests/<workspace_id>.json`, written to a `.tmp` file and then renamed. |

**How a refresh finds changed files.** It does not use mtimes. Every refresh re-reads and
re-hashes every source file (`chunk.rs:204`, `:237`). Per file, the cache decision is
`chunk.rs:300-308`. Workspace-level staleness compares the fresh `chunk::fingerprint` with
`Manifest.fingerprint` (`src/app/indexing.rs:342-345`).

## 4. Indexes and channels

### 4.1 LanceDB vector store (`src/store.rs`)

| Item | Location | Notes |
| --- | --- | --- |
| `Store` | `src/store.rs:22` | Opened at `<data_dir>/lancedb` (`src/app/mod.rs:62`). |
| Table name | `lci-core/src/workspace.rs:40` | `ws_<first 16 hex of the workspace id>`. The id is SHA-256 of the canonical path (`workspace.rs:17`). A legacy name `ws_<64 hex>` is still read (`:48`) and dropped at the next commit (`store.rs:318`). |
| Schema | `src/store.rs:236-255` | The Utf8 columns are `workspace_id`, `relative_file_path`, `language`, `code` and `content_hash`. The UInt32 columns are `start_line` and `end_line`. `vector` is `FixedSizeList<Float32, dim>`. |
| Schema metadata | `src/store.rs:256-269` | `embedding_identity`, `workspace_path` (used to detect name collisions, `:145`) and `indexed_at` (unix seconds). |
| Commit | `src/store.rs:220` (`replace`) | One table per workspace, rewritten whole with `CreateTableMode::Overwrite` on every index pass. Unchanged chunks reuse their cached vectors, but every row is written again. |
| Vector search | `src/store.rs:331` (`search`) | Cosine distance. Over-samples `min(4 * limit, 160)` rows, then applies `EffectiveFilter::matches` row by row (`:364`). The score is `1 - distance`. |
| Full scan | `src/store.rs:400` (`chunks`) | Reads every row except `vector`. Every search calls it (`search.rs:689`). |
| Embedded text | `src/app/indexing.rs:56` (`document_text`) | Fills `embedding_document_template` (default `{code}`, `lci-core/src/config.rs:244`) with `{code}` and `{path}`. A non-default template changes `embedding_identity` (`config.rs:921`). |
| Query text | `lci-core/src/models.rs:393` | Fills `embedding_query_template` (instruction prefix plus `{query}`, `config.rs:241`). |
| Hit row | `src/store.rs:27` (`Hit`) | `Chunk` (flattened) plus `file_path`, `semantic_score`/`semantic_rank`, `lexical_rank`/`lexical_score`/`lexical_match_count`, `lsp_rank`, `fusion_score`, `retrieval_channels`, `reranker_score`, `source_role` and `source_role_prior`. This is the serialized result shape. |

### 4.2 Lexical channel (`lci-core/src/lexical.rs`)

There is no persisted lexical index. Each query runs a live `rg` over the working tree:

- `search` (`lci-core/src/lexical.rs:512`) runs `rg --json --line-number --ignore-case
  --fixed-strings` with one `--glob` per adapter and one `-e` per term (`:519-530`).
- Terms come from `query_terms` (`:237`): at most `MAX_QUERY_TERMS = 24` (`:15`), with stop
  words removed and identifiers split.
- Each line is credited through a `TermMatcher` term bitmask (`:294`).
- Lines are mapped to chunks in `App::map_lexical_matches` (`src/app/search.rs:1139`), and
  chunks are scored with BM25 by `score_chunks` (`lexical.rs:694`). The parameters are
  `BM25_K1 = 1.2` and `BM25_B = 0.75`. Document frequency is counted over indexed files only.
  Terms that appear in more than 25% of files are dropped (`:26`), and at most 12 terms score
  (`:29`).
- Results are capped at `lexical_candidate_count` (40).

Ripgrep and the scanner see different file sets:

- `rg` is not given `--hidden` or `.lciignore`, so it skips hidden files that the scanner
  indexes. Files ignored by `.lciignore` drop out anyway, because only lines inside indexed
  chunks count.
- `rg` reads the live tree while the chunks come from the snapshot. Under `manual` or `watch`
  freshness, line ranges can drift.

Aligning the `rg` flags is planned in `docs/development/roadmap.md:686`.

### 4.3 LSP channel and navigation

| Item | Location | Notes |
| --- | --- | --- |
| `LspAdapter` trait | `lci-core/src/lsp/adapter.rs:14` | `provider`, `languages`, `enabled`, `transport_config`, `initialize_params`, readiness hooks. A registry of trait objects, and the closest existing model for a `Map` trait. |
| `Manager` | `lci-core/src/lsp.rs:540` | Holds 7 adapters (rust-analyzer, csharp-ls, typescript-language-server, pyright, gopls, jdtls, clangd), built in `:552`. Sessions are keyed by `(provider, workspace_id)` (`:111`). Has idle sweep and stuck-session replacement. |
| Requests | `lci-core/src/lsp.rs:875`, `:893`, `:922` | `symbols` (`workspace/symbol`), `definition`, `references`. All return `Location` (`:47`): path, range, name, kind, language and provider. |
| Search use | `src/app/search.rs:730-800` | Up to 8 symbol queries, each a term plus `#` (with a singular form added for plurals). Providers run in parallel, each under `lsp_search_budget_ms` (1500). A provider that runs past the budget produces a note. A provider that fails produces a warning. Locations map onto chunks with the same language that contain `start_line` (`search.rs:1259`). Capped at `lsp_candidate_count` (40). |
| Navigation tools | `src/app/navigation.rs:76`, `:213`, `:243` | `symbols`, `definition` and `references`. These are not fused and return raw `Location`s. They require an index containing the file's language (`:196`). |

### 4.4 Model clients (`lci-core/src/models.rs`)

`Models` (`:19`) talks to two services:

- **Embeddings.** `embed` (`:318`) POSTs to `<embedding_url>/embeddings`, OpenAI-compatible,
  and validates the response: count, index, finiteness, non-zero vectors and one dimension.
  Indexing retries up to 3 attempts (`:64`). `query` (`:399`) makes one attempt.
- **Reranking.** `rerank` (`:411`) makes one attempt and returns `(index, score)` pairs best
  first. After a refused connection, a circuit breaker skips the reranker for 30 s (`:88`).

Indexing isolates batches the service rejects as oversized. It retries the inputs one at a
time with prefixes halved down to 512 bytes (`src/app/indexing.rs:75`).

## 5. Indexing flow and freshness

`App::index` (`src/app/indexing.rs:185`) takes the workspace write lock and then calls
`index_locked` (`:196`):

1. Take the process-wide `index_lock` (`src/app/mod.rs:49`) and the cross-process OS lock on
   `<data_dir>/index.lock` (`mod.rs:113`).
2. `manifest::try_load`, then `chunk::scan_incremental` (in `spawn_blocking`).
3. Open the previous snapshot. If its identity matches, load `cached_vectors`. Embed only the
   missing keys (deduplicated), in batches of `embedding_batch_size`.
4. `Store::replace` writes the whole table. Then `manifest::save`.
5. Return an `IndexReport` (`:23`) with counts, lossy, truncated and skipped files, and timing.

Locks:

- `WorkspaceCoordination` (`src/app/mod.rs:31`) holds a per-workspace `RwLock`, the
  `flight_error` shared by concurrent first-index waiters, and the `last_stale_check`
  throttle.
- A search holds the read guard for its whole run (`IndexResolution._read_guard`,
  `src/app/search.rs:521`). This keeps the snapshot from being replaced mid-search.

Freshness (`IndexFreshness`, `lci-core/src/config.rs:19`; resolved in
`resolve_index_for_search`, `src/app/search.rs:1022`):

| Case | Behaviour | `index.action` |
| --- | --- | --- |
| No snapshot | Index now, or wait for the job that holds the lock. A failed automatic flight shares its error (`search.rs:1060`). | `created` / `waited_for_existing_job` |
| `on-search` (default) | `refresh_stale_snapshot` (`indexing.rs:306`). Re-fingerprints at most every `stale_check_interval_seconds` (10). If stale, tries the write lock and refreshes incrementally. Otherwise waits up to `wait_for_existing_job_seconds` (120) and re-checks. A failure keeps the old snapshot and warns (`:402`). | `reused` / `refreshed_incrementally` / `waited_for_existing_job` |
| `manual`, `watch` | Never refresh from a search. | `reused` |
| Embedding identity changed | Hard error in every mode. Old vectors are never served (`search.rs:1105`, `indexing.rs:406`). | (error) |

`watch` (`indexing.rs:530`) runs a polling task. It re-fingerprints every
`watch_poll_milliseconds`, debounces, and calls `App::index`.

## 6. Search pipeline

Entry points: `search_in_profile` (`src/app/search.rs:583`, used by MCP and REST) and
`search_with_options` (`:560`, used by evaluate). Both go through `search_configured` (`:608`,
which appends the candidate dump when `LCI_CANDIDATE_DUMP` is set) to `search_once` (`:637`).
A `[search_profiles.<name>]` (`lci-core/src/config.rs:107`) overrides only the channel
switches and `rerank_candidate_count` (`config.rs:880`).

| # | Step | Location | Details |
| --- | --- | --- | --- |
| 1 | Validate request | `search.rs:647-650` | Query must be non-empty and at most 16000 bytes. `top_k` is 1..=40 (default 8). |
| 2 | Validate filters | `search.rs:655`; `lci-core/src/filter/request.rs:224` | `FilterRequest` (`request.rs:120`) becomes `EffectiveFilter` (`:140`): registered languages, include/exclude globs, source roles. `matches` is at `:284`. |
| 3 | Source roles | `lci-core/src/filter/source_role.rs:105` | Classified from the path only. Precedence: generated > test > benchmark > example > documentation > configuration > source. 7 roles (`:10`). |
| 4 | Resolve index | `search.rs:656-664`, `:1022` | Freshness policy as in section 5. The read guard is held from here on. |
| 5 | Build the filtered unit universe | `search.rs:689-690` | `Store::chunks` then `hit_matches`. Every chunk, including its code, is loaded on each search. |
| 6 | Pick LSP adapters | `search.rs:695-710` | Enabled adapters for the languages present in the universe. |
| 7 | Fan out in parallel | `search.rs:712` (`tokio::join!`) | Semantic: query embedding. Lexical: `rg`. LSP: providers under the budget. A channel switched off in `[channels]` (`config.rs:68`) does no work and raises no warning. |
| 8a | Semantic candidates | `search.rs:836-881` | `Store::search(limit = semantic_candidate_count = 40)`, filtered inside. An embedding or search error becomes a warning and an empty list. |
| 8b | Lexical candidates | `search.rs:882-904`, `:1139` | Mapped onto the filtered universe, top 40. An error becomes a warning. |
| 8c | LSP candidates | `search.rs:905-906`, `:1259` | Mapped onto the filtered universe, top 40. Failures become warnings, budget overruns become notes (`:813-835`). |
| 9 | All channels empty | `search.rs:907-931` | Returns an empty report when filters were requested, or when semantic is off and a channel ran cleanly. Otherwise fails with "every enabled retrieval channel failed". |
| 10 | Weighted RRF | `search.rs:1297` (`fuse_hits`) | Each channel adds `weight / (rrf_k + rank)`. `rrf_k = 60` (`config.rs:645`). Weights are all 1.0 (`FusionConfig`, `config.rs:172`). Summed in the fixed f32 order semantic, lexical, lsp. |
| 11 | Source-role prior | `search.rs:1355-1357`; `src/store.rs:48` | Adds 0.004 for `source` and 0 for everything else, unweighted. For scale, a rank-1 RRF term is 1/61 ≈ 0.0164. |
| 12 | Post-fusion filter and sort | `search.rs:940-941`, `:337` | Re-applies `hit_matches`, then sorts by `fusion_order` (score, path, start, end, hash). |
| 13 | Optional trace | `search.rs:951-961`, `:352` | `CandidateTrace` rows (`:256`) record the whole pool before the cap. |
| 14 | Per-file cap | `search.rs:962`, `:476` | `max_chunks_per_file = 2` (`config.rs:218`). Extra chunks are demoted, never removed. |
| 15 | Shortlist cut | `search.rs:963-971` | Truncates to `rerank_candidate_count` (24). Adds a note when `top_k` is larger than the window (`:462`). |
| 16 | Rerank (fail-open) | `search.rs:974-996`, `:1366` | Each document is the path, a newline, then at most `rerank_document_max_bytes` (4096) of code (`:329`). On error, results stay in fusion order and a warning is appended. With the reranker switched off, results are in fusion order with no warning. |
| 17 | Final filter | `search.rs:998` | Applies `hit_matches` once more. |
| 18 | Response | `search.rs:46` (`SearchReport`) | Fields: `results: Vec<Hit>` (per-channel ranks and scores, `fusion_score`, `reranker_score`, `retrieval_channels`), `warning` (counts as a fallback), `notes`, `timings` (`:29`, a fixed set of 7 fields), `index {action, wait_ms}` (`IndexAction`, `src/app/indexing.rs:175`), `filters`, `candidate_count`, `reranked`, and optionally `candidates`/`rerank_outcome`. |

## 7. MCP tools and REST

MCP is defined by `#[tool_router] impl McpServer` (`src/server.rs:89`). It is mounted at
`/mcp` and at `/mcp/<profile>` for each search profile (`server.rs:213`, `:293-305`). The
tool list is the same on every endpoint.

| Tool | Handler | Parameters | App method |
| --- | --- | --- | --- |
| `index_workspace` | `src/server.rs:94` | `workspace_path` (`WorkspaceArgs`, `:22`) | `App::index` (`indexing.rs:185`) |
| `index_status` | `src/server.rs:100` | `workspace_path` | `App::status` (`indexing.rs:421`) |
| `list_indexed_files` | `src/server.rs:106` | `workspace_path` | `App::indexed_files` (`indexing.rs:510`) |
| `watch_workspace` | `src/server.rs:119` | `workspace_path` | `App::watch` (`indexing.rs:530`) |
| `unwatch_workspace` | `src/server.rs:123` | `workspace_path` | `App::unwatch` (`indexing.rs:595`) |
| `search_symbols` | `src/server.rs:132` | `workspace_path`, `query` (`SymbolArgs`, `:45`) | `App::symbols` (`navigation.rs:76`) |
| `find_definition` | `src/server.rs:142` | `workspace_path`, `relative_file_path`, 1-based `line`, UTF-16 `character` (`PositionArgs`, `:50`) | `App::definition` (`navigation.rs:213`) |
| `find_references` | `src/server.rs:157` | The same as `find_definition`, plus `include_declaration` (`ReferencesArgs`, `:61`) | `App::references` (`navigation.rs:243`) |
| `search_code` | `src/server.rs:176` | `workspace_path`, `query`, `top_k`, `languages`, `include_paths`, `exclude_paths`, `source_roles` (`SearchArgs`, `:27`) | `App::search_in_profile` (`search.rs:583`) |
| `service_status` | `src/server.rs:197` | none | `App::service_status` (`src/app/readiness.rs:98`) |

REST (`src/rest.rs:262`) exposes each tool as `POST /v1/<tool>`, except `GET
/v1/service_status`. It has one extra route, `POST /v1/release_workspace` (`rest.rs:71`,
calling `App::release_language_servers` at `navigation.rs:54`), which MCP does not offer.
`search_code` takes `?search_profile=` (`rest.rs:126`). `GET /openapi.json` is hand-assembled
(`rest.rs:171`). `/health` and `/ready` are in `server.rs:265-282`. The CLI subcommands
(`src/main.rs:46`) mirror the tools and add `evaluate` (`:119`), `compare` (`:146`) and
`replay` (`:168`). Every tool is stateless request/response: `App` keeps no per-MCP-session
state.

## 8. Evaluation harness

| Part | Location | Notes |
| --- | --- | --- |
| Definition | `src/evaluate/types.rs:13`, `:40` | `EvaluationFile` holds `workspaces` (`key`, `required`) and `queries`. Each `EvalQuery` has: `query`, `workspace`, `top_k`, `filters`, `expected_paths`, `preferred_paths`, `disfavored_paths`, `expected_roles`, `required_snippet` and `tags`. Gold is path globs; there are no line ranges and no multi-step answers. |
| Sets | `evaluations/core.toml` (14 queries, hand-written; workspaces `self` and optional `gust`), `evaluations/mined/self.toml` (35 queries, generated) | The mined file says it must not be used to tune ranking. |
| Miner | `xtask/src/mine.rs:949` (`run`), `:813` (`mine`), `:705` (`mine_commit`) | One query per commit: the subject plus the first body paragraph. Gold is the supported-extension files the commit touched that still exist at HEAD, with renames followed (`:309`, `:334`). Skips merges, reverts, noise and subjects under 4 words. Skips commits touching more than 10 files (`:56`). Tags: `keyword`/`keyword-agnostic`, `gold-drift`, `short-query`/`long-query`. `top_k = 10`. |
| Runner | `src/evaluate/runner.rs:101` | Calls `search_with_options` with `trace_candidates` when `--dump-candidates` is set. Keeps bounded evidence (`build_evidence`, `:55`), plus the `CandidateArm` (`types.rs:203`) and the trace. |
| Metrics | `src/evaluate/metrics.rs:50` (`query_metrics`), `:397` (`aggregate`) | hit@1, hit@3, hit@8-or-window, reciprocal rank, graded nDCG@10 (gain 2 for expected, 1 for preferred, `:302`), recall@3/@8 (`:343`), file accuracy @5/@10 (`:363`), role distribution, reranker state. Aggregates by tag and per-stage latency percentiles. `QueryMetrics` is at `types.rs:283`. |
| Compare | `src/evaluate/compare.rs:221` | Pairs queries by `(workspace, query text)` (`:295`). Reports the mean paired delta of reciprocal rank, nDCG@10 and hit@1 with a seeded paired bootstrap 95% CI (`:382`; seed `0x4C434933`, 10,000 resamples) and W/L/T counts. |
| Replay | `src/evaluate/replay.rs:1062`, `:498` | Offline. Re-runs fusion, cap, window and rerank (`replace`/`vote`/`off`) from the dumped ranks and reranker scores. First verifies the dump bit for bit (`:406`) and refuses a query that does not match. The fusion formula is hard-coded to three channels (`:307`). It cannot replay a deeper channel, a different chunking or a different embedding. |

## 9. Seams for maps and step-wise traversal

### 9.0 Invariants any design must keep

These come from `AGENTS.md` (Hard rules) and `src/app/AGENTS.md`:

- **Fail-open channels.** Semantic, lexical and LSP fail independently. An optional channel
  that fails never fails a search. A reranker failure returns fusion order with a warning.
  Every new map must be an optional channel with the same contract: its errors go into
  `warning`, and work skipped on purpose goes into `notes`.
- **Filter placement.** Filters run inside each channel or on the universe, after fusion, and
  after rerank (`search.rs:690`, `:940`, `:998`; `store.rs:364`). A map that expands the
  frontier (graph neighbours, co-change) must filter newly reached chunks before they can
  reach the reranker or the results.
- **Freshness.** Freshness follows `[index].freshness`. Only `on-search` may refresh from a
  search, and it reports `refreshed_incrementally`. A failed refresh keeps the old snapshot
  and warns. An embedding-identity change is a hard error.
- **No new store or service.** No Docker, no extra database, no extra service. Data stays in
  `data_dir`, outside every workspace. Map stores must be LanceDB tables or files under
  `data_dir` (manifest-style JSON).
- **Repository content is data.** Agent instruction files indexed as a map are still data,
  never instructions.
- **Public API.** Keep `App` as the facade. Preserve public paths and serialized responses
  (`src/app/AGENTS.md`). `Hit`, `SearchReport` and the dump format are consumed by clients
  and by replay.
- **Tests offline.** Tests never contact ports 8765-8767. New maps need fakes under
  `tests/support`.

### 9.1 (a) A `Map` trait over chunks

| | |
| --- | --- |
| Where | Fan-out `search.rs:712`; per-channel post-processing `:836-906`; `fuse_hits` `:1297`; `Hit` `src/store.rs:27`; `ChannelsConfig` `config.rs:68`; `FusionConfig` `config.rs:172`; `Timings` `search.rs:29`; `CandidateTrace` `search.rs:256` |
| What exists | Three channels hard-coded as three `tokio::join!` arms. Each yields a `Vec<Hit>` with its own rank field. `fuse_hits` takes exactly three lists. Weights, switches, timings and trace columns are fixed structs. `LspAdapter` (`lci-core/src/lsp/adapter.rs:14`) already shows the trait-object registry pattern. |
| What would change | (1) A stable chunk id. Today's key includes `start_line`, so it moves when lines are inserted above the chunk. Option: `(path, content_hash, ordinal among equal hashes in the file)`, computed in `chunks_with_adapter` (`chunk.rs:144`) and stored as a column. (2) A trait in `lci-core`: `id`, `build(&Scan)`, `update(changed files)`, and `query(&MapQuery, &Universe) -> Result<Vec<(ChunkId, score)>>`, with seed-set queries for graph maps. Impls needing LanceDB (vector maps) go in the root crate. (3) `fuse_hits` becomes a loop over `(map_id, ranked list, weight)`. (4) `Hit` gains a `contributions` map. The legacy `semantic_rank`/`lexical_rank`/`lsp_rank` fields, `retrieval_channels` and `Timings` stay as projections so the JSON is unchanged. |
| Risk | The f32 summation order is part of the dump contract (`search.rs:244-254`), and replay checks bit-exact equality (`replay.rs:406`). A generic loop must add the three legacy channels first, in that order, or every old dump is refused. Per-search cost: `Store::chunks` already loads the whole table (`search.rs:689`), and each extra map that maps onto the universe adds a linear pass (roadmap N84 covers the lexical one). |

### 9.2 (b) Step-wise traversal with checkpoints

| | |
| --- | --- |
| Where | `search_once` `search.rs:637-1015` (a single function); `IndexResolution` `:521` |
| What exists | The stages run straight through with local variables: resolve (`:656-688`), universe (`:689-710`), fan-out (`:712-906`), fuse/filter/sort (`:932-941`), cap/cut (`:962-972`), rerank (`:974-996`), final filter (`:998`). `CandidateTrace` is already a serializable frontier row with per-channel evidence. |
| What would change | Split `search_once` into stage functions over a `TraversalState`: query, filters, snapshot identity (`Snapshot.indexed_at` plus the manifest fingerprint), cached universe, `frontier: Vec<CandidateTrace>`-like rows keyed by chunk id with per-map contributions, step log, and warnings/notes. Natural yield points: after fan-out (before fusion), after fuse/cap (the shortlist is the rerank cost decision), after rerank, and after each expansion hop. `search_once` then becomes "run every step without pausing", so existing behaviour is one traversal policy. |
| Risk | The read guard (`_read_guard`, `search.rs:522`) borrows `WorkspaceCoordination` and is held for the whole search. A paused traversal cannot hold it across MCP calls without blocking indexing, so each step would re-acquire it and check snapshot drift. `Store::replace` overwrites the table (`store.rs:301-305`), so a paused traversal cannot pin old rows. Whether older Lance versions stay readable by checkout was not checked. A traversal must also keep the filter-before-rerank rule at every step that adds chunks. |

### 9.3 (c) Index-time repository profile

| | |
| --- | --- |
| Where | `index_locked` `src/app/indexing.rs:196`; `Manifest` `lci-core/src/manifest.rs:20`; `IndexConfig` `lci-core/src/config.rs:28`; `embedding_identity` `config.rs:921`; walker `chunk.rs:204`; `Status` `indexing.rs:115`; `IndexReport` `indexing.rs:23` |
| What exists | One fixed build: chunk, embed, one table. Compatibility is a single identity string in table metadata (`store.rs:256`). The manifest records chunks only. The fingerprint covers supported-extension files only. |
| What would change | A profile picks which maps to build, either detected from the repository or set in a new config section. Note that `IndexConfig` and `Config` are `deny_unknown_fields`, and "profile" is already taken by query-time search profiles (`config.rs:107`). Changes: bump the manifest to v3 with `profile` and per-map `{identity, fingerprint, built_at}`, migrating the way `ManifestV1` does (`manifest.rs:57`). `index_locked` runs each selected map's build/update after `scan_incremental` and before `manifest::save`, under the same locks. Each map has its own identity, so changing one map's settings does not re-embed everything. Maps over non-code files (agent instructions, config keys, build graph) need their own walk with the same `WalkBuilder` settings (`chunk.rs:207-213`) and their own fingerprint, otherwise `on-search` staleness misses their changes. Map tables would be named `ws_<16hex>_<map>`. |
| Risk | Windows path length (`AGENTS.md` running-locally notes, `workspace.rs:33-39`). A map that fails during indexing must not fail the core index, which keeps the fail-open rule at index time. An identity change in an optional map must not inherit the hard-error rule that exists for the embedding identity (`search.rs:1105`). Watch and on-search both go through `index_locked`, so map build cost lands on search latency under `on-search`. |

### 9.4 (d) MCP traversal tools

| | |
| --- | --- |
| Where | `#[tool_router]` `src/server.rs:89`; REST `src/rest.rs:262` and `openapi_document` `:171`; `App` state `src/app/mod.rs:43` |
| What exists | Stateless tools. rmcp sessions exist (`LocalSessionManager`, `server.rs:243`, keep-alive 1 h), but `McpServer` holds only `app` and `search_profile`. The precedent for server-side state keyed by id is `App.watchers: Mutex<HashMap<_, CancellationToken>>` (`mod.rs:50`). |
| What would change | Add `start_traversal(workspace_path, query, filters, plan?)`, `step_traversal(id, until?)`, `inspect_traversal(id, detail?)`, `steer_traversal(id, ops)` and `end_traversal(id)`. Ops would include pin/drop chunk, expand through map M from a seed set, reweight, add a sub-query, and set the shortlist size. Each answer carries `index.action`, `warning`/`notes` and a bounded frontier summary (path, lines, role, contributions, `bounded_snippet`, `src/store.rs:63`). State lives in `App` keyed by traversal id, with a TTL and a cap on live traversals. Each tool gets a REST `POST /v1/...` mirror and an OpenAPI entry. |
| Risk | Payload size: the dump documents about 100 KB per query (`src/main.rs:136-141`), so the frontier must be summarised. Memory and lifetime of abandoned traversals. A traversal id must not be usable across endpoints or workspaces. The tools appear on every `/mcp/<profile>` endpoint. Steering text from an agent is control input, while chunk text stays data. |

### 9.5 (e) Evaluation changes

| | |
| --- | --- |
| Where | `EvalQuery` `src/evaluate/types.rs:40`; `QueryMetrics` `:283`; `CandidateTrace` `src/app/search.rs:256`; replay `fusion_score` `src/evaluate/replay.rs:307`; `CandidateArm` `types.rs:203`; compare key `compare.rs:295`; miner `xtask/src/mine.rs:705` |
| What exists | One query, one final list, path-glob gold. Per-channel contribution is visible only through three fixed rank columns in the dump. Replay and compare assume a deterministic single-shot search. |
| What would change | (1) Multi-hop items: an ordered or set-valued gold per hop (for example "config key, then reader, then test"), plus a scripted steering policy so runs are reproducible and pairable by `compare`. A free agent loop belongs to the I-10 agent A/B, not this harness. (2) Per-map contribution: generalise `CandidateTrace`/`CandidateArm` to a map-keyed rank/score map, keeping the legacy columns, and extend replay's fusion formula. Report per-map "unique gold contributed" (gold reached only through map M) and leave-one-map-out replays. (3) Step metrics: gold recall of the frontier at each checkpoint, plus steps and rerank tokens spent (`est_tokens` already exists, `search.rs:320`). |
| Risk | Leakage. A git co-change map built at HEAD includes the commits `mine-queries` turned into gold, so scoring it on `evaluations/mined/self.toml` would leak the answers. Build it from history before each query's commit, or exclude those commits. The miner says its output must not tune ranking (`evaluations/mined/self.toml:3-5`). Bit-exact replay of old dumps must keep working. |
