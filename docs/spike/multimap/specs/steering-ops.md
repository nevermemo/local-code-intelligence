# Spec: steering op vocabulary v1 and resumable handles

Status: **v0, pre-freeze. Built: the steering ladder, the `steer` object and file-backed handles (3.6). Design only, not built: the three `traverse_*` tools, the ops v1 patch envelope, its validation codes and the
packet (3.1 to 3.5).** Written 2026-10-12 at slice S0. The op vocabulary is "v1" as a vocabulary; the spec around it is v0 and freezes with the Map trait at S4. Labels (**Read**, **Inferred**, **built**) are defined
in [README.md](README.md). The shipped steering is summarised in [../engine.md](../engine.md), "Steering".

## 1. Purpose

A traversal can stop at checkpoints so a thinking agent can inspect it and steer it (the owner's core idea, [../ideas.md](../ideas.md); idea S7: "the steering verbs are validated patches"). This spec fixes the six
ops of v1 (pause, prune, expand, add a map, drop a map, reweight), the patch envelope that carries them, the validation rules, and the resumable handle they act on, with the three MCP and REST tools that expose it.

## 2. What exists today (Read)

Facts about the server and search code the design builds on (symbols, checked on the spike tree): the MCP tools are stateless (`#[tool_router] impl McpServer`, `src/server.rs`; `McpServer` holds only `app`, the
router and `search_profile`), so handle state must live in `App` or in files. There is one MCP endpoint per search profile, each with its own session manager (`mcp_service`, `mcp_path`; "a session id minted by one
endpoint is unknown to every other"; keep-alive 1 h), so a handle is bound to its endpoint the same way. `src/rest.rs` mirrors the MCP argument structs (routes, a hand-assembled OpenAPI document, `search_profile` as
a query parameter), so new tools get `POST /v1/traverse_*` routes built from the same `schemars` structs. `App.watchers: Mutex<HashMap<String, CancellationToken>>` (`src/app/mod.rs`) is the precedent for server-side
state keyed by id. A search holds the per-workspace read guard for its whole length (`IndexResolution._read_guard`, `WorkspaceCoordination`), so a paused traversal must not hold it between calls, and
`Store::replace` overwrites the table, so it cannot pin old rows and re-resolves on resume. Errors are tool errors and results structured JSON (`result` in `src/server.rs`, `respond` in `src/rest.rs`): a rejected
patch is a structured result (`applied: false`); an unknown or expired handle is a tool error. One search is a straight line of stages in `search_once` (`src/app/search.rs`: resolve, universe, fan-out,
fuse/filter/sort, cap/cut, rerank, final filter); the yield points Y1-Y4 split it (run-record.md, 3.1).

## 3. Design (Inferred; 3.1 to 3.5 not built)

### 3.1 The handle (design)

Fields: `id` (128 random bits, base32; unguessable, never derived from the query or workspace), `endpoint` (`/mcp`, `/mcp/<profile>`, `/v1`, `/v1?search_profile=<p>`), `workspace_id`, `snapshot` (`{ indexed_at,
fingerprint }`, the one the last step read), `seq` (`u32`, increments on every accepted continue or steer), `state` (`paused(Yn)` or `finished(outcome)`, never "running" between calls), `expires_at` (UTC), and
`plan`, `frontier`, `seen`, `pruned`, `pins`, `budget_left`, `run_id`.

- Registry: `App.traversals: Mutex<HashMap<HandleId, Arc<tokio::sync::Mutex<Traversal>>>>`, next to `watchers`. One call at a time per handle: a second concurrent call gets `busy` at once.
- Lifetime: idle TTL (default 600 s), absolute cap (default 3600 s, the MCP keep-alive), at most 32 live handles per process and 8 per workspace. All four numbers are guesses to measure (labelled G on the map card's
  engine row). Creating a handle beyond a cap is an error that names the cap; nothing is evicted silently except expired handles.
- Binding: a handle is usable only on the endpoint and workspace it was created for. Any other use answers "unknown handle", the same text as a truly unknown id, so ids cannot be probed.
- Locks: `traverse_continue` holds the workspace read guard only while it runs steps to the next pause; `traverse_steer` takes no index lock (it only edits the handle).
- Resume and drift: each `traverse_continue` re-resolves the snapshot under the freshness policy (`resolve_index_for_search`). If the snapshot changed, frontier, seen, pruned and pinned NodeIds are kept when the
  node still exists (chunk ids survive line moves, [node-id.md](node-id.md)) and dropped otherwise; the step records the drift (run-record.md, `drift`) and adds a note. `plan.on_drift = "fail"` ends it.

### 3.2 Tools (MCP, with REST mirrors; design)

| Tool | Arguments | Result |
| --- | --- | --- |
| `traverse_start` | `workspace_path`, `query`, the `search_code` filters (`languages`, `include_paths`, `exclude_paths`, `source_roles`), optional `plan` (`maps` with weights, `pause_at` list of yield points, `budget`, `frontier_rows`, `on_drift`) | A packet (3.5) paused at the first requested yield point, or finished results when `pause_at` is empty (then it equals `search_code`). |
| `traverse_continue` | `handle`, `expect_seq`, optional `until` (`next` default, a yield point, or `end`), optional `budget` top-up within the caps | The next packet, or the final results when the traversal ends. `until = end` runs to completion without further pauses. |
| `traverse_steer` | `handle`, `expect_seq`, `ops` (1-16 ops), optional `agent_tokens`, optional `reason` (free text, logged, bounded to 500 bytes) | `{ applied, seq, errors[], frontier_delta }`. No map runs during a steer; ops that need map work (expand, add_map) run at the next continue. |

REST: `POST /v1/traverse_start`, `/v1/traverse_continue`, `/v1/traverse_steer`, each with the same `schemars` argument struct as the MCP tool, the `search_profile` query parameter on `traverse_start`, and an OpenAPI
entry next to the existing ones. The tools appear on every `/mcp/<profile>` endpoint. Ending a traversal without consuming it: `traverse_continue` with `until = end` and `budget.max_steps = 0` returns the current
frontier as results and releases the handle. No separate end tool in v1.

### 3.3 Ops v1 (design)

Every op is a JSON object with an `op` field. Ops in one patch apply in order; a later op may refer to the effect of an earlier one (for example `add_map` then `expand` through it). Not in v1: pin (S-3), sub-query
text (S-4), shortlist size, fork (idea N2), free-form expressions.

| Op | Fields | Effect |
| --- | --- | --- |
| `pause` | `at`: list of yield points (`Y1`..`Y4`), or `[]` to clear | Replaces the pause set used by later continues. |
| `prune` | `nodes`: NodeIds, and/or `where`: `{ paths: globs, roles: source roles, only_from: MapId, below_score: number }` | Moves matching frontier nodes to `pruned`. A pruned node is never re-added by any map in this traversal (idea NM, negative maps). `where` cannot widen the run's filter; it only removes. |
| `expand` | `nodes`: NodeIds (each in frontier, seen or pinned), `via`: MapId of an `Expansion` map, `depth`: 1..3, `limit`: 1..40, optional `params` | Schedules one Y4 hop per depth level at the next continue; new nodes pass the run filter first (contract C2) and carry `via` evidence. |
| `add_map` | `map`: MapId, `weight`: 0..10, optional `params` | Adds a registered map to the plan from the next step on; a text map runs on the query, an expansion map waits for `expand`. |
| `drop_map` | `map`: MapId | Removes the map from later steps; its past contributions stay in the frontier scores and in the RunRecord. |
| `reweight` | `map`: MapId (in the plan), `weight`: 0..10 | Changes the map's RRF weight; the frontier is re-fused at the next continue from the recorded per-map ranks (no map calls). After a Y3 pause the recorded rerank order is then stale: the next continue re-runs cap, cut and rerank (Y2 to Y3) on the re-fused list and records a new rerank step. |

### 3.4 Validation (all or nothing; design)

A patch is applied only if every op passes; otherwise nothing changes and `errors` lists each failing op with its index and a code. `stale_seq`: `expect_seq` differs from the handle's `seq` (optimistic concurrency,
like a JSON Patch `test`). `unknown_op` / `bad_field`: schema check (unknown op, missing or extra field, wrong type). `unknown_node`: a NodeId that does not parse, or is not in frontier, seen or pinned (for
`expand`) or in frontier (for `prune` by id). `unknown_map` / `wrong_kind`: a MapId not registered, or `expand.via` not an `Expansion` map, or `reweight` of a map not in the plan. `out_of_range`: weight not finite
or outside 0..10; depth or limit outside range; more than 16 ops. `over_budget`: the patch would schedule more map calls or nodes than the remaining budget allows. `would_widen_filter`: any op that would admit nodes
outside the run's filter. `finished`: the traversal has ended. Validation never reads chunk text, and op fields are never filled from retrieved text by the engine: ops are control input from the client, chunk text
is data (`AGENTS.md`, Hard rules).

### 3.5 Packet (design)

What a paused step returns, bounded to about 1k tokens by default (idea S7): handle, `seq`, paused yield point, index lifecycle (`action`, `wait_ms`), budget left, per-map status for the last step (status, hits,
elapsed), up to `frontier_rows` rows (default 12) of NodeId, path, line range, role, score, per-map contributions and a short snippet (the existing bounded snippet cut further), the frontier size, counts of seen and
pruned nodes, and the step's warnings and notes. The full frontier is in the RunRecord, not in the packet.

### 3.6 Built: the ladder, `steer` and handles (`lci-maps/src/traverse/`, `src/app/traverse.rs`)

What shipped is a different, smaller surface than 3.1 to 3.5, reached through `search_code mode="traverse"` and `lci traverse` (no new tools, no `App.traversals`):

- **Stops.** A `StopPolicy` (`stop.rs`) has the triggers `every_pops`, `budget_share`, `on_disagreement`, `on_stagnation` and `likely_complete` (Lincoln-Petersen estimate), bounded by `max_stops`; a budget hit is
  not a trigger, it ends the call and returns the checkpoint. Each call writes `<data_dir>/traces/<name>.jsonl` with `stop`, `steer` (rung, handler, confidence, actions, outcome `applied`, `uncertain` or `returned`)
  and `return` events ([../engine.md](../engine.md), "RunRecord and traces").
- **Ladder** (`ladder.rs`): rung 1 `RuleList` (the agent's rules as a closed JSON AST: at most `MAX_RULES` = 64 rules, nesting `MAX_NESTING` = 8, `MAX_STEPS` = 200_000 predicate evaluations per stop, wall-time
  backstop `EVAL_CAP` = 250 ms); rung 2 System One (a slot, a test double until S8); rung 3 the outer agent. The first handler whose decision has `confidence >= tau` decides; when none is confident the call ends
  `Stopped` and returns the `checkpoint` and a `handle`, and nothing of a lower rung is applied. The request exposes rungs `rules` and `agent` only.
- **Actions** (`Directive`s, `handler.rs`): `continue`, `prune`, `expand`, `boost` (up to `BOOST_MAX` = 8), `map_on`, `map_off`, `map_weight`, `redirect` (up to `REDIRECT_MAX` = 16 node ids), `emit` (note up to
  `EMIT_MAX` = 200), `stop`; and, only in a `steer`, `budget` ({pops, tokens}), capped in total by `RAISE_MAX_POPS` = 200 and `RAISE_MAX_TOKENS` = 20_000. Against the v1 ops: `prune` = prune, `expand` = expand
  (reorders the frontier), `map_off`/`map_on` = drop_map/add_map for registered maps, `map_weight` = reweight; there is no `pause` (stops are triggers), no `pin`, no sub-query. A rule list cannot raise a cap.
- **`steer` object** (`SteerArgs`): `policy`, `rules`, `script` (reserved, not available), `ladder`, `max_stops`, `tau`, `stop_every`, `on_disagreement`, `likely_complete`, `actions` (only with `handle`: `[{"node"?,
  "action"}]`, at most `MAX_STEER_ACTIONS` = 64). A lint (`lint.rs`) turns what the types decide before the run (a rule that prunes every node, a rule that cannot fire, two rules with one match and opposite actions,
  a `stop_every` above the pops, a stop count of zero) into response `warnings`; it never errors.
- **Handles** (`handle.rs`): a stopped or budget-limited traversal leaves a `Snapshot` of its state, a `HandleStore` keeps it as `<data_dir>/traverse/handles/<handle>.json` under the hash of its JSON (equal runs
  give equal handles; a stored file is checked on load), and `Traversal::resumed` continues from it. A handle is bound to the workspace and index it was made on (`check_identity`). Budgets (`pops`, `est_tokens`,
  `external_calls`) are absolute for the whole traversal; `deadline` and `max_depth` belong to the call; what the agent raised stays with the traversal. A resume refuses a changed corroboration boost, score-bound
  `k`, hop priors, tie-break file or reranker identity. Handle text is at most 64 characters. There is no TTL, cap on live handles, endpoint binding or `seq`, and no drift handling beyond the
  identity check. S-1 to S-8 describe the design of 3.1 to 3.5 only.

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

Design tests for 3.1 to 3.5, all offline, through `tests/cases/mcp.rs`-style tests with the fake model service and fake LSP; none is written. The built ladder, rule list, steer, handle and lint are tested in
`lci-maps/src/traverse/tests.rs` and the module test files (budgets, determinism, fail-open, the effect of every handler action, resume).

- One-shot equivalence: `traverse_start` with no pauses returns the same results (NodeIds, order, scores) as `search_code` with the same arguments.
- Pause and resume equivalence: a traversal paused at Y1, Y2 and Y3 and resumed with no ops returns the same results as the one-shot run.
- No guard held across calls: pause a traversal, then run `index_workspace` on the same workspace: it completes without waiting for the handle.
- Drift: pause, edit a fixture file and reindex, continue: surviving chunk NodeIds remain in the frontier, the edited chunk's old id is reported in `drift.dropped`, and a note is present.
- Validation is atomic: a patch whose third op is invalid leaves the handle unchanged (`seq` and frontier equal) and reports exactly that op's index and code.
- Stale seq: two steers with the same `expect_seq`: the second gets `stale_seq`.
- Filter cannot widen: `expand` from a node into a filtered-out role adds nothing (contract C2); `prune.where` with a broader glob is accepted but only removes.
- Endpoint binding: a handle from `/mcp/<profile>` used on `/mcp` or on REST gets the same "unknown handle" error as a random id.
- TTL and caps: with a test clock, an idle handle expires after the TTL; the 9th handle on one workspace is refused with a message naming the cap.
- Reweight replay: `reweight` then continue gives the fused scores that fusion produces from the recorded per-map ranks with the new weight (bit-exact).
- Data is not instructions: a fixture chunk whose text is a valid ops JSON never changes the plan unless the client sends it.
