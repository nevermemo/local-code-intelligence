# Spec: map card schema

Status: **v0, pre-freeze.** Written 2026-10-12 at slice S0. The measured file format below is design only (not built); the in-code
`MapCard` stub of 3.5 is built. Schema id `lci.mapcard/0`. Labels: **Read** = in the code or docs; **Inferred** = designed here; **as built** =
checked against the code at the S1 tree.

## 1. Purpose

A map card says, per map: what it costs, what it covers, how fresh it is, what it is good for, and which contracts it passes (idea N3,
[../ideas.md](../ideas.md)). It has two readers: the traversal (to order and budget map calls, and to show the agent a menu at a checkpoint) and
the index-time storage selection (idea S9). Cards are updated from benchmark RunRecords, never by hand-typed numbers. Every number carries an
evidence label: **M** measured (from named runs), **A** approximate (derived from measurements), **G** guess.

## 2. What exists today (Read)

| Item | Where (symbols) | Use |
| --- | --- | --- |
| Inventory rows with estimated costs and update models | The retired maps inventory (its facts are in [../engine.md](../engine.md), "Maps") | Seed the first cards with label G. |
| Per-search stage timings | `Timings` in `src/app/search.rs`; latency percentiles `aggregate` in `src/evaluate/metrics.rs` | Source of the first M query costs for the three legacy maps. |
| Per-query metrics and paired deltas | `query_metrics` in `src/evaluate/metrics.rs`; paired bootstrap in `src/evaluate/compare.rs` | Source of `good_for` deltas. |
| Readiness of optional dependencies | `App::service_status` (`src/app/readiness.rs`); `/ready` in `src/server.rs` | `needs` and availability. |
| Evaluation tags (`keyword`, `short-query`, ...) | the miner, `xtask/src/mine.rs` | Question shapes for `good_for`. |
| Question families in the public seed sets | `eval-seed.json` map kinds; `gold-bugloc.json` `change_kind` | Question types for `good_for`. |

## 3. Design

### 3.1 Two parts (Inferred)

- **Static part**, compiled into the binary from the map's `MapDescriptor` ([map-trait.md](map-trait.md)): id, version, contribution kind, node
  kinds, freshness model, source labels, needs.
- **Measured part**, a JSON file per map produced by the harness from RunRecords: committed as `docs/spike/multimap/cards/<map-id>.json` for
  public gold sets only. At run time the engine reads the compiled defaults and, if present, a card file under `<data_dir>/cards/` (never inside a
  workspace, `AGENTS.md`). A card file with a different `map.version` than the binary's map is ignored with a note.

### 3.2 Schema (Inferred, not built; JSON Schema 2020-12, `$id` `lci.mapcard/0`)

Required top level: `schema` (const `lci.mapcard/0`), `map`, `updated_at`, `cost`, `coverage`, `freshness`, `good_for`, `contracts`. Optional: `tree`,
`bad_for`.

| Field | Type and meaning |
| --- | --- |
| `evidence` | enum `M`, `A`, `G`. |
| `num` | `{value, label}` required; optional `unit`, `runs` (RunRecord run ids or a harness batch id; required when label is M or A), `n` (sample size), `repo_profile` (which repository size the number is for, for example `self` or `large`). |
| `delta` | `{metric, vs, mean, ci95, n, label}` required; optional `runs`. `metric` is for example ndcg@10, recall@8, file_hit@5, first_gold_step. `vs` is `no-graph`, `random`, `churn` or `reference`. `ci95` is two numbers. |
| `map` | Required `id`, `version` (integer), `contribution` (`rank-list`, `prior`, `filter`, `expansion`), `accepts`, `emits`, `source_labels`, `needs` (string arrays). Optional `title`, `inventory_row` (row in the retired maps inventory), `status` (`default`, `opt-in`, `parked`), `parked_condition` (copied from the [../lessons.md](../lessons.md) parking log). |
| `updated_at` | date-time, UTC, from the system clock when the harness wrote the card. |
| `tree` | Commit of the indexed tree behind the measurements. |
| `cost` | Optional `num` fields: `build_ms`, `update_ms_per_file`, `storage_bytes`, `resident_bytes`, `query_ms_p50`, `query_ms_p95`, `external_calls_per_query`, `est_tokens_per_query`, `cold_start_ms`. |
| `coverage` | `languages` (strings); `node_fraction` (per node kind, a `num`: share of the snapshot's nodes the map can return); `edge_precision` and `edge_recall` (`num`); `edge_reference` (what they were measured against, for example scip at a named commit); `hub_nodes` (NodeIds the map reaches from most seeds, such as files every commit touches, lesson L13). |
| `freshness` | Required `model` (`per-file`, `per-commit`, `live`, `global`); optional `observed` (`snapshot`, `live-tree`, `as-of`), `staleness_window` (`num`), `invalidated_by` (for example content hash, adapter version, HEAD move, config change). |
| `good_for` | Array of `{question_type, deltas}` plus optional `condition` (when the gain appears, for example clean seeds only, lesson L11). `question_type` is a map kind of eval-seed, a `change_kind` of gold-bugloc, a query-shape tag, or a family of a private set by opaque id. |
| `bad_for` | Same shape as `good_for`, for significant losses. |
| `contracts` | Keys `C1` to `C9` ([contract-kit.md](contract-kit.md)); each `{state}` required (`pass`, `fail`, `n/a`, `not-run`), optional `test`, `commit`. |

### 3.3 Update rules (Inferred)

1. Only the harness writes the measured part, from RunRecords of one batch on one tree with a frozen binary (`AGENTS.md`, Working method: copy the
   binary before timed comparisons).
2. A value is M only if every run it cites exists in the batch; A if derived (for example scaled to another repository size); G otherwise. A card
   never upgrades a label without new runs.
3. `good_for` lists a question type only when the paired 95% interval against the map's own control (contract C7) excludes zero; `bad_for` likewise
   for losses. Other types are simply not listed. A `reference` delta (against a baseline arm such as lexical plus reading) is recorded for tracking
   improvement and never decides `status`.
4. A card for a private gold set stays outside the repository; a committed card names only public sets (`eval-seed`, `gold-bugloc`, the mined and
   core evaluations).
5. Every card update is one commit with the batch id in the message, so `git log` on a card is its history.

### 3.4 How the traversal reads a card (Inferred; MVP)

- Order map calls within a step by `cost.query_ms_p95` (cheap first) when the budget is tight; skip a map whose `cost` would exceed the remaining
  budget, with a note.
- Show the agent at a checkpoint: id, title, `good_for` question types, `cost.query_ms_p50`, freshness model, and whether it is stale for this
  snapshot (the packet budget allows about one line per map).
- Never change fusion weights from a card in the MVP. Weight learning from cards is idea U5 and a later study.

### 3.5 The `MapCard` stub (as built, `lci-maps/src/edge.rs`)

`Map::card()` returns `edge::MapCard`, a stub with five fields: `expand_p50_ms: Option<f64>`, `coverage: Vec<NodeKind>`, `freshness_lag:
Option<Duration>`, `good_for: Vec<QuestionType>`, `dependent: bool`. `QuestionType` is Impact, Flow, WhereTo, History, Instructions. The default
`MapCard::from_descriptor` fills only `coverage` (the descriptor's `emits`). The maps `config.key`, `git.history`, `grep.live`, `lsp.live`,
`probe.compile` and `scip.impact` fill `expand_p50_ms` (measured, `git.history` falls back to a prior) and `good_for`. `structure.contains` sets
`dependent: true`; `Registry::independent_expanders` counts maps whose card says `dependent` once together, which bounds the corroboration count of
a node (see [../engine.md](../engine.md), "Walk"). None of the file format, the `cost`/`freshness`/`contracts` blocks, the harness writer or the
`<data_dir>/cards/` loader is built.

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| D-1 | Measured part committed under `docs/spike/multimap/cards/` (public sets) and loaded at run time from `<data_dir>/cards/`. | Compile cards into the binary at build time (no runtime file, but every benchmark needs a rebuild). | S5. |
| D-2 | `good_for` only with an interval that excludes zero against the map's own control. | List every measured delta and let the reader judge (noisier menus). | First card from S3. |
| D-3 | Cards never set weights in the MVP. | Weights from `good_for` per question type (U5). | Map-to-map study. |
| D-4 | Question types are the public sets' own tags. | One shared taxonomy across public and private sets (needs the private schema mapped first). | Synergy study (W2). |
| D-5 | One card per map version. | One card per (map, repository profile). Partly covered by `repo_profile` on each number. | When a second repository is benchmarked. |

## 5. How a test proves it

All rows are design (not built); the stub of 3.5 is covered by per-map tests (`card.good_for`, `expand_p50_ms`) in the map modules.

| Property | Test |
| --- | --- |
| Schema | Every card the harness writes validates against the schema above; the committed cards are validated in the unit suite. |
| Provenance | A test feeds the card writer a batch and a card claiming M for a run id not in the batch: the writer refuses. |
| No private leakage | The writer refuses to write a card under the repository when any cited run's `item.set` is a private set id. |
| Version guard | A card file for `version = n` next to a binary map of version `n+1` is ignored, and the packet carries a note. |
| Budget use | With a fake map whose card says `query_ms_p95 = 5000` and a remaining budget of 1000 ms, the engine skips it with a note and the RunRecord shows `skipped`. |
| Reference rows | A batch where the map loses to the reference arm but beats its own controls still lists the gain in `good_for` and keeps `status = default`. |
