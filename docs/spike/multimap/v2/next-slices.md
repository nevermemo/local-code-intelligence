# Spike v2: next slices (2026-10-16)

Direction set with the owner on 2026-10-16, after bench-1023 and the S8j roles step 1, and
revised the same day after two independent sanity checks (a Fable review and a gpt-6-astra
review, same brief, public tree at ae81933). Every kill rule below is fixed before its
slice runs; a slice that misses its rule is recorded as killed with its numbers, not tuned
until it passes.

## Why the direction changed

Measured so far (`engine-results.md`, `s8j-roles-results.md`): the gains come from the
first stage (exact operators and search seeds) and from filtering results, not from the
steered walk. Search alone beats the walk on where-to, one hop beats it on flow, a
gold-aware steering probe gains little over a filter of the walk's results, and every stop
rule tried (strict bound, the model's "enough?", a model-chosen step count) failed its
rule. The one decision model role that survived is the judge used as a result filter.

Caveat found by both reviews: in bench-1023 the append merge returns the first-stage cards
plus the walk's top k (about 20 answers against about 10 for the other arms), and the bench
scores every returned answer. Part of the first-stage gain is an answer-budget effect.
N0 is rerun at matched budgets before anything builds on it.

## The owner's proposals

The owner proposed two ideas that set the order below:

1. **Adapt inside the traversal instead of training first.** Start from a good entry point
   (named by the agent, or found by LCI with agentic-search style methods), start with
   little information, update and adapt mid-traversal. The worry it answers: an agent
   should never use a weaker tool while a component learns.
2. **Chained questions.** One call carries several dependent questions (a chain, tree or
   graph); each step can use the answers of earlier steps; breakpoints between steps
   decide whether to continue or return the resolved answers with the reason for stopping.
   Example: find where a component is set up, then its config, then everything that reads
   the same config, then the other components affected. Today that is three or four tool
   calls; grep cannot answer the "reads the same config" step in one.

Refinements recorded with them: answers return their reasoning (why the traversal
stopped, what it left unexplored, how sure it is), so an agent continues instead of
starting over; recipes (named plans for common jobs) keep the schema off the agent's
critical path; partial plans are allowed, because an agent often cannot plan step 3
before it has seen step 1.

Later the same day the owner added N4: train a result filter offline on labels, if it has
real merit, as a parallel track that does not hold up the others.

## Prior art

Chaining is not new: programmatic tool calling and tool-call batching in agent runtimes,
graph query languages over code (Cypher over code graphs, CodeQL, Glean's Angle), and
multi-hop graph tools exposed over MCP all compose several lookups into one call. LCI's
own part is the typed operator set with provenance (exact closure, `reads_key`, pickaxe,
search) on a local index. N3's baseline therefore includes plain programmatic composition,
not only single-step tools.

## Freshness: out of scope for now

Owner decision, 2026-10-16: the spike works on frozen trees, so freshness is ignored for
now. One review called a freshness contract a blocker for live use; that stays true for a
release and is recorded under "Later". Guard inside the spike: every N1 and N3 task runs
read-only on the frozen tree, so no freshness question arises in a measurement.

## Order

N0 first, then N1, N2, N3 in the owner's order; N4 runs in parallel once N0b's judge
harness exists. N0 and N1 run in parallel; N2's code is written in parallel and benched
only after N0b.

| slice | what | depends on |
| --- | --- | --- |
| N0 | first stage alone against first stage plus walk, matched answer budget | - |
| N0b | the same two arms, each with and without a local judge as a result filter | N0 |
| N1 | chained-query plan study, plans executed offline (Wizard of Oz executor) | - |
| N2 | in-request edge bandit with a yield tracker | N0b keeps the walk somewhere |
| N3 | chained executor with breakpoints, or recipes only | N1's verdict |
| N4 | result filter trained on offline labels | N0b's harness; parallel |
| F1, F2 | verify-on-read freshness; staleness after real edits | later |

An option the owner may take: a thin N3 (recipes only) in parallel with N2, since N3 does
not depend on N2. Not taken by default; the owner's order stands.

## N0: does the walk add anything after the first stage?

- Arms, at a matched final answer budget: F-first with first_k 20 against F-append
  (first_k 10 plus the walk's top 10); both arms also cut at 10 answers. Bench-1023 setup
  otherwise (release build, same maps, same deadline, 95% paired bootstrap).
- Primary metric per set, fixed before the run: where-to set F1, flow path recall, impact
  second-ring declaration recall, history hit@10. Holm correction across the four sets.
- Decision, per set: the walk stays in the default path only where F-append beats F-first
  with a Holm-adjusted interval that excludes zero. Elsewhere it becomes opt-in for that
  kind of question.
- Agreement check (offline): does overlap between the exact answers and the search seeds
  predict whether the walk adds a gold item? Killed on the first, unmatched run (AUC .584
  on impact, .472 on history, both below .65); not repeated.

## N0b: does a result filter make the walk worth it?

- Four arms at the same final answer budget: first stage; first stage plus walk; each with
  and without the same local judge (picked from the S8j screen, run in-engine). Exact-origin
  answers are never reordered by the judge.
- Decision, per set: the configuration with the best primary metric under Holm becomes the
  default; the walk stays only where walk-plus-filter beats first-stage-plus-filter with an
  interval that excludes zero. Physical cost (expansions, model calls, input tokens) is
  reported for every arm.

## N1: will agents write good query plans? (Wizard of Oz)

- Agents get a draft chained-query schema (typed step inputs and outputs, relation
  direction, roles), a short recipe list, and real tasks on the frozen tree. Part of the
  task list comes from the gold sets; 6 to 8 tasks are written independently from reading
  the tree (ambiguous anchors, conditional branches), with gold set by reading the code.
- Each plan is executed offline: its steps are replayed with today's operators on the
  frozen tree. Judges see the replay outputs, not only the plan, and do not know the model
  or condition. Partial plans are scored against their declared intermediate objective.
- Kill: fewer than half of the plans are executable and on target, with the sample size
  and a 95% interval stated before the run. Then the schema stays internal and recipes
  become the agent-facing form.
- Also recorded, not gates: the plan's step count against the tool calls the same agent
  makes without the schema, and an uncued arm (native tools plus today's LCI tool
  descriptions, no cue) that records whether agents pick LCI tools at all.
- Operator gaps found while designing the tasks are listed per task (no implements edge,
  no doc-section nodes, no path operator, `config.key` misses string-literal TOML).

## N2: in-request edge bandit

- Every few pops a check rates a few newly found nodes; a yield tracker counts hits per
  edge type, and the walk shifts priority toward edge types that pay for the rest of that
  request. Nothing carries over between requests.
- The hit signal is frozen before the run and independent of the walk's own scheduling
  score (a reranker or judge verdict, not "would enter the current top k"), so the bandit
  cannot reward the priors it reweights. Exploration is bounded.
- Arms: F-append plus the bandit against F-append, a cost-matched fixed reweighting and a
  cost-matched random reweighting. Benched only on sets where N0b keeps the walk, and it
  must beat the winning N0b configuration, not only F-append.
- Kill unless it gains at least +.03 on flow path recall or impact second-ring recall with
  a Holm-adjusted paired interval that excludes zero, no set loses more than .02 on its
  primary metric, and physical cost (expansions, model calls, tokens) stays within 1.5
  times F-append's. Power is checked first from the paired variance; if +.03 is not
  detectable at the available n, the slice reports that instead of a verdict.
- Flow walks average about 9.7 pops per item, so an every-few-pops learner gets few rounds
  there; the trend row is reported with the result.
- A yield-based stop is tried only as a by-product, under the stopper rule already used
  (at least 20% extra saved at matched recall, and better than a fixed step count).

## N3: chained executor with breakpoints

- Built on the exact operators (closure, `reads_key`, pickaxe) and search. If N1 was
  killed, only recipes are built.
- Needs a composite multi-step gold slice (component, config, readers), fixed before the
  run, read-only on the frozen tree.
- Counted: agent round trips, tool invocations, internal operator calls, and tokens. The
  control gets ordinary batching and a thin programmatic-composition baseline, so wrapping
  four calls in one cannot pass the gate by construction. A pilot sets n before the run
  (a powered 5-point non-inferiority needs a few hundred task pairs).
- Kill (agent A/B): against the control, the chained tool must cut agent round trips by at
  least 25% with task success no worse than 5 points below. If it does not, it stays
  internal. An uncued arm reports uptake separately.

## N4: a result filter trained on offline labels (parallel track)

- The question: does training a filter on labels collected offline beat the untrained
  judge enough to be worth it? Run while the other slices proceed; it uses N0b's harness
  and reads its outputs.
- Train on patterns, never node identities: inputs are the question, the card's snippet
  and its provenance (map, edge type, depth, first-stage origin), output is keep or drop.
  Labels come from gold matches on the bench sets plus the judge transcripts already
  collected.
- Test on a time split (questions from later commits than the training questions) and on
  held-out repositories where available.
- Cheapest form first: a small head on the local judge's outputs and the card features.
  A fine-tuned decision model (Decisio task heads on a local base model) only if the
  cheap head shows signal.
- Kill unless the trained filter beats the untrained judge by at least .02 on the primary
  metric of two or more sets with Holm-adjusted intervals that exclude zero, loses no set
  by more than .02, and costs no more model calls per question. Otherwise the untrained
  judge stays.

## Later: freshness (F1, F2)

- F1, verify on read: every answer carries the content hash of its file and is checked
  against the live file before it is returned; a stale answer is recomputed from live
  operators or marked stale. Precomputed maps become accelerators trusted only on a hash
  match.
- F2: measure staleness after real edits (no benchmark has done so: all ran on a frozen
  tree), including whether the SCIP-based maps rebuild after an edit.
- Learned components (N4 included) train on patterns, never on node identities, and are
  tested on a later commit than the one they were trained on.
