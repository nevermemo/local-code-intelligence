# Multimap trend

One row per (wave, set), appended by `cargo xtask maps bench` (see
`xtask/src/maps/bench.rs`). Rows are never rewritten: a new measurement is a
new wave label. The gold sets and the frozen tree are private; they are passed
to the runner as `<gold>` and `<tree>` and nothing here names their contents.

How to read a row:

- `n_gold` is the number of gold items in the run (impact: the `primary` items
  only, cut by `--limit`). `n_attempted` is how many got a traversal call;
  `n_scored` is how many returned an answer to score. `skipped` counts the
  rest by cause: `timeout` (the call ran past the per-item limit and was
  killed), `cli_error` (the CLI failed or printed no JSON),
  `no_answer_type` (no selected map emits the node kind the set needs) and
  `map_unavailable` (the preflight call for the map failed, so the run did not
  use it). Nothing is dropped without a count: n_gold = n_scored + skipped
  (+ the items of a set that was not attempted at all).
- Every metric, precision, F0.5 and total below is over the `n_scored` items
  only; a skipped item is in no mean. `answered` is how many of the scored
  items returned at least one answer; an item that returns nothing scores 0,
  including in precision.
- `metrics` are the gold scorer's own metrics for the set (means over the
  scored items where defined; `(k)` marks a metric defined on only k items).
- `precision` and `F0.5` are of the returned answer set against gold, per item,
  then averaged. F0.5 weighs precision twice as much as recall, because
  low-precision context costs the agent that reads it. The per-item precision
  and recall come from the scorer's own definitions (impact: declaration
  precision and recall; flow: answers that are not spurious, and path recall;
  where-to: set precision and recall, and for registration items the distinct
  non-neutral files against the expected files; history: accepted commits among
  the distinct commits returned, and whether one was found; instructions:
  section precision and recall for I1, required-unit precision@5 and recall
  for I2).
- `pops`, `hops`, `xhops` (cross-map hops) and `stops` are sums over the scored
  items of the traversal's own totals. `ans tok` is the sum of characters of the
  answer text the traversal returned, divided by 4 (an approximation, not a
  tokenizer count).
- `engine ms` is the traversal's own elapsed time per item, p50/p95;
  `wall ms` is the whole `lci traverse` process per item (it loads the index
  each time), p50/p95. A latency marked `under load` was measured while other
  work ran on the machine and is not comparable with a clean run.
- `maps` are the maps asked for; `maps unavailable` lists those whose preflight
  failed (they were not used).
- A set no selected map can answer gets `no map yields this answer type`
  instead of zeros.

| wave | set | n_gold | n_attempted | n_scored | skipped | answered | metrics (over n_scored) | precision | F0.5 | pops | hops | xhops | stops | ans tok | engine ms p50/p95 | wall ms p50/p95 | maps | maps unavailable | policy | note |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S7-stand-in-seeds | impact | 141 | 141 | 141 | none | 140/141 | decl_recall=0.014; decl_f1=0.013; decl_ndcg@10=0.018; file_f1=0.188 (over 141) | 0.016 | 0.014 | 7601 | 8103 | 1166 | 0 | 23534 | 14/54 | 480/819 | seeds.scip_names,graph.scip | none | default | tree bfaff44; debug build; name-token stand-in seeds; ans tok = chars/4 |
| S7-stand-in-seeds | flow | 111 | 111 | 111 | none | 111/111 | path_recall=0.035; tau=n/a (0); spurious=9.766 (over 111) | 0.012 | 0.011 | 8656 | 9657 | 1148 | 0 | 20220 | 18/63 | 533/716 | seeds.scip_names,graph.scip | none | default | tree bfaff44; debug build; name-token stand-in seeds; ans tok = chars/4 |
| S7-stand-in-seeds | where-to | 59 | 59 | 59 | none | 59/59 | reg_hit@3=0.320 (25); reg_mrr=0.253 (25); set_f1=0.018 (34) (over 59) | 0.085 | 0.084 | 5057 | 5632 | 613 | 0 | 11119 | 28/68 | 535/615 | seeds.scip_names,graph.scip | none | default | tree bfaff44; debug build; name-token stand-in seeds; ans tok = chars/4 |
