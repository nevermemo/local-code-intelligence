# Spec: Map trait

Status: **v0, pre-freeze.** Written 2026-10-12T10:04Z (`date -u`) at slice S0. Docs only. The
trait freezes at the S4 gate (first end-to-end slice: lexical map plus a symbols map through
NodeId, extraction and the harness). Until then every signature below may change; the
properties in section 6 should not.

Code read at spike head `df7faaa` (code of `7f9c636`). Labels: **Read** = in the code or docs
at that tree; **Inferred** = designed here.

## 1. Purpose

A map is one index (stored or live) over the NodeId space ([node-id.md](node-id.md)) that
answers: given a query text and/or a seed set of nodes, which nodes come next, how strongly,
and through which evidence. The traversal engine calls maps, fuses what they return, and
records each call in a RunRecord ([run-record.md](run-record.md)). The three retrieval
channels and the source-role prior that exist today become the first four maps, with
bit-identical results.

## 2. What exists today (Read)

| Item | Where | Note |
| --- | --- | --- |
| Hard-coded three-way fan-out | `tokio::join!`, `src/app/search.rs:712-801` | Semantic arm returns only the query embedding (`:713-719`); lexical arm returns ripgrep matches (`:720-729`); LSP arm returns `Location`s with failures and over-budget providers (`:730-800`). |
| Per-channel post-processing into `Vec<Hit>` | `src/app/search.rs:836-906`; `map_lexical_matches` `:1139-1222`; `map_lsp_locations` `:1259-1287` | The LanceDB query runs after the join (`Store::search`, `:844-851`). Each list carries its own rank field. |
| Fusion | `fuse_hits`, `src/app/search.rs:1297-1359` | Weighted RRF: `weight / (rrf_k + rank)`, summed in f32 in the order semantic, lexical, LSP; then the source-role prior is added unweighted (`:1355-1357`). Key `(path, start_line, content_hash)` (`:1304`). |
| Dump contract | `CandidateTrace` docs, `src/app/search.rs:238-254` | The f32 order is part of the contract; replay checks bit-exact equality (architecture-map.md, section 8). |
| Fail-open handling | `src/app/search.rs:813-835` (LSP notes and warnings), `:855-876` (semantic), `:894-902` (lexical), `:907-931` (all empty) | Errors become `warning`; a deliberate skip is silent; an over-budget provider is a `note`. |
| Filters | `hit_matches`, `src/app/search.rs:435-441`, applied to the universe (`:690`), after fusion (`:940`) and on results (`:998`); `Store::search` filters inside (`src/store.rs:331-337`) | Every list is already filtered before fusion. |
| Universe | `Store::chunks`, `src/store.rs:400`, loaded on every search (`search.rs:689`) | Every chunk with code; `Hit` rows have `fusion_score: 0.0` (`src/store.rs:440`). |
| Switches and weights | `ChannelsConfig` `lci-core/src/config.rs:68-89`; `FusionConfig` `:172` | Fixed structs, `deny_unknown_fields`. |
| Registry precedent | `LspAdapter` trait objects, `lci-core/src/lsp/adapter.rs:14`; `Arc<dyn LspAdapter>` map in `search.rs:695-710` | Trait-object registry already used for optional providers. `lci-core` depends on `async-trait` (`lci-core/Cargo.toml`). |
| Crate split | Workspace members `xtask`, `lci-core` (`Cargo.toml:1-2`); `lci-core` must not depend on LanceDB, Arrow, axum or rmcp (architecture-map.md, section 1) | Vector maps that touch LanceDB live in the root crate. |
| petgraph | `petgraph 0.8.3` in `Cargo.lock`, license `MIT OR Apache-2.0` (from `cargo metadata`), reached through `lancedb` -> `lance` -> `datafusion-physical-expr` (from `cargo tree -i petgraph`; `Cargo.lock` also lists it under `prost-build`) | Already compiled for the root crate; not yet a dependency of `lci-core`. |

## 3. Design (Inferred)

### 3.1 Crate placement

- New crate `lci-maps` (S2), depending on `lci-core` only: the `Map` trait, `MapCard`, the
  registry, the fusion loop, the in-memory graph projection (petgraph), and maps that need no
  LanceDB (lexical, LSP symbols, source role, parse-derived and git maps).
- The root crate implements maps that touch LanceDB (`vec.chunk.code` and later vector
  variants) against the same trait, and owns the engine that `App` calls.
- `lci-maps` adds `petgraph = "0.8"` as a direct dependency (same locked version, no new
  download). Graph maps use `petgraph::graph::DiGraph<u32, EdgeAttr>` over the snapshot's
  interned NodeIds; nothing in-house unless a benchmark shows petgraph is the bottleneck.

### 3.2 The trait

```rust
#[async_trait::async_trait]
pub trait Map: Send + Sync {
    /// Static description: id, version, contribution kind, node kinds, freshness model.
    /// The measured part of the card is loaded separately (map-card.md).
    fn describe(&self) -> &MapDescriptor;

    /// One call. Never returns Err and never panics across this boundary: the registry wraps
    /// the call (catch_unwind on the blocking part, a timeout on the async part) and turns any
    /// failure into `MapStatus::Failed` (contract C1).
    async fn query(&self, request: &MapRequest, view: &SnapshotView<'_>) -> MapResponse;
}

/// Optional, for maps with stored or cached data (S1 onwards). Live maps do not implement it.
pub trait IndexedMap: Map {
    /// Data produced for one file inside the parse (the extraction hook beside
    /// `chunks_with_adapter`), path-free, keyed by (content_hash, language, adapter_version, producer).
    fn extract(&self, file: &ParsedFile<'_>) -> Option<FileBundle>;
    /// Rebuild resident state (reverse indexes) from the current bundles. Must equal a full
    /// rebuild (contract C4).
    fn refresh(&self, delta: &ScanDelta, bundles: &BundleStore) -> anyhow::Result<()>;
}
```

`MapDescriptor`:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | `MapId` (`&'static str`, dotted) | `lexical`, `vec.chunk.code`, `lsp.symbols`, `prior.source_role`, `sym.defs`, ... Stable across versions. |
| `version` | `u32` | Bumped when output for the same input changes. Part of RunRecord and the card. |
| `contribution` | `RankList` / `Prior` / `Filter` / `Expansion` | How fusion uses the output (3.4). |
| `accepts` | set of NodeId kinds, plus `text` | What a request may carry. |
| `emits` | set of NodeId kinds | What hits may be. |
| `freshness` | `per-file` / `per-commit` / `live` / `global` | The update model of maps-inventory.md, "Update model vocabulary". |
| `source_labels` | subset of the C5 vocabulary | The labels its evidence may carry. |
| `needs` | `embedding` / `reranker` / `lsp:<provider>` / `git` / `none` | External dependencies, for readiness and fail-open reporting. |

### 3.3 Inputs and outputs

`MapRequest` (inputs):

| Field | Meaning |
| --- | --- |
| `text: Option<&str>` | The query, or a sub-query a steering op added. |
| `seeds: &[(NodeId, f32)]` | Seed nodes with weights, for expansion maps. Empty for text maps. |
| `limit: usize` | Most hits to return (today 40 for each channel: `semantic_candidate_count`, `lexical_candidate_count`, `lsp_candidate_count`). |
| `budget: Budget { deadline: Instant, max_external_calls: u32, max_nodes_touched: u64 }` | Contract C8. Today only LSP has a budget (`lsp_search_budget_ms`, `search.rs:711`, `:764-768`). |
| `params: &serde_json::Value` | Map-specific parameters from the plan or a steering op, validated by the map against its own schema. |
| `as_of: Option<GitSha>` | For history maps: see nothing after this commit (contract C9). |

`SnapshotView` (read-only context): the workspace, the snapshot reference (`indexed_at` from
`Snapshot`, `src/store.rs:76-81`, plus the manifest fingerprint, `lci-core/src/manifest.rs:24`),
the filtered universe (today's `all_chunks` after `hit_matches`, `search.rs:689-690`) with its
NodeId interning, the `EffectiveFilter`, and the config. A map never sees the read guard.

`MapResponse` (outputs):

| Field | Meaning |
| --- | --- |
| `hits: Vec<MapHit>` | Ordered best first. `MapHit { node: NodeId, rank: u32 (1-based, the value fusion consumes), score: Option<f32> (map's own score, never enters RRF), evidence: Vec<Evidence> }`. |
| `Evidence` | `{ via: Option<NodeId>, edge: EdgeKind, label: SourceLabel, detail: Option<String> (bounded) }`. A text hit has `via = None`; an expansion hit names the seed it came from. |
| `status` | `Ok` / `Partial { note }` (budget ran out, answers so far kept) / `Skipped { note }` (switched off, not applicable) / `Failed { warning }`. |
| `cost: Cost` | Measured for this call: `elapsed_ms`, `external_calls`, `nodes_touched`, `bytes_read`, `est_tokens` (tokens of any document it sends to a model, `bytes / 4` as in `CandidateTrace::est_tokens`, `search.rs:318-320`). |
| `freshness: Freshness` | `{ model, snapshot: SnapshotRef, observed: live-tree / snapshot / as-of <sha>, stale: bool }`. In a RunRecord the snapshot is recorded once per step, not per map call ([run-record.md](run-record.md), `mapCall.freshness`). A live map that reads the working tree while the universe comes from the snapshot says so (lexical today, architecture-map.md 4.2). |

### 3.4 Fusion over maps

`fuse` replaces `fuse_hits` with a loop that keeps today's arithmetic exactly:

1. Order: `vec.chunk.code`, `lexical`, `lsp.symbols` first, in that order; then every other
   `RankList` or `Expansion` map in registry order (sorted by `MapId`); then `Prior` maps.
2. For each map's hits in its own order: `score[node] += weight(map) / (rrf_k + rank as f32)`,
   in `f32`, starting from `0.0`. A node's first contribution inserts it.
3. `Prior` maps add their value unweighted (the source-role prior, `src/store.rs:48-53`).
4. `Filter` maps remove nodes before step 2 (they never add).
5. The fused list is sorted with today's `fusion_order` (`search.rs:337-344`: score, then path,
   `start_line`, `end_line`, `content_hash`), not by NodeId, so equal-score chunks keep their
   legacy order (contract C6).
6. Hits that are not chunks (a Symbol, a DocSection) are projected to chunk NodeIds through
   `defined_in` / `occurs_in` edges before fusion in the MVP; the RunRecord keeps the original
   node. Non-chunk results in the final list are an S5+ question (Open decisions).

The legacy `semantic_rank`, `lexical_rank`, `lsp_rank`, `retrieval_channels` and `Timings`
fields of `Hit` (`src/store.rs:27-45`) and `SearchReport` (`search.rs:28-37`) stay as
projections of the per-map contributions, so the serialized `search_code` response does not
change until a new field is added on purpose.

### 3.5 Wrapping the existing channels

| Map | Wraps (Read) | Rank / score | Status mapping | Freshness |
| --- | --- | --- | --- | --- |
| `vec.chunk.code` (root crate) | `Models::query` (`lci-core/src/models.rs:399`) then `Store::search` (`src/store.rs:331`) with `semantic_candidate_count` and the filter | `semantic_rank`, `semantic_score` | embedding error or search error -> `Failed` with today's warning text (`search.rs:855-876`); `channels.semantic = false` -> `Skipped`, no note | `per-file` (vectors reused by content hash); reads the snapshot |
| `lexical` | `lexical::search` (`lci-core/src/lexical.rs:512`) then `map_lexical_matches` (`search.rs:1139`) | `lexical_rank`, `lexical_score`, evidence detail = credited line count | error -> `Failed` (`search.rs:894-902`) | `live`; observed = live tree, mapped onto the snapshot |
| `lsp.symbols` | symbol queries (`search.rs:741-749`), `provider_symbols` (`:1234`) under `lsp_search_budget_ms`, `map_lsp_locations` (`:1259`) | `lsp_rank` | provider failures -> `Failed` or `Partial` with today's warning; over-budget -> `Partial` with today's note (`:813-835`) | `live` |
| `prior.source_role` | `classify_source_role` (`lci-core/src/filter/source_role.rs:105`), `source_role_prior` (`src/store.rs:48`) | prior value | never fails | `live` (path-only) |

Two behaviours must survive the wrap: the "every enabled channel failed" error and the
"empty is fine" cases (`search.rs:907-931`) are decided by the engine from the four
statuses, not by a map; and a disabled channel adds neither a warning nor a note.

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| M-1 | One `query` method for text and seed requests; `contribution` says how the output is used. | Separate `TextMap` and `ExpansionMap` traits (clearer types, more registry code). | S4 freeze. |
| M-2 | `query` returns `MapResponse`, never `Result`; the registry enforces fail-open. | Return `Result` and let the engine convert (simpler impls, but one forgotten `?` breaks C1). | S4. |
| M-3 | Fusion order fixed: three legacy maps, then others sorted by `MapId`, then priors. | Order by a declared `fusion_slot` number. | S2 (T0 test). |
| M-4 | Non-chunk hits are projected to chunks before fusion in the MVP. | Fuse over all node kinds and return mixed results (needs a response shape change). | S5. |
| M-5 | petgraph `DiGraph` over interned ids, as a direct dependency of `lci-maps`. | An in-house CSR adjacency (the default of an earlier slice plan, not in the repository); taken only if petgraph shows up in a profile. | First graph map (W1b). |
| M-6 | `MapRequest.params` is untyped JSON validated by each map. | A typed enum of all map parameters (compile-time checks, but every new map edits a shared enum, which AGENTS.md asks to avoid in parallel work). | S4. |
| M-7 | A map's LanceDB table, if any, is `ws_<16hex>_<map>` under the same `data_dir` (architecture-map.md, 9.3). | One wide table with nullable columns per map. | First stored map (W1b). |
| M-8 | Index-time map failures never fail the core index; an optional map's identity change is a stale map, not the hard error reserved for the embedding identity (`search.rs:1105-1107`). | none considered | S1. |

## 5. Not decided here

The traversal loop and yield points (in [steering-ops.md](steering-ops.md) and
[run-record.md](run-record.md)), card contents ([map-card.md](map-card.md)), and the contract
tests ([contract-kit.md](contract-kit.md)).

## 6. How a test proves it

| Property | Test |
| --- | --- |
| Bit-exact legacy fusion (T0) | Run the existing fusion unit cases (`search.rs:1589-1845`, for example `unit_weights_match_plain_rrf`, and `:2103-2174`, `traced_ranks_weights_and_prior_reproduce_every_fusion_score_bit_for_bit`) through the registry with the four wrapped maps: every `fusion_score` equal by `to_bits()`. Then replay stored candidate dumps with `lci replay`: zero refused queries. |
| Response unchanged | Golden JSON of `search_code` for the offline fixture workspace before and after the wrap: byte-identical (field order included). |
| Fail-open per map | A fake map that returns `Failed`, one that panics, one that sleeps past its deadline: the search returns the other maps' fusion, with one warning (failure, panic) or one note (deadline) naming the map (contract C1, C8). |
| Disabled is silent | With each `[channels]` switch off in turn: no warning, no note, zero timing for that map (as today). |
| Filter before fusion | A fake expansion map that returns nodes outside the filter: none reaches fusion, the rerank request (fake reranker records its documents) or the results (contract C2). |
| Order independence of registration | Registering the extra maps in a different order gives the same fused scores (the loop sorts by `MapId`). |
| Status to error mapping | All enabled maps `Failed` -> the same error text as today; semantic off and lexical `Ok` with no hits -> empty report, no error (`search.rs:907-931`). |

## 7. S2 as built (2026-10-12)

Code: crate `lci-maps` (`lci-maps/src/lib.rs`: types, `Map`, `Registry`, fusion;
`lci-maps/src/kit.rs`: contract kit C1 and C6), the wrapped legacy maps in
`src/app/search/legacy_maps.rs`, T0 in `src/app/search/tests/t0.rs` and two integration
tests in `tests/cases/indexing.rs` (golden `search_code` JSON, registry against a traced
search). The production search does not call the registry. Line references re-checked on
`66f999d`: `search_once` `src/app/search.rs:653`, fan-out `:732`, `map_lexical_matches`
`:1159`, `map_lsp_locations` `:1269`, `fuse_hits` `:1307` (key `:1314`), `fusion_order`
`:337`, `source_role_prior` `src/store.rs:52`, `Store::search` `:335`.

Changes to this spec, each the conservative choice where one was open:

- **`Partial` carries an optional note and an optional warning** (was `Partial { note }`).
  Today's LSP channel can keep the answers of one language server while another failed,
  which is a warning with hits; none of the four v0 statuses could carry both. `Failed`
  still drops its hits.
- **The wrapped lexical and LSP maps live in the root crate**, not in `lci-maps` (3.1): the
  code they call (`App::map_lexical_matches`, `App::provider_symbols`,
  `App::map_lsp_locations`, `Hit::source_role_prior`) is there, and moving it would change
  the production path. `LspMap` repeats the 40-line provider orchestration of `search_once`
  (symbol queries, per-provider budget, query-by-query merge) because that arm is inline;
  the integration test that compares the registry with a traced search guards against drift.
- **No petgraph yet.** No graph map exists in S2; M-5 stands for the first graph map (W1b).
- **Filter maps are refused at registration** (`RegisterError::FilterUnsupported`) until
  contract C2 lands; a filter that is silently skipped would be worse.
- **Priors are asked about the fused nodes**: after the rank lists are fused, every
  `Prior` map gets the fused nodes as seeds and returns one hit per node with its value in
  `score`, added unweighted. Rank lists run side by side (as the `tokio::join!` today),
  priors after them.
- **Registry API**: `Registry::register(Box<dyn Map + 'a>, weight, limit)` keeps entries
  in fusion order whatever the registration order; `set_enabled(id, false)` records the call
  as `Skipped` (silent); `run(text, budget, view)` returns every call and the fused pool.
  `Run::warnings()` and `Run::notes()` attribute each message to its map id (the "naming"
  in contract C1 is structural; the wrapped maps keep today's texts, which do not contain
  the id).
- **The registry checks every answer** and turns a bad one into `Failed` naming the map:
  a node outside the view (or, for a prior, outside its seeds), a node twice, a rank of 0
  or a falling rank (ties are allowed: the fusion unit cases give three hits rank 1), a
  non-finite score, a prior without a value, more hits than the limit. A panic is `Failed`;
  a missed deadline is `Partial` with a note.
- `Need` gains `Ripgrep`; `Lsp` names no provider. `EdgeKind` has `QueryMatch` and `Prior`
  only. `SnapshotView` holds the snapshot reference and the filtered universe with interned
  NodeIds; the `EffectiveFilter` and config are not in it yet (C2).
- A legacy hit that is not a chunk of the view is left out of a wrapped map's answer
  (`map_hits`); with the same snapshot and filter none is. The N-5 twins (one legacy key,
  two NodeIds) would fuse as two entries in the registry and one in `fuse_hits`; no fixture
  and no snapshot of this repository's last 201 commits has one (`../s1-stability.md`).

Section 6 status: bit-exact legacy fusion: done (36 weighting cases of the four row sets,
the merge, alone and tie cases, by `to_bits()`, plus the live end-to-end comparison). Stored
candidate dumps: none is committed in the repository (dumps go to `test-results/`), so the
replay part could not run; the end-to-end test compares with a traced search's pool
instead. Response unchanged: done (golden JSON written before `lci-maps` existed, unchanged
after). Fail-open per map: done through the kit (failed, panic, deadline, foreign node,
NaN). Disabled is silent: done at registry level (kit, "switched off"); the timing part
waits for an engine. Order independence of registration: done. Filter before fusion and
status-to-error mapping: deferred, they need C2 and the engine (S4).
