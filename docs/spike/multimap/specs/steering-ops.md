# Spec: steering op vocabulary v1 and resumable handles

Status: **v0, pre-freeze.** Written 2026-10-12T10:07Z (`date -u`) at slice S0. Docs only. The
op vocabulary is "v1" as a vocabulary; the spec around it is v0 and freezes with the Map
trait at S4. The engine that executes it lands in S5.

Code read at spike head `df7faaa` (code of `7f9c636`). Labels: **Read** = in the code or docs at
that tree; **Inferred** = designed here.

## 1. Purpose

A traversal can stop at checkpoints so a thinking agent can inspect it and steer it (the
owner's core idea, [../ideas.md](../ideas.md); idea S7: "the steering verbs are validated
patches"). This spec fixes the six ops of v1 (pause, prune, expand, add a map, drop a map,
reweight), the patch envelope that carries them, the validation rules, and the resumable
handle they act on, with the three tools that expose it over MCP and REST.

## 2. What exists today (Read)

| Item | Where | Consequence |
| --- | --- | --- |
| Stateless tools | `#[tool_router] impl McpServer`, `src/server.rs:89-200`; `McpServer` holds only `app`, the router and `search_profile` (`:10-19`) | Handle state must live in `App`, not in the MCP server object. |
| One endpoint per search profile, each with its own session manager | `mcp_service`, `src/server.rs:220-251` ("a session id minted by one endpoint is unknown to every other", `:220-223`); `mcp_path` `:213-218`; mounted per profile `:293-306`; keep-alive 1 h `:243-244` | A handle is bound to the endpoint that created it, the same way. |
| REST mirror reusing the MCP argument structs | `src/rest.rs:1-8`, `:10-13`; routes `:262-279`; hand-assembled OpenAPI `:171-260`; `search_profile` as a query parameter `:122-131` | New tools get `POST /v1/traverse_*` routes and OpenAPI entries built from the same `schemars` structs. |
| Server-side state keyed by id | `App.watchers: Mutex<HashMap<String, CancellationToken>>`, `src/app/mod.rs:50` | Precedent for a handle registry in `App`. |
| Read guard held for a whole search | `IndexResolution._read_guard`, `src/app/search.rs:517-527`; per-workspace `RwLock` in `WorkspaceCoordination`, `src/app/mod.rs:31-41` | A paused traversal must not hold it between calls, or indexing blocks. |
| Snapshot replaced whole | `Store::replace` with overwrite (`src/store.rs:220`; architecture-map.md, 5) | A paused traversal cannot pin old rows; it re-resolves on resume. |
| One search is a straight line of stages | `search_once`, `src/app/search.rs:637-1015`: resolve `:656-688`, universe `:689-710`, fan-out `:712-906`, fuse/filter/sort `:932-941`, cap/cut `:962-972`, rerank `:974-996`, final filter `:998` | The yield points Y1-Y4 split it (run-record.md, 3.1). |
| Errors as tool errors, results as structured JSON | `result`, `src/server.rs:72-77`; `respond`, `src/rest.rs:25-33` | A rejected patch is a structured result (`applied: false`), not a tool error; an unknown or expired handle is a tool error. |

## 3. Design (Inferred)

### 3.1 The handle

```text
Handle {
  id:          128 random bits, base32 (unguessable; never derived from the query or workspace)
  endpoint:    "/mcp" | "/mcp/<profile>" | "/v1" | "/v1?search_profile=<p>"
  workspace_id
  snapshot:    { indexed_at, fingerprint }   // the one the last step read
  seq:         u32                           // increments on every accepted continue or steer
  state:       paused(Yn) | finished(outcome) // never "running" between calls
  expires_at:  UTC
  plan, frontier, seen, pruned, pins, budget_left, run_id
}
```

- Registry: `App.traversals: Mutex<HashMap<HandleId, Arc<tokio::sync::Mutex<Traversal>>>>`,
  next to `watchers`. One call at a time per handle: a second concurrent call gets a `busy`
  error at once instead of queueing.
- Lifetime: idle TTL (default 600 s), absolute cap (default 3600 s, the MCP keep-alive), at
  most 32 live handles per process and 8 per workspace. All four numbers are guesses to
  measure (labelled G on the map card's engine row). Creating a handle beyond a cap is an
  error that names the cap; nothing is evicted silently except expired handles.
- Binding: a handle is usable only on the endpoint and workspace it was created for. Any other
  use answers "unknown handle", the same text as a truly unknown id, so ids cannot be probed.
- Locks: each `traverse_continue` acquires the workspace read guard, runs steps up to the next
  pause, and drops the guard before it returns. `traverse_steer` takes no index lock: it only
  edits the handle.
- Resume and drift: each `traverse_continue` re-resolves the snapshot under the freshness
  policy (`resolve_index_for_search`, `src/app/search.rs:1022`). If the snapshot changed,
  frontier, seen, pruned and pinned NodeIds are kept when the node still exists (chunk ids
  survive line moves, [node-id.md](node-id.md)) and dropped otherwise; the step records the
  drift (run-record.md, `drift`) and adds a note. `plan.on_drift = "fail"` ends the
  traversal instead.

### 3.2 Tools (MCP, with REST mirrors)

| Tool | Arguments | Result |
| --- | --- | --- |
| `traverse_start` | `workspace_path`, `query`, the `search_code` filters (`languages`, `include_paths`, `exclude_paths`, `source_roles`), optional `plan` (`maps` with weights, `pause_at` list of yield points, `budget`, `frontier_rows`, `on_drift`) | A packet (3.4) paused at the first requested yield point, or finished results when `pause_at` is empty (then it equals `search_code`). |
| `traverse_continue` | `handle`, `expect_seq`, optional `until` (`next` default, a yield point, or `end`), optional `budget` top-up within the caps | The next packet, or the final results when the traversal ends. `until = end` runs to completion without further pauses. |
| `traverse_steer` | `handle`, `expect_seq`, `ops` (1-16 ops), optional `agent_tokens`, optional `reason` (free text, logged, bounded to 500 bytes) | `{ applied, seq, errors[], frontier_delta }`. No map runs during a steer; ops that need map work (expand, add_map) run at the next continue. |

REST: `POST /v1/traverse_start`, `/v1/traverse_continue`, `/v1/traverse_steer`, each with
the same `schemars` argument struct as the MCP tool, the `search_profile` query parameter on
`traverse_start`, and an OpenAPI entry next to the existing ones (`src/rest.rs:235-258`). The
tools appear on every `/mcp/<profile>` endpoint, like every other tool.

Ending a traversal without consuming it: `traverse_continue` with `until = end` and
`budget.max_steps = 0` returns the current frontier as results and releases the handle. No
separate end tool in v1.

### 3.3 Ops v1

Every op is a JSON object with an `op` field. Ops in one patch apply in order; a later op may
refer to the effect of an earlier one (for example `add_map` then `expand` through it).

| Op | Fields | Effect |
| --- | --- | --- |
| `pause` | `at`: list of yield points (`Y1`..`Y4`), or `[]` to clear | Replaces the pause set used by later continues. |
| `prune` | `nodes`: NodeIds, and/or `where`: `{ paths: globs, roles: source roles, only_from: MapId, below_score: number }` | Moves matching frontier nodes to `pruned`. A pruned node is never re-added by any map in this traversal (idea NM, negative maps). `where` cannot widen the run's filter; it only removes. |
| `expand` | `nodes`: NodeIds (each in frontier, seen or pinned), `via`: MapId of an `Expansion` map, `depth`: 1..3, `limit`: 1..40, optional `params` | Schedules one Y4 hop per depth level at the next continue; new nodes pass the run filter first (contract C2) and carry `via` evidence. |
| `add_map` | `map`: MapId, `weight`: 0..10, optional `params` | Adds a registered map to the plan from the next step on; a text map runs on the query, an expansion map waits for `expand`. |
| `drop_map` | `map`: MapId | Removes the map from later steps; its past contributions stay in the frontier scores and in the RunRecord. |
| `reweight` | `map`: MapId (in the plan), `weight`: 0..10 | Changes the map's RRF weight; the frontier is re-fused at the next continue from the recorded per-map ranks (no map calls). After a Y3 pause the recorded rerank order is then stale: the next continue re-runs cap, cut and rerank (Y2 to Y3) on the re-fused list and records a new rerank step. |

Deliberately not in v1: pin, sub-query text, shortlist size, fork (idea N2), free-form
expressions. Each is an open decision below.

### 3.4 Validation (all or nothing)

A patch is applied only if every op passes; otherwise nothing changes and `errors` lists each
failing op with its index and a code:

| Code | Rule |
| --- | --- |
| `stale_seq` | `expect_seq` differs from the handle's `seq` (optimistic concurrency, like a JSON Patch `test`). |
| `unknown_op` / `bad_field` | Schema check: unknown op, missing or extra field, wrong type. |
| `unknown_node` | A NodeId that does not parse, or is not in frontier, seen or pinned (for `expand`) or in frontier (for `prune` by id). |
| `unknown_map` / `wrong_kind` | A MapId not registered, or `expand.via` not an `Expansion` map, or `reweight` of a map not in the plan. |
| `out_of_range` | Weight not finite or outside 0..10; depth or limit outside range; more than 16 ops. |
| `over_budget` | The patch would schedule more map calls or nodes than the remaining budget allows. |
| `would_widen_filter` | Any op that would admit nodes outside the run's filter. |
| `finished` | The traversal has ended. |

Validation never reads chunk text, and op fields are never filled from retrieved text by the
engine: ops are control input from the client, chunk text is data (`AGENTS.md`, Hard rules).

### 3.5 Packet (what a paused step returns)

Bounded to about 1k tokens by default (idea S7): handle, `seq`, paused yield point, index
lifecycle (`action`, `wait_ms`), budget left, per-map status for the last step (status, hits,
elapsed), up to `frontier_rows` rows (default 12) of NodeId, path, line range, role, score,
per-map contributions and a short snippet (the existing bounded snippet cut further), the
frontier size, counts of seen and pruned nodes, and the step's warnings and notes. The full
frontier is in the RunRecord, not in the packet.

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| S-1 | Three tools; ending is `continue(until=end)`. | A fourth `traverse_end` tool (clearer, one more tool on every endpoint). | S5. |
| S-2 | Steer never runs maps; work happens at the next continue. | Execute cheap ops (prune, reweight) immediately and return the new packet (one fewer round trip). | S5, after measuring round trips. |
| S-3 | No `pin` in v1. | Add `pin` (protect nodes from prune and the cut). Likely v2; the handle already has a `pins` set. | First scripted-agent study. |
| S-4 | No sub-query op in v1. | `add_query { text }` that runs text maps on extra text (idea N1, ephemeral maps). | W4 (ephemeral maps). |
| S-5 | Defaults TTL 600 s, cap 3600 s, 32 per process, 8 per workspace. | Any other numbers; all are guesses until measured. | S5 load test. |
| S-6 | On snapshot drift: keep surviving nodes, drop the rest, note it. | Fail the traversal on any drift (simpler semantics, worse for long agent pauses under `on-search`). Available as `on_drift = fail`. | S5. |
| S-7 | Concurrent calls on one handle get `busy`. | Queue them. | S5. |
| S-8 | Handle ids are random and endpoint-bound; a wrong endpoint looks unknown. | Bind to the MCP session id as well (stronger, but REST has no session). | S5 security review. |

## 5. How a test proves it

All offline, through `tests/cases/mcp.rs`-style tests with the fake model service and fake LSP.

| Property | Test |
| --- | --- |
| One-shot equivalence | `traverse_start` with no pauses returns the same results (NodeIds, order, scores) as `search_code` with the same arguments. |
| Pause and resume equivalence | A traversal paused at Y1, Y2 and Y3 and resumed with no ops returns the same results as the one-shot run. |
| No guard held across calls | Pause a traversal, then run `index_workspace` on the same workspace: it completes without waiting for the handle. |
| Drift | Pause, edit a fixture file and reindex, continue: surviving chunk NodeIds remain in the frontier, the edited chunk's old id is reported in `drift.dropped`, and a note is present. |
| Validation is atomic | A patch whose third op is invalid leaves the handle unchanged (`seq` and frontier equal) and reports exactly that op's index and code. |
| Stale seq | Two steers with the same `expect_seq`: the second gets `stale_seq`. |
| Filter cannot widen | `expand` from a node into a filtered-out role adds nothing (contract C2); `prune.where` with a broader glob is accepted but only removes. |
| Endpoint binding | A handle from `/mcp/<profile>` used on `/mcp` or on REST gets the same "unknown handle" error as a random id. |
| TTL and caps | With a test clock, an idle handle expires after the TTL; the 9th handle on one workspace is refused with a message naming the cap. |
| Reweight replay | `reweight` then continue gives the fused scores that fusion produces from the recorded per-map ranks with the new weight (bit-exact). |
| Data is not instructions | A fixture chunk whose text is a valid ops JSON never changes the plan unless the client sends it. |
