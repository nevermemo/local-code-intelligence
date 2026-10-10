# Evaluation workflow

Use the checked-in portable definitions and supply local workspace mappings at runtime. This is the procedure that has produced trustworthy before/after evidence in this repository; follow it unless the human asks for something else.

## Procedure

1. Record the baseline commit, configuration overrides, model names, and whether indexes were created or reused.
2. Run the deterministic automated tests with the fake services first (`cargo xtask test --suite Filter` / `Evaluation` / `Indexing` as relevant).
3. Freeze the binaries. Build the baseline, copy `target/debug/local-code-intelligence(.exe)` to a scratch path such as `lci-base.exe`; apply the change, build again, copy to `lci-new.exe`. Later rebuilds then cannot disturb a running comparison, and on Windows the running copy does not lock the build output.
4. Give each scratch configuration its own short `data_dir` (for example `C:\lci-eval\base`). On Windows LanceDB fails with `os error 3` when the data directory is long, because table names are `ws_<64 hex>` plus Lance's internal paths. Never point a scratch run at the running server's data directory.
5. Run the smallest live evaluation that exercises the changed stage against 8766/8767, once per binary:

   ```bash
   <bin> --config <cfg> evaluate ./evaluations/core.toml \
     --workspace self=<repo> \
     --output test-results/<name>.json
   ```

   Keep self and external-repository results separate; absent optional workspaces are skips, not failures.
6. For a change to priors, source-role classification, fusion, or candidate counts, also run both binaries with the reranker switched off (`[channels]` `reranker = false` in the scratch config; it yields the same fusion-order shortlist as a failed reranker, with zero `reranking_ms` and no warning). The reranker masks fusion-order changes on small fixtures; the fusion-order run is where such a change shows. Other arms: grep-only (`semantic = false`, `lsp = false`, `reranker = false`) and semantic-only (`lexical = false`, `lsp = false`, `reranker = false`); reports do not record the switches, so name the output files after the arm.
7. Compare hit@1, hit@3, hit@8, MRR, nDCG@10 (`mean_ndcg_at_10`), recall@3/@8 (`mean_recall_at_3`, `mean_recall_at_8`), file acc@5/@10 (`mean_file_acc_at_5`, `mean_file_acc_at_10`), first expected ranks, disfavored results above expected results, role failures, fallbacks, the index lifecycle counts including `index_refreshed` (created + reused + waited + refreshed should equal the executed count), and per-stage latency from the aggregate `stage_latency` block (p50/p95 for query embedding, LanceDB, lexical, LSP, fusion, reranking, and total). The `@10` metrics are bounded by each query's `top_k` (8 in `core.toml`, 10 in the mined sets), so treat them as @8 unless `top_k >= 10`.
   Pair the two reports instead of eyeballing the aggregates: `<bin> compare baseline.json candidate.json` (offline, no services or config) matches queries by workspace and query text and prints, for MRR, nDCG@10 and hit@1, both means, the mean per-query delta with a seeded 95% paired-bootstrap interval, and reciprocal-rank wins/losses/ties; it also counts unpaired and skipped queries and, for tagged definitions, repeats the table per tag (`--output <file>` saves the JSON, `--seed`/`--resamples` change the bootstrap). Read the interval before claiming a win: a delta whose interval contains zero is not evidence, and `core.toml`'s eleven queries will rarely clear it. Report the interval and the W/L/T with any claimed gain, and check the unpaired counts are zero. `evaluate ... --baseline <report.json>` embeds the same comparison in the new report.
8. Inspect path, line, role, and bounded source evidence for every failure; aggregate metrics alone do not prove correctness.
9. Save reports under `test-results/` and keep them out of commits. Name them so the pair is obvious (`<change>-baseline-rerank-on.json`, `<change>-after-rerank-off.json`).

## Operating constraints

- Run live evaluations sequentially, never in parallel. The reranker on the shared iGPU is the bottleneck: a reranked query with 24 candidates takes roughly 25-90 s on the current development machine (median about 30 s), so `evaluations/core.toml`'s self set runs for several minutes per binary.
- Do not compile, run the test suite, or start another indexing pass while a timed live evaluation runs. Heavy CPU/GPU load distorts reranker timings and makes the before/after latency comparison meaningless.
- Scratch fixtures and decoy evaluation TOMLs live outside the repository (or under `test-results/`), never in `evaluations/`, which holds only portable, path-free definitions.
- `evaluations/core.toml` has eleven required self queries, so one query is about nine points of hit@1. Treat small metric moves as noise; look at which queries moved and why, and prefer a larger scratch decoy set when the change targets a specific role or language.

## Latency work

Separate first indexing, query embedding, LanceDB, lexical, LSP, fusion, reranking, and total time. Warm the relevant model once when startup latency is outside the question. Use a small number of representative runs unless the human asks for extended benchmarking.

## Decision rule

Accept a ranking change only when the intended metric or latency improves without material regressions in required queries or fail-open behavior, in both the reranked and the fusion-order runs where both apply. Never insert evaluation paths, symbols, snippets, or queries into production ranking logic, and never tune a prior or threshold to a specific evaluation query.
