# Steering policies for `maps bench --policy`

A policy file is the `--policy` argument of `lci traverse` (the `steer` object
of `search_code`: rules, ladder, max_stops, tau, stop_every, on_disagreement,
likely_complete). JSON has no comments and the object refuses unknown keys, so
each file is described here.
Pass the path to `cargo xtask maps bench --policy <file>`; the trend row's
`policy` column then names the file instead of `default`.

## ladder-rules-only.json

Turns the stop ladder on with rules only (no model, no agent answer):

- a stop every 20 pops (`stop_every`), at most 8 stops, rung order
  `rules` then `agent`;
- prune rules: test nodes (`kind: test`) and anything under a `tests`
  directory or module, anything under a `generated` directory, and
  `*.pb.rs` files. Symbols are matched by their module path joined with `/`
  (no `src/`), files and chunks by their path. A rule list cannot see the
  question, so the pruning applies to every question type; it is meant for the
  impact set, where tests are noise, and costs recall on a set that asks for
  tests;
- a `stop` rule when less than 10 percent of the pop budget is left;
- a final `continue` rule that matches every node. Without it a stop no rung
  can decide returns a checkpoint, which ends a CLI run (the CLI has no
  agent to answer). With it the rules are confident at every stop and the run
  goes on;
- two more stop triggers beside the periodic one (`on_disagreement` and
  `likely_complete`; every stop counts toward `max_stops`): a stop once 5 new
  disagreements (a node a fuzzy map offered that a structural map, asked at
  the same expansion, did not) have been recorded since the last stop, and
  one stop once the mark-recapture estimate says 90 percent of the findable
  nodes are found. Both only decide when the rules are consulted; with the
  catch-all `continue` rule the run goes on after each.
