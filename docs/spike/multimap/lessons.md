# Lessons from earlier spikes

Status: REFERENCE. Written 2026-10-12 (slice S0); the notes of the retired recycled-research file were merged in on
2026-10-17, so every number lives next to its lesson. Lessons learned during the v2 spike are LESSON entries in
[../../decisions.md](../../decisions.md); measured results are indexed in [results.md](results.md).

Every failure below is data about the conditions under which maps work together, not a verdict on a map. A lesson names
the condition, what to build or measure because of it, and its source. All numbers come from earlier internal research
on one Rust repository (this one, at one commit) that is not in this repository; they are undated, not reproducible
from this repository alone, and are conditions for the synergy study, not verdicts on any map.

Labels: **Read** means the source states it. **Inferred** means this file draws the consequence for the spike; check
it before relying on it. Contract ids C2 to C7 are defined in [specs/contract-kit.md](specs/contract-kit.md); the
engine facts named in the Source column are in [engine.md](engine.md) and
[../../architecture.md](../../architecture.md). The earlier research's map and merge ideas to test are in
[ideas.md](ideas.md), "Combinations to test together", item D.

Where the sections of the retired recycled-research file went (other docs cite them by their old names): "Freshness" and
"What was recycled" are "Identity and freshness" (L1 to L5); "Conditions learned from failed experiments" are "Edges and their
sources" (L6, L7) and "Where graph context helps and where it hurts" (L9 to L13); "Gold sets" is L8; "Method to reuse" is "Method"
(L19); "Storage note" is L20; "Overlap with the spike's novel list" is L21; "Map and merge ideas worth testing" is in
[ideas.md](ideas.md).

## Identity and freshness

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L1 | A stable per-declaration key (language, module path, container chain, name, kind, disambiguator) matched the compiler's SCIP symbols 1:1 on 2,440 declarations and collided on 0 of 1,358 keys here. Keys that omit the impl header would collide for 3-5% of declarations in unlike code. (Read) | The symbol NodeId keeps the container chain including the impl header (see [specs/node-id.md](specs/node-id.md)); one NodeId space with this kind of key; a binding from key to content is kept beside the key. (Inferred) | earlier internal research, identity |
| L2 | Per-file derived data keyed by (blob hash, producer) with no paths inside matched a full rebuild at every one of 114 replayed commits (publish p50 12-19 ms). Bundles that embedded paths failed on a commit that moved 18 files. (Read) | Parse-time sidecars are path-free and keyed by `(content_hash, language, adapter_version, producer)`; paths live only in the manifest. The full-rebuild equivalence auditor is contract C4 and a per-map contract test. (Inferred) | earlier internal research, freshness |
| L3 | Reverse indexes (name to referrers, path to referrers) had to be resident state, not per-file data. (Read) | Global maps declare `update = global` and rebuild their reverse index from the per-file bundles, never store it inside a bundle. (Inferred) | earlier internal research, freshness; [engine.md](engine.md), maps and update models |
| L4 | The fusion key includes `start_line`, so an edit above a chunk changes its key although its content hash is unchanged. (Read) | NodeId for a chunk drops `start_line`: `(path, content_hash, ordinal)`. Line ranges become attributes, not identity. (Inferred) | [engine.md](engine.md), chunk identity |
| L5 | No syntax tree, declaration, signature or edge survives a scan; an unchanged file is not parsed at all. (Read) | Parse-derived maps need an extraction hook inside `chunks_with_adapter` and a per-file cache, or a reparse. (Read) | [engine.md](engine.md), what persists between index passes |

## Edges and their sources

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L6 | Tree-sitter tags as a call/reference backbone had recall 0.12 and precision 0.88 (an owned query reached 0.23 / 0.93). False positives were method calls on external types. (Read) | Every edge carries a source label (`tags`, `scip`, `lsp`, `heuristic`), and benchmark rows split by label (contract C5). A tags-only call map is a cheap, imprecise map, not a failed one. (Inferred) | earlier internal research, failed experiments |
| L7 | For Rust edits, `textDocument/semanticTokens` plus `definition` on function and method tokens refreshed a file's outgoing call edges at p95 0.74 s with recall 1.00 against SCIP; `callHierarchy` alone missed 16% of call pairs (94% of its misses were inside macro arguments). A warm rust-analyzer held 3.5 GB and took 1-2 minutes to start. (Read) | Precise edges are a live, per-file, opt-in map with its cost on its map card; the traversal must not wait for a cold server (the existing `lsp_search_budget_ms` pattern). (Inferred) | earlier internal research, precise edges |
| L8 | The gold sets are derived from a SCIP index of one frozen commit (`bfaff44`, tag `eval/bfaff44`): seven Rust gold sets, about 630 items, covering flow paths, impact, where-to, architecture, history, instructions and agent tasks. Maps built from the same SCIP data would be circular against them. No gold exists for other languages. (Read) | A map whose source label is `scip` is reported but excluded from claims scored on SCIP-derived gold (C5 plus the harness leak scan). Current sets: [evaluation.md](evaluation.md), Sets. (Inferred) | earlier internal research, gold sets |

## Where graph context helps and where it hurts

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L9 | Graph-built cards as cross-encoder input lost 0.03-0.07 MRR; the same graph context helped readers of cards (interaction +0.05 nDCG@10 mined, +0.11 core). (Read) | The reader type is a condition. Graph evidence goes to the agent (frontier packet), not into the reranker document, unless a benchmark row shows otherwise. (Inferred) | earlier internal research, failed experiments |
| L10 | Vector-neighbour hops did not find co-edited partners (0 of 94 in the top 8 neighbours); gain over a size-matched random list was +8 points with weak structure and about 0 with precise structure. (Read) | Every expansion map is benchmarked against a size-matched random expansion and a no-graph control (contract C7). (Inferred) | earlier internal research, failed experiments |
| L11 | Co-change lost to a plain churn list when rescuing items missing from a noisy pool (MRR 0.14 vs 0.38), but beat it with clean coupled seeds (+0.09 to +0.15 MRR). (Read) | Seed quality is a condition: co-change is scored separately for clean and noisy seeds, always next to a churn control (C7). Always include the churn control. (Inferred) | earlier internal research, failed experiments |
| L12 | Neighbour injection after a reranker and a pseudo-query "no embedder" mode failed against no-graph controls; the fast end was bound by the real query embedding. (Read) | Expansion happens before the reranker and before the final filter (contract C2), and a mode that skips the query embedding is not a latency win to promise. (Inferred) | earlier internal research, failed experiments |
| L13 | Query shape mattered in every run: keyword vs descriptive queries, short vs long queries, files that every commit touches (entry points, test harnesses). (Read) | RunRecord carries query-shape tags, and every table breaks results down by them; hub files are flagged on map cards. (Inferred) | earlier internal research, failed experiments; [engine.md](engine.md), miner tags |

## Pipeline invariants that a map can break

| # | Lesson (condition) | Consequence for the spike | Source |
| --- | --- | --- | --- |
| L14 | The f32 summation order of fusion is part of the dump contract, and replay refuses a dump it cannot reproduce bit for bit. (Read) | The map registry adds the three legacy channels first, in the order semantic, lexical, LSP; a bit-exact replay test (T0) guards it. (Read) | [engine.md](engine.md), seams and invariants; `CandidateTrace` |
| L15 | A search holds the workspace read guard for its whole run, and `Store::replace` overwrites the table, so a paused traversal cannot hold the guard or pin old rows. (Read) | A resumable handle re-acquires the guard per step and checks snapshot drift (`indexed_at` plus manifest fingerprint); drift is reported, never hidden. (Inferred) | [engine.md](engine.md), seams and invariants |
| L16 | Filters run inside channels, after fusion and after rerank; a map that adds chunks must filter them before they reach the reranker. (Read) | Contract C2 (filter before rerank) applies to every step that adds nodes. (Read) | [engine.md](engine.md), seams and invariants |
| L17 | `rg` reads the live tree while chunks come from the snapshot; under `manual` or `watch` line ranges can drift. (Read) | Live maps declare freshness `live` and the snapshot they map onto; a RunRecord step logs both (contract C3). (Inferred) | [engine.md](engine.md), freshness |
| L18 | A co-change map built at HEAD contains the commits the miner turned into gold. (Read) | History maps are built as of each item's base commit, and leak scans are assertions in the harness, not reviews. A last-change subject leaks the answer when the query is a commit message. (Read) | [engine.md](engine.md), seams and invariants; L19 |

## Method

| # | Lesson | Source |
| --- | --- | --- |
| L19 | Pre-register the threshold, include no-graph and random controls, report paired bootstrap intervals, freeze the tree, and make leak scans code assertions. Test interactions with 2x2 arms rather than cumulative arms. (Read) | earlier internal research, method to reuse; [evaluation.md](evaluation.md) |
| L20 | `AGENTS.md` forbids an additional database. A ledger on an embedded KV store won an internal bake-off narrowly (4.3 vs 4.2) over per-file bundles on LanceDB; the spike stays on Lance, files and in-memory projections unless that rule is changed. (Read) | earlier internal research, storage note; `AGENTS.md`, Hard rules |
| L21 | Ideas with prior art are cited, not reinvented: hierarchical routing (road-network style, rejected below about 200k chunks), as-of-commit queries, decayed relevance and PageRank, anytime reranking under a deadline, a workload-driven view advisor, agent-defined traversal programs. Partial prior art: disagreement between sources (as diagnostics), cleared/negative evidence, per-session seen sets. Not found: fork/join traversal for parallel agents, per-map cards, pause-and-steer checkpoints, fog-of-war style region states, flow fields, replay-trained steering on the engine's own traces. (Read) | earlier internal research, overlap with the novel list; [prior-art.md](prior-art.md) |

## Parking log

Rule (tactics only): a tactic is parked, not killed, after two consecutive benchmark runs with no gain over its own
control, or when it cannot pass a contract test. The entry records the condition, so the tactic can be retried when the
condition changes. No baseline is a gate; "its own control" means the C7 control arms for that map.

| Date | Tactic | Condition observed | Runs (RunRecord ids) | Retry when |
| --- | --- | --- | --- | --- |
| 2026-10-12 | Relation-as-rotation embeddings (RotatE, QuatE) as a link-suggestion map | On this Rust repository they tie Tree-sitter tags on held-out edges and lose to cheap heuristics on combined queries; fold-in does not unlearn removed edges. Parked by owner decision (2026-10-12T18:08Z), not by the two-run rule above. | none (experiment P1 of the representation research, not RunRecords: `representations-results.md` on `cloud/spike-repr` at `7fc2e86`) | A language lacks a cheap call parser, or edges must be predicted across repositories |

Open decision (P-1): "two consecutive runs" and "no gain" (a paired 95% interval against the map's own control that
includes zero) are taken here; the alternative is a pre-registered minimum effect per question type. Revisit at the
first parking candidate.
