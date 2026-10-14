# Lessons from earlier spikes

Status: v0, written at slice S0 (2026-10-12T10:01Z, `date -u`). Docs only.

Every failure below is data about the conditions under which maps work together, not a
verdict on a map. A lesson names the condition, what to build or measure because of it, and
its source document in this directory. Numbers come from one Rust repository at one commit
and are not reproducible from this repository alone (see the status line of
[recycled-research.md](recycled-research.md)).

Labels: **Read** means the source document states it. **Inferred** means this file draws the
consequence for the spike; check it before relying on it.

## Identity and freshness

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L1 | A per-declaration key (language, module path, container chain, name, kind, disambiguator) matched compiler SCIP symbols 1:1 and did not collide here. Keys that drop the impl header collide for a few percent of declarations in unlike code. (Read) | The symbol NodeId keeps the container chain including the impl header (see [specs/node-id.md](specs/node-id.md)). A binding from key to content is kept beside the key. (Inferred) | recycled-research.md, "What was recycled", Identity |
| L2 | Per-file derived data keyed by (blob hash, producer) with no paths inside matched a full rebuild at every replayed commit; bundles that embedded paths failed on a commit that moved many files. (Read) | Parse-time sidecars are path-free and keyed by `(content_hash, language, adapter_version, producer)`; paths live only in the manifest. The full-rebuild equivalence auditor is contract C4. (Inferred) | recycled-research.md, "What was recycled", Freshness |
| L3 | Reverse indexes (name to referrers, path to referrers) had to be resident state, not per-file data. (Read) | Global maps declare `update = global` and rebuild their reverse index from the per-file bundles, never store it inside a bundle. (Inferred) | recycled-research.md, Freshness; maps-inventory.md, "Update model vocabulary" |
| L4 | Today's fusion key includes `start_line`, so an edit above a chunk changes its key although its content hash is unchanged. (Read) | NodeId for a chunk drops `start_line`: `(path, content_hash, ordinal)`. Line ranges become attributes, not identity. (Inferred) | architecture-map.md, section 2 "Chunk identity" and 9.1 |
| L5 | No syntax tree, declaration, signature or edge survives a scan; an unchanged file is not parsed at all. (Read) | Parse-derived maps need an extraction hook inside `chunks_with_adapter` and a per-file cache, or a reparse. (Read, maps-inventory.md) | maps-inventory.md, "What persists between index passes"; architecture-map.md, 9.3 |

## Edges and their sources

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L6 | Tree-sitter tags as a call/reference backbone had low recall and high precision; false positives were method calls on external types. (Read) | Every edge carries a source label (`tags`, `scip`, `lsp`, `heuristic`), and benchmark rows split by label (contract C5). A tags-only call map is a cheap, imprecise map, not a failed one. (Inferred) | recycled-research.md, "Conditions learned from failed experiments" |
| L7 | `semanticTokens` plus `definition` refreshed a file's call edges with full recall against SCIP within a second at p95; `callHierarchy` alone missed a sixth of call pairs, mostly inside macro arguments. A warm language server held several GB and took minutes to start. (Read) | Precise edges are a live, per-file, opt-in map with its cost on its map card; the traversal must not wait for a cold server (the existing `lsp_search_budget_ms` pattern). (Inferred) | recycled-research.md, "What was recycled", Precise edges |
| L8 | Gold sets were derived from a SCIP index of one frozen commit; maps built from the same SCIP data would be circular against them. (Read) | A map whose source label is `scip` is reported but excluded from claims scored on SCIP-derived gold (C5 plus the harness leak scan). (Inferred) | recycled-research.md, "Gold sets" |

## Where graph context helps and where it hurts

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L9 | Graph-built cards fed to a cross-encoder lost a few hundredths of MRR; the same graph context helped readers of the cards. (Read) | The reader type is a condition. Graph evidence goes to the agent (frontier packet), not into the reranker document, unless a benchmark row shows otherwise. (Inferred) | recycled-research.md, "Conditions learned" |
| L10 | Vector-neighbour hops did not find co-edited partners; their gain over a size-matched random list was small with weak structure and about zero with precise structure. (Read) | Every expansion map is benchmarked against a size-matched random expansion and a no-graph control (contract C7). (Inferred) | recycled-research.md, "Conditions learned" |
| L11 | Co-change lost to a plain churn list when rescuing items missing from a noisy pool, but beat it with clean coupled seeds. (Read) | Seed quality is a condition: co-change is scored separately for clean and noisy seeds, always next to a churn control (C7). (Inferred) | recycled-research.md, "Conditions learned" |
| L12 | Neighbour injection after the reranker and a pseudo-query "no embedder" mode failed against no-graph controls; the fast end was bound by the real query embedding. (Read) | Expansion happens before the reranker and before the final filter (contract C2), and a mode that skips the query embedding is not a latency win to promise. (Inferred) | recycled-research.md, "Conditions learned" |
| L13 | Query shape mattered in every run: keyword versus descriptive, short versus long, and files that every commit touches. (Read) | RunRecord carries query-shape tags, and every table breaks results down by them; hub files are flagged on map cards. (Inferred) | recycled-research.md, "Conditions learned"; architecture-map.md, 8 (miner tags) |

## Pipeline invariants that a map can break

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L14 | The f32 summation order of fusion is part of the dump contract, and replay refuses a dump it cannot reproduce bit for bit. (Read) | The map registry adds the three legacy channels first, in the order semantic, lexical, LSP; a bit-exact replay test (T0) guards it. (Read, architecture-map.md 9.1) | architecture-map.md, 9.1 Risk; 8 Replay |
| L15 | A search holds the workspace read guard for its whole run, and `Store::replace` overwrites the table, so a paused traversal cannot hold the guard or pin old rows. (Read) | A resumable handle re-acquires the guard per step and checks snapshot drift (`indexed_at` plus manifest fingerprint); drift is reported, never hidden. (Inferred) | architecture-map.md, 5 and 9.2 Risk |
| L16 | Filters run inside channels, after fusion and after rerank; a map that adds chunks must filter them before they reach the reranker. (Read) | Contract C2 (filter before rerank) applies to every step that adds nodes. (Read) | architecture-map.md, 9.0 |
| L17 | `rg` reads the live tree while chunks come from the snapshot; under `manual` or `watch` line ranges can drift. (Read) | Live maps declare freshness `live` and the snapshot they map onto; a RunRecord step logs both (contract C3). (Inferred) | architecture-map.md, 4.2 |
| L18 | A co-change map built at HEAD contains the commits the miner turned into gold. (Read) | History maps are built as of each item's base commit, and leak scans are assertions in the harness, not reviews. (Read, recycled-research.md "Method") | architecture-map.md, 9.5 Risk; recycled-research.md, "Method to reuse" |

## Method

| # | Lesson | Source |
| --- | --- | --- |
| L19 | Pre-register thresholds, include no-graph and random controls, report paired bootstrap intervals, freeze the tree, and test interactions with 2x2 arms rather than cumulative arms. (Read) | recycled-research.md, "Method to reuse" |
| L20 | An embedded KV ledger beat per-file bundles on LanceDB only narrowly in an internal bake-off; the project rules forbid an additional database. Stay on Lance, files and in-memory projections. (Read) | recycled-research.md, "Storage note"; AGENTS.md, Hard rules |
| L21 | Ideas with prior art are cited, not reinvented: hierarchical routing (rejected below about 200k chunks), as-of-commit queries, decayed relevance and PageRank, anytime reranking under a deadline, a workload-driven view advisor. (Read) | recycled-research.md, "Overlap with the spike's novel list" |

## Parking log

Rule (tactics only): a tactic is parked, not killed, after two consecutive benchmark runs with
no gain over its own control, or when it cannot pass a contract test. The entry records the
condition, so the tactic can be retried when the condition changes. No baseline is a gate;
"its own control" means the C7 control arms for that map.

| Date (`date -u`) | Tactic | Condition observed | Runs (RunRecord ids) | Retry when |
| --- | --- | --- | --- | --- |
| 2026-10-12 | Relation-as-rotation embeddings (RotatE, QuatE) as a link-suggestion map | On this Rust repository they tie Tree-sitter tags on held-out edges and lose to cheap heuristics on combined queries; fold-in does not unlearn removed edges. Parked by owner decision (2026-10-12T18:08Z), not by the two-run rule below. | none (experiment P1 of the representation research, not RunRecords: `representations-results.md` on `cloud/spike-repr` at `7fc2e86`) | A language lacks a cheap call parser, or edges must be predicted across repositories |

Open decision (P-1): "two consecutive runs" and "no gain" (a paired 95% interval against the
map's own control that includes zero) are taken here; the alternative is a pre-registered
minimum effect per question type. Revisit at the first parking candidate.
