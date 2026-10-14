# Multimap traversal: interface specs (v0, pre-freeze)

Written 2026-10-12T10:10Z (`date -u`) at slice S0, when no code existed for anything
specified here. Since then parts are built (not frozen): NodeId (S1, node-id.md section 7),
the Map trait, registry and kit C1/C6 (S2, map-trait.md 7, contract-kit.md 6), and the
extraction hook, bundle sidecar, `sym.defs` and kit C3/C4 (S3a, node-id.md 8, map-trait.md
8, contract-kit.md 7). Each "as built" section says what changed against the design text
above it; RunRecord, steering ops and map cards are still design only. Every spec is **v0,
pre-freeze**. The Map trait and the types it touches
freeze at the S4 gate, after the first end-to-end slice (lexical map plus a symbols map
through NodeId, extraction and the harness); until then signatures may change, while the
properties in each spec's "How a test proves it" should not.

Each spec separates **Read** (stated by the code or docs at the cited tree) from **Inferred**
(designed or concluded here), ends with "Open decisions" (the choice taken, its alternative
and when to revisit it) and "How a test proves it".

## Index

| Spec | Decides | Builds on (main code) |
| --- | --- | --- |
| [node-id.md](node-id.md) | One identity space for file, chunk, symbol, commit, test, config key, doc section and string literal; canonical strings; stability across edits. Chunk = `(path, content_hash, ordinal)`, no `start_line`. | `Chunk` (`lci-core/src/chunk.rs:43`), `chunks_with_adapter` (`:144`), `CachedFile` and `Manifest` (`lci-core/src/manifest.rs:12`, `:20`), fusion key (`src/app/search.rs:1304`) |
| [map-trait.md](map-trait.md) | The `Map` trait (descriptor, request, response with status, cost and freshness), the fusion loop that keeps today's f32 order, how the three channels and the source-role prior are wrapped; petgraph for the graph projection. | `search_once` (`src/app/search.rs:637`), `fuse_hits` (`:1297`), `map_lexical_matches` (`:1139`), `map_lsp_locations` (`:1259`), `Store::search` (`src/store.rs:331`) |
| [contract-kit.md](contract-kit.md) | Contracts C1-C9 as testable properties, with an applicability table and tests for the kit itself. | fail-open paths (`search.rs:813-931`, `:1392-1406`), filters (`:435`, `:690`, `:940`, `:998`), replay (`src/evaluate/replay.rs:406`) |
| [run-record.md](run-record.md) | JSON Lines trace per traversal (`run`, `step`, `end`) with a JSON Schema; privacy rules. | `CandidateDumpLine` (`search.rs:116`), `CandidateTrace` (`:256`), `Timings` (`:29`), `Snapshot` (`src/store.rs:76`) |
| [steering-ops.md](steering-ops.md) | Ops v1 (pause, prune, expand, add_map, drop_map, reweight) as validated all-or-nothing patches on a resumable, endpoint-bound handle; three MCP tools with REST mirrors. | `McpServer` tool router (`src/server.rs:89`), per-endpoint services (`:224`), REST routes (`src/rest.rs:262`), `App.watchers` (`src/app/mod.rs:50`), `IndexResolution` (`search.rs:521`) |
| [map-card.md](map-card.md) | Card schema (cost, coverage, freshness, good_for, contracts) with M/A/G labels, written only by the harness from RunRecords. | `Timings`, `src/evaluate/metrics.rs:50`, `:397`, `src/evaluate/compare.rs:221` |

Related docs in the parent directory: [../lessons.md](../lessons.md) (lessons L1-L21 that the
specs cite), [../maps-inventory.md](../maps-inventory.md), [../architecture-map.md](../architecture-map.md),
[../ideas.md](../ideas.md).

## Base tree and line drift

All `path:line` references in the specs point at the spike head `df7faaa`, whose code equals
`7f9c636`. `origin/fix/verified-bugs` (at `3b8d306` when this was written, 43 commits ahead
of that base) changes `lci-core/src/chunk.rs`, `src/app/search.rs`, `src/store.rs`,
`lci-core/src/lsp.rs` and `src/app/mod.rs`, and also `lci-core/src/config.rs`,
`lci-core/src/models.rs`, `lci-core/src/lsp/adapter.rs`, `src/app/indexing.rs` and
`src/app/readiness.rs` (the reviewer checked that none of the lines the specs cite in those
five files move); `lci-core/src/manifest.rs`, `src/server.rs` and
`src/rest.rs` are unchanged. Main items on both trees (Read, by grep of each tree):

| Item | Spike `df7faaa` | Fix branch `3b8d306` |
| --- | --- | --- |
| `Scan`, `Chunk` | `chunk.rs:15`, `:43` | `:15`, `:43` |
| `chunks_with_adapter` | `chunk.rs:144` | `:159` |
| `sources` | `chunk.rs:204` | `:225` (plus `sources_with_read` `:229`) |
| `fingerprint_material` | `chunk.rs:251` | `:293` |
| `scan_incremental` | `chunk.rs:270` | `:312` |
| `SearchReport`, `CandidateTrace`, `fusion_order` | `search.rs:46`, `:256`, `:337` | `:46`, `:256`, `:337` |
| `hit_matches`, `cap_by_file`, `IndexResolution` | `search.rs:435`, `:476`, `:521` | `:435`, `:476`, `:521` |
| `search_in_profile` | `search.rs:583` | `:599` |
| `search_once` | `search.rs:637` | `:653` |
| fan-out `tokio::join!` | `search.rs:712` | `:732` |
| `resolve_index_for_search` | `search.rs:1022` | `:1042` |
| identity-change error (`manual`/`watch`) | `search.rs:1105-1107` | `:1125-1127` |
| `map_lexical_matches` | `search.rs:1139` | `:1159` |
| `map_lsp_locations` | `search.rs:1259` | `:1270` |
| `fuse_hits` (key) | `search.rs:1297` (`:1304`) | `:1308` (`:1315`) |
| `rerank_into_report` | `search.rs:1366` | `:1377` |
| `Hit`, `source_role_prior`, `bounded_snippet` | `store.rs:27`, `:48`, `:63` | `:31`, `:52`, `:67` |
| `Snapshot`, `vector_key` | `store.rs:76`, `:93` | `:80`, `:97` |
| `Store::replace`, `search`, `chunks` | `store.rs:220`, `:331`, `:400` | `:224`, `:335`, `:421` |
| `lsp::Location` | `lsp.rs:47` | `:44` |
| `WorkspaceCoordination`, `App`, `watchers` | `app/mod.rs:31`, `:43`, `:50` | `:31`, `:43`, `:50` |

Fix-branch changes that touch these specs (Read, from commit subjects): `83e1164` drops one
leading byte order mark before hashing (chunk hashes then ignore a BOM); `96e8b73` keeps
backslashes in Unix file names (path normalisation, see node-id.md N-7); `6c3fa84` resolves
ripgrep paths by hash lookup in `map_lexical_matches`; `25f2ca5` returns early from
`search_once` when no chunk passes the filter. None changes a decision here; all change line
numbers, which S1 refreshes after the merge.

## Review

An independent, read-only review by a fresh subagent that did not write the specs ran
against the code and the brief; findings received 2026-10-12T10:18Z (`date -u`). It
verified most `path:line` citations and all 24 drift-table rows against both trees. It found
no blockers, 9 should-fix and 9 nit findings. All were addressed in the commit that adds this
section:

| # | Severity | Finding | Resolution |
| --- | --- | --- | --- |
| 1 | should-fix | C6 broke ties by NodeId string; legacy order uses path, `start_line`, `end_line`, hash (`search.rs:337-344`), so equal-score chunks would reorder. | C6 and map-trait 3.4 now keep `fusion_order` for chunk ties; NodeId order only for non-chunk ties. |
| 2 | should-fix | C1 "never turns success into an error" contradicted today's "every enabled channel failed" error (`search.rs:907-931`), which map-trait's test keeps. | C1 now states that carve-out as engine-owned and compares with the M-removed run only when another map returned candidates. |
| 3 | should-fix | C1 said a deliberate skip adds a note; today a disabled channel is silent. | C1: a skip is silent; only `Partial` adds a note. |
| 4 | should-fix | C1's "sleep past deadline" mode would be `Partial` under C8. | The injector emits nothing before the deadline; partial answers are tested under C8. |
| 5 | should-fix | NodeId line-independence test inserted a comment, which attaches to the next chunk as a prefix (`language.rs:299-302`). | Test inserts blank lines only; the reason is stated. |
| 6 | should-fix | The fake reranker does not record document paths. | C2 now says the fake must be extended to record each document's first line. |
| 7 | should-fix | C3 test ignored the 10 s staleness throttle and claimed old answers for the live lexical map. | Test sets `stale_check_interval_seconds = 0`, leaves the watcher off, and scopes old-snapshot answers to stored maps; live maps report `observed = live-tree`. |
| 8 | should-fix | "The draft plan" is not in the repository. | Reworded to "the slice plan for this spike, which is not in the repository", with in-repo grounding named. |
| 9 | nit | Freshness and evidence fields differed between map-trait and run-record. | Aligned: `stale` flag in both, snapshot per step; RunRecord hits carry an `evidence` array (`via`, `edge`, `label`, `detail`). |
| 10 | nit | BOM row said every chunk gets a new id. | Only the first chunk on the spike tree. |
| 11 | nit | `derived_from: scip` is not an existing gold-set field. | Marked as proposed. |
| 12 | nit | `reweight` after Y3 left a stale rerank order. | The next continue re-runs cap, cut and rerank. |
| 13 | should-fix | Wrong line for the bit-exact fusion test. | Cited `search.rs:2103-2174`. |
| 14 | nit | petgraph is also under `prost-build`. | "only" dropped, both paths named. |
| 15 | nit | C3 identity-error lines. | `search.rs:1105-1108`. |
| 16 | nit | Snippet is 400 bytes plus a 3-byte ellipsis. | Corrected. |
| 17 | nit | Fix branch also changes five more cited files. | Listed; their cited lines do not move. |
| 18 | nit | Parking rule thresholds are a decision. | Added as open decision P-1 in lessons.md. |

What the reviewer checked and found correct: base commits and the 43-commit gap; every
drift-table row; the cited lines in chunk.rs, manifest.rs, workspace.rs, store.rs,
search.rs, app/mod.rs, server.rs, rest.rs and the evaluation modules; the factual claims on
fusion order, filter placement, the read guard, `Store::replace`, freshness rules and the
MCP/REST structure; NodeId stability rows (apart from 5 and 10); consistent vocabularies
across specs; lessons' sources; the brief's required sections; no baseline-as-gate wording;
and no machine paths, e-mail addresses, personal names, usage or credit figures or private
gold-set names in the changed files.
