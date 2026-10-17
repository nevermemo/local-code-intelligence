# Spec: map contract kit C1-C9

Status: **v0, pre-freeze; C1, C3, C4, C5 and C6 built, C2 and C8 not built, C7 and C9 partly or not built** (per contract below). Written 2026-10-12 at slice S0,
then merged with the as-built notes of S2, S3a and S3b. Labels (**Read**, **Inferred**, **built**) are defined in [README.md](README.md).

Naming: these ids are the *map contracts*. They are unrelated to the roadmap items `C1` and `C2` in `docs/development/roadmap.md` (per-chunk symbol metadata,
contextual headers). In prose and commit messages write "contract C1".

## 1. Purpose

Every map ships with the same kit of contract tests before it may appear in a benchmark row. The kit is generic: one test harness in `lci-maps` (`kit.rs`,
`kit_c3.rs`, `labels.rs`, `audit.rs`) runs each contract against any `Map` impl with a fixture workspace, so a new map costs a fixture and a few parameters, not a
new test file per contract. A map that cannot pass a contract is not merged into the default registry; it may be parked with its condition in
[../lessons.md](../lessons.md) ("Parking log").

Origin of each contract: C1-C5 and C7 are the contracts listed in the slice plan for this spike, which is not in the repository (fail-open, filter-before-rerank,
freshness model, full-rebuild equivalence, source label, no-graph and churn controls); in-repo grounding for them is lessons.md (which absorbed the recycled research) and the engine invariants
in [../engine.md](../engine.md) ("Seams and invariants"). C6, C8 and C9 are added here (Inferred) from lessons L14, L7/L15 and L18.

## 2. The contracts

Each contract is a property: a statement that holds for every input, with the test that checks it. "The kit" is the generic harness; "fixture" is a small offline
workspace under `tests/` with the fake model service and fake LSP (`tests/support/mod.rs`, `tests/support/fake_lsp.rs`).

### C1 Fail-open

- **Property.** For any map M and any failure of M (error, panic, timeout, invalid NodeId, non-finite score), a search or traversal that includes M returns the same
  results as the same run with M removed, plus exactly one `warning` naming M (failure) or one `note` (partial answer). A deliberate skip (M switched off) is silent:
  no warning, no note, as for a disabled channel today. M's failure never turns a successful run into an error, with one carve-out kept from today: when every
  enabled map failed or returned nothing and the semantic map is enabled, the run fails with "every enabled retrieval channel failed". That decision belongs to the
  engine, from the maps' statuses, so the M-removed comparison is made only where another enabled map returned candidates. An optional map never fails indexing.
- **Grounding (Read).** Today's channel rule: `AGENTS.md`, Hard rules; warnings appended, not replaced (`search_once`, `src/app/search.rs`); over-budget is a note;
  the reranker falls back to fusion order with a warning.
- **Test.** The kit wraps M in a fault injector with five modes (return `Failed`, panic, sleep past the deadline having emitted nothing, emit a NodeId absent from
  the universe, emit `NaN`). The sleeping mode emits no hit before the deadline; a map that did emit some is a `Partial` answer and is tested under C8. The fixture
  keeps another map returning candidates (see the carve-out). For each mode: results equal the M-removed run (compare NodeIds and `fusion_score` bits), `warning` or
  `notes` contains M's id once, and the run's exit status is success. Index-time variant: M's `extract` panics on one file; the core index commits and `index_status`
  reports M stale.
- **Built.** `kit::check_fail_open` over a `Fixture` (view, query text, per-call budget, slack, `rrf_k`) with a subject map and a control map that returns candidates
  (the carve-out). It runs the subject as it is, switched off, and under the five injected faults, and requires the fused pool (nodes, score bits, parts, order) to
  equal the control-only run, one warning (failure) or one note (deadline) attributed to the subject and none to another map, and every run to end within the budget
  plus the slack. `kit::check_real_failure` runs the subject configured to fail for real and requires the control-only pool plus exactly one warning or note naming
  it: the injected faults never call the map, so without it a map that hides its error behind an empty `Ok` would pass C1. The injected panic unwinds with
  `std::panic::resume_unwind`: the registry catches it as it catches `panic!`, but the panic hook does not run (with `RUST_BACKTRACE` set the hook captures and
  symbolises a backtrace, CPU work that ran inside the timed run and, on a loaded machine, took more than the slack: 2.6 s in one Full gate). A real `panic!` map is
  still tested against the registry (`the_injected_panic_fails_the_map_without_running_the_panic_hook`). The index-time variant is not built. Kit tests
  (`lci-maps/src/tests.rs`): a known-good rank list and prior pass both; a map that blocks its thread past the deadline fails C1 (the registry's timeout cannot
  preempt it, so the search stalls); a map that swallows its failure passes the injected faults and fails `check_real_failure`. The four wrapped legacy maps pass C1
  and C6 (`src/app/search/tests/t0.rs`), and so does the live lexical map over a real directory. A stored map's blocking work is moved off the async workers by
  `lci_maps::off_thread` (merge condition M5). Kit case `a_stored_map_blocking_off_thread_misses_its_deadline_without_stalling_the_run`: work that blocks for twice
  the time to its deadline passes C1 with a 300 ms budget and a 250 ms slack, answers `Partial` with one note, and the control's answer is kept; with the work run
  inline it fails ("kept the run for 600.28ms, past 550ms").

### C2 Filter before rerank (not built)

- **Property.** No node reaches fusion, the reranker request, the results or a steering packet unless it passes the run's `EffectiveFilter` (languages, include and
  exclude globs, source roles). This holds for every step that adds nodes, including expansion hops and steering `expand` ops.
- **Grounding (Read).** Filters run on the universe, inside `Store::search` (`src/store.rs`), after fusion and on results (`search_once`); `hit_matches`;
  `EffectiveFilter::matches` (`lci-core/src/filter/request.rs`).
- **Test.** A fixture with files in every source role and two languages. M is driven to return nodes from all of them. The fake reranker in `tests/support/mod.rs`
  counts the documents it receives and keeps the largest document size; the kit needs it extended to record each document's first line (the path, `rerank_document`).
  For each filter combination: every recorded document path, every result and every frontier node in the RunRecord matches the filter; `filters` in the report is
  unchanged.
- **Status.** `SnapshotView` carries no `EffectiveFilter`, and the registry refuses `Filter` maps (map-trait.md 3.4). The walk filters nodes through the view's
  universe instead.

### C3 Freshness model

- **Property.** M declares one freshness model (`per-file`, `per-commit`, `live`, `global`) and its answers honour `[index].freshness`: under `manual` and `watch` no
  map refreshes from a search; under `on-search` a stale map is refreshed incrementally or reported stale; a failed refresh keeps the previous data and warns. Every
  response states the snapshot it answered from and, for a live map, that it read the live tree. An optional map's identity change makes the map stale; it is never
  the hard error reserved for the embedding identity.
- **Grounding (Read).** `AGENTS.md`, Hard rules (freshness); `IndexFreshness` (`lci-core/src/config.rs`); `resolve_index_for_search` (`src/app/search.rs`); the
  identity-change error under `manual`/`watch`; the lexical channel reads the live tree while chunks come from the snapshot.
- **Test (design).** Set `stale_check_interval_seconds = 0` (default 10 throttles the check; existing tests do the same in `tests/cases/indexing.rs`) and, for
  `watch`, do not start the watcher. For each freshness mode, edit a fixture file after indexing and query M. For a stored map: `manual`/`watch` give the old
  snapshot's answer and a `freshness.snapshot` equal to the old `indexed_at`; `on-search` gives the new answer and `index.action = refreshed_incrementally`. For a
  `live` map (lexical today) the answer reflects the edited tree mapped onto the old snapshot's chunk ranges in every mode, and the response says so with `observed =
  live-tree` (lesson L17). Then bump M's version only: the search succeeds, M is reported stale or rebuilt, and no hard error occurs.
- **Built** (`lci-maps/src/fresh.rs`, `lci-maps/src/kit_c3.rs`, tests in `lci-maps/src/kit_c3/tests.rs`). The freshness rule for stored maps is code, not only a
  test: `fresh::prepare(map, mode, target)` is what an engine calls for each stored map after the core index has been resolved under `[index].freshness`. It returns
  `Current` (data already for the target), `Built` (no data: built in any mode, as a search may create a missing index), `Refreshed` (`on-search` only), `Stale`
  (`manual`/`watch`: the map keeps its data and says it is stale) or `Failed { warning }` (an error or a panic in the build; the previous data is kept and the
  warning names the map). The map's answer carries the snapshot its data was built for and `stale` when that is not the searched one.
  `kit_c3::check_freshness(subject, bumped, case)` runs on a fixture with an old and a new snapshot (two tree directories, so both stay readable) and a broken target
  (the new files under a root that does not hold them). Reference answers come from fresh builds of each snapshot, and the kit refuses a fixture whose edit does not
  change the answer. For each of `manual`, `watch`, `on-search`: the first search builds the map; after the edit, under `on-search` the map is refreshed and answers
  exactly as a fresh build of the new snapshot, while under `manual`/`watch` it answers exactly as the old one; then, for `manual`/`watch`, when the core moved to
  the new snapshot without a search, the map is not refreshed and its answer names the old snapshot with `stale = true` and equals the old answer restricted to the
  chunks the new view still has (the old data stays searchable). A failed refresh (the broken target's files carry content hashes no bundle has, so the refresh must
  read the missing files whatever the store holds) must return a warning naming the map, keep the old data and answer as before. A version bump over the previous
  version's stored data must be built or stale: never current (that would hide the identity change) and never a failure. `kit_c3::check_live_freshness` requires a
  live map to answer with `observed = live-tree`, as of the view's snapshot, never stale (running it on the wrapped lexical map, which lives in the root crate, is
  left to S4; that map's own test asserts `LiveTree`). Deviations from the design test: the check works at the map level with the policy function, not through
  `search_code` with `stale_check_interval_seconds = 0`, because no engine calls stored maps yet (S4); `index.action = refreshed_incrementally` is the core's report
  and is not checked here. The map's version is not the producer's: a version bump that changes only how the map answers reuses the stored bundles (`sym.defs@1`),
  and a payload change needs a new producer id (N-12). Known gaps of the check (review of e369120): a stored map that answers from the live tree it last refreshed
  from is not caught, because the old and new trees are separate directories and `manual` never hands the map the new one (an in-place edit phase would catch it);
  and `check_live_freshness` checks the freshness fields only, not that the answer reflects an edit, so a live map that caches its first answer and reports
  `live-tree` passes. Known-bad maps the kit rejects (`c3_rejects_each_known_bad_stored_map`): one that hides staleness (claims the searched snapshot), one that
  drops its data when a refresh fails, one that claims to have no data so every search rebuilds it (a refresh from a search under `manual`), one that refuses to
  build after a version bump, one that answers nothing while stale; plus, for live maps, one that reports `observed = snapshot`.

### C4 Full-rebuild equivalence

- **Property.** For any sequence of tree states S0..Sn, M's data after incremental updates through S1..Sn equals M's data from a full build at Sn, compared on a
  canonical serialisation (sorted NodeIds, sorted edges, bit-exact scores). Per-file data contains no paths, so a move-only commit reuses it.
- **Grounding (Read).** Per-file reuse when hash, language and adapter version match (`scan_incremental`, `lci-core/src/chunk.rs`); the earlier per-file derivation
  matched a full rebuild at every replayed commit, and path-embedding bundles failed on a move commit (lessons.md, Freshness; lesson L2).
- **Test.** The equivalence auditor: replay a fixed list of commits of this repository (at least one that moves files, one that changes an adapter version, one that
  deletes files) into a scratch worktree; after each step compare incremental and full builds; any difference fails with the first differing NodeId. A small
  synthetic edit script runs in the unit suite; the commit replay runs in a slower suite.
- **Built** (`lci-maps/src/audit.rs`, tests in `lci-maps/src/audit/tests.rs`). `audit::audit(subject, root, steps, apply)` walks a list of tree states; at each it
  updates the subject incrementally from its own previous state (`Audited::incremental`), builds it from nothing (`Audited::full`, a fresh scan into a fresh store)
  and compares the two sorted canonical lists; the first difference fails C4 with the entry and the side it is on. `SymDefsAudit` is the `sym.defs` subject: its
  state is the previous scan's files and a persistent bundle store; its canonical form is `<symbol>\t<head chunk or ->` per declaration. Adapter bump, a spec change:
  the adapter version belongs to the indexer binary, not to the tree, so replaying commits never changes it. A step with `bump_adapter` instead makes the subject
  relabel its stored state as produced by the previous adapter (`<version>~prior` in the manifest and in the bundle keys) and makes that state detectably different
  from what the current adapter produces (each file with more than one chunk loses its last one; each stored declaration's name gets a suffix; the bundle under the
  current key is removed). A subject that reuses any of it differs from the full build. Today's scan already re-parses any file whose adapter version differs, so the
  chunk perturbation guards a future scan; the bundle side is what a key without the adapter version trips on. Unit suite (`Full`): a synthetic script of nine steps
  on one directory (initial, edit inside a function, lines inserted above, a duplicated chunk, a new file, a delete, a move, an adapter bump, an edit after the
  bump); `sym.defs` is equal to a full build at every step, the delete and the move extract nothing, and the bump extracts every Rust file again. Known-bad maps,
  each rejected at its own step: a producer that keeps the module path in the bundle (move), a bundle key without the adapter version (adapter bump), an index that
  keeps a deleted file's declarations (delete). Slow suite (K-3, K-12): `audit::tests::c4_commit_replay`, `#[ignore]`d, run with `cargo test -p lci-maps --lib --
  --ignored c4_commit_replay --nocapture`. It replays this repository's commits into a scratch `git worktree`: `3ba1422^`, `3ba1422` (deletes `src/app.rs`),
  `0729d9a^`, `0729d9a` (moves 18 files into `lci-core/src`), an adapter bump on the same tree, and `b87b16a`. At the move commit exactly two files are extracted
  (`src/lib.rs` edited, `lci-core/src/lib.rs` added) and every moved file's bundle is reused; the test asserts it. Measured 2026-10-12 on the cloud machine (debug
  build): 19.4 s in total, 1.1 to 1.9 s per step except 12.1 s for the last (2680 declarations, 95 files extracted). It stays out of `Full` because CI checks out one
  commit (no history to replay), not for its time. Run against the path-in-bundle producer, it fails at the move commit with the first differing symbol
  (`lci-core::chunk::chunk::Chunk/struct`, the old module path joined under the new one). Running it once on a merge candidate is cheap evidence that the bundle
  sidecar still equals a full rebuild.

### C5 Source label

- **Property.** Every edge and every hit's evidence carries exactly one source label from a closed vocabulary: `vector`, `lexical`, `lsp`, `scip`, `tags`
  (Tree-sitter queries), `heuristic` (name or path rules), `git`, `config`, `doc`, `agent` (steering), `prior`. The map card lists the labels a map may emit; a label
  outside that list is a contract failure. Benchmark rows split by label, and a claim scored on SCIP-derived gold excludes `scip` evidence. "Exactly one label" is
  read per evidence item: a hit may carry several evidence items with different labels, each on the card (K-7).
- **Grounding (Read).** Tags-based call edges have low recall and high precision; edges must be labelled by source (lessons.md, "Conditions learned";
  lesson L6); SCIP-derived gold would be circular for SCIP-built maps (lessons.md, "Gold sets"; lesson L8); today `retrieval_channels` names the channels
  per hit (`src/store.rs`).
- **Test.** The kit validates every `MapResponse` in every contract run against the label vocabulary and the card's list. The harness refuses to emit a score row for
  a map with `scip` evidence against a gold set whose schema says `derived_from: scip`, unless the row is marked as a circular reference. `derived_from` is a
  proposed gold-set field; no evaluation schema has it (`src/evaluate/types.rs`).
- **Built** (`lci-maps/src/labels.rs`). The wire names of `SourceLabel` (`SourceLabel::ALL`, `as_str`, `from_wire`) with `LABELS_VERSION` equal to the RunRecord
  schema id; a test requires the checked-in schema's `label` enum to list exactly `SourceLabel::ALL`, in order, so a label is added or renamed only with a new schema
  id (K-5). `check_labels` refuses an `Ok` or `Partial` answer with a hit without evidence (K-7) or an evidence label the map's card (`MapDescriptor::source_labels`)
  does not list. Since merge condition M6 (`394faba`) `Registry::run` applies `check_labels` to every answer itself (a refused partial answer keeps its own note and
  warning in the refusal); the legacy maps are registered bare (`Labelled` stays for a caller without a registry; `git grep -n "Labelled::new" -- src` finds
  nothing); and `labels::check_source_labels` asks the subject and the control directly, outside a registry, because the registry now refuses the answers the kit must
  still see. A panic in a subject therefore unwinds through the kit's C5 check, and an answer the registry would refuse for a structural reason (over the limit, a
  foreign node) is judged on its labels only; C1 covers both. Test `the_registry_refuses_an_answer_off_its_card_without_a_wrapper`: a bare mislabelled map and a bare
  evidence-less map each give the control's pool and one warning naming the map. The kit's tests reject a map whose card lists `lexical` while it emits `vector`, a
  map whose hits carry no evidence, and a mislabelled control. The kit check refuses to pass vacuously: a subject with no used hit is a `kit` failure. Offline,
  `RunRecordFile::check_labels` (`src/evaluate/run_record.rs`) checks every used call in a record against the labels its plan lists. Not built: the `derived_from:
  scip` refusal (no gold-set field carries it), and benchmark rows split by label (no live rows in the cloud).

### C6 Determinism and replay

- **Property.** The same snapshot, query, seeds, parameters and budget give the same ranked NodeIds and bit-identical scores, whatever the registration order and
  thread timing. Ties between chunks keep the legacy fused order (`fusion_score` descending, then path, `start_line`, `end_line`, `content_hash`; `fusion_order`,
  `src/app/search.rs`); only ties that involve a non-chunk node fall back to the NodeId canonical string. Wrapping the legacy channels as maps leaves every fusion
  score bit-identical and every stored candidate dump replayable.
- **Grounding (Read).** The dump contract fixes the f32 order (`CandidateTrace` docs); replay verifies dumps bit for bit and refuses a query otherwise
  (`src/evaluate/replay.rs`, `fusion_score`); the LSP channel merges providers query by query to keep its rank deterministic.
- **Test.** T0: every existing fusion unit case and every candidate dump in the evaluation fixtures replays with zero refusals through the registry. Then each map
  runs twice with shuffled registration and a randomised thread pool size: identical output.
- **Built.** `kit::check_determinism` compares the subject's answer and the fused pool across a second run and a reversed registration; the randomised thread pool is
  not done, and three calls cannot catch a map that drifts only every fourth call or later. A map whose answer drifts between calls fails C6 and still passes C1 (kit
  test). T0 ran on the unit cases and one three-file fixture; the stored-dump half is described in map-trait.md section 6.

### C7 Controls

- **Property.** A benchmark row for an expansion or prior map is only emitted together with its control rows from the same run: the no-graph control (the map off), a
  size-matched random expansion (same number of added nodes, drawn with a recorded seed from the same filtered universe), and, for history maps, a churn control
  (files ranked by commit count in the same window). Gain is always reported against the map's own controls. A baseline arm (for example lexical plus reading) is a
  reference row for tracking improvement, never a gate.
- **Grounding (Read).** Vector-neighbour hops were close to random with precise structure; co-change beat churn only with clean seeds (lessons.md,
  "Conditions learned"; lessons L10, L11); method: no-graph and random controls, paired bootstrap (lessons.md, "Method to reuse";
  `src/evaluate/compare.rs`).
- **Test.** The harness has a unit test that feeds it a map result set lacking a control arm and asserts it refuses the row; and one that checks the random control
  has the same added count per query as the map arm.
- **Status.** The controls part is not built as a kit check. Code drift: `kit_c3::check_edge_invariants` and `check_expand` report the contract id "C7" for the edge
  invariants of `Map::expand` (every edge leaves the expanded node, names the map, has a confidence of at most `CONF_MAX`, the list is in `sort_edges` order and
  holds at most `EXPAND_LIMIT` edges). Control arms are produced by the `maps bench` and evaluation harness, see [../evaluation.md](../evaluation.md).

### C8 Budget and cost honesty (not built as a kit check)

- **Property.** M stops within its `deadline` plus a fixed slack and within `max_external_calls` and `max_nodes_touched`; when a budget runs out it returns `Partial`
  with what it has. Its reported `cost` is measured, not estimated: `elapsed_ms` is within the wall-clock time the engine measured around the call, and counters
  match the fakes' request logs.
- **Grounding (Read).** The LSP budget keeps answers gathered before the deadline and adds a note (`search_once`); `est_tokens = document_bytes / 4`
  (`CandidateTrace::est_tokens`); a cold language server takes minutes (lesson L7).
- **Test.** The fake LSP and fake model service get a delay flag; with a 200 ms deadline M returns in under 200 ms plus slack, status `Partial`, a note, and
  `cost.external_calls` equal to the requests the fake recorded.
- **Status.** The registry enforces the deadline only (map-trait.md 3.3); the deadline case is covered by C1 and the M5 kit case.

### C9 As-of and leak safety (not built)

- **Property.** A map queried with `as_of = c` reads no commit, blob or file state after `c`; a history map built for a gold item with `base_commit = c` contains
  nothing after `c`. A RunRecord whose query was mined from a commit carries a leak scan result, and the harness fails the run (not the row) when the scan finds the
  answer leaked into the inputs (for example the last-change subject equal to the query).
- **Grounding (Read).** A co-change map built at HEAD contains the commits the miner turned into gold (lesson L18; the engine invariant "Leakage" in
  [../engine.md](../engine.md)); leak scans must be code assertions (lessons.md, "Method to reuse"); per-item `base_commit` in the public bug-localisation
  set ([../evaluation.md](../evaluation.md), "Bug-localisation set").
- **Test.** A fixture repository with a commit after `c` that adds a unique token: M with `as_of = c` never returns or mentions it. A planted leak (a query equal to
  a commit subject inside the window) makes the harness fail with the leak named.

## 3. Applicability

| Contract | Text maps (lexical, vector, LSP symbols) | Expansion maps (graph, co-change) | Prior and filter maps | Index-time (producers) |
| --- | --- | --- | --- | --- |
| C1 | yes | yes | yes | yes (extraction) |
| C2 | yes | yes | yes | n/a |
| C3 | yes | yes | yes | yes |
| C4 | only if stored | yes if stored | n/a | yes |
| C5 | yes | yes | yes | yes |
| C6 | yes | yes | yes | yes |
| C7 | no (they are the controls' base) | yes | yes | n/a |
| C8 | yes | yes | n/a | yes (build time on the card) |
| C9 | only history-aware | yes for history maps | yes for recency | yes for history maps |

## 4. Open decisions

K-7 to K-9 were the numbers on `cloud/spike-s3a`; they were renumbered K-10 to K-12 when `cloud/spike-s3b`, which had already used K-7 and K-8, was merged.

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| K-1 | Keep the ids C1-C9 and say "contract C1" in prose. | Rename to K1-K9 to avoid the clash with roadmap items C1 and C2. | When the roadmap next adds a C-row. |
| K-2 | One generic kit in `lci-maps` driven by fixtures. | Hand-written tests per map (more freedom, more drift). | S2. |
| K-3 | The C4 commit-replay auditor runs in a slow suite, not in `Full`. | Run it in `Full` on a short commit list. | After its first timing (S1). |
| K-4 | Random control is size-matched per query from the filtered universe. | Degree-matched random (same seed degree), closer to the map's structure. | First expansion map (W1b). |
| K-5 | C5 vocabulary is closed and versioned with the RunRecord schema. | Free-form labels with a registry. | S4 freeze. |
| K-6 | A map failing a contract is kept out of the default registry but may still be benchmarked as an opt-in arm with the failure recorded. | Block it entirely until it passes. | First failing map. |
| K-7 | C5 refuses a used hit with no evidence (it has no source label); a map card must list every label its evidence uses. A hit may carry several evidence items with different labels, each on the card. | Allow label-less hits and label them by the map's first card label. | S4 freeze. |
| K-8 | C5 is applied at run time inside `Registry::run`'s answer check (`labels::check_labels`), so every registry run checks labels, the kit's C1 and C6 runs included; the legacy maps are registered bare; `Labelled` stays for a caller without a registry. A mislabelled answer becomes `Failed` with one warning (C1), keeping a partial answer's own note or warning. (First taken in S3 as a wrapper in `labels::Labelled`; resolved 2026-10-13, merge condition M6, `394faba`.) | Wrap each map in `Labelled` at registration. | S4 freeze. |
| K-10 | The C3 policy is a function (`fresh::prepare`) the engine calls per stored map, after the core snapshot is resolved; stored maps follow the core snapshot and never probe the tree themselves. | Each stored map probes the tree and refreshes itself under the mode. | S4 engine. |
| K-11 | The C4 adapter bump is simulated by relabelling and perturbing the stored state. | Build a second adapter version into test builds. | When a second chunker version exists. |
| K-12 | The commit replay runs in an ignored test, outside `Full`, because CI has no history. | Fetch the needed commits in CI (`fetch-depth: 0`) and run it in `Full` (about 20 s). | When CI's checkout depth is next changed. |

## 5. How a test proves it

The kit is itself tested: each contract has a known-bad fake map (one that leaks a filtered node, one that keeps paths in its bundle, one that reads past `as_of`,
one that misreports its cost, one without a source label) and the kit must fail each with the right contract id. A kit that passes a known-bad map is a kit bug. The
known-good legacy maps (wrapped in S2) must pass every applicable contract with no exceptions recorded. Built known-bad maps: C1 (blocking, swallowed failure), C3
(five stored and one live), C4 (three), C5 (three). Not written: the known-bad maps of C2, C7, C8 and C9.
