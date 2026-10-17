# Multimap traversal spike

## What it is now

The spike is LCI's main line of work: LCI as an extension of grep that also walks a code graph. The framing is a Wikirace on a code graph: a fast,
decisive "System 1" racer follows compiler-grade links (SCIP, language server, git) between nodes, with a steering ladder (rules, then a local model
slot, then the outer agent) deciding at stops. Search (lexical plus vector plus rerank) stays as the first stage and the seed finder; it gets no more
embedder-or-reranker benchmarks (owner direction, 2026-10-17). Multi-step questions are the target: an error to the commits responsible, a
trait to its implementors, a config key to its readers, a symbol to its impact rings. Evaluation is by metric tiers and a rule-based simulated
agent, not by wall time ([evaluation.md](evaluation.md)).

## Status

- Shipped and described in [engine.md](engine.md): the `Map` trait and registry, NodeId, the traversal engine, the steering ladder with handles, the
  first stage, five recipes and `lci_compose` (off by default), `lci traverse`, and `search_code mode = "traverse"` (off by default).
- Measured defaults per question type are in `engine.md` ("Walk"); one open item: the MCP default `top_k` 8 gives 18 answers, which nobody measured
  (roadmap row S1b).
- Not built: the unbuilt parts of [specs/](specs/README.md) (steering ops v1 and `traverse_*` tools, production RunRecord writer, map-card files),
  a System One rung, an exact-literal seed channel, doc-file support, SCIP in the evaluation trees ([engine.md](engine.md), "Known gaps").
- Next steps and priorities live in [../../development/roadmap.md](../../development/roadmap.md); decisions and findings with ids in
  [../../decisions.md](../../decisions.md).

## Files

Living (edited in place):

| File | Holds |
| --- | --- |
| [engine.md](engine.md) | The shipped engine: identity, maps, first stage, walk, steering, recipes, traces, config keys, surfaces, seams, gaps. |
| [specs/](specs/README.md) | Interface specs (v0, pre-freeze): contracts and the reasons behind them. |
| [evaluation.md](evaluation.md) | Gold sets, metric tiers, scorers and reporting rules, the simulated agent, the agent A/B method. |
| [results.md](results.md) | One row per experiment: question, verdict, headline number, file. |
| [lessons.md](lessons.md), [ideas.md](ideas.md), [prior-art.md](prior-art.md) | Lessons L1-L21 and parking rules, the idea catalogue, prior work. |
| [v2/gold-scorers.md](v2/gold-scorers.md), [v2/policies/README.md](v2/policies/README.md) | The `cargo xtask gold` scorers and the steering policy files. |

Frozen (never edited; a correction is an additive footer or a new file):

| File | Holds |
| --- | --- |
| `v2/*-prereg.md` | Pre-registrations: question, arms, metrics, kill rule, written before the run. |
| `v2/*-results.md`, [v2/engine-results.md](v2/engine-results.md), [s1-stability.md](s1-stability.md) | Results of those runs; `engine-results.md` also holds "Shipped defaults" and "Reading rules". |
| [v2/trend.md](v2/trend.md) | One row per `cargo xtask maps bench` run; appended by xtask, never by hand. |
| `eval-seed.json`, `gold-bugloc.json`, `specs/runrecord-0.schema.json`, `v2/policies/ladder-rules-only.json` | Data and schema assets read by code or xtask; [eval-seed.md](eval-seed.md) is the readable form of the seed questions. |

Retired on 2026-10-17 (frozen files may still cite them; the full text is `git show bbd3292:docs/spike/multimap/<file>`):

| Retired file | Content now in |
| --- | --- |
| `roadmap.md` | [../../decisions.md](../../decisions.md) (S0-S4 history) and [../../development/roadmap.md](../../development/roadmap.md) (open slices) |
| `v2/next-slices.md` | [results.md](results.md), [evaluation.md](evaluation.md) "Set portfolio" and "Slice order and freshness", decisions d346-d351, the roadmap |
| `merge-conditions.md` | [engine.md](engine.md) "Seams and invariants" and the decisions log |
| `maps-inventory.md` | [engine.md](engine.md) "Maps" |
| `architecture-map.md` | [../../architecture.md](../../architecture.md) and [engine.md](engine.md) "Seams and invariants" |
| `gold-bugloc.md` | [evaluation.md](evaluation.md) "Bug-localisation set" |
| `ab-prereg-addendum-draft.md` | [evaluation.md](evaluation.md) "Agent A/B method notes" and d352 |
| `recycled-research.md` | [lessons.md](lessons.md) |

## How to add a result

1. Write the pre-registration first as `v2/<id>-prereg.md`: question, arms, primary metrics, sample, kill rule. Commit it before the run.
2. Run with a frozen binary on a frozen tree. For bench runs, `cargo xtask maps bench` appends the row to `v2/trend.md` and writes the run
   directory; do not edit the row.
3. Write `v2/<id>-results.md` with the verdict by the frozen rule, numbers copied from the run, deviations and caveats. It is frozen once committed.
4. Add one index row to [results.md](results.md) and, when the result changes a default, a decision row in [../../decisions.md](../../decisions.md)
   and the matching row of "Walk" in [engine.md](engine.md).
