# Spike bench: comparing frozen builds

Status: **v1 design.** Written 2026-10-12 (`date -u` 17:09Z, revised 17:14Z after the
independent review) on `cloud/spike-bench`, stacked on `cloud/spike-s1s2` (`b87b16a`).
Labels as in [specs/](specs/README.md): **Read** = in the code or docs at that tree;
**Inferred** = designed here. Everything below that names `cargo xtask bench` is Inferred
until the commit that adds it.

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
| A | Contracts: T0 (fusion bit for bit through the map registry, `src/app/search/tests/t0.rs`), the contract kit, fail-open. Already in `cargo xtask test --suite Full` (Read). | CI, every push | The map layer reproduces today's fusion exactly; maps fail open. | Anything about quality or cost. |
| B | Structural: index cost and size, update cost, search latency, and symbol lookup against commit-derived gold. | Cloud or CI, fake embedder | A build indexes or updates more slowly, stores more, or finds a changed declaration's file less often by name. | Retrieval quality for natural-language queries; LSP navigation (see 2.1). |
| C | Retrieval with the deterministic fake embedder (2.3), no reranker, no LSP: a `fusion` arm (semantic plus lexical) and a `grep` arm (lexical only). | Cloud or CI | A change to chunking, the lexical channel, fusion, priors, filters or the plumbing between them moved file-level ranks on the set. | Semantic quality: the fake embedder is a hashed bag of words, not a model. |
| D | Level C's `fusion` arm with a real embedder (Qwen3-Embedding-0.6B on CPU), no reranker. | Once, by hand, in a container with the model | File-level retrieval of that build with a real (small) embedder on the stated set or subset. | Production quality (production uses a larger embedder and a reranker); latency of the owner's machine. |

### 2.1 Level B in v1 (Inferred)

The brief names `sym.defs` and navigation precision/recall. At `b87b16a` neither has a
surface a frozen binary can be asked through: `sym.defs` is built on `cloud/spike-s3a`
(not on this branch's base) with no CLI, and navigation needs a language server, which the
cloud runner and CI do not have. v1 therefore measures:

- `index`: full index wall time, stored chunk count, and data directory size, per
  repository at the first task's base commit.
- `update`: wall time of `index` after moving the checkout from a task's base commit to
  its fix commit (exactly the fix's diff), per task, with the binary's embedded and
  reused chunk counts.
- `latency`: per-query `total_ms` from level C (p50, p95, max), per arm.
- `lookup` (a proxy, labelled as such): for every gold symbol, a search for its bare name
  in the `fusion` arm; over the top 5 *distinct files*, precision@5 = share of them in
  which the fix changed that symbol, recall@5 = share of gold symbols with such a file in
  the top 5.
- `defs` (added 2026-10-12 on `cloud/spike-bench-runs`, before any `defs` result
  existed): `sym.defs` (built on `cloud/spike-s3a`) is registered for tests and the
  contract kit only, so no frozen binary can be asked. `cargo xtask bench freeze-defs
  --src <tree> --out <probe>` builds a small probe (`xtask/bench-defs-probe/main.rs`)
  against the `lci-core` and `lci-maps` of the frozen commit's own tree, resolved
  offline from a copy of that tree's `Cargo.lock` (cargo prunes the entries the probe
  does not use, so `--locked` cannot be passed; a probe whose lock names a version
  missing from the tree's lock is not that tree's); `bench run --defs-probe <probe>`
  runs it once per task at the
  base commit. The probe scans the checkout with the core scanner, builds `sym.defs`
  from the declaration bundles (its bundle store persists across a repository's tasks,
  so unchanged files reuse their bundles) and asks the map, through its own ranking
  (`symdefs::answer`, up to 50 hits), for each gold symbol's bare name. Scoring is the
  lookup proxy's: over the top 5 distinct files, `hit5` (a file whose changed
  declaration has that name) and `precision5`. A name the probe does not answer, or a
  task whose probe failed, is counted as an error, never as a miss. Results also carry
  the probe's scan and build time and the reused/extracted bundle counts. This
  measures the map's library code at the frozen commit, not a production search path
  (no production search runs `sym.defs` at `b466f7f`). Reported only: no
  pre-registered rule judges `defs` (section 6), so a drop is a question, not a
  verdict.
- `navigation`: recorded as `unavailable` with the reason (the results' `unavailable`
  block; `defs` is listed there too when no probe was given). It switches on when the
  runner has a language server; the gold is already in the set (section 4).

Latency in B is wall clock on whatever machine ran it; it is judged only under rule 4
(section 6).

### 2.2 Level C harness (Inferred)

The RunRecord harness (S4 offline part) is on `cloud/spike-s3b`, not on this branch's
base. Level C v1 drives the frozen binary's existing `evaluate` command (Read:
`src/main.rs`, `Command::Evaluate`) with a generated one-workspace definition per task,
`top_k = 10` set explicitly (the default is 8), and reads only
`queries[].evidence[].relative_file_path` and `queries[].timings.total_ms` from its
report. **Switch note:** once the harness reaches this branch's lineage, level C should
read RunRecords instead; the scoring rules in section 5 stay.

### 2.3 The fake embedder (Inferred)

The fake model service in `tests/support/mod.rs` (Read) returns one constant vector for
almost every text and lives inside the integration tests, so no frozen binary can reach
it. The bench brings its own: an OpenAI-compatible `/v1/embeddings` and `/v1/models`
service that `cargo xtask bench run` starts in process on `127.0.0.1` with an ephemeral
port (never 8765-8770), pointed to by the run's scratch configuration. The vector of a
text is fixed by this definition, so two machines produce the same vectors:

- when the text starts with `Instruct:` (the production query template), strip it up to
  and including the first `Query: `; any other text is embedded whole;
- split into words at every non-alphanumeric character and at camelCase boundaries,
  lowercase, keep words of two or more characters that are not all digits;
- each distinct word adds `1 + ln(count)` to dimension `h mod 512` with sign `+` when bit
  63 of `h` is clear, else `-`, where `h` is the 64-bit FNV-1a hash of the word's UTF-8;
- L2-normalise; a text with no words gets the unit vector on dimension 0.

## 3. Producing base and new builds (Inferred)

Follows `.agents/skills/lci-retrieval-evaluation/references/evaluation-workflow.md`
step 3 (freeze the binaries), debug profile as there:

```bash
git checkout <base-commit>
cargo xtask bench freeze --out <scratch>/lci-base    # builds, copies the binary, records the commit
git checkout <new-commit>
cargo xtask bench freeze --out <scratch>/lci-new
cargo xtask bench run --bin <scratch>/lci-base --work <scratch>/run-base --output <scratch>/base.json
cargo xtask bench run --bin <scratch>/lci-new  --work <scratch>/run-new  --output <scratch>/new.json
cargo xtask bench compare <scratch>/base.json <scratch>/new.json
```

For level B `defs` on a tree with `sym.defs` (2.1), freeze the probe from the same
checkout and pass it to that build's run:

```bash
cargo xtask bench freeze-defs --src <checkout of new-commit> --out <scratch>/defs-new
cargo xtask bench run --bin <scratch>/lci-new --defs-probe <scratch>/defs-new ...
```

`freeze` writes `<out>` (the binary) and `<out>.json` (commit, dirty flag, profile);
`freeze-defs` writes the same two files for the probe and leaves its package (manifest,
lock, and `target/` unless `CARGO_TARGET_DIR` is set) in `<out>.src/`.

A commit without `bench freeze` (a spike head) is frozen by hand the same way: in its own
worktree, `cargo build --locked --bin local-code-intelligence`, copy the binary, write
the same sidecar. Never point two worktrees at one `CARGO_TARGET_DIR` without first
touching every file of the second: cargo keys a workspace crate on its relative path and
file times, so it can reuse the first tree's build for the second (seen 2026-10-13,
caught by comparing the binaries' hashes).
Each run has its own work directory: one checkout per repository (a `git worktree` of a
cached clone) and, as its sibling and never inside it, one data directory per
repository. Freshness is `manual`, so no search refreshes an index; the runner calls
`index` explicitly before each task. Runs are sequential.

A repository's tasks are replayed oldest first in one checkout: for each task, check out
its base commit, `index` (incremental from the previous state), search, then check out
its fix commit and `index` again (the `update` measurement). This relies on the
incremental-equals-full contract (S1 tests it); the check in section 7 guards it.

The scheduled workflow (`.github/workflows/spike-bench.yml`) freezes only the head build
and compares it with the stored baseline results file in
`docs/spike/multimap/bench/results/` (the brief asks for stored results there; the
committed file is evidence data like the bench set, not a `test-results/` report).
Level C is deterministic by construction, so a stored file stands in for a rebuilt base
on quality; cost against a file from another machine is printed, never judged.

## 4. Bench set v1

Data only, in `docs/spike/multimap/bench/bench-set-v1.json`: per repository its URL,
licence and snapshot commit; per task the fix commit, its parent (the base commit the
search runs on), the query text, the gold files and the gold symbols. No code from those
repositories is stored. Mined by `cargo xtask bench mine` (filters recorded in the file's
`filter` block, including the extension list fixed at mining time, and in
[bench/README.md](bench/README.md)).

- Replayed commit: the search runs on the fix commit's parent, so the gold files exist
  and hold the code before the fix (unlike `mine-queries`, which searches `HEAD`).
- Query text: the commit message (subject, plus the first body paragraph when it adds
  words). Linked issues are not fetched (no API calls in the miner; Inferred,
  conservative). A query that names a gold file stem or has an identifier shape is
  tagged `keyword`, one that names a gold symbol `names-symbol`.
- Gold files: the supported-extension files the fix modified, excluding test files;
  fixes that add, delete or rename a non-test source file are skipped.
- Gold symbols: per hunk of `git diff -U0 base fix` of each gold file, the declaration
  on the first changed code line if there is one, else (an indented change inside a
  body) the name in git's built-in `rust`, `golang` or `python` funcname hunk header,
  selected through a temporary `core.attributesFile` (never a file inside the
  checkout); comment-only hunks, top-level non-declaration hunks and names under three
  characters give none. The bare name is the identifier after `fn` (first), else
  `struct`/`enum`/`trait`/`union`/`type`/`const`/`static` (Rust), after `func` and an
  optional receiver or after `type` (Go), after `def`/`class` (Python). Approximate by
  construction (the header is the nearest declaration above the hunk). Not derived from
  a SCIP or LSP index, nor from LCI's own parser (lesson L8): the gold must not share a
  source with any map it scores.

## 5. Metrics and scoring (per arm, per task)

From the ranked evidence of one search (top 10), collapsed to distinct files in rank
order:

- `rr`: 1 / rank of the first gold file (0 if none in the top 10). **Primary.**
- `hit1`, `hit5`, `hit10`: a gold file at rank <= 1, 5, 10.
- `recall10`: share of gold files in the top 10.

Aggregates: means over tasks; per-repository and per-tag breakdowns. A task the binary
failed on is an error: it is never scored as zero and never silently dropped.

Comparison of two results files (`bench compare`): tasks are paired by task id and arm;
a pair needs a score on both sides, so errored and unpaired tasks are excluded from every
pair and counted. Deltas are new minus base. For `rr` it prints both means, the mean
paired delta and a 95% percentile interval from a paired bootstrap (10,000 resamples of
paired tasks, fixed seed 42), and wins, losses and ties; `hit10` and `recall10` get the
same table as secondary, unjudged metrics.

## 6. Pre-registered regression rules

Written before any result exists. In a `compare` verdict:

1. **Quality (level C `fusion` arm; level D, see below):** regression when the 95%
   interval of the mean `rr` delta lies entirely below zero. One primary metric only,
   so one test per verdict.
2. **Coverage:** regression when a task that scored on base errors on new.
3. **Index size (level B):** regression when the total data directory size grows by
   more than 10%.
4. **Cost (level B):** regression when p50 of `update` wall time or of the `fusion`
   arm's `total_ms` grows by more than 25%, judged only when both results files carry
   the same `machine` fingerprint (CPU model, logical cores, OS, architecture, ripgrep
   version) and both runs started within 6 hours of each other (back to back);
   otherwise printed, never judged.
5. **Insufficient data:** with fewer than 30 paired tasks in an arm, rule 1 and its
   mirror give "insufficient data", not a verdict.

An improvement is claimed only by rule 1 mirrored (interval entirely above zero), with
zero errored and zero unpaired tasks on either side, and the W/L/T beside it.

Never a regression by itself: the `grep` arm (measured and reported, never a gate),
`hit10` and `recall10`, the `lookup` proxy (a drop is a question, not a verdict), the
`defs` probe (same), chunk counts, and any interval that contains zero.

Level D: a same-build rerun comes first. Its interval must contain zero (the noise
floor, as for the reranker-off semantic arm in the evaluation workflow) before a
cross-build level D interval is read.

The workflow prints the verdict in the job summary and does not fail on a regression
(Open decisions, D1).

## 7. Determinism checks (pre-registered)

- Level C is exact by construction: the fake embedder is a pure function of the text,
  the lexical channel is ripgrep, the reranker and LSP are off. The same build run twice
  on one machine must give identical evidence for every task, and `compare` all ties.
- Incremental equals full: for a sample of tasks, a fresh full index at the task's base
  must give the same evidence as the replayed incremental index.
- Across machines: the first workflow run on the baseline's own commit must compare all
  ties against the stored baseline; until that has happened, a workflow quality verdict
  is provisional.
- Level D is not bit-reproducible (batching changes float sums); see rule 1's level D
  note.

## 8. Approvals

Given by the owner in chat on 2026-10-12 for this work only: cloning public repositories
to mine tasks; adding the one workflow file `.github/workflows/spike-bench.yml`
(schedule plus manual dispatch); downloading Qwen3-Embedding-0.6B from its official
repository and running it as a CPU embedding service inside the cloud container, for
level D only. Anything beyond that (another model, a service on the owner's machine, a
gating workflow) needs a new approval.

## 9. Open decisions

- **D1:** the workflow reports a regression verdict but does not fail the job. Flip by
  adding `--fail-on-regression` to its `bench run` step once the owner wants the
  schedule to gate.
- **D2:** level B `navigation` stays `unavailable` until the runner has language
  servers; `defs` is measured through the `sym.defs` probe (section 2.1) on builds whose
  tree has `lci-maps/src/symdefs.rs`, and is `unavailable` on the others.
- **D3:** level C reads `evaluate` reports, not RunRecords, until the S4 harness reaches
  this lineage (section 2.2).
- **D4:** queries come from commit messages only, not linked issues.
- **D5:** the stored baseline is updated only by a commit that says so, never by the
  workflow.
- **D6:** one checkout per repository replayed in order (section 3) instead of one per
  task, to keep the run within CI time; guarded by the incremental-equals-full check.
