Status: PLAN

# Roadmap

This file holds plans only: what is open, what is next and what is deferred. Results, history and the reasons
behind decisions live in [decisions.md](../decisions.md), where `roadmap@bbd3292 <ID>` names the text of an item in the
last version of the old roadmap that carried full result paragraphs (`git show bbd3292:docs/development/roadmap.md`).
Ids are never renumbered or reused.

## How to use this file

1. Take the top `ready` item of "Now and next", then the lowest tier in "Open items by tier" whose dependencies are done.
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
`needs-approval(what)` | `research` (produces a decision, not a feature) | `paused(direction)` (parked by the 2026-10-17
direction, see below). The table row is the only home of a status.

Size: XS = a few lines, S = under half a day, M = about a day, L = more than a day (split before picking).

Naming: a slice is the item id plus a lowercase letter (A6a, N3b), never a new id. The tier names P0, P1, P2 and P3 only
name priority tiers and are never slice names. A document's internal numbering (amendments, audit actions) is scoped to
that document and is not a roadmap id.

## Direction

- Main focus (owner, 2026-10-17, d313): multimap traversal and its iterations. The embedder and reranker premise gets no
  further benches or iterations; its merit and lessons stay in the log. LCI is an extension of grep: grep and the other
  agent tools are steps inside multi-step queries (d337).
- Core loop (d330): a Wikirace on a code graph, with a System 1 racer and compiler-grade links. Without SCIP and doc
  edges the graph is sparse, so traversal equalled search (d333); building those edges comes first.
- Order (d317, d319, d320): doc types first, then SCIP for the Rust repositories, then the walk-on-SCIP premise test.
  Cleanup order, sanity adjustments, metric tiers and the simulated agent are decisions d319 to d329. The cleanup run's
  own decisions are d300 to d312.

## Now and next

Ordered; the first item with all dependencies met is next. Every item gets its kill rule written before its slice runs.

| # | Item | Status | Notes |
| --- | --- | --- | --- |
| 1 | Doc-type prose adapters: `.md`, `.rst`, `.adoc`, `.txt` | partly done: `.txt`, `.rst`, `.adoc` (each its own slice behind the Markdown tests); DocSection nodes and a `doc.sections` map after (shaping count 54.9% > the 30% bar) | Real adapters, never wrapped as code-like files (d317, d325). Markdown landed as heading sections (d375); its before/after evaluation is in the commit. |
| 2 | Structured formats `.json`, `.yaml`, `.toml` | ready after 1 | Start on the grep fallback with size and generated-file filters (d325). Real adapters only if a measured gap appears. |
| 4 | Walk-on-SCIP premise test | blocked(N97) | Trait-impl set plus an R1 re-run with the walk on (d320). On the R1 and external-repository trees the walk was never measured with SCIP edges (d333); the frozen tree `bfaff44` already has a SCIP index and its walk benches are in `docs/spike/multimap/v2/engine-results.md`, so the test targets the new trees. Needs the implementation edges of N97 and likely a seeds.rs follow-up so a trait can seed a walk; the trait set stratifies explicit, derive and macro impls and takes its gold independently of the derivation (d378). Kill rule set before the run; the comparison is against plain `lci search` (R1 span hit .64) and against rg. |
| 5 | Error-to-commit set with SZZ gold | parked (d377) | "Which code changes in git history are responsible for this error" (d314). Gold: blame the fix commit's lines at its parent, then verify; first a premise check of usable fix commits. Baseline: `git bisect` where a reproducing test exists (d324). The premise check failed its kill rule (8 usable items, 40 needed); re-check is N96. |
| 6 | LSP-MCP baseline for the trait set | blocked(N97) | rust-analyzer implementation requests as the competitor (d324). Hypotheses for why LCI never measured trait impls: no SCIP in the trees, implementation edge not ingested, LSP channel sends no implementation request, derive/blanket/macro impls (d315). |
| 7 | Simulated agent | ready | Rule-based loop, no LLM (d329): first grep from the rarest identifier terms, read ~40-line windows, follow identifiers by tree-sitter, stop when the gold appears; scores tool calls and tokens; two or three policies; calibrate once against N3/N1 transcripts; only arm differences count. |
| 8 | Tier-2 calibration | blocked(7) | Confidence calibration and a small judged subset with a cheap model at milestones (d327). Tier 3, real agent A/B runs, stays rare and validates the proxies (pv2 rho .699, d261). |
| 9 | S1b: where-to MCP default | ready | The MCP default `top_k` of 8 returns about 18 answers on where-to and has not been measured (d244, d259). Measure it at matched answer budgets before changing it. |
| 10 | N14 re-evaluate | paused until search work resumes | Tag `keep/n14-lexical-suffix`, commit e0f7c10 (d115, d302). The before/after run needs the 4B pass. |

## Open items by tier

Tier order: P0 correctness and measurement, P1 ranking, storage, freshness and LSP, P2 agent ergonomics and protocol,
P3 umbrella research. Rows are open items only. Item text names symbols, never line numbers.

### P0 - verified defects, test quality, evaluation hygiene

| ID | Item | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| E5 | LSP-stable evaluations: prime before the first timed query; per-query `lsp_budget_hit`; `compare` excludes budget-hit queries. After N7 the budget-hit windows cover 14 to 22 of 35 mined queries (d054) (roadmap@bbd3292 E5) | ready | S | N7 done |
| E6 | Identifier-query stratum: `cargo xtask mine-symbols` samples declaration names (seeded, names of 5+ characters, defined in one file, not test-only) with `expected_paths` = the defining file, tag `identifier`, committed apart from `self.toml`; unit tests on synthetic input; one fusion-only and one reranked baseline recorded (roadmap@bbd3292 E6) | ready | S | E2 done |
| N8 | The LSP channel goes dark for 15 to 25 s windows under back-to-back searches (hypothesis: rust-analyzer swaps its crate graph after build scripts and proc macros load, invalidating the primed symbol index); diagnose and fix (roadmap@bbd3292 N8) | ready | S-M, live | N7 done |
| N16 rest | A workspace search on a legacy-only table waits on the store-wide legacy guard; the revision pin and the re-read after a commit during the freshness check are not pinned by a failing test (roadmap@bbd3292 N16) | ready | S | - |
| N18 | Low batch (item (b), the Java readiness probe, was fixed as N77, d190): (a) `manifest::save` removes the destination before `rename`; (c) `serve --port 0` advertises port 0; (d) three navigation tests assert rust-analyzer's flag; (e) the embedding model listing is probed at the origin; (f) no per-file size cap (roadmap@bbd3292 N18) | ready | XS each | - |
| N19 | Language-adapter batch: `java.args` is appended after `-jar` so JVM options never apply; relative `java.path`; C# project discovery follows symlinked directories (roadmap@bbd3292 N19) | ready | XS-S each | - |
| N20 | Cancellation and lock scope: (a) the MCP index and search handlers ignore the request's cancellation token; (b) lock scope; (c) `unwatch` race; (d) blocking lock inside `spawn_blocking`; (e) Ctrl+C graceful shutdown has no deadline (roadmap@bbd3292 N20) | ready | S-M | N16 done |
| N21 | Two open test-quality items (a third, a fixed 500 ms sleep in the data-dir lock test, was fixed as N83, d193): (a) `evaluation_reports_required_failures_optional_skips_and_json_metrics` checks only that `hit_at_1_rate`, `mrr` and `role_distribution` exist, not their values (check whether another test pins them); (b) its last assert checks an `EvaluationFile` the test built itself (tautological): exercise `load_definition` instead (roadmap@bbd3292 N21) | ready | XS | - |
| N24 | Java launcher jar ordering (`find_launcher_jar` takes the first directory entry that matches); Windows `.cmd` spawn. The third part, non-adjacent `search_symbols` duplicates, was fixed as N74 (d187) (roadmap@bbd3292 N24) | ready | XS | - |
| N25 | Six weak assertions in `tests/cases/indexing.rs`: (a) `a_hung_reranker_is_tried_once_and_keeps_earlier_warnings` sets a 1 s reranker timeout but bounds the elapsed time at 10 s; (b) the unreachable-reranker test asserts only `reranking_ms` < 5000 (count accepted connections instead); (c) the 200 ms budget tests use 150 to 1200 ms bounds; (d) a stop-word query is not in the fixture; (e) the traced-search test lacks a rank >= 1 assert; (f) the `mixed_languages` deletion test checks top-8 over equal vectors without a chunk count (roadmap@bbd3292 N25) | ready | XS | - |
| N26 c | `search_symbols` has no result cap; record the size of a broad name's answer first (roadmap@bbd3292 N26) | ready | XS | - |
| N32 | Rewrite the model-facing strings within budgets: `search_code` 350 characters (436 today), `search_symbols` 280 (328), instructions 450 (732); state facts only (d278): what comes back, the first-call index cost and the navigation preconditions; no advice on when to prefer Grep, which goes to an on-demand skill measured as its own arm. Kill rule: uptake rises by fewer than 6 of 27 runs, mean F1 falls by more than 0.05, or tokens rise by more than 5%. Text is `cloud-ok`, the A/B arm is `local`. See [agent-payload-notes.md](agent-payload-notes.md) (roadmap@bbd3292 N32) | ready after N26 | S | N26 |
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
| N95 | A Markdown language server (marksman, MIT) for prose navigation: links, headings, references; the language bar's LSP item is waived for prose until then (d375) | research | S | - |
| N96 | Re-check the error-to-commit premise (item 5) with full clones and the error text of linked GitHub issues, under a new pre-registered rule (d377) | ready | M | - |
| N97 | Trait-implementation edges from SCIP impl-header occurrences: `Implements` in graph.scip plus an opt-in `scip.implements` map; derives and macro impls out; plan and kill rule pre-registered (d378) | in progress | M | - |
| N14 | Lexical matches credited to the first chunk of a path-suffix match: tag only (see Now and next 10) (roadmap@bbd3292 N14) | paused | S | - |

Items marked `cloud-ok` in older notes may run in a cloud agent; items marked `local` need the owner's machine.

### P1 - ranking quality, storage, freshness, LSP correctness

Retrieval items tied to the embedder and reranker premise are `paused(direction)`; they stay listed because their
acceptance data is in the log.

| ID | Item | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| L0 | Engine-independent lexical fixes: scanner-aligned `rg` flags, timeout and stdout cap, newest bundled `rg`, interval index for LSP mapping, timing split (d074) (roadmap@bbd3292 L0) | paused(direction) | XS-S | - |
| L1 | Lexical engine spike: arms (a) `rg` subprocess, (b) in-process `grep-searcher` + `ignore`, (c) LanceDB FTS, (d) Tantivy. Adoption rule: +20% relative nDCG@10 on both strata with p50 not worse (d075); prefer (c) over (d) at equal quality; adopt (b) only if at least as fast and it removes the rg-not-found failure; a missing index degrades to `rg` with a warning (roadmap@bbd3292 L1) | research, paused | M | - |
| L3 | Anchor extraction for issue-like queries; optional agent-supplied `lexical_terms` (roadmap@bbd3292 L3) | paused(direction) | S | - |
| L4 | Ranked BM25 channel as a fourth RRF list (engine per L1) (roadmap@bbd3292 L4) | blocked(L1) | L, split | L1 |
| C1 | Per-chunk symbol metadata (name, kind, enclosing, signature) and Rust `impl` header carry (roadmap@bbd3292 C1) | ready | M | - |
| C2 | Contextual headers in embedding, lexical and reranker documents (one re-embed); gate: no keyword-stratum loss, core hit@1 at least 0.818 (d076) (roadmap@bbd3292 C2) | blocked(C1), paused | M | C1 |
| F2d | Definition boost in fusion (LSP symbol kinds now, C1 `kind` later) (roadmap@bbd3292 F2d) | proposed, paused | S | C1 |
| F3 | Query-type routing (identifier, prose, issue-like) (roadmap@bbd3292 F3) | blocked(L3, R2, E6); R2 is dropped (d357) | M | L3, R2, E6 |
| N1 | Per-provider symbol-query syntax (roadmap@bbd3292 N1) | ready | S | - |
| S1 | Per-file incremental table writes (delete + add or `merge_insert`), lancedb bump (roadmap@bbd3292 S1) | ready | M | - |
| S2 | `optimize()` and version cleanup, `gc` for orphaned workspaces and manifests (roadmap@bbd3292 S2) | ready | M | S1 preferred |
| S4 | Stop loading every chunk's code per search; interval maps for channel mapping (roadmap@bbd3292 S4) | ready | M | - |
| W1 | mtime+size stat cache before hashing (manifest additive fields) (roadmap@bbd3292 W1) | ready | M | - |
| W2 | `notify`-based watcher with polling fallback and persisted watch roots (roadmap@bbd3292 W2) | ready | M | W1 preferred |
| N2 | LSP document lifecycle: open once, `didChange` on edits, `didClose`, drop fixed sleeps (roadmap@bbd3292 N2) | ready | M | - |

Short accounts. C1: carries the `impl` header into method chunks and gives later features a `kind`; accept with unit
tests per language and a before/after run on both sets. S1/S2/S4: a commit today overwrites the whole workspace table
and every search loads every chunk; accept with an equal-results test and a timing run on a large workspace. W1/W2: freshness
cost today is a read and SHA-256 of every supported file at most every 10 s; accept with a manifest compatibility test.
N2: accept with a live LSP test that a second symbol request reuses the open document.

### P2 - agent ergonomics, navigation by name, protocol upgrade

| ID | Item | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| N4 | Project-root discovery and priming for TS/Python/Go servers (roadmap@bbd3292 N4) | ready | M | N2 |
| N5 | Symbol-name addressing for `find_definition` and `find_references` (roadmap@bbd3292 N5) | ready | M | N1 |
| N6 | Outline, hover, implementations, call hierarchy tools (two slices) (roadmap@bbd3292 N6) | blocked(C1) | M + M | C1, N2 |
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
| G1 | Bounded structural expansion of top hits via LSP references or call hierarchy, labelled with a reason; evidence: SpIDER arXiv 2512.16956 (+27% relative Recall@20), CodexGraph token cost arXiv 2408.03910 (roadmap@bbd3292 G1) | blocked(N6b, E2) | L | N6b (a slice of N6), E2 |
| G2 | `repo_map(query, max_tokens)` from tree-sitter tags and personalized PageRank (Aider's repo map; best at an 8K-token budget, arXiv 2607.24882) (roadmap@bbd3292 G2) | blocked(C1) | L | C1 |
| X2 | Cross-workspace vector reuse by content hash (worktrees, clones, branches), multi-repo query-time merge (Cursor content-hash cache, Semble) (roadmap@bbd3292 X2) | research | L | S1 |
| X3 | Dependency scope: lockfile to registry sources, read-only, version-keyed; optional SCIP ingestion (SCIP, Glean, Context7 pattern) (roadmap@bbd3292 X3) | research | L | X2 |
| X4 | Git-aware channels: `changes_since(ref)`, commit-message channel mapped to touched files (BM25 over commit messages up to +80% MAP, arXiv 2502.07067) (roadmap@bbd3292 X4) | research | M | E2 |
| M4 | Matryoshka dimension truncation (`embedding_dimensions`) and int8/binary storage with rescoring; a storage item, search is 60 ms here (roadmap@bbd3292 M4) | research, paused | M | - |
| A5 | Availability and safety extras: lexical results while the first index builds, secret redaction on returned text, agent-supplied `annotate` summaries keyed by chunk hash, optional bearer token on the loopback HTTP surface (roadmap@bbd3292 A5) | research | M each | A1 |
| P1 | Packaging: cargo-dist installers (shell, PowerShell, MSI, Homebrew) and `service install|uninstall|start|stop` via the `service-manager` crate (roadmap@bbd3292 P1) | research | M + M | - |

### Investigations

| ID | Question | Status |
| --- | --- | --- |
| I-1 | Is shelling out to `rg` worth keeping over in-process grep crates, LanceDB FTS or Tantivy? Design: L1's four arms on this repository and the mined set, lexical-only; metric: nDCG@10 and hit@3 per stratum, p50 and p95, index size and build time, new crates; decision: see L1 (roadmap@bbd3292 I-1) | research, paused |
| I-6 | Does evaluating mined queries at HEAD, not at the parent commit, bias results? Design: for 50 mined commits, index a `git worktree` at the parent into a disposable data dir and compare; decision: if the bias is above 0.05 absolute, extend E2 with an `--at-parent` mode (roadmap@bbd3292 I-6) | research |
| I-8 | How expensive is harvesting LSP references and call hierarchy at index time with rust-analyzer, jdtls and clangd? Design: time per symbol on this repository and a Java fixture; decision: above 50 ms per symbol, G1 stays query-time only (roadmap@bbd3292 I-8) | research |

## Deferred

- Adoption logging from real sessions (owner, 2026-10-17, d328): until LCI reaches release stage, tier-3 agent runs are
  the only adoption measure; occasional CLI use is qualitative only.
- Freshness (F1, F2), not before the spike works on live trees (d247): F1 verify on read, so that every answer checks
  the files it cites; F2 measure staleness after real edits. A freshness contract stays a release blocker.
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
