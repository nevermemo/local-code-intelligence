# Spec: RunRecord (trace log per traversal)

Status: **v0, pre-freeze.** Written 2026-10-12T10:06Z (`date -u`) at slice S0. Docs only.
Schema id `lci.runrecord/0`; the first frozen version will be `/1`.

Code read at spike head `df7faaa` (code of `7f9c636`). Labels: **Read** = in the code or docs at
that tree; **Inferred** = designed here.

## 1. Purpose

One RunRecord per traversal: a one-shot search run through the map engine, a scripted
traversal in the harness, or an agent-steered traversal through the handle tools. It is the
only input the harness needs to score a run, to rebuild a map card ([map-card.md](map-card.md))
and to replay steering for study S4 (snapshot-forked counterfactuals). It is written as JSON
Lines: a `run` header line, one `step` line per engine step, and one `end` line, so a run that
dies mid-way still leaves a readable prefix.

## 2. What exists today (Read)

| Item | Where | Reused how |
| --- | --- | --- |
| Candidate dump, one JSON line per search | `CandidateDumpLine`, `src/app/search.rs:116-138`; written by `append_candidate_dump` (`:166-217`) under a process-wide lock (`:145-159`), fail-open | Same writing discipline: one `write_all` per line, append-only, a write failure is a log warning, never a search error. |
| `CandidateTrace` rows | `src/app/search.rs:255-325` | The frontier rows of a step reuse its fields (ranks, `fusion_score`, `capped_rank`, `shortlisted`, `rerank_rank`, `document_bytes`, `est_tokens`, `snippet`) plus `node_id` and per-map contributions. |
| `Timings` (7 fixed fields) | `src/app/search.rs:28-37` | Become per-map `cost.elapsed_ms` plus engine stage timings. |
| `RerankOutcome` | `src/app/search.rs:220-236` | Reused as is for rerank steps. |
| `SearchIndexLifecycle { action, wait_ms }`, `IndexAction` | `src/app/search.rs:39-43`; `src/app/indexing.rs:175-182` | Recorded on every step that re-resolves the snapshot. |
| `EffectiveFilterReport` | reported in `SearchReport.filters` (`search.rs:68`) | Recorded once in the header. |
| Snapshot identity | `Snapshot { identity, dimension, indexed_at }`, `src/store.rs:76-81`; `Manifest.fingerprint`, `lci-core/src/manifest.rs:24` | `snapshot` object; the embedding identity is hashed (it contains the service URL). |
| Paired comparison | `src/evaluate/compare.rs` (pairs by workspace and query, `:295`; bootstrap at `:382`) | The harness pairs RunRecords by `(item.set, item.id, arm)`. |

The dump writes `workspace_path` (`search.rs:121`). A RunRecord does not: it carries the
workspace id only, so records can be shared without machine paths.

## 3. Design (Inferred)

### 3.1 Lines

| `type` | Count | Content |
| --- | --- | --- |
| `run` | 1, first | Who and what: schema, run id, start time, build, workspace id, snapshot, query, filters, plan (maps, weights, policy, budget), evaluation item reference. |
| `step` | 0..n | One engine step: its yield point, the map calls with status, cost and freshness, the frontier summary after the step, steering ops applied before it, notes and warnings raised in it. |
| `end` | 1, last | Final results, totals (engine cost, agent tokens), outcome (`completed`, `expired`, `cancelled`, `failed`), and, when the harness filled it, scores and leak-scan results. |

Yield points (from architecture-map.md, 9.2): `Y1` after fan-out, before fusion; `Y2` after
fuse, filter, cap and cut (the shortlist); `Y3` after rerank; `Y4` after each expansion hop. A
one-shot search records steps for all of them and never pauses.

### 3.2 Privacy rules (enforced by the writer, checked by a test)

- No absolute paths: workspace id only; NodeIds carry repository-relative paths.
- No service URLs: embedding and reranker identities are recorded as SHA-256 hex.
- Code text only as `snippet` (the existing bounded snippet: cut at 400 bytes plus a 3-byte
  ellipsis, `src/store.rs:56-73`), and only when `capture.snippets` is true.
- Private gold sets: `item.set` is an opaque id; records of private items stay outside the
  repository, like every report (`test-results/`, `AGENTS.md`, Working method).

### 3.3 JSON Schema

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "lci.runrecord/0",
  "oneOf": [
    { "$ref": "#/$defs/run" },
    { "$ref": "#/$defs/step" },
    { "$ref": "#/$defs/end" }
  ],
  "$defs": {
    "nodeId": { "type": "string", "pattern": "^(file|chunk|sym|commit|test|cfg|doc|str):" },
    "mapId": { "type": "string", "pattern": "^[a-z][a-z0-9_]*(\\.[a-z0-9_]+)*$" },
    "utc": { "type": "string", "format": "date-time", "description": "RFC 3339, UTC, from the system clock" },
    "hex": { "type": "string", "pattern": "^[0-9a-f]{16,64}$" },
    "label": { "enum": ["vector", "lexical", "lsp", "scip", "tags", "heuristic", "git", "config", "doc", "agent", "prior"] },
    "yield": { "enum": ["Y1", "Y2", "Y3", "Y4", "none"] },
    "cost": {
      "type": "object",
      "required": ["elapsed_ms"],
      "properties": {
        "elapsed_ms": { "type": "number", "minimum": 0 },
        "external_calls": { "type": "integer", "minimum": 0 },
        "nodes_touched": { "type": "integer", "minimum": 0 },
        "bytes_read": { "type": "integer", "minimum": 0 },
        "est_tokens": { "type": "integer", "minimum": 0 }
      },
      "additionalProperties": false
    },
    "snapshot": {
      "type": "object",
      "required": ["indexed_at", "fingerprint"],
      "properties": {
        "indexed_at": { "type": "string" },
        "fingerprint": { "$ref": "#/$defs/hex" },
        "embedding_identity_sha256": { "$ref": "#/$defs/hex" },
        "index_action": { "enum": ["reused", "created", "waited_for_existing_job", "refreshed_incrementally"] },
        "wait_ms": { "type": "number", "minimum": 0 }
      }
    },
    "mapCall": {
      "type": "object",
      "required": ["map", "version", "status", "cost", "freshness", "hits"],
      "properties": {
        "map": { "$ref": "#/$defs/mapId" },
        "version": { "type": "integer", "minimum": 0 },
        "request": {
          "type": "object",
          "properties": {
            "text": { "type": "boolean", "description": "true when the query text was sent; the text itself is in the run line" },
            "seeds": { "type": "array", "items": { "$ref": "#/$defs/nodeId" } },
            "limit": { "type": "integer" },
            "as_of": { "type": "string" },
            "params_sha256": { "$ref": "#/$defs/hex" }
          }
        },
        "status": { "enum": ["ok", "partial", "skipped", "failed"] },
        "message": { "type": "string" },
        "cost": { "$ref": "#/$defs/cost" },
        "freshness": {
          "type": "object",
          "required": ["model", "observed"],
          "properties": {
            "model": { "enum": ["per-file", "per-commit", "live", "global"] },
            "observed": { "enum": ["snapshot", "live-tree", "as-of"] },
            "stale": { "type": "boolean" }
          }
        },
        "hits": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["node", "rank"],
            "properties": {
              "node": { "$ref": "#/$defs/nodeId" },
              "rank": { "type": "integer", "minimum": 1 },
              "score": { "type": ["number", "null"] },
              "evidence": {
                "type": "array",
                "items": {
                  "type": "object",
                  "required": ["label"],
                  "properties": {
                    "via": { "$ref": "#/$defs/nodeId" },
                    "edge": { "type": "string" },
                    "label": { "$ref": "#/$defs/label" },
                    "detail": { "type": "string", "maxLength": 200 }
                  }
                }
              }
            }
          }
        }
      }
    },
    "frontierRow": {
      "type": "object",
      "required": ["node", "score", "contributions"],
      "properties": {
        "node": { "$ref": "#/$defs/nodeId" },
        "start_line": { "type": "integer" },
        "end_line": { "type": "integer" },
        "source_role": { "type": "string" },
        "score": { "type": "number" },
        "contributions": { "type": "object", "additionalProperties": { "type": "number" } },
        "fused_rank": { "type": "integer" },
        "capped_rank": { "type": "integer" },
        "shortlisted": { "type": "boolean" },
        "rerank_rank": { "type": ["integer", "null"] },
        "reranker_score": { "type": ["number", "null"] },
        "est_tokens": { "type": ["integer", "null"] },
        "state": { "enum": ["frontier", "pinned", "pruned", "seen"] },
        "snippet": { "type": "string" }
      }
    },
    "run": {
      "type": "object",
      "required": ["type", "schema", "run_id", "started_at", "build", "workspace_id", "snapshot", "query", "plan"],
      "properties": {
        "type": { "const": "run" },
        "schema": { "const": "lci.runrecord/0" },
        "run_id": { "type": "string", "description": "ULID; sortable by start time" },
        "parent_run_id": { "type": "string", "description": "set for a fork (S4 counterfactual replay, N2 fork/join)" },
        "started_at": { "$ref": "#/$defs/utc" },
        "build": {
          "type": "object",
          "required": ["version"],
          "properties": {
            "version": { "type": "string", "description": "CARGO_PKG_VERSION" },
            "commit": { "type": "string" },
            "dirty": { "type": "boolean" }
          }
        },
        "workspace_id": { "$ref": "#/$defs/hex" },
        "tree_commit": { "type": "string", "description": "HEAD of the indexed tree when known" },
        "snapshot": { "$ref": "#/$defs/snapshot" },
        "endpoint": { "type": "string", "description": "/mcp, /mcp/<profile>, /v1, cli or harness" },
        "search_profile": { "type": "string" },
        "query": {
          "type": "object",
          "required": ["text"],
          "properties": {
            "text": { "type": "string" },
            "shape": { "type": "array", "items": { "type": "string" }, "description": "keyword, descriptive, short-query, long-query, ... (lesson L13)" }
          }
        },
        "filters": { "type": "object" },
        "plan": {
          "type": "object",
          "required": ["maps", "policy"],
          "properties": {
            "maps": {
              "type": "array",
              "items": {
                "type": "object",
                "required": ["map", "version", "weight"],
                "properties": {
                  "map": { "$ref": "#/$defs/mapId" },
                  "version": { "type": "integer" },
                  "weight": { "type": "number" },
                  "card_sha256": { "$ref": "#/$defs/hex" },
                  "params": { "type": "object" }
                }
              }
            },
            "rrf_k": { "type": "number" },
            "max_chunks_per_file": { "type": "integer" },
            "rerank_candidate_count": { "type": "integer" },
            "policy": {
              "type": "object",
              "required": ["kind"],
              "properties": {
                "kind": { "enum": ["one-shot", "scripted", "agent"] },
                "script": { "type": "string", "description": "id and version of a scripted steering policy" },
                "pause_at": { "type": "array", "items": { "$ref": "#/$defs/yield" } }
              }
            },
            "budget": {
              "type": "object",
              "properties": {
                "deadline_ms": { "type": "integer" },
                "max_steps": { "type": "integer" },
                "max_nodes": { "type": "integer" },
                "max_engine_tokens": { "type": "integer" },
                "max_agent_tokens": { "type": "integer" }
              }
            },
            "seed": { "type": "integer", "description": "RNG seed for random controls" },
            "arm": { "type": "string", "description": "harness arm name, e.g. map-on, no-graph, random, churn, reference" }
          }
        },
        "item": {
          "type": "object",
          "required": ["set", "id"],
          "properties": {
            "set": { "type": "string", "description": "public set name or an opaque id for a private set" },
            "id": { "type": "string" },
            "base_commit": { "type": "string" }
          }
        },
        "capture": {
          "type": "object",
          "properties": {
            "snippets": { "type": "boolean" },
            "frontier_rows": { "type": "integer", "description": "most frontier rows written per step" }
          }
        }
      }
    },
    "step": {
      "type": "object",
      "required": ["type", "run_id", "seq", "at_ms", "kind", "yield"],
      "properties": {
        "type": { "const": "step" },
        "run_id": { "type": "string" },
        "seq": { "type": "integer", "minimum": 1 },
        "at_ms": { "type": "number", "description": "ms since started_at, monotonic clock" },
        "kind": { "enum": ["resolve", "fan_out", "fuse", "cap_cut", "rerank", "expand", "pause", "steer", "resume", "end"] },
        "yield": { "$ref": "#/$defs/yield" },
        "snapshot": { "$ref": "#/$defs/snapshot", "description": "present when the step re-resolved the snapshot (every resume)" },
        "drift": { "type": "object", "description": "nodes dropped or remapped after a snapshot change", "properties": { "dropped": { "type": "array", "items": { "$ref": "#/$defs/nodeId" } }, "remapped": { "type": "integer" } } },
        "ops": { "type": "array", "items": { "type": "object" }, "description": "validated steering ops applied before this step (steering-ops.md)" },
        "agent_tokens": { "type": "integer", "minimum": 0, "description": "tokens the agent reported spending to decide the ops" },
        "map_calls": { "type": "array", "items": { "$ref": "#/$defs/mapCall" } },
        "engine_cost": { "$ref": "#/$defs/cost" },
        "rerank_outcome": { "enum": ["scored", "failed", "off", "not_run"] },
        "frontier": {
          "type": "object",
          "required": ["size"],
          "properties": {
            "size": { "type": "integer" },
            "rows": { "type": "array", "items": { "$ref": "#/$defs/frontierRow" } }
          }
        },
        "warnings": { "type": "array", "items": { "type": "string" } },
        "notes": { "type": "array", "items": { "type": "string" } }
      }
    },
    "end": {
      "type": "object",
      "required": ["type", "run_id", "finished_at", "outcome", "results", "totals"],
      "properties": {
        "type": { "const": "end" },
        "run_id": { "type": "string" },
        "finished_at": { "$ref": "#/$defs/utc" },
        "outcome": { "enum": ["completed", "expired", "cancelled", "failed"] },
        "error": { "type": "string" },
        "results": { "type": "array", "items": { "$ref": "#/$defs/frontierRow" } },
        "totals": {
          "type": "object",
          "required": ["elapsed_ms", "steps"],
          "properties": {
            "elapsed_ms": { "type": "number" },
            "steps": { "type": "integer" },
            "engine": { "$ref": "#/$defs/cost" },
            "agent_tokens": { "type": "integer" },
            "per_map": { "type": "object", "additionalProperties": { "$ref": "#/$defs/cost" } }
          }
        },
        "evaluation": {
          "type": "object",
          "description": "filled by the harness, never by the engine",
          "properties": {
            "metrics": { "type": "object", "additionalProperties": { "type": "number" } },
            "first_gold_step": { "type": ["integer", "null"] },
            "unique_gold_by_map": { "type": "object", "additionalProperties": { "type": "integer" } },
            "leak_scan": { "enum": ["passed", "failed", "not_applicable"] }
          }
        }
      }
    }
  }
}
```

### 3.4 Where records go

- Engine: off by default. A config switch or an environment variable (like
  `LCI_CANDIDATE_DUMP`, `search.rs:97`) names a JSONL file or directory; one file per run
  (`<run_id>.jsonl`) under `test-results/` for the harness. Fail-open like the dump.
- Handle tools: the `end` line is also summarised in the `traverse_*` tool response
  ([steering-ops.md](steering-ops.md)); the full record is never sent to the client unless
  asked, because the frontier rows are large (the dump is about 100 KB per query,
  architecture-map.md, 9.4).

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| R-1 | JSON Lines with `run`/`step`/`end` lines. | One JSON document per run (simpler to validate, lost on a crash). | S3. |
| R-2 | The query text is stored in full. | Store only its hash plus shape tags for private items (private records stay outside the repository anyway). | First private-gold run. |
| R-3 | Frontier rows per step capped by `capture.frontier_rows` (default 50); `size` is always exact. | Full frontier every step (exact replay of every step, larger files). | S4 counterfactual study. |
| R-4 | Map hits stored per call, so leave-one-map-out can be replayed offline. | Store only fused contributions (smaller; no per-map replay). | S3. |
| R-5 | `agent_tokens` are self-reported by the caller of `traverse_steer`. | Measure from the agent client's own logs in the harness. | Map-to-map study. |
| R-6 | ULID run ids. | UUIDv7. Same properties; ULID chosen for shorter strings. | none |
| R-7 | Engine timings are per map and per step; the legacy 7-field `Timings` is a projection for `search_code`. | Extend `Timings` with a map-keyed object. | S5. |

## 5. How a test proves it

| Property | Test |
| --- | --- |
| Schema validity | Every RunRecord the offline test suites write validates against the schema above (a JSON Schema validator as a dev-dependency, or a hand-written checker in the harness if no validator is already in the tree). |
| One-shot equivalence | For a one-shot run, `end.results` equals `SearchReport.results` of the same search (NodeIds, order, `fusion_score` bits) and the steps' frontier rows match the `CandidateTrace` rows of a traced search. |
| Crash safety | Kill a run after step 2 (fault-injected map panic in the writer test): the file holds a valid `run` line and two valid `step` lines; the harness reports `outcome` missing, not a parse error. |
| Privacy | A writer test asserts that no string field contains the workspace's absolute path, the data directory, or the configured service URLs, and that snippets are absent when `capture.snippets` is false. |
| Replay | Re-running fusion from the recorded `map_calls` hits and the plan's weights reproduces each `fuse` step's scores bit for bit (extends `src/evaluate/replay.rs` beyond three channels). |
| Fail-open writer | Point the writer at an unwritable path: the search succeeds, a log warning is emitted, nothing else changes. |
