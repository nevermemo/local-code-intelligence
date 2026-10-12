# Bench results

Stored results of the spike bench ([../../bench.md](../../bench.md)). Each file is a
`lci.bench-results/1` JSON written by `cargo xtask bench run`; it holds commit ids,
scores, ranked file paths of the benchmarked repositories, timings and a machine
fingerprint (CPU model, cores, OS, architecture, ripgrep version), and no host names,
user names or local paths. A stored file changes only through a commit that says so
(open decision D5); the workflow never writes here.

## `baseline-bc.json`: levels B and C, the workflow's baseline

Build `a730ef1` (debug, clean tree; the LCI binary's sources are those of
`cloud/spike-s1s2` at `b87b16a`, only `xtask/`, docs and the workflow differ), bench set
v1 (60 tasks), fake embedder `lci-bench-fake-hash-512`, arms `fusion` and `grep`.
Run in a 4-core cloud container on 2026-10-12, 18:08:05Z to 18:14:34Z (wall 389 s),
with nothing else running:

```bash
cargo xtask bench freeze --out <scratch>/bin/lci-a730ef1
cargo xtask bench run --bin <scratch>/bin/lci-a730ef1 --work <scratch>/run-base \
  --clones <scratch>/clones --label "stored baseline (levels B, C)" \
  --output <scratch>/out/baseline-bc.json
```

| Arm | MRR (rr) | hit@1 | hit@10 | recall@10 | total_ms p50 |
| --- | --- | --- | --- | --- | --- |
| fusion (fake semantic + lexical) | 0.546 | 0.433 | 0.817 | 0.767 | 534 |
| grep (lexical only) | 0.637 | 0.533 | 0.833 | 0.769 | 446 |

Level B: full index at the first task's base commit, tokio 763 files / 17,704 chunks /
13.7 s / 51.1 MB, caddy 344 / 4,614 / 6.6 s / 16.4 MB, black 320 / 6,998 / 16.3 s /
25.5 MB; update (index after the fix commit) p50 1.45 s; symbol lookup proxy over 119
symbols recall@5 0.681, precision@5 0.136. `defs` and `navigation` are recorded as
unavailable (D2).

The `fusion` arm scoring below `grep` says nothing about semantic retrieval: the fake
embedder is a hashed bag of words (bench.md 2.3). It is the reference a later build is
compared with, not a quality claim.

Checks run before storing it (same container, same build family):

- Determinism (bench.md 7): two runs of build `8a06de7` (identical LCI sources) gave
  identical ranked files for 60 of 60 tasks in both arms; this baseline also matches
  them on every task (`compare`: 0 wins, 0 losses, 60 ties per arm).
- Incremental equals full: tasks `tokio-ae9d01121377`, `tokio-8705c87040c6`,
  `caddy-fbc88ae23220`, `caddy-6ae02b5ccbd9`, `black-42047e102f6d`,
  `black-c9b1148b5b87` run alone (`--task`, index from scratch at the base commit) gave
  the same files and chunk counts as the replay.
- Noise between identical builds on one machine, about 30 min apart: index size -1.2%,
  update p50 +2.2%, fusion `total_ms` p50 +12.6%, grep `total_ms` max +21.8%. Rule 3
  (10%) and rule 4 (25%, same machine within 6 h) sit above that.
- Across machines (bench.md 7, third check) the first workflow run on this commit is
  still to come.
