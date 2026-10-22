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

On main: K12c (marksman, PR 183), N104 + N106 (tree barrier for navigation and search, PR 184, d495, d496) K18a (`seeds.repo_map`, PR 185, d485) N107 (child stdin null, PR 186, d503) and ASYNC-2 B (per-uri diagnostics wait, PR 187, d479). In flight: K11d-2a (the rust count arms, then the serial latency pass) and N108 (the edit-loop cost of the barrier), measured in the same quiet window. Held until the weekly reset (d505): K39a, ASYNC-2 C (parked until the rust latency pass), K11b (after N108), K20c; then the K11d-2 docs PR. Only docs PRs and the coordinator rewrite this line; a feature PR leaves it alone so parallel PRs do not conflict (d476). The live state is the coordinator's run checkpoint.

## Cycle 1

Plan reviewed by the planning workflow, the coordinator and a cross-family reviewer (d397). Milestones run in order;
inside a milestone the order below is the default. Each row is one slice and one commit unless marked. Check = what the
slice's tests must show (ceiling test: works given ideal input; mechanism: the code path does what it claims). Every
new behaviour is a `[run_profile]` key whose default keeps today's behaviour unless the row says otherwise.

M2 runs as three lanes by real dependencies: {K7b, K10a}, {K8a, K7c} and {K9a, K9b}, converging on K11d; K11e is
independent, and K20b comes before K11b so the default is tested with SCIP absent. Ceiling and mechanism checks show that
an arm works: no slice claims a gain over rg or LSP before the paired screening of cycle 2 (d422).

| Slice | Item | Check | Size | Absorbs |
| --- | --- | --- | --- | --- |
| **M1** | **Profile, freshness harness, no-service path**: done 2026-10-20 (K2b, d420) | | | |
| **M2** | **Language servers as the source of truth** | | | |
| K7b | Partly done: shared LSP sessions across traverse calls, `[run_profile.lsp] sessions` (d414); K7b part 4 landed (SHARED, d454): `sessions = "shared"` is the default, with per-adapter routing: only a server that lists the configuration files of its tree stamp (`LspAdapter::stamp_manifests`, every adapter but jdtls) and watches its workspace (`LspAdapter::watches_workspace`, rust-analyzer alone) or is synced (`sync = "watch"`) keeps a session across calls, every other server starts per call, and every kept session passes a tree barrier before a walk uses it (a stamp from the index's file list, one directory read per stamped directory, one level of child directories and the configuration files, no recursive walk; a changed or failed stamp restarts the server; barriers of one workspace are serialised), so a reused server is never staler than `per_call` for any change inside the stamp's coverage, and its answer carries a `kept session` note (the residuals are listed in the README and d454); the outline cache keyed by content hash, `[run_profile.lsp] outline_cache` (d424); readiness signals per server as a report, `Status.analyzers` and notes, no key (K7b-3a, d446). Remaining: the `busy_repeats` feedback (K7b-3b), after SHARED (d454) merges and the N8 run (`maps lsp-cost` with K7b-3a's analyzer states): the dark calls it repeats exist only on a resident busy server, so it lands only if they are a visible share of the loaded arm's calls, else cycle 2 (the K24c run that the sanity-8 freeze waited for is done, d459) | Fake LSP tests | M | N8 (readiness part) |
| K37b | Done (d475): concurrent walks as the load test of SHARED's tree-stamp barrier, with the fake server and no product change. No stale reuse within the stamp's coverage, restarts serialised per workspace, correct "kept session" and "restarted" notes, and no per-server request queue needed (no cross-talk observed; per-id matching by construction in the transport, d422, untested against an out-of-order answer; K37a stays dropped). New fake flag `--ignore-watched-files` (test support, off by default). The optional real rust-analyzer smoke was not run and does not gate (d470) | Four integration tests and one timer-free unit test, no sleep or wall assert | S | - |
| ASYNC-1 | Done (d470): the three small fixes (2 to 4) of the async audit, no default or key change. `lsp.live` expansion bounded by the walk's `deadline_ms` only (`CALL_TIMEOUT` removed); the warm start unbounded by any clock, ended by its abort (`Manager::prime_languages(.., None, ..)`, each probe bounded by `readiness_timeout_seconds`); the version probe memoised per process (resolved path, modification time, size; success only). Two of the three 2026-10-21 wall-clock flakes fixed (`models.rs` breaker test, `grep.live` tests). | Paused-clock and gate tests, no sleep or wall assert; Freshness answers byte-identical except notes that no longer carry the deadline note | S | - |
| ASYNC-2 | A landed (d479, PR 180: `didOpen` once per changed text, per-session uri to hash table); D landed (d480); B landed (per-uri diagnostics wait, d479 B note, PR 187); C parked until the rust latency pass (d505). The rest of the async audit. `didOpen` only on changed text (per-client uri to hash) and a per-URI `publishDiagnostics` wait replacing the 3 s `head_start` (python, typescript, go, clangd); readiness by event (rust-analyzer's 10 s cap becomes `wait_ready(remaining)`, repeats on the busy/ready watch, `Manager` owns its sleeper), measured by `cargo xtask maps lsp-cost` and K11d-2 per adapter; the test-timing item (the wall-clock asserts of the audit, a scanner seam in `channel_maps.rs` for `the_live_lexical_map_passes_c1_and_c6_and_fails_open_without_ripgrep`, the `lsp_stuck_seconds` clock); polls to waits (index watcher, data-dir lock, `try_wait` loops, the cargo gate); commit D (d480): the data-dir lock, the `try_wait` loops and the cargo gate are waits on events (lock release, child exit, gate release), the index watcher stays a poll by contract | Second definition on one file costs no sleep; lsp.live expansion on a Python fixture warm under 1 s; no `elapsed` assert in the listed tests | M | - |
| K10a | Done (K10a-1 d442, K10a-2 d449): place table without SCIP, `[run_profile.lsp] places` (`"scip"` default or `"outline"`, d442): `OutlinePlaces` over the LSP outline, ids from the index else minted with the file's language; the `LostEdge` gap closes under `outline`; `lsp.live` stays labelled `SiteDrift` because card sites are the index's. Rust members the index lacks take the ids of the `sym.defs` extractor's declarations (`chunk::extract_for_path`, not the bundle store, which `traverse::run` cannot reach; `CrateNames::read_paths`), so Rust ids equal the index's. Left open: placing a Symbol seed whose file was never outlined (module-path inversion, with K21a, d442), outlining destination files so `holder` names a reference in a never-outlined file | `lsp.live` runs with `scip.mode = off`; a shifted-line placement check: after lines are inserted above a symbol its place follows (closes the freshness `LostEdge` gap under `places = outline`; `lsp.live` stays labelled `SiteDrift`) | M | - |
| K7c | Document lifecycle: open once, `didChange` on edits, `didClose` on eviction, no fixed sleeps (after K8a) | Fake LSP event log; the value check is K11d's latency measurement (per adapter, `open` against `per_request`, real servers) | M | N2 |
| K11e | Done (d474, first commit of the K11e + N103 PR; N103's `lsp_start_timeout_seconds` is the second): LSP down-window breaker, `[run_profile.lsp] breaker = false` (default off, 30 s per provider, shared by the process's managers): a failed version probe, or a server start that failed without timing out (spawn failure, exit, protocol break), opens the window, and the next probe or start of that provider is refused at once with a note that names it; a started server is still asked, a timed-out start opens nothing, `/ready` still probes. Turning it on by default is K11b | Fake LSP tests: a dead server costs one probe per window, not one per call | S | - |
| K39 | Speed-up: split `run_with_trait_seeds` (src/app/traverse/mod.rs, about 1,176 lines and 141 branch tokens on main at 7684817) into a few phases that return typed values (for example prepared, registered maps, execution, finalisation; each carries only what the next phase needs), keeping the public signature and the report JSON. Why: it is the file where parallel lanes collide (39 commits since 2026-10-08, 4 of the last 20 catch-up merges edited it on both sides), and every new map and `[run_profile]` key lands in that one function. Risk is temporal coupling (setup order, resource lifetimes, freshness stamps, deadline accounting, continuation semantics), not length. Starts only after K11e and N103 land and no open lane edits that file (otherwise the split is the conflict). Owner request via the maintainability audit, d487 | Move-only commits; before the first move pin a byte-identical traverse output matrix (fresh vs resumed handle, an opt-in map off vs on, LSP down vs ready, trace and record output) that every commit reproduces; then the Full gate | M (split before picking if it grows) | - |
| K11d | K11d-2a harness (commit 1) in review (d481); K11d-2 only (K11d-1 landed, d438: `cargo xtask maps lsp-cost` and the numbers on main): after SHARED (d454) the arms `base` and `per_call+sync` write `sessions = "per_call"` explicitly, and a real-rust-analyzer re-run of `--arms base,shared` records the default's cost; per-adapter ms per `lsp.live` expansion for `documents = open` against `per_request` (arms `base`, `open`, `open+shared`, `open+cache`; the acceptance fixture trees; a server not installed is skipped and listed), plus the typescript `outline_cache` interaction d424 deferred to K7c; after K7c. Still open from K11d-1: N8 (the analyzer state at each dark call needs K7b-3a), and the `shared+calls` arm of K9b | Report under `test-results/lsp-cost/` | S | N8 (live diagnosis) |
| K20b | Done (K20b-1, d471): `[run_profile.scip] mode` defaults to `cache`; an index that cannot be parsed is parsed once per file stamp (size and modified time) and later calls say it was not read again; recipes with no index refuse with the hint to run `rust-analyzer scip`. The golden moved by the `scip.mode` line and the hash only; results stamped under an unset mode carry a new profile hash (K11d-1's numbers, d438, carry the old one). No label and no arbitration yet (K20c) | Its own tests: SCIP absent, the remembered failure (traverse unit, MCP, recipes) | S | - |
| K20c | Arbitration of SCIP edges: `role` in `maps[]`, SCIP edges subordinate and labelled, LSP wins a disagreement, confirmed and overruled counts, the navigation.rs ceiling check. After K9b and K11b (no authority exists before them) | Its own tests: SCIP present but subordinate, a disagreement | S | - |
| K11b | `lsp.live` on by default when a server is available (`lsp.default = on`), fails open, spent from the external-call budget; the search channel's default is verified. After K11d (N101 is d450), K11e and K20b | Fail-open test with a dead server; K11d's cold ms in range; tests that assumed it absent are updated | S | - |
| **M3** | **Walk, cards, seeds, router, cost** | | | |
| K10b | Done (d452): `SymbolKind` gains Field, Variable, Interface and Class (names `field`, `variable`, `interface`, `class`), produced only by `OutlinePlaces` from LSP outlines: class 5 -> Class (was Struct), 23 Struct, 11 Trait in a `.rs` file (rust-analyzer's trait) else Interface, 7 and 8 -> Field, 13 -> Variable; enum members (22), impl blocks (19) and type parameters (26) stay without a node (EnumMember is a later XS). Fields and variables are asked like any symbol (the planned request guard was reversed in review: interface properties answer implementation). No adapter version bump: nothing persisted holds an outline node (`sym.defs` and `cache_version` hold the extractor's kinds, which are unchanged), and a bump would re-chunk every index for identical bytes. No key; the default `places = "scip"` is byte-identical | Unit tests | S | - |
| K13 | Done (d453): `[run_profile.traverse] answer_labels` (default `false`): an answer-level `completeness {complete, why, estimate}` (complete = status `complete` and no map lost, else `why` `degraded` with the maps listed; `why` the stop reason; `estimate` the K29 fraction, `TraversalResult.estimate`) and a `freshness` word (`fresh | changed | stale | unverified`) on every card, response-only (trace, checkpoint hash and handle unchanged; nothing withheld; key off is byte-identical). Left open: a `freshness` word on the search report's hits (XS follow-up, after the owner sees the traverse labels); flipping `stat_gate`'s default stays its own slice (d440) | Card schema test | S | - |
| K21a-2 | Done (d459; the 2026-10-21 measurement run `work/run/measure-2026-10-21`, `max-depth=0 --include-seeds`, no gain claim, d422): `seeds.bm25` beats `seeds.scip_names` at depth 0 on traits (set_f1 0.346 vs 0.162, paired n 146, 65 wins and 19 losses; hit@10 0.733 vs 0.508, n 120) and where-to (reg_hit@3 0.60 vs 0.20, n 25; set_f1 0.175 vs 0.006, n 34), at 289 vs 115 answer tokens per item. The d392 regex rg rule beats every seed source on traits (set_f1 0.704), and the walk with `scip.implements` matches it (0.703), so on traits the seeds are not the gap; where-to has only the as-typed rg baseline (reg_hit@3 0.48). `seeds.hybrid` ran lexical-only (8766/8767 down), so it is not the hybrid ceiling. `seeds.bm25` ran through the raw driver (no cost columns) because the bench's `SEED_MAPS` does not know it (XS row before K22a-2). Left open: one 8766 and 8767 window for the hybrid row, the live bench seed map | Gold symbol in the top k per arm (Q2 `hit@10`, Q1/Q3 `set_recall`; where-to file hits), reported under `test-results/`; cycle 2's screening reads the table | S-M | K21a-1, K5a (baseline arms), L1, L4, I-1 |
| K24c | Done (d459): log_odds off (it turns the score bound off, d432: pops per item 6.4 -> 106 on traits, 13 -> 185 on impact, 11.4 -> 188 on where-to; traits set_f1 0.703 -> 0.579, n 146; where-to and impact flat); `relative` has no effect (1 of 146 traits pairs differs, impact decl_f1 -0.004), so it leaves the cycle 2 factor list; a fair cycle 2 log_odds arm equalises the bound. Numbers are in the arm manifest | Measurement | XS-S | - |
| K22a-0 | Done (d461): the bench's `SEED_MAPS` (`xtask/src/maps/bench.rs`) knows `seeds.bm25` and `seeds.literal`; `map_emits` gives bm25 `[Symbol]` and literal `[Symbol, File]` (both match the descriptors). Naming either map in `--maps` turns its `[run_profile.seeds]` key on in a derived overlay under `<out-dir>/<wave>/overlay`, and the row's `config=` note and `score.json` carry its hash; no default changed. `--maps seeds.bm25` ran live on traits and where-to; `--maps seeds.literal` ran live on traits (tauri, 5 items, after the fix below: 5/5 scored, cost columns present); the where-to questions name nothing in code spans, so literal has nothing to search there. A call with no search now says the ripgrep note even for a question without code spans. `lci traverse` opened the search application (the source of `seeds.literal`'s ripgrep) only when `seeds.hybrid` was named or no maps were, so `seeds.literal` alone found no seeds; fixed here in `src/main.rs` (`wants_seed_search`, with a unit test) | `--maps seeds.bm25` and `--maps seeds.literal` run through the bench and carry cost columns | XS | K21a-2, K22a-2 |
| K22a | Done for K22a-0 and K22a-2 (d461, d497): exact-literal seed source over ripgrep, `seeds.literal` under `[run_profile.seeds] literal` (off; K22a-1, d448; `exact_seeds` reuses exact cards and is not this): whole-word `rg` for the names in the question's code spans, each line placed through `SymbolPlaces` (SCIP layer today; K10a's `OutlinePlaces` is the one-argument upgrade), a `file:` seed for declaration lines (the seedless-trait cases). K22a-2 measured (d497; the 2026-10-21 K22a-2 run; `max-depth=0 --include-seeds`, release build of the frozen commit 1d0b097, lexical-only, three trait repos plus impact and where-to): `seeds.literal` alone is weak on traits (pooled set_f1 0.152, hit@10 0.183, against `seeds.scip_names` 0.162 / 0.508 and `seeds.bm25` 0.346 / 0.733) but strong on impact (decl_f1 0.676 against the walk control's 0.436); hybrid+literal beats hybrid on traits (set_f1 +0.065 [+0.040, +0.092], hit@10 +0.142 [+0.083, +0.208]); in the walk the trait seeds come from the traits path, so added seeds change nothing there (B0 vs B1 set_f1 0.703 vs 0.705). The B2 signal is not evaluable (bevy and deno B2 not run). No default changed: `seeds.literal` stays off and goes to question shapes that name literals through K22a-3. Remaining: the seedless-trait mechanism test with K9a's documents fake, which expands the file seed through `lsp.live` under `places = outline`; point the K5a rg arm at `lexical::parse_matches`; B2 on bevy and deno (about 55 min, `armB B2 traits bevy|deno`) only if signal 3 is wanted | Ceiling, beside an rg baseline arm (done, d497) | S | item 4 rest |
| K22a-3 | Route `seeds.literal` by question shape through the K30a router (d456): turn `[run_profile.seeds] literal` on for questions that name literals in code spans (impact-shaped, error strings), off for trait-shaped questions and where-to; the global default stays off (d497) | Impact-shaped items gain decl_f1 over the router's current choice (A1 0.676 and the walk control 0.436 are the reference, d497) with no loss on traits (set_f1 and hit@10 within the paired CI of the unrouted default) | S | K22a-2, K30a |
| K33a | Done (K33a-1 d444, K33a-2 d451): trajectory rows per pop (state, candidates, choice) linked to the outcome (gold hit) in evaluation runs. `lci traverse --trajectory` (CLI only, no key) writes a `pop` event per counted pop; `cargo xtask maps trajectories --runs <bench out dir> --gold-dir <dir> [--scip <index>] [--set <name>]` writes one row per pop to `<wave>/<set>/trajectories.jsonl` with `trajectories.md` beside it (a separate command; no bench file changes). Labels come from the set's own scorer; an unlocated symbol is `null`. Pops come from `end.totals`; a put-back pop the totals do not count is `counted: false`. Cycle 2's learned racer reads the rows | Schema test | S | - |
| **M4-core** | **Maps, servers, safety, mining (unconditional)** | | | |
| K26 | `grep.live` proposes, `lsp.live` confirms the top-k frontier (`walk.confirm`); no Codanna dependency | Cost and quality per call against the walk without confirmation | M | G1 |
| K9c | Type-hierarchy edges. paused(measurement), moved to cycle 2: the K24c control shows SCIP `implements` edges already bring traits to the regex-rg level (d459); LSP type-hierarchy edges matter only under `scip.mode = off` with `places = outline`, which no cycle 1 run measures | Ceiling | S | - |
| K12a | Server roster and per-language readiness; root discovery per server | Live acceptance per server | S | N4 |
| K12c | Done (PR 183): marksman adapter for prose | Live acceptance | S | N95 |
| K18a | Built (d485, merged in PR 185): `seeds.repo_map`, an opt-in start-node map under `[run_profile.seeds] repo_map` (default off; K18a-1): personalized PageRank over the SCIP graph (calls and containment, `implements` edges left out, damping 0.85, at most 30 iterations), with the jump on the top 64 `seeds.bm25` matches, so a function the question's matched functions call becomes a seed even when its own name holds none of the question's words; its top `seed_k` nodes go after the hybrid and `seeds.bm25` ones. No bm25 match, no seeds (no global hubs). Open: the ceiling run (`maps bench --set traits --set where-to --max-depth 0 --include-seeds`, waves `k18a-rm`, `k18a-bm25-rm`, plus the depth-1 walk `k18a-walk` against the K24c control; report the reorder rate against bm25's top 10); park K18a-2 (the SCIP-free graph) when the reorder rate is under 30 % and where-to does not move | Ceiling | M | G2 |
| K19 | Built (d483): `grep.structural`, an opt-in map under `[run_profile.grep] structural` (default off): ripgrep names the files that hold a symbol's name as a whole word, an ast-grep pattern over each file's tree (the chunker's own grammars) keeps the `impl`, `extends` and `implements` headers (`Implements` edges) and the calls (`CallsOrRefs` edges). `ast-grep-core =0.40.1` is the last release on tree-sitter 0.25: it moves with K15a. Open: the ceiling run (`maps bench --set traits --max-depth 1`, waves `k19-ctl`, `k19-on`, `k19-only`; A = the K24c control arms, B = A + the map, C = the map as the only implements source; no gain claim, d422 (11); `maps bench` turns the key on in the arm's derived overlay when `--maps` names the map) | Ceiling on structural queries | M | - |
| K32-1 | `tests.tested_by` (opt-in map over the SCIP impact index, d458): built; open: the ceiling run (`maps bench --set impact` with and without the map, `tests_recall` and `tests_deep_recall` per stratum) | Ceiling | S | - |
| K32-2 | `docs.mentions` (`Mentions` edges from documentation, a wrapper of `grep.live` over `SourceRole::Documentation` paths, d462): built; open: the ceiling run (`maps bench --set impact` with and without the map, `docs_recall`). `i1_section_*` is not a check of this row: I1 needs DocSection nodes and section lines (K32-2b, unplanned, cycle 1 optional) | Ceiling (`docs_recall`) | S | K32-1 |
| K6a | Done (d457; K6a-1 strata, K6a-2 miner): `cargo xtask gold strata` and a `guard` stratum (`core`, `grep_easy`, `agent_trap`) on every scorer, from the gold item alone; `cargo xtask mine-transcripts` counts question shapes and trace classes over synthetic transcripts (the sim trace shape), writing shapes and counts only to `test-results/transcripts/`. Estimated S, was two S (strata plus miner) | Tests | S + S | - |
| K6b | Done (d464): question shapes mined from real Claude Code and Codex transcripts, locally. `cargo xtask mine-transcripts --counts-only` (vocabulary-only report) and the stop `answered` (classed like `gold`) are the only repository change; the converters stay outside the repository. Chains by source, grep_easy/agent_trap/core/unresolved (cells under 5 hidden): Claude interactive (422) 71/<5/64/284, Claude subagent (2230) 111/399/767/953, SDK harness (1196) 217/6/268/705, Codex interactive (173) 21/<5/44/106, Codex subagent (30) <5/<5/<5/25. Per-shape cells, quantiles and the reading against the gold `guard` mix are in d464 | Counts | S | K6a |
| **M4-conditional** | **Codanna, grammar upgrade, optional tools, external gold, baselines: each runs in cycle 1 when its condition holds** | | | |
| K14b | Cargo feature `syntax-codanna`: parse one file through Codanna into LCI nodes | Ceiling on fixtures | M | blocked: owner and Codanna repo owner; K14a NO-GO as specified (d422) |
| K14a2 | Run the built Codanna 0.17.1 binary on the offline TypeScript ground truth (384 and 291 distinct pairs) and join its edges with it; pre-register the kill rule first (suggested: stop below 95% precision or below 85% recall on `this.m()` plus bare calls) | Precision and recall per call shape | S | held for the owner (d422) |
| K14c | In-memory resolution glue; edges with confidence by the rule that picked them | Precision and recall against LSP on fixtures | M | blocked(K14b) |
| K14d | Name-based invalidation: on a change, re-resolve the file's outgoing edges and edges whose target names it added or removed | K2a | S | blocked(K14b) |
| K16 | Codanna parser tests as fixtures; its `tsconfig`, `go.mod` and `pyproject` readers for server root discovery | Tests | S | N4 (part); blocked(K14b) |
| K15a | tree-sitter 0.26 and its grammar crates (javascript and python also to 0.25, K14a); every chunking test re-run; adapter version bump; default-build regression check. Waits for the Codanna decision or a measured grammar need; ast-grep-core needs tree-sitter ^0.25.4 (K19): `=0.40.1` is the last 0.25 release, so K15a re-pins it (0.40.5 or 0.44.1 for 0.26; 0.45 and later need 0.27) | Existing tests | M | - |
| K17 | `oxc_resolver` for TS and JS imports, only if K14a2 shows Codanna's TS resolution is weak | Imports against typescript-go | S | - |
| K12b | typescript-go adapter | Live acceptance | M | - |
| K31 | Opengrep data-flow map, opt-in; consumes Opengrep's existing flow analysis, no engine written | Ceiling on a flow set | M | - |
| K4a | Done (K4a, d484): `cargo xtask gold external convert` turns a SWE-PolyBench Verified `test.csv` into where-to exemplar items at file granularity with the leak stratum, licence, url, revision and sha256 in every item, `--known-repo` overlap marks and a counts report; the LongCodeArena column adapter (`lca_row_from_columns`) is tested on synthetic rows; held out, never used for tuning (stays in cycle 1) | Leak check | S | - |
| K4b | Done (K4b, d499): `cargo xtask gold external fetch` (official Hugging Face files pinned at the revision `main` has, `fetch.json` with url, bytes and sha256, refuses a different hash without `--force`; `Fetcher` test double) and the LongCodeArena JSONL reader (no `parquet` crate, d499: the parquet files go through a one-off conversion outside the repository); leak-rule fixes from the K4a review (stem token split, repository owner ignored, wider generic directory tokens); the real runs: SWE-PolyBench Verified 382 rows, 382 items, leak 298, clean 84; LongCodeArena test split 150 rows, 100 items, leak 85, clean 15, 50 Kotlin rows dropped; counts in the PR body and d499 (`test-results/` is gitignored) | Counts | S | K4a (done) |
| K5a | Baseline arms rg-only and LSP-over-MCP wired into `maps baseline` (no A/B runs); not blocked. K5a-1 landed: the rg arm, `cargo xtask maps baseline rg --set traits or where-to` (d443), with three named rules: `fixed`, `fixed-decl-first` (`--decl-first`) and `regex` (`--rule regex`, traits only; the `rg_arm.py` rule behind d392). K5a-2 remains: LSP over MCP (the `workspace/symbol` arm), after K11d-1's `McpSession` harness | Smoke query | S | item 6 arm |
| K5b | Baseline arms Codanna and Serena wired (no A/B runs); the Serena arm is not blocked, only the Codanna arm waits for the owner | Smoke query | S | - |

M0 filler, any time a build slot is free: K0a follow-ups review: a breaker-open embedder skip in traverse `search.vector` still counts one external call (`AppChunks::embedding_skipped` could return the models' `embedder_skipped()`), and the `EXTERNAL_CALLS` doc in src/app/traverse/mod.rs says no map makes any (d416). The stat-gated re-hash left this filler and landed as K38 (d440).

Finish line of the cycle 1 report (d422): M2 + M3 + M4-core plus every unblocked M4-conditional slice. Conditional slices
still run in cycle 1 when their condition holds; K4a and K4b do not move to cycle 2.

Deferred to cycle 2: rust-analyzer in process (`ra_ap_*`), ty or Pyrefly (in process or as servers), ts-rust as a TS
server arm, rung 2 model training, a gix blame map, a tantivy engine for K21 (only after a measured gap).

### Cycle 1 time estimate

Agent wall time per slice, for the whole loop (plan, implement, Full gate, review, PR, CI): XS 0.5 h, S 1.5 h, S-M
2.5 h, M 3.5 h. Measured on 2026-10-20 for S slices only (K1a, K1b, K1c in two parts: 1 to 1.5 h each, one at a
time; the Full gate takes about 25 min and CI about 20 min); K0a part 1 (S-M) took about 3.5 h with two review
rounds. M is not measured yet. Update this table at each milestone end with the measured hours. Re-base it at the end of M2 (d422): the warm Full gate
is now about 3 min and CI about 8 min (d415, d419), so per-slice time is review rounds and serial merges, and the
`Merge wait` column then records the measured wait for a merge into `main`. Re-based on 2026-10-21 (d459): slice counts are the open rows
and hours are the size table's figures; the Measured column holds the measured wall time.

| Milestone | Slices left (by size) | Sequential hours | Measured | Merge wait | Note |
| --- | --- | --- | --- | --- | --- |
| M1 | K0a part 2, K7a, K2a (M), K2b (S-M), K23a, K35a (XS), K34a | ~12 | ~20 h wall (roadmap merge 03:49Z to K2b), with K7b-1, K7b-2 and K9a of M2 and a 2.5 h speed-up lane in parallel | not tracked | five lanes in parallel since 2026-10-20 |
| M2 | 5 active: SHARED (M), K11e + N103 (2 S), K20b-1 (S, implemented, in review), K11b (S), K11d-2 (S); K7c (S-M) conditional on K11d-2's `base` arm | ~11, K7c +2.5 | ~30 h wall so far, with K11d-1 at 2 h machine time | not tracked | serial chain SHARED, K11e, K20b-1, K11b: three default changes, each a review round of about 2 h; K37b follows SHARED; closes 10-22 evening at the earliest (usage stop to the weekly reset, N108 before K11b) |
| M3 | K22a-3 (S); K30a (PR 160) landing; K22a-0, K22a-2 and the docs PR done (d461, d497) | ~1.5 | ~8 h wall plus 6.5 h measurement machine time (lane A 3.5 h, lane B 3 h, debug builds under load, for 25 to 90 min of estimate), plus K22a-2 about 5 h wall (build 32 min, main run 4 h 12 min, release, under load; d497) | not tracked | later measurement lanes use a release build of the frozen commit, one lane at a time (d459) |
| M4-core | K26 (M), K18a (M), K19 (M), K12a, K12c, K32-1, K32-2, K37b (moved to M2), K6a, K6b (S each) | ~20 | - | - | closes about 2026-10-23; K32-3 (pickaxe) is a decision, K9c is cycle 2 |
| M4-conditional (unblocked) | K5a-2, K5b, K4a, K4b, K12b (M), K31 (M); K15a and K17 on their conditions | ~15 | - | - | about 2026-10-23 to 2026-10-24; most uncertain: Codanna on tree-sitter 0.26, typescript-go, marksman, Serena and the ast-grep pin on Windows, live server checks, external gold |
| Total | about 27 slices left | ~51 with K7c, plus about 5 h of measurement machine time | - | - | hours are the size table's figures, not re-measured per slice |

Pace on 2026-10-21 (sanity 10, d459): PRs 139 to 159, 21 merges in 8 h (09:36 to 17:39 local), 2.6 per hour with four lanes,
33 merges since midnight. Parallel lanes (separate worktrees, up to three cargo builds gated on free RAM) are limited by
dependencies, by merges into `main` one at a time and by the build slots. Estimate from the remaining work above: M2 and M3
close about 2026-10-22 12:00Z (superseded for M2 by sanity 13, d505: see its Note), M4-core about 2026-10-23, the unblocked M4-conditional slices about 2026-10-23 to 2026-10-24;
the earlier 2026-10-22 to 2026-10-23 end holds for M2, M3 and M4-core only with one review round per M row. Measurement
machine time is about 5 h (the SEED_MAPS fix and K22a-2 1 to 3 h, the SHARED stamp walk and an edit smoke check 1 h, K11d-2
1 to 2 h, N8 1 h). Risks, largest first: the weekly usage stop (thrift is suspended until 2026-10-22T16:00Z, but the 95 %
weekly and 85 % five-hour rules still cut lanes); SHARED's 1 s stamp-walk kill rule on deno-sized trees (30k files); lanes on
debug builds under load; M rows with two review rounds (about +30 %); Windows gaps for marksman, Serena and the ast-grep
tree-sitter pin; outputs dying with worktrees (copy to `work/run` first); this file's "Now and next" going stale after every
merge burst (d476: feature PRs no longer touch it, so it lags until the next docs PR).

S evidence from the closed external-gold group (sanity 13, d505): K4a took about 2.7 h wall (21:1xZ to the 23:5xZ merge) and K4b
about 2 h (23:5xZ to 01:38Z), one review round each, both S. No Measured cell is filled from them. Machine time so far for M2: the
K11d-2 counts pass (about 1 h 20, 00:27Z to 01:45Z) plus builds; the latency pass and N108 are still owed (about 1.5 h, quiet).

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
| E6 | Identifier-query stratum: `cargo xtask mine-symbols` samples declaration names (seeded, names of 5+ characters, defined in one file, not test-only) with `expected_paths` = the defining file, tag `identifier`, committed apart from `self.toml`; unit tests on synthetic input; one fusion-only and one reranked baseline recorded (roadmap@bbd3292 E6) | ready | S | E2 done |
| N8 | The LSP channel goes dark for 15 to 25 s windows under back-to-back searches (hypothesis: rust-analyzer swaps its crate graph after build scripts and proc macros load, invalidating the primed symbol index); diagnose and fix (roadmap@bbd3292 N8) | absorbed(K7b (readiness), K11d (the live diagnosis)) | S-M, live | N7 done |
| N16 rest | A workspace search on a legacy-only table waits on the store-wide legacy guard; the revision pin and the re-read after a commit during the freshness check are not pinned by a failing test (roadmap@bbd3292 N16) | ready | S | - |
| N18 | Low batch (item (b), the Java readiness probe, was fixed as N77, d190): (a) `manifest::save` removes the destination before `rename`; (c) `serve --port 0` advertises port 0; (d) three navigation tests assert rust-analyzer's flag; (e) the embedding model listing is probed at the origin; (f) no per-file size cap (roadmap@bbd3292 N18) | ready | XS each | - |
| N20 | Cancellation and lock scope: (a) the MCP index and search handlers ignore the request's cancellation token; (b) lock scope; (c) done (a watcher pass queued for the workspace write lock ends on `unwatch`); (d) partly done (the data-directory file lock was fixed in N91; a `probe.compile` probe now waits for its scratch-checkout gate only as long as the query budget; still open: the scan threads in `src/app/indexing.rs` (`chunk::scan_incremental_with`, `fingerprint_with`) outlive a dropped request and run to their end, which needs cooperative cancellation in the scanner); (e) Ctrl+C graceful shutdown has no deadline (roadmap@bbd3292 N20); per-server request ordering (the part K37a was meant to absorb) is closed: no queue is needed (d475: no cross-talk observed; id matching by construction) | ready | S-M | N16 done |
| N21 | Two open test-quality items (a third, a fixed 500 ms sleep in the data-dir lock test, was fixed as N83, d193): (a) `evaluation_reports_required_failures_optional_skips_and_json_metrics` checks only that `hit_at_1_rate`, `mrr` and `role_distribution` exist, not their values (check whether another test pins them); (b) its last assert checks an `EvaluationFile` the test built itself (tautological): exercise `load_definition` instead (roadmap@bbd3292 N21) | ready | XS | - |
| N25 | Six weak assertions in `tests/cases/indexing.rs`: (a) `a_hung_reranker_is_tried_once_and_keeps_earlier_warnings` sets a 1 s reranker timeout but bounds the elapsed time at 10 s; (b) the unreachable-reranker test asserts only `reranking_ms` < 5000 (count accepted connections instead); (c) the 200 ms budget tests use 150 to 1200 ms bounds; (d) a stop-word query is not in the fixture; (e) the traced-search test lacks a rank >= 1 assert; (f) the `mixed_languages` deletion test checks top-8 over equal vectors without a chunk count (roadmap@bbd3292 N25) | ready | XS | - |
| N26 c | `search_symbols` has no result cap; record the size of a broad name's answer first (roadmap@bbd3292 N26) | ready | XS | - |
| N32 | Rewrite the model-facing strings within budgets: `search_code` 350 characters (436 today), `search_symbols` 280 (328), instructions 450 (732); state facts only (d278): what comes back, the first-call index cost and the navigation preconditions; no advice on when to prefer Grep, which goes to an on-demand skill measured as its own arm. Its former kill-rule thresholds move to the cycle 2 screening design (d396). Text is `cloud-ok`, the A/B arm is `local`. See [agent-payload-notes.md](agent-payload-notes.md) (roadmap@bbd3292 N32) | ready after N26 | S | N26 |
| N33 | Schema documentation for the four search and navigation tools: describe `query`; list the valid `languages` and `source_roles` values (roles: source, test, benchmark, example, generated, documentation, configuration); state glob syntax, that an empty `include_paths` matches nothing and that empty language or role lists are errors; document `include_declaration`; describe `workspace_path` on every tool (today only on `WorkspaceArgs`, 73 characters); stop hard-coding "(8)" in the `top_k` text; drop what N32 makes redundant (the language-server list repeated 4 times, about 400 characters). Acceptance: a test that every argument of the four tools has a description; `tools/list` no larger than today (roadmap@bbd3292 N33) | ready | S | N32 |
| N34 | `default_top_k` 5 versus 8 as a measured arm: 61 appearances, 57 at rank 5 or better, ranks 6 to 8 carry 18.7% of returned lines, 20 of 94 answered files were at rank 6 or later (U1 calls); lowest priority (roadmap@bbd3292 N34) | ready, local | S | N28 done |
| N42 | Incremental-index peak memory: `Store::cached_vectors` plus a vector clone scale with the whole index (estimate 0.9 to 1.5 GB at 150,000 to 250,000 chunks of 768 floats, not measured); measure first; fix direction: load only vectors whose chunk hash is still present and move instead of clone; acceptance: state the bound and a test fails above it; `cloud-ok` (roadmap@bbd3292 N42) | ready, low | S | - |
| N48 | Windows verbatim `\\?\` prefix at paths of 260 characters or more breaks prefix comparisons (roadmap@bbd3292 N48) | ready, low | XS-S | - |
| N87 | The live-acceptance workflow does not use `--require-all`, so a missing tool reads green (roadmap@bbd3292 N87) | ready | S | N56 done |
| N94 | Extension comparisons are case-sensitive in 11 places (clippy pedantic census): a `.RS` or `.Py` file is skipped; decide and test (d371) | ready | S | - |
| N95 | A Markdown language server (marksman, MIT) for prose navigation: links, headings, references; the language bar's LSP item is waived for prose until then (d375) | absorbed(K12c) | S | - |
| N96 | Re-check the error-to-commit premise (item 5) with full clones and the error text of linked GitHub issues, under a new pre-registered rule (d377) | ready | M | - |
| N99 | System-1 rescoring rung: a decision model orders the reranker's 24-candidate shortlist, the seam where N98 (killed) showed the reranker's value (d382). Gate K0 passed on a hosted System One prototype arm (d383); slice 2 (the opt-in rescorer rung, with status-only HTTP errors, transmitted trace sizes and a joint-batch replay guard) merged as PR 88 and the default is unchanged; K1 passed (slice 3, d385); slice 4 failed bar S (d387): the local 4B decider loses on flow and mined search and needs about 19 GB of VRAM, so the reranker stays the default and the rung stays opt-in. The hosted model is never shipped | done (local rung killed, d387) | M | - |
| N100 | Where-to routing: an un-steered `traverse` call on a where-to question returns 16 answers (8 seeds and 8 walk) where the `where-to` preset returns the 8 first-stage answers, with set_f1 +0.015 [+0.005, +0.026] and 42% fewer answer tokens; cut to 8 the two are identical, so the gain is the shorter list (d389). Whether to route such questions to the preset (question-type detection, or a fact in the tool description under the facts-only rule) needs its own pre-registered test. Detection (b) was killed at B0 (R 0.339 against the 0.70 bar, d391) | partly done: detection killed (d391); routing is the caller's `steer.policy` (K30a, d456); the description fact (a) is parked until a tier-3 run is approved | S | - |
| N101 | The 5 s readiness probe of `lsp.live` (`PROBE_TIMEOUT`, the server `--version` call in `ManagerBackend::open`) times out under machine load and fails a walk before the server is asked anything; seen in K9a's runs. Decide a configurable or wider deadline and test it under load | partly done: the default is 15 s and `lsp.live` warm-starts shared servers at the first touch (d450); the probe ms under lane load is re-measured after merge (`lsp-cost --arms shared`) | XS | - |
| N102 | `[run_profile.lsp] sync` on Windows: `notify` 8.2 stops watching a directory without telling the handler when ReadDirectoryChangesW fails with anything but aborted or access denied (a 16 KiB buffer overflow during a large checkout gives ERROR_NOTIFY_ENUM_DIR), so the sync goes silent and never rescans. Detect it (a periodic stat check of the table, a bridge of notify's `log` errors into a rescan, or a larger buffer upstream) and re-open the watcher with a rescan | Unit test with an injected watcher failure; a burst of thousands of file writes on Windows reaches the server | S | K8a |
| N103 | Done (d486, second commit of the K11e + N103 PR): `lsp_start_timeout_seconds = 0`, decided: a start bound of its own, default = the request bound (`0` keeps today's behaviour, the start is bounded by `lsp_timeout_seconds`; not in the profile hash). Was: a language server's `initialize` runs under the per-request `lsp_timeout_seconds` (an inactivity timeout; default 60 s, so it bites a user who sets a low value, and the tests that did), and a timed-out `initialize` is a transport failure that is not retried (`fake_csharp_lsp_timeout_and_malformed_protocol_return_bounded_errors` pins one spawn): where the value is low a healthy server whose process start is slow (a loaded machine; test fixtures of 5 s and 10 s failed under four parallel builds, d439) becomes a failed start. Decide whether the start gets its own wider bound (for example `lsp_start_timeout_seconds`) and `lsp_timeout_seconds` keeps bounding requests to a started server; test it with the fake server's `--initialize-delay-ms` | done | S | - |
| N104 | Navigation (`symbols`, `definition`, `references`) and the search channel's `provider_symbols` reuse the service's kept language-server sessions without a tree-stamp barrier, so a non-watching server's answer can predate an edit (pre-existing, found in the SHARED review 2026-10-21); decide a barrier or a per-call route after SHARED's index-list stamp lands. Done in the N104 + N106 PR (d495): navigation and the search channel pass the tree barrier once per call and carry `lsp:` notes (`NavigationReport.notes`, `SearchReport.notes`); owed: a release-build navigation p50 on the bevy tree against the 150 ms kill bound (not run) | Done (d495, PR 184) | S | - |
| N106 | K11e test gap (review of d474): no test fails if `per_call_manager` stops attaching the shared windows (`.with_breakers(..)` in src/app/traverse/mod.rs): the dead-binary case fails at the probe through the shared manager, and the second manager in the ceiling test borrows the windows directly. Add a fake that passes `--version` and exits at start, two walks, the second refused through the call's own manager. Done in the N104 + N106 PR (d496): fake flag `--exit-at-start`, test `a_per_call_manager_opens_the_offered_managers_windows` | Done (d496, PR 184) | XS | - |
| N108 | An edit-then-search loop restarts rust-analyzer on each call under the default (N104 extends d454's restart on a changed stamp to navigation and search). Measure the edit-loop cost of the tree barrier (d495) on a real Rust repo (bevy, `sessions = "shared"`): edit, `search_code`, edit, `search_code` ... with rust-analyzer kept in the service's manager (`maps lsp-cost` with an edit step between calls, no new harness). Record per edit: restarts, whether the LSP channel answered or ended as an over-budget note (post-edit searches whose LSP channel times out), and the milliseconds to ready. Only then decide whether the search channel alone keeps a `watches_workspace` adapter's session and labels the answer (keep and label) while navigation stays exact (barrier). Evidence-gated: d495's rule (the barrier for both) stands until the measurement says otherwise. Run in the quiet window with the K11d-2 latency pass (d505); before K11b | ready, evidence-gated | S | - |
| N109 | `Manager::tree_barrier` (lci-core/src/lsp.rs, the stamp block near :1411-1442) restarts a kept session when the index has no file list or the stamp fails, and the caller's note then says `the tree changed since its last call`, which is false in those cases. Return a distinct reason per restart (tree changed, no file list, stamp failed) and word the note by it; the `restarted` list becomes reasons, so the navigation, search and traverse notes and the tests that pin them change together | ready | S | - |
| N110 | The channel-maps `LspMap` status (src/app/search/channel_maps.rs near :405-427) drops the barrier's `restarted` and `reused` that `lsp_channel` returns, so `fuse_through_maps` notes differ from the search report's. Fold them into its status note and drop the `without_barrier_notes` mask of tests/cases/indexing.rs where it is then unnecessary (optional, small) | ready, low | XS | N104 |
| N111 | ASYNC-2 B follow-up: a navigation call cancelled in the middle of `didOpen` whose write then succeeds records the text with no diagnostics wait, so the next caller skips the open and may be answered before the server's analysis ends. Keep a pending wait (per uri, with the version) for the next caller. PLAUSIBLE (Sol re-check 1), no real-call repro | ready | S | ASYNC-2 B |
| N112 | Flaky on CI once (2026-10-22, PR 188, docs-only, same code as the passing PR 187): `freshness::with_sync_on_the_session_is_told_about_the_edit_and_the_barrier_replaces_it` read the replacement session's `sync_status` as `walks 0`, no backend, before its start walk ran (expected `walks 1`). Wait for the sync's walk on a real signal (not a sleep) before asserting, or have the session start await its first walk | ready | XS | N104 |
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
