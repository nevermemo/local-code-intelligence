# Spike bench: comparing frozen builds

Status: **v1 design.** Written 2026-10-12 (`date -u` 17:20Z) on `cloud/spike-bench`, stacked
on `cloud/spike-s1s2` (`b87b16a`). Labels as in [specs/](specs/README.md): **Read** = in the
code or docs at that tree; **Inferred** = designed here.

## 1. Purpose

A repeatable benchmark that answers one question: did spike iteration *new* change what
LCI does, relative to iteration *base*, on a fixed task set that is not this repository?
Both sides are frozen builds (a copied binary, never `target/debug` itself), the task set
is frozen data, and every score is computed by the runner from ranked evidence rows, so
two builds whose report schemas differ are still compared on the same rules.

It does not replace the owner's live evaluation (`evaluations/core.toml`, the mined self
set, the agent A/B): it is a cheap, repeatable guard that can run on a schedule without a
GPU or model services.

## 2. Levels

| Level | What runs | Where | Can claim | Cannot claim |
| --- | --- | --- | --- | --- |
| A | Contracts: T0 (fusion bit for bit through the map registry), the contract kit, fail-open. Already in `cargo xtask test --suite Full` (Read: `lci-maps` tests at `b87b16a`). | CI, every push | The map layer reproduces today's fusion exactly; maps fail open. | Anything about quality or cost. |
| B | Structural: index cost and size, update cost, search latency, and symbol lookup against commit-derived gold. | Cloud or CI, fake embedder | A build indexes or updates more slowly, stores more, or finds a changed declaration's file less often by name. | Retrieval quality for natural-language queries; LSP navigation (see 2.1). |
| C | Retrieval with the deterministic fake embedder (no reranker, no LSP): a `fusion` arm (semantic plus lexical) and a `grep` arm (lexical only). | Cloud or CI | A change to chunking, the lexical channel, fusion, priors, filters or the plumbing between them moved file-level ranks on the set. | Semantic quality: the fake embedder is a hashed bag of words, not a model. |
| D | Level C's `fusion` arm with a real embedder (Qwen3-Embedding-0.6B on CPU), no reranker. | Once, by hand, in a container with the model | File-level retrieval of that build with a real (small) embedder on the stated set or subset. | Production quality (production uses a larger embedder and a reranker); latency of the owner's machine. |

### 2.1 Level B in v1 (Inferred)

The brief names `sym.defs` and navigation precision/recall. At `b87b16a` neither has a
surface a frozen binary can be asked through: `sym.defs` is built on `cloud/spike-s3a`
(not on this branch's base) with no CLI, and navigation needs a language server, which the
cloud runner and CI do not have. v1 therefore measures:

- `index`: full index wall time, stored chunk count (`status`), and data directory size,
  per repository at the first task's base commit.
- `update`: wall time of `index` after moving the checkout from a task's base commit to
  its fix commit (exactly the fix's diff), per task.
- `latency`: per-query `total_ms` from level C (p50, p95, max), per arm.
- `lookup` (a proxy, labelled as such): for every gold symbol, a search for its bare name
  in the `fusion` arm; precision@5 = share of the top 5 results that lie in a file where
  the fix changed that symbol, recall@5 = share of gold symbols with such a file in the
  top 5.
- `defs` and `navigation`: recorded as `unavailable` with the reason. They switch on when
  a build exposes `sym.defs` (a CLI or REST surface) and the runner has a language server;
  the gold for them is already in the set (section 4).

Latency in B is wall clock on whatever machine ran it; it is comparable only between two
builds run back to back on one machine (section 6).

### 2.2 Level C harness (Inferred)

The RunRecord harness (S4 offline part) is on `cloud/spike-s3b`, not on this branch's
base. Level C v1 drives the frozen binary's existing `evaluate` command (Read:
`src/main.rs`, `Command::Evaluate`) with a generated one-workspace definition per task and
reads only `queries[].evidence[].relative_file_path` and `queries[].timings.total_ms` from
its report. **Switch note:** once the harness reaches this branch's lineage, level C
should read RunRecords instead; the scoring rules in section 5 stay.

## 3. Producing base and new builds

Follows `.agents/skills/lci-retrieval-evaluation/references/evaluation-workflow.md`
step 3 (freeze the binaries):

```bash
git worktree add <scratch>/base <base-commit>        # or check out the base commit
cargo xtask bench freeze --out <scratch>/lci-base    # builds, copies the binary, records the commit
git checkout <new-commit>
cargo xtask bench freeze --out <scratch>/lci-new
cargo xtask bench run --bin <scratch>/lci-base --levels B,C --output <scratch>/base.json
cargo xtask bench run --bin <scratch>/lci-new  --levels B,C --output <scratch>/new.json
cargo xtask bench compare <scratch>/base.json <scratch>/new.json
```

`freeze` writes `<out>` (the binary) and `<out>.json` (commit, dirty flag, profile). Each
run gets its own scratch data directory and its own task checkouts under the runner's
work directory; nothing is shared between the two runs, and nothing is written inside an
indexed checkout. Runs are sequential.

The scheduled workflow (`.github/workflows/spike-bench.yml`) freezes only the head build
and compares it with the stored baseline results file
(`docs/spike/multimap/bench/results/baseline-bc.json`): level C is deterministic, so a
stored file is as good as a rebuilt base for quality; latency against a stored file from
another machine is report-only.

## 4. Bench set v1

Data only, in `docs/spike/multimap/bench/bench-set-v1.json`: per repository its URL,
licence and language; per task the fix commit, its parent (the base commit the search
runs on), the query text, the gold files and the gold symbols. No code from those
repositories is stored. Mined by `cargo xtask bench mine` (filters in the file's `filter`
block and in [bench/README.md](bench/README.md)).

- Replayed commit: the search runs on the fix commit's parent, so the gold files exist
  and hold the code before the fix (unlike `mine-queries`, which searches `HEAD`).
- Query text: the commit message (subject, plus the first body paragraph when it adds
  words). Linked issues are not fetched (no API calls in the miner; Inferred,
  conservative). A query that names a gold file stem or gold symbol is tagged `keyword`.
- Gold files: the supported-extension files the fix changed, excluding test files.
- Gold symbols: the enclosing declaration names git reports in the hunk headers of the
  fix's diff of each gold file (git's built-in `rust`, `golang` and `python` diff
  drivers), per file. Not derived from a SCIP or LSP index, nor from LCI's own parser
  (lesson L8): the gold must not share a source with any map it scores.

## 5. Metrics and scoring (per arm, per task)

From the ranked evidence of one search (top 10), collapsed to distinct files in rank
order:

- `rr`: 1 / rank of the first gold file (0 if none in the top 10).
- `hit1`, `hit5`, `hit10`: a gold file at rank <= 1, 5, 10.
- `recall10`: share of gold files in the top 10.

Aggregates: means over tasks; per-repository and per-tag breakdowns (`keyword`,
`keyword-agnostic`). A task the binary failed on is scored as an error, never dropped.

Comparison of two results files (`bench compare`): tasks are paired by task id and arm;
for `rr`, `hit10` and `recall10` it prints both means, the mean paired delta and a 95%
percentile interval from a paired bootstrap (10,000 resamples of tasks, fixed seed 42),
and wins, losses and ties on `rr`. Unpaired and errored tasks are counted and printed.

## 6. Pre-registered regression rules

Written before any result exists. "Regression" in a `compare` verdict means:

1. **Quality (level C `fusion` arm; level D when both sides ran it):** the 95% bootstrap
   interval of the mean delta lies entirely below zero for `rr` or for `hit10`.
2. **Coverage:** a task that scored on base errors on new.
3. **Index size (level B):** total data directory size grows by more than 10%.
4. **Cost (level B, only when both results files record the same `machine` fingerprint):**
   p50 of `update` wall time or of level C `total_ms` grows by more than 25%. Across
   machines these are printed, never judged.

Never a regression by itself: the `grep` arm (measured and reported, never a gate), the
`lookup` proxy (reported; a drop is a question, not a verdict), chunk counts, and any
interval that contains zero. An improvement is claimed only by rule 1 mirrored (interval
entirely above zero), with the W/L/T and the unpaired count beside it.

The workflow prints the verdict in the job summary and does not fail on a regression
(Open decisions, D1).

## 7. Determinism

Level C is exact by construction: the fake embedder is a pure function of the text,
the lexical channel is ripgrep, the reranker and LSP are off. Check (pre-registered): the
same build run twice must produce identical evidence for every task, and `compare` must
show all ties. Level D is not bit-reproducible (batching changes float sums), so a level D
claim needs its interval as above, never one task.

## 8. Open decisions

- **D1:** the workflow reports a regression verdict but does not fail the job. Flip with
  `bench compare --fail-on-regression` once the owner wants the schedule to gate.
- **D2:** level B `defs` and `navigation` stay `unavailable` until a build exposes
  `sym.defs` and the runner has language servers (section 2.1).
- **D3:** level C reads `evaluate` reports, not RunRecords, until the S4 harness reaches
  this lineage (section 2.2).
- **D4:** queries come from commit messages only, not linked issues.
- **D5:** the stored baseline is updated only by a commit that says so, never by the
  workflow.
