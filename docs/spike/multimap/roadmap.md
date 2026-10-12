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
| S4 | First end-to-end slice and trait freeze: lexical map plus `sym.defs` through NodeId, extraction and the harness; offline harness scoring of RunRecords. Offline harness built early on `cloud/spike-s3b` (scoring, one-shot equivalence, `fuse` replay); the freeze is the owner's, by the [S4 freeze checklist](#s4-freeze-checklist-for-the-owner) | `lci-maps/`, `src/evaluate/` | cloud for the offline harness; owner for live rows and the freeze | yes | offline harness only |
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

## S4 freeze checklist (for the owner)

Written on `cloud/spike-s3b` (2026-10-12) instead of freezing: the trait and the RunRecord
schema stay at v0 (`lci.runrecord/0`) until the owner has gone through this list. Freezing
means renaming the schema id to `lci.runrecord/1`, copying `runrecord-0.schema.json` to
`runrecord-1.schema.json`, and from then on changing either only with a new id.

Evidence to have before the freeze:

- [ ] The A/B decision (CONTINUE, overall or in a stratum); S4 waits for it.
- [ ] One end-to-end slice through the trait with a stored map (`sym.defs` from the sibling
      S3 branch) and the lexical map, recorded as RunRecords and scored by the offline harness.
- [ ] Live rows on the owner's machine (embedder and reranker up), recorded and replayed
      with no `fuse` replay mismatch.
- [ ] Contracts C3 and C4 (sibling branch) and C5 (this branch) green on every map in the
      default registry.

Trait decisions to confirm or change (map-trait.md, contract-kit.md):

- [ ] `MapStatus::Partial { note, warning }` (S2 open question 1).
- [ ] Fusion order rule M-3 (legacy three, then rank lists by id, then priors) and the tie
      rule (`fused_order`, contract C6).
- [ ] `MapRequest.params` untyped (M-6), `as_of` semantics (C9), `Budget` beyond the
      deadline (C8): enforced or advisory.
- [ ] `Contribution::Filter` (contract C2): in the frozen trait or added later as `/2`.
- [ ] `EdgeKind`: only `query_match` and `prior` today; expansion maps need more. Decide
      whether `edge` stays a free string in the schema or becomes an enum.
- [ ] C5: the closed label list (`SourceLabel::ALL`, K-5), a hit without evidence refused
      (K-7), `Labelled` as the registry's C5 boundary or the check moved into
      `Registry::run`'s own answer check (`lib.rs` was out of scope here), and the
      `derived_from: scip` refusal (needs a gold-set field that does not exist yet).
- [ ] `Map::describe` outside the fail-open boundary (it must not panic).

RunRecord decisions to confirm or change (run-record.md, section 4):

- [ ] R-1 JSON Lines; R-6 ULID run ids.
- [ ] R-2 query text in full (private items stay outside the repository) or hashed.
- [ ] R-3 frontier rows capped (default 50, `size` exact); replay checks the rows present
      and that no left-out node belongs before them (by score, and by path only on a tie).
- [ ] R-4 map hits stored per call (needed by the `fuse` replay and leave-one-map-out).
- [ ] R-8 fingerprint fallback, R-9 hand-written schema checker versus a validator crate (a
      new dependency needs approval), R-10 no switch on the production path, R-11 redaction
      by known values with path boundaries.
- [ ] Strictness: `additionalProperties: false` everywhere except `filters`, `ops`,
      `params` and `metrics`.
- [ ] Step kinds a one-shot run writes (`fan_out` Y1, `fuse`, `cap_cut` Y2, and `rerank` Y3
      once a path reranks), and post-hoc writing on the registry path: a run killed while
      maps run leaves only the `run` line until the engine writes steps as they happen (S5).
- [ ] `cost.elapsed_ms` of a call is the registry's wall time; `per_map` and
      `engine_cost` meanings.
- [ ] The `evaluation` block on `end` is written by the harness only (not written yet).
- [ ] Which row fields are required: the harness needs `source_role`, `snippet` (when
      captured) and `fused_rank`; `language` is not recorded at all.
- [ ] Harness parity rules: a gold query is scored only against a completed, unfiltered
      run with the same `top_k`; pairing by query text or by `(item.set, item.id, arm)`.
- [ ] The float invariant the bit-for-bit replay rests on: every recorded score is an
      `f32` widened to `f64` (`run_record::number`).
- [ ] One-shot equivalence is shown for the default profile with the reranker off; a
      profile and a `rerank` step are still to come.

