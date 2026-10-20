Status: PLAN

# Roadmap

This file holds plans only: what is open, what is next and what is deferred. Results, history and the reasons
behind decisions live in [decisions.md](../decisions.md), where `roadmap@bbd3292 <ID>` names the text of an item in the
last version of the old roadmap that carried full result paragraphs (`git show bbd3292:docs/development/roadmap.md`).
Ids are never renumbered or reused.

## How to use this file

1. Take the next slice of "Cycle 1" in table order (named in "Now and next"), then "Earlier items outside cycle 1", then
   the lowest tier in "Open items by tier" whose dependencies are done.
2. One item is one slice and one commit, plus the regular `cargo xtask test --suite Full` gate. Split an item that grows.
3. Load the matching project skill first (`.agents/skills/lci-*`; the list is in `AGENTS.md`).
4. A ranking, fusion, reranker, document-format, instruction or candidate-count change needs a before/after evaluation
   (`evaluate` on `evaluations/core.toml` and `evaluations/mined/self.toml`, frozen binaries, `compare` for the CI).
   Keep the JSON under `test-results/` (never committed) and put the headline numbers in the commit message.
5. Update this file in the commit that does the work: remove the item, add its result to decisions.md as one line, set
   the status of a successor whose only blocker it was, and append newly found work under the next free id of its letter.
6. Downloading model weights needs the owner's explicit approval (state model, source, size). Docker, a web UI, another
   database, another service or contacting the owner's production port are out of bounds (`AGENTS.md` "Hard rules") and
   belong in the parking lot.

Status legend: `ready` (dependencies met) | `in-progress` | `partly done: <what remains>` | `blocked(reason)` |
`needs-approval(what)` | `absorbed(K..)` (taken into a cycle 1 slice) | `research` (produces a decision, not a feature) | `paused(direction)` (parked by the 2026-10-17
direction, see below). The table row is the only home of a status.

Size: XS = a few lines, S = under half a day, M = about a day, L = more than a day (split before picking).

Naming: a slice is the item id plus a lowercase letter (A6a, N3b), never a new id. The tier names P0, P1, P2 and P3 only
name priority tiers and are never slice names. A document's internal numbering (amendments, audit actions) is scoped to
that document and is not a roadmap id.

## Direction

- Main focus (owner, 2026-10-17, d313): multimap traversal and its iterations. The embedder and reranker premise gets no
  further benches or iterations; its merit and lessons stay in the log. LCI is an extension of grep: grep and the other
  agent tools are steps inside multi-step queries (d337).
- Core loop (d330): a Wikirace on a code graph, with a System 1 racer and compiler-grade links.
- Cycles (owner, 2026-10-19/20, d396): work runs in cycles. Cycle 1 puts every layer of the vision in place at least once
  behind flags in one typed profile; later cycles iterate on what high-volume real-agent A/B runs show. Features get
  ceiling tests and mechanism checks, not per-feature kill rules; the final selection is a later preregistered
  screening or factorial design. Real-agent A/B runs wait for the owner.
- Sources (d396): language servers are the source of truth for meaning-level facts and are on by default where a server
  is available; no batch index file is a primary source (SCIP is a cache or test ground truth); proven libraries are
  reused where they fill a slot (custom code only for a measured gap).

## Now and next

In flight: K2a-1 (freshness harness), K20a (`scip.mode`), N20 c and d; K7b after K2a-1. Rewrite this line at every push.

## Cycle 1

Plan reviewed by the planning workflow, the coordinator and a cross-family reviewer (d397). Milestones run in order;
inside a milestone the order below is the default. Each row is one slice and one commit unless marked. Check = what the
slice's tests must show (ceiling test: works given ideal input; mechanism: the code path does what it claims). Every
new behaviour is a `[run_profile]` key whose default keeps today's behaviour unless the row says otherwise.

| Slice | Item | Check | Size | Absorbs |
| --- | --- | --- | --- | --- |
| **M1** | **Profile, freshness harness, no-service path** | | | |
| K2a | Partly done: the per-map freshness label `[run_profile.traverse] map_freshness` (d410); the harness remains. Freshness harness: edit a fixture file, query every map at once, assert the answer reflects the edit or carries an accurate stale label and lag | Ceiling per map | M | Deferred F2 |
| K2b | Verify on read (`freshness.verify_on_read`): cited files re-checked by content hash; which maps may return stale evidence is listed; the index-freshness contract is unchanged | K2a | S-M | Deferred F1 |
| **M2** | **Language servers as the source of truth** | | | |
| K20a | Traverse no longer loads the SCIP index unconditionally (`scip.mode = off, cache, authoritative`; default stays authoritative until K20b) | Traverse runs with no index file | S-M | - |
| K7b | Shared LSP sessions across traverse calls (server lifetime), a symbol-outline cache keyed by content hash (no empty results), readiness signals per server | Fake LSP tests | M | N8 (readiness part) |
| K8a | LSP sync independent of index watching: while any session is live, file changes reach every session (`didChangeWatchedFiles`, `didChange`); a full walk only at start and after a watcher overflow | K2a with K7a's strict fake | M | W2 |
| K7c | Document lifecycle: open once, `didChange` on edits, `didClose` on eviction, no fixed sleeps | Fake LSP event log | M | N2 |
| K10a | Place table without SCIP: `SymbolPlaces` over the LSP outline and `symdefs` | `lsp.live` runs with `scip.mode = off` | M | - |
| K9a | `lsp.live` implementation edges (`Implements`) | Ceiling: edges equal the server's answers | S | N6 (map side), G1 |
| K9b | `lsp.live` call-hierarchy edges in and out (`CallsOrRefs`, witness `Exact`; no new `EdgeKind`) | Same | M | - |
| K11d | LSP cost report: cold and warm ms per hop, RAM, budget hits, fail-open behaviour, pops per 5 s, before any default flips | Report under `test-results/` | S | I-8 |
| K11b | `lsp.live` on by default when a server is available (`lsp.default = on`), fails open, spent from the external-call budget; the search channel's default is verified | Tests that assumed it absent are updated | S | - |
| K20b | `scip.mode` default `cache`: SCIP edges are subordinate and labelled; LSP wins a disagreement | K2a | S | - |
| **M3** | **Walk, cards, seeds, router, cost** | | | |
| K10b | New `SymbolKind` variants (Field, Variable, Interface, Class) from LSP outlines; adapter version bump | Unit tests | S | - |
| K11c | Prime before the first timed query; per-query budget-hit flag | Bench | S | E5 |
| K3a | Cost metrics in every report: answer tokens, external calls, LSP ms, pops, per stratum; `compare` excludes budget-hit queries | Bench | S | E5, item 8 proxies |
| K13 | Cards say whether an answer is complete and how fresh it is; partial answers are labelled, never withheld | Card schema test | S | - |
| K27 | Paths on cards: every hop with its edge kind and witness | Card schema test | S | - |
| K28 | Handle spec (format, lifetime, snapshot binding) and a round-trip test | Test | S | - |
| K29 | `likely_complete`, the score bound and tau get written meanings | Docs and unit tests | S | - |
| K21a | BM25 entity seeds over symbol names, qualified paths and doc comments, in process (reusing the lexical scoring); `seeds.bm25` off | Ceiling: gold symbol in top k on the trait and where-to gold | M | L1, L4, I-1 |
| K22a | Exact-literal seed source (new; `exact_seeds` reuses exact cards and is not this), including the seedless-trait cases | Ceiling | S | item 4 rest |
| K24a | Fusion variant: capped log-odds next to the current fusion (`walk.fusion`) | Unit tests | S | - |
| K24b | Rank pricing normalised by sibling count (`walk.pricing`) | Unit tests | S | - |
| K25 | Depth registered as a profile factor (`max_depth` 1, 2, 3, 6) | Profile test | XS | - |
| K30a | Router (`router.mode = off, intent, features`): a caller-supplied intent or observable query features pick walk, grep or LSP; gold labels never route | Ceiling: route by the gold's question type | S-M | F3, N100 (detector stays killed) |
| K33a | Trajectory rows per pop (state, candidates, choice) linked to the outcome (gold hit) in evaluation runs | Schema test | S | - |
| **M4** | **Syntax tier, more maps, gold, baselines** | | | |
| K15a | tree-sitter 0.26 and its grammar crates; every chunking test re-run; adapter version bump; default-build regression check | Existing tests | M | - |
| K14a | Offline proof for Codanna 0.16.0 as an optional crates.io dependency: public API, dependency graph, grammar compatibility; TypeScript edge quality against an offline ground truth | Report | S | - |
| K14b | Cargo feature `syntax-codanna`: parse one file through Codanna into LCI nodes | Ceiling on fixtures | M | - |
| K14c | In-memory resolution glue; edges with confidence by the rule that picked them | Precision and recall against LSP on fixtures | M | - |
| K14d | Name-based invalidation: on a change, re-resolve the file's outgoing edges and edges whose target names it added or removed | K2a | S | - |
| K16 | Codanna parser tests as fixtures; its `tsconfig`, `go.mod` and `pyproject` readers for server root discovery | Tests | S | N4 (part) |
| K17 | `oxc_resolver` for TS and JS imports, only if K14a shows Codanna's TS resolution is weak | Imports against typescript-go | S | - |
| K26 | Syntax proposes, LSP confirms the top-k frontier (`walk.confirm`) | Cost and quality per call | M | G1 |
| K9c | Type-hierarchy edges | Ceiling | S | - |
| K12a | Server roster and per-language readiness; root discovery per server | Live acceptance per server | S | N4 |
| K12b | typescript-go adapter | Live acceptance | M | - |
| K12c | marksman adapter for prose | Live acceptance | S | N95 |
| K18a | Repo map: PageRank over the syntax and outline graph as a seed prior (`seeds.repo_map` off) | Ceiling | M | G2 |
| K19 | `grep.structural` map over ast-grep-core (pinned, after K15a) | Ceiling on structural queries | M | - |
| K31 | Opengrep data-flow map, opt-in | Ceiling on a flow set | M | - |
| K32 | Verify which planned maps exist (`tests.tested_by`, `docs.mentions`, pickaxe as a walk map); build the missing ones | Ceiling per map | XS each | - |
| K37a | One request queue per server | Load test | S | N20 (a, b, e) |
| K37b | Concurrent walks | Load test | S | - |
| K37c | `probe.compile` runs `cargo check`, which can execute build scripts: a bounded sandbox, or opt-in only with a warning | Test | S | - |
| K36 | Language scope table: server, syntax tier and gold per language | Docs | XS | - |
| K4a | External held-out gold adapters (SWE-PolyBench Verified, LongCodeArena); dataset licences recorded; never used for tuning | Leak check | S | - |
| K4b | External gold download and conversion | Counts | S | - |
| K5a | Baseline arms rg-only and LSP-over-MCP wired into `maps baseline` (no A/B runs) | Smoke query | S | item 6 arm |
| K5b | Baseline arms Codanna and Serena wired (no A/B runs) | Smoke query | S | - |
| K6a | Gold strata schema (core, grep-easy guard, agent-trap guard) and a miner over synthetic transcripts | Tests | S | - |
| K6b | Mining question shapes from real Claude Code and Codex transcripts, locally; output gitignored | Counts | S | - |

M0 filler, any time a build slot is free: N20 (c, d); K0a follow-ups: `VectorChunks::vector_of` (lci-maps) counts an external call it never makes on a vectorless snapshot, and `with_legacy_run` builds its `SemanticMap` from the unadjusted channels (d404); K2a-2 follow-up: with `map_freshness` on, every traverse call reads and hashes the whole tree; keep `(mtime, len)` per file and re-hash only changed files (needs a public lci-core source lister) (d410).

Deferred to cycle 2: rust-analyzer in process (`ra_ap_*`), ty or Pyrefly (in process or as servers), ts-rust as a TS
server arm, rung 2 model training, a gix blame map, a tantivy engine for K21 (only after a measured gap).

### Cycle 1 time estimate

Agent wall time per slice, for the whole loop (plan, implement, Full gate, review, PR, CI): XS 0.5 h, S 1.5 h, S-M
2.5 h, M 3.5 h. Measured on 2026-10-20 for S slices only (K1a, K1b, K1c in two parts: 1 to 1.5 h each, one at a
time; the Full gate takes about 25 min and CI about 20 min); K0a part 1 (S-M) took about 3.5 h with two review
rounds. M is not measured yet. Update this table at each milestone end with the measured hours.

| Milestone | Slices left (by size) | Sequential hours | Measured | Note |
| --- | --- | --- | --- | --- |
| M1 | K0a part 2, K7a, K2a (M), K2b (S-M), K23a, K35a (XS), K34a | ~12 | - | five lanes in parallel since 2026-10-20 |
| M2 | 6 M, 1 S-M, 4 S | ~30 | - | K7b, K8a and K7c build on each other |
| M3 | 1 M, 1 S-M, 11 S, 1 XS | ~23 | - | mostly independent slices |
| M4 | 8 M, 16 S, 4 XS | ~54 | - | most uncertain: Codanna on tree-sitter 0.26, typescript-go, marksman, live server checks, external gold |
| Total | 55 slices | ~119 | - | |

Parallel lanes (separate worktrees, up to three cargo builds gated on free RAM) are expected to give a 2 to 2.5x
speed-up, limited by dependencies, by merges into `main` one at a time and by the build slots: about 48 to 60 h of
wall time, so cycle 1 ends around 2026-10-22 to 2026-10-23. Risks that move it: M slices needing two review rounds
(about +30% on M2 and M4), and M4's external tools on Windows.

## Earlier items outside cycle 1

| # | Item | Status | Notes |
| --- | --- | --- | --- |
| 1 | Doc-type prose adapters: `.md`, `.rst`, `.adoc`, `.txt` | partly done: `.txt`, `.rst`, `.adoc` (each its own slice behind the Markdown tests); DocSection nodes and a `doc.sections` map after (shaping count 54.9% > the 30% bar) | Real adapters, never wrapped as code-like files (d317, d325). Markdown landed as heading sections (d375) and is opt-in via `[index] prose` after its docs-lane evaluation (d380). |
| 2 | Structured formats `.json`, `.yaml`, `.toml` | unfinished (d393): the lockfile skip failed its rule and was dropped | Grep fallback stays as it is (d325). Skipping files does not shorten a capped listing; a follow-up would test listing relevance, on a set where the simulated agent can succeed. Real adapters only if a measured gap appears. |
| 5 | Error-to-commit set with SZZ gold | parked (d377) | "Which code changes in git history are responsible for this error" (d314). Gold: blame the fix commit's lines at its parent, then verify; first a premise check of usable fix commits. Baseline: `git bisect` where a reproducing test exists (d324). The premise check failed its kill rule (8 usable items, 40 needed); re-check is N96. |
| 8 | Tier-2 calibration | partly done: the judged subset of the simulated agent ran (d390, inconclusive at n 48, proxies uncalibrated) | The one allowed follow-up (a new prereg at n 96) is dropped (d395): a zero-cost projection gives P(inconclusive again) 0.84 or more. Confidence calibration of an LCI score (the other half of d327) waits for a component that emits a probability. Tier 3, real agent A/B runs, stays rare and validates the proxies (pv2 rho .699, d261). |
| 10 | N14 re-evaluate | paused until search work resumes | Tag `keep/n14-lexical-suffix`, commit e0f7c10 (d115, d302). The before/after run needs the 4B pass. |

## Open items by tier

Tier order: P0 correctness and measurement, P1 ranking, storage, freshness and LSP, P2 agent ergonomics and protocol,
P3 umbrella research. Rows are open items only. Item text names symbols, never line numbers.

### P0 - verified defects, test quality, evaluation hygiene

| ID | Item | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| E5 | LSP-stable evaluations: prime before the first timed query; per-query `lsp_budget_hit`; `compare` excludes budget-hit queries. After N7 the budget-hit windows cover 14 to 22 of 35 mined queries (d054) (roadmap@bbd3292 E5) | absorbed(K11c, K3a) | S | N7 done |
| E6 | Identifier-query stratum: `cargo xtask mine-symbols` samples declaration names (seeded, names of 5+ characters, defined in one file, not test-only) with `expected_paths` = the defining file, tag `identifier`, committed apart from `self.toml`; unit tests on synthetic input; one fusion-only and one reranked baseline recorded (roadmap@bbd3292 E6) | ready | S | E2 done |
| N8 | The LSP channel goes dark for 15 to 25 s windows under back-to-back searches (hypothesis: rust-analyzer swaps its crate graph after build scripts and proc macros load, invalidating the primed symbol index); diagnose and fix (roadmap@bbd3292 N8) | absorbed(K7b (readiness); the live diagnosis stays open) | S-M, live | N7 done |
| N16 rest | A workspace search on a legacy-only table waits on the store-wide legacy guard; the revision pin and the re-read after a commit during the freshness check are not pinned by a failing test (roadmap@bbd3292 N16) | ready | S | - |
| N18 | Low batch (item (b), the Java readiness probe, was fixed as N77, d190): (a) `manifest::save` removes the destination before `rename`; (c) `serve --port 0` advertises port 0; (d) three navigation tests assert rust-analyzer's flag; (e) the embedding model listing is probed at the origin; (f) no per-file size cap (roadmap@bbd3292 N18) | ready | XS each | - |
| N20 | Cancellation and lock scope: (a) the MCP index and search handlers ignore the request's cancellation token; (b) lock scope; (c) `unwatch` race; (d) blocking lock inside `spawn_blocking`; (e) Ctrl+C graceful shutdown has no deadline (roadmap@bbd3292 N20) | ready | S-M | N16 done |
| N21 | Two open test-quality items (a third, a fixed 500 ms sleep in the data-dir lock test, was fixed as N83, d193): (a) `evaluation_reports_required_failures_optional_skips_and_json_metrics` checks only that `hit_at_1_rate`, `mrr` and `role_distribution` exist, not their values (check whether another test pins them); (b) its last assert checks an `EvaluationFile` the test built itself (tautological): exercise `load_definition` instead (roadmap@bbd3292 N21) | ready | XS | - |
| N25 | Six weak assertions in `tests/cases/indexing.rs`: (a) `a_hung_reranker_is_tried_once_and_keeps_earlier_warnings` sets a 1 s reranker timeout but bounds the elapsed time at 10 s; (b) the unreachable-reranker test asserts only `reranking_ms` < 5000 (count accepted connections instead); (c) the 200 ms budget tests use 150 to 1200 ms bounds; (d) a stop-word query is not in the fixture; (e) the traced-search test lacks a rank >= 1 assert; (f) the `mixed_languages` deletion test checks top-8 over equal vectors without a chunk count (roadmap@bbd3292 N25) | ready | XS | - |
| N26 c | `search_symbols` has no result cap; record the size of a broad name's answer first (roadmap@bbd3292 N26) | ready | XS | - |
| N32 | Rewrite the model-facing strings within budgets: `search_code` 350 characters (436 today), `search_symbols` 280 (328), instructions 450 (732); state facts only (d278): what comes back, the first-call index cost and the navigation preconditions; no advice on when to prefer Grep, which goes to an on-demand skill measured as its own arm. Its former kill-rule thresholds move to the cycle 2 screening design (d396). Text is `cloud-ok`, the A/B arm is `local`. See [agent-payload-notes.md](agent-payload-notes.md) (roadmap@bbd3292 N32) | ready after N26 | S | N26 |
| N33 | Schema documentation for the four search and navigation tools: describe `query`; list the valid `languages` and `source_roles` values (roles: source, test, benchmark, example, generated, documentation, configuration); state glob syntax, that an empty `include_paths` matches nothing and that empty language or role lists are errors; document `include_declaration`; describe `workspace_path` on every tool (today only on `WorkspaceArgs`, 73 characters); stop hard-coding "(8)" in the `top_k` text; drop what N32 makes redundant (the language-server list repeated 4 times, about 400 characters). Acceptance: a test that every argument of the four tools has a description; `tools/list` no larger than today (roadmap@bbd3292 N33) | ready | S | N32 |
| N34 | `default_top_k` 5 versus 8 as a measured arm: 61 appearances, 57 at rank 5 or better, ranks 6 to 8 carry 18.7% of returned lines, 20 of 94 answered files were at rank 6 or later (U1 calls); lowest priority (roadmap@bbd3292 N34) | ready, local | S | N28 done |
| N42 | Incremental-index peak memory: `Store::cached_vectors` plus a vector clone scale with the whole index (estimate 0.9 to 1.5 GB at 150,000 to 250,000 chunks of 768 floats, not measured); measure first; fix direction: load only vectors whose chunk hash is still present and move instead of clone; acceptance: state the bound and a test fails above it; `cloud-ok` (roadmap@bbd3292 N42) | ready, low | S | - |
| N48 | Windows verbatim `\\?\` prefix at paths of 260 characters or more breaks prefix comparisons (roadmap@bbd3292 N48) | ready, low | XS-S | - |
| N57 | The xtask and live-acceptance workflow cleanup deletes every `lci-*` temp directory, not only its own (roadmap@bbd3292 N57) | ready | XS | - |
| N81 | Windows test helper `process_is_running` decides from output alone; ignores exit status and stderr (roadmap@bbd3292 N81) | ready, local, low | XS | - |
| N82 | Windows `finish_holder_test` skips an ordinal whose pid file is missing, so a missing sleeper goes unchecked (roadmap@bbd3292 N82) | ready, local, low | XS | - |
| N87 | The live-acceptance workflow does not use `--require-all`, so a missing tool reads green (roadmap@bbd3292 N87) | ready | S | N56 done |
| N88 | Two load-sensitive budget tests in `tests/cases/indexing.rs`: find out whether the product or the test is at fault (roadmap@bbd3292 N88) | ready | S | - |
| N94 | Extension comparisons are case-sensitive in 11 places (clippy pedantic census): a `.RS` or `.Py` file is skipped; decide and test (d371) | ready | S | - |
| N95 | A Markdown language server (marksman, MIT) for prose navigation: links, headings, references; the language bar's LSP item is waived for prose until then (d375) | absorbed(K12c) | S | - |
| N96 | Re-check the error-to-commit premise (item 5) with full clones and the error text of linked GitHub issues, under a new pre-registered rule (d377) | ready | M | - |
| N99 | System-1 rescoring rung: a decision model orders the reranker's 24-candidate shortlist, the seam where N98 (killed) showed the reranker's value (d382). Gate K0 passed on a hosted System One prototype arm (d383); slice 2 (the opt-in rescorer rung, with status-only HTTP errors, transmitted trace sizes and a joint-batch replay guard) merged as PR 88 and the default is unchanged; K1 passed (slice 3, d385); slice 4 failed bar S (d387): the local 4B decider loses on flow and mined search and needs about 19 GB of VRAM, so the reranker stays the default and the rung stays opt-in. The hosted model is never shipped | done (local rung killed, d387) | M | - |
| N100 | Where-to routing: an un-steered `traverse` call on a where-to question returns 16 answers (8 seeds and 8 walk) where the `where-to` preset returns the 8 first-stage answers, with set_f1 +0.015 [+0.005, +0.026] and 42% fewer answer tokens; cut to 8 the two are identical, so the gain is the shorter list (d389). Whether to route such questions to the preset (question-type detection, or a fact in the tool description under the facts-only rule) needs its own pre-registered test. Detection (b) was killed at B0 (R 0.339 against the 0.70 bar, d391) | partly done: detection killed (d391); the description fact (a) is parked until a tier-3 run is approved | S | - |
| N14 | Lexical matches credited to the first chunk of a path-suffix match: tag only (see Earlier items outside cycle 1, row 10) (roadmap@bbd3292 N14) | paused | S | - |

Items marked `cloud-ok` in older notes may run in a cloud agent; items marked `local` need the owner's machine.

### P1 - ranking quality, storage, freshness, LSP correctness

Retrieval items tied to the embedder and reranker premise are `paused(direction)`; they stay listed because their
acceptance data is in the log.

| ID | Item | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| L0 | Engine-independent lexical fixes: scanner-aligned `rg` flags, timeout and stdout cap, newest bundled `rg`, interval index for LSP mapping, timing split (d074) (roadmap@bbd3292 L0) | paused(direction) | XS-S | - |
| L1 | Lexical engine spike: arms (a) `rg` subprocess, (b) in-process `grep-searcher` + `ignore`, (c) LanceDB FTS, (d) Tantivy. The adoption rule (d075) is retired by d396; K21a builds the in-process arm; a missing index degrades to `rg` with a warning (roadmap@bbd3292 L1) | absorbed(K21a) | M | - |
| L3 | Anchor extraction for issue-like queries; optional agent-supplied `lexical_terms` (roadmap@bbd3292 L3) | paused(direction) | S | - |
| L4 | Ranked BM25 channel as a fourth RRF list (engine per L1) (roadmap@bbd3292 L4) | absorbed(K21a) | L, split | L1 |
| C1 | Per-chunk symbol metadata (name, kind, enclosing, signature) and Rust `impl` header carry (roadmap@bbd3292 C1) | ready | M | - |
| C2 | Contextual headers in embedding, lexical and reranker documents (one re-embed); gate: no keyword-stratum loss, core hit@1 at least 0.818 (d076) (roadmap@bbd3292 C2) | blocked(C1), paused | M | C1 |
| F2d | Definition boost in fusion (LSP symbol kinds now, C1 `kind` later) (roadmap@bbd3292 F2d) | proposed, paused | S | C1 |
| F3 | Query-type routing (identifier, prose, issue-like) (roadmap@bbd3292 F3) | absorbed(K30a) | M | L3, R2, E6 |
| N1 | Per-provider symbol-query syntax (roadmap@bbd3292 N1) | ready | S | - |
| S1 | Per-file incremental table writes (delete + add or `merge_insert`), lancedb bump (roadmap@bbd3292 S1) | ready | M | - |
| S2 | `optimize()` and version cleanup, `gc` for orphaned workspaces and manifests (roadmap@bbd3292 S2) | ready | M | S1 preferred |
| S4 | Stop loading every chunk's code per search; interval maps for channel mapping (roadmap@bbd3292 S4) | ready | M | - |
| W1 | mtime+size stat cache before hashing (manifest additive fields) (roadmap@bbd3292 W1) | ready | M | - |
| W2 | `notify`-based watcher with polling fallback and persisted watch roots (roadmap@bbd3292 W2) | absorbed(K8a) | M | W1 preferred |
| N2 | LSP document lifecycle: open once, `didChange` on edits, `didClose`, drop fixed sleeps (roadmap@bbd3292 N2) | absorbed(K7c) | M | - |

Short accounts. C1: carries the `impl` header into method chunks and gives later features a `kind`; accept with unit
tests per language and a before/after run on both sets. S1/S2/S4: a commit today overwrites the whole workspace table
and every search loads every chunk; accept with an equal-results test and a timing run on a large workspace. W1/W2: freshness
cost today is a read and SHA-256 of every supported file at most every 10 s; accept with a manifest compatibility test.
N2: accept with a live LSP test that a second symbol request reuses the open document.

### P2 - agent ergonomics, navigation by name, protocol upgrade

| ID | Item | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| N4 | Project-root discovery and priming for TS/Python/Go servers (roadmap@bbd3292 N4) | absorbed(K12a, K16) | M | N2 |
| N5 | Symbol-name addressing for `find_definition` and `find_references` (roadmap@bbd3292 N5) | ready | M | N1 |
| N6 | Outline, hover, implementations, call hierarchy tools (two slices) (roadmap@bbd3292 N6) | absorbed(K9a, K9b, K10a (map side); the tool side stays open) | M + M | C1, N2 |
| A1 | Compact results, `read_chunk`, `token_estimate`, `max_tokens` (roadmap@bbd3292 A1) | ready | M | - |
| A2 | MCP `outputSchema`, tool annotations, deterministic tool order, instructions (roadmap@bbd3292 A2) | ready (verify rmcp support) | S | A1 preferred |
| A3 | Upgrade rmcp 3.2.0 to 3.5.1 (2026-07-28 stateless spec, back-compatible) (roadmap@bbd3292 A3) | ready | M | - |
| A4 | `index_workspace` as an MCP Task with progress (roadmap@bbd3292 A4) | blocked(A3) | M | A3 |
| C3 | Split declarations above about 8 KB with header carry (roadmap@bbd3292 C3) | blocked(C1) | M | C1 |

The tool count is 16 (`src/server.rs`); N6 and A1 to A4 change the agent-facing surface, so each keeps an alias or
records the break (d323).

### P3 - structure, scope, packaging, research (split before promotion)

Umbrella items: split each into S/M slices with acceptance criteria, and show a measured benefit, before picking. Evidence bullets and the enlarged-evaluation wording of the old tier: roadmap@bbd3292 P3.

| ID | Item | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| G1 | Bounded structural expansion of top hits via LSP references or call hierarchy, labelled with a reason; evidence: SpIDER arXiv 2512.16956 (+27% relative Recall@20), CodexGraph token cost arXiv 2408.03910 (roadmap@bbd3292 G1) | absorbed(K9a, K26) | L | N6b (a slice of N6), E2 |
| G2 | `repo_map(query, max_tokens)` from tree-sitter tags and personalized PageRank (Aider's repo map; best at an 8K-token budget, arXiv 2607.24882) (roadmap@bbd3292 G2) | absorbed(K18a) | L | C1 |
| X2 | Cross-workspace vector reuse by content hash (worktrees, clones, branches), multi-repo query-time merge (Cursor content-hash cache, Semble) (roadmap@bbd3292 X2) | research | L | S1 |
| X3 | Dependency scope: lockfile to registry sources, read-only, version-keyed; optional SCIP ingestion (SCIP, Glean, Context7 pattern) (roadmap@bbd3292 X3) | research | L | X2 |
| X4 | Git-aware channels: `changes_since(ref)`, commit-message channel mapped to touched files (BM25 over commit messages up to +80% MAP, arXiv 2502.07067) (roadmap@bbd3292 X4) | research | M | E2 |
| M4 | Matryoshka dimension truncation (`embedding_dimensions`) and int8/binary storage with rescoring; a storage item, search is 60 ms here (roadmap@bbd3292 M4) | research, paused | M | - |
| A5 | Availability and safety extras: lexical results while the first index builds, secret redaction on returned text, agent-supplied `annotate` summaries keyed by chunk hash, optional bearer token on the loopback HTTP surface (roadmap@bbd3292 A5) | research | M each | A1 |
| P1 | Packaging: cargo-dist installers (shell, PowerShell, MSI, Homebrew) and `service install|uninstall|start|stop` via the `service-manager` crate (roadmap@bbd3292 P1) | research | M + M | - |

### Investigations

| ID | Question | Status |
| --- | --- | --- |
| I-1 | Is shelling out to `rg` worth keeping over in-process grep crates, LanceDB FTS or Tantivy? Design: L1's four arms on this repository and the mined set, lexical-only; metric: nDCG@10 and hit@3 per stratum, p50 and p95, index size and build time, new crates; decision: see L1 (roadmap@bbd3292 I-1) | absorbed(K21a)
| I-6 | Does evaluating mined queries at HEAD, not at the parent commit, bias results? Design: for 50 mined commits, index a `git worktree` at the parent into a disposable data dir and compare; decision: if the bias is above 0.05 absolute, extend E2 with an `--at-parent` mode (roadmap@bbd3292 I-6) | research |
| I-8 | How expensive is harvesting LSP references and call hierarchy at index time with rust-analyzer, jdtls and clangd? Design: time per symbol on this repository and a Java fixture; the 50 ms decision rule is retired by d396 (roadmap@bbd3292 I-8) | absorbed(K11d)

## Deferred

- Adoption logging from real sessions (owner, 2026-10-17, d328): until LCI reaches release stage, tier-3 agent runs are
  the only adoption measure; occasional CLI use is qualitative only.
- Freshness: F1 (verify on read) and F2 (staleness after real edits) are cycle 1 slices K2b and K2a (d396). A freshness
  contract stays a release blocker.
- A/B-era items: the U1 4B pass and the other agent A/B reruns rebuild their indexes on the current model (d304); they
  wait for a real need and a tier-3 slot (d327). Occasional-use and uptake questions are recorded in d122.
- Direction-paused retrieval items (d313): the embedder and reranker trials, window and instruction sweeps, and the
  items marked `paused(direction)` above. Their last numbers are DROPPED lines in the log.
- Model-controlling setup for development is allowed; a release LCI stays fully configurable with defaults (d318).

### S4 freeze checklist (owner)

Old wording (spike roadmap@bbd3292 S4 freeze checklist) also lists the A/B decision bullet, the C2 `Contribution::Filter` question, the `EdgeKind` enum question and the C5 closed label list.

Freezing the trait and the RunRecord schema means renaming the schema id to `lci.runrecord/1`, copying
`runrecord-0.schema.json` to `runrecord-1.schema.json` and changing either only with a new id from then on. Do it after
the owner has gone through this list:

- [ ] One end-to-end slice through the trait with a stored map and the lexical map, recorded as RunRecords and scored by
  the offline harness; live rows on the owner's machine replay with no `fuse` mismatch.
- [ ] Contracts C3, C4 and C5 green on every map in the default registry.
- [ ] Trait decisions confirmed or changed: `MapStatus::Partial { note, warning }`, the fusion order rule (legacy three,
  then rank lists by id, then priors) with its tie rule, untyped `MapRequest.params`, `as_of` semantics, `Budget`.
- [ ] The spec set under [docs/spike/multimap/specs](../spike/multimap/specs/README.md) read through for open questions.

## Parking lot

Ideas kept out until the owner changes the product boundary.

| Idea | Why parked |
| --- | --- |
| LLM-generated chunk or file summaries and wikis (DeepWiki, CodeWiki, kit's docstring index) | Needs a generation model, and the only one available is a service LCI must never contact (`AGENTS.md` "Hard rules"). The boundary-respecting variant is A5's `annotate` tool that stores summaries the calling agent already wrote, keyed by chunk hash. |
| Server databases or search engines (Qdrant, Milvus, Meilisearch, Typesense, Elasticsearch, Memgraph, Neo4j) | `AGENTS.md` forbids another database or service; LanceDB already has FTS, `merge_insert`, `optimize` and scalar indexes. |
| Embedded second database for the manifest (SQLite, redb) | Arguably another database; try LanceDB-native and JSON improvements (W1) first; revisit only if the manifest becomes a measured bottleneck. |
| Web UI, system tray, graph explorer | A web frontend is excluded; a tray app is new product surface. |
| Docker packaging | Excluded by `AGENTS.md`. |
| Symbolic editing (rename, replace body) | Conflicts with the retrieval focus; documented duplicate-code failure mode (oraios/serena issue 576). An LSP `rename` preview that returns a WorkspaceEdit without applying it could be revisited. |
| Custom trigram or suffix-array index (Zoekt, livegrep style) | Premature below monorepo scale; `rg` scans this repository in 0.15 s. |
| stack-graphs, Kuzu, CozoDB | Archived or dormant upstreams. |
| Non-commercial models and code (jina-reranker-v3 and v3.5, recent Jina embeddings, GitNexus, jCodeMunch) | Licensing blocks redistribution or reuse (also d084). |
| In-process fallback embedder (fastembed-rs ONNX, model2vec `potion-code-16M-v2`) | Needs model download approval, ONNX runtime packaging and a second vector column; quality is far below Qwen3-4B (CoIR 39 versus about 60 for CodeRankEmbed). Revisit if a service-less mode is wanted. |
| LLM query rewriting or HyDE inside LCI | Would need a generation model and demonstrably drops anchors (CORE-Bench); the calling agent can rewrite its own queries. |
