# Spec: map contract kit C1-C9

Status: **v0, pre-freeze.** Written 2026-10-12T10:05Z (`date -u`) at slice S0. Docs only.

Code read at spike head `df7faaa` (code of `7f9c636`). Labels: **Read** = in the code or docs at
that tree; **Inferred** = designed here.

Naming: these ids are the *map contracts*. They are unrelated to the roadmap items `C1` and
`C2` in `docs/development/roadmap.md` (per-chunk symbol metadata, contextual headers). In
prose and commit messages write "contract C1".

## 1. Purpose

Every map ships with the same kit of contract tests before it may appear in a benchmark row.
The kit is generic: one test harness in `lci-maps` (S2) runs each contract against any
`Map` impl with a fixture workspace, so a new map costs a fixture and a few parameters, not a
new test file per contract. A map that cannot pass a contract is not merged into the default
registry; it may be parked with its condition in [../lessons.md](../lessons.md) ("Parking
log").

Origin of each contract: C1-C5 and C7 are the contracts listed in the slice plan for this
spike, which is not in the repository (fail-open, filter-before-rerank, freshness model,
full-rebuild equivalence, source label, no-graph and churn controls); in-repo grounding for
them is architecture-map.md 9.0 and recycled-research.md. C6, C8 and C9 are added here
(Inferred) from lessons L14, L7/L15 and L18.

## 2. The contracts

Each contract is a property: a statement that holds for every input, with the test that
checks it. "The kit" below is the generic harness; "fixture" is a small offline workspace
under `tests/` with the fake model service and fake LSP (`tests/support/mod.rs`,
`tests/support/fake_lsp.rs`).

### C1 Fail-open

- **Property.** For any map M and any failure of M (error, panic, timeout, invalid NodeId,
  non-finite score), a search or traversal that includes M returns the same results as the
  same run with M removed, plus exactly one `warning` naming M (failure) or one `note`
  (partial answer). A deliberate skip (M switched off) is silent: no warning, no note, as for
  a disabled channel today (`search.rs:691-693`, `:714-716`, `:836-839`). M's failure never
  turns a successful run into an error, with one carve-out kept from today: when every
  enabled map failed or returned nothing and the semantic map is enabled, the run fails with
  "every enabled retrieval channel failed" (`search.rs:907-931`). That decision belongs to the
  engine, from the maps' statuses, so the M-removed comparison is made only where another
  enabled map returned candidates. An optional map never fails indexing.
- **Grounding (Read).** Today's channel rule: `AGENTS.md`, Hard rules; warnings appended, not
  replaced (`src/app/search.rs:824-835`, `:855-876`, `:894-902`); over-budget is a note
  (`:813-823`); the reranker falls back to fusion order with a warning (`:1392-1406`).
- **Test.** The kit wraps M in a fault injector with five modes (return `Failed`, panic,
  sleep past the deadline having emitted nothing, emit a NodeId absent from the universe, emit
  `NaN`). The sleeping mode emits no hit before the deadline; a map that did emit some is a
  `Partial` answer and is tested under C8. The fixture keeps another map returning candidates
  (see the carve-out). For each mode: results equal the M-removed run (compare NodeIds and `fusion_score` bits), `warning` or
  `notes` contains M's id once, and the run's exit status is success. Index-time variant: M's
  `extract` panics on one file; the core index commits and `index_status` reports M stale.

### C2 Filter before rerank

- **Property.** No node reaches fusion, the reranker request, the results or a steering
  packet unless it passes the run's `EffectiveFilter` (languages, include and exclude globs,
  source roles). This holds for every step that adds nodes, including expansion hops and
  steering `expand` ops.
- **Grounding (Read).** Filters run on the universe (`search.rs:689-690`), inside
  `Store::search` (`src/store.rs:331-337`, row check at `:364`), after fusion (`search.rs:940`)
  and on results (`:998`); `hit_matches` at `:435-441`; `EffectiveFilter::matches` at
  `lci-core/src/filter/request.rs:284`.
- **Test.** A fixture with files in every source role and two languages. M is driven to
  return nodes from all of them. The fake reranker in `tests/support/mod.rs` today counts
  the documents it receives and keeps the largest document size (`tests/support/mod.rs:30`, `:44`,
  `:110`); the kit needs it extended to record each document's first line (the path,
  `rerank_document`, `search.rs:329-335`). For each filter combination: every recorded document path, every
  result and every frontier node in the RunRecord matches the filter; `filters` in the report
  is unchanged.

### C3 Freshness model

- **Property.** M declares one freshness model (`per-file`, `per-commit`, `live`, `global`;
  maps-inventory.md, "Update model vocabulary") and its answers honour `[index].freshness`:
  under `manual` and `watch` no map refreshes from a search; under `on-search` a stale map is
  refreshed incrementally or reported stale; a failed refresh keeps the previous data and
  warns. Every response states the snapshot it answered from and, for a live map, that it read
  the live tree. An optional map's identity change makes the map stale; it is never the hard
  error reserved for the embedding identity.
- **Grounding (Read).** `AGENTS.md`, Hard rules (freshness); `IndexFreshness`
  (`lci-core/src/config.rs:19`); `resolve_index_for_search` (`src/app/search.rs:1022`);
  identity-change error under `manual`/`watch` (`search.rs:1105-1108`); the lexical channel
  reads the live tree while chunks come from the snapshot (architecture-map.md, 4.2).
- **Test.** Set `stale_check_interval_seconds = 0` (default 10 throttles the check,
  `src/app/indexing.rs:313-315`; existing tests do the same, `tests/cases/indexing.rs:349`)
  and, for `watch`, do not start the watcher. For each freshness mode, edit a fixture file
  after indexing and query M. For a stored map: `manual`/`watch` give the old snapshot's
  answer and a `freshness.snapshot` equal to the old `indexed_at`; `on-search` gives the new
  answer and `index.action = refreshed_incrementally`. For a `live` map (lexical today,
  `search.rs:726`) the answer reflects the edited tree mapped onto the old snapshot's chunk
  ranges in every mode, and the response says so with `observed = live-tree` (lesson L17). Then bump M's
  version only: the search succeeds, M is reported stale or rebuilt, and no hard error occurs.

### C4 Full-rebuild equivalence

- **Property.** For any sequence of tree states S0..Sn, M's data after incremental updates
  through S1..Sn equals M's data from a full build at Sn, compared on a canonical
  serialisation (sorted NodeIds, sorted edges, bit-exact scores). Per-file data contains no
  paths, so a move-only commit reuses it.
- **Grounding (Read).** Per-file reuse when hash, language and adapter version match
  (`lci-core/src/chunk.rs:300-308`); the earlier per-file derivation matched a full rebuild at
  every replayed commit, and path-embedding bundles failed on a move commit
  (recycled-research.md, Freshness; lesson L2).
- **Test.** The equivalence auditor: replay a fixed list of commits of this repository (at
  least one that moves files, one that changes an adapter version, one that deletes files)
  into a scratch worktree; after each step compare incremental and full builds; any
  difference fails with the first differing NodeId. A small synthetic edit script runs in the
  unit suite; the commit replay runs in a slower suite.

### C5 Source label

- **Property.** Every edge and every hit's evidence carries exactly one source label from a
  closed vocabulary: `vector`, `lexical`, `lsp`, `scip`, `tags` (Tree-sitter queries),
  `heuristic` (name or path rules), `git`, `config`, `doc`, `agent` (steering), `prior`. The
  map card lists the labels a map may emit; a label outside that list is a contract failure.
  Benchmark rows split by label, and a claim scored on SCIP-derived gold excludes `scip`
  evidence.
- **Grounding (Read).** Tags-based call edges have low recall and high precision; edges must
  be labelled by source (recycled-research.md, "Conditions learned"; lesson L6); SCIP-derived
  gold would be circular for SCIP-built maps (recycled-research.md, "Gold sets"; lesson L8);
  today `retrieval_channels` names the channels per hit (`src/store.rs:42`).
- **Test.** The kit validates every `MapResponse` in every contract run against the label
  vocabulary and the card's list. The harness refuses to emit a score row for a map with
  `scip` evidence against a gold set whose schema says `derived_from: scip`, unless the row is
  marked as a circular reference. `derived_from` is a proposed gold-set field; no evaluation
  schema has it today (`src/evaluate/types.rs`).

### C6 Determinism and replay

- **Property.** The same snapshot, query, seeds, parameters and budget give the same ranked
  NodeIds and bit-identical scores, whatever the registration order and thread timing. Ties
  between chunks keep the legacy fused order (`fusion_score` descending, then path,
  `start_line`, `end_line`, `content_hash`; `fusion_order`, `search.rs:337-344`, tested at
  `:1802`); only ties that involve a non-chunk node fall back to the NodeId canonical string. Wrapping the legacy channels as maps leaves every
  fusion score bit-identical and every stored candidate dump replayable.
- **Grounding (Read).** The dump contract fixes the f32 order (`search.rs:244-254`); replay
  verifies dumps bit for bit and refuses a query otherwise (`src/evaluate/replay.rs:406`,
  `fusion_score` at `:307`); the LSP channel merges providers query by query to keep its rank
  deterministic (`search.rs:789-797`).
- **Test.** T0: every existing fusion unit case and every candidate dump in the evaluation
  fixtures replays with zero refusals through the registry. Then each map runs twice with
  shuffled registration and a randomised thread pool size: identical output.

### C7 Controls

- **Property.** A benchmark row for an expansion or prior map is only emitted together with
  its control rows from the same run: the no-graph control (the map off), a size-matched
  random expansion (same number of added nodes, drawn with a recorded seed from the same
  filtered universe), and, for history maps, a churn control (files ranked by commit count
  in the same window). Gain is always reported against the map's own controls. A baseline arm
  (for example lexical plus reading) is a reference row for tracking improvement, never a
  gate.
- **Grounding (Read).** Vector-neighbour hops were close to random with precise structure;
  co-change beat churn only with clean seeds (recycled-research.md, "Conditions learned";
  lessons L10, L11); method: no-graph and random controls, paired bootstrap
  (recycled-research.md, "Method to reuse"; `src/evaluate/compare.rs:382`).
- **Test.** The harness has a unit test that feeds it a map result set lacking a control arm
  and asserts it refuses the row; and one that checks the random control has the same added
  count per query as the map arm.

### C8 Budget and cost honesty

- **Property.** M stops within its `deadline` plus a fixed slack and within
  `max_external_calls` and `max_nodes_touched`; when a budget runs out it returns `Partial`
  with what it has. Its reported `cost` is measured, not estimated: `elapsed_ms` is within
  the wall-clock time the engine measured around the call, and counters match the fakes'
  request logs.
- **Grounding (Read).** The LSP budget keeps answers gathered before the deadline and adds a
  note (`search.rs:750-772`, `:813-823`); `est_tokens = document_bytes / 4`
  (`search.rs:318-320`); a cold language server takes minutes (lesson L7).
- **Test.** The fake LSP and fake model service get a delay flag; with a 200 ms deadline M
  returns in under 200 ms plus slack, status `Partial`, a note, and `cost.external_calls`
  equal to the requests the fake recorded.

### C9 As-of and leak safety

- **Property.** A map queried with `as_of = c` reads no commit, blob or file state after `c`;
  a history map built for a gold item with `base_commit = c` contains nothing after `c`. A
  RunRecord whose query was mined from a commit carries a leak scan result, and the harness
  fails the run (not the row) when the scan finds the answer leaked into the inputs (for
  example the last-change subject equal to the query).
- **Grounding (Read).** A co-change map built at HEAD contains the commits the miner turned
  into gold (architecture-map.md, 9.5 Risk; lesson L18); leak scans must be code assertions
  (recycled-research.md, "Method to reuse"); per-item `base_commit` in the public
  bug-localisation set (`gold-bugloc.md`, "Scoring").
- **Test.** A fixture repository with a commit after `c` that adds a unique token: M with
  `as_of = c` never returns or mentions it. A planted leak (a query equal to a commit subject
  inside the window) makes the harness fail with the leak named.

## 3. Applicability

| Contract | Text maps (lexical, vector, LSP symbols) | Expansion maps (graph, co-change) | Prior and filter maps | Index-time (IndexedMap) |
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

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| K-1 | Keep the ids C1-C9 and say "contract C1" in prose. | Rename to K1-K9 to avoid the clash with roadmap items C1 and C2. | When the roadmap next adds a C-row. |
| K-2 | One generic kit in `lci-maps` driven by fixtures. | Hand-written tests per map (more freedom, more drift). | S2. |
| K-3 | The C4 commit-replay auditor runs in a slow suite, not in `Full`. | Run it in `Full` on a short commit list. | After its first timing (S1). |
| K-4 | Random control is size-matched per query from the filtered universe. | Degree-matched random (same seed degree), closer to the map's structure. | First expansion map (W1b). |
| K-5 | C5 vocabulary is closed and versioned with the RunRecord schema. | Free-form labels with a registry. | S4 freeze. |
| K-6 | A map failing a contract is kept out of the default registry but may still be benchmarked as an opt-in arm with the failure recorded. | Block it entirely until it passes. | First failing map. |

## 5. How a test proves it

The kit is itself tested: each contract has a known-bad fake map (one that leaks a filtered
node, one that keeps paths in its bundle, one that reads past `as_of`, one that misreports
its cost, one without a source label) and the kit must fail each with the right contract id.
A kit that passes a known-bad map is a kit bug. The known-good legacy maps (wrapped in S2)
must pass every applicable contract with no exceptions recorded.

## 6. S2 as built (2026-10-12)

`lci-maps/src/kit.rs` implements C1 (`check_fail_open`) and C6 (`check_determinism`) over a
`Fixture` (view, query text, per-call budget, slack, `rrf_k`) with a subject map and a
control map that returns candidates (the C1 carve-out). C1 runs the subject as it is, switched
off, and under the five injected faults, and requires the fused pool (nodes, score bits,
parts, order) to equal the control-only run, one warning (failure) or one note (deadline)
attributed to the subject and none to another map, and every run to end within the budget
plus the slack. C6 compares the subject's answer and the fused pool across a second run and a
reversed registration; the randomised thread pool is not done yet.

The kit's own tests (`lci-maps/src/tests.rs`): a known-good rank list and prior pass both; a
map that blocks its thread past the deadline fails C1 (the registry's timeout cannot preempt
it, so the search stalls); a map whose answer drifts between calls fails C6 and still passes
C1. The four wrapped legacy maps pass C1 and C6 (`src/app/search/tests/t0.rs`), and so does
the live lexical map over a real directory. C2-C5 and C7-C9 and their known-bad maps are not
written yet.
