# Engineering roadmap and backlog

This file is the source of truth for ongoing LCI improvement work. It is written
for an implementer who has not read any prior conversation: every item is a
bounded slice with its own evidence, scope, acceptance criteria, and
dependencies. It survives context compactions; the research that produced it is
not committed, so the evidence each item needs is restated here with URLs.

## How to use this file

1. Pick the top `ready` item in the lowest-numbered priority tier whose
   dependencies are `done`. Do not skip tiers for convenience; P0 exists because
   later items cannot be judged without it.
2. One item = one slice = one commit (plus the regular `cargo xtask test --suite
   Full` gate). Split an item rather than letting a commit grow past it.
3. Before starting, load the matching project skill (`.agents/skills/lci-*`):
   ranking/latency work uses `lci-retrieval-evaluation`; LSP work uses
   `lci-lsp-adapter`; grammar/extension work uses `lci-language-adapter`.
4. Ranking, fusion, reranker, document-format, instruction, and candidate-count
   changes need a before/after evaluation (the retrieval contract requires it):
   run `evaluate ./evaluations/core.toml --workspace self=$PWD` (and the mined
   set from E2 once it exists) before and after, keep the JSON under
   `test-results/` (never committed), and put the headline numbers (hit@1,
   hit@3, MRR, nDCG@10 when available, p50 total and p50 reranking) in the commit
   message and in this file's item entry.
5. Update this file in the same commit as the work: move the item to the Done
   table with its commit hash, set a successor's status to `ready` if its
   dependency was the only blocker, and append newly discovered work as new IDs.
   Never renumber or reuse IDs.
6. Anything that downloads model weights needs the owner's explicit approval
   first (state model, source, size). Anything that would add Docker, a web UI,
   another database, another service, or contact port 8765 is out of bounds
   (AGENTS.md "Boundaries") and belongs in the parking lot, not the backlog.

Status legend: `done` (hash in Done table) | `in-progress` (uncommitted or
another agent) | `ready` (deps met, can start now) | `blocked(reason)` |
`needs-approval(what)` | `research` (produces a decision, not a feature) |
`dropped(reason)`.

Size: S = under half a day, M = about a day, L = more than a day (must be split
before it is picked).

## Snapshot: where things stand (verified 2026-10-10, branch `fix/verified-bugs`, HEAD `fa0f262`)

| Fact | Value |
| --- | --- |
| Pipeline | Tree-sitter chunks (11 languages, 12 extensions) -> Qwen3-Embedding-4B vectors in embedded LanceDB -> query fans out to LanceDB cosine (40) + `rg` subprocess (40, ranked by matching-line count) + LSP `workspace/symbol` (40, first-seen order) -> RRF k=60 + 0.004 production-source prior -> top 24 to Qwen3-Reranker-4B (fail-open) -> top_k (default 8, max 40) |
| Pins | `lancedb =0.38.0` (crates.io latest 0.40.0, 2026-10-07), `rmcp =3.2.0` (latest 3.5.1, 2026-10-05; 3.5.0 implements the 2026-07-28 stateless spec), `tree-sitter 0.25`, no `notify`, no `tantivy` |
| Measured on the owner's machine (AMD iGPU gfx1150, vLLM via WSL ROCm, both models bf16, max-model-len 8192) | search 45-65 s on this repo, of which reranking 35-54 s, cold rust-analyzer ~10 s, query embedding 0.5 s, LanceDB 60 ms, ripgrep 0.15 s |
| `evaluations/core.toml` (11 required self queries, 3 optional GUST) | hit@1 0.818-0.909, hit@3 1.0, MRR 0.91-0.95, 0 fallbacks |
| Chunk sizes in this repo | median ~200 B, p90 ~1.3 KB, p99 ~5 KB, max 17 KB (chunker targets 6,000 B groups, keeps declarations whole to 24,000 B) |
| Storage | every commit overwrites the whole workspace table (`CreateTableMode::Overwrite`, `src/store.rs`); every search loads every chunk row including code (`Store::chunks`); no `optimize`, `cleanup_old_versions`, scalar index, FTS index, or vector index |
| Freshness | `on-search` (default) re-reads and SHA-256-hashes every supported file at most every 10 s; `watch` polls every 2 s doing the same; `index_status` does it on every call; watch registrations die with the process |
| LSP | 7 persistent adapters behind `LspAdapter`; `didOpen` with `version: 1` re-sent before every position request, never `didChange`/`didClose`/`shutdown`; TS/Python/Go/clangd sleep up to 3 s after `didOpen`; the same `term#` symbol syntax (rust-analyzer's) is sent to every provider |
| Evaluation harness | hit@1/3/8, MRR, median/p95 total latency, fallback count, created/reused/waited counts; no nDCG, Recall@k, per-stage percentiles, paired comparison, strata, or `refreshed_incrementally` counter |

## Done

| Commit | What landed | Why it mattered |
| --- | --- | --- |
| `b86a0db` | Focused `cargo xtask test` suites run with `--workspace` and fail when their filter matches zero tests | `Chunk`/`Filter`/`Unit`/`Lsp` suites had been running 0 tests since the `lci-core` split |
| `845038e` | DNS-rebinding Host/Origin checks on REST, `/openapi.json`, `/health`, `/ready` (`loopback_guard` in `src/server.rs`) | Only `/mcp` had rmcp's allowlist; the REST mirror could be driven by a rebinding page |
| `38ad6a8` | Fail-open fixes: non-UTF-8 files decoded lossily (one such file broke every search); oversized chunks embedded from a halved prefix on HTTP 400/413/422 (one 106 KB literal made a workspace unindexable against max-model-len 8192); query embedding and reranker no longer retry timeouts (a hung reranker cost ~6 min per search); reranker failures append to earlier warnings; a failed LSP provider is skipped for the rest of a search | Robustness of the default path |
| `29b9f32` | Source-role classifier: Go `_test.go`, Java `*Test/*Tests/*TestCase/*IT`, C/C++ test names, Gradle/Android test source sets; `config/` dirs and JVM package paths no longer demote production code | Decoy evaluation with reranker off: hit@1 0.75 -> 1.0 |
| `43d56d9` | `rerank_document_max_bytes` (default 4096): reranker gets path + UTF-8-safe head of each chunk; results keep full chunks | One oversized chunk made the whole rerank 400; core.toml cap sweep (24 candidates): uncapped / 8192 / 4096 / 2048 -> rerank median 29.1 / 27.4 / 25.1 / 22.5 s, max 72.8 / 61.6 / 45.9 / 32.6 s, quality within noise |
| `3847b8c` | README `search_symbols` row and LSP timing wording fixed | Stale since LSP generalisation |
| `438670d` | Cross-process index lock `<data_dir>/index.lock` (`lock_data_dir_for_indexing`) | Verified: CLI + server could commit one workspace concurrently. Live: CLI waited ~6 min for the server's 1224-chunk commit, then proceeded |
| `fa0f262` | AGENTS.md/CLAUDE.md/skills rewritten for unattended runs; new `lci-autonomous-improvement` skill; AGENTS.md freshness drift fixed (closes H2's AGENTS part) | Owner's autonomous mandate |

## In progress

Nothing. Mark an item `in-progress (<date>)` here and in its table row when
a slice starts; remove it when the slice is committed.

Known remaining defects are scheduled as S5 (Windows long data_dir paths), N2
(LSP document lifecycle) and H1 (Rust `tests.rs` role).

---

## P0 - measure first, then cut latency, then fix verified defects

The reranker is the only stage whose cost is user-visible (35-54 s of a 45-65 s
search), and its benefit over a 4B retriever is unproven: Qwen3-Reranker-4B's
81.20 MTEB-Code was measured reranking the 0.6B embedder's top 100 (75.41),
while LCI's 4B embedder already scores 80.06 on that suite
(https://arxiv.org/abs/2506.05176). Nothing about ranking can be decided with 11
queries (each one moves hit@1 by 9 points), so the evaluation harness comes
first.

| ID | Title | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| R0 | Fusion-only baseline: does the reranker help at all on core.toml? | ready | S | - |
| E0 | Dump every reranked candidate with fused rank and reranker score, for offline replay | ready | S | - |
| E1 | nDCG@10, Recall@k, file Acc@k, per-stage p50/p95, `refreshed_incrementally` in the harness | ready | S | - |
| E2 | Git-mined query set generator with keyword / keyword-agnostic strata | ready | M | E1 |
| E3 | Paired comparison (`--baseline`) and channel/reranker arm toggles incl. a grep-only baseline | ready | M | E1 |
| R1 | Reranker depth x document-size sweep; set defaults from data | ready (better after E3) | S | - |
| R2 | Skip the reranker when an exact symbol-definition hit exists | ready | S | - |
| R3 | Reranker as a weighted RRF vote instead of a replacement | ready | S | E3 |
| R4 | Trial a smaller reranker (Qwen3-Reranker-0.6B or gte-reranker-modernbert-base) on port 8767 | needs-approval(download) | S | E3 |
| R5 | Reranker query instruction | ready | S | E3 |
| H1 | Classify Rust test files (`tests.rs`, `test_*.rs`, `*_test(s).rs`) as `test` | ready | S | - |
| S5 | Shorten LanceDB table names so long `data_dir` paths work on Windows | ready | S | - |

### R0 - Fusion-only baseline
`ready` - S, live, config-only - deps: none - skill: `lci-retrieval-evaluation`
- Why: `core.toml` has never been run without the reranker, yet reranking is ~85-95% of search latency on the owner's machine. Measured (2026-10-10, vLLM logs over 44 reranks): the reranker is compute-bound at ~286-290 prompt tokens/s; cost fits `rerank_s ~= 0.4 + computed_tokens / 290` within ~10%; a 24-document rerank computes ~8.5k tokens at the 4096-byte cap. CoREB found no off-the-shelf reranker improves text-to-code retrieval (best: Qwen3-Reranker-4B at -0.1% nDCG@10; https://arxiv.org/abs/2605.04615). If fusion order matches reranked quality here, the default could change (owner decision).
- Scope: no code. Run the current binary on `core.toml` twice, sequentially: reranker on (default) and reranker unreachable (`reranker_url = "http://127.0.0.1:9/rerank"`; ignore the ~9 s connection-retry latency in that arm, or subtract `reranking_ms`). Compare hit@1/3/8, MRR, first expected ranks per query. Repeat on `evaluations/mined/self.toml` once E2 lands.
- Accept: both reports under `test-results/`, a per-query table in this item; no default change without the owner if quality differs or the evidence is within noise.
- Risk: 11 queries cannot detect small differences; this is a go/no-go signal, not a final verdict.

### E0 - Candidate and score dump for offline replay
`ready` - S - deps: none - skill: `lci-retrieval-evaluation`
- Why: every reranker strategy question (12 vs 24 candidates, vote-into-RRF, confident skip, progressive early exit, score caching) can be answered offline at zero GPU cost if the evaluator records, per query, all reranker candidates with path, start line, channel ranks, fused score/rank, source role, document bytes sent, and reranker score. Today only the top-k results are kept.
- Scope: an opt-in diagnostic on the search report (e.g. `SearchReport.candidates` populated only when an evaluation flag/config asks for it) in `src/app/search.rs`, surfaced by `src/evaluate/runner.rs` into the JSON report; no ranking change; MCP/REST output unchanged by default. Add a small replay script or xtask subcommand later (separate item) that recomputes metrics for alternative orders from the dump.
- Accept: mock integration test that the dump contains every shortlisted candidate with both scores and is absent by default; one live `evaluate` run produces it.
- Risk: keep it out of default responses (token budget for agents).

### E1 - Richer evaluation metrics
`ready` - S - deps: none - skill: `lci-retrieval-evaluation`
- Why: the harness reports hit@k and MRR only. The literature LCI will compare against reports nDCG@10, Recall@k and file-level Acc@5/10 (LocBench https://aclanthology.org/2025.acl-long.426.pdf, CORE-Bench https://arxiv.org/abs/2606.11864, Long Code Arena https://huggingface.co/datasets/JetBrains-Research/lca-bug-localization). Aggregate latency is total-only, but the question is always "how much of it is reranking". The aggregate has no `refreshed_incrementally` counter, so created+reused+waited does not sum to executed.
- Scope: `src/evaluate/types.rs` (`QueryMetrics` gains `ndcg_at_10`, `recall_at_k` for multi-path gold, `file_acc_at_5/10` keyed on distinct files; `AggregateReport` gains means of those, `index_refreshed`, and per-stage p50/p95 for every `EvalTimings` field), `src/evaluate/metrics.rs`, `src/evaluate/tests.rs`, README "Retrieval evaluation" paragraph. Graded gain: an `expected_paths` match = 2, a `preferred_paths` match = 1.
- Accept: unit tests with hand-computed nDCG/Recall values and an all-miss case; `cargo xtask test --suite Evaluation` passes; one live `evaluate` run against `core.toml` emits the new fields (numbers in the commit message).
- Risk: none to ranking; evaluation only. Keep metric code pure (no App access).

### E2 - Git-mined query set
`ready` - M - deps: E1 - skill: `lci-retrieval-evaluation`
- Why: gold labels for repository-level retrieval come from merged changes: commit/PR text as the query, touched files as relevance (Gandhi et al. https://arxiv.org/abs/2502.07067; voyage-code-4's agentic suite https://blog.voyageai.com/2026/08/13/voyage-code-4/). Hundreds of labelled queries per repo at zero annotation cost. Benchmarks are "saturated with keyword references" (LogicLoc https://arxiv.org/abs/2604.16021), so a stratum whose queries do not name the changed files is needed to measure the semantic channel honestly.
- Scope: new `xtask/src/mine.rs` + `cargo xtask mine-queries --repo <path> --key <workspace-key> --out evaluations/mined/<key>.toml --max 300` using `git log --no-merges --name-status --format=...` through `std::process` (no new crates). Rules: skip merges/reverts, lockfiles, vendored paths, commits touching >10 or 0 supported-extension files, subjects under 4 words; `expected_paths` = touched files that still exist at HEAD (document that evaluation runs at HEAD, an approximation; see Investigations I-6 for the parent-commit variant). Add optional `tags: Vec<String>` to `EvalQuery` (`src/evaluate/types.rs`) and per-tag aggregates; tag each query `keyword` (subject contains a touched file stem or an identifier from the diff header) or `keyword-agnostic`. Commit a mined set for `self` (this repo).
- Accept: unit tests for the filters and tagging on synthetic log output; the committed `evaluations/mined/self.toml` validates with `evaluate` (expected paths exist); README documents the command and the HEAD approximation; `.lciignore` unaffected (evaluations are TOML, not indexed).
- Risk: evaluation must never feed ranking (skill rule); mined files are data, not tuning targets. Keep `core.toml` as the required hand-written set.

### E3 - Paired comparison and arm toggles
`ready` - M - deps: E1 - skill: `lci-retrieval-evaluation`
- Why: every P0/P1 ranking item needs "A vs B on the same queries" with a significance signal (ranx-style paired tests https://github.com/AmenRa/ranx; q-IDF paper used 10,000-resample paired bootstrap https://arxiv.org/abs/2605.18561). The single most important experiment LCI has never run is a grep-only baseline (Cursor switched semantic search off in July 2026 after a +12.5% result eight months earlier https://cursor.com/blog/semsearch, https://forum.cursor.com/t/what-do-you-think-about-cursor-removing-the-codebase-indexing-settings/165899; de4er00 found hybrid can lose to dense alone https://github.com/de4er00/code-rag-mcp).
- Scope: (a) `evaluate --baseline <report.json> --label <name>`: per-query deltas of reciprocal rank and nDCG@10, win/loss/tie counts, mean delta with a seeded 95% paired-bootstrap CI (`src/evaluate/compare.rs`, wired in `src/main.rs`). (b) `[channels]` config section in `lci-core/src/config.rs` with `semantic`, `lexical`, `lsp`, `reranker` booleans (default true), honoured in `src/app/search.rs` (a disabled channel contributes no candidates and no timing; disabled reranker reports `reranked=false` with no warning). Grep-only arm = `semantic=false, lsp=false, reranker=false`. Document in `config.example.toml` and README.
- Accept: unit tests for the comparison math; mock integration test that each toggle removes exactly that channel; live: `core.toml` run with reranker on vs off, numbers in the commit message.
- Risk: toggles are production config, not evaluation data, so they do not violate "evaluation never feeds ranking". Readiness must still probe the embedding service even when `semantic=false` (document).

### R1 - Reranker depth and document-size sweep
`ready` (use E3 for the comparison once available) - S, live - deps: none - skill: `lci-retrieval-evaluation`
- Why: reranking is 35-54 s per search here. Already measured at 24 candidates (commit `43d56d9`): caps uncapped/8192/4096/2048 -> rerank median 29.1/27.4/25.1/22.5 s, quality within noise; 4096 chosen. The cost model from R0's evidence predicts 12 candidates at a 2048-byte cap would take ~8-12 s (estimate, not measured). Whether cost scales with candidates x bytes or is dominated by model load decides every later reranker item (Investigation I-2). Qwen's reranker gains were measured on top-100 shortlists; LCI uses 24.
- Scope: config-only runs of `evaluate` over `core.toml` (and `evaluations/mined/self.toml` if E2 landed) for `rerank_candidate_count` in {12, 24, 48} x `rerank_document_max_bytes` in {1024, 2048, 4096}, plus one reranker-off arm. Record p50/p95 `reranking_ms`, hit@1/3, MRR. Change the defaults in `lci-core/src/config.rs`, `config.example.toml`, README only if a cell dominates.
- Accept: a results table in the commit message and in this item; decision rule: pick the cheapest cell whose hit@3 and MRR are within the E3 CI (or, before E3, within 0.03 absolute) of the best cell.
- Risk: a 48-candidate cell may exceed vLLM's batch limits; treat 4xx as a data point, not a bug.

### R2 - Skip reranking on an exact symbol-definition hit
`ready` - S - deps: none - skill: `lci-retrieval-evaluation`
- Why: most real code-search queries are identifiers (78.2% at Debian Code Search https://michael.stapelberg.ch/posts/2019-09-29-dcs-positional-turbopfor-index/); short keyword queries make dense models collapse and rerankers inconsistent (CoREB https://arxiv.org/abs/2605.04615). When the LSP channel already returned the symbol the user typed, 35-54 s of reranking buys nothing.
- Scope: `src/app/search.rs`: after fusion, if `lexical::terms(query)` has exactly one term, the raw query matches `^[A-Za-z_][A-Za-z0-9_:.#]*$`, and an LSP location whose `name` equals that term (case-insensitive, `#` stripped) maps to a fused hit at rank <= 3, return fusion order with `reranked=false` and a new `rerank_skipped: Option<String>` field (`"exact_symbol_match"`). Config `rerank_skip_on_exact_symbol: bool` default true. REST/MCP get the field for free via serde.
- Accept: mock test with the fake LSP returning a symbol named like the query -> `rerank_skipped` set, zero rerank requests; prose query -> reranked as before. Live: `core.toml` unchanged metrics; a `symbols`-style query shows `reranking_ms` ~0.
- Risk: none to fail-open; the gate only removes work.

### R3 - Reranker as a weighted vote in RRF
`ready` - S - deps: E3 - skill: `lci-retrieval-evaluation`
- Why: two peers fuse the cross-encoder's order back into RRF rather than letting it replace the ranking; de4er00 measured that replacement raised MRR but lowered file recall@5 (https://github.com/de4er00/code-rag-mcp, https://github.com/rutmir/code-search-mcp). LCI exposes every channel rank already, so this is a small change with an honest A/B.
- Scope: `rerank_mode = "replace" | "vote"` and `rerank_vote_weight` (default 1.0) in config; in `rerank_into_report` (`src/app/search.rs`) `vote` adds `weight / (rrf_k + rerank_rank)` to `fusion_score` and re-sorts; `reranker_score` stays visible. Default remains `replace` until E3 says otherwise.
- Accept: unit test on a hand-built hit list; live E3 comparison replace vs vote on `core.toml` + mined set; flip the default only on a non-regressing win.
- Risk: keep `fusion_score` and `reranker_score` distinct (retrieval contract).

### R4 - Trial Qwen3-Reranker-0.6B
`needs-approval(download Qwen/Qwen3-Reranker-0.6B, Apache-2.0, ~0.6B params, ~1.2 GB bf16, https://huggingface.co/Qwen/Qwen3-Reranker-0.6B)` - S - deps: E3
- Why: a third-party L4 measurement puts the 0.6B at 65 ms p50 vs 580 ms for the 4B (https://superlinked.com/blog/qwen3-embedding-reranker-guide); on an iGPU the ratio should be similar. MTEB-Code drops 81.20 -> 73.42 (https://arxiv.org/abs/2506.05176), but that was over a weak retriever; over LCI's 4B embedder the loss may be small and the latency gain decisive.
- Scope: no code. Owner approves and serves the model on 8767 under a distinct `reranker_model` name; run E3 comparison (4B vs 0.6B vs off) on `core.toml` + mined set; document the recommended profile in README/`config.example.toml`.
- Accept: comparison table in the commit; decision rule as R1 (cheapest within CI).
- Risk: model weights never enter the repo (validation matrix).
- Caution (explorer, 2026-10-10): in Qwen's own table the 0.6B reranker (73.42 MTEB-Code) scores *below* the 0.6B first-stage retriever it reranks (75.41), so it may hurt rather than help. `Alibaba-NLP/gte-reranker-modernbert-base` (149M, Apache-2.0; needs approval: download, ~0.3 GB) is the only small reranker found whose code number (CoIR 79.99) is not below its first stage (79.31) -- one model-card table, protocol unspecified, vLLM ROCm support unverified. bge-reranker-v2-m3 (41.38) and jina-reranker-v2 (58.98, CC-BY-NC) are far below the retriever. A cascade (small model over 24, 4B over the top 8) is the alternative to an outright replacement. Run R0 first: if fusion alone matches, no reranker swap is needed.

### R5 - Reranker query instruction
`ready` - S - deps: E3 - skill: `lci-retrieval-evaluation`
- Why: the Qwen3-Reranker card says a task instruction gives 1-5% (https://huggingface.co/Qwen/Qwen3-Reranker-4B). LCI sends the raw query.
- Scope: `reranker_query_prefix: String` (default empty) in config, prepended to `query` in `Models::rerank` (`lci-core/src/models.rs`). First verify how the deployed `/rerank` endpoint applies Qwen3's `<Instruct>/<Query>/<Document>` template (vLLM may already wrap it); record the finding. Candidate text: "Given a code search query, judge whether the code passage answers it".
- Accept: unit test that the prefix is sent; live E3 comparison with/without; default only changes on a win.
- Risk: none; query-side only, no re-embedding.

### H1 - Rust test-file classification
`ready` - S - deps: none - skill: `lci-retrieval-evaluation` (filters)
- Why: `lci-core/src/filter/source_role.rs` has test-name rules for Python, JS/TS, Go, Java, C/C++, C# but none for Rust, so `lci-core/src/filter/tests.rs` and `src/evaluate/tests.rs` are `source` and receive the production prior. Acceptance item 12 (production beats decoys) is undermined for Rust.
- Scope: `is_test_file_name`: `.rs` stems `tests`, `test`, `test_*`, `*_test`, `*_tests`; tests in `lci-core/src/filter/tests.rs`; README role paragraph. Note in the item that inline `#[cfg(test)] mod tests` blocks cannot be path-classified (chunk-level role via C1's symbol metadata is the follow-up).
- Accept: `cargo xtask test --suite Filter`; live `evaluate core.toml` shows the test-role query still passes and no self query regresses.
- Risk: `test.rs` as a production module name is rare; document the convention.

### S5 - Windows long-path failure for long `data_dir`
`ready` - S - deps: none
- Why: verified defect: a `data_dir` longer than ~100 characters fails on Windows with Lance "os error 3" (ERROR_PATH_NOT_FOUND), consistent with MAX_PATH 260: table dir `ws_<64 hex>.lance` (72 chars) plus Lance's internal `_versions/...manifest`, `data/<uuid>.lance`, `_transactions/...` paths.
- Scope: `lci-core/src/workspace.rs` `table_name()` -> `ws_` + first 20 hex chars of the id; `Store::snapshot` verifies the schema metadata `workspace_path` equals the resolved path (collision guard, error otherwise); migration in `Store`: if the old long-name table exists and the short one does not, read from the old one until the next commit writes the short one, then drop the old. Readiness (`src/app/readiness.rs`): warn when `data_dir` length > 160 on Windows. README "Persistent data" paragraph.
- Accept: unit test for name length; integration test (tests/cases/indexing.rs) that an index created under the old name is searchable and migrated on refresh; manual repro with a 200-character temp `data_dir` on Windows before/after (note the result in the commit).
- Risk: changing identity would orphan indexes; this changes the table name only, not `Workspace::id`.

---

## P1 - ranking quality, storage, freshness, LSP correctness

Order inside P1: lexical spike and cheap lexical wins first (they feed fusion
and the reranker gate), then chunk context, then fusion tuning, then storage and
freshness (independent of ranking, can interleave), then LSP lifecycle.

| ID | Title | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| L1 | Lexical engine spike: `rg` subprocess vs in-process grep crates vs LanceDB FTS vs Tantivy | research | M | E2, E3 |
| L2 | IDF weighting and identifier splitting in the existing `rg` channel | ready | S | E3 |
| L3 | Anchor extraction for issue-like queries; optional agent-supplied `lexical_terms` | ready | S | E3 |
| L4 | Ranked BM25 channel as a fourth RRF list (engine per L1) | blocked(L1 decision) | L (split) | L1, L2 |
| C1 | Per-chunk symbol metadata (name, kind, enclosing, signature) and Rust `impl` header carry | ready | M | - |
| C2 | Contextual headers in embedding, lexical and reranker documents (one re-embed) | blocked(C1) | M | C1, E3 |
| F1 | Re-justify the 0.004 production prior with a sweep | ready | S | E3 |
| F2 | Weighted RRF per channel, definition boost, per-file cap | ready | M | E3 |
| F3 | Query-type routing (identifier / prose / issue-like) | blocked(F2, L3, R2) | M | F2, L3, R2 |
| M1 | Configurable, task-specific query instructions | ready | S | E3 |
| N1 | Per-provider symbol-query syntax | ready | S | - |
| S1 | Per-file incremental table writes (delete + add or `merge_insert`), lancedb bump | ready | M | - |
| S2 | `optimize()` / version cleanup and `gc` for orphaned workspaces and manifests | ready | M | S1 preferred |
| S4 | Stop loading every chunk's code per search; interval maps for channel mapping | ready | M | - |
| W1 | mtime+size stat cache before hashing (manifest additive fields) | ready | M | - |
| W2 | `notify`-based watcher with polling fallback and persisted watch roots | ready | M | W1 preferred |
| N2 | LSP document lifecycle: open once, `didChange` on edits, `didClose`, drop fixed sleeps | ready | M | - |
| N3 | LSP `shutdown`/`exit`, idle eviction, registry-derived status | ready | S | - |

### L1 - Lexical engine spike
`research` - M - deps: E2, E3 - skill: `lci-retrieval-evaluation`
- Why: LCI ranks lexical hits by raw matching-line count with no IDF, so common tokens rank like rare identifiers (`src/app/search.rs::map_lexical_matches`). Peers use a ranked BM25 index: Tantivy (ck https://github.com/BeaconBay/ck, codesearch https://github.com/flupkede/codesearch, rutmir), SQLite FTS5, or LanceDB's own Lance-native BM25 index with tokenizer controls (https://docs.lancedb.com/indexing/fts-index; Tantivy support is being removed, https://github.com/lancedb/lancedb/issues/2998). Identifier-aware tokenization "largely eliminated" BM25's gap on CodeSearchNet-Go and a tuned IDF added +89% (https://arxiv.org/abs/2605.18561). ripgrep's own `grep-searcher`/`grep-regex`/`ignore` crates can run in-process (https://crates.io/crates/grep-searcher), removing `rg.exe` discovery and a process spawn per query. Nobody has measured these against each other on code; pick by data, not fashion.
- Design: four arms over this repo's chunks and the E2 `self` query set, lexical channel only (E3 toggles): (a) current `rg` count ranking; (b) in-process `grep-searcher` + `ignore` with the same ranking (measures spawn cost and Windows-discovery removal only); (c) LanceDB FTS on `code` (+ a pre-split identifier column) with `stem=false`, `remove_stop_words=false`, `lower_case=true`, `max_token_length` >= 100, `with_position=false`; (d) Tantivy with a custom camelCase/snake_case tokenizer. Metrics: lexical-only nDCG@10 / hit@3 / MRR per stratum, p50/p95 query latency, index build time and on-disk size, number of new crates. Arms (b) and (d) live in `xtask` behind an optional cargo feature `lexical-bench` with optional deps; arm (c) is an `#[ignore]`d integration test in `tests/` (LanceDB is already a dependency). Nothing ships in the product from this item.
- Decision rule: adopt an indexed BM25 (c or d) only if lexical-only nDCG@10 improves >= 20% relative over (a) on both strata with p50 <= (a)'s; prefer (c) over (d) at equal quality (no new dependency); adopt (b) only if it is at least as fast as (a) and removes the "rg not found" failure mode; otherwise keep `rg` and rely on L2.
- Accept: a results table and the decision written into L4's entry (and into Investigations I-1); spike code either deleted or kept behind the feature flag with a comment.

### L2 - IDF weighting and identifier splitting in the `rg` channel
`ready` - S - deps: E3 - skill: `lci-retrieval-evaluation`
- Why: cheap, engine-independent improvement (Windsurf weights terms by rarity https://devin.ai/blog/remote-indexing-multirepo-announcement; splitting identifiers cuts code vocabulary 7-10x https://arxiv.org/pdf/1904.01873). `lexical::terms` keeps <= 12 lowercase whole tokens and never splits `parseConfig` into `parse`/`config`.
- Scope: `lci-core/src/lexical.rs`: `terms()` emits sub-tokens of camelCase/snake_case identifiers plus the original (original first, cap stays 12); `src/app/search.rs::map_lexical_matches`: per-term document frequency = files with >= 1 match (from `rg --json` paths), `idf = ln((N - df + 0.5)/(df + 0.5) + 1)` with N = indexed file count, chunk score = sum over matched terms of `idf * (1 + ln(matches_in_chunk))`; new `lexical_score: Option<f32>` on `Hit` (`lexical_match_count` kept). To know which term matched which line, run one `rg` per term group or parse `submatches` from the JSON event (preferred, already in the output).
- Accept: unit tests for splitting and scoring; `cargo xtask test --suite Indexing`; live E3 lexical-only comparison on `core.toml` + mined set; accept on non-regression of hit@3/MRR and improved lexical-only nDCG.
- Risk: keep `rg`'s `--fixed-strings`; sub-tokens shorter than 3 chars are dropped (existing rule).

### L3 - Anchor extraction for issue-like queries
`ready` - S - deps: E3 - skill: `lci-retrieval-evaluation`
- Why: "Reformulate, Retrieve, Localize" improved first-file ranking 35% by pulling identifiers/snippets out of bug reports before lexical retrieval (https://arxiv.org/abs/2512.07022); LLM rewriting hurts because it drops anchors (CORE-Bench https://arxiv.org/abs/2606.11864); "dense expands, sparse anchors" (https://arxiv.org/abs/2608.15851). LCI must not call an LLM, so the extractor is deterministic. zvec-grep lets the agent pass its own `fts` keyword list (https://github.com/zvec-ai/zvec-grep).
- Scope: `lci-core/src/lexical.rs` (or new `lci-core/src/query.rs`): `anchors(query)` for queries with >= 25 words or stack-trace/newline markers: paths (`[\w./-]+\.\w{1,5}(:\d+)?`), `a::b`/`a.b` chains, camel/snake identifiers, quoted strings, error codes (`E\d{4}`, `0x...`); anchors go to `rg` and LSP symbol queries (caps 12 / 8), full text to the embedder. New optional `lexical_terms: Option<Vec<String>>` on `search_code` (MCP `SearchArgs` in `src/server.rs`, REST, CLI `--lexical-term`) overriding extraction; reported in `filters`/report as `lexical_terms_effective`.
- Accept: unit tests on a synthetic issue text and a stack trace; mock search test that supplied terms reach the fake `rg` invocation (or the term list); live E3 on an issue-like subset of the mined set (PR-body-sized subjects).
- Risk: respect the 16,000-byte query cap; no network.

### L4 - Ranked BM25 channel
`blocked(L1 decision)` - L, split into L4a (index build at commit + FTS query API) and L4b (fuse as fourth RRF list, docs, eval) - deps: L1, L2
- Why: README has promised "BM25 ... can join the same fusion boundary" since the first commit; Sourcegraph's BM25F (5x boost on filename/symbol matches, demote tests/generated) gave ~20% (https://sourcegraph.com/blog/keeping-it-boring-and-relevant-with-bm25f); Anthropic's contextual BM25 cut retrieval failures a further 14 points over embeddings alone (https://www.anthropic.com/news/contextual-retrieval).
- Scope (if LanceDB FTS wins L1): `src/store.rs` creates the FTS index on `code` and on an identifier-split `tokens` column at commit (and `optimize()` refreshes it, see S2); `Store::lexical_search(query_terms, limit, filters)`; `src/app/search.rs` adds `bm25_rank` to `Hit` and a fourth RRF list; `rg` stays for exact/regex-like terms (LanceDB FTS has no boolean operators). If Tantivy wins: a `<data_dir>/tantivy/<workspace>` index kept in sync by the same commit path. Fields boosted: path, symbol (from C1), header (from C2).
- Accept: deterministic tests that BM25 candidates fuse and respect filters; multilingual acceptance; live E3 before/after; README "Module boundaries" sentence about BM25 updated.
- Risk: a second index must be invalidated by the same commit that rewrites rows (S1), never lag it.

### C1 - Per-chunk symbol metadata and Rust `impl` header carry
`ready` - M - deps: none - skill: `lci-language-adapter`
- Why: chunks carry path, lines, code, hash only. Every later quality item (headers C2, definition boost F2, outline N6, name addressing N5, chunk-level test role) needs the chunk's symbol name, kind, enclosing scope and signature. A known quirk: a Rust `impl A { fn second() {} }` yields an `impl A` chunk and a `{ fn second() {} }` chunk that no longer names the type (test in `lci-core/src/chunk.rs`); other families already attach the parent header to the first child (`attaches_header_to_child` in `lci-core/src/language.rs`). ColGREP/needle index name, signature and path alongside code (https://github.com/lightonai/next-plaid, https://github.com/alexandremourin/needle).
- Scope: `Chunk` gains `symbol`, `kind`, `enclosing`, `signature: Option<String>` (serde defaults so manifests load); `lci-core/src/chunk.rs` fills them from the declaration's name node per family and `signature` = the first line of the declaration up to its body; Rust `impl_item`/`trait_item` join `attaches_header_to_child`; bump every adapter's `cache_version` (e.g. `-v2`) so the incremental scanner reparses once; `src/store.rs` writes the new columns (schema metadata `schema_version=2`; `Store::snapshot` treats a missing column as "needs full rewrite", which the next commit performs). Expose the fields on `Hit`. Optional follow-up noted, not in scope: chunk-level `test` role for `#[cfg(test)] mod tests` blocks.
- Accept: chunk unit tests per family (name/kind/enclosing/signature asserted for one declaration each, Rust method chunk now carries `impl A`); `cargo xtask test --suite Chunk`, `Indexing`; multilingual acceptance; README chunking paragraph and `.agents/skills/lci-language-adapter/references/chunking-contract.md` updated.
- Risk: `code` stays the exact source slice (lexical mapping relies on it); only metadata is added. Vector reuse is unaffected (content hash of `code` unchanged) except for the Rust chunks whose text changes.

### C2 - Contextual headers in the embedded, lexical and reranker documents
`blocked(C1)` - M - deps: C1, E3 - skill: `lci-retrieval-evaluation`
- Why: prepending context to chunks cut top-20 retrieval failures 35% for embeddings and 49% with contextual BM25 (https://www.anthropic.com/news/contextual-retrieval); metadata-as-text prefixes consistently beat plain text (https://arxiv.org/abs/2601.11863); role-aware representations beat path-only ones by up to 40% Hit@5 (https://arxiv.org/abs/2607.11046). LCI embeds raw code; the reranker sees `path\n` + code. A deterministic header needs no model and lets the embedder and reranker see the production/test distinction the 0.004 prior currently enforces.
- Scope: `document_text(chunk, role)` in `lci-core/src/chunk.rs`: `"// {path} | {language} | {kind} {enclosing}::{symbol} | {signature} | role={role}\n{code}"`, bounded (~200 B header); used by `src/app/indexing.rs` for embedding, by `rerank_into_report` (header + `head_within(code)`), and by L4's BM25 column; rename `DOCUMENT_FORMAT_VERSION` to `"chunks-with-header-v2"` (one full re-embed per workspace, which the identity change already triggers). README step 4/9 wording ("embedded as source") updated.
- Accept: unit test of the header format; mock test that embedded texts start with the header while `Hit.code` is unchanged; live E3 before/after on `core.toml` + mined set (both strata); `cargo xtask acceptance multilingual` (acceptance item 12) still passes.
- Risk: full re-embed on the owner's GPU takes minutes per workspace; say so in the commit. Qwen's "no instruction for documents" guidance does not preclude a metadata header, but verify by the ablation, not by assumption.

### F1 - Re-justify the 0.004 production prior
`ready` - S, live - deps: E3 - skill: `lci-retrieval-evaluation`
- Why: with k=60 a rank-1 hit is worth 1/61 = 0.0164, so 0.004 equals about 20 rank positions in one channel; it mostly decides which 24 candidates reach the reranker. de4er00's source prior cut "how do I" recall@5 from 0.75 to 0.50 (https://github.com/de4er00/code-rag-mcp). It has never been swept.
- Scope: expose `source_role_prior: f32` (default 0.004) in config and `src/store.rs::source_role_prior`; sweep {0, 0.001, 0.002, 0.004, 0.008} with reranker on and off via E3 on `core.toml` + mined set, per stratum.
- Accept: table in this entry and the commit; change the default only on a non-regressing win; README "small transparent prior" sentence updated with the measured rank-equivalent.
- Risk: F3 may later make the prior query-kind dependent; keep the scalar config.

### F2 - Weighted RRF, definition boost, per-file cap
`ready` - M - deps: E3 - skill: `lci-retrieval-evaluation`
- Why: RRF is sensitive to its parameters and a tuned weighted combination beats flat RRF (Bruch et al., TOIS 2023 https://arxiv.org/abs/2210.11934); Semble's post-fusion signals lifted BM25 from 0.675 to 0.834 nDCG@10, mostly from definition boosts, adaptive lexical weight and per-file coherence (https://github.com/MinishLab/semble/blob/HEAD/benchmarks/README.md); de4er00 caps two chunks per file.
- Scope: `[fusion]` config: `semantic_weight`, `lexical_weight`, `lsp_weight` (default 1.0), `definition_boost` (default 0; applies when the chunk's C1 `kind` is a declaration or an LSP symbol kind in {Class, Struct, Interface, Enum, Function, Method, Trait}), `max_chunks_per_file` (default 0 = unlimited; applied when building the rerank shortlist, refilling from the next candidates). `fuse_hits` in `src/app/search.rs`; weights kept visible in `fusion_score` only (contract: distinct scores).
- Accept: unit tests on hand-built channel lists (search.rs now has a `tests` module); live E3 sweep of weights {0.5, 1, 2} per channel and cap {0, 2} on the mined set; defaults change only on a win.
- Risk: do not tune on `core.toml` paths specifically (skill rule); use the mined set.

### F3 - Query-type routing
`blocked(F2, L3, R2)` - M - deps: F2, L3, R2 - skill: `lci-retrieval-evaluation`
- Why: BM25 is best for code-to-code/identifier queries and dense for NL (JetBrains https://arxiv.org/abs/2510.20609); keyword queries collapse dense models (CoREB); Semble raises lexical weight for symbol-like queries. Fixed per-channel depths "may not transfer across changing queries" (https://arxiv.org/abs/2609.15143).
- Scope: `lci-core/src/query.rs`: `QueryKind { Identifier, Prose, IssueLike }` by rules (single identifier-like token; >= 25 words or trace markers; else prose); per-kind presets in `[fusion.<kind>]` overriding F2 weights, candidate depth (`semantic/lexical/lsp_candidate_count`), R2 gate, and M1 instruction; `query_kind` reported in `SearchReport`.
- Accept: unit tests for the classifier; live E3 per stratum (identifier vs prose vs issue-like) before/after; README.
- Risk: presets must default to current behaviour so the change is opt-in until measured.

### M1 - Configurable, task-specific query instructions
`ready` - S - deps: E3 - skill: `lci-retrieval-evaluation`
- Why: the Qwen3-Embedding card reports a 1-5% retrieval loss without a query instruction and recommends task-specific English instructions (https://huggingface.co/Qwen/Qwen3-Embedding-4B). LCI has one hard-coded `QUERY_INSTRUCTION` (`lci-core/src/models.rs`).
- Scope: `query_instruction: String` (default = current text) in config; optional per-kind overrides consumed by F3 later; documents never get an instruction (unchanged). README step 6.
- Accept: unit test of the formatted query; live E3 with two candidate instructions ("Given a bug report or feature request, retrieve the code that must be changed" for issue-like; "Given an identifier or API name, retrieve its definition and usages" for identifiers) on the mined strata.
- Risk: query-side only; no identity change, no re-embed.

### N1 - Per-provider symbol-query syntax
`ready` - S - deps: none - skill: `lci-lsp-adapter`
- Why: `src/app/search.rs` builds `term#` queries (rust-analyzer's "all symbols" suffix) and lowercases terms, then sends the same strings to csharp-ls, typescript-language-server, pyright, gopls, jdtls and clangd, which do not know the suffix and may be case-sensitive.
- Scope: `LspAdapter::symbol_query(&self, term: &str, original_case: Option<&str>) -> String` in `lci-core/src/lsp/adapter.rs` (default: the term as typed; rust-analyzer appends `#`); `search.rs` keeps original-case tokens from the query for this purpose.
- Accept: adapter unit tests; `cargo xtask test --suite Lsp`; real `cargo xtask acceptance <lang>-lsp` for at least Rust and one other server; live `search_code` shows non-Rust LSP hits where before there were none (note in commit).
- Risk: none; channel stays fail-open.

### S1 - Per-file incremental writes
`ready` - M - deps: none (bump lancedb first)
- Why: every refresh rewrites every row and vector (`Store::replace`), and old Lance versions accumulate with no cleanup (README says so). LanceDB supports `delete` and `merge_insert` with `when_matched_update_all` / `when_not_matched_insert_all` / `when_not_matched_by_source_delete`, recommends a scalar index on the join column, and warns that per-row adds are an anti-pattern (https://docs.lancedb.com/tables/update, https://docs.lancedb.com/performance). Blackbird ingests only new blobs (https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/). Own rows by path, cache by content (LlamaIndex bug where equal-content docs hid deletions: https://github.com/run-llama/llama_index/issues/23002).
- Scope: step 0: bump `lancedb` to 0.40.0 (verify whether the `remote` feature workaround from README is still required; full suite). Then `Store::apply(workspace, removed_paths, changed_files_chunks, vectors)`: `delete("relative_file_path IN (...)")` in batches + one `add` for changed files; fall back to full overwrite when there is no table, identity or schema version changed, or > 50% of files changed; create a BTREE scalar index on `relative_file_path` when rows exceed ~10k. `src/app/indexing.rs` passes `scan.parsed_files`/removed set.
- Accept: integration tests: one-file update changes only that file's rows (assert other rows' hashes and the row count), deletion reconciliation, restart reuse, failed-update preservation all still pass; `cargo xtask acceptance multilingual`; `table.version()` increments by <= 2 per refresh.
- Risk: a half-applied delete+add must not be observable: keep the per-workspace write lock and the data-dir lock around both operations; if `add` fails after `delete`, fall back to a full overwrite from the in-memory snapshot (which already exists).

### S2 - Version cleanup and `gc`
`ready` (S1 preferred first) - M - deps: none hard
- Why: README: "This milestone has no automatic index cleanup command"; moving a repo orphans its table and manifest forever. LanceDB `optimize()` compacts, prunes versions older than `cleanup_older_than` (7-day default) and refreshes indexes incrementally (https://docs.lancedb.com/tables/versioning, https://docs.lancedb.com/indexing/reindexing). Semble has `clear orphans`, SemTools `workspace prune`.
- Scope: after each commit call `optimize` with `cleanup_older_than` = `index.retention_minutes` (default 60; configurable); CLI `gc [--orphans] [--dry-run] [--yes]` and REST `POST /v1/gc` (no new MCP tool unless the owner asks): list tables whose schema-metadata `workspace_path` no longer exists on disk and manifests with no table; delete only with `--yes`. Check the exact Rust `OptimizeAction` signature on docs.rs for the pinned version.
- Accept: integration test that 20 refreshes do not grow the version count unboundedly; orphan detection test with a renamed temp workspace; README "Persistent data" and CLI sections.
- Risk: `destructive`; never delete a table whose workspace exists; dry-run is the default.

### S4 - Stop loading every chunk's code per search
`ready` - M - deps: none
- Why: `Store::chunks` loads every row including `code` on every search, and `map_lexical_matches`/`map_lsp_locations` do a linear `find` per match (O(matches x chunks)). Fine at this repo's size; the first mid-size workspace will notice.
- Scope: `Store::chunk_locations` (path, language, lines, hash, role; no code) + `Store::codes_for(hashes)`; per-file sorted interval index for channel mapping (binary search); load `code` only for the fused shortlist and final results; `Hit` construction unchanged for callers.
- Accept: existing suites pass; a timing note in the commit (fusion_ms and total before/after on this repo); the lexical/LSP mapping tests in `tests/cases/indexing.rs` unchanged.
- Risk: results must still carry the full chunk (reranker caps are separate).

### W1 - Stat cache before hashing
`ready` - M - deps: none
- Why: `chunk::fingerprint` and `scan_incremental` read and SHA-256 every supported file on each `on-search` check (<= every 10 s), every 2 s watch poll and every `index_status` call. Git skips rehashing when cached `lstat` data matches and guards same-timestamp ("racy") edits (https://git-scm.com/docs/racy-git); Semble updates by mtime.
- Scope: `manifest::CachedFile` gains `mtime_ns: Option<i128>`, `size: Option<u64>` (serde default, manifest version unchanged); `Manifest` gains `saved_at_ns`; `chunk::sources`/`fingerprint` take the prior files and skip read+hash when (mtime, size) match and `mtime + 2 s < saved_at`; otherwise read and hash. `index_status`, the watch poll and `refresh_stale_snapshot` pass the manifest through.
- Accept: deterministic tests: unchanged file not re-read (count reads via a test hook or by asserting a file whose mtime was set back but content changed with equal size is still detected when mtime >= saved_at); fingerprint equality before/after for an unchanged tree; timing of `fingerprint` on this repo in the commit.
- Risk: correctness backstop stays the hash; the fast path only skips reading.

### W2 - Native file watching with polling fallback; persisted watch roots
`ready` (W1 preferred first) - M - deps: none hard
- Why: 2-second full-tree polling; registrations lost on restart (README). `notify` 8.2.0 (stable; 9.0.0-rc.5 is a release candidate, 2026-08-30) is the Rust standard (rust-analyzer, Zed, Deno) with a polling fallback; `notify-debouncer-full` stitches renames (https://github.com/notify-rs/notify, https://docs.rs/notify-debouncer-full). Codegraph uses a native watcher with a 2,000 ms debounce (https://github.com/colbymchenry/codegraph).
- Scope: `watch_workspace` uses `RecommendedWatcher` + debouncer with the existing `watch_debounce_milliseconds`; events are hints that trigger the existing fingerprint-and-index path; on watcher error fall back to the current poll loop (keep `watch_poll_milliseconds`); persist watch roots in `<data_dir>/watches.json`, restore at `serve` start when `index.restore_watches = true` (default true) after a reconcile scan. Adds two crates to the root crate (not `lci-core`).
- Accept: `tests/cases/watching.rs` passes with the native watcher and with forced polling; round-trip test for `watches.json`; README watching paragraph.
- Risk: boundary-neutral (a crate, not a service). Watchers drop events under overflow; the hash reconcile remains authoritative.

### N2 - LSP document lifecycle
`ready` - M - deps: none - skill: `lci-lsp-adapter`
- Why: `before_position_request` in the TypeScript, Python, Go, Java and clangd adapters sends `textDocument/didOpen` with `version: 1` before every request and never `didChange`/`didClose`, which violates LSP's open-once rule; servers may answer from stale text after an edit. No visible effect seen with gopls; untested with tsserver/pyright. Each of TS/Python/Go/clangd also sleeps up to 3 s per request.
- Scope: per-session `open_documents: HashMap<Uri, (version, content_hash)>` in `lci-core/src/lsp.rs` `GenericClient`; `ensure_open(file)`: `didOpen` if absent, `didChange` (full sync, version+1) if the content hash changed, nothing otherwise; `didClose` LRU when > 32 documents are open; the 3 s head start applies only to a first `didOpen` in a session. Adapters keep a `language_id_for` hook; the shared logic moves out of the adapters.
- Accept: fake LSP (`tests/support/fake_lsp.rs`) records notifications: exactly one `didOpen` per file per session, a `didChange` after an edit, `didClose` after eviction; `cargo xtask test --suite Lsp`; real `cargo xtask acceptance typescript-lsp`, `python-lsp`, `go-lsp`, `java-lsp`, `c-lsp`, `cpp-lsp` pass (record versions).
- Risk: the retry-once-with-fresh-process rule must clear the open set.

### N3 - LSP shutdown, idle eviction, registry-derived status
`ready` - S - deps: none - skill: `lci-lsp-adapter`
- Why: processes are only `kill_on_drop`; no `shutdown`/`exit` handshake; servers live as long as the service (many workspaces = many servers). `Status` in `src/app/indexing.rs` hard-codes seven `*_analyzer_running` booleans. codesearch evicts idle watchers/servers (https://github.com/flupkede/codesearch); Claude Code's LSP config has `shutdownTimeout` (https://code.claude.com/docs/en/plugins-reference).
- Scope: `shutdown` request + `exit` notification bounded to 2 s on session drop and on `serve` Ctrl+C; `lsp_idle_minutes` (default 30, 0 = never) eviction task; `Status.analyzers_running: BTreeMap<String, bool>` derived from `Manager::adapters()` (keep the old fields one release for REST/MCP consumers, mark deprecated in README).
- Accept: fake LSP sees `shutdown` then `exit`; eviction test with a 1-second idle setting; `cargo xtask acceptance <lang>-recovery` still passes.
- Risk: none to retrieval.

---

## P2 - agent ergonomics, navigation by name, protocol upgrade, model hygiene

| ID | Title | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| N4 | Project-root discovery and priming for TS/Python/Go servers | ready | M | N2 |
| N5 | Symbol-name addressing for `find_definition`/`find_references` | ready | M | N1 |
| N6 | Outline, hover, implementations, call hierarchy tools (two slices) | blocked(C1) | M + M | C1, N2 |
| A1 | Compact results, `read_chunk`, `token_estimate`, `max_tokens` | ready | M | - |
| A2 | MCP `outputSchema`, tool annotations, deterministic tool order, instructions | ready (verify rmcp support) | S | A1 preferred |
| A3 | Upgrade rmcp 3.2.0 -> 3.5.1 (2026-07-28 stateless spec, back-compat) | ready | M | - |
| A4 | `index_workspace` as an MCP Task with progress | blocked(A3) | M | A3 |
| M2 | Model hygiene: swapped-model canary, reranker sanity probe, reference serving doc | ready | M | - |
| M3 | Low-resource embedding profile with Qwen3-Embedding-0.6B | needs-approval(download) | S | E3 |
| C3 | Split declarations above ~8 KB with header carry | blocked(C1) | M | C1, E3 |
| X1 | Extensions `.h`, `.cc/.cxx/.hh/.hxx`, `.mts/.cts`, `.mjs/.cjs` through the acceptance bar | ready | S each | - |
| H2 | Documentation sync: README docs index links this file, stale rows, module-boundary sentence | ready | S | - |

### N4 - Project-root discovery and priming
`ready` - M - deps: N2 - skill: `lci-lsp-adapter`
- Why: README documents that `search_symbols` for TypeScript/Python sees only files already opened because LCI never writes `tsconfig.json`/`pyrightconfig.json`. OpenCode resolves each server's `NearestRoot` (pyproject/pyrightconfig/setup.cfg; `go.work` before `go.mod`; the project's own `tsserver.js`) and starts servers per root (https://github.com/anomalyco/opencode/blob/HEAD/packages/opencode/src/lsp/server.ts). Serena's 30 GB pyright blow-up came from servers ignoring its ignore lists (https://github.com/oraios/serena/issues/944).
- Scope: `LspAdapter::project_root(workspace, file) -> PathBuf` per adapter (TS: nearest `tsconfig.json`/`jsconfig.json`/`package.json`; Python: `pyproject.toml`/`pyrightconfig.json`/`setup.cfg`/`setup.py`; Go: `go.work` then `go.mod`); sessions keyed by (provider, root); on spawn, `didOpen` one representative indexed file per root (nearest to the root) so `workspace/symbol` has project context; pass LCI's ignore set as `exclude` in pyright's `didChangeConfiguration` and tsserver's `initializationOptions` where supported. Never write into the repo.
- Accept: unit tests for root discovery; extend `cargo xtask acceptance typescript-lsp` and `python-lsp` to assert `search_symbols` finds a symbol with no prior `find_definition`; README limitation paragraph rewritten.
- Risk: more processes per workspace in monorepos; cap roots per provider (e.g. 8) and document.

### N5 - Symbol-name addressing
`ready` - M - deps: N1 - skill: `lci-lsp-adapter`
- Why: LCI's navigation takes a 1-based line and a 0-based UTF-16 character; agents miscount columns. Tools that address by name report fewer errors: Serena's name paths (https://github.com/oraios/serena/blob/main/src/serena/tools/symbol_tools.py), cclsp's `symbol_name` + `symbol_kind` with a candidate list when ambiguous (https://github.com/ktnyt/cclsp), Sourcegraph's `symbol` parameter (https://sourcegraph.com/docs/api/mcp).
- Scope: `PositionArgs`/`ReferencesArgs` (`src/server.rs`, REST, CLI) accept either `line`+`character` or `symbol` (+ optional `kind`, `relative_file_path`); resolution via `workspace/symbol` on the applicable adapter(s), exact-name filter (then C1 chunk metadata as a heuristic fallback, labelled); one match -> run the position request at its selection range; several -> return `ambiguous: true` with candidates (name, kind, path, line, character), not an error. Also accept a `character_base: 1` option for OpenCode-convention callers.
- Accept: fake-LSP tests for unique, ambiguous and missing names; real acceptance for Rust (`openapi_document`) via `cargo xtask acceptance lsp` extended; README tool table.
- Risk: never guess among candidates.

### N6 - Outline, hover, implementations, call hierarchy
`blocked(C1)` - two slices: N6a outline + hover (M), N6b implementations + call hierarchy (M) - deps: C1, N2 - skill: `lci-lsp-adapter`
- Why: harness vendors ship these for free (Claude Code `LSP` tool, OpenCode `lsp` with nine operations https://github.com/anomalyco/opencode/blob/HEAD/packages/opencode/src/tool/lsp.ts), so LCI's navigation must offer at least the same set to stay worth configuring; an outline saves ~80% of a file read (jCodeMunch https://github.com/jgravelle/jcodemunch-mcp); Cline's tree-sitter outline "frequently fails" across languages (https://github.com/cline/cline/issues/704), so LCI's always-available tree-sitter outline must be tested per language.
- Scope: N6a `document_symbols(workspace_path, relative_file_path, depth)` from C1 chunk metadata (always) upgraded with LSP `textDocument/documentSymbol` when available, each entry labelled `provenance: lsp | tree-sitter`; `hover(symbol | position)`. N6b `find_implementations`, `call_hierarchy(symbol | position, direction, depth <= 2, cap 50)` via `prepareCallHierarchy` -> `incomingCalls`/`outgoingCalls`. REST mirror, CLI, README, OpenAPI for each.
- Accept: fake-LSP protocol tests; real acceptance for at least rust-analyzer and one optional server per slice (record versions); unsupported-extension rejection before process start.
- Risk: tool-count growth; keep descriptions short and point to A2's instructions.

### A1 - Compact results, `read_chunk`, token budgets
`ready` - M - deps: none
- Why: search is 56.6% of agent tokens in real tasks (Relace https://relace.ai/blog/fast-agentic-search); agents favour recall over precision (ContextBench https://arxiv.org/abs/2602.05892); peers converge on compact-first results with a fetch tool and budgets: codesearch `compact=true` + `get_chunk` (https://github.com/flupkede/codesearch), rutmir 600-char previews + `code_read_chunk` (https://github.com/rutmir/code-search-mcp), GitNexus `maxTokens` at 4 bytes/token (https://github.com/abhigyanpatwari/GitNexus), ck `snippet_length`/pagination (https://github.com/BeaconBay/ck).
- Scope: `search_code` gains `compact: bool` (default false for compatibility; instructions recommend true) returning path, lines, symbol/kind (C1 if present), role, scores and a <= 400-byte preview; `max_tokens: Option<usize>` (estimate 4 bytes/token) truncating the list with `truncated: true`; `token_estimate` on every hit. New tool `read_chunk(workspace_path, relative_file_path, start_line, end_line)` reading from disk inside the canonical workspace (reject traversal/symlink escape), capped at 20,000 chars. REST + CLI mirrors, OpenAPI, README.
- Accept: `tests/cases/mcp.rs` and REST tests for compact vs full and for `read_chunk` boundary rejection; size comparison (bytes of a typical compact vs full response) in the commit.
- Risk: results are repository data, not instructions (already stated in MCP instructions).

### A2 - Output schemas, annotations, deterministic order, instructions
`ready` - S - deps: none (A1 preferred first)
- Why: typed results and read-only hints help clients and prompt caches; the 2026-07-28 spec asks for deterministic `tools/list` order (https://modelcontextprotocol.io/specification/latest/changelog). codesearch ships MCP `instructions` describing the intended flow; lsmcp prescribes Overview -> Search -> Details (https://github.com/mizchi/lsmcp).
- Scope: derive `schemars::JsonSchema` on `SearchReport`, `Hit`, `NavigationReport`, `Status`, etc. and return them as rmcp structured output so `outputSchema` is published; annotations `readOnlyHint=true` on search/status/navigation, `destructiveHint=true` on `gc` (if S2 added an MCP tool); verify the router emits a stable order; rewrite `with_instructions` in `src/server.rs` to the recommended flow (`search_code compact -> read_chunk -> find_definition by symbol`). First verify the pinned rmcp's macro support for annotations/output schema; if missing, do this after A3.
- Accept: `tests/cases/mcp.rs` asserts `outputSchema` and annotations in `tools/list`; OpenAPI response schemas can now be typed (`src/rest.rs`) instead of "open object"; README MCP table.
- Risk: schema derivation on `Workspace` (PathBuf) and `BTreeMap` fields; keep `#[serde(flatten)]` on `Hit.chunk` compatible.

### A3 - rmcp upgrade to 3.5.1
`ready` - M - deps: none
- Why: `rmcp` 3.5.1 is current (crates.io, 2026-10-05); 3.5.0 (2026-09-28) targets the 2026-07-28 spec, which removes protocol sessions and `initialize`, adds `server/discover`, Tasks as an extension, `subscriptions/listen`, and fixes explicit-default-port matching in the Origin allowlist (https://github.com/modelcontextprotocol/rust-sdk/releases). LCI's 3,600 s keep-alive exists only because long `index_workspace` calls were torn down as "Session not found" (commit `3f52fa4`); statelessness removes the problem class.
- Scope: `Cargo.toml` pin; `src/server.rs` compiles against 3.5.1 keeping session-based compatibility for current clients (`LATEST_WITH_INITIALIZE`); confirm `LocalSessionManager` keep-alive still applies to legacy sessions; re-check `allowed_origins` behaviour and the `loopback_guard` tests; `cargo xtask test --suite Mcp`; connect a real client (Claude Code `/mcp`) and run `search_code`.
- Accept: suites pass; live client connection recorded in the commit; README "MCP transport" sentence updated.
- Risk: API churn in `StreamableHttpService`; keep the change mechanical, no feature work.

### A4 - Indexing as an MCP Task with progress
`blocked(A3)` - M - deps: A3
- Why: cold indexing takes 2-10+ minutes; the spec moved long-running work to the `io.modelcontextprotocol/tasks` extension with `tasks/get` polling and `notifications/progress` on the request stream (https://modelcontextprotocol.io/specification/latest/changelog); rmcp has a server-side `TaskManager` (https://github.com/modelcontextprotocol/rust-sdk). rutmir streams progress (embedding -> retrieving -> reranking); DeepContext ran indexing in detached processes to dodge MCP timeouts.
- Scope: `index_workspace` returns a task handle when the client advertises the tasks capability, else stays synchronous; progress = files hashed, chunks embedded / total (already logged by `index_locked`); `src/app/indexing.rs` gains a progress callback; REST stays synchronous.
- Accept: `tests/cases/mcp.rs` with an rmcp client that polls `tasks/get` to completion; the synchronous path unchanged for legacy clients; README.
- Risk: keep the per-workspace single-flight semantics (concurrent first searches share one job).

### M2 - Model hygiene
`ready` - M - deps: none
- Why: README: replacing a model behind an unchanged name/URL is undetectable. ck and codesearch refuse to mix vectors from different models (https://github.com/BeaconBay/ck, https://github.com/flupkede/codesearch). Community GGUF conversions of Qwen3-Reranker return near-zero scores (https://gist.github.com/VooDisss/42bce4eb5c76d3c325633886c5e348ee), so a broken reranker can silently degrade ranking; `llama-server` serves both embeddings and `/v1/rerank` as a single binary, Ollama has no rerank endpoint, vLLM accepts Cohere/Jina rerank shapes (https://docs.vllm.ai/en/latest/serving/online_serving/).
- Scope: (a) canary: at commit time embed a fixed constant string and store its vector in schema metadata; at `App::open`/first search per process embed it again and compare cosine >= 0.98, else `Status.compatible=false` with reason "embedding model changed behind the same name" and a search warning. (b) optional readiness component `reranker_sane`: rerank a fixed (query, relevant, irrelevant) triple and flag degenerate scores. (c) README section "Reference serving backends" (vLLM, llama-server; Ollama embeddings only) and the accepted `/rerank` request/response shape.
- Accept: mock tests for canary mismatch and degenerate reranker; readiness test; live `/ready` output in the commit.
- Risk: the canary costs one embedding call per process start (cache it); never auto-reindex on mismatch, only report.

### M3 - Low-resource embedding profile
`needs-approval(download Qwen/Qwen3-Embedding-0.6B, Apache-2.0, ~0.6B params, ~1.2 GB bf16, https://huggingface.co/Qwen/Qwen3-Embedding-0.6B)` - S - deps: E3
- Why: 4B models assume a GPU; MTEB-Code 75.41 (0.6B) vs 80.06 (4B) (https://arxiv.org/abs/2506.05176); on CORE-Bench the 4B and 8B are close, so 0.6B is the only meaningful cheaper tier among Qwen's. A documented CPU/low-VRAM profile widens who can run LCI.
- Scope: `config.low-resource.example.toml` (0.6B embedder + R4's 0.6B reranker, separate `data_dir` because vectors are not comparable); E3 comparison vs the 4B profile on `core.toml` + mined set; README profile section. No code unless M4 (dimension truncation) is wanted at the same time.
- Accept: comparison table; documented profile.
- Risk: weights stay outside the repo.

### C3 - Split very large declarations
`blocked(C1)` - M - deps: C1, E3 - skill: `lci-language-adapter`
- Why: `MAX_DECLARATION_BYTES = 24000` keeps declarations whole (~6-8K tokens, near the reranker's 8,192-token reference limit) and dilutes one vector over a huge `impl`; cAST splits large AST nodes and merges small siblings to a budget (https://arxiv.org/html/2506.15655v1); function-only chunking is never Pareto-optimal, while cAST-style and sliding windows tie (https://arxiv.org/abs/2605.04763). This repo's max chunk is 17 KB (p99 5 KB), so few chunks change.
- Scope: `lci-core/src/chunk.rs`: recurse into declarations above 8,000 B, each sub-chunk carrying the parent's header (C1 `enclosing`/`signature`); bump all adapter cache versions; chunk tests; E3 before/after.
- Accept: `cargo xtask test --suite Chunk`; multilingual acceptance; live E3 non-regression; README chunking numbers.
- Risk: one re-embed of the affected chunks only (content hashes change).

### X1 - Additional source extensions
`ready` - S per slice - deps: none - skill: `lci-language-adapter` (full 14-item acceptance bar per slice)
- Why: `language.rs` explicitly rejects `.h`, `.cc`, `.cxx`, `.hh`, `.hxx`, `.mts`, `.cts`, `.mjs`, `.cjs`; C headers in particular are half of most C projects. Each maps onto an existing grammar and language server (clangd, typescript-language-server), so the cost is the acceptance bar, not a new adapter.
- Scope: one slice per family: `.h` -> `c` (document the choice; clangd handles C++ headers too), `.cc/.cxx/.hh/.hxx` -> `cpp`, `.mts/.cts` -> `typescript`, `.mjs/.cjs` -> `javascript`. Registry entries in `lci-core/src/language.rs`, role rules, README table, `rg --glob` list (derived from the registry already), multilingual fixture coverage, `*-lsp` acceptance assertions.
- Accept: `.agents/skills/lci-language-adapter/references/acceptance.md` items 1-14 per slice, including real-server navigation on a file with the new extension.
- Risk: `.h` ambiguity; make the mapping a documented decision, not a heuristic.

### H2 - Documentation sync
`ready` - S - deps: none (do after the in-progress diff lands)
- Why: README's Documentation list does not mention this roadmap; the "There is no BM25 ..." module-boundary sentence and the `search_symbols` row need to track landed items; AGENTS.md freshness wording is being fixed by another agent (verify, do not duplicate).
- Scope: README Documentation list (link `docs/development/roadmap.md`), any stale rows after the uncommitted README diff lands, `docs/architecture.md` one-line pointer; no behaviour change.
- Accept: `cargo xtask test --suite Full` (fmt only matters); a grep for "Rust/C#" and "Rust LSP" in README returns nothing stale.

---

## P3 - structure, scope, packaging, research (split before promotion)

These are umbrella items. Each must be split into S/M slices and given its own
acceptance criteria before it is picked, and each needs the enlarged evaluation
(E1-E3) to show it pays for itself.

| ID | Title | Status | Size | Deps |
| --- | --- | --- | --- | --- |
| G1 | Bounded structural expansion of top hits via LSP references/call hierarchy, labelled with a structural reason | blocked(N6b, E2) | L | N6b, E2 |
| G2 | `repo_map(query, max_tokens)` from tree-sitter tags + personalized PageRank | blocked(C1) | L | C1 |
| X2 | Cross-workspace vector reuse by content hash (worktrees, clones, branches) and multi-repo query-time merge | research | L | S1 |
| X3 | Dependency scope (lockfile -> registry sources, read-only, version-keyed) and optional SCIP ingestion | research | L | X2 |
| X4 | Git-aware channels: `changes_since(ref)`, commit-message channel mapped to touched files | research | M | E2 |
| M4 | Matryoshka dimension truncation (`embedding_dimensions`) and int8/binary storage with rescoring | research | M | E3 |
| M5 | Research bets: Qwen3-Embedding-8B trial (needs-approval: download, ~16 GB bf16), LoRA fine-tune on the user's own commit pairs, late chunking | needs-approval / research | L | E2 |
| A5 | Availability and safety extras: lexical results while the first index builds, secret redaction on returned text, agent-supplied `annotate` summaries keyed by chunk hash, optional bearer token on the loopback HTTP surface | research | M each | A1 |
| P1 | Packaging: cargo-dist installers (shell, PowerShell, MSI, Homebrew) and `service install|uninstall|start|stop` via the `service-manager` crate | research | M + M | - |

Evidence for the tier (restated so the file stays self-contained):
- G1: call-edge expansion from dense seeds gave >= 27% relative Recall@20 (SpIDER https://arxiv.org/abs/2512.16956); CoRet added >= 15 points (https://aclanthology.org/2025.acl-short.62/); LARGER +11.8-13.9 file Acc@5 (https://arxiv.org/abs/2605.16352); unbounded structure costs tokens (CodexGraph 22.16k vs 1.47k per task https://arxiv.org/html/2408.03910; Codebase-Memory traded 9 points of answer quality for 10x fewer tokens https://arxiv.org/abs/2603.27277). Label edges `lsp` vs `heuristic` (https://github.com/CodeGraphContext/CodeGraphContext/issues/1090). Store edges as a LanceDB table, not a graph DB.
- G2: Aider's repo map (personalized PageRank over def/ref tags, binary-searched to a token budget https://github.com/Aider-AI/aider/blob/HEAD/aider/repomap.py); RepoMap was best at an 8K-token budget on Agent Retrieval Bench (https://arxiv.org/abs/2607.24882).
- X2: grepai seeds worktree indexes from the main worktree (https://github.com/yoanbernabeu/grepai/blob/HEAD/docs/src/content/docs/git-worktrees.md); Cursor's content-hash embedding cache cut p99 time-to-first-query from 4.03 h to 21 s (https://cursor.com/blog/secure-codebase-indexing); Semble merges per-repo indexes at query time (https://github.com/MinishLab/semble).
- X3: docs-mcp-server's version-specific package indexes and Context7's resolve-id-then-query pattern (https://github.com/arabold/docs-mcp-server, https://github.com/upstash/context7); SCIP has indexers for every LCI language and carries Definition/Test/Generated roles (https://github.com/sourcegraph/scip); Glean blends a precomputed index with live clangd (https://engineering.fb.com/2024/12/19/developer-tools/glean-open-source-code-indexing/).
- X4: BM25 over commit messages mapped to touched files gave up to +80% MAP (https://arxiv.org/abs/2502.07067); ChunkHound's diff-aware search (https://github.com/chunkhound/chunkhound/blob/HEAD/chunkhound/mcp_server/tools.py).
- M4: Qwen3-Embedding supports MRL output from 32 to 2560 dims (https://huggingface.co/Qwen/Qwen3-Embedding-4B); voyage-code-3 reports binary 1024-dim nearly lossless with rescoring (https://blog.voyageai.com/2024/12/04/voyage-code-3/); Copilot's new embedder made its index ~8x smaller (https://github.blog/news-insights/product-news/copilot-new-embedding-model-vs-code/). LanceDB search is 60 ms here, so this is storage, not latency.
- M5: 8B adds +0.62 MTEB-Code and is not better on CORE-Bench/Agent Retrieval Bench; SFT on PR data raised the 4B's issue-to-edit nDCG@10 from 18.3 to 30.3 (https://arxiv.org/abs/2606.11864); late chunking (https://arxiv.org/pdf/2409.04701) has no code evidence yet.
- A5: SeaGOAT serves regex hits during indexing (https://github.com/kantord/SeaGOAT); de4er00 redacts secrets before storage; zvec-grep uses Bearer auth on its local Streamable HTTP MCP; RepoAgent/CodeWiki show per-object summaries keyed to code changes (https://github.com/OpenBMB/RepoAgent, https://github.com/FSoft-AI4Code/CodeWiki) - LCI may only store summaries an agent already wrote, never generate them (port 8765 is off-limits).
- P1: cargo-dist builds shell/PowerShell/MSI/Homebrew/npm installers, not winget (https://axodotdev.github.io/cargo-dist/book/installers/index.html); `service-manager` covers launchd/systemd/sc.exe/WinSW (https://crates.io/crates/service-manager).

---

## Investigations (open questions, with how to answer them)

| ID | Question | Design | Metric | Decision rule |
| --- | --- | --- | --- | --- |
| I-1 | Is shelling out to `rg` worth keeping over in-process grep crates, LanceDB FTS, or Tantivy? | L1's four arms on this repo + E2 set, lexical-only | lexical-only nDCG@10/hit@3 per stratum, p50/p95, index size/build time, new crates | see L1 |
| I-2 | What makes reranking cost 35-54 s here: candidate count, document bytes, or model compute on the iGPU? | Time `POST /rerank` directly (curl) with 1/6/12/24 docs x 512/2048/4096 bytes, warm | wall time per call | if time scales with docs x bytes, R1 caps are the lever; if flat, R4 (smaller model) or R2 (skip) is |
| I-3 | Which reranker configuration minimizes p50 without a significant hit@3/MRR loss? | R1 grid + R3 vote + R4 model, via E3 | p50 `reranking_ms`, hit@3, MRR with CI | cheapest cell within the CI of the best |
| I-4 | Does the 4B reranker add anything over the 4B embedder on this corpus? | E3 reranker on/off, per stratum | nDCG@10, MRR, hit@1 | if off is within CI, make `reranker` default off for identifier queries (F3) |
| I-5 | Does the 0.004 prior help or hurt per query kind? | F1 sweep per stratum | hit@3, disfavored-before-expected | per-kind value via F3 or a single lower constant |
| I-6 | Does evaluating mined queries at HEAD (not the parent commit) bias results? | For 50 mined commits, create a `git worktree` at the parent, index into a disposable data dir, compare | delta in hit@3/MRR vs HEAD evaluation | if bias > 0.05 absolute, extend E2 with a `--at-parent` mode |
| I-7 | Do contextual headers help NL queries without hurting identifier queries? | C2 before/after per stratum | nDCG@10 per stratum | ship if NL improves and identifier is within CI |
| I-8 | How expensive is harvesting LSP references/call hierarchy at index time with rust-analyzer, jdtls, clangd? | Time per symbol on this repo and a Java fixture | ms per symbol, peak RSS | if > 50 ms/symbol, G1 stays query-time only |
| I-9 | Is the Windows long-path failure fully explained by table-name length? | Repro with a 200-char temp `data_dir` before and after S5 | pass/fail | if it still fails, open a Lance issue and document a `data_dir` length limit |
| I-10 | Does LCI beat grep-only for a given client model? | Owner-run A/B: same task list in Claude Code with and without the LCI MCP; log tokens, tool calls, cache reads, success | success rate, tokens, wall time | publish per model; this is the number that justifies the semantic channel (Cursor's reversal shows it must be re-measured per model) |

---

## Parking lot (ideas kept out until the owner changes the product boundary)

| Idea | Why parked |
| --- | --- |
| LLM-generated chunk/file summaries and wikis (DeepWiki, CodeWiki, kit's docstring index) | Needs a generation model; the only one available is on port 8765, which LCI must never contact. The boundary-respecting variant is A5's `annotate` tool that stores summaries the calling agent already wrote, keyed by chunk hash. |
| Server databases or search engines (Qdrant, Milvus, Meilisearch, Typesense, Elasticsearch, Memgraph/Neo4j) | AGENTS.md forbids another database or service; LanceDB already has FTS, `merge_insert`, `optimize`, scalar indexes. |
| Embedded second database for the manifest (SQLite, redb) | Arguably "another database"; try LanceDB-native and JSON improvements (W1) first; revisit only if the manifest becomes a measured bottleneck. |
| Web UI, system tray, graph explorer | Web frontend is excluded; a tray app is new product surface. |
| Docker packaging | Excluded by AGENTS.md. |
| Symbolic editing (rename, replace body) | Conflicts with the retrieval focus; documented duplicate-code failure mode (https://github.com/oraios/serena/issues/576). An LSP `rename` preview that returns a WorkspaceEdit without applying it could be revisited. |
| Custom trigram/suffix-array index (Zoekt, livegrep style) | Premature below monorepo scale; `rg` scans this repo in 0.15 s. |
| stack-graphs, Kuzu, CozoDB | Archived or dormant upstreams. |
| Non-commercial models and code (jina-reranker-v3/v3.5, recent Jina embeddings, GitNexus, jCodeMunch) | Licensing blocks redistribution or reuse. |
| In-process fallback embedder (fastembed-rs ONNX, model2vec `potion-code-16M-v2`) | Needs model download approval plus ONNX runtime packaging and a second vector column; quality is far below Qwen3-4B (CoIR 39 vs ~60 for CodeRankEmbed). Revisit after M3 if a service-less mode is wanted. |
| LLM query rewriting / HyDE inside LCI | Would need a generation model and demonstrably drops anchors (CORE-Bench); the calling agent can rewrite its own queries. |
