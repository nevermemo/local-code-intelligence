# Evaluation workflow

Use the checked-in portable definitions and supply local workspace mappings at runtime.

1. Record the baseline commit, configuration, model names, and whether indexes were created or reused.
2. Run deterministic automated tests with mock services first.
3. Run the smallest live evaluation that exercises the changed stage with 8766/8767.
4. Compare hit@1, hit@3, hit@8, MRR, first expected ranks, disfavored results above expected results, fallbacks, and per-stage latency.
5. Inspect path, line, role, and bounded source evidence for failures; aggregate metrics alone do not prove correctness.
6. Keep self and external-repository results separate. Report absent optional workspaces as skips.
7. Save generated reports under `test-results` and keep them out of commits.

For latency work, separate first indexing, query embedding, LanceDB, lexical, LSP, fusion, reranking, and total time. Warm the relevant model once when startup latency is outside the question. Use a small number of representative runs unless the user asks for extended benchmarking.

Accept a ranking change only when the intended metric or latency improves without material regressions in required queries or fail-open behavior. Never insert evaluation paths, symbols, snippets, or queries into production ranking logic.
