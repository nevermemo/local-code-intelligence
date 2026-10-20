# Spec: RunRecord (trace log per traversal)

Status: **v0, pre-freeze; schema, writer (spike path) and offline harness built; the production writer and the handle-tool summary are not.** Written 2026-10-12 at
slice S0, updated at S3 (schema checked in, registry steps, redaction, R-8 to R-11). Schema id `lci.runrecord/0`; the first frozen version will be `/1`. Labels
(**Read**, **Inferred**, **built**) are defined in [README.md](README.md). The traverse engine writes its own event trace, a different format:
[../engine.md](../engine.md), "RunRecord and traces".

## 1. Purpose

One RunRecord per traversal: a one-shot search run through the map engine, a scripted traversal in the harness, or an agent-steered traversal through the handle
tools. It is the only input the harness needs to score a run, to rebuild a map card ([map-card.md](map-card.md)) and to replay steering for study S4 (snapshot-forked
counterfactuals). It is written as JSON Lines: a `run` header line, one `step` line per engine step, and one `end` line, so a run that dies mid-way still leaves a
readable prefix.

## 2. What exists today (Read)

| Item | Where | Reused how |
| --- | --- | --- |
| Candidate dump, one JSON line per search | `CandidateDumpLine`, `append_candidate_dump`, `src/app/search.rs`, under a process-wide lock, fail-open | Same writing discipline: one `write_all` per line, append-only, a write failure is a log warning, never a search error. |
| `CandidateTrace` rows | `src/app/search.rs` | The frontier rows of a step reuse its fields (ranks, `fusion_score`, `capped_rank`, `shortlisted`, `rerank_rank`, `document_bytes`, `est_tokens`, `snippet`) plus `node_id` and per-map contributions. |
| `Timings` (7 fixed fields) | `src/app/search.rs` | Become per-map `cost.elapsed_ms` plus engine stage timings. |
| `RerankOutcome` | `src/app/search.rs` | Reused as is for rerank steps. |
| `SearchIndexLifecycle { action, wait_ms }`, `IndexAction` | `src/app/search.rs`; `src/app/indexing.rs` | Recorded on every step that re-resolves the snapshot. |
| `EffectiveFilterReport` | reported in `SearchReport.filters` | Recorded once in the header. |
| Snapshot identity | `Snapshot { identity, dimension, indexed_at }`, `src/store.rs`; `Manifest.fingerprint`, `lci-core/src/manifest.rs` | `snapshot` object; the embedding identity is hashed (it contains the service URL). |
| Paired comparison | `src/evaluate/compare.rs` (pairs by workspace and query; seeded paired bootstrap) | The harness pairs RunRecords by `(item.set, item.id, arm)`. |

The dump writes `workspace_path`. A RunRecord does not: it carries the workspace id only, so records can be shared without machine paths.

## 3. Design (Inferred; built in `lci-maps/src/run_record.rs`)

### 3.1 Lines

| `type` | Count | Content |
| --- | --- | --- |
| `run` | 1, first | Who and what: schema, run id, start time, build, workspace id, snapshot, profile stamp, query, filters, plan (maps, weights, policy, budget), evaluation item reference. |
| `step` | 0..n | One engine step: its yield point, the map calls with status, cost and freshness, the frontier summary after the step, steering ops applied before it, notes and warnings raised in it. |
| `end` | 1, last | Final results, totals (engine cost, agent tokens), outcome (`completed`, `expired`, `cancelled`, `failed`), and, when the harness filled it, scores and leak-scan results. |

Yield points: `Y1` after fan-out, before fusion; `Y2` after fuse, filter, cap and cut (the shortlist, the rerank cost decision); `Y3` after rerank; `Y4` after each
expansion hop. A one-shot search records steps for all of them and never pauses. (The search read guard, `_read_guard` in `search_once`, is held for the whole
search; a paused traversal cannot hold it across MCP calls without blocking indexing, so each step re-acquires it and checks snapshot drift.)

The `run` line's required `profile` object stamps the configuration the run answered under (`run_record::profile_stamp`, from `Config`):

| Field | Value |
| --- | --- |
| `hash` | `Config::run_profile().hash`: SHA-256 of the canonical result-affecting configuration. |
| `keys` | `Config::run_profile().keys`: the dotted name of every profile leaf, sorted; names only, no values. |
| `embedding_identity_sha256` | SHA-256 of `Config::embedding_identity()`; never null. |
| `priors_hash`, `tie_break_hash` | The first 16 hex digits of SHA-256 of the text of the file at `[traverse] priors` / `tie_break` (the hash `HopPriors` and `TieBreak` keep); `null` when the key is unset. A configured file that cannot be read makes the stamp, and so the record, fail (fail-open: a warning, the run's answer unchanged). |

The two file hashes and the embedding identity are outside `hash` (`excluded_keys()` in `lci-core/src/config/profile.rs` leaves them out of the hash); the stamp carries them so a record names every
answer-changing input. `profile` (this object) is not `search_profile` (a string: the name of the `[search_profiles]` entry a request selected, absent on the
registry path).

### 3.2 Privacy rules (enforced by the writer, checked by a test)

- No absolute paths: workspace id only; NodeIds carry repository-relative paths.
- No service URLs: embedding and reranker identities are recorded as SHA-256 hex.
- The profile stamp's `keys` are configuration key names, never values; the stamp holds hashes only (no file path, no URL).
- Code text only as `snippet` (the existing bounded snippet: cut at 400 bytes plus a 3-byte ellipsis, `src/store.rs`), and only when `capture.snippets` is true.
- Redaction: before a line is written, the workspace path and the data directory (as given when absolute, and resolved; trailing separators dropped; each also with
  `/` separators, as a `file://` URI and with spaces as `%20`, and on Windows lower-cased and with the drive colon as `%3A`), the embedding and reranker URLs and
  their `host:port` (only when the URL names a port), and every absolute executable path in the configuration are replaced by a placeholder (`<workspace>`,
  `<data_dir>`, `<embedding_url>`, `<embedding_host>`, `<reranker_url>`, `<reranker_host>`, `<executable>`) in every string, the query text included. A match counts
  only where the secret stands on its own (no letter, digit, `_`, `-` or `.` before it; no letter, digit, `_` or `-` after it), so a workspace at `/src` never
  rewrites the repository path `lib/src/x.rs`. NodeId fields (`node`, `via`) are never rewritten: they are repository-relative by construction. A relative path is
  never a secret. Evidence `detail` is cut to 200 characters.
- Private gold sets: `item.set` is an opaque id; records of private items stay outside the repository, like every report (`test-results/`, `AGENTS.md`, Working
  method).

### 3.3 JSON Schema

The schema is [runrecord-0.schema.json](runrecord-0.schema.json) (frozen asset), compiled into `lci-maps` (`run_record::SCHEMA_JSON`); the writer refuses any line it
rejects. Beyond the S0 draft (which was too loose to catch a wrong line): `additionalProperties: false` on every object with a fixed shape (lines, plan, map entry,
map call, request, freshness, hit, evidence, frontier row, cost, snapshot, totals, capture, item, budget, policy, drift, evaluation), so a misspelt field is an
error; a `runId` definition (ULID pattern) used by `run_id` and `parent_run_id` on every line; `mapHit` and `evidence` definitions, with `edge` required on evidence;
`plan.maps[]` carries `contribution`, `limit`, `enabled` and `labels` (the labels the map's card lists, contract C5) and `plan` carries `top_k`; `end` carries
`warnings` and `notes` (every map's, in fusion order); `minimum` on counts, ranks, lines and times. The `run` line requires `profile` (3.1); a `nullableHex`
definition (`hex` or `null`) types its two file hashes. A `run` line without `profile` is refused by the reader and the writer alike (d399).

The checker is hand-written (`lci-maps/src/run_record/schema.rs`; no JSON Schema validator is in `Cargo.lock`; R-9, merge condition M7: the merge's `Cargo.lock` diff
adds `lci-maps` and no schema-validation crate). It implements `$ref`, `oneOf`, `type`, `required`, `properties`, `additionalProperties`, `items`, `enum`, `const`,
`pattern` (only the four patterns the schema uses), `minimum`, `maxLength` and `format: date-time`, and refuses to load a schema with any other keyword, pattern or
reference.

#### Steps of a registry run (built)

The registry path (`App::record_through_maps`, the recording twin of `fuse_through_maps`; feature `spike-maps`, `src/app/search/legacy_maps/record.rs`) writes, after
the `run` line:

| seq | `kind` | `yield` | Content |
| --- | --- | --- | --- |
| 1 | `fan_out` | `Y1` | The rank-list calls in fusion order, with hits, status, cost and freshness. |
| 2 | `fuse` | `none` | The prior calls (asked about the fused nodes) and the fused pool as frontier rows: node, lines, source role, score, contributions per map, `fused_rank`. |
| 3 | `cap_cut` | `Y2` | The pool in capped order (the production `cap_by_file`), with `capped_rank` and `shortlisted`. |

The `end` line's `results` are the first `min(top_k, rerank_candidate_count)` capped rows: what `search_code` returns with the reranker off. The registry path has no
reranker, so it writes no `rerank` step. A call's `cost.elapsed_ms` is the wall time the registry measured around it (`MapCall::engine_ms`), so a failure the
registry decided (panic, deadline) still has a time. The `fuse` step's `engine_cost.elapsed_ms` is the registry's time after the fan-out (priors and fusion), so the
two steps do not count the fan-out twice. `Registry::run` is one call, so the registry path writes its steps after the run returns: a process killed while the maps
run leaves only the `run` line. The writer itself is crash safe (whole lines, one write each; tested by killing a writer process after step 2); steps written as they
happen need the S5 engine.

### 3.4 Where records go

- Engine (design): off by default. A config switch or an environment variable (like `LCI_CANDIDATE_DUMP`) names a JSONL file or directory; one file per run
  (`<run_id>.jsonl`) under `test-results/` for the harness. Fail-open like the dump. Not built.
- Registry path (built): only `App::record_through_maps`, which takes the directory as an argument; no environment variable or config switch exists yet, and the
  production search never writes a record. One file per run, `<dir>/<run_id>.jsonl`, created new (never overwritten). A record that cannot be written is a log
  warning and an error beside the result; the run's answer is unchanged.
- Handle tools (design, not built): the `end` line is also summarised in the `traverse_*` tool response ([steering-ops.md](steering-ops.md)); the full record is
  never sent to the client unless asked, because the frontier rows are large (the dump is about 100 KB per query).

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| R-1 | JSON Lines with `run`/`step`/`end` lines. | One JSON document per run (simpler to validate, lost on a crash). | S3. |
| R-2 | The query text is stored in full (local RunRecords keep it; owner decision 2026-10-12). | Store only its hash plus shape tags for private items (private records stay outside the repository anyway). | First private-gold run. |
| R-3 | Frontier rows per step capped by `capture.frontier_rows` (default 50); `size` is always exact. | Full frontier every step (exact replay of every step, larger files). | S4 counterfactual study. |
| R-4 | Map hits stored per call, so leave-one-map-out can be replayed offline. | Store only fused contributions (smaller; no per-map replay). | S3. |
| R-5 | `agent_tokens` are self-reported by the caller of `traverse_steer`. | Measure from the agent client's own logs in the harness. | Map-to-map study. |
| R-6 | ULID run ids. | UUIDv7. Same properties; ULID chosen for shorter strings. | none |
| R-7 | Engine timings are per map and per step; the legacy 7-field `Timings` is a projection for `search_code`. | Extend `Timings` with a map-keyed object. | S5. |
| R-8 | `snapshot.fingerprint` is the manifest's fingerprint; when the manifest has none (written before fingerprints existed), SHA-256 of `indexed_at` and the embedding identity stands in. | Omit the snapshot, or make `fingerprint` optional. | S4 freeze. |
| R-9 | Schema validity is checked by a hand-written checker that refuses unknown keywords and patterns. | A JSON Schema crate as a dev-dependency (needs owner approval: a new dependency). Owner decision 2026-10-12: keep the hand-written checker. | S4 freeze. |
| R-10 | The registry path writes records only when called with a directory; no switch reaches the production search. | An environment variable like `LCI_CANDIDATE_DUMP`. | S5 (engine). |
| R-11 | Secrets are redacted by string replacement of known values. | A structural rule (no field may hold an absolute path), checked by the writer. | S4 freeze. |

## 5. How a test proves it

| Property | Test |
| --- | --- |
| Schema validity | Every RunRecord the offline test suites write validates against the schema (the hand-written checker, R-9). |
| One-shot equivalence | For a one-shot run, `end.results` equals `SearchReport.results` of the same search (NodeIds, order, `fusion_score` bits) and the steps' frontier rows match the `CandidateTrace` rows of a traced search. Shown for the default search profile with the reranker off. |
| Crash safety | Kill a run after step 2: the file holds a valid `run` line and two valid `step` lines; the harness reports `outcome` missing, not a parse error. Built by killing a real writer process (the test binary re-run as a child that writes three lines and waits), since a map panic is caught by the registry and never stops a run. |
| Privacy | A writer test asserts that no string field contains the workspace's absolute path, the data directory, or the configured service URLs, and that snippets are absent when `capture.snippets` is false. |
| Replay | Re-running fusion from the recorded `map_calls` hits and the plan's weights reproduces each `fuse` step's scores bit for bit. Built as `src/evaluate/run_record.rs` (5.1) beside `replay.rs`, which still replays candidate dumps over the three legacy channels. |
| Profile stamp | `run.profile.hash` equals `Config::run_profile().hash` of the recording app's config, `keys` equal and sorted, `embedding_identity_sha256` equals the snapshot's; both file hashes are `null` when unset and equal the `HopPriors` / `TieBreak` hashes of the files when set; an unreadable configured file is an error; a `run` line without `profile` is refused by writer and reader. |
| Fail-open writer | Point the writer at an unwritable path: the search succeeds, a log warning is emitted, nothing else changes. |

### 5.1 Offline harness (built)

`src/evaluate/run_record.rs` (`RunRecordFile`, re-exported from `evaluate`), no live models:

- Reading: every whole line must pass the schema; the first is `run`, then `step` lines with `seq` 1, 2, ..., at most one `end`, nothing after it, one `run_id`. An
  unterminated last line (a writer killed mid-write) is dropped and reported as `torn_tail`; a record without `end` reads as a prefix, and asking for its results is
  the error "outcome missing", never a parse error.
- Scoring: `end.results` become `evaluate`'s `ResultEvidence` (path from the NodeId; `source_role` required; `snippet` required when snippets were captured; the
  language is not recorded, and no metric reads it) and are scored with `query_metrics`, unreranked. Refused: a gold query for another text, one with
  `required_snippet` against a record without snippets, one with filters (registry runs are unfiltered), one whose `top_k` differs from `plan.top_k`, and a run whose
  `outcome` is not `completed`. Records are paired with gold queries by query text (`gold_query`), not yet by `item`.
- Replay: every `fuse` step is recomputed from the preceding `fan_out` step's used calls and the plan's weights and `rrf_k`, then the step's own prior calls, in
  `f32` and in recorded order, and compared with the frontier bit for bit (scores and every contribution), with the pool size, the ranks, the fused order (ties by
  path, lines and content hash), and, for a capped row list, that no left-out node belongs before the last row (by score, and on a tie by path only: a left-out
  node's lines are not recorded, so a left-out chunk of the same file with a lower start line is not caught). A frontier with nodes but no rows is refused (nothing
  would be checked). The pool of a `fuse` step is the hits of the last `fan_out` step before it; an engine that accumulates pools across steps (S5) needs a different
  replay.
- C5 offline: every used hit carries evidence whose labels are on its map's plan entry (`RunRecordFile::check_labels`).

Tested on the offline fixture with the public gold file `tests/fixtures/run_records/gold.toml` (three queries; the private gold sets are not used): for each query
the record's results equal `search_code`'s with the reranker off (path, lines, order, source role, `fusion_score` bits), its `fuse` rows equal the traced candidates
(rank, lines, score bits), its `cap_cut` rows their capped ranks and shortlist (with `max_chunks_per_file = 1` and `rerank_candidate_count = 3`, so the cap reorders
and the cut drops), each map's recorded ranks equal the traced channel ranks, the replay checks every row, and the record's metrics equal those computed from the
search's own results.

Not built: offline replay of the `cap_cut` step and a check that `end.results` are its first `min(top_k, rerank_candidate_count)` rows (both are checked only against
production in the integration test); `reranked` is always false (no `rerank` step yet; replay of a reranked run); blank lines and a byte-order mark are refused as
"not JSON", and a complete last line without its newline is treated as torn; `read_dir` fails the whole directory on one broken record; the `evaluation` block on
`end` (the harness returns metrics and writes nothing), `first_gold_step`, `unique_gold_by_map`, the leak scan, and pairing by `(item.set, item.id, arm)` for paired
comparison (a gold query whose text names a redacted secret cannot pair by text).
