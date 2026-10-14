# M0b pre-registration: fused and reranked LCI on the `impact`, `where-to` and `flow` gold sets

Slice M0b of the multimap spike v2: the deferred half of M0.3. M0.3 measured today's LCI with only
the lexical channel (`m0.3-prereg.md`, `m0.3-results.md`); this slice adds the model channels.
Written before the first run on the gold items; the commit that adds this file precedes the commit
of any result. Scorers: `cargo xtask gold` (`gold-scorers.md`). Results go to
`docs/spike/multimap/v2/m0b-results.md`.

Labels: [M] measured, [I] inferred, [G] guess. Nothing in this slice is a gate: the MVP has no
viability gates, so a number here is data about the order of work, not a verdict.

## 1. Question

With the semantic channel and the reranker switched on, how much more of the three structural
questions does LCI answer than with the lexical channel alone, and how does it stand against the
trivial baselines the gold sets ship with?

This is a **retrieval ablation** on a frozen tree: ranked chunks in, gold scorers out. It is not an
agent result. It says nothing about whether an agent given these tools solves tasks better, about
tool-call behaviour, or about latency under real use (latency is recorded as context only,
section 3). "F", "FR" and "S" below always name the ablation arms of section 3.

## 2. Frozen inputs

Everything from M0.3 is kept identical, so the lexical arm is comparable; only the channels (and
the index needed to serve them) change.

| input | identity |
| --- | --- |
| Indexed root | `<tree>` at commit `bfaff446316636e2089e0440bfcf127394950c42`, as in M0.3. |
| Gold | `<gold>/impact.json` sha256 `8c1182136ea530ee737f1780e800b3afc3e33bbcb97020fddbd7956cebb52314` (141 primary items); `<gold>/flow.json` sha256 `2c3a514c1e325225f3fb88038db1c4eae495cd9e29d33215c1f424c9b25ed62a` (111 items, 55 headline); `<gold>/where-to.json` sha256 `5f0f0e7fd8094ece73a5f9fa25e5c92c2b305f7f9fc12a20d777dea4546903a8` (59 items, the accepted file M0.3 used). Total 311 queries per arm. If a hash differs at run start the run stops: L (reused) would no longer be comparable. The gold is then re-pinned in a new pre-registration, or L is re-run and labelled. |
| Queries, windows | The item `question`, verbatim; `top_k` 20 for `impact`, 10 for `where-to` and `flow` (as M0.3). |
| Scorers | `xtask/src/gold/`. `git diff f840513 2b8db07` is empty for `src/`, `lci-core/`, `xtask/`, `Cargo.toml` and `Cargo.lock` [M], so M0.3's LCI and scorers are this slice's LCI and scorers. |
| LCI | A release build of this branch at the commit that adds this file, used through `lci evaluate`. |
| Converter | **C1**, unchanged from M0.3 section 3 (`impact`/`where-to`: one `{path, line = start_line}` per result in rank order; `flow`: the `fn` identifiers inside each result's span, in rank order and source order). The rank used is the evaluate report's final order (after reranking for FR). |
| Reference arms | `grep_baseline` (`cargo xtask gold baseline --set impact`) and the file-level BM25 (the gold's own `where-to` algorithm, also applied to `impact`), exactly as in M0.3; both reused from M0.3, not re-run. None exists for `flow`. |
| L | M0.3's lexical-only result (reports, run files and scores), reused. See the identity check in section 3. |

State of the gold sets: `impact` v2 with a fresh independent audit still owed; `where-to` accepted
at its gold section 12.12; `flow` v2 audited, descriptive wording unread by a second person. These
caveats carry over from M0.3 and are not repeated per number.

## 3. Arms

All arms: `index.freshness = "manual"`, LSP off, every other key at the built-in default unless
listed, no local overlay (`--no-local-config`), no `LCI_*` variables, one `lci evaluate` definition
per set made by `cargo xtask gold to-evaluate`, as in M0.3.

**LSP is off in every arm.** M0.3 had it off, and the arms must differ from L only in the model
channels. Whether a language server adds anything is a separate question [I]; it also starts a
server per workspace and adds a start-up latency unrelated to the models.

| arm | semantic | lexical | lsp | reranker | what it is |
| --- | --- | --- | --- | --- | --- |
| L | off | on | off | off | M0.3, reused |
| F | on | on | off | off | RRF fusion of semantic and lexical, fusion order returned |
| FR | on | on | off | on | F, then the reranker on the shortlist: the product's search path minus LSP |
| S | on | off | off | off | semantic channel alone; **diagnostic**, no reading rule depends on it |

Shared settings, from the shipped `config.example.toml` unless said otherwise:

* Embedder: model id `qwen3-embedding-0.6b` through the OpenAI-compatible embeddings endpoint at
  `http://127.0.0.1:8766/v1`; prompt templates, batch size and the semantic candidate count at
  their defaults. Reranker: Qwen3-Reranker-4B, Q8_0 GGUF served by llama.cpp at
  `http://127.0.0.1:8767/rerank` (`reranker_model = "qwen3-reranker-4b"`).
* `rerank_candidate_count = 24` and `rerank_document_max_bytes = 4096`. These equal the shipped
  defaults and the A/B's frozen configuration (read from its configuration file at the time of writing), so
  M0b and the A/B describe the same product [I]. `rrf_k = 60`, all fusion weights 1.0,
  `max_chunks_per_file = 2`, `lexical_candidate_count = 40`, `semantic_candidate_count = 40`,
  `reranker_timeout_seconds = 240` (the A/B's value; the shipped default is lower), `embedding_timeout_seconds = 120`. The `top_k` windows (20, 10,
  10) are within the 24-candidate shortlist.
* The shipped example names a different embedder (`qwen3-embedding-4b`); this slice uses the
  0.6b model named above because that is the A/B's frozen embedder. If the A/B's frozen embedder
  or any of the keys above differs from what is written here, the A/B's value wins, the results
  document states the difference, and this paragraph is not edited after the first run.
* **New index.** M0.3's index was built with a stub embedder whose vectors were never read; it
  cannot serve the semantic channel and its embedding identity differs. M0b indexes `<tree>` once
  with the real embedder into a fresh data directory `<scratch>/data-m0b` (outside the repository
  and outside the user's data directory). All arms use that one index. Chunking does not depend
  on the vectors, so chunk boundaries should equal M0.3's [I]; the check below tests this.
* **L identity check (pre-registered, not an arm).** Before F, FR or S, the M0.3 lexical-only
  configuration is run once over all 311 queries on the new index (no model service is called at
  query time). If every ranked chunk list equals M0.3's, L is valid as reused. If any differs, the
  differences are reported, that check run becomes **L'** (a full arm, labelled), and every
  comparison "against L" is made against L' and also reported against L.
* **Order of runs:** index, L check, S, F, FR. Queries run one at a time (no parallel queries, so
  reranker timings are not inflated by self-contention). The model services belong to a shared
  machine, so recorded timings are [M] but are not a latency claim; the service load at run time
  is recorded as a note.
* **Recorded in the results document:** the model ids and quantization the services report, the
  service start commands or config with machine values replaced by placeholders, the embedding
  dimension, the chunk count, `reranked` and `warning` for every FR query, and per-stage timings
  from the evaluate report.

**FR validity and timeout rule.** The product fails open: when the reranker is unavailable or slow
it returns the fusion order with `reranked = false` and a warning (and after a refused connection
it skips the reranker for 30 s). A query is **degraded** if its FR report has `reranked = false`,
carries a warning, errors, or does not finish within 300 s of wall clock. Degraded
queries get exactly one retry pass after the main FR pass, for infrastructure reasons only (never
chosen by score). A query still degraded after the retry counts as a **miss**: an empty answer,
scored 0 by the scorer, in the primary FR numbers. The results report the count of degraded
queries before and after the retry, the first attempt stays in the record, and a secondary
column ("FR-as-returned") scores the fusion-order list a degraded query did return. If more than
16 of 311 queries (5%) remain degraded, FR is reported as "run invalid for FR" and no
FR-versus-anything reading is made until a new pre-registered run.

**Cost and time estimate** (planning only, no result depends on it):

* Queries: 311 per arm: L check 311 (about 0.1 s each [M, M0.3: 99 to 123 ms median], under a
  minute), S 311, F 311, FR 311 plus retries.
* S and F: each is a query embedding plus the ripgrep pass for F; about 0.3 to 1.5 s per query
  [G], so 2 to 8 minutes per arm.
* FR: the reranker scores up to 24 documents of up to 4096 bytes each per search. The A/B saw a
  median of about 30 s per search under agent load [M from the A/B, as reported in the
  request for this slice; not re-read here]. Unloaded the figure should be lower; the planning range is 5 to 30 s per
  query [G], so **26 minutes to 2.6 hours** for 311 queries, plus retries.
* Indexing the tree with the real embedder: 2 to 15 minutes [G], depending on chunk count and on
  other load on the embedding service.
* Total, serial: about 0.5 to 3.5 hours of wall clock [G]. The services are loopback only; no
  other listener is opened. The run starts only when no other lane uses the services (the
  coordinator checks this and records it).

## 4. Metrics

Exactly M0.3's, from the same scorers, per family; no metric is added.

* **impact** (141 primary items; strata `band`, `kind`, `coverage`): primary `decl_f1`; also
  `decl_precision/recall`, `decl_recall@5/10/20`, `decl_ndcg@10`, `file_f1/precision/recall`.
  Against `grep_baseline` on `decl_f1`, `decl_recall`, `file_f1`; against BM25 on file-level
  metrics.
* **where-to** (59 items; strata `form`, `leak`): registration `reg_hit@3` (primary), `reg_mrr`,
  `reg_decl_hit@3`; exemplar and convention `set_recall`, `set_precision`, `set_f1`. Against BM25.
* **flow** (111 items; headline 55): primary `path_recall` on the headline items; `spurious`,
  answers per item, and the share of items with every gold node named as context; the split
  shortcut-flagged / not flagged, and by `stratum`. `tau` and `exact_order` are not read.
* **Per item and intervals**: as M0.3 section 4. Per-item values by one-item `--partial` scoring,
  checked against the full-run means. 95% Wilson for 0/1 per-item metrics (`reg_hit@3`, share of
  flow items with `path_recall` = 1), else percentile bootstrap, 10,000 resamples, seed 20261014;
  flow headline resampling by `prefix_cluster`, cluster count printed.
* **Paired differences**, all by the same bootstrap over per-item differences (flow: over
  clusters; 0/1 metrics: also with the McNemar counts of items gained and lost printed):
  F minus L; FR minus L; FR minus F; S minus L and S minus F (descriptive); F and FR against
  `grep_baseline` (`impact`) and against BM25 (`where-to`, and `impact` file level).
* Every table prints `n`. A cell with fewer than 10 items is printed and labelled "too small to
  read".

## 5. Reading rules

Fixed here so the reading does not depend on the numbers.

1. **Inherited from M0.3:** rules 1, 3, 4, 5, 6 and 8 to 9 of `m0.3-prereg.md` section 5 apply
   unchanged (n and interval beside every number; `impact` by name-ambiguity band first; `flow`
   headline first and structure claimed only from not-flagged items; `where-to` by `form`, leak
   strata kept apart; C1 caveat and file-level metrics as the more direct reading; no setting
   changed after the first full run; results [M], explanations [I] or [G]).
2. **"X beats Y"** on a metric needs both: the paired interval of X minus Y excludes 0 on the
   positive side, and the point difference is at least a floor of 0.05 for a mean metric or 3 of 25
   items (0.12) for `reg_hit@3` [G, a practical size set before seeing data]. An interval above 0
   but below the floor is read "small gain". An interval that includes 0 is read "no evidence of a
   difference". Symmetric for "worse". Separate-interval overlap is never used.
3. **Primary comparisons** (the only ones that carry a stated conclusion; all others are
   descriptive and carry no verdict):
   * `impact`: `decl_f1` of F and of FR against L and against `grep_baseline`, in each band;
   * `where-to`: registration `reg_hit@3` and `reg_mrr`, and convention and exemplar `set_recall`,
     of F and FR against L and against BM25;
   * `flow`: headline `path_recall` of F and of FR against L, overall and in the not-flagged
     split; no reference arm exists for `flow`.
4. **"F (or FR) beats L" for a family** needs the rule-2 condition on that family's primary metric
   in the family's primary cell (impact: the `unique` or `common` band, whichever the rule is
   met in, named explicitly; where-to: registration `reg_hit@3` or convention `set_recall`;
   flow: headline). A gain only on shortcut-flagged flow items is called a name-matching gain.
5. **"F (or FR) beats the baseline"** needs the rule-2 condition against `grep_baseline` or BM25 on
   the metric of rule 3; for `flow` the statement does not exist.
6. **"The reranker helps"** in a family needs FR minus F to satisfy rule 2 on the family's primary
   metric in its primary cell; "hurts" is the mirror. Anything else is "no evidence of an effect".
   The result is stated on the ranking metrics (`reg_hit@3`, `reg_mrr`, `decl_recall@5/10`,
   `decl_ndcg@10`) as well as the window-level ones, because the reranker reorders a 24-candidate
   shortlist and the 20 or 10 returned chunks come from it [I]; a gain on the window-level
   metrics only would come from a changed selection, and is reported as such. The counts of items
   improved, tied and worsened are printed. Reranker cost is outside this rule; it is reported
   beside it.
7. **Fusion** (F against L and against S) is read only descriptively: whether F is at least as good
   as the better channel in each primary cell. The S arm never supports a claim by itself.
8. **A degraded FR query** is handled as in section 3; FR numbers always state the degraded count.
9. **Not an agent result.** Wording in the results: "retrieval ablation", "ranked chunks", never
   "the agent finds", "users get" or "the product solves". Nothing here supports a claim about
   what an agent does with the results.

## 6. Expectations (guesses, fixed before the run)

[G] The semantic channel rescues descriptive wording that has no word overlap, so:

* `impact`: F and FR stay far below `grep_baseline` on `unique` (baseline 0.995), with `decl_f1`
  not above 0.30; the smallest gap to the baseline, and any "beats", is on `common` (baseline
  0.421); file recall at least as high as L's 0.79.
* `where-to`: registration `reg_hit@3` of FR 0.80 or more (L 0.72, BM25 0.72); convention
  `set_recall` of F and FR above L and not clearly above BM25 (0.253).
* `flow`: headline `path_recall` of F and FR above L's 0.104, with the largest gain on
  `descriptive` items (L 0.023) and little on not-flagged headline items.
* The reranker improves ranking metrics a little (`reg_mrr`, `decl_recall@5`) and leaves window
  recall unchanged within noise.

An expectation that fails is reported as such; none triggers any action.

## 7. Known limits

* Inherited from M0.3 section 7: C1 is a lossy chunk-to-declaration view (it under-credits
  declaration metrics and can inflate `flow` `spurious`); one tree, one commit; name leakage and
  lexical shortcuts apply to every arm; gold audits still owed.
* The reranker sees chunks cut to 4096 bytes and a window of 24; a query whose evidence is in a
  chunk ranked 25 or lower after fusion cannot be rescued by it.
* The per-file cap (2 chunks per file) applies in every arm including L, so the arms differ in
  channel mix, not in cap.
* The embedder and reranker are specific models and quantizations; a different pair is a
  different experiment. S and FR numbers are not claims about "semantic search" in general.
* Services run on a shared machine; timings and, if a service is under memory pressure, degraded
  rates may be higher than on a quiet machine. Reranker output could in principle vary slightly
  between runs; no repeat run is planned, so run-to-run variation is not measured [I].
* No latency is claimed. LSP is not measured.

## 8. Deviations and disclosure

* At the time of writing no query has been run against the gold items for this slice, no server
  was started, no model service was called, and no build was done. M0.3's results were read for
  their numbers, which the expectations in section 6 quote.
* The A/B's frozen configuration and its median reranker time were taken from the request that
  commissioned this slice, not from a document in this repository. The results document states
  what the services actually reported.
* Any later deviation is listed in the results document with the reason. The first full run
  stays in the record; a repeat is allowed only if a run was invalid (crash, wrong tree, wrong
  gold hash, services down for most of the run), never on the basis of a score.

## 9. What this can change

Nothing here is a gate. The result changes the order and the base of the multimap work:

* **FR beats L on `where-to` and `flow` but not on `impact`** : the multimap work
  builds on the FR search path as its retrieval base for locating and tracing questions, and
  `impact` is left to structural maps, since neither channel nor reranker closes the gap to
  `grep_baseline`.
* **FR beats L on all three families**: the same, and the structural maps are measured as
  additions to FR rather than to L.
* **FR does not beat L (or F) anywhere, or the reranker hurts**: the A/B's search result is already
  the product baseline and carries the same limits; expansion by structural maps before the
  reranker is then the main lever (spike lesson L12), and the model channels are not where to spend
  the next slice.
* **F beats L but FR does not beat F**: the reranker's cost is not justified for these three
  question types; the maps are built on F, and the reranker remains a per-profile choice.
* **S beats F in some primary cell**: reported as a finding about the fusion (weights, caps) to
  look at, not acted on in this slice.
* Whatever the outcome, the numbers become the "product baseline" column that M0.4 (the scored
  walk on the flow gold set) and later structural arms are read against. No outcome here supports
  a claim about agent performance.

## 10. Run

```text
cargo xtask gold to-evaluate --set <set> --gold <gold>/<set>.json --out <def>.toml --top-k <k>
lci --config <scratch>/lci-m0b-<arm>.toml --no-local-config evaluate <def>.toml \
    --workspace self=<tree> --output <report>-<arm>.json
<convert report -> run file, C1>
cargo xtask gold score --set <set> --gold <gold>/<set>.json --run <run>.json --out <score>.json
```

One configuration file per arm (L check, S, F, FR); they share `data_dir = "<scratch>/data-m0b"`
and differ only in `[channels]`. Channels never change the index, so no re-index happens between
arms. Scripts, configurations and raw reports stay in the scratch directory; only the two
documents of this slice are committed.
