# Item 2 pre-registration: lockfile exclusion in the live-grep fallback, on structured config files

Slice 2a of roadmap item 2 (bounded structured-file grep). This file freezes the question, the gold,
the arms and the decision rule before any product change or arm run; its commit precedes those of
slices 2b (eligibility change) and 2c (comparison). Results go to
`docs/spike/multimap/structured-grep-results.md`. Labels: [M] measured, [I] inferred, [G] guess.
Section 10 lists what is fixed now and what is filled in later. The frozen plan restated here has
sha256 `66bce2812f74d8c3a50607f6b03718077a404a3e33294f482bb04cb2efd37619`; section 11 lists departures.

## 1. Purpose and question

Persistent indexing has no adapter for `.json`, `.yaml`, `.yml` or `.toml` [M, plan inspection]:
those files have no chunks or embeddings, so ordinary `search_code` cannot return them. The opt-in
`grep.live` path scans text regardless of extension and so can reach them. It limits files to
1,048,576 bytes, a scan to 256 MiB, matches to 50 and a call to three seconds. It skips no lockfiles.

Fixed question: **if the live-grep fallback stops scanning lockfiles, does a simulated agent find
the same structured-config evidence at equal or lower cost, on keys in real repositories?**

Question types, each with a gold fact behind it: **setting location** (where is key X set; gold
separates assignments, defaults and examples), **behaviour to setting** (which key controls Y; the
question does not spell the key) and **setting to reader** (what reads key Z; gold lists direct
readers in a named component). This is a fallback-eligibility experiment with a rule-based simulated
agent. It is not an LLM-agent result, not a claim that `search_code` supports structured files, and
not a parser test.

## 2. What slice 2-0 measured (premise check)

Read-only: no LCI change, no build; plain `rg` stood in for the scanner [I].

- **Sample [M]:** 450 keys, 150 per repository (LCI, Vite, Home Assistant), seed 20261018, stratified
  by format; each key at least six characters and not on live grep's common-name list; 900 `rg` runs
  (with and without `--max-filesize 1M`), all completed. Matched lines after the paths live grep
  already skips: 59,459 pooled (LCI 10,509; Vite 5,793; Home Assistant 43,157).

| Class | LCI | Vite | Home Assistant | Pooled | Share of noise |
| --- | ---: | ---: | ---: | ---: | ---: |
| Lockfile | 897 | 1,138 | 3 | 2,038 | 99.853% |
| Generated | 0 | 0 | 3 | 3 | 0.147% |
| Vendored | 0 | 0 | 0 | 0 | 0% |
| Oversize (second run) | 0 | 0 | 0 | 0 | 0% |

- **Pooled noise share [M]:** 2,041 of 59,459 = 3.433% (LCI 8.536%, Vite 19.644%, Home Assistant
  0.014%). **Keys with a noise line in the first 50 matches [M]:** 122 of 450 = 27.11% (LCI 17/150,
  Vite 101/150, Home Assistant 4/150). Noise files were `Cargo.lock`, `pnpm-lock.yaml` and one
  generated JSON file.

Kill rule, written before slice 2-0 ran: close item 2 as "the grep fallback is already clean enough"
if the pooled noise share is below 5% **and** fewer than 10% of keys have a noise line in their
first 50 matches. First line: true (3.433%). Second line: false (27.11%). The rule did **not** fire.
Classes under 1% of noise lines were to be dropped from 2b.

Limits [M]: counts are exact for these keys, trees and rules, not population prevalence. 222 of the
450 keys are not plain identifiers, so live grep would reject them (rg-proxy results). No timeout,
byte cap or `.lciignore` effect was simulated; no end-to-end success was measured.

## 3. Scope of slice 2b: the lockfile class only

Generated (0.147% of noise), vendored (0%) and oversize (0%) fall under the 1% rule and are not
built. Only the lockfile class (99.853%) can ship, and only if section 7 allows it. The change
[fixed now]: inside `lci-maps/src/live_grep.rs::scan`, before a file is read and before it
can use a match slot, skip files whose basename is exactly one of `package-lock.json`,
`npm-shrinkwrap.json`, `composer.lock`, `Cargo.lock`, `yarn.lock`, `pnpm-lock.yaml`, `Pipfile.lock`,
`poetry.lock`, `uv.lock`, `bun.lock`, `bun.lockb`. There is no broad `*lock*` rule.

Unchanged: the 1 MiB, 256 MiB and 50-match caps, the hidden-path, `target` and `node_modules` skips,
UTF-8 and binary handling, default map selection, the persistent index. Agents still get lexical
mentions with `path:line`, not verified reads. Not built here, and not claimed harmless:
generated-file and vendor-directory rules, a tighter read bound for growing files, and `.lciignore`
handling in live grep.

## 4. The gold

Built in slice 2a, independently of LCI retrieval: `evaluations/structured/lci.json`, `vite.json`,
`home-assistant.json`; provenance in `evaluations/structured/NOTES.md`.

Revisions: LCI (TOML) `579a5bec69309d98c78825654aabe8f03ab64421`; Vite (JSON)
`7a89794331526a67bc9b29047b86957c581c128c`; Home Assistant (YAML)
`bd92409b6ade6e2873cac603104f23e5d9e7ec61`.

- **Two strata.** The **config stratum** (`evaluations/structured/*.json`) is the plan's gold: 8 key
  families per repository, 24 in all, one question of each type per family, so 72 questions, 24 per
  type. The 24 `behaviour_to_setting` questions do not spell the key; the other 48 do. No repository
  fell short of the floor of six. The **dependency stratum** (`evaluations/structured/dependency/`,
  added before any arm ran; section 11) holds dependency declarations, one `setting_location`
  question per family, up to 8 families per repository, sampled from a deterministic frame (below).
- **How facts were read:** `git show <revision>:<path>` on pinned blobs, blob id recomputed, exports
  byte-equal. JSON parsed strictly, TOML with a standard parser, YAML as inert syntax nodes; spans
  come from source marks and carry the whole-file sha256. The builder used Git grep; `rg` only produced the descriptive lockfile table.
  No LCI result, config-key map, retrieval output, candidate filter, network call or model was an
  evidence source.
- **Per family:** qualified key; labelled assignment spans (assignment, default, example, fixture);
  direct-reader spans with their derivation (literal, serde, ast, constant); documented behaviour.
  **Scopes are stated:** an assignment question is exhaustive only for its named file, a reader
  question only for its named module or function. Fixture and example settings are labelled, never
  implied to be deployment use. Whole-map serialization is not a key-specific reader. Every
  reader scope is a named component; where the value leaves for a library or constructor, the scope
  says it ends at that hand-off. Counted reader spans: 8 (LCI), 10 (Vite), 8 (Home Assistant).
  Every Vite assignment is a labelled playground fixture.
- **Independence (config stratum):** no two families of a repository share an assignment span, at
  most two share a reader file, Home Assistant takes at most two families from one YAML file, and
  every key's last segment has at least six characters (live grep's floor is four).
- **Dependency frame [fixed now]:** per repository, every dependency name declared in a tracked
  non-lockfile manifest that live grep can search (identifier, at least six characters, not in the
  common-name list) and whose premise `rg` output has a lockfile line in its first 50 lines, ordered
  by the seeded SplitMix64 shuffle; candidates are taken in that order and the first verifiable ones
  kept (each skip and its reason is in that stratum's NOTES.md). Its gold is the one declaring line
  the question singles out; it has no reader or behaviour question.
- **Dependency stratum size [M]:** frames of 11 (LCI) and 28 (Vite) names; the first 8 of each were
  verifiable and kept, none skipped. Home Assistant's frame is empty (its only lockfile is a test
  tool's `pnpm-lock.yaml` that names none of its Python requirements), so it has no dependency gold.
  When a name is declared in two or three manifests, the gold is the manifest whose path sorts first
  and the question names that crate or package. Held-out (6 per repository, 12 questions): LCI
  `reqwest`, `protobuf`, `tempfile`, `globset`, `tracing`, `sysinfo`; Vite `premove`, `nanoid`,
  `parse5`, `tinyexec`, `foobar`, `mrmime`. The split takes floor(n/4) dev (changed from floor(n/2)
  before any run, which would have left 8 held-out questions, under the floor of 10 in section 7).
- **Split [fixed now]:** per repository and stratum, family ids sorted bytewise, Fisher-Yates from the
  back driven by SplitMix64 seeded 20261018; the first four config families (the first floor(n/4)
  dependency families) are `dev`, the rest `heldout`; a family's questions share its half. Config
  stratum: 12 dev and 12 held-out families (36 questions each). Disclosure: the first build used
  CPython's `random.Random(20261018)`; it was replaced before any arm ran, because that generator is
  not portable. Config held-out families: LCI `embedding_query_template`, `index.freshness`,
  `readiness_timeout_seconds`, `rust_analyzer_trusted`; Vite `compilerOptions.experimentalDecorators`,
  `exports`, `workspace`, `workspaces`; Home Assistant `blueprint.input.no_motion_wait.default`,
  `command_line.binary_sensor.command_timeout`, `sensor.filters.precision`, `sensor.filters.window_size`.
  Two held-out pairs are related (Vite `workspace`/`workspaces`, both read by root discovery; the two
  Home Assistant filter options); the family-clustered bootstrap does not remove that correlation.
  One reader span is shared across halves: Vite `compilerOptions.experimentalDecorators` (held out)
  and `compilerOptions.jsxImportSource` (dev) both read through the same esbuild option copy loop,
  so the dev family's reader question is not used for harness shakedown.
- **Validator:** `cargo xtask gold validate-structured --gold-dir evaluations/structured --tree
  lci=<dir> --tree vite=<dir> --tree home-assistant=<dir>`. It checks schema, counts, spans against
  the checked-out trees, key leaks into questions that must not name the key, the seeded split and
  portability (no machine paths), for both strata (`--gold-dir evaluations/structured` and
  `--gold-dir evaluations/structured/dependency`). The LCI tree given to it, and to every arm, is a
  clean export of the gold revision: the working tree holds the gold files themselves, which would
  put every key and question into the searched text. It must exit zero at the freeze commit, which records its report;
  the gold files do not change after that commit.
- **Use of halves:** held-out families are scored once, for the decision; dev families serve harness
  shakedown only. **Limits [I]:** the sample is curated, component-scoped and small; not representative.

## 5. Arms, budgets and the frozen runner

- **A, today:** frozen baseline LCI with `grep.live` explicitly enabled. **B, slice:** identical
  maps, seeds, budgets and settings; only the section 3 rule differs. **R, plain rg:** actual ripgrep
  plus file reads, independent of LCI's scanner; executable, flags, ignore policy and output ordering
  are frozen, and no candidate exclusions are copied into R.
- **Agent and budget:** the simulator's read and follow accounting, greedy and breadth-first
  policies, **25 calls and 20,000 returned-token equivalents** per question, identical traversal pop
  caps. Seeds come from the query only; no gold location is injected. No LLM is called.
- **Frozen runner:** the simulator's grep-only path calls `live_grep::scan`, so rebuilding the runner
  between arms would change more than the rule under test. The fallback runner is built once, hashed
  and used unchanged for A and B; the section 3 rule is their only difference. Binaries are frozen
  copies, each arm has its own short data directory, runs are on Windows with native rg.
- **Scanner level and end to end are reported separately.** Usable SCIP indexes may not exist [G];
  then the report says so and does not substitute scanner hits for traversal success.
- **Correctness checks** (constructed fixtures; behaviour, not efficacy): each listed basename is
  skipped; near-miss names and authored configs beside a lockfile are scanned; excluded files use no
  match slots; persistent index counts are unchanged; Windows paths, CRLF and Unicode behave as before.

## 6. Outcomes
Primary, held-out half, per arm: (1) **complete success within budget**, all the required evidence
of the question's gold fact; (2) **assignment span hit@5**; (3) **direct-reader precision, recall and
F1** on the closed reader set; (4) **calls and returned tokens to complete evidence**. Misses at the
budget are reported, not only averages over successes.

Secondary: traversal pops and expansions; recall per 1,000 returned tokens; excluded-match counts;
bytes and files scanned; cap and failure counts; all by repository, format and question type. Speed
is judged by these work proxies, **not wall time**. Statistics: paired bootstrap, 10,000 resamples,
clustered by key family, with intervals and wins, losses and ties. Every table prints n; a cell under
10 items is labelled "too small to read".

## 7. Decision rule (B against A, held-out half) [fixed now]
Ship only if **all** hold. Conditions 2, 3 and 6 are checked on each stratum's held-out half
separately; condition 4 on the config stratum; condition 5 on the dependency stratum.
1. The correctness checks of section 5 pass.
2. Held-out complete-success drops by at most **2 percentage points** overall.
3. No repository loses more than **one** question.
4. Reader precision drops by at most **2 points**.
5. **Either** at least **15% fewer** charged tokens at preserved success, **or** at least
   **10 points higher** complete-success with tokens no more than 5% worse. The paired interval must
   exclude zero, and the greedy and breadth-first policies must agree in direction. On the config
   stratum, where lockfiles rarely reach the first 50 matches, charged tokens must simply be no more
   than 5% worse. A dependency held-out half under 10 questions cannot meet this condition: the
   result is then unresolved. **Preserved success** means B completes at least as many dependency
   held-out questions as A. The frame admits only names with a lockfile line in rg's first 50, so B
   returns fewer lines for every family by construction: the direction of the token change is not
   in question, only its size (at least 15%, interval excluding zero) and whether success holds.
6. Calls and pops each worsen by no more than **5%**.

At these sizes one question is 2.8 points of the config held-out half (36) and 8.3 points of the
dependency held-out half (12), so condition 2 allows no net lost question; condition 3 is kept for
the record and cannot bind on its own.

If any condition fails, or the result is unresolved, the patch is dropped, the evidence is kept and
item 2 is recorded as unfinished. Thresholds are not retuned on the held-out half. R is context, not
part of the ship rule. If the rule is met, `docs/spike/multimap/engine.md` documents the exclusion
and item 2 stays open for the remaining work.

## 8. Adapter-experiment rule [fixed now]

A separate adapter experiment is justified only if at least **six** held-out failures, spread over at
least **two** repositories, persist with **both B and R**, and an audit of each finds a cause that
needs structural knowledge: qualified nested keys, aliases or serializer relationships. A failure is
a question without complete required evidence within the budget. Seed routing, deliberate exclusions,
short or common keys and unavailable SCIP do not count. Adapter work needs its own format-specific
preregistration first.

## 9. What this slice cannot answer

- Free-text entry, short or common keys, hidden configuration, nested-key ambiguity and generic
  reader navigation; these are recorded separately, not charged to missing parsers.
- Whether generated, vendored or oversize files hurt: slice 2-0 found 3, 0 and 0 lines of them, rare
  in that sample, not shown harmless. Whether an LLM agent does better: the simulated agent's proxies
  are uncalibrated (d386, d390).
- Questions whose answer is inside a lockfile ("which version is locked?"): B cannot answer them by
  design, and neither stratum contains them; that cost is a known limit of the rule, not measured.
- Whether live-grep hits are correct reads (they are lexical mentions); `search_code`; persistent
  indexing; other formats or repositories; macOS and Linux (d381); population prevalence of lockfile
  noise (slice 2-0 noise sits mostly in two of three repositories).

## 10. Fixed now and filled in after the run

- **Fixed now:** sections 1 to 9 as written, including the lockfile-only scope and the section 2 numbers.
- **Frozen with the gold (the commit that adds this file):** gold sha256 of the committed (LF) bytes:
  `evaluations/structured/lci.json` 9753c4c5781041309ea70d67aa235267cda981047567a11fed7cd899b8bc2f96,
  `vite.json` 963335bba2c34417c1aded1bd6c7a0eda66601ba36728cc615cf12b476a0e7f9,
  `home-assistant.json` 8b23b361997dff1f27333b471330b86696c8278da953e07eca654bd916b01843,
  `dependency/lci.json` 44f646dca277f1dfe6761c899534836a74249e3f54b70f26813ea5c0f72fa81f,
  `dependency/vite.json` 4af5cd72d48443f15229490418f1e70e6f1f9515a904470d9eee285d2a44d71b.
  The validator passes on both strata against a clean export of the LCI gold revision and the two
  pinned exports.
- **Filled in before any arm runs (the 2c setup, recorded in the results document):** traversal pop
  cap, bootstrap seed, runner and rg hashes and flags.
- **Filled in after the run:** commit hashes, every measured value and interval, which conditions
  held, the decision row, whether SCIP indexes were usable.

## 11. Disclosure and differences from the plan

- The gold was built before this file, so its author knew the families and the split (which came
  from the seeded algorithm, not hand choice). No arm has run. The 2b change exists on its own
  branch (`feat/live-grep-lockfiles`), unmerged; it merges only if section 7 is met. To keep one
  frozen runner (section 5), the skip mechanism itself reaches main first with an empty default and a
  harness switch, so A and B run the same binary; what ships, if section 7 is met, is the default.
- **Amendment before any run:** the config stratum's descriptive lockfile table (its NOTES.md)
  showed lockfile lines in rg's first 50 matches for one of 24 keys, against 122 of 450 sampled keys
  in slice 2-0 (101 of 150 in Vite). On that gold alone condition 5 could not hold whatever the rule
  did, so the decision would have been made by the gold's construction, not by evidence. The
  dependency stratum was added, with its frame fixed in section 4, condition 5 moved to it and the
  config stratum kept as the no-harm test. The first round's gold was also revised after a review
  (near-duplicate families across halves, a four-character key); its families are not the ones above.
- **Second amendment before any run (pull-request review of the gold):** four reader scopes named a
  function that does not contain the cited lines (Home Assistant `target_sensor`, LCI
  `embedding_document_template` and `rust_analyzer_trusted`, Vite `browser`); their scopes and
  reader questions were corrected. One YAML assignment span (`sensor.entities`) ended a line late and
  was shortened. The build scripts now assign the seeded split themselves. No family, split or key
  changed; the config hashes in section 10 are the corrected files.
- LCI revisions differ across sources: the plan inspected `ec458ac611390b3b616dd90cd3b955949a6bb3d3`,
  slice 2-0 measured the working tree at `a0920ab1c7bbd660e4ddcd18729d9c4ea94a8534`, and the gold
  uses the revision of section 4. Vite and Home Assistant match. Premise counts are not re-measured
  at the gold revision.
- The plan puts generated, vendored and `.lciignore` rules and a read bound in 2b. This file follows
  the slice 2-0 result and drops them, and narrows the plan's stress fixtures to the lockfile class.
  Any later deviation is listed in the results document with its reason.
