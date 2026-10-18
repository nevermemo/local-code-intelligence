# Gold scorers (`cargo xtask gold`)

Scorers for five local gold sets: `impact`, `flow`, `where-to`, `history` and
`instructions`, and a sixth, `traits` (added 2026-10-18, scored on its own and
not part of `maps bench --set all`). The gold files are not part of this repository; every command
takes them with `--gold <path>`. Nothing here embeds gold content, and the unit
tests use small synthetic items.

```
cargo xtask gold score       --set <impact|flow|where-to|history|instructions|traits> --gold <path> --run <path> [--out <path>] [--partial] [--strict]
cargo xtask gold baseline    --set <set> --gold <path> [--kind shipped|oracle|constant] [--out <path>]
cargo xtask gold to-evaluate --set <set> --gold <path> --out <path> [--workspace self] [--top-k 10]
```

Gold file: a JSON array of items, each with a string `id`. A file with one JSON
item per line (the `traits` question files are) is read as well: it is taken as
lines when its extension is `.jsonl` or when the text is not one JSON value. A
scorer reads only the fields named below; extra fields are ignored.

## Run file

A run is one JSON object mapping an item id to a ranked array of answers (best
first):

```json
{ "impact-0001": [ {"path": "src/a.rs", "line": 120}, "src/b.rs" ] }
```

Each answer is a string or an object. Object fields, all optional:

| field | meaning |
| --- | --- |
| `path` (alias `doc`, `file`) | repository-relative path, `/` separators |
| `line`, `end_line` | one line, or a span (`start_line` is an alias of `line`) |
| `name` | symbol or function name, `Owner::name` or `module::name` |
| `symbol` | the SCIP symbol string, when the system has one |
| `commit` | commit hash, full or a prefix of 7 or more hex digits (history) |
| `files` | history only: files the answer says the commit touched |

String shorthand: `path`, `path:line`, `path:start-end`, a name, or (history) a
hex commit. A string that has a `/`, or a `.` and no `::`, whitespace or `(`,
is a path; other strings are names.

An item missing from the run is scored as an empty answer (0) unless
`--partial` is given, which scores only the items present. The report lists
`run_ids_not_in_gold`.

## Report

`score` writes (stdout or `--out`):

```
{ set, gold_items, excluded_from_population, scored_items, items_missing_from_run,
  missing_scored_as_empty, run_ids_not_in_gold, strict,
  overall: { n, metrics: {name: mean}, n_defined?: {name: k} },
  strata:  { <dimension>: { <value>: { n, metrics, n_defined? } } } }
```

`n` is the number of items in the cell. A metric that is undefined for some
items of the cell (for example tau on one-node answers, or `file_recall` when
no answer lists files) is averaged over the items where it is defined, and
`n_defined` gives that count. Means are rounded to 4 places.

## Metrics

Shared primitives (in `xtask/src/gold/metrics.rs`): precision (0 when nothing
was returned), recall (1 when the gold is empty), F1, hit@k, reciprocal rank
with a cut-off, binary nDCG@k, and Kendall tau (concordant minus discordant over
pairs).

**impact** (the `primary: true` items only; strata `kind`, `band` = name
ambiguity, `coverage` = test coverage label). A returned declaration matches a
gold R1 declaration by `symbol`, by `path` plus a line inside the declaration
span, or by `path` plus the bare or full name (a name alone matches too). An
answer with a line, name or symbol is a declaration; a path-only answer is a
file. Declarations in `accept.neutral_decls`, the seed's own declaration, and
files in `accept.neutral_files` are skipped; everything else not in the gold is
wrong. Metrics: `decl_precision`, `decl_recall`, `decl_f1`,
`decl_recall@5/10/20`, `decl_ndcg@10` (a repeat of an already-credited
declaration earns no gain), `file_precision/recall/f1` over distinct answered
paths against `gold.r1.files`; secondary recalls `r2_decl_recall`,
`tests_recall`, `tests_deep_recall`, `tests_lexical_recall`, `docs_recall`
(config items), reported only where the item has that gold.

**flow** (strata `stratum` = first part of the stratum, `headline`). An answer
names functions; it matches a gold node by `Owner::name`, by bare name when
that is unambiguous inside the item, or by `file:line`, case-insensitive
(backticks and a trailing `()` dropped). `path_recall` is the fraction of gold
intermediates named (multi-path: best alternate; config-flow: field plus
readers). `tau` and `exact_order` are for path items; `spurious` counts
answers that match no known node and are not in `accept.ignorable_nodes`;
`fanout_recall` is for dispatch items.

**where-to** (strata `form`, `leak`). Registration items: `reg_hit@3` and
`reg_mrr` (rank cut at 10) of the first expected file, and `reg_decl_hit@3`
for answers with a line. Preferred, documentation, definition, related and
neutral sites are skipped when ranking unless `--strict`. Exemplar and
convention items: `set_recall`, `set_precision`, `set_f1`; an entry that
matches no member lowers precision, a line inside an implementation (or its
equivalent) matches it, and a path-only answer matches when exactly one
implementation lives in that file.

**history** (stratum `family` = H1, H2, H3a, H3b). `mrr` of the first accepted
commit (H1/H2: `gold.commit`; H3: `accept.mrr_commits`), `hit@1/2/3/5/10`, and
`file_recall` against `gold.files` (with `accept.alt_files` as the same file
under its other name) when answers carry `files`. Commits match by prefix.

**instructions** (strata `subtype`, `stratum`). I1: `i1_section_precision`,
`i1_section_recall`, `i1_section_f1` (a returned span counts when it lies in a
gold section of the same document), and `i1_path_specific_recall` over the
sections whose scope is `path`. I2: `i2_hit@3_primary`, `i2_hit@3_core`
(overlap in the same document), `i2_required_recall@5`, `i2_required_recall`,
`i2_required_precision@5`, `i2_decoy_in_top3`. A path-only answer is the whole
document.

**traits** (strata `template` = Q1, Q2, Q3; Q2 also `kind`). The items are the
questions of the trait-implementation set, one record per question with its
gold (`set` names the template). Gold entries carry `stratum`
(explicit, blanket, macro or derive) and spans `{file, begin_line, end_line}`:
`impl` (the impl block; a macro impl: the call site, as the gold records it) and
`type_decl` (null for a blanket impl) for Q1, `location` (the member; a macro
impl: the call site) for Q2 and Q3.

- *Q1* "which types implement `path::Trait`": an answer credits an
  implementation when its line falls on the impl span or on the type declaration
  span (either credits). Budget k = number of gold implementations: the first k
  counted answers are scored (the scorer cuts, so an arm may return more).
  `set_precision`, `set_recall`, `set_f1`.
- *Q3* "what must change if `Trait::m` changes": the same, with the member
  `location` as the only credit span.
- *Q2* "where is `Trait::m` implemented for `Type`": the rank of the first
  counted answer on the gold `location`: `hit@1`, `hit@10`, `mrr` (cut 10).
  `counted@10` is the number of counted answers after exclusions, capped at ten;
  bench precision divides `hit@10` by this count (zero when none are counted).
- *Matching.* An answer is a located node (`path` and `line`); one without
  either is returned but matches nothing. Answers are distinct by path, line,
  `symbol` and `name`, so the nodes of one macro call site stay apart (put the
  node id in `symbol`). One answer credits at most one gold implementation and
  each implementation is credited once (maximum matching): a call site that
  stands for N implementations needs N distinct answers. Precision is matched
  answers over counted answers; an empty answer scores 0.
- *Neither hit nor miss*, and not charged to the budget: an answer on the
  trait's own declaration (the item's optional `trait_decl` span, which should
  cover its method declarations), on an optional `neutral` span, or on an
  optional `cfg_gated` span (an implementation the gold's build did not compile;
  counted apart as `cfg_gated_answers`, mean per item). Gold whose impl site or
  any credit span overlaps a cfg-gated span is removed from k and the recall
  denominator; answers on any of its spans are also dropped. Retained gold wins
  over other ignored spans. The report's `traits` block says how many items carry
  `trait_decl` and `cfg_gated`: without them nothing is dropped, and the trait declaration card
  counts as a miss. When no item has a `trait_decl`, the scorer warns once on
  stderr and the report has `warnings: ["no trait_decl spans: the trait
  declaration card counts as a miss"]`.
- *Per kind.* `recall_explicit`, `recall_blanket`, `recall_macro`,
  `recall_derive` come from the same matching and are defined where the item has
  gold of that kind (`n_defined` says how many). **The kill rule's explicit-stratum
  recall is `recall_explicit`**: one matching over all the gold at k = number of
  gold implementations, recall over the explicit gold; it is also the bench
  headline column. `explicit_precision/recall/f1` are descriptive: they score the
  explicit gold alone (its own k; answers on the other kinds' gold are dropped as
  neutral). Precision has no kind: an answer is not typed.
- *Without the dominant macro call site.* `nodom_precision/recall/f1` drop the
  gold at the item's `dominant_site` and the answers that hit it (own k). The
  site is the macro call site that carries the most Q1 gold in the whole set (at
  least two entries; ties by file and line), written onto every Q1 item by
  `gold::prepare` (`score` and `maps bench` call it before dropping any item,
  so the site does not move with the items scored). It is undefined (null) when
  the set has no such site, or when a per-item scorer sees an item that was not
  prepared (`maps compare` rebuilds item records that way).
- *Bench.* `maps bench --set traits` reads `traits.json` (JSON lines allowed)
  in `--gold-dir`. Its adapter offers every located symbol for Q1 and only
  functions and methods for Q2 and Q3, each with its node id as `symbol`; the
  trait declaration card is offered like any other and dropped by the scorer.
  The row shows `set_f1`, `recall_explicit`, `hit@10`, `mrr`.
- *Q1 answer unit.* Producers must supply one answer per distinct impl or type.
  The plain scorer receives path/line/symbol answers and cannot infer impls:
  it only deduplicates identical answers, so distinct methods of one impl use
  separate budget slots. Every producer must collapse these before scoring;
  the bench adapter does this with `impl_unit`. In a Q1 walk a method
  (or other member) node counts as the impl that contains it: the adapter reads
  that impl from the container chain of the node id (`impl Tr for Foo` plus the
  module path; no span is needed) and collapses the duplicates before the
  scorer's budget cut, keeping the best ranked node's site. So an impl node plus
  two of its method nodes is one answer, one slot, one credit. Impls of
  different types at one macro call site have different headers and stay apart,
  one answer per distinct node. A type node and the methods of its impl are not
  merged (the ids do not tie them). Q2 and Q3 keep every member node.
- The overall cell mixes the templates; read `strata.template`. The
  `traits` set is private: the unit tests use tiny inline gold.

## Baselines

`baseline --kind shipped` (the default) rebuilds what the gold file itself can
reproduce:

- `impact`: the `grep_baseline` field stores only metrics, not the returned
  declarations, so no run can be built. The output is a metrics report in the
  `score` layout (metric names prefixed `grep_`), averaged per stratum.
- `history`: a run. H1 and H3 get the best constant commit; H2 gets the
  "gold sits at rank N of the file log" shortcut rebuilt from
  `accept.file_log_rank`, with placeholder entries that match no commit. The H1
  "file created by the gold commit" shortcut needs git history and is not
  rebuilt. `--kind constant` uses a constant commit for H2 as well.
- `instructions`: a run for the I1 items only: the root documents' sections that
  occur in some I1 gold. Recall equals the real root-only system's; precision is
  an upper bound. The I2 lexical baseline needs the rule corpus and is not
  rebuilt (score with `--partial`).
- `flow`, `where-to` and `traits` ship no baseline; use a run from your own system.

`baseline --kind oracle` answers every item with its own gold. Scoring it must
give 1.0 on every primary metric; it is a self-check of the scorer and of the
run schema.

## Conversion to `lci evaluate`

`to-evaluate` writes the TOML definition `lci evaluate` reads (see
`lci-core/src/evaluation.rs`): one workspace and one query per item, with
`expected_paths`, `disfavored_paths`, `top_k` and tags. The definition is
validated with the same function the evaluator uses before it is written, and a
summary of dropped items goes to stderr. The format is file-level, so:

| set | `expected_paths` | lost or dropped |
| --- | --- | --- |
| impact | `gold.r1.files`; decoy files are `disfavored_paths` | the non-primary items; declaration gold, R2, tests, docs |
| flow | files of the gold intermediates (config-flow: field and readers; dispatch: plus fan-out) | hop order, tau, spurious nodes, entry and target files |
| where-to | expected files, exemplar member files or convention implementation files; decoys as `disfavored_paths` | spans, neutral sites, precision |
| history | `gold.files` plus `accept.alt_files` | the commit (the real target) and the as-of cut-off; evaluate searches the checked-out tree, so a file that no longer exists cannot be found |
| instructions | rule documents (I1) or the primary units' documents (I2, other required documents as `preferred_paths`) | sections and units; these documents are outside the default index, so a score of 0 is expected for today's index |
| traits | files of the impl blocks and implementing type declarations (Q1) or of the member locations (Q2, Q3) | spans, kinds, the budget, the trait declaration |

Queries run against the workspace key given with `--workspace` (default
`self`); the evaluated tree must be the one the gold was built on for the
structural sets to mean anything.

## Reporting rules (2026-10-16, additive)

These rules change how results are read and which columns sit beside the frozen
ones. No gold file, item, split, stratum or metric definition changes, and no
earlier number is rescored. A new column is computed from per-item outputs that
already exist; anything that would need a bench rerun is listed under
"Deferred" below.

- **Bench supplement.** `cargo xtask maps bench` writes `supplement.md` beside
  `row.md`, `score.json` and `items.json` of a run directory (new file; the
  other outputs and the trend row are unchanged). It is computed from
  `score.json` and `items.json` and shows, per set, the secondary metrics and
  strata the one-line row leaves out, and a footer naming the ceilings below.
  A `--rescore` run with `--out-dir` writes one too, under its own wave label.
- **impact.** Read the `common` band (35 items), `r2_decl_recall` and
  `tests_recall` first; `decl_recall` stays as a regression column. R1
  declaration recall on unique names is saturated by a word-boundary grep plus
  the enclosing declaration (stored baseline: unique .995, some .79, common
  .42), so a walk or filter delta on a saturated band is not evidence. The
  supplement table shows these three metrics for every `band` value.
- **history.** The H3a, H3b and H1 rows are printed but marked "not used for
  decisions": H3 floors near zero by construction and H1 is circular with
  `exact.pickaxe`. No walk, bandit or filter question reads history.
- **flow.** Report answer precision (the share of returned answers that are not
  spurious; the `precision` column of the trend row and of `items.json`) and
  `tau` beside `path_recall` in every table, with the breadth-first reference
  line (BFS reaches 97 to 100% of the gold paths inside a 200-pop budget, M0.4)
  beside it. The supplement prints the mean precision. The `flow_path` recipe
  arm on the same 111 items needs a run, so it is deferred.
- **where-to.** No new items. The tantivy items (30) are a loss guard only: a
  delta there that is not a loss is not a gain claim.
- **Ceilings.** Impact R1, flow path recall and the error set's message-to-site
  question are near a ceiling. A table that shows one of them carries a footer
  saying so, and no delta on that metric is read as evidence without the
  baseline column beside it.

### Sets beside the frozen five

States are as of 2026-10-16; the tests-for-code and docs-for-code rows are possibly stale, so read the current state in
[../evaluation.md](../evaluation.md) ("Sets") and the results in [../results.md](../results.md).

| set | state | numbers | rule |
| --- | --- | --- | --- |
| error (34 items) | kept as a regression check, not a discriminating set | first scored 2026-10-16 by a stand-alone scorer (not the `xtask gold` path, tolerance 3 lines for grep): hit@10 for the stored fixed-string fragment .971, for the whole quoted message .735, for `lci search` on the question .50 and on the quoted message .59 (reranker off) | Report the grep columns beside search always; the message-to-site question is saturated by a fixed-string search, and search sits below it (an exact-message routing gap). Primary scoring should add the entry-point chain. It needs its own id, seed, hash and prereg before any further scoring, and enters `xtask gold` only through a reviewed change. |
| trait-dispatch (trait implementors) | not built (as of 2026-10-16) | 34 qualifying traits (at least 3 non-test impls and 3 non-test call sites) over three repositories, 33 of them in tantivy; bar 40 | Do not propose it again without a second repository that has traits. |
| co-change | parked | prior co-change hit@10 .456 against .500 for same directory by recent modification | The baseline equals the premise, so a set would not discriminate. |
| tests-for-code | frozen (as of 2026-10-16; possibly stale, see `evaluation.md` and `results.md`) | - | Added beside the five; scored on its own. |
| docs-for-code | being built (as of 2026-10-16; possibly stale, see `evaluation.md` and `results.md`) | - | Added beside the five when frozen. |

### Deferred (needs a run or a frozen-set edit, not done)

- The `rg -w` plus enclosing-declaration baseline column on every impact table
  (cited figures above are not re-measured here).
- The `flow_path` recipe arm on the 111 flow items.
- An entry-point chain scorer for the error set, and the error scorer in
  `xtask gold`.
- An independently audited error scorer and a rerun with the reranker on.
