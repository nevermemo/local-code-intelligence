# Evaluation workflow

Use the checked-in portable definitions and supply local workspace mappings at runtime. This is the procedure that has produced trustworthy before/after evidence in this repository; follow it unless the human asks for something else.

## Procedure

1. Record the baseline commit, configuration overrides, model names, and whether indexes were created or reused.
2. Run the deterministic automated tests with the fake services first (`cargo xtask test --suite Filter` / `Evaluation` / `Indexing` as relevant).
3. Freeze the binaries. Build the baseline, copy `target/debug/local-code-intelligence(.exe)` to a scratch path such as `lci-base.exe`; apply the change, build again, copy to `lci-new.exe`. Later rebuilds then cannot disturb a running comparison, and on Windows the running copy does not lock the build output.
4. Give each scratch configuration its own short `data_dir` (for example `C:\lci-eval\base`). On Windows LanceDB fails with `os error 3` when the data directory is long, because table names are `ws_<16 hex>` plus Lance's internal paths; keep it under 150 characters (`/ready` flags a longer one as `data_dir_path_length`). Never point a scratch run at the running server's data directory.
5. Run the smallest live evaluation that exercises the changed stage against 8766/8767, once per binary:

   ```bash
   <bin> --config <cfg> evaluate ./evaluations/core.toml \
     --workspace self=<repo> \
     --output test-results/<name>.json
   ```

   Keep self and external-repository results separate; absent optional workspaces are skips, not failures.
6. For a change to priors, source-role classification, fusion, or candidate counts, also run both binaries with the reranker switched off (`[channels]` `reranker = false` in the scratch config; it yields the same fusion-order shortlist as a failed reranker, with zero `reranking_ms` and no warning). The reranker masks fusion-order changes on small fixtures; the fusion-order run is where such a change shows.
   - Other arms: grep-only (`semantic = false`, `lsp = false`, `reranker = false`) and semantic-only (`lexical = false`, `lsp = false`, `reranker = false`); reports do not record the switches, so name the output files after the arm.
   - Config sections use `deny_unknown_fields`, so a binary older than a key (for example `[fusion] max_chunks_per_file`) rejects a scratch config that sets it: give each binary a config it understands, and set new keys only for the binary that has them.
   - The default fusion weights are plain RRF (`[fusion] semantic_weight = lexical_weight = lsp_weight = 1.0`), and the default caps a file at two chunks ahead of other files (`[fusion] max_chunks_per_file = 2`, F2b). An arm that must match a report made under other defaults sets them explicitly: see "Reproducing older reports" at the end.
7. Compare hit@1, hit@3, hit@8, MRR, nDCG@10 (`mean_ndcg_at_10`), recall@3/@8 (`mean_recall_at_3`, `mean_recall_at_8`), file acc@5/@10 (`mean_file_acc_at_5`, `mean_file_acc_at_10`), first expected ranks, disfavored results above expected results, role failures, fallbacks, the index lifecycle counts including `index_refreshed` (created + reused + waited + refreshed should equal the executed count), and per-stage latency from the aggregate `stage_latency` block (p50/p95/max and `n` for query embedding, LanceDB, lexical, LSP, fusion, reranking, and total; nearest-rank p95 hides the worst query at small `n`, so read `max`). The `@10` metrics are bounded by each query's `top_k` (8 in `core.toml`, 10 in the mined sets), so treat them as @8 unless `top_k >= 10`.
   Pair the two reports instead of eyeballing the aggregates: `<bin> compare baseline.json candidate.json` (offline, no services or config) matches queries by workspace and query text and prints, for MRR, nDCG@10 and hit@1, both means, the mean per-query delta with a seeded 95% paired-bootstrap interval, and reciprocal-rank wins/losses/ties; it also counts unpaired and skipped queries and, for tagged definitions, repeats the table per tag (`--output <file>` saves the JSON, `--seed`/`--resamples` change the bootstrap). Read the interval before claiming a win: a delta whose interval contains zero is not evidence, and `core.toml`'s eleven queries will rarely clear it. Report the interval and the W/L/T with any claimed gain, and check the unpaired counts are zero. `evaluate ... --baseline <report.json>` embeds the same comparison in the new report.
   Before trusting a paired difference, check that both arms had the same channel coverage: count per-query `lsp_search_ms` at the `lsp_search_budget_ms` cap (or the LSP note in `notes` in a dump) in each report. Fusion-only runs can lose the LSP channel on a long run of queries while reranked runs do not (roadmap N8); on 2026-10-10 that made F2a's weight grid look significant (+0.059 MRR) when an exact replay with LSP present showed no effect. When coverage differs, decide from an E0 dump replayed with `replay` instead of from live arms.
8. Inspect path, line, role, and bounded source evidence for every failure; aggregate metrics alone do not prove correctness.
9. Save reports under `test-results/` and keep them out of commits. Name them so the pair is obvious (`<change>-baseline-rerank-on.json`, `<change>-after-rerank-off.json`).

## Replaying a dump instead of running live

For a live service rather than an evaluation run, `LCI_CANDIDATE_DUMP=<file>` (README) appends the same pool per search to a JSON Lines file.

`evaluate --dump-candidates` records every query's whole fused candidate pool, the dumped configuration (`arm`), and the reranker's scores; `<bin> replay dump.json --output <file> [options]` then scores other settings from that file in seconds, offline (no services, config, or index), through the unchanged metrics. Its output is an evaluate-shaped report, so `<bin> compare dump.json <file>` pairs it with real reports and with other replays. Run one dump live; replay the grid.

Replay when the question is a pure function of the dumped pool and scores, so both arms share identical candidates (which also removes run-to-run LSP coverage differences):

- the rerank window (`--window`), the per-file cap and where it runs (`--cap`, `--cap-stage shortlist|results`);
- fusion weights, `rrf_k`, and the source-role prior (`--semantic-weight`, `--lexical-weight`, `--lsp-weight`, `--rrf-k`, `--prior`) with `--rerank-mode off`, which is fusion order alone;
- how the reranker's scores are used (`--rerank-mode replace|vote|off`, `--vote-weight`).

Run live when anything the dump cannot hold changes: `rerank_document_max_bytes`, the reranker model or instruction, the query template (`embedding_query_template`), chunking, embedding, a window or channel depth larger than the dump took, or a new channel. Run live too for the final acceptance of a replayed winner (one live run of the adopted setting, compared with `compare`) and for every latency claim: replayed `reranking_ms` and `total_ms` are labelled cost-model estimates (`replay.latency`; the fit and its R^2 are reported), the measured timings stay under `replay.original`.

Taking the dump: use a frozen binary and a scratch configuration with `rerank_candidate_count` as large as the largest window you will replay (at most 200) and, to replay either cap stage or other caps, `[fusion] max_chunks_per_file = 0`; a replayed window can only be scored where the dump sent the candidates to the reranker. Before trusting a grid, check determinism: `<bin> replay dump.json --output same.json` (every setting left out keeps the dumped value) must reproduce the dump's evidence and metrics, so `compare dump.json same.json` shows all ties, zero refused queries, and zero `replay.past_window_queries` (production returns at most the window, so with a window smaller than `k` replay returns more rows and counts those queries). A failed determinism check is a defect in replay, not a result.

What is exact: any window up to the dumped shortlist; the per-file cap at either stage; any weights, `rrf_k`, or prior under `--rerank-mode off`; and `replace`/`vote` over candidates the dump scored (the reranker scores each query-document pair independently, so a score does not depend on its neighbours). What is refused, per query and never approximated: a reranked window holding an unscored candidate (`unscored_window`), a reranked arm over a dump whose reranker failed or was off (`rerank_not_scored`), a dump whose fusion or cap does not reproduce at its own arm (`fusion_mismatch`, `shortlist_mismatch`), and a query with no dump (`not_dumped`). Refused queries are written as skipped with a `replay_refusal`, counted in `replay.refused`, and listed in `replay.refused_queries`; `compare` excludes them from every pair, so read its `paired` and skipped counts and report the refused counts with any replayed result. If a refusal differs between the arms you are comparing, the pairs are different query sets: compare each arm against the dumped arm replayed over the same queries.

Pre-register the grid before looking at results, select on one half of the queries, confirm on the other, and apply the decision rule below to the paired intervals as for live arms. Replay does not replace the checks in steps 6 to 8 for the adopted setting; it decides which settings deserve a live run.

## Operating constraints

- Run live evaluations sequentially, never in parallel. The reranker on the shared iGPU is the bottleneck: a reranked query with 24 candidates takes about 20 s on this machine (measured 2026-10-10 with the llama.cpp reranker on 8767: search p50 20.2 s, p95 34.8 s, reranking p50 19.6 s at the production window of 24 and 9.9 s at window 12; roadmap, the "R1 live confirm (2026-10-10 ...)" bullet under V6, which compares against V6's arm 0), so `evaluations/core.toml`'s self set runs for several minutes per binary. Re-measure when the serving setup or the machine changes.
- Do not compile, run the test suite, or start another indexing pass while a timed live evaluation runs. Heavy CPU/GPU load distorts reranker timings and makes the before/after latency comparison meaningless.
- Freeze the evaluated tree. The self set indexes the working tree, and a search refreshes a stale index, so any edit during a run (including roadmap commits, since documentation is indexed too) changes the candidate pools of later queries. The V5 run (2026-10-10) refreshed the index on 8 of 35 mined queries and kept identical top-24 pools on only 20 of them, which confounded a reranker comparison. Make no edits in the evaluated tree while a run is going; for multi-arm work, evaluate a frozen copy (a `git worktree` at a fixed commit, indexed once) instead. Compare top-24 fused-pool identity per query before attributing a difference to a reranker or weight change.
- Neutrality checks ("this slice changes no result") compare each query's ranked `evidence` (`relative_file_path`, `start_line`, `end_line`), never metrics alone, and a helper that reads rows must fail on an empty list. The semantic channel is not bit-reproducible: the same binary run twice with the reranker off differed on 2 of 14 core queries (2026-10-11, vLLM embedder under concurrent indexing load; batch composition shifts the query embedding). So prove identity on a lexical-only configuration (`[channels] semantic = false`, exact) and with the reranker on, and use a same-binary rerun as the noise floor for the reranker-off semantic arm (A7a, 2026-10-11).
- Scratch fixtures and decoy evaluation TOMLs live outside the repository (or under `test-results/`), never in `evaluations/`, which holds only portable, path-free definitions.
- `evaluations/core.toml` has eleven required self queries, so one query is about nine points of hit@1. Treat small metric moves as noise; look at which queries moved and why, and prefer a larger scratch decoy set when the change targets a specific role or language.

## Latency work

Separate first indexing, query embedding, LanceDB, lexical, LSP, fusion, reranking, and total time. Warm the relevant model once when startup latency is outside the question. Use a small number of representative runs unless the human asks for extended benchmarking.

## Decision rule

Accept a ranking change only when the intended metric or latency improves without material regressions in required queries or fail-open behavior, in both the reranked and the fusion-order runs where both apply. Never insert evaluation paths, symbols, snippets, or queries into production ranking logic, and never tune a prior or threshold to a specific evaluation query.

## Reproducing older reports

A report made under other defaults is reproduced by setting the then-defaults explicitly in the arm's scratch config. Commits: `79138e4` (F2a: `semantic_weight` default 2.0), `92a7060` (F2b: `max_chunks_per_file` default 2), `bb6589d` (reverts the semantic weight default to 1.0, after an exact offline replay with the LSP channel present found no significant effect).

| Report made | Defaults then | Arm settings that match it (today's defaults: weights 1.0, cap 2) |
| --- | --- | --- |
| before `79138e4` | weights 1.0, no cap | `max_chunks_per_file = 0` |
| from `79138e4` to `92a7060` | `semantic_weight = 2.0`, no cap | `semantic_weight = 2.0`, `max_chunks_per_file = 0` |
| from `92a7060` to `bb6589d` | `semantic_weight = 2.0`, cap 2 | `semantic_weight = 2.0` |
| after `bb6589d` | today's defaults | none |
