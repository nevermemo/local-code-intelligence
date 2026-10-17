# Steering policies for `maps bench --policy`

A policy is the `--policy` argument of `lci traverse`. It is one of: a preset name (`explore`, `impact`, `flow`, `where-to`, `history`; a preset also
sets the measured first-stage and walk defaults of its question type, see [../../engine.md](../../engine.md), "Walk"), a JSON file, or inline JSON that
is either an array of rules or the `steer` object of `search_code`. The `steer` object has the keys `policy`, `rules`, `script`, `ladder`, `max_stops`,
`tau`, `stop_every`, `on_disagreement`, `likely_complete` and `actions`; `script` is reserved (an embedded script handler, not available) and
`actions` is only valid together with `handle` (the outer agent's answer to a checkpoint, so a policy file never carries it). JSON has no comments and
the object refuses unknown keys, so each file is described here.

`cargo xtask maps bench --policy <value>` passes the value on to `lci traverse --policy` and defaults to `default` (no policy); the trend row's
`policy` column names the file's stem, or `default`. `--policy-dir <dir>` is a bench-only, gold-aware oracle arm: a directory of per-item policy files
(`<slug of item id>.json`, written by `maps oracle-policy`), one passed to `lci traverse --policy` for each item. It sees the gold, so it is a steering
ceiling and not a result; the row says `ORACLE` (`oracle:<directory name>`), an item with no file is skipped, and it excludes `--policy`.

## ladder-rules-only.json

Turns the stop ladder on with rules only (no model, no agent answer). Its seven rules, `ladder` `["rules", "agent"]`, `max_stops` 8, `tau` 0.5,
`stop_every` 20, `on_disagreement` 5 and `likely_complete` 90 mean:

- a stop every 20 pops (`stop_every`), at most 8 stops, rung order `rules` then `agent`;
- prune rules: test nodes (`kind: test`) and anything under a `tests` directory or module, anything under a `generated` directory, and `*.pb.rs`
  files. Symbols are matched by their module path joined with `/` (no `src/`), files and chunks by their path. A rule list cannot see the question, so
  the pruning applies to every question type; it is meant for the impact set, where tests are noise, and costs recall on a set that asks for tests;
- a `stop` rule when less than 10 percent of the pop budget is left;
- a final `continue` rule that matches every node. Without it a stop no rung can decide returns a checkpoint, which ends a CLI run (the CLI has no
  agent to answer). With it the rules are confident at every stop and the run goes on;
- two more stop triggers beside the periodic one (`on_disagreement` and `likely_complete`; every stop counts toward `max_stops`): a stop once 5 new
  disagreements (a node a fuzzy map offered that a structural map, asked at the same expansion, did not) have been recorded since the last stop, and
  one stop once the mark-recapture estimate says 90 percent of the findable nodes are found. Both only decide when the rules are consulted; with the
  catch-all `continue` rule the run goes on after each.
