# Language-server channel

Code-level picture of how the language servers run inside LCI: the `lci-core` `lsp` module (`Manager`, sessions, breakers, readiness, sync, outline cache) and how the search channel, the navigation tools and the `lsp.live` traversal map use it. Crate layout and the other channels: [architecture.md](architecture.md); settings and defaults: [README.md](../README.md); behaviour and defaults of the `lsp.live` switches: [CLI reference](reference/cli.md); `lsp.live` as a map: [engine.md](spike/multimap/engine.md).

- **LSP channel** (`lci-core` `lsp`): answers are merged query by query across providers and first-seen order decides the LSP rank (`lsp_channel` in `src/app/search.rs`). `LspAdapter` is a trait-object registry; `Manager` holds 8 adapters (rust-analyzer,
  csharp-ls, typescript-language-server, pyright, gopls, jdtls, clangd, marksman), sessions keyed by `(provider, workspace_id)`,
  started on a detached task shared by concurrent callers (its `initialize` is bounded by `lsp_start_timeout_seconds`,
  default 0 = `lsp_timeout_seconds`, like every later request) and primed with one background `workspace/symbol`. Idle
  sessions are released after `lsp_idle_timeout_seconds` (1800) and a session that answers only transient errors for
  `lsp_stuck_seconds` (900) is replaced. Requests: `symbols` (`workspace/symbol`), `definition`, `references`, all
  returning `Location` (path, range, name, kind, language, provider). Search use: up to 8 symbol queries (a term plus
  `#`, plus a singular form for plurals), providers in parallel, each under `lsp_search_budget_ms` (1500); a provider
  over budget adds a note, a provider that fails adds a warning; locations map onto chunks of the same language that
  contain `start_line`; capped at `lsp_candidate_count` (40). Navigation tools (`symbols`, `definition`, `references`)
  are not fused, return raw `Location`s and need an index containing the file's language. A server whose symbols are not code names (marksman: headings) opts out of the search channel (`LspAdapter::joins_search_channel`, default true) and is reached by navigation only. The `lsp.live` map of a
  traversal (`lci-maps` `lsp_live`) asks the `Manager`s it is given: `ManagerBackend::open(workspace, Arc<Manager>)` (one for every server) or `ManagerBackend::open_split(workspace, shared, own, probe)`, which probes through `shared` and routes each server to `shared` when `LspAdapter::watches_workspace()` (rust-analyzer alone: it runs its own file watcher) or `shared` syncs (`sync = "watch"`, the default since K11b-4), and in both cases only when the adapter lists its tree stamp's configuration files (`keeps_session`; not jdtls or marksman), else to `own`; a server routed to `shared` first passes `Manager::tree_barrier` (a walk passes two, see below), which stats the server's indexed files, their directories and ancestors and its manifests (`lsp::stamp`, from the index's file list that `App` supplies through `stamp::IndexedFiles` over the workspace manifest) and stops the session when the stamp changed, so the walk starts one that reads the disk. Navigation and the search channel pass the same barrier, once per call, and carry the same notes (`lsp:` for `lsp.live:`; `NavigationReport.notes`, `SearchReport.notes`); the search channel alone (`Manager::search_barrier`) keeps a running session of a server that watches its workspace through a changed stamp, records no stamp for it and labels the answer `kept session; <reason> (kept: it watches its workspace)`, so the next navigation call or walk restarts it (N108). `lsp.live` is in the default map set of a call that names no maps, has a tree, runs under `sessions = "shared"` and was given a manager (the service's; the CLI one-shot has none unless `--maps` or a policy names the map) while `[run_profile.lsp] live` is `true` (the default; `app::traverse::run_with_trait_seeds`). A walk that runs the map on the shared manager passes the strict tree barrier once, before its seed search (`walk_tree_barrier`): the seed search is the search channel and would otherwise keep a watching server through an edit that the backend's own barrier then restarts inside the walk's deadline, so the server is restarted once, up front, and the seed search and the map find the same session. The backend then passes its own strict barrier after the seed search, as before (a continuation skips the pre-seed one and has only the backend's), so a walk has one restart: the pre-seed barrier restarts a stale server, and the backend's restarts it again only if the stamp changed between the two (a refresh that adds a file in between). Under
  `[run_profile.lsp] sessions = "per_call"` `app::traverse::run` builds one per call, so the map owns it and its
  server dies with the map; under `"shared"` (default) it uses `Call.lsp_sessions` as `shared` and a per-call manager as `own`, so a server that lists no configuration files for the tree stamp (jdtls, marksman), or that neither sees an edit on its own nor is told by the sync (`sync = "off"`; the default is `"watch"`), starts per call, and a reused server is never staler than `per_call` for any change inside the tree stamp's coverage (its residuals are in the [CLI reference](reference/cli.md), "Language-server sessions of `lsp.live`"; the walk labels a reused server's answer with a `kept session` note); `Call.lsp_sessions` is the `Arc<Manager>` that `App::language_servers()`
  hands out (the service's own, so a server started by navigation, search or a kept traverse server serves the others; the CLI
  `traverse` builds one per process and shuts it down after the walk), and a kept server's lifetime is the manager's (idle
  sweeper, `release_workspace`, shutdown). `Manager::probe_once` caches a successful readiness probe per provider for
  the manager's life (the probe is per adapter, `adapter::probe_subprocess`: `<server> --version` that exits 0 by default, `gopls version`,
  and for pyright a start whose exit status is ignored, since `pyright-langserver` has no version flag and exits 1; so a dead pyright
  shows first at the session start, not at the probe). Under `[run_profile.lsp] breaker = true` (the default) a manager also holds `lsp::LspBreakers`, one 30 s
  window per provider (`LSP_DOWN_WINDOW`, the `ConnectBreaker` of the model clients): a failed probe in `probe_once`
  (`adapter::ProbeFailure::Failed`, or an adapter's own readiness error; a missing binary or a probe that timed out opens
  no window, since it is no evidence of a dead server: the miss costs under a millisecond and its note is the same on every
  walk, and a slow probe may be a loaded machine),
  or a start in `Session::begin` that failed for a reason other than the transport's inactivity timeout
  (`transport::InactivityTimeout`: a slow `initialize` is not a dead server), opens it, and `probe_once` and
  `Manager::session` (a new start only; a usable session is returned whatever the window says) then refuse the
  provider with a `lsp breaker` error until it has passed or a probe or start succeeds. `per_call_manager` gives
  a call's own manager the windows of `Call.lsp_sessions` (under both `sessions` values), so a dead server found by
  one call is skipped by the next; `/ready` probes the adapters directly and never reads them. `Status.servers` and
  `Status.languages_without_server` (`index_status`, `lci status`) are the per-language roster: per adapter its
  languages, the ones the index holds, `configured`/`enabled`/`command`, `Manager::probe_state` (the `probe_once` memo
  read as `probed`/`not_probed`; it runs no probe), `Manager::breaker_remaining` (an open skip window as `breaker_open_ms`, `null` when none) and the session's `ServerReadiness`, and the indexed languages no
  enabled, configured adapter lists; built from the manager's memory, it starts no server. The roster's `root` and `root_reason` are the directory a server starts at: `[run_profile.lsp] roots` (`Manager::new`; `"workspace"` default) makes `Session::begin` choose it, in its start task, through `lsp::roots` (`Manager::server_root`; under `"manifest"`, `roots::for_server`: the topmost manifest the adapter names in the directories of the index's files of its languages and their ancestors, one directory read each, the workspace for none, for sibling manifests or when files would fall outside), pass it to `GenericClient::spawn` and the adapter's `transport_config`/`initialize_params`, and keep it for `Manager::session_root`; one session per server per workspace, the session key, the tree stamp and the barrier are unchanged. The recipes walk no `lsp.live` and pass no manager. What `"manifest"` was measured to gain (a premise check on small fixtures): pyright resolves a name that depends on a nested project's own settings (`extraPaths`) only from the project directory; typescript-language-server already finds the nearest `tsconfig.json` from the workspace root and gains nothing; one indexed file of the server's languages outside the project directory returns the root to the workspace. `[run_profile.lsp] outline_cache` (read by
  `Manager::new` from the config it is built with: the App's, the CLI's, or `Call.lsp` for a per-call manager) gives the
  manager an outline cache, used only for adapters whose `LspAdapter::opens_documents()` is true (pyright, gopls,
  jdtls, clangd, typescript-language-server, marksman: the hook sends a changed disk text, as a `didChange` with the whole
  text when the session read change sync (`ChangeSync`, from `textDocumentSync` in the `initialize` result) from the server and as a second `didOpen` otherwise, and the server takes it; the wait for the `publishDiagnostics` of the send is kept in `SentTexts` until a hook has finished it, so a cancelled hook leaves it to the next caller of the file; `SentTexts` holds at most `MAX_OPEN_DOCUMENTS` = 64 files per session and closes the least recently used one that has no open in flight before a new file is opened): `Manager::document_symbols` keys each
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
  nodes, under `[run_profile.lsp] places = "scip"`. With `places = "outline"` it is `OutlinePlaces` over it; `places = "auto"` (default) picks per walk: `ScipPlaces` when the SCIP graph read for the workspace has a node, else `OutlinePlaces` (`app::traverse::run`), so a tree with no index read (`scip.mode = "off"`, or `"cache"` with no file) still gets `lsp.live` cards. `OutlinePlaces`:
  `ask` outlines a symbol's file once per map instance (`Manager::document_symbols`, one counted external call;
  `SymbolPlaces::outline_wanted`), as it already does for a file or chunk node, and `SymbolPlaces::learn` takes the
  outline in. A member whose kind this build has a node for (`SymbolKind`: functions, methods, types, enums,
  modules, constants, classes, interfaces, fields, variables; LSP kinds 12, 6, 9, 5, 23, 11, 10, 2-4, 14, 7 and 8, 13; 5 is `Class`, 11 is `Trait` in a Rust file and `Interface` elsewhere; enum members 22, impl blocks 19 and type parameters 26 have no node) takes the SCIP node's id for the same file, name and
  nearest line, else (a Rust file, `OutlinePlaces::with_tree`) the canonical id of the `chunk::extract_for_path` declaration around the member, else a minted `SymbolKey` with the file's `language::for_path` identifier and the equal names
  numbered in document order; where a file is learned its places win over the index's, which may be older than the
  tree. `flatten_document_symbols` gives a nested `DocumentSymbol` the range from its name to its item's end, so
  `Member::end_line` is the last line of the item.
  Readiness is reported, not acted on: the transport keeps the `is_ready` latch (the adapter's `ready_predicate`
  matched once, never cleared) and, beside it, a `watch` of `ServerState` (`busy`, `changes`, `changed_at`) fed by the
  adapter's `busy_predicate` (default: never; rust-analyzer `quiescent: false`, jdtls `Starting`). `Manager::readiness`
  turns a session into a `ServerReadiness` (`starting`, `ready`, `busy`, `failed` or `stuck`, primed, counters, ages)
  without touching `last_used`; `Status.analyzers`, the search over-budget note and the `lsp.live` notes of a traverse
  call read it. No request, repeat or answer depends on it.
  `[run_profile.lsp] sync = "watch"` (the default since K11b-4; read by `Manager::new`; a per-call
  manager is built `without_sync`) gives each workspace with a session an `lsp::sync::WorkspaceSync`: a `notify`
  watcher of the root (polling at `watch_poll_milliseconds` when the platform watcher fails) whose events a task
  drains into batches, typed by the disk (created, changed, deleted) against a `(mtime, length)` table from one
  `chunk::source_files` walk (walked again only after an overflow). The manager's sink drops the files' kept outlines
  and sends each batch to the workspace's sessions as `workspace/didChangeWatchedFiles`; `Manager::client` drains
  before every request. The sync starts with the workspace's first session and stops with its last (`release`,
  `shutdown_all`, the idle sweeper). Lock order: drain lock, then the session registry, then `syncs`.
  `Status.sync` (N118) reports `Manager::sync_status` for the workspace (backend, `failed` when neither watcher started or the opening task died,
  the counters) only while a sync runs, so a failed or silent watcher is visible; it is a report and changes no answer.
