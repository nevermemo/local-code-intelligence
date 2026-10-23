# Spec: Map trait

Status: **v0, pre-freeze; built (S2, S3a, and the traversal expansion).** Written 2026-10-12 at slice S0, then merged with the as-built notes of S2 and S3a. The
trait freezes at the S4 gate (first end-to-end slice: lexical map plus a symbols map through NodeId, extraction and the harness). Until then every signature below
may change; the properties in section 6 should not. Labels (**Read**, **Inferred**, **built**) are defined in [README.md](README.md). The shipped map list is in
[../engine.md](../engine.md), "Maps".

## 1. Purpose

A map is one index (stored or live) over the NodeId space ([node-id.md](node-id.md)) that answers: given a query text and/or a seed set of nodes, which nodes come
next, how strongly, and through which evidence. The traversal engine calls maps, fuses what they return, and records each call in a RunRecord
([run-record.md](run-record.md)). The three retrieval channels and the source-role prior that exist today are wrapped as the first four maps, with bit-identical
results.

## 2. What exists today (Read)

The legacy search path, named by symbol (checked on the spike tree). The production search does not call the registry.

| Item | Where | Note |
| --- | --- | --- |
| Hard-coded three-way fan-out | `tokio::join!` in `search_once`, `src/app/search.rs` | Semantic arm returns only the query embedding; lexical arm returns ripgrep matches; LSP arm returns `Location`s with failures and over-budget providers. |
| Per-channel post-processing into `Vec<Hit>` | `map_lexical_matches`, `map_lsp_locations`, `src/app/search.rs` | The LanceDB query (`Store::search`) runs after the join. Each list carries its own rank field. |
| Fusion | `fuse_hits`, `src/app/search.rs` | Weighted RRF: `weight / (rrf_k + rank)`, summed in f32 in the order semantic, lexical, LSP; then the source-role prior is added unweighted. Key `(path, start_line, content_hash)`. The f32 order is part of the dump contract (`CandidateTrace` docs); replay checks bit-exact equality. Weights and switches are fixed `deny_unknown_fields` structs (`ChannelsConfig`, `FusionConfig`, `lci-core/src/config.rs`). |
| Fail-open handling | `search_once` | Errors become `warning`; a deliberate skip is silent; an over-budget provider is a `note`. |
| Filters | `hit_matches`, `src/app/search.rs`; `Store::search` filters inside | Applied to the universe, after fusion and on results; every list is already filtered before fusion. |
| Universe | `Store::chunks`, `src/store.rs`, loaded on every search | Every chunk with code; `Hit` rows have `fusion_score: 0.0`. |
| Crate split | Workspace members `xtask`, `lci-core`, `lci-maps`; `lci-core` must not depend on LanceDB, Arrow, axum or rmcp | Vector maps that touch LanceDB live in the root crate. |
| Registry precedent | `LspAdapter` trait objects, `lci-core/src/lsp/adapter.rs`; an `Arc<dyn LspAdapter>` map in `src/app/search.rs` | Trait-object registry already used for optional providers. `lci-core` depends on `async-trait` (`lci-core/Cargo.toml`). |
| petgraph | `petgraph 0.8.3` in `Cargo.lock`, license `MIT OR Apache-2.0` (from `cargo metadata`), reached through `lancedb` -> `lance` -> `datafusion-physical-expr` (from `cargo tree -i petgraph`; `Cargo.lock` also lists it under `prost-build`) | Already compiled for the root crate. Not used by any shipped map (M-5). |

## 3. Design (Inferred; built in `lci-maps/src/lib.rs`)

### 3.1 Crate placement

- Crate `lci-maps` depends on `lci-core` only: the `Map` trait, `MapCard`, the registry, the fusion loop and the maps that need no LanceDB.
- The root crate implements the maps that touch LanceDB or `App` internals and owns the engine that `App` calls. The wrapped lexical and LSP maps live in the root
  crate too (`src/app/search/channel_maps.rs`), not in `lci-maps`: the code they call (`App::map_lexical_matches`, `App::provider_symbols`, `App::map_lsp_locations`,
  `Hit::source_role_prior`) is there, and moving it would change the production path. `LspMap` and `search_once` call the same shared functions in
  `src/app/search.rs` (`App::applicable_lsp_adapters`, `lsp_symbol_queries`, `App::lsp_channel`, `App::lsp_over_budget_note`, `lsp_failure_warning`); the integration
  test that compares the registry with a traced search guards the map path.
- `lci-maps` adds `petgraph = "0.8"` only when a graph map needs it (M-5); no shipped map uses it.
- Spike entry points on the `App` facade (`App::fuse_through_maps`, `App::record_through_maps`) and the module `channel_maps` compile only with `#[cfg(any(test,
  feature = "map-equivalence"))]`, so the default build adds no public path.

### 3.2 The trait

```rust
#[async_trait::async_trait]
pub trait Map: Send + Sync {
    /// Static description: id, version, contribution kind, node kinds, freshness model.
    fn describe(&self) -> &MapDescriptor;

    /// One call. Implementations should not panic or exceed the deadline, but the registry does not
    /// rely on it: it catches a panic, stops waiting at the deadline and checks the answer (contract C1).
    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse;

    /// The edges leaving `node`, sorted and capped at EXPAND_LIMIT (256). The default is a seeded query:
    /// `node` the only seed, no text, hits turned into edges. Graph maps override it. The answer carries the
    /// cost of the call beside the edges (`edge::Expanded`), once, whatever the number of edges.
    async fn expand(&self, node: &NodeId, budget: Budget, view: &SnapshotView<'_>) -> edge::Expanded;

    /// What this map says about itself to a traversal that prices hops (`edge::MapCard`, see map-card.md).
    fn card(&self) -> edge::MapCard;
}
```

The v0 optional trait `IndexedMap` (extract and refresh) is split in two. Per-file extraction is a `lci_core::bundle::Producer` (`id`, `applies`, `produce(path,
&Extraction)`, `key`), not a method of the map: the hook lives in `lci-core`, which cannot depend on `lci-maps` (N-2). The map side is `fresh::Refreshable {
data_snapshot(), refresh(&mut self, &Target) }`, where `Target` is the core snapshot's identity, the workspace root and the snapshot's files (the manifest's),
instead of `(ScanDelta, BundleStore)`: the map recomputes its delta from the bundle keys, so it needs no delta type, and a refresh error is a `String` because
`lci-maps` has no `anyhow` dependency.

`MapDescriptor`:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | `MapId` (`&'static str`, dotted) | `lexical`, `vec.chunk.code`, `lsp.symbols`, `prior.source_role`, `sym.defs`, ... Stable across versions. |
| `version` | `u32` | Bumped when output for the same input changes. Part of RunRecord and the card. |
| `contribution` | `RankList` / `Prior` / `Filter` / `Expansion` | How fusion uses the output (3.4). |
| `accepts_text`, `accepts` | bool, set of NodeId kinds | What a request may carry. |
| `emits` | set of NodeId kinds | What hits may be. |
| `freshness` | `FreshnessModel`: `PerFile` / `PerCommit` / `Live` / `Global` | The update model: per-file (recomputed when the file is re-parsed), per-commit (when HEAD moves), live (computed per request), global (a change in one file can change entries keyed by others). |
| `source_labels` | subset of the C5 vocabulary | The labels its evidence may carry: vector, lexical, lsp, scip, tags, heuristic, git, config, doc, agent, prior. |
| `needs` | `Need`: `Embedding` / `Reranker` / `Lsp` (no provider named) / `Git` / `Ripgrep` | External dependencies, for readiness and fail-open reporting. |

### 3.3 Inputs and outputs

`MapRequest` (inputs):

| Field | Meaning |
| --- | --- |
| `text: Option<&str>` | The query, or a sub-query a steering op added. |
| `seeds: &[(NodeId, f32)]` | Seed nodes with weights, for expansion maps. Empty for text maps. |
| `limit: usize` | Most hits to return (today 40 for each channel: `semantic_candidate_count`, `lexical_candidate_count`, `lsp_candidate_count`). |
| `budget: Budget { deadline: Instant, max_external_calls: u32, max_nodes_touched: u64 }` | Contract C8. Only the deadline is enforced by the registry. |
| `params: &serde_json::Value` | Map-specific parameters from the plan or a steering op, validated by the map against its own schema. |
| `as_of: Option<&str>` | For history maps: see nothing after this commit (contract C9). A string, not a `GitSha`. |

`SnapshotView` (read-only context): the snapshot reference (`SnapshotRef { indexed_at, fingerprint }`: `indexed_at` from `Snapshot` in `src/store.rs` plus the
manifest fingerprint) and the filtered universe (`all_chunks` after `hit_matches`) with its NodeId interning (`SnapshotView::new`, `ids`, `contains`, `chunk`,
`locate`). The `EffectiveFilter` and config are not in it yet (C2). A map never sees the read guard.

`MapResponse` (outputs):

| Field | Meaning |
| --- | --- |
| `hits: Vec<MapHit>` | Ordered best first. `MapHit { node: NodeId, rank: u32 (1-based, the value fusion consumes), score: Option<f32> (map's own score, never enters RRF; a prior map's value), evidence: Vec<Evidence> }`. |
| `Evidence` | `{ via: Option<NodeId>, edge: EdgeKind, label: SourceLabel, detail: Option<String> (bounded) }`. A text hit has `via = None`; an expansion hit names the seed it came from. |
| `status` | `Ok` / `Partial { note: Option, warning: Option }` (part of the answer is missing and the hits are kept; a budget that ran out is a note, a source that failed while others answered is a warning; at least one is set; was `Partial { note }`, widened so the LSP channel can keep one server's answers while another failed) / `Skipped { reason }` (switched off, not applicable; silent, never shown) / `Failed { warning }` (any hits are ignored). |
| `cost: Cost` | Measured for this call: `elapsed_ms`, `external_calls`, `nodes_touched`, `bytes_read`, `est_tokens` (tokens of any document it sends to a model, `bytes / 4` as in `CandidateTrace::est_tokens`). |
| `freshness: Freshness` | `{ model, snapshot: SnapshotRef, observed: Snapshot / LiveTree / AsOf(sha), stale: bool }`. In a RunRecord the snapshot is recorded once per step, not per map call ([run-record.md](run-record.md), `mapCall.freshness`). A live map that reads the working tree while the universe comes from the snapshot says so (`LiveTree`). |

`EdgeKind` (`lci-maps/src/edge.rs`) has 14 kinds: query_match, prior, calls_or_refs, contains, imports, implements, tests, reads_key, mentions, touched, cochange,
compile_error_at, near, defined_in. Edges are sorted by confidence, then destination; the witness classes and `Site` are described in [../engine.md](../engine.md),
"Maps".

### 3.4 Fusion over maps

`fuse` replaces `fuse_hits` with a loop that keeps today's arithmetic exactly:

1. Order: `vec.chunk.code`, `lexical`, `lsp.symbols` first, in that order (`LEGACY_ORDER`); then every other `RankList` or `Expansion` map in registry order (sorted
   by `MapId`); then `Prior` maps.
2. For each map's hits in its own order: `score[node] += weight(map) / (rrf_k + rank as f32)`, in `f32`, starting from `0.0`. A node's first contribution inserts it.
3. `Prior` maps add their value unweighted (the source-role prior, `src/store.rs`). After the rank lists are fused, every `Prior` map gets the fused nodes as seeds
   and returns one hit per node with its value in `score`. Rank lists run side by side (as the `tokio::join!` does), priors after them.
4. `Filter` maps remove nodes before step 2 (they never add). The registry refuses to register one (`RegisterError::FilterUnsupported`) until contract C2 lands; a
   filter that is silently skipped would be worse.
5. The fused list is sorted with today's `fusion_order` (score, then path, `start_line`, `end_line`, `content_hash`), not by NodeId, so equal-score chunks keep their
   legacy order (contract C6).
6. Hits that are not chunks (a Symbol, a DocSection) are projected to chunk NodeIds through `defined_in` / `occurs_in` edges before fusion in the MVP; the RunRecord
   keeps the original node. Non-chunk results in the final list are an S5+ question (M-4).

The legacy `semantic_rank`, `lexical_rank`, `lsp_rank`, `retrieval_channels` and `Timings` fields of `Hit` (`src/store.rs`) and `SearchReport` (`src/app/search.rs`)
stay as projections of the per-map contributions, so the serialized `search_code` response does not change until a new field is added on purpose.

Registry API (built): `Registry::new(rrf_k)`; `register(Box<dyn Map + 'a>, weight, limit)` keeps entries in fusion order whatever the registration order;
`set_enabled(id, false)` records the call as `Skipped` (returns whether the id is registered); `run(text, budget, view)` returns every call and the fused pool
(`Run`); `Run::warnings()` and `Run::notes()` attribute each message to its map id (the "naming" in contract C1 is structural; the wrapped maps keep today's texts,
which do not contain the id). `independent_expanders`, `expanding_maps` and `ids` serve the traversal. `describe()` is called outside the fail-open boundary and must
not panic; `Registry::run` needs a Tokio runtime with the time driver.

The registry checks every answer and turns a bad one into `Failed` naming the map: a node outside the view (or, for a prior, outside its seeds), a node twice, a rank
of 0 or a falling rank (ties are allowed: the fusion unit cases give three hits rank 1), a non-finite score, a prior without a value, more hits than the limit, a
label off the card (C5, via `labels::check_labels`, merge condition M6) and `Partial { note: None, warning: None }` (it would add its hits while hiding what is
missing). A panic is `Failed`; a missed deadline is `Partial` with a note. A refused partial answer keeps its own note and warning in the refusal. A stored map's CPU
or file work runs off the async workers: `lci_maps::off_thread` runs it with `tokio::task::spawn_blocking` and awaits it (a panic resumes in the map, so the registry
reports it); at most one task per stored map and search, and Tokio runs at most `max_blocking_threads` (512 by default) at once. `Registry::run` never calls
`Refreshable::refresh` or `fresh::prepare`, so a refresh is not on the request path (wiring `prepare` into a search is S4's, K-10).

Wrapper behaviours (`map_hits`): `LspMap` is `Failed` only when every provider failed and no hit was gathered; a provider that answered some symbol queries before
failing keeps its hits, because `search_once` fuses whatever was gathered (`lsp_status`). A chunk a channel lists twice (two stored rows with one path, lines and
hash) is kept once at its first rank; `fuse_hits` would score the merged entry by the later row (semantic) or by both rows (lexical, LSP), and without the dedupe the
registry would fail the whole channel ("returned twice"). A legacy hit that is not a chunk of the view is left out of a wrapped map's answer; with the same snapshot
and filter none is. The N-5 twins (one legacy key, two NodeIds) would fuse as two entries in the registry and one in `fuse_hits`; no fixture and no snapshot of this
repository's last 201 commits has one ([../s1-stability.md](../s1-stability.md)).

### 3.5 Wrapping the existing channels

| Map | Wraps (Read) | Rank / score | Status mapping | Freshness |
| --- | --- | --- | --- | --- |
| `vec.chunk.code` (root crate) | `Models::query` then `Store::search` with `semantic_candidate_count` and the filter | `semantic_rank`, `semantic_score` | embedding error or search error -> `Failed` with today's warning text; `channels.semantic = false` -> `Skipped`, no note | `per-file` (vectors reused by content hash); reads the snapshot |
| `lexical` | `lexical::search` then `map_lexical_matches` | `lexical_rank`, `lexical_score`, evidence detail = credited line count | error -> `Failed` | `live`; observed = live tree, mapped onto the snapshot |
| `lsp.symbols` | symbol queries, `provider_symbols` under `lsp_search_budget_ms`, `map_lsp_locations` | `lsp_rank` | provider failures -> `Failed` or `Partial` with today's warning; over-budget -> `Partial` with today's note | `live` |
| `prior.source_role` | `classify_source_role` (`lci-core/src/filter/source_role.rs`), `source_role_prior` (`src/store.rs`) | prior value | never fails | `live` (path-only) |

Two behaviours must survive the wrap: the "every enabled channel failed" error and the "empty is fine" cases are decided by the engine from the four statuses, not by
a map; and a disabled channel adds neither a warning nor a note.

### 3.6 `sym.defs` (built, S3a)

Code: `lci-maps/src/symdefs.rs`, `lci-maps/src/fresh.rs` (`Target`, `Refreshable`), tests in `lci-maps/src/symdefs/tests.rs`. Descriptor: id `sym.defs`, version 1,
`RankList`, accepts text, emits `Chunk`, freshness `per-file`, label `tags` (Tree-sitter), needs nothing.

- **Answer.** The query is split into identifier tokens (maximal runs of alphanumeric characters and `_`, Unicode-aware). A declaration matches when its name equals
  a token ignoring case; exact-case matches rank first, then the rest, each class ordered by the symbol's canonical string. Each match is projected to its head chunk
  (M-4): among the chunks overlapping the declaration's lines, the first in source order whose code names the declaration as a whole identifier (the signature chunk,
  not a body fragment or a lone `pub trait` keyword chunk); failing that, the chunk holding its first line, the one starting last on a tie. Two items on one line can
  share a head. A declaration in no chunk is left out, as is a head outside the view. A chunk is listed once, at its best declaration; ranks are positions. Evidence:
  `via = None`, `QueryMatch`, `tags`, detail = the symbol's canonical string (its last 256 bytes when longer, so `::<name>/<kind>` survives). `score` is `None`.
- **Build.** `build`/`refresh` syncs the bundles, joins them and replaces the data only when the sync succeeded; `last_sync` records (reused, extracted). It then
  deletes the store's bundles no current file uses (`BundleStore::retain` with this producer's keys, so the store must hold no other producer's bundles; today it
  holds only `sym.defs@1`); a failed clean-up is kept in `cleanup_error` and does not fail the build. Its token match runs in `off_thread`.
- **Freshness of the answer.** `freshness.snapshot` is the snapshot the data was built for, not the view's; `stale` is true when they differ (the hits are then those
  of the old data that are still chunks of the view). A map that was never built answers `Failed` with a warning.
- **Cost.** `nodes_touched` is the number of declarations held; `elapsed_ms` is measured. The deadline is not checked inside the call (the answer is an in-memory
  lookup); the registry's timeout still applies.
- **Kit.** C1 (`check_fail_open`) and C6 (`check_determinism`) pass for `sym.defs` against a control map; registered beside it, its hits fuse with weight 1. No
  production search or traversal call uses it: it lives in `lci-maps` and runs in the contract kit and the audit, and the traversal map selector (`MAPS`
  and `OPT_IN_MAPS` in `src/app/traverse/mod.rs`) neither lists nor accepts it ([../engine.md](../engine.md), "Other maps").

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| M-1 | One `query` method for text and seed requests; `contribution` says how the output is used. | Separate `TextMap` and `ExpansionMap` traits (clearer types, more registry code). | S4 freeze. |
| M-2 | `query` returns `MapResponse`, never `Result`; the registry enforces fail-open. | Return `Result` and let the engine convert (simpler impls, but one forgotten `?` breaks C1). | S4. |
| M-3 | Fusion order fixed: three legacy maps, then others sorted by `MapId`, then priors. | Order by a declared `fusion_slot` number. | S2 (T0 test). |
| M-4 | Non-chunk hits are projected to chunks before fusion in the MVP. | Fuse over all node kinds and return mixed results (needs a response shape change). | S5. |
| M-5 | petgraph `DiGraph` over interned ids, as a direct dependency of `lci-maps`, when the first graph map needs it. | An in-house CSR adjacency (the default of an earlier slice plan, not in the repository); taken only if petgraph shows up in a profile. | First graph map (W1b). |
| M-6 | `MapRequest.params` is untyped JSON validated by each map. | A typed enum of all map parameters (compile-time checks, but every new map edits a shared enum, which AGENTS.md asks to avoid in parallel work). | S4. |
| M-7 | A map's LanceDB table, if any, is `ws_<16hex>_<map>` under the same `data_dir`. | One wide table with nullable columns per map. | First stored map (W1b). |
| M-8 | Index-time map failures never fail the core index; an optional map's identity change is a stale map, not the hard error reserved for the embedding identity. | none considered | S1. |

## 5. Not decided here

The traversal loop and yield points (in [steering-ops.md](steering-ops.md) and [run-record.md](run-record.md)), card contents ([map-card.md](map-card.md)), and the
contract tests ([contract-kit.md](contract-kit.md)).

## 6. How a test proves it

| Property | Test |
| --- | --- |
| Bit-exact legacy fusion (T0) | Run the existing fusion unit cases (in `src/app/search/tests`, for example `unit_weights_match_plain_rrf` and `traced_ranks_weights_and_prior_reproduce_every_fusion_score_bit_for_bit`) through the registry with the four wrapped maps: every `fusion_score` equal by `to_bits()`. Built in `src/app/search/tests/t0.rs`: 36 weighting cases of the four row sets, the merge, alone and tie cases, plus the live end-to-end comparison with a traced search's pool. The replay part (stored candidate dumps through `lci replay`: zero refused queries) could not run: dumps go to `test-results/` and none is committed. |
| Response unchanged | Golden JSON of `search_code` for the offline fixture workspace before and after the wrap: byte-identical (field order included). Built (`tests/cases/indexing.rs`; the golden JSON was written before `lci-maps` existed and is unchanged). |
| Fail-open per map | A fake map that returns `Failed`, one that panics, one that sleeps past its deadline: the search returns the other maps' fusion, with one warning (failure, panic) or one note (deadline) naming the map (contract C1, C8). Built through the kit (failed, panic, deadline, foreign node, NaN) and the real lexical failure (ripgrep missing) via `kit::check_real_failure`; the semantic and LSP failure paths (embedding refused, one language server failing while another answers) are compared with a traced search in `tests/cases/indexing.rs`. |
| Disabled is silent | With each `[channels]` switch off in turn: no warning, no note, zero timing for that map (as today). Built at registry level (kit, "switched off"); the timing part waits for an engine. |
| Filter before fusion | A fake expansion map that returns nodes outside the filter: none reaches fusion, the rerank request (fake reranker records its documents) or the results (contract C2). Deferred (needs C2 and the engine, S4). |
| Order independence of registration | Registering the extra maps in a different order gives the same fused scores (the loop sorts by `MapId`). Built. |
| Status to error mapping | All enabled maps `Failed` -> the same error text as today; semantic off and lexical `Ok` with no hits -> empty report, no error. Deferred (needs the engine, S4). |
