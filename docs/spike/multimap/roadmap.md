# Multimap spike roadmap (cloud-optimised)

Written 2026-10-12 (`date -u` 11:07Z) for the cloud run that builds S1 and S2. Status:
plan, not a result. The slices follow from the v0 specs in [specs/](specs/README.md); where a
spec does not name a slice, the assignment below is **Inferred** and says so.

Owner decision (2026-10-12): S1 and S2 are built now, ahead of the agent A/B (I-10 in
`docs/development/roadmap.md`, "Strategy checkpoint"), and not as throwaway. Both are
additive: the production search path does not change, so they can merge as infrastructure
or sit unused whatever the A/B decides. Every later slice waits for the A/B unless its row
says otherwise.

Where a slice can run:

- **cloud**: no GPU, no model services, fake model service and fake LSP only
  (`tests/support`); `cargo xtask test --suite Full` is the gate.
- **owner**: needs the owner's machine: a live embedder or reranker on 8766/8767, the agent
  A/B, or a gold-set evaluation (live retrieval numbers).

A/B branches (from the roadmap's "Long-range map"): **STOP** (LCI does not help agents:
narrow to a fast fusion-only default, navigation ergonomics and robustness; shelve the map),
**CONTINUE** (overall or in a stratum: phase 2 measures maps and interfaces), **INCONCLUSIVE**
(extend the tasks before deciding).

## Slices

| Slice | Goal | Main files | Where | Waits for A/B | Tonight |
| --- | --- | --- | --- | --- | --- |
| S0 | v0 specs, lessons, inventory | `docs/spike/multimap/` | cloud | no | done (cloud/spike-s0) |
| S1 | `NodeId`: canonical strings, chunk ids `(path, content_hash, ordinal)`, symbol keys with the impl header; derived from scan output; stability measured on this repository's history | `lci-core/src/node.rs`, an `lci-core` example, `docs/spike/multimap/s1-stability.md` | cloud | no (owner override) | **yes** |
| S2 | `lci-maps` crate: `Map` trait, request/response/cost/freshness, registry with fail-open, fusion loop; the three legacy channels and the source-role prior wrapped as maps; T0; contract-kit skeleton | `lci-maps/`, a wrapper module beside `src/app/search.rs` | cloud | no (owner override) | **yes** |
| S3 | Extraction hook beside `chunks_with_adapter`, path-free per-file bundle sidecar, first stored map (`sym.defs`, declarations), RunRecord writer; contract kit C3, C4 (commit-replay auditor), C5 | `lci-core/src/chunk.rs` (hook), `lci-maps/` | cloud | no for the hook and `sym.defs`; RunRecord writer only on CONTINUE (Inferred) | no |
| S4 | First end-to-end slice and trait freeze: lexical map plus `sym.defs` through NodeId, extraction and the harness; offline harness scoring of RunRecords | `lci-maps/`, `src/evaluate/` | cloud for the offline harness; owner for live rows | yes | no |
| S5 | Traversal engine with yield points and resumable handles; `traverse_*` MCP tools and REST mirrors; `node_id` in `CandidateTrace`; non-chunk results (M-4) | `src/app/`, `src/server.rs`, `src/rest.rs` | cloud for offline tests; owner for agent studies | yes | no |
| S6 (Inferred: no spec names it) | Measured map cards from RunRecords; C7 controls; the first expansion map (W1b) against its own no-graph and random controls | `docs/spike/multimap/cards/`, `lci-maps/` | owner (gold sets, live models) | yes, CONTINUE only | no |

## Acceptance (from each spec's "How a test proves it")

- **S1** (node-id.md section 6): round trip and injectivity for every kind; line
  independence (blank lines only); determinism (empty versus full previous manifest,
  shuffled input order); incremental equals full over a scripted edit sequence; locality;
  ordinal rule; symbol key with impl header against a deliberately reduced key; legacy
  mapping with the N-5 cases enumerated. Deferred to S3: "no paths in bundles" (no bundle
  store exists before the sidecar). Plus the stability measurement over up to 200 commits.
- **S2** (map-trait.md section 6, contract-kit.md section 5): T0, every existing fusion unit
  case through the registry with `fusion_score` equal by `to_bits()`; golden `search_code`
  JSON on the offline fixture byte-identical before and after; stored candidate dumps
  replayed with zero refusals if any are in the repository; fail-open per map (failed,
  panic, deadline); disabled is silent; order independence of registration; status to error
  mapping; contract kit C1 and C6 with a known-bad map the kit must reject.
- **S3**: C4 auditor equal at every replayed commit (one move, one adapter bump, one
  delete); a moved file reuses its bundle unchanged; C3 under all three freshness modes;
  C5 label vocabulary validated on every response; RunRecord schema validity, crash
  safety, privacy and fail-open writer (run-record.md section 5).
- **S4**: RunRecord one-shot equivalence with `search_code`; replay of `fuse` steps bit for
  bit; trait and RunRecord schema frozen at `/1`.
- **S5**: steering-ops.md section 5 (one-shot and pause/resume equivalence, no guard held
  across calls, drift, atomic validation, stale seq, filter cannot widen, endpoint binding,
  TTL and caps, reweight replay, data is not instructions).
- **S6**: map-card.md section 5 (schema, provenance, no private leakage, version guard,
  budget use, reference rows) and contract C7 refusals.

## Purpose under each A/B outcome

| Slice | STOP | CONTINUE | INCONCLUSIVE |
| --- | --- | --- | --- |
| S1 | Symbol addressing for navigation ergonomics (a stable name for a declaration); chunk ids that survive edits above a chunk, so dumps and result links stay valid across commits | The identity space every map joins on | Kept; additive, no runtime cost |
| S2 | Fail-open and determinism contracts tested per channel; T0 guards today's fusion bit for bit; a fusion-only fast default is a registry configuration | The base of every phase 2 map and interface | Kept unused or merged as infrastructure; production path unchanged |
| S3 | `sym.defs` serves symbol addressing and compact results; the extraction hook replaces a reparse | First stored map; C4 auditor | Build only the hook and `sym.defs` |
| S4 | Not built (only the trait freeze if S3 merged) | First end-to-end measurement | Wait |
| S5 | Shelved | Interfaces for phase 2 (verbs, bundles, steering) | Wait |
| S6 | Shelved | Decides which maps to build | Wait |

## Order and gates

S1 then S2 tonight, each gated by `cargo fmt --all --check`, clippy as in
`docs/development/testing.md`, `cargo xtask test --suite Full` and
`cargo xtask lint-machine-paths`, then an independent read-only review. S3 can start in the
cloud before the A/B result for the hook and `sym.defs` only. S4 to S6 start after the A/B.
