# Spec: RunRecord (trace log per traversal)

Status: **v0, pre-freeze.** Written 2026-10-12T10:06Z (`date -u`) at slice S0. Docs only.
Updated at S3 (2026-10-12, branch `cloud/spike-s3b`): schema checked in, registry steps,
redaction, open decisions R-8 to R-11.
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
- Redaction (S3): before a line is written, the workspace path and the data directory (as
  given when absolute, and resolved; each also with `/` separators, as a `file://` URI and
  with spaces as `%20`), the embedding and reranker URLs and their `host:port`, and every
  absolute executable path in the configuration are replaced by a placeholder
  (`<workspace>`, `<data_dir>`, `<embedding_url>`, `<embedding_host>`, `<reranker_url>`,
  `<reranker_host>`, `<executable>`) in every string, the query text included. A match
  counts only where the secret stands on its own (no letter, digit, `_`, `-` or `.` before
  it; no letter, digit, `_` or `-` after it), so a workspace at `/src` never rewrites the
  repository path `lib/src/x.rs` inside a NodeId. A relative path is never a secret.
  Evidence `detail` is cut to 200 characters.
- Private gold sets: `item.set` is an opaque id; records of private items stay outside the
  repository, like every report (`test-results/`, `AGENTS.md`, Working method).

### 3.3 JSON Schema

The schema is checked in as [runrecord-0.schema.json](runrecord-0.schema.json) (S3) and
compiled into `lci-maps` (`run_record::SCHEMA_JSON`); the writer refuses any line it rejects.
It replaces the inline draft that stood here at S0. Changes against that draft, all made at S3
because the implementation showed the draft too loose to catch a wrong line (Inferred):

- `additionalProperties: false` on every object with a fixed shape (lines, plan, map entry,
  map call, request, freshness, hit, evidence, frontier row, cost, snapshot, totals,
  capture, item, budget, policy, drift, evaluation), so a misspelt field is an error.
- `runId` definition (ULID pattern) used by `run_id` and `parent_run_id` on every line.
- `mapHit` and `evidence` definitions; `edge` is required on evidence.
- `plan.maps[]` gains `contribution`, `limit`, `enabled` and `labels` (the labels the map's
  card lists, contract C5); `plan` gains `top_k`.
- `end` gains `warnings` and `notes` (every map's, in fusion order).
- `minimum` on counts, ranks, lines and times.

The checker is hand-written (`lci-maps/src/run_record/schema.rs`; no JSON Schema validator
is in `Cargo.lock`). It implements `$ref`, `oneOf`, `type`, `required`, `properties`,
`additionalProperties`, `items`, `enum`, `const`, `pattern` (only the four patterns the
schema uses), `minimum`, `maxLength` and `format: date-time`, and refuses to load a schema
with any other keyword, pattern or reference.

#### Steps of a registry run (S3, Read in the code)

The registry path (`App::record_through_maps`, the recording twin of `fuse_through_maps`)
writes, after the `run` line:

| seq | `kind` | `yield` | Content |
| --- | --- | --- | --- |
| 1 | `fan_out` | `Y1` | The rank-list calls in fusion order, with hits, status, cost and freshness. |
| 2 | `fuse` | `none` | The prior calls (asked about the fused nodes) and the fused pool as frontier rows: node, lines, source role, score, contributions per map, `fused_rank`. |
| 3 | `cap_cut` | `Y2` | The pool in capped order (the production `cap_by_file`), with `capped_rank` and `shortlisted`. |

The `end` line's `results` are the first `min(top_k, rerank_candidate_count)` capped rows:
what `search_code` returns with the reranker off. The registry path has no reranker, so it
writes no `rerank` step. A call's `cost.elapsed_ms` is the wall time the registry measured
around it (`MapCall::engine_ms`), so a failure the registry decided (panic, deadline) still
has a time.
The `fuse` step's `engine_cost.elapsed_ms` is the registry's time after the fan-out
(priors and fusion), so the two steps do not count the fan-out twice.

`Registry::run` is one call, so the registry path writes its steps after the run returns:
a process killed while the maps run leaves only the `run` line. The writer itself is crash
safe (whole lines, one write each; tested by killing a writer process after step 2); steps
written as they happen need the S5 engine.

### 3.4 Where records go

- Engine: off by default. A config switch or an environment variable (like
  `LCI_CANDIDATE_DUMP`, `search.rs:97`) names a JSONL file or directory; one file per run
  (`<run_id>.jsonl`) under `test-results/` for the harness. Fail-open like the dump.
- Registry path (S3): only `App::record_through_maps`, which takes the directory as an
  argument; no environment variable or config switch exists yet, and the production search
  never writes a record. One file per run, `<dir>/<run_id>.jsonl`, created new (never
  overwritten). A record that cannot be written is a log warning and an error beside the
  result; the run's answer is unchanged.
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
| R-8 | `snapshot.fingerprint` is the manifest's fingerprint; when the manifest has none (written before fingerprints existed), SHA-256 of `indexed_at` and the embedding identity stands in. | Omit the snapshot, or make `fingerprint` optional. | S4 freeze. |
| R-9 | Schema validity is checked by a hand-written checker that refuses unknown keywords and patterns. | A JSON Schema crate as a dev-dependency (needs owner approval: a new dependency). | S4 freeze. |
| R-10 | The registry path writes records only when called with a directory; no switch reaches the production search. | An environment variable like `LCI_CANDIDATE_DUMP`. | S5 (engine). |
| R-11 | Secrets are redacted by string replacement of known values. | A structural rule (no field may hold an absolute path), checked by the writer. | S4 freeze. |

## 5. How a test proves it

| Property | Test |
| --- | --- |
| Schema validity | Every RunRecord the offline test suites write validates against the schema above (a JSON Schema validator as a dev-dependency, or a hand-written checker in the harness if no validator is already in the tree). |
| One-shot equivalence | For a one-shot run, `end.results` equals `SearchReport.results` of the same search (NodeIds, order, `fusion_score` bits) and the steps' frontier rows match the `CandidateTrace` rows of a traced search. |
| Crash safety | Kill a run after step 2: the file holds a valid `run` line and two valid `step` lines; the harness reports `outcome` missing, not a parse error. S3 kills a real writer process (the test binary re-run as a child that writes three lines and waits), since a map panic is caught by the registry and never stops a run. |
| Privacy | A writer test asserts that no string field contains the workspace's absolute path, the data directory, or the configured service URLs, and that snippets are absent when `capture.snippets` is false. |
| Replay | Re-running fusion from the recorded `map_calls` hits and the plan's weights reproduces each `fuse` step's scores bit for bit (extends `src/evaluate/replay.rs` beyond three channels). |
| Fail-open writer | Point the writer at an unwritable path: the search succeeds, a log warning is emitted, nothing else changes. |
