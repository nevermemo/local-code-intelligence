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
| Traversal pop cap | not applicable: every arm is grep-only, so no traversal runs; charged follow-greps are bounded by the call budget and reported as pops, and a follow-grep that finds nothing new is not charged (reading 6) |
| Bootstrap | 10,000 resamples of whole families (keyed by repository and family), SplitMix64 seed 20261018, percentile interval [x(250), x(9749)] (0-based) |
| Runner | release `xtask` built from main `26011c1` (PR 100), sha256 `36be3e59e65427afba352022c3046f2296eff64f862a6e84f9566369971173cc`; every arm and `compare` run this one copy (the dev rehearsal's arms ran the `ac82e48` build, whose `run` code is the same; only `compare` changed) |
| rg (R arm) | ripgrep 15.2.0 (rev e89fff89ac), x86_64-pc-windows-msvc, sha256 `14231169855ec5205cf5a1b6f1db358ff4aed4247c86b69ce8aae647c77f6680`, a copy run by path |
| Gold | the five files exported from the `ac82e48` blobs (LF), sha256 equal to prereg section 10; a Windows checkout converts them to CRLF, which changes the bytes but not the parsed gold |

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
6. **A and B scan an in-memory copy of the tree.** A follow-grep that finds nothing new is not charged, so the
   call budget does not bound the number of scans, and each scan of Home Assistant from disk took about 4 s: the
   config dev shakedown ran 70 minutes without finishing a question. The runner now walks each tree once into
   `live_grep::Corpus` (the same walker and filters as `scan`) and replays `scan`'s loop over it; only the speed
   changes. `sim-structured check-corpus` compared the two on every needle of the gold questions: 1,326 needles
   over the three trees (config gold, `--grep-skip none`; dependency gold, `--grep-skip lockfiles`), 0 differences
   in any field but elapsed time; a Home Assistant scan fell from 4,253 ms to 111 ms. The one difference by
   construction: the corpus checks the scan deadline per file rather than per walk entry, and a run with any
   deadline stop is void either way. The R arm still runs rg on the tree on disk.

### Shakedown on the dev half (descriptive, decides nothing)

On the dependency dev half (4 questions, greedy, debug build) A and B both completed 1 of 4. The simulator's first
search ORs the three rarest terms, and for a rare package name the source files that import it fill the 50 listed
lines before the manifest. This is the frozen simulator's behaviour, not changed here; it lowers base success and
so the power of the dependency stratum.

A rehearsal of the whole procedure on the dev half then ran every arm and policy on the frozen runner (R on the
dependency stratum only, to save its hours) and one pooled `compare`. It found one harness defect: `compare`
checked each R context run against every pair under its policy label, so the dependency R run met the config A
run's gold and the registered invocation was refused. The fix (PR 100) checks a context run against the pair of its
own stratum; the runner was frozen again after it. Descriptive dev readings, n 36 config and 4 dependency
questions: complete success equal in every policy and stratum (config 6 of 36 bfs, 5 of 36 greedy; dependency 1 of
4); config tokens B - A -0.8% (bfs) and -2.8% (greedy); config calls and pops +2.8% and +5.6% (bfs), +2.2% and
+4.2% (greedy). Of the four questions with more calls under B, three stopped on the token budget under A and on
the call budget under B, having spent fewer tokens (`vite.sideEffects.q1` under both policies, +16 calls;
`vite.imports.q1` under bfs, +3): a lockfile left out of a listing leaves budget unspent, and with no stop at the
gold the agent spends it on further calls. The fourth (`vite.browser.q1`, bfs) is +1 call and +296 tokens, both
arms stopping on tokens. Condition 6 is applied as registered; this reading is recorded here, before the held-out
run, so a C6 result can be read against it.

## 2. Held-out result

Every A and B arm ran on the frozen runner with the frozen gold, with no incomplete scan; `compare` on the A/B
pairs (both strata pooled under each policy label) gives the verdict. R is context only: its dependency lanes
finished, and its config lanes were stopped unfinished (18 to 36 minutes a question on Home Assistant,
where one rg call takes about 8 s), so R context covers the dependency stratum only. Adding it
leaves every verdict line unchanged.

**Verdict: the rule is not met. Condition 5 fails, so the patch (2b, lockfiles skipped by default) is dropped and
item 2 is recorded as unfinished** (prereg section 7). No threshold was retuned.

| Condition | Stratum | Greedy | Breadth-first | Verdict |
| --- | --- | --- | --- | --- |
| C1 correctness checks | - | the skip switch's fixture tests in `live_grep` pass (Full gate); not audited against each check of section 5 (persistent index counts among them), since C5 decides | | not assessed |
| C2 complete-success drop at most 2 pp | config (36) | 6 and 6 | 6 and 6 | holds |
| C2 | dependency (12) | 0 and 0 | 0 and 0 | holds |
| C3 no repository loses more than one | both | lost 0 everywhere | lost 0 everywhere | holds |
| C4 reader precision drop at most 2 points | config (12 q3) | 12.3% and 12.3% | 13.2% and 13.2% | holds |
| C5 tokens no more than 5% worse | config | +2.1% [+2, +452] | +3.3% [+32, +937] | holds |
| C5 at least 15% fewer tokens at preserved success, or +10 pp success | dependency | -3.1% [-1,559, +524] | +2.3% [-1,227, +1,908] | **fails** |
| C6 calls and pops at most 5% worse | config | +0.1%, +0.0% | +0.0%, -0.6% | holds |
| C6 | dependency | +0.0%, +0.0% | -2.0%, -4.8% | holds |

Token intervals are of the mean paired delta B - A per question (10,000 family resamples). On the dependency
stratum neither branch of C5 can hold: B saves 3.1% under greedy and costs 2.3% under breadth-first, both
intervals include zero, and both arms complete none of the 12 questions, so the success branch has nothing to
gain from.

R context on the dependency stratum (n 12, means per question): greedy A 13,111 tokens, B 12,704, R 12,503, with
complete success 0 of 12 in all three; breadth-first A 14,695, B 15,031, R 14,629, with R completing 1 of 12 (A and
B none). Calls and pops are within one of each other across the three arms. Plain rg sits with A and B: neither
the lockfile skip nor swapping the scanner for rg changes what the simulated agent spends on this stratum by more
than a few percent.

### Sanity check: why B saved no tokens

The preregistration expected fewer tokens by construction ("B returns fewer lines for every family"). It does
not, because the simulator's search listing shows a fixed number of lines (the first 50 matches, each line cut at
160 characters), not a fixed number of tokens. When a family has more than 50 matches, the lines a lockfile held
are filled by other matches, and those are longer. Matched call for call, until the two traces first differ, the
first search step costs **+12.6%** tokens under B on the dependency stratum (9,006 against 10,138, the same 12
questions under both policies) and **+6.2%** on the config stratum (46,018 against 48,868); every other step kind
is within 1.4%. Most questions end on the call budget in both arms (dependency: 10 of 12 greedy, 9 of 12
breadth-first; config: 26 of 36), so the extra tokens are not budget left over and re-spent. So the premise
behind C5 was wrong for capped listings: leaving lockfiles out changes which lines an agent sees, not how much it
reads, and the end-to-end token totals moved by a few percent either way.

What this leaves open, not tested here: whether the lines that replace lockfile lines are more useful. The
success metric could not show it, since base success on the dependency stratum is zero in both arms (the
simulator's first search ORs the rarest terms, see the shakedown note).
