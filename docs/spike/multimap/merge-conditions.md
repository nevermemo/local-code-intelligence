# Merge conditions for S2 and S3a

Written 2026-10-12 (`date -u` 18:37Z) by the spike-trim run, from the independent sanity
review (`reports/sanity-spike.md` on `cloud/sanity-spike`, sections 2.1, 2.2 and 5 item 4)
and the owner's decisions recorded in `reports/owner-notes.md` on `cloud/monitor` (entries
2026-10-12T13:33Z and 18:08Z). Status: conditions, not a plan to merge.

Branches: S2 is `cloud/spike-s1s2` (head `b87b16a`, which also carries S1); S3a is
`cloud/spike-s3a` (head `219d0c0`, stacked on S2; read for this document, not changed). S3b
(`cloud/spike-s3b`) is trimmed and not a merge candidate; the conditions that name it apply
only if it is ever proposed.

**When.** Not before the agent A/B (I-10) result: the owner accepted the review's TRIM
verdict on 2026-10-12 (18:08Z), which keeps S1, S2 and S3a unmerged until then. The
conditions below are necessary, not sufficient: they say what a merge must change, not that
one should happen.

Why conditions are needed (sanity review, section 2.1): both branches are additive by
duplication. S2 repeats the production LSP arm in `LspMap`; S3a repeats the chunker's body
in `extract_with_adapter`; both put spike entry points on the public `App` facade. Each
duplicate is kept equal by a test today. That is right for a spike and wrong for a merge.

## Conditions

Each condition names what must be true in the merge candidate and how to check it. "T0"
means the bit-for-bit tests in `src/app/search/tests/t0.rs`; "the golden test" means
`search_code_json_matches_the_golden_file` in `tests/cases/`; "the traced-search tests" means
`the_map_registry_fuses_the_pool_of_a_traced_search_bit_for_bit` and its failure-path variant
in `tests/cases/indexing.rs`.

### M1. One LSP orchestration (S2; owner decision 2026-10-12T13:33Z)

Today `search_once` (`src/app/search.rs`, the LSP arm around lines 714-846 at `b87b16a`:
applicable adapters, `lsp_search_budget_ms`, the per-provider fan-out, the over-budget and
failure warnings) and `LspMap::query` (`src/app/search/legacy_maps.rs` lines 372-470) carry
the same logic twice ("the 40-line provider orchestration of `search_once`",
`docs/spike/multimap/specs/map-trait.md` lines 200-205). The owner decided that at merge the arm is extracted into one function that both
`search_once` and `LspMap` call, guarded by T0.

Check:

- One definition: the provider selection, budget and fan-out exist once
  (`git grep -n "lsp_search_budget_ms" -- src/app` finds the shared function and the
  warning text once, not twice).
- T0, the golden test and the traced-search tests pass unchanged (no expected value edited).
- `cargo xtask test --suite Lsp` passes.
- Mutation spot check: change the shared function's warning text or budget; both a
  production-path test and a map-path test fail.

### M2. One chunker (S3a)

`extract_with_adapter` (`lci-core/src/chunk.rs` on `cloud/spike-s3a`, +66 lines) repeats
the `ranges` walk, trimming, line arithmetic and hash of `chunks_with_adapter` "so today's
chunk output cannot change" (its doc comment). At merge, `chunks_with_adapter`'s body is
replaced by a call to the hook (its signature and output unchanged), and the copied steps
are deleted.

Check:

- One copy: `chunk.rs` computes `content_hash: hash(code)` and walks `ranges` in one
  function.
- The golden test passes byte-identical, and `cargo xtask test --suite Chunk` passes.
- `extraction_tests` (`hook_chunks_are_byte_identical_on_a_tree_of_every_language`,
  `hook_chunks_are_byte_identical_on_this_repository`, `hook_keeps_the_scan_order_of_chunks`)
  pass. Once there is one chunker, comparing the hook with `chunks_with_adapter` compares a
  function with itself, so before the switch commit the merge records the current chunk
  output of those fixture trees (a checked-in expected file or a hash per file) and the
  tests compare against that record. Proof that it is not vacuous: the record test passes on
  the commit before the switch and after it, and a mutation (drop the `trim`) fails it.
- Cheap cross-check: `cargo run -p lci-core --example node_stability -- --rev 66f999d
  --commits 200` prints the same table as `docs/spike/multimap/s1-stability.md` (the tool
  chunks through `chunks_for_path`).
- Cost: the hook also walks Rust declarations. If `chunks_for_path` starts paying for that
  walk, time a full scan of this repository before and after on the same machine and record
  both; otherwise the declaration walk stays opt-in for callers that ask for it.

### M3. Spike entry points behind a test feature (S2, S3b)

`App::fuse_through_maps` (`src/app/search/legacy_maps.rs` line 615) and, on S3b,
`App::record_through_maps` (`src/app/search/legacy_maps/record.rs` line 60) are `pub` and
`#[doc(hidden)]` on the facade, and `legacy_maps.rs` and `record.rs` are compiled into the
production binary. At merge they sit behind a Cargo feature used by tests (name to choose,
for example `spike-maps`), together with the modules only they use.

Open for the owner: on 2026-10-12T13:33Z the owner answered the S2 report's question 4 with
"It can stay", read by the monitor as "`App::fuse_through_maps` stays public and
doc-hidden". The review (section 5 item 4) and this run's brief ask for the feature gate. The
gate is the conservative reading and is reversible; this document lists it as a condition,
and the owner's later word decides.

Check:

- `cargo check -p local-code-intelligence` without the feature compiles, and neither method
  exists there (`cargo doc` without the feature, or a `compile_fail` doctest).
- The integration tests that call them (`tests/cases/indexing.rs`, and S3b's
  `tests/cases/run_records.rs`) compile only with the feature, and `cargo xtask test --suite
  Full` enables it, so the tests do not vanish. Proof: the number of tests Full reports is
  the same before and after the gating commit.
- `src/app/AGENTS.md` (preserve public paths) is satisfied: the default build adds no public
  path.

### M4. `SnapshotView::new` timed on a realistic set (S2)

`SnapshotView::new` (`lci-maps/src/lib.rs` lines 141-162) derives Chunk NodeIds for the
whole filtered universe on every run (a sort of every chunk). Today's search already loads
every chunk row, so this is one more pass per search, and it is timed nowhere. The
registry must not replace `fuse_hits` until this is a number (the spec's own risk note,
`architecture-map.md` section 9.1).

Check:

- On the owner's machine, through `fuse_through_maps` or an added per-stage timing, time
  `SnapshotView::new` per query over the mined set `evaluations/mined/self.toml` on this
  repository and over the largest repository the A/B indexes; report p50, p95 and max
  beside the same queries' end-to-end search p50 (the E4 per-stage format).
- The acceptable share of search latency is the owner's to set before the measurement, not
  after it.

### M5. `spawn_blocking` for stored maps (S3a)

Rank-list maps run inside one `join_all` on the caller's task (`lci-maps/src/lib.rs` lines
492-497), so a map that blocks the thread stalls the run, and the registry's timeout
(`lib.rs` lines 628-632) cannot pre-empt it; the kit only detects such a map
(`lci-maps/src/tests.rs` lines 350-363). `sym.defs` (`lci-maps/src/symdefs.rs` on
`cloud/spike-s3a`) does its matching inline in `query` (`answer`, line 173) and its refresh
does synchronous file I/O (bundle `sync` and `retain` in `build`). At merge, a stored map's
CPU or file work runs off the async workers (`tokio::task::spawn_blocking`, the registry
awaiting the handle under its timeout), either inside the map or by the registry for every
map that declares stored data.

Check:

- A kit case: a stored map whose query blocks for twice its deadline yields `Partial` or
  `Failed` within the deadline plus a stated slack, and another map's answer in the same run
  is kept.
- `sym.defs` refresh is not on the request path, or runs in `spawn_blocking`: a refresh with
  a deliberately slow producer does not delay a concurrent map's answer.
- The bound on detached blocking work (a timed-out `spawn_blocking` task keeps running) is
  written down, with the maximum number in flight.

### M6. C5 check in `Registry::run` (S3b; owner decision 2026-10-12T18:08Z, B)

S3b checks contract C5 (closed label vocabulary) by wrapping each registered map in
`lci_maps::labels::Labelled` (`src/app/search/legacy_maps.rs` lines 581-584 on
`cloud/spike-s3b`; decision K-8 in `contract-kit.md`). The owner decided that at merge the
check moves into `Registry::run`, so a map registered without the wrapper is checked too.

Check:

- `Labelled` is no longer needed at registration (`git grep -n "Labelled::new" -- src`
  finds nothing outside tests).
- An `lci-maps` test registers a map with an unknown label directly (no wrapper) and
  `Registry::run` refuses its answer, naming the map; the existing three C5 tests pass.

### M7. Hand-written schema checker kept (S3b; owner decision 2026-10-12T18:08Z, B)

S3b validates RunRecord lines with its own checker (`lci-maps/src/run_record/schema.rs`)
against `docs/spike/multimap/specs/runrecord-0.schema.json`. The owner decided to keep it:
no JSON Schema crate is added (open question R-9 in `reports/spike-s3b.md`).

Check:

- The merge's `Cargo.lock` diff adds no schema-validation crate (`git diff
  <base>..<candidate> -- Cargo.lock` lists `lci-maps` and nothing for JSON Schema).
- The checker's tests in `lci-maps/src/run_record/tests.rs` pass.

## Notes (not conditions of this merge)

- Before the registry replaces `fuse_hits` (a later step than this merge), the skipped half
  of T0 runs: one real candidate dump from `test-results/` replayed through the registry
  with zero refusals (owner's machine; review section 5 item 3). Until then "bit-exact
  against `fuse_hits`" means "on the unit cases and one three-file fixture".
- The C4 commit-replay auditor (`lci-maps/src/audit.rs` on `cloud/spike-s3a`) is ignored in
  Full because CI has no history; running it once on the merge candidate is cheap evidence
  that the bundle sidecar still equals a full rebuild.
- Other owner decisions recorded for S3b (18:08Z, B), not merge conditions: the record path
  stays in `legacy_maps.rs` until merge; full query text is kept in local RunRecords; no
  profile or reranker step now.
