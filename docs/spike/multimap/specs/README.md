# Multimap traversal: interface specs (v0, pre-freeze)

Written 2026-10-12 at slice S0, before any code existed for them. Since then most of the design is built, none of it frozen. The Map trait and the
types it touches freeze at the S4 gate, after the first end-to-end slice (lexical map plus a symbols map through NodeId, extraction and the harness).
Until then signatures may change; the properties in each spec's "How a test proves it" should not. The shipped engine is described in
[../engine.md](../engine.md); the specs hold the contracts and the reasons.

## Convention

Each spec separates **Read** (stated by the code or docs at the named tree), **Inferred** (designed or concluded in the spec) and **as built**
(checked against the code at the S1 tree and marked built, deferred or not built). A spec ends with "Open decisions" (the choice taken, its
alternative and when to revisit it) and "How a test proves it". Code references are symbol names, never line numbers. Section numbers are cited by
code, the roadmap and frozen preregs: never renumber or rename a section.

## Index

| Spec | Decides | Built | Builds on (symbols) |
| --- | --- | --- | --- |
| [node-id.md](node-id.md) | One identity space for file, chunk, symbol, commit, test, config key, doc section and string literal; canonical strings; stability across edits. Chunk = `(path, content_hash, ordinal)`, no `start_line`. Decisions N-1..N-15. | `NodeId` (S1); extraction hook and bundle store (S3a) | `Chunk`, `chunks_with_adapter` (`lci-core/src/chunk.rs`), `CachedFile`, `Manifest` (`lci-core/src/manifest.rs`), `fuse_hits` key (`src/app/search.rs`) |
| [map-trait.md](map-trait.md) | The `Map` trait (descriptor, request, response with status, cost and freshness), the fusion loop that keeps today's f32 order, how the three search channels and the source-role prior are wrapped; petgraph for the graph projection. M-1..M-8. | trait, `Registry`, `sym.defs` (S2, S3a); channel-map wrapping behind `map-equivalence` | `search_once`, `fuse_hits`, `map_lexical_matches`, `map_lsp_locations` (`src/app/search.rs`), `Store::search` (`src/store.rs`) |
| [contract-kit.md](contract-kit.md) | Contracts C1-C9 as testable properties, with an applicability table and tests for the kit itself. K-1..K-12. | C1, C3, C4, C5, C6 (`lci-maps/src/kit*.rs`, `audit.rs`, `labels.rs`); C2 and C8 not built; C7 and C9 partly or not | fail-open paths and filters in `src/app/search.rs`, `evaluate::replay` (`src/evaluate/replay.rs`) |
| [run-record.md](run-record.md) | JSON Lines trace per traversal (`run`, `step`, `end`) with a JSON Schema; privacy rules; the `run` line's `profile` stamp (run-profile hash, keys, embedding identity, priors and tie-break file hashes). R-1..R-11. | schema, spike-feature writer and offline harness; no production writer | `CandidateDumpLine`, `CandidateTrace`, `Timings` (`src/app/search.rs`), `Snapshot` (`src/store.rs`) |
| [steering-ops.md](steering-ops.md) | Ops v1 (pause, prune, expand, add_map, drop_map, reweight) as validated all-or-nothing patches on a resumable handle; three MCP tools with REST mirrors. S-1..S-8. | only the ladder, `steer` and file-backed handles (section 3.6); the ops, tools and TTL are design | `McpServer` tool router (`src/server.rs`), REST routes (`src/rest.rs`), `App.watchers` (`src/app/mod.rs`), `IndexResolution` (`src/app/search.rs`) |
| [map-card.md](map-card.md) | Card schema (cost, coverage, freshness, good_for, contracts) with M/A/G labels, written only by the harness from RunRecords. D-1..D-5. | only the `MapCard` stub (section 3.5); the file format is design | `Timings`, `query_metrics`, `evaluate::compare` (`src/evaluate/`) |

Related docs in the parent directory: [../lessons.md](../lessons.md) (lessons L1-L21 that the specs cite; the parking rule thresholds are decision
P-1), [../ideas.md](../ideas.md), [../engine.md](../engine.md).

## Retired from this file

The 2026-10-12 table of line numbers on the spike head `df7faaa` against the fix branch `3b8d306`, and the table of the 18 findings of the S0 review
(9 should-fix, 9 nit; all addressed in the specs), are retired: line numbers are replaced by symbol names, and every finding's resolution is part
of the spec text it changed. The decision log keeps the history.
