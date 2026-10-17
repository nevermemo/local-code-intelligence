# Simulated agent

`cargo xtask sim` replays a gold question as a fixed chain of tool calls over a
frozen tree and scores how soon the chain reads the gold. It uses no model and
no network, and two runs of one configuration produce the same bytes. It is a
proxy for an agent, not an agent: it measures how a rule-following reader
spends calls and tokens, and results are reported only as differences between
arms on the same items (`maps compare`), never as absolute numbers.

Code: `xtask/src/sim/` (`terms`, `follow`, `sim_loop`, `score`).

## Arms

- **A0, grep-only (implemented).** Probe, search, read, follow (below).
- **A1, LCI search first (7b).** One `lci search ... --top-k 10` call and a read
  of its snippet spans, then the A0 chain.
- **A2, LCI multimap walk (7b).** One `lci traverse --scip ...` call and a
  40-line read at each site it returns, then the A0 chain. Only on trees that
  have a SCIP index.

## The A0 chain

1. **Probe.** The question goes through `lci_core::lexical::query_terms`. The
   `probe_terms` longest terms (default 6) get one grep call each
   (`lci_maps::live_grep::scan`; a term is searched as `term`, `Term`, `TERM`
   and as the question wrote it, merged, because `scan` is case sensitive). A
   term's rarity is the number of files it occurs in, counted without any cap.
   Terms found nowhere are dropped.
2. **Search.** One call lists the occurrences of the (up to three) rarest
   terms together, files with more distinct terms first, then more lines, then
   path; at most 50 lines.
3. **Read.** The top 3 files, one call each, as 40-line windows around the
   occurrences (overlapping windows merged, at most 3 per file).
4. **Follow.** Identifiers are the tree-sitter identifier leaves inside the
   text read (any leaf whose node kind ends in `identifier`, through the
   `lci-core` language adapters; plain words for a file with no adapter).
   A name is kept when it has at least 4 bytes, is not in `GrepOptions`
   `skip_names` and is not a stop word of `query_terms`. The names of one read
   are ranked rarest first (fewest files), ties in first-seen order. Following
   a name costs two calls: a grep for it, then a read of its first occurrence
   in text not read yet. A name with no unread occurrence is skipped without a
   call. The rarity used for ranking is an uncharged lookup (it stands for the
   agent's judgement that a name looks specific); the grep that follows is the
   charged call. This is a modelling choice to calibrate in 7c.

## Policies

Both rank one read's names rarest first. They differ in where a new batch goes
in the frontier:

- **greedy**: in front of everything pending, so the newest read's rarest name
  is next (depth-first).
- **bfs**: behind everything pending, so names are followed in the order they
  were found.

## Budgets and tokens

At most 25 tool calls and 20,000 tokens per question; tokens are characters
divided by 4, rounded up, of each tool output (as `xtask/src/maps/bench.rs`).
A call that would pass either budget is not made and the run stops (`stop`:
`calls` or `tokens`). Other stops: `exhausted` (nothing left to follow),
`no-match`, `no-terms`, and `gold` (only with `--stop-at-gold`, which is
gold-aware and measures calls to the gold for a reader that recognises it; off
by default). The loop never reads the gold otherwise.

## Scoring (per item)

| field | meaning |
| --- | --- |
| `tool_calls`, `tokens_read` | calls made and tokens of all output |
| `calls_to_gold`, `tokens_to_gold` | call number and cumulative tokens of the first read that hits the gold; null if none |
| `hit_at_12`, `hit_at_25` | `calls_to_gold` is within 12 / 25 calls |
| `listed_at` | position (among the files of the search listing) of the first file with a listed line that hits the gold |

A read window hits the gold when it overlaps a gold span, or, for an expected
file that has no span (`gold::expected_files`), when it is a window of that
file. Spans exist only for where-to (`expected`, `members`, `impls` and
`equivalents`, the sub-objects `expected_files` reads). Impact and flow are
scored by file, as the oracle arms of `maps` are: impact by its R1 files, so
the R2 and test declarations never count; flow by its intermediate files. An
item with no gold has null hit fields.

`hit_at_12` is exactly the 12-call prefix of the run: the loop is the same up
to call 12 whatever the larger budget, so a 25-call run and a 12-call run agree
on it (tested).

## Output

`<out-dir>/<wave>/<set>/` holds `sim.json` (config, aggregate, every item with
its trace), `items.json` (the numbers above plus `expansions` = tool calls and
`model_calls` = 0, the shape `maps compare` reads) and `meta.json`. Use one
`--wave` per policy and arm.

Comparing two runs: `maps compare` takes `--metric calls_to_gold hit_at_12`
(its default metrics are the gold sets' own and are not in these files).
`calls_to_gold` is null on a miss, so it is compared on the items both arms hit
only; read it together with `hit_at_12` and `hit_at_25`, which count the
misses, and never alone.

Not written yet: `run.json` and `score.json`. They come from
`maps::baseline::write_arm_run`, which is private to `maps`; making it
`pub(crate)` and giving each sim item a ranked answer list (the windows read,
as `{path, line}`) is part of 7b.

## Limits

- The case-insensitive grep is emulated with the spellings `term`, `Term`,
  `TERM` and the token as the question wrote it (`parseConfig`); a name that
  is camelCase in the middle of a longer identifier (`getParseConfig` for
  `parseconfig`) is still missed.
- Uncharged whole-tree knowledge. Two steps use information the agent has not
  paid a call for: a name's rarity (the number of files holding it, an
  uncapped count over the whole tree) ranks the names found in a read, and the
  "already covered" test skips a name whose occurrences all lie in text
  already read. The grep that follows a chosen name is charged. Every arm and
  policy gets the same help, so differences between them are not biased by it,
  but absolute calls are optimistic.
- Probing costs calls, so a long question spends part of the budget before
  reading; `probe_terms` is the knob.
- A scan that stops on its deadline or byte cap is counted in `meta.json`
  (`incomplete_scans`); a non-zero count means the run may not repeat.
