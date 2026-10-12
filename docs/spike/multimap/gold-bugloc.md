# Bug-localisation gold set ("debug" family)

Generated: 2026-10-12T08:55Z (`date -u`). Source branch: `origin/fix/verified-bugs` at `bfbe51d`.
Data: [gold-bugloc.json](gold-bugloc.json) (34 items).

Each item is a real bug that was fixed on `fix/verified-bugs`. It is asked the way an agent or user would
meet it (a symptom), and the gold answer is where the fix landed. The tree to search is the fix's parent.

## Method

1. Candidates: `git log origin/fix/verified-bugs --format='%H %s' --grep='^N[0-9][0-9]*[a-z]*: '` gave
   39 commits. For each, the matching row of `docs/development/roadmap.md` (on that branch) and the commit
   body were read.
2. Kept a commit only if its diff changes non-test Rust source under `src/`, `lci-core/src/` or
   `xtask/src/`. A file counts as test code if it sits under a `tests` directory, or if all of its changed
   lines are inside `#[cfg(test)]`, `mod tests` or a `#[test]` / `#[tokio::test]` function.
3. Gold files: the changed non-test source files that exist at `base_commit`. A file the fix creates is
   not gold (an agent cannot find it in the base tree); it is named in `notes`.
4. Gold symbols: the functions and methods whose bodies or signatures contain a changed non-test line,
   found by a small brace-matching Rust scanner over the `-U0` hunks of both sides of the diff, and kept
   only if the function already exists at `base_commit`. Form: `Type::method` inside an `impl` (the type,
   last path segment) or a trait, `module::fn` for a free function (module = file stem, or the directory
   for `mod.rs`). Functions the fix adds are listed in `notes` ("Fix also adds"). Files that also have
   item-level changes (struct fields, constants, `use` lines) are listed in `notes`; those changes give
   no symbol.
5. Question: one or two sentences describing the symptom, written from the row text and the commit body.
   Hard checks (asserted by the build script): no gold file name, no gold symbol name as a whole word,
   no row id, no word "fix".
6. `name_leak`: true if any identifier token from the gold set appears in the question. Tokens are taken
   from gold file paths (directories and stem, without `src`, `lci-core`, `xtask`) and gold symbols, split
   on `::`, `_`, `/`, `.`, `-` and camelCase, lower-cased, length 4 or more. A question word matches a
   token if it equals it or adds `s`, `es`, `d`, `ed` or `ing`. The leaked tokens are listed in `notes`.
   Generic leaks were reworded away where it was cheap (for example "chunk" became "snippet"); the
   remaining 9 are domain words a reporter would use (`rust`, `language`, `java`, an LSP error name such
   as `RequestFailed`, a quoted error message such as "indexing failed while waiting").

Fields: `id`, `row`, `fix_commit`, `base_commit`, `question`, `gold_files`, `gold_symbols`,
`change_kind` (wrong-result, panic, hang-or-timeout, error-reporting, validation, resource, other),
`name_leak`, `notes`.

## Counts

| change_kind | items |
| --- | --- |
| wrong-result | 13 |
| error-reporting | 7 |
| other | 4 |
| resource | 3 |
| panic | 3 |
| hang-or-timeout | 2 |
| validation | 2 |
| total | 34 |

- Gold files: 65 in all, median 1.5 per item; 17 items have exactly one gold file.
- Gold symbols: 90 in all, median 2 per item, at most 10 (`bug-006`, a language-server session change).
- `name_leak`: 9 of 34 items.
- Two items cover two rows each, because one commit landed both (`N75, N76` and `N52, N51`); their
  questions describe both symptoms.

Dropped (5 of 39 candidates):

| Commit | Row | Reason |
| --- | --- | --- |
| `55cdbd9` | N17 | test-only change |
| `f5648a0` | N46 | test-only change |
| `49b2748` | N62 | test-only change (pins existing behaviour) |
| `c1d8d98` | N64-N66 | matched the grep through its body; CI and workflow changes only, no Rust source |
| `d7f77aa` | N3b | feature work (a new release endpoint and shutdown on Ctrl+C); most changed symbols are new, so there is no failing behaviour to localise |

## Verification

A fresh subagent (a different model from the one that built the set) got 9 items (25% of 34, rounded
up; drawn with `random.Random(20261012)`: bug-001, 002, 004, 017, 018, 025, 027, 033, 034) with only
`fix_commit`, `base_commit` and `question`. It re-derived gold files and symbols from the commits under
the same rules and judged each question for leakage and answerability.

- Gold files: 9 of 9 items identical. Gold symbols: 9 of 9 items identical (21 symbols). Agreement 100%
  on this sample; the scanner is not verified on the other 25 items.
- Leakage: "none" for all 9 (no file, symbol, row id or "fix"; domain words only).
- Answerability: 8 "yes", 1 "partly". bug-004 blamed declaration and usage lookups as well, but at
  `base_commit` those already returned an error for a crashed server; only the symbol lookup hid it.
  The question was narrowed to the symbol lookup; the gold is unchanged, because those two methods'
  bodies changed too (they gained the new warnings field).
- Noted, not changed: in bug-034 `src/store.rs` is a gold file without a gold symbol (the fix adds a
  new method there); in bug-017 three of the changed functions are new and therefore not gold.

Nothing was dropped at verification.

## Caveats

- One Rust repository, one branch of fixes, mostly found by review swarms rather than user reports. The
  set is small; per-kind numbers are anecdotes.
- The questions were written with knowledge of the fix. Even without names they lean on the row text,
  which already describes the mechanism; real reports are vaguer. Treat scores as an upper bound.
- Every item has its own `base_commit` (34 distinct trees). A runner must index each item at its own
  `base_commit`; indexing one tree for all items puts the gold at the wrong lines or in code that is
  already fixed.
- Several items bundle more than one problem (`bug-007` has three, `bug-010` and `bug-011` two rows), and
  some fixes are wide (`bug-005`: 5 gold files, 8 symbols; `bug-006`: 10 symbols). Recall over the whole gold set is harsh
  there; first-hit metrics are kinder.
- Symbol extraction is a brace-matching scanner, not a compiler. Macro-generated items and nested
  functions are attributed to the outermost enclosing function. The verification pass is the check on it.
- Some fixes changed several call sites of a bad pattern; an agent that finds one site has localised the
  bug even if the other sites score as misses.

## Scoring (for a benchmark runner)

Per item, check out `base_commit`, index it, ask `question`, and record the ranked locations the system
or agent returns.

- **File recall@k**: the share of `gold_files` among the top k distinct files (k = 1, 3, 5, 10). Also
  report **file hit@k**: at least one gold file in the top k.
- **Symbol recall@k / hit@k**: the same over `gold_symbols`. A returned location matches a gold symbol if
  it lies inside that function's span at `base_commit` (match by span, not by name text, so that
  overloaded or same-named methods are not credited by accident).
- **Hops**: the number of tool calls (or traversal expansions) until the first gold file, and until the
  first gold symbol, enters the result; cap at the budget and report misses separately.
- **Tokens**: tokens read by the agent (tool results) and tokens spent by the engine until the first gold
  hit and in total; for steered runs count the agent's steering tokens too.
- Report by `change_kind` and split by `name_leak` (leaky items are easier for lexical search). Report
  the mean with a paired bootstrap interval against a plain text-search baseline on the same items.
