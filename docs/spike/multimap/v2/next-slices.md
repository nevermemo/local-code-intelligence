# Spike v2: next slices (2026-10-16)

Direction set with the owner on 2026-10-16, after bench-1023 and the S8j roles step 1.
Every kill rule below is fixed before its slice runs; a slice that misses its rule is
recorded as killed with its numbers, not tuned until it passes.

## Why the direction changed

Measured so far (`engine-results.md`, `s8j-roles-results.md`): the gains come from the
first stage (exact operators and search seeds) and from filtering results, not from the
steered walk. Search alone beats the walk on where-to, one hop beats it on flow, an oracle
steer gains little over a filter of the walk's results, and every stop rule tried (strict
bound, the model's "enough?", a model-chosen step count) failed its rule. The one decision
model role that survived is the judge used as a result filter.

## The owner's proposals

The owner proposed two ideas that set the order below:

1. **Adapt inside the traversal instead of training first.** Start from a good entry point
   (named by the agent, or found by LCI with agentic-search style methods), start with
   little information, update and adapt mid-traversal. The worry it answers: a learned
   component trained offline goes stale when the code changes, and an agent should never
   use a weaker tool while a component learns.
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

## Order

N0 first, then N1, N2, N3 in the owner's order. N0 and N1 run in parallel; N2's code is
written in parallel and benched after N0. Freshness work (F1, F2) comes much later: the
spike does not work with changing code yet.

| slice | what | depends on |
| --- | --- | --- |
| N0 | first-stage-only arm; first-stage agreement check | - |
| N1 | chained-query plan study, no executor (Wizard of Oz) | - |
| N2 | in-request edge bandit with a yield tracker | N0 for its baseline |
| N3 | chained executor with breakpoints, or recipes only | N1's verdict |
| F1, F2 | verify-on-read freshness; staleness after real edits | later |

## N0: does the walk add anything after the first stage?

- Arms: F-first (the first stage alone, no walk) against F-append, on the bench-1023 setup
  (release build, same maps, same deadline, 95% paired bootstrap).
- Decision, per set: the walk stays in the default path only where F-append beats F-first
  with an interval that excludes zero on the set's primary metric. Where it does not, the
  walk becomes opt-in for that kind of question.
- Agreement check (offline, from the bench outputs): does the overlap between the exact
  answers and the search seeds predict whether the walk adds a gold item? Kill as a
  "skip the walk" signal if its AUC is below .65.

## N1: will agents write good query plans? (Wizard of Oz)

- Agents get a draft chained-query schema and a short recipe list, and real tasks drawn
  from the gold sets on the frozen tree. They write plans; nothing executes them.
- Each plan is scored offline: executable (valid schema, references resolve, every
  operator exists) and on target (running its steps with today's operators would reach
  the gold). The task list and the rubric are fixed before any agent runs.
- Kill: fewer than half of the plans are executable and on target. Then the schema stays
  internal and recipes become the agent-facing form.
- Also recorded, not a gate: the plan's step count against the tool calls the same agent
  makes on the task without the schema.

## N2: in-request edge bandit

- Every few pops a cheap check rates a few newly found nodes; a yield tracker counts hits
  per edge type, and the walk shifts priority toward edge types that pay for the rest of
  that request. Nothing carries over between requests.
- Arm: F-append plus the bandit against F-append.
- Kill unless it gains at least +.03 on flow path recall or impact second-ring recall
  with a paired interval that excludes zero, no set loses more than .02 on its primary
  metric, and pops stay within 1.5 times F-append's.
- A yield-based stop is tried only as a by-product, under the stopper rule already used
  (at least 20% extra saved at matched recall, and better than a fixed step count).

## N3: chained executor with breakpoints

- Built on the exact operators (closure, `reads_key`, pickaxe) and search. If N1 was
  killed, only recipes are built.
- Needs a composite multi-step gold slice (component, config, readers), fixed before the
  run.
- Kill (agent A/B): against the same agent with single-step LCI tools, the chained tool
  must cut tool calls by at least 25% with task success no worse than 5 points below. If
  it does not, it stays internal.

## Later: freshness (F1, F2)

- F1, verify on read: every answer carries the content hash of its file and is checked
  against the live file before it is returned; a stale answer is recomputed from live
  operators or marked stale. Precomputed maps become accelerators trusted only on a hash
  match.
- F2: measure staleness after real edits (no benchmark has done so: all ran on a frozen
  tree), including whether the SCIP-based maps rebuild after an edit.
- Learned components train on patterns (question and snippet in, decision out), never on
  node identities, and are tested on a later commit than the one they were trained on.
