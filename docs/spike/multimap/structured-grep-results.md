# Item 2: live grep skips lockfiles, results

Preregistration: `structured-grep-prereg.md` (frozen with the gold, merged in PR 96). This file records the
2c setup fixed before any held-out run (prereg section 10), then the results.

## 1. Setup fixed before any held-out run

Harness: `cargo xtask sim-structured` (`xtask/src/sim/structured/`). One frozen release `xtask` binary runs every arm;
the sim calls `live_grep::scan` in-process, so the runner is that binary. A and B differ only in
`--grep-skip none` against `--grep-skip lockfiles` (`lci_maps::live_grep::LOCKFILES`); `compare` refuses runs whose
binary hash, arm, skip setting, policy or budgets disagree.

| Value | Setting |
| --- | --- |
| Calls and returned tokens per question | 25 and 20,000 (prereg section 5) |
| Probe terms | 8 (see reading 1) |
| Files read after the first search | 3 (the simulator's default) |
| Scan byte cap | 4 GiB for every arm; a run with any incomplete scan is void |
| Traversal pop cap | not applicable: every arm is grep-only, so no traversal runs; follow-greps are bounded by the call budget and reported as pops |
| Bootstrap | 10,000 resamples of whole families (keyed by repository and family), SplitMix64 seed 20261018, percentile interval [x(250), x(9749)] (0-based) |
| Runner and rg hashes | filled in at the freeze below |

Trees: LCI is a clean export of the gold revision (no gold files inside; `run` refuses a tree holding the gold);
Vite and Home Assistant are the pinned exports of prereg section 4.

### Readings of the preregistration (fixed now, before any held-out run)

1. **Probe terms 8, not the simulator's default 6.** The simulator probes the longest query terms. At 6, the
   package name of six dependency questions and the key of two config questions lose to longer generic words
   (`sim-structured check-probes`: 2 config and 6 dependency failures at 6, none at 8), so every arm would fail them
   alike. `check-probes` passes at 8 on both strata.
2. **Behaviour questions (q2):** complete when a read window covers an assignment span or a documentation span of
   the family, the two places where the key and its behaviour meet; which of the two is recorded.
3. **Assignment-span hit@5:** ranked over the distinct files of the first search step's listing, in listing order.
   Headline for q1 and q2; reported for q3 as "not the asked fact" and left out of rollups.
4. **Reader precision (q3):** the simulator has no answer set, so the predicted reader set is its read windows in
   the scope file the question names. Recall counts reader spans covered by any window, so F1 mixes the two bases.
5. **R arm:** `rg --no-config -n --no-heading --color never -F -w --hidden -g '!.git' --sort path --max-filesize 1M`.
   `-F` is added to the frame command (the needles are identifiers, so it changes nothing but guards regex
   characters). The trees have no `.git`, so rg applies no `.gitignore` while `scan` does.

### Shakedown on the dev half (descriptive, decides nothing)

On the dependency dev half (4 questions, greedy, debug build) A and B both completed 1 of 4. The simulator's first
search ORs the three rarest terms, and for a rare package name the source files that import it fill the 50 listed
lines before the manifest. This is the frozen simulator's behaviour, not changed here; it lowers base success and
so the power of the dependency stratum.
