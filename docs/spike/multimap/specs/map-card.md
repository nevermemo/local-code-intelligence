# Spec: map card schema

Status: **v0, pre-freeze.** Written 2026-10-12T10:09Z (`date -u`) at slice S0. Docs only.
Schema id `lci.mapcard/0`.

Code read at spike head `df7faaa` (code of `7f9c636`). Labels: **Read** = in the code or docs at
that tree; **Inferred** = designed here.

## 1. Purpose

A map card says, per map: what it costs, what it covers, how fresh it is, what it is good
for, and which contracts it passes (idea N3, [../ideas.md](../ideas.md)). It has two readers:
the traversal (to order and budget map calls, and to show the agent a menu at a checkpoint)
and the index-time storage selection (idea S9). Cards are updated from benchmark RunRecords,
never by hand-typed numbers. Every number carries an evidence label: **M** measured (from
named runs), **A** approximate (derived from measurements), **G** guess.

## 2. What exists today (Read)

| Item | Where | Use |
| --- | --- | --- |
| Inventory rows with estimated costs and update models | [../maps-inventory.md](../maps-inventory.md), "Inventory" (all costs "estimates, not measurements") | Seed the first cards with label G. |
| Per-search stage timings | `Timings`, `src/app/search.rs:28-37`; latency percentiles in `src/evaluate/metrics.rs:397` (`aggregate`) | Source of the first M query costs for the three legacy maps. |
| Per-query metrics and paired deltas | `src/evaluate/metrics.rs:50` (`query_metrics`); `src/evaluate/compare.rs:221`, paired bootstrap `:382` | Source of `good_for` deltas. |
| Readiness of optional dependencies | `App::service_status`, `src/app/readiness.rs:98`; `/ready`, `src/server.rs:271-282` | `needs` and availability. |
| Evaluation tags (`keyword`, `short-query`, ...) | miner, `xtask/src/mine.rs` (architecture-map.md, 8) | Question shapes for `good_for`. |
| Question families in the public seed sets | `eval-seed.json` map kinds; `gold-bugloc.json` `change_kind` | Question types for `good_for`. |

## 3. Design (Inferred)

### 3.1 Two parts

- **Static part**, compiled into the binary from the map's `MapDescriptor`
  ([map-trait.md](map-trait.md)): id, version, contribution kind, node kinds, freshness
  model, source labels, needs.
- **Measured part**, a JSON file per map produced by the harness from RunRecords:
  committed as `docs/spike/multimap/cards/<map-id>.json` for public gold sets only. At run
  time the engine reads the compiled defaults and, if present, a card file under
  `<data_dir>/cards/` (never inside a workspace, `AGENTS.md`). A card file with a different
  `map.version` than the binary's map is ignored with a note.

### 3.2 Schema

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "lci.mapcard/0",
  "type": "object",
  "required": ["schema", "map", "updated_at", "cost", "coverage", "freshness", "good_for", "contracts"],
  "$defs": {
    "evidence": { "enum": ["M", "A", "G"] },
    "num": {
      "type": "object",
      "required": ["value", "label"],
      "properties": {
        "value": { "type": "number" },
        "unit": { "type": "string" },
        "label": { "$ref": "#/$defs/evidence" },
        "runs": { "type": "array", "items": { "type": "string" }, "description": "RunRecord run ids or a harness batch id; required when label is M or A" },
        "n": { "type": "integer", "description": "sample size behind the value" },
        "repo_profile": { "type": "string", "description": "which repository size the number is for, e.g. self or large" }
      }
    },
    "delta": {
      "type": "object",
      "required": ["metric", "vs", "mean", "ci95", "n", "label"],
      "properties": {
        "metric": { "type": "string", "description": "e.g. ndcg@10, recall@8, file_hit@5, first_gold_step" },
        "vs": { "enum": ["no-graph", "random", "churn", "reference"] },
        "mean": { "type": "number" },
        "ci95": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
        "n": { "type": "integer" },
        "label": { "$ref": "#/$defs/evidence" },
        "runs": { "type": "array", "items": { "type": "string" } }
      }
    }
  },
  "properties": {
    "schema": { "const": "lci.mapcard/0" },
    "map": {
      "type": "object",
      "required": ["id", "version", "contribution", "accepts", "emits", "source_labels", "needs"],
      "properties": {
        "id": { "type": "string" },
        "version": { "type": "integer" },
        "title": { "type": "string" },
        "inventory_row": { "type": "string", "description": "row number in maps-inventory.md, e.g. 1, 2a, 13" },
        "contribution": { "enum": ["rank-list", "prior", "filter", "expansion"] },
        "accepts": { "type": "array", "items": { "type": "string" } },
        "emits": { "type": "array", "items": { "type": "string" } },
        "source_labels": { "type": "array", "items": { "type": "string" } },
        "needs": { "type": "array", "items": { "type": "string" } },
        "status": { "enum": ["default", "opt-in", "parked"] },
        "parked_condition": { "type": "string", "description": "copied from the lessons.md parking log" }
      }
    },
    "updated_at": { "type": "string", "format": "date-time", "description": "UTC, from the system clock when the harness wrote the card" },
    "tree": { "type": "string", "description": "commit of the indexed tree behind the measurements" },
    "cost": {
      "type": "object",
      "properties": {
        "build_ms": { "$ref": "#/$defs/num" },
        "update_ms_per_file": { "$ref": "#/$defs/num" },
        "storage_bytes": { "$ref": "#/$defs/num" },
        "resident_bytes": { "$ref": "#/$defs/num" },
        "query_ms_p50": { "$ref": "#/$defs/num" },
        "query_ms_p95": { "$ref": "#/$defs/num" },
        "external_calls_per_query": { "$ref": "#/$defs/num" },
        "est_tokens_per_query": { "$ref": "#/$defs/num" },
        "cold_start_ms": { "$ref": "#/$defs/num" }
      }
    },
    "coverage": {
      "type": "object",
      "properties": {
        "languages": { "type": "array", "items": { "type": "string" } },
        "node_fraction": { "type": "object", "additionalProperties": { "$ref": "#/$defs/num" }, "description": "per node kind: share of the snapshot's nodes the map can return" },
        "edge_precision": { "$ref": "#/$defs/num" },
        "edge_recall": { "$ref": "#/$defs/num" },
        "edge_reference": { "type": "string", "description": "what precision and recall were measured against, e.g. scip at a named commit" },
        "hub_nodes": { "type": "array", "items": { "type": "string" }, "description": "NodeIds the map reaches from most seeds (files every commit touches, lesson L13)" }
      }
    },
    "freshness": {
      "type": "object",
      "required": ["model"],
      "properties": {
        "model": { "enum": ["per-file", "per-commit", "live", "global"] },
        "observed": { "enum": ["snapshot", "live-tree", "as-of"] },
        "staleness_window": { "$ref": "#/$defs/num" },
        "invalidated_by": { "type": "array", "items": { "type": "string" }, "description": "e.g. content hash, adapter version, HEAD move, config change" }
      }
    },
    "good_for": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["question_type", "deltas"],
        "properties": {
          "question_type": { "type": "string", "description": "a map kind of eval-seed, a change_kind of gold-bugloc, a query-shape tag, or a family of a private set by opaque id" },
          "deltas": { "type": "array", "items": { "$ref": "#/$defs/delta" } },
          "condition": { "type": "string", "description": "when the gain appears, e.g. clean seeds only (lesson L11)" }
        }
      }
    },
    "bad_for": { "type": "array", "items": { "type": "object" }, "description": "same shape as good_for, for significant losses" },
    "contracts": {
      "type": "object",
      "description": "C1..C9 from contract-kit.md",
      "propertyNames": { "pattern": "^C[1-9]$" },
      "additionalProperties": {
        "type": "object",
        "required": ["state"],
        "properties": {
          "state": { "enum": ["pass", "fail", "n/a", "not-run"] },
          "test": { "type": "string" },
          "commit": { "type": "string" }
        }
      }
    }
  }
}
```

### 3.3 Update rules

1. Only the harness writes the measured part, from RunRecords of one batch on one tree with a
   frozen binary (`AGENTS.md`, Working method: copy the binary before timed comparisons).
2. A value is M only if every run it cites exists in the batch; A if derived (for example
   scaled to another repository size); G otherwise. A card never upgrades a label without new
   runs.
3. `good_for` lists a question type only when the paired 95% interval against the map's own
   control (contract C7) excludes zero; `bad_for` likewise for losses. Other types are simply
   not listed. A `reference` delta (against a baseline arm such as lexical plus reading) is
   recorded for tracking improvement and never decides `status`.
4. A card for a private gold set stays outside the repository; a committed card names only
   public sets (`eval-seed`, `gold-bugloc`, the mined and core evaluations).
5. Every card update is one commit with the batch id in the message, so `git log` on a card
   is its history.

### 3.4 How the traversal reads a card (MVP)

- Order map calls within a step by `cost.query_ms_p95` (cheap first) when the budget is
  tight; skip a map whose `cost` would exceed the remaining budget, with a note.
- Show the agent at a checkpoint: id, title, `good_for` question types, `cost.query_ms_p50`,
  freshness model, and whether it is stale for this snapshot (the packet budget allows about
  one line per map).
- Never change fusion weights from a card in the MVP. Weight learning from cards is idea U5
  and a later study.

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| D-1 | Measured part committed under `docs/spike/multimap/cards/` (public sets) and loaded at run time from `<data_dir>/cards/`. | Compile cards into the binary at build time (no runtime file, but every benchmark needs a rebuild). | S5. |
| D-2 | `good_for` only with an interval that excludes zero against the map's own control. | List every measured delta and let the reader judge (noisier menus). | First card from S3. |
| D-3 | Cards never set weights in the MVP. | Weights from `good_for` per question type (U5). | Map-to-map study. |
| D-4 | Question types are the public sets' own tags. | One shared taxonomy across public and private sets (needs the private schema mapped first). | Synergy study (W2). |
| D-5 | One card per map version. | One card per (map, repository profile). Partly covered by `repo_profile` on each number. | When a second repository is benchmarked. |

## 5. How a test proves it

| Property | Test |
| --- | --- |
| Schema | Every card the harness writes validates against the schema above; the committed cards are validated in the unit suite. |
| Provenance | A test feeds the card writer a batch and a card claiming M for a run id not in the batch: the writer refuses. |
| No private leakage | The writer refuses to write a card under the repository when any cited run's `item.set` is a private set id. |
| Version guard | A card file for `version = n` next to a binary map of version `n+1` is ignored, and the packet carries a note. |
| Budget use | With a fake map whose card says `query_ms_p95 = 5000` and a remaining budget of 1000 ms, the engine skips it with a note and the RunRecord shows `skipped`. |
| Reference rows | A batch where the map loses to the reference arm but beats its own controls still lists the gain in `good_for` and keeps `status = default`. |
