Status: PLAN

# Roadmap

This file holds plans only: what is open, what is next and what is deferred. Results, history and the reasons
behind decisions live in [decisions.md](../decisions.md), where `roadmap@bbd3292 <ID>` names the text of an item in the
last version of the old roadmap that carried full result paragraphs (`git show bbd3292:docs/development/roadmap.md`).
Ids are never renumbered or reused.

## How to use this file

1. Take the next slice of "Cycle 1" in table order that the owner's cut keeps in cycle 1 (see "Now and next"), then "Earlier items outside cycle 1", then
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

Cycle 1 implementation is complete (finish line d422): the last feature merges were K39b (d564) and K12a-2 (d533); M2 closed 2026-10-23 without K11b-6, whose release-build measurement failed, so per-language `places = "auto"` moves to cycle 2 with mixed-tree surfacing (d562). On main: the docs and code cleanup (PRs 226 onward, finished 2026-10-24, d573) on top of PR 225. Next: the cycle 1 report went to the owner on 2026-10-23; the owner's cut (asked 2026-10-23) and the answers to the report's questions follow, then cycle 2. Waiting on the cut: sanity 20 (d563) recommends K12b-2 and K31 move to cycle 2 and K12b-1 stay parked on its branch (nothing merges before the tsgo facts check and the lsp-cost pair, d537), and the placement of the K20c rust and K26 value windows (each needs a release quiet window with no cargo; the arbitration stays off until the K20c measurement passes), which sanity 20 proposes for cycle 2. K7b-3b and K11d-2b stay open (their rows), and ASYNC-2 C stays parked. Per the autonomy rule the coordinator waits for the owner's word after the report. Only docs PRs and the coordinator rewrite this section; a feature PR leaves it alone so parallel PRs do not conflict (d476). The live state is the coordinator's run checkpoint.

## Cycle 1

Open slices of the cycle 1 plan (reviewed by the planning workflow, the coordinator and a cross-family reviewer, d397); done slices are in the log. Milestones run in order; inside a milestone the order below is the default. Each row is one slice
and one commit unless marked. Check = what the slice's tests must show (ceiling test: works given ideal input; mechanism: the code path does what it claims). Every new behaviour is a `[run_profile]` key whose default keeps today's behaviour unless
the row says otherwise.

Ceiling and mechanism checks show that an arm works: no slice claims a gain over rg or LSP before the paired screening of cycle 2 (d422).

| Slice | Item | Check | Size | Absorbs |
| --- | --- | --- | --- | --- |
| **M2** | **Language servers as the source of truth** | | | |
| K7b | Remaining: the `busy_repeats` feedback (K7b-3b). The rest is done (d414, d424, d446, d454). The dark calls it repeats exist only on a resident busy server, so it lands only if they are a visible share of the loaded arm's calls (after the N8 run: `maps lsp-cost` with K7b-3a's analyzer states), else cycle 2 | Fake LSP tests | M | N8 (readiness part) |
| ASYNC-2 | C, parked (A d479, B d479 and d512, D d480 are done; d505, d512): readiness by event (rust-analyzer's 10 s cap becomes `wait_ready(remaining)`, repeats on the busy/ready watch, `Manager` owns its sleeper), measured by `cargo xtask maps lsp-cost`; and the test-timing item (the wall-clock asserts of the audit, a scanner seam in `channel_maps.rs` for `the_live_lexical_map_passes_c1_and_c6_and_fails_open_without_ripgrep`, the `lsp_stuck_seconds` clock). Parked because the remaining rust cost is the per-call cold start, which shared sessions already avoid (d512) | No `elapsed` assert in the listed tests | M | - |
| K10a | Open remainder (K10a-1 d442 and K10a-2 d449 are done): placing a Symbol seed whose file was never outlined (module-path inversion, with K21a, d442), and outlining destination files so `holder` names a reference in a never-outlined file | Tests | not sized | - |
| K11d | K11d-2b remainder (K11d-1 d438, K11d-2a d509, d510 and d512 are done; the `open` arms do not exist because K7c added no key, and K7c's before and after on the same arm text replaced them, done for typescript and python, d545): the typescript `outline_cache` interaction that d424 deferred to K7c is unmeasured (typescript opens documents since d544); N8 (the analyzer state at each dark call, with K7b-3a's states, d446); the `shared+calls` arm of K9b | Report under `test-results/lsp-cost/` | S | N8 (live diagnosis) |
| K20c | K20c-1 (d534, PR 207) and K20c-2 (d556, PR 216) are done: `[run_profile.scip] arbitration` (default `false`), `role` labels and counts. Open: the pre-registered rust measurement (false-overrule rate at most 5 %, the edit pass, no card or deadline loss; the coordinator's lane, with a Sol review for the flip; rust only, d556), which needs a release quiet window with no cargo; the arbitration stays off until it passes; its placement (cycle 1 or 2) is the owner's cut (d563) | False-overrule rate, edit pass, card and deadline loss | S | - |
| K11b | K11b-6 only (K11b-1 to K11b-5 are done: d523, d524, d550, d555, d557): per-language `places = "auto"` (SCIP places where the loaded graph has a node, else the server's outline, per language). Not merged (branch `feat/k11b-6-per-language-auto`): its pre-registered release-build measurement failed, 0 python `lsp.live` cards on the mixed tree, because Contains edges carry prior 0.5 and the 20-pop budget goes to the rust seeds' neighbours (d562). Cycle 2, together with mixed-tree surfacing (Contains prior, budget, top_k, a python-seeded mixed question, a discovered-nodes rule; seed-side before priors, d563), under a new pre-registered plan. Until then `places = "auto"` is per workspace, not per language (d553) | Pre-registered mixed-tree release measurement | S | - |
| **M3** | **Walk, cards, seeds, router, cost** | | | |
| K13 | Open remainder (answer labels are done, d453): a `freshness` word on the search report's hits (XS follow-up, after the owner sees the traverse labels); flipping `stat_gate`'s default is its own slice (d440) | Card schema test | XS | - |
| K21a-2 | Open remainder (measured, d459; the 2026-10-21 run `measure-2026-10-21` ran lexical-only, so it is not the hybrid ceiling): one 8766 and 8767 window for the `seeds.hybrid` row | Gold symbol in the top k per arm (Q2 `hit@10`, Q1/Q3 `set_recall`; where-to file hits), reported under `test-results/`; cycle 2's screening reads the table | S-M | K21a-1, K5a (baseline arms), L1, L4, I-1 |
| K22a | Open remainder (K22a-0 d461, K22a-2 d497 and K22a-3 d519 are done; `seeds.literal` stays off): the seedless-trait mechanism test with K9a's documents fake, which expands the file seed through `lsp.live` under `places = outline`; point the K5a rg arm at `lexical::parse_matches`; B2 on bevy and deno (about 55 min, `armB B2 traits bevy|deno`) only if signal 3 is wanted | Ceiling, beside an rg baseline arm | S | item 4 rest |
| **M4-core** | **Maps, servers, safety, mining (unconditional)** | | | |
| K26 | K26-1 (d528, PR 197) and K26-2 (d559, PR 220) are done: `walk.confirm` (default `false`); `grep.live` proposes, `lsp.live` confirms the top-k frontier; no Codanna dependency. Open: the value measurement, cost and quality per call against the walk without confirmation; it needs a release quiet window with no cargo, and its placement (cycle 1 or 2) is the owner's cut (d563) | Cost and quality per call against the walk without confirmation | M | G1 |
| K9c | Type-hierarchy edges. paused(measurement), moved to cycle 2: the K24c control shows SCIP `implements` edges already bring traits to the regex-rg level (d459); LSP type-hierarchy edges matter only under `scip.mode = off` with `places = outline`, which no cycle 1 run measures | Ceiling | S | - |
| K12a | K12a-1 (d529) and K12a-2 (d533, opt-in `roots = "manifest"`) are done. Open: the default flip, gated on the lsp-cost rule (the claim is only the narrow pyright gain; gopls, csharp-ls and cost are unmeasured); the per-server live acceptance (only pyright and typescript-language-server ran against real servers); one session per sibling module (not built) | Live acceptance per server | S | N4 |
| K18a | Built (d485, PR 185): `seeds.repo_map`, opt-in under `[run_profile.seeds] repo_map` (default off). Open: the ceiling run (`maps bench --set traits --set where-to --max-depth 0 --include-seeds`, waves `k18a-rm`, `k18a-bm25-rm`, plus the depth-1 walk `k18a-walk` against the K24c control; report the reorder rate against bm25's top 10); park K18a-2 (the SCIP-free graph) when the reorder rate is under 30 % and where-to does not move | Ceiling | M | G2 |
| K19 | Built (d483): `grep.structural`, opt-in under `[run_profile.grep] structural` (default off). Open: the ceiling run (`maps bench --set traits --max-depth 1`, waves `k19-ctl`, `k19-on`, `k19-only`; A = the K24c control arms, B = A + the map, C = the map as the only implements source; no gain claim, d422 (11); `maps bench` turns the key on in the arm's derived overlay when `--maps` names the map). `ast-grep-core =0.40.1` is the last release on tree-sitter 0.25 and moves with K15a | Ceiling on structural queries | M | - |
| **M4-conditional** | **Codanna, grammar upgrade, optional tools, external gold, baselines: each runs in cycle 1 when its condition holds** | | | |
| K14b | Cargo feature `syntax-codanna`: parse one file through Codanna into LCI nodes | Ceiling on fixtures | M | blocked: owner and Codanna repo owner; K14a NO-GO as specified (d422) |
| K14a2 | Run the built Codanna 0.17.1 binary on the offline TypeScript ground truth (384 and 291 distinct pairs) and join its edges with it; pre-register the kill rule first (suggested: stop below 95% precision or below 85% recall on `this.m()` plus bare calls) | Precision and recall per call shape | S | held for the owner (d422) |
| K14c | In-memory resolution glue; edges with confidence by the rule that picked them | Precision and recall against LSP on fixtures | M | blocked(K14b) |
| K14d | Name-based invalidation: on a change, re-resolve the file's outgoing edges and edges whose target names it added or removed | K2a | S | blocked(K14b) |
| K16 | Codanna parser tests as fixtures; its `tsconfig`, `go.mod` and `pyproject` readers for server root discovery | Tests | S | N4 (part); blocked(K14b) |
| K15a | tree-sitter 0.26 and its grammar crates (javascript and python also to 0.25, K14a); every chunking test re-run; adapter version bump; default-build regression check. Waits for the Codanna decision or a measured grammar need; ast-grep-core needs tree-sitter ^0.25.4 (K19): `=0.40.1` is the last 0.25 release, so K15a re-pins it (0.40.5 or 0.44.1 for 0.26; 0.45 and later need 0.27) | Existing tests | M | - |
| K17 | `oxc_resolver` for TS and JS imports, only if K14a2 shows Codanna's TS resolution is weak | Imports against typescript-go | S | - |
| K12b | typescript-go adapter: K12b-1 is built on fakes on its branch and not merged (d535: section `[tsgo]`, provider `typescript-go`, `LCI_TSGO_PATH`; typescript and tsgo both enabled is a config error); parked pending the owner's cut (d563); no merge before the tsgo facts check and the lsp-cost pair against typescript-language-server (d537) | Live acceptance | M | - |
| K31 | Opengrep data-flow map, opt-in; consumes Opengrep's existing flow analysis, no engine written | Ceiling on a flow set | M | - |
| K5a | K5a-2 remains: the LSP-over-MCP baseline arm (the `workspace/symbol` arm) in `maps baseline`, after K11d-1's `McpSession` harness (K5a-1, the rg arm, is done, d443); no A/B runs; not blocked | Smoke query | S | item 6 arm |
| K5b | The Codanna baseline arm (no A/B runs) waits for the owner; the Serena arm is done (d527, PR 198) | Smoke query | S | - |

M0 filler, any time a build slot is free: K0a follow-ups review: a breaker-open embedder skip in traverse `search.vector` still counts one external call (`AppChunks::embedding_skipped` could return the models' `embedder_skipped()`), and the `EXTERNAL_CALLS` doc in src/app/traverse/mod.rs says no map makes any (d416). The stat-gated re-hash left this filler and landed as K38 (d440).

Cycle 1 finish line (d422): M2 + M3 + M4-core plus every unblocked M4-conditional slice. Implementation is complete (d563, d564); the rows above are what is left, and the owner's cut (see "Now and next") decides which stay in cycle 1 and which move to cycle 2.

Deferred to cycle 2: rust-analyzer in process (`ra_ap_*`), ty or Pyrefly (in process or as servers), ts-rust as a TS
server arm, rung 2 model training, a gix blame map, a tantivy engine for K21 (only after a measured gap), the K22a-3 follow-ups (d519: literal seeds restricted to definitions or an occurrence prior; a larger impact `seed_k`, which needs `--seed-k` forwarded by `maps bench`).

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
| N18 | Low batch, open part: (f) no per-file size cap. (a), (c), (d) and (e) are done (PR 195, d567); (b) was N77 (d190) | partly done: (f) stays open | XS | - |
| N20 | Cancellation and lock scope, open parts: (b) lock scope; (d) the scan threads in `src/app/indexing.rs` (`chunk::scan_incremental_with`, `fingerprint_with`) outlive a dropped request and run to their end, which needs cooperative cancellation in the scanner; (e) Ctrl+C graceful shutdown has no deadline; the other MCP tools (navigation, recipes, `watch`/`unwatch`, `index_status` and the rest) still run to their end after a cancel (roadmap@bbd3292 N20). Done: (a) (d551, d552), (c) and part of (d) (d412, d199), per-server request ordering (no queue needed, d475) | partly done | S-M | N16 done |
| N26 c | `search_symbols` has no result cap; record the size of a broad name's answer first (roadmap@bbd3292 N26) | ready | XS | - |
| N32 | Rewrite the model-facing strings within budgets: `search_code` 350 characters (436 today), `search_symbols` 280 (328), instructions 450 (732); state facts only (d278): what comes back, the first-call index cost and the navigation preconditions; no advice on when to prefer Grep, which goes to an on-demand skill measured as its own arm. Its former kill-rule thresholds move to the cycle 2 screening design (d396). Text is `cloud-ok`, the A/B arm is `local`. See [agent-payload-notes.md](agent-payload-notes.md) (roadmap@bbd3292 N32) | ready after N26 | S | N26 |
| N33 | Schema documentation for the four search and navigation tools: describe `query`; list the valid `languages` and `source_roles` values (roles: source, test, benchmark, example, generated, documentation, configuration); state glob syntax, that an empty `include_paths` matches nothing and that empty language or role lists are errors; document `include_declaration`; describe `workspace_path` on every tool (today only on `WorkspaceArgs`, 73 characters); stop hard-coding "(8)" in the `top_k` text; drop what N32 makes redundant (the language-server list repeated 4 times, about 400 characters). Acceptance: a test that every argument of the four tools has a description; `tools/list` no larger than today (roadmap@bbd3292 N33) | ready | S | N32 |
| N34 | `default_top_k` 5 versus 8 as a measured arm: 61 appearances, 57 at rank 5 or better, ranks 6 to 8 carry 18.7% of returned lines, 20 of 94 answered files were at rank 6 or later (U1 calls); lowest priority (roadmap@bbd3292 N34) | ready, local | S | N28 done |
| N42 | Incremental-index peak memory: `Store::cached_vectors` plus a vector clone scale with the whole index (estimate 0.9 to 1.5 GB at 150,000 to 250,000 chunks of 768 floats, not measured); measure first; fix direction: load only vectors whose chunk hash is still present and move instead of clone; acceptance: state the bound and a test fails above it; `cloud-ok` (roadmap@bbd3292 N42) | ready, low | S | - |
| N96 | Re-check the error-to-commit premise (item 5) with full clones and the error text of linked GitHub issues, under a new pre-registered rule (d377) | ready | M | - |
| N100 | Where-to routing: whether to route an un-steered `traverse` call on a where-to question to the `where-to` preset (8 answers instead of 16: set_f1 +0.015 [+0.005, +0.026] and 42% fewer answer tokens, the gain is the shorter list, d389), by a fact in the tool description under the facts-only rule; it needs its own pre-registered test. Detection (b) was killed at B0 (R 0.339 against the 0.70 bar, d391) | partly done: detection killed (d391); routing is the caller's `steer.policy` (K30a, d456); the description fact (a) is parked until a tier-3 run is approved | S | - |
| N101 | The 5 s readiness probe of `lsp.live` (`PROBE_TIMEOUT`, the server `--version` call in `ManagerBackend::open`) timed out under machine load and failed a walk before the server was asked anything (seen in K9a's runs); the default is now 15 s and `lsp.live` warm-starts shared servers at the first touch (d450). Open: re-measure the probe ms under lane load (`lsp-cost --arms shared`) | partly done (d450): the re-measure is open | XS | - |
| N102 | `[run_profile.lsp] sync` on Windows: `notify` 8.2 stops watching a directory without telling the handler when ReadDirectoryChangesW fails with anything but aborted or access denied (a 16 KiB buffer overflow during a large checkout gives ERROR_NOTIFY_ENUM_DIR), so the sync goes silent and never rescans. Detect it (a periodic stat check of the table, a bridge of notify's `log` errors into a rescan, or a larger buffer upstream) and re-open the watcher with a rescan | Unit test with an injected watcher failure; a burst of thousands of file writes on Windows reaches the server | S | K8a |
| N104 | Owed from the navigation and search-channel tree barrier (done, d495, PR 184): a release-build navigation p50 on the bevy tree against the 150 ms kill bound (not run) | partly done (d495): the measurement is owed | S | - |
| N114 | A fourth `RestartReason` for a session with no recorded stamp: after a NoFileList or StampFailed barrier, the next good barrier says the tree changed for a fresh session (lci-core lsp.rs tree_barrier). From the B2 review (d514) | ready, low | XS | N109 |
| N121 | If the detached start task of a session panics before it publishes an outcome, `failed()` stays false and the session is never idle-swept; a panic during priming leaves the ready client set with `primed` false. No normal server failure takes either path. Fix: publish `Err` when the task ends (a drop guard) and treat a priming panic as settled. From the K11b-4 Sol re-check | ready, low | XS | N117 |
| N124 | `open_split` partial-failure visibility: when one provider's probe fails and another passes, the walk shows the drop only per node ("no language server is available for python"). Add one note per failed provider (`lsp.live: <provider> unavailable: <error>`). Also from the K11b-5 Sol review: a broken pyright shim passes the start probe (`/ready` green), and an initialize inactivity timeout opens no breaker window. Deferred from K11b-5 commit 2 | ready, low | XS | K11b-5 |
| N125 | Under per-language `places = "auto"` (K11b-6, cycle 2), a direct symbol seed in an unindexed language gets no `lsp.live` expansion until its file is reached and outlined (the `lsp_live.rs` ask returns without `place_of`); file and chunk expansions do outline. Option: outline the seed's file first. From the K11b-6 Sol review; not the cause of the K11b-6 failure (its python seed was a chunk) | blocked(K11b-6, cycle 2), low | XS | K11b-6 |
| N126 | `ImpactMap` and `ImplementsMap` (`scip_ranked_hits`): the Partial note quotes the configured `cap` while `edges_of` applies `cap.min(EXPAND_LIMIT)` (256), so a configured cap above 256 makes the note state the wrong cap. Fix: quote the applied cap (an intended output change; update the per-map note pins). From the cleanup S5 review (d574) | ready, low | XS | - |
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
| N119 | Burst cost under the `sync = "watch"` default: single events skip `.gitignore`, so the writes of `npm install` or `cargo build` become batches sent to the servers; the drain stats every path under a std mutex on a runtime worker; the poll fallback rescans the tree every 2 s. Measure on a large tree before changing anything. From the K11b-4 review | measure first | S | K11b-4 |
| N120 | The search channel still restarts a synced non-watching server on a changed stamp (the K11b-2 exception keeps `watches_workspace` adapters only); the loss is seconds (csharp cold first call 9 s, go 1.8 s in the K11b-4 plan's estimate), not the 31 s of rust-analyzer in d510. Measured follow-up: extend the exception to synced servers (as fresh as the OS watcher, drained before every request). K11b-4 plan residual and Sol question 4 | ready after a measurement | S | K11b-4, N104 |

Short accounts. C1: carries the `impl` header into method chunks and gives later features a `kind`; accept with unit
tests per language and a before/after run on both sets. S1/S2/S4: a commit today overwrites the whole workspace table
and every search loads every chunk; accept with an equal-results test and a timing run on a large workspace. W1/W2: freshness
cost today is a read and SHA-256 of every supported file at most every 10 s; accept with a manifest compatibility test.

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
| N127 | Acceptance coordinator dedupe: table-drive the `assert_search` calls of `xtask/src/acceptance/multilingual.rs`, and share one runner for the per-language `*_lsp.rs` and `*_recovery.rs` steps; evidence names stay verbatim. Only a live acceptance run proves it (no automated gate). From the cleanup S1e audit (d574) | ready | M | a live acceptance run |

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
