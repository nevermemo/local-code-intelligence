# Simulated agent

`cargo xtask sim` replays a gold question as a fixed chain of tool calls over a
frozen tree and scores how soon the chain reads the gold. It uses no model and
no network, and two runs of one configuration produce the same bytes. It is a
proxy for an agent, not an agent: it measures how a rule-following reader
spends calls and tokens, and results are reported only as differences between
arms on the same items (`maps compare`), never as absolute numbers. Its proxies are uncalibrated
(`sim-calibration.md`, d390): the judged subset neither passed nor killed them at n 48.

Code: `xtask/src/sim/` (`terms`, `follow`, `sim_loop`, `arms`, `score`).

## Arms

- **A0, grep-only.** Probe, search, read, follow (below).
- **A1, LCI search first.** One `lci search <tree> <question> --top-k 10` call
  (`--lci-top-k`), then the A0 chain. The call prints the snippets (chunk code),
  so the span of each result counts as text read by that one call: it is a
  window for gold hits and `listed_at`, and its identifiers join the frontier.
  When the documentation lane ran, its prose snippets (`docs.results`) are
  shown too, after the code results and each lane in its own ranking. A docs-lane
  failure (`docs.warning`, or `docs_warning` in the compact view) fails open
  in `lci` and does not skip the item: it is recorded as `tool.docs_warning` in
  `sim.json` and under `docs_lane_warnings` in `meta.json`, with a warning on
  stderr.
- **A2, LCI multimap walk.** One `lci traverse --scip ...` call, built by
  `CliTraverser::command` (the call `maps bench` makes; `--maps` unset leaves
  the choice of maps to the CLI), then the A0 chain. The call prints one line
  per result (`path:line  text`); the agent then reads 40 lines around each
  returned site, one read call per file (`site-read`), for the first
  `--read-files` files in result order (at most 3 windows per file). Refused
  with a message when `--scip` is missing or not a file: a tree without a SCIP
  index can only run A0 and A1.

For A1 and A2 the opening comes first and the chain runs unchanged behind it,
with the opening's windows already covered. Calls are numbered from the opening
call, and the `lci` call counts against `--max-calls` and `--max-tokens`. If
the grep chain finds nothing but the opening showed text, its names are still
followed. What the agent is shown is a compact rendering of the tool JSON (A1:
`path:first-last` and the chunk code per result; A2: one line per result,
`path:line  text`, the text on one line and cut at 160 characters like an A0
listing line), charged at characters / 4; the JSON envelope (timings, scores,
index state) is not charged. A1 charges the whole snippet code it shows. A site
whose file is not there, or whose line lies past the end of the file, is
nothing to read: it costs no call and takes no file slot of `--read-files`.

An `lci` call that fails (non-zero exit, timeout, no JSON) skips the item: it
is not run as if the call had found nothing. It is written to `items.json`
with null metrics and `lci_failure` 1, and to `raw/` with a map-failure flag,
so `maps compare` counts it as excluded (flagged) and not as missing; it is
also listed under `skipped_lci_failures` in `meta.json`. Every difference
between two arms must be quoted together with `skipped_lci_failures` of each
side and with compare's `only in A` / `only in B` and `excluded` counts, or a
difference over fewer items than the arms ran passes as a full result. A1 needs
the tree indexed beforehand (`lci index`; a missing or stale index is refreshed
by the first call, which can take minutes and would count against
`--lci-timeout-s`) and the embedder up; the profile (embedder, reranker on or
off) is the configuration `--lci-config` names and must be stated with any A1
result. The calls inside `lci` (embedder, reranker) are not counted:
`model_calls` is 0.

A2 keeps the raw traversal reply. A traversal that stopped on its deadline or
another budget (pops, tokens, external calls, the `lsp.live` deadline), or
with map or reranker failures (the flags `maps bench` computes,
`ItemFlags::from_raw`), is written to `raw/<item>.json`, which `maps compare`
reads: such a pair is excluded unless `--keep-failed`. `meta.json` counts them
(`deadline_stops`, `flagged_items`). `sim.json` records per item which maps the
call had enabled (`tool.maps`, read from the start event of the call's trace;
null when the trace is missing), its status and stop reason.

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
   occurrences (overlapping windows merged, at most 3 per file). A window
   whose lines an opening call (A1, A2) already showed is not read again, as a
   follow target is not (`is_covered`); a file with no window left is skipped
   without a call, so behind an A1 opening that showed the same lines the
   chain is A0's with that read dropped (one call more for the `lci` call,
   one fewer for the read).
4. **Follow.** Identifiers are the tree-sitter identifier leaves inside the
   text read (any leaf whose node kind ends in `identifier`, through the
   `lci-core` language adapters; plain words for a file with no adapter or a
   prose adapter such as Markdown, which has no grammar).
   A name is kept when it has at least 4 bytes, is not in `GrepOptions`
   `skip_names` and is not a stop word of `query_terms`. The names of one read
   are ranked rarest first (fewest files), ties in first-seen order, and
   `max_candidates` (30) applies to each read on its own: the opening call, each
   site read and each of the first reads is a batch of its own, queued in the
   priority order of "Policies" below. Following
   a name costs two calls: a grep for it, then a read of its first occurrence
   in text not read yet. A name with no unread occurrence is skipped without a
   call. The rarity used for ranking is an uncharged lookup (it stands for the
   agent's judgement that a name looks specific); the grep that follows is the
   charged call. This is a modelling choice to calibrate (uncalibrated: `sim-calibration.md`, d390).

## Policies

Both rank one read's names rarest first. They differ in where a new batch goes
in the frontier:

- **greedy**: in front of everything pending, so the newest read's rarest name
  is next (depth-first).
- **bfs**: behind everything pending, so names are followed in the order they
  were found.

The first batches (those of the first reads) are queued so that each arm
explores its own evidence first, under both policies. Their priority order is
the opening call's text (A1 snippets; A2 has none of its own), then the site
reads (A2), then the chain's initial reads in read order. A name is kept by
the first batch that holds it. Bfs pushes the batches in that order; greedy
pushes them in reverse, so the opening's batch ends on top and pops first.
Either way the first names followed are the opening's, then the site reads',
then the chain's, and the policies part ways only on the batches of later
(follow) reads. Without an opening (A0) the order is the chain's reads in read
order. (An earlier version queued the batches in read order, which under
greedy put the chain's reads on top of the opening's evidence and made A2 on
the fork fixture need 12 calls to A0's 8.)

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
| `listed_at` | position (among the files of the first listing the agent was shown) of the first file with a listed line that hits the gold; the listing is the `lci` call of A1 (whole snippet spans) or A2 (site lines), else the A0 search; an `lci` call that listed nothing leaves it null (the later A0 search does not stand in) |

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

`<out-dir>/<wave>/<set>/` (wave default `sim-<arm>`; use one `--wave` per
policy and arm) must lie outside `--tree`: a run dir inside it would be
scanned by the next grep, so the command refuses it (paths compared after
resolving symlinks and `..`) before calling any tool. It holds the files `maps::baseline::write_arm_run` writes for the
other non-walk arms, so `maps compare` pairs any two of them:

- `run.json`: per item, the reading list, one `{path, line}` per distinct
  window shown (its first line), in call order;
- `items.json`: the gold scorer's metrics on that list, plus the numbers above,
  `expansions` = tool calls, `model_calls` = 0 and `lci_failure` (1, with null
  metrics, for an item whose `lci` call failed);
- `raw/<item>.json`: the raw traversal reply of A2, and for any item whose `lci`
  call failed a note that flags it (a rerun into the same dir deletes the raw
  file of an item that now has no raw reply, so an old failure marker does not
  outlive the recovery); `maps compare` reads them for its exclusions;
- `score.json`, `meta.json`: the scorer's report; the arm, the opening's
  settings, `deadline_stops`, `flagged_items` and any skipped items.

`maps compare` keys on the ids in `items.json`, which each run writes afresh,
and reads `raw/<item>.json` only for those ids. A `raw/` file of an item that a
rerun left out (a smaller `--limit`, a filtered set) into the same directory
therefore survives on disk and `maps compare` never reads it; only an item that
is in the run and has no raw reply gets its file removed. Other tools read
`raw/` wholesale (`maps fit-priors`, the tie-break fit, the oracle), so they
would see it: use a fresh `--wave` rather than a directory with older items in
it.

The scorer's own metrics are a by-product: the reading list is not an answer
set, and impact and flow gold are declaration lines that a window start rarely
equals. The numbers above are the result. `sim.json` holds the config, the
aggregate and every item with its trace. A2 also leaves the `lci traverse`
traces under `cli/`.

Comparing two runs: `maps compare` takes `--metric calls_to_gold hit_at_12`
(its default metrics are the gold sets' own and are not in these files).
`calls_to_gold` is null on a miss, so it is compared on the items both arms hit
only; read it together with `hit_at_12` and `hit_at_25`, which count the
misses, and never alone.

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
- A1 and A2 ask `lci` once per question and the call is not cached or
  repeated: its output is whatever the index and the services gave at that
  time. Two runs agree only if the tool output agrees (the unit tests use
  stubs); the kill rule's determinism check is therefore also a check of the
  `lci` calls.
- A2 reads only the first `--read-files` files of the returned sites (3 by
  default, as the A0 first reads) and the traversal returns at most
  `--lci-top-k` results; sites beyond that are shown in the listing but not
  read. This cap is a modelling choice, like the others, to calibrate.
- The proxies are uncalibrated (`sim-calibration.md`, d390): a judged subset of 48
  items gave pooled tau-b 0.228 and Spearman 0.253, between the pass and kill bars.
  The "to calibrate" choices above (the `read_files` cap, the uncharged rarity, the
  40-line window) stay as they are until a new pre-registered slice.
