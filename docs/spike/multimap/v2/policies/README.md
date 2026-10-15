# Steering policies for `maps bench --policy`

A policy file is the `--policy` argument of `lci traverse` (the `steer` object
of `search_code`: rules, ladder, max_stops, tau, stop_every). JSON has no
comments and the object refuses unknown keys, so each file is described here.
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
  goes on.

Not expressible here: the likely-complete and disagreement stop triggers.
`StopPolicy` has both (`likely_complete`, `on_disagreement`), but the
`steer` object has no field for them and the traverse call only builds
`StopPolicy::every(..).max_stops(..)`. Adding them needs `SteerArgs` and the
handler construction in `src/app/traverse.rs`, which this slice did not touch.
