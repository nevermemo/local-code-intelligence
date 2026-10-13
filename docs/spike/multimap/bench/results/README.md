# Bench results

Stored results of the spike bench ([../../bench.md](../../bench.md)). Each file is a
`lci.bench-results/1` JSON written by `cargo xtask bench run` (a `.md` beside it is the
`bench compare` table); it holds commit ids,
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

## Spike iterations, 2026-10-13 (levels B and C)

Four builds of the multimap spike, each compared with `baseline-bc.json` above. All
runs: bench set v1 (60 tasks), fake embedder `lci-bench-fake-hash-512`, arms `fusion`
and `grep`, one 4-core cloud container (same `machine` fingerprint as the baseline's,
but a different container), runs one after another with no build or gate running
beside them, except that the S2 run overlapped a few short `xtask` unit-test and clippy
runs (light load; its costs are the least clean). Level D did not run: `huggingface.co` was still denied by the
environment's network policy at 04:12:50Z, so no real-embedder numbers exist.

Builds (debug, clean trees, frozen by hand as bench.md section 3 says, because these
commits have no `bench freeze`; each in its own worktree, binary hashes checked):

| Build | Branch head | LCI sources | `sym.defs` probe |
| --- | --- | --- | --- |
| `b87b16a` | `cloud/spike-s1s2` (S2) | the baseline's (`a730ef1` differs only in `xtask/`, docs, the workflow) | none: the tree has no `sym.defs` |
| `b466f7f` | `cloud/spike-s3a` (S3a) | S2 plus the S3a maps | frozen from `b466f7f` |
| `6f73fb9` | `cloud/spike-int` at 04:12Z | identical to `b466f7f` (`git diff` empty; same binary hash) | frozen from `6f73fb9` |
| `2a09464` | `cloud/spike-int` at 04:58Z (trim and S3b merged) | S3a plus the trim and S3b | frozen from `2a09464` |

Commands (runner `xtask` frozen from `5661b1c` on `cloud/spike-bench-runs`; `<scratch>`
is a directory outside the repository, `<tree>` a worktree of the build's commit):

```bash
# per build: freeze the binary by hand (bench.md 3)
# (first touch every file of <tree>: see the shared-target note in bench.md 3)
cd <tree> && cargo build --locked --bin local-code-intelligence
cp <target>/debug/local-code-intelligence <scratch>/bin/lci-<build>
# sidecar <scratch>/bin/lci-<build>.json: {"commit": <full sha>, "dirty": false, "profile": "debug"}
# per build with sym.defs: freeze the probe from the same tree
cargo xtask bench freeze-defs --src <tree> --out <scratch>/bin/defs-<build>
# per build: run levels B and C against the stored baseline
cargo xtask bench run --bin <scratch>/bin/lci-<build> [--defs-probe <scratch>/bin/defs-<build>] \
  --work <scratch>/run-<build> --clones <scratch>/clones --label "<label>" \
  --output <scratch>/out/<file>-bc.json \
  --baseline docs/spike/multimap/bench/results/baseline-bc.json \
  --table <scratch>/out/<file>-vs-baseline.md
# between iterations
cargo xtask bench compare <older>-bc.json <newer>-bc.json
```

Stored files: `<file>-bc.json` (results), `<file>-vs-baseline.md` (comparison with the
baseline), and three iteration comparisons: `s3a-b466f7f-vs-s2-b87b16a.md`,
`int-2a09464-vs-s3a-b466f7f.md`, and `int-6f73fb9-vs-s3a-b466f7f.md` (the same binary
twice: the same-build rerun).

**Cost (rule 4):** every comparison with the baseline is printed, never judged on cost:
the runs started more than 6 h after it, and by the owner's decision of 2026-10-13 runs
from different containers are never judged on cost even with the same fingerprint. The
iteration comparisons ran in one container within 6 h, so rule 4 applies to them.

### `s2-b87b16a-bc.json`: S2 `b87b16a`, 04:46:24Z to 04:53:09Z (wall 406 s)

| Arm | MRR (rr) | hit@1 | hit@10 | recall@10 | total_ms p50 | rr delta vs baseline [95% CI] | W/L/T |
| --- | --- | --- | --- | --- | --- | --- | --- |
| fusion | 0.546 | 0.433 | 0.817 | 0.767 | 454 | +0.000 [+0.000, +0.000] | 0/0/60 |
| grep (never a gate) | 0.637 | 0.533 | 0.833 | 0.769 | 385 | +0.000 [+0.000, +0.000] | 0/0/60 |

Level B: index size 93.9 MB (+1.0%), update p50 1329 ms, lookup proxy recall@5 0.681 /
precision@5 0.136 (119 names), `defs` unavailable. Same files as the baseline on 60 of
60 tasks in both arms. Verdict: no regression.

### `s3a-b466f7f-bc.json`: S3a `b466f7f`, 04:57:45Z to 05:11:47Z (wall 842 s, of which the probe about 7 min)

| Arm | MRR (rr) | hit@1 | hit@10 | recall@10 | total_ms p50 | rr delta vs baseline [95% CI] | W/L/T |
| --- | --- | --- | --- | --- | --- | --- | --- |
| fusion | 0.546 | 0.433 | 0.817 | 0.767 | 493 | +0.000 [+0.000, +0.000] | 0/0/60 |
| grep (never a gate) | 0.637 | 0.533 | 0.833 | 0.769 | 415 | +0.000 [+0.000, +0.000] | 0/0/60 |

Level B: index size 92.8 MB (-0.1%), update p50 1434 ms, lookup proxy 0.681 / 0.136.
`defs` (hit@5 / precision@5, 0 errors): all 0.218 / 0.183 (n=119); tokio 0.703 / 0.588
(n=37); caddy 0.000 / 0.000 (n=36); black 0.000 / 0.000 (n=46). Probe on tokio: 8,566
declarations, about 8.6 s per task (scan plus build). Verdict: no regression.

### `int-6f73fb9-bc.json`: spike-int `6f73fb9`, 05:27:53Z to 05:41:51Z (wall 838 s)

| Arm | MRR (rr) | hit@1 | hit@10 | recall@10 | total_ms p50 | rr delta vs baseline [95% CI] | W/L/T |
| --- | --- | --- | --- | --- | --- | --- | --- |
| fusion | 0.546 | 0.433 | 0.817 | 0.767 | 473 | +0.000 [+0.000, +0.000] | 0/0/60 |
| grep (never a gate) | 0.637 | 0.533 | 0.833 | 0.769 | 392 | +0.000 [+0.000, +0.000] | 0/0/60 |

Level B: index size 93.6 MB (+0.7%), update p50 1371 ms, lookup proxy 0.681 / 0.136,
`defs` as S3a. Verdict: no regression. Against S3a (the same binary): 60/60 ties in both
arms, equal `defs`; size and p50 costs moved by -5.6% to +0.8% (index size +0.8%,
update p50 -4.5%, fusion p50 -3.9%, grep p50 -5.6%): this container's run-to-run noise.

### `int-2a09464-bc.json`: spike-int `2a09464`, 05:13:49Z to 05:27:53Z (wall 844 s)

| Arm | MRR (rr) | hit@1 | hit@10 | recall@10 | total_ms p50 | rr delta vs baseline [95% CI] | W/L/T |
| --- | --- | --- | --- | --- | --- | --- | --- |
| fusion | 0.546 | 0.433 | 0.817 | 0.767 | 472 | +0.000 [+0.000, +0.000] | 0/0/60 |
| grep (never a gate) | 0.637 | 0.533 | 0.833 | 0.769 | 396 | +0.000 [+0.000, +0.000] | 0/0/60 |

Level B: index size 93.8 MB (+1.0%), update p50 1429 ms, lookup proxy 0.681 / 0.136,
`defs` as S3a (all 0.218 / 0.183, tokio 0.703 / 0.588). Verdict: no regression.

### Reading

- **Retrieval (level C): no difference** between any two of the five builds (baseline,
  S2, S3a, spike-int `6f73fb9` and `2a09464`). Every interval is [0, 0] with 60 pairs
  and no errored or unpaired task, and the ranked files are the same on 60 of 60 tasks in
  both arms. Under rule 1 that is neither a regression nor an improvement. This is what
  the spike promises at this stage: the maps run beside the production search path, so
  fusion (T0) and the lexical channel must not move. It also means the trim and S3b
  merges in `2a09464` did not touch ranking on this set.
- **The grep arm** (reported, never a gate) is unchanged too: MRR 0.637 on every build.
- **Index size and cost (level B):** no rule fired. Index size stays within -0.1% to
  +1.0% of the baseline (rule 3's limit is 10%). Between iterations (same container,
  within 6 h, so judged) update p50 and search p50 moved by at most +8.6% (S2 to S3a),
  under rule 4's 25%; the same-build rerun moved by up to 5.6%, so these moves are of the
  size of the noise. Against the baseline, costs are printed only (rule 4 and the owner's
  cross-container decision).
- **`defs` (new, reported only):** S3a's `sym.defs` covers Rust only, so tokio is the only
  repository where it can score; caddy and black score 0 by construction, which pulls
  the three-repository mean down to 0.218 / 0.183. On tokio's 37 gold names it finds a
  changed declaration's file in the top 5 for 0.703 of names (the lookup proxy, a search
  for the bare name: 0.730) with precision@5 0.588 (lookup: 0.146): about the same
  recall, four times the precision, because it returns declarations, not text matches.
  `defs` did not change from S3a to `2a09464`. These are the map's library answers
  through the probe, not a production search path.
- **Not measured:** level D (real embedder; network policy), `navigation` (no language
  servers on the runner).
