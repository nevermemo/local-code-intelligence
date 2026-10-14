# Multimap traversal: idea catalogue

Status: a catalogue of candidates, not a plan. An idea is built only after a measurement shows its premise holds
(the owner's working principle in `docs/development/roadmap.md`, "Strategy checkpoint": measure a premise before
designing on it, prefer small falsification tests to idea volume). The agent A/B (I-10) decides first whether any of
it is pursued; on CONTINUE, each map or interface is measured against native tools plus git and a language server,
and only the winners are built (the roadmap's "Long-range map", branch 2).

Sources:
- the owner's design;
- the coordinating agent's proposals;
- a six-lane idea swarm, each lane forced into a different field, then a synthesis and an independent refutation
  pass by another model family;
- earlier internal research (not in the repository).

"Prior" means the earlier internal research already had the idea, "partial" means it had a related one, and "new"
means a keyword search found nothing. External citations are from memory and must be checked before they are quoted.

## Core (owner)
- **Many maps over one node-identity space.** The nodes are file, chunk, symbol, commit, test, config key, doc section
  and string literal. Portals connect maps through shared nodes or explicit cross-granularity links.
- **Pause-and-steer checkpoints.** The traversal stops at checkpoints so a thinking agent can inspect it and steer:
  prune, expand chosen nodes, add or drop a map, change weights. New: the earlier research had only pagination
  cursors.
- **Agent-assisted storage selection at index time.** Partial: an automated access-path advisor existed. The agent
  choosing from a menu with evidence is new.

## Game-AI analogies
| Id | Idea | Status |
| --- | --- | --- |
| G1 | Hierarchical pathfinding: coarse map first, then refine | prior (judged unnecessary below ~200k chunks) |
| G2 | Influence maps: decaying relevance fields over the graph | partial (personalised PageRank, decay) |
| G3 | Fog of war: explored, seen and unknown regions shown to the agent | partial (seen-sets, coverage) |
| G4 | Flow fields: precomputed "toward the answer" directions per question type | partial, weak |
| G5 | Anytime time-slicing: the best answer so far at any budget | partial (reranker deadlines only) |
| G6 | Trace replays as training data for a steering policy | partial (deferred in earlier work) |

## Engine ideas
| Id | Idea | Status |
| --- | --- | --- |
| N1 | Ephemeral query-time maps defined by the agent at a checkpoint (a regex, a commit filter, a path set) | partial |
| N2 | Fork/join traversal: split the frontier into hypotheses for parallel subagents, then join the evidence | new |
| N3 | Map cards: per-map cost, coverage, freshness and "good for", updated from benchmarks, read by the traversal and by storage selection | partial |
| N4 | Time travel: traverse as of a commit, or along history | prior for as-of, partial along history |
| N5 | Disagreement between maps shown as the best steering point | partial, weak |
| NM | Negative maps: cleared regions that are not revisited | partial |

## Proven elsewhere and compatible
- **P1 Contextual chunk headers.** Each chunk gets its file, enclosing module or impl, and function signature
  prepended before embedding and lexical indexing. Published "contextual retrieval" results report large cuts in
  retrieval failures. Partial (a passing mention only).
- **P2 Coverage-based test-to-code map.** Per-test coverage gives exact test-to-function edges (test impact analysis).
  New.
- **P3 Spectrum-based fault localisation.** Given a failing test, rank functions that failing tests run more than
  passing tests (Ochiai, Tarantula). It reuses P2. New.
- **P4 Personalised-PageRank repo map.** A token-budgeted signature outline ranked around the files in focus, as some
  coding assistants already ship. Prior as an idea, never built.

## Untested
- **U1 Capture-recapture completeness.** When independent maps keep re-finding the same nodes, estimate how much is
  still unseen and show it at checkpoints. New.
- **U2 Compiler-in-the-loop impact probe.** Apply the change in a scratch copy, run the type checker, and treat the
  errors as exact impact edges. New.
- **U3 Bidirectional meet-in-the-middle.** Search forward from the question seeds and backward from answer-type
  anchors (entry points, tests, an error string). The meeting path is the explanation. Partial.
- **U4 Speculative steering.** Pre-expand the likely steers while the agent thinks. Partial.
- **U5 Steer feedback as map credit.** Nodes the agent marks re-weight the maps that found them, and the map cards
  are updated. Partial.
- **U6 Dependency portals.** Dependency sources and docs become maps that traversal can cross into. Prior (a
  lockfile-pinned dependency signature tier).

## Idea swarm: 12 kept after refutation
Six lanes (databases, debuggers, distributed systems, cartography and game engines, cognitive science, ML systems)
produced 42 ideas. The synthesis kept 22 and ranked 15. The refutation pass kept 12 with narrower contracts and
dropped 3.

The refuter's summary: the value is disciplined integration and evaluation, not novel mechanisms. Most novelty
scores dropped by about 2 points.

| # | Idea | Change required by the refutation |
| --- | --- | --- |
| S1 | Witnessed portal algebra: sequence, union and intersection that keep the pivot nodes as witnesses; a scope-widening hop is typed as such, not as evidence | implement a bounded subset; keep provenance and costs separate |
| S2 | The agent as a priced operator: the engine owns joins and budgets, the agent judges bounded evidence, and its tokens are charged to the plan | judgments are optional and caller-supplied; no generation dependency |
| S3 | Yoked steering studies: active, yoked-passive and fixed-policy arms with the same answering agent | an evaluation design, not causal attribution; counterbalance and report real cost |
| S4 | Snapshot-forked counterfactual steer scoring: replay K rounds with and without a steer | gold scoring for the evaluator only; online estimates are labelled surrogates |
| S5 | Watchpoint-triggered pauses: invariants such as cost, role change, granularity crossing and freshness lag raise a checkpoint | throttle soft watchpoints; budget and freshness limits cannot be skipped |
| S6 | Role-typed raise sites: "why this error?" edges typed raise, wrap, log or assert, with the guard span | claim syntactic roles only; measure per language |
| S7 | Residual-plan checkpoint packet: frontier items carry completed-operation masks; the packet is about 1k tokens; the steering verbs are validated patches | release locks before a pause; revalidate the snapshot on resume |
| S8 | Equality-controlled three-form join crossover: one logical map pair stored pre-merged, joined on the fly and materialised, with identical results required | prove equivalence first, then measure budget-limited differences |
| S9 | Canary-measured storage menu: build candidate maps on a sample; the choice is a replayable ticket | no fixed sample rule; unattended indexing reuses the ticket and never waits for an agent |
| S10 | Demand-driven reachability from an error message through raise sites and callers to entry points | name it candidate reachability; keep confidence and unresolved dispatch |
| S11 | Entry beacons: handler, then registered route, mounted router and activation, with witnesses | registration evidence only; narrow framework adapters; never run the application |
| S12 | Message-key gazetteer: normalised aliases for error strings, routes and command forms across languages | aliases seed candidates only, never cross-language edges |

Dropped or folded:
- the ddmin minimal-frontier oracle (at most a failure reproducer);
- portal fan-out credits (folded into the budget engine);
- patch-level steering (folded into S7 and G1).

## Combinations to test together (with the condition where synergy should appear)
- **A. Steer lab (S7 + S4 + S3).** Flow, architecture and impact questions where the maps disagree, with a tight
  frontier budget. Where-to questions serve as the negative control. Measure the interaction on recall at equal total
  tokens, agent tokens included.
- **B. Error to entry (S12 + S6 + S10 + S1 + S2).** Error-string questions with decoys: the same message in a logger,
  a raise and a test. The agent gate should add value only when role typing leaves two or more candidates.
- **C. Merge crossover (S8 + S5 + S9).** Pairs used repeatedly, high fan-out hubs and edit-heavy workloads. Below
  about 5k chunks, on-the-fly joins should always win (the negative control).

## Live agent map (owner): ownership and borrowing for parallel agents
- **The idea.** A live map over the same node ids records which agent reads or writes which file or symbol, with
  Rust-like semantics:
  - shared read borrows with a reader count;
  - an exclusive write borrow;
  - weak watches;
  - leases released on exit or by TTL;
  - task-scoped lifetimes;
  - pinned nodes.
- **Borrow regions are semantic.** Coupled nodes (caller and callee, trait and impl, config key and reader, test and
  code) conflict even without a textual merge conflict.
- **Enforcement.** Agent tool hooks: an edit asks for a write borrow, a read records a read borrow. Cloud agents get
  launch-time forecasting and landing-time validation.
- **Rollout.** Advisory first, then enforced where the measurements justify it. New.
- **Supporting ideas:**
  - A1, a conflict forecaster and wave scheduler for parallel tasks;
  - A2, stale-read signals to weak readers (partial);
  - A3, an optimistic copy-on-write mode with semantic validation at merge;
  - A4, a living documentation layer (below);
  - A5, declared lifetimes and scope guards from each task's allowed-file list.

## Living documentation layer (owner): notes, AI files and human docs with a drift watch
- **Three text maps over the same code nodes.** Human docs, AI instruction files and agent notes. Each is sections with
  typed mentions: path, symbol, command, config key, tool name, and a value next to a key.
- **Notes.** Anchored to a node and a content hash, with a TTL and confirmations. A stable, confirmed note that no
  document covers becomes a proposed documentation edit for a human to approve.
- **Translation rules.** Declared in the repository. Each has a source, a traversal, a target, an overlap metric, a
  threshold and a severity. The rules:
  - mentions resolve;
  - code entity sets match the documentation tables both ways;
  - AI and human sections on the same footprint agree;
  - each rule has a single owner;
  - notes expire or retire.
- **Overlap scores.** Jaccard and directional coverage between the node footprints that sections reach, plus claim
  agreement. Thresholds are calibrated on the current tree, and alerts fire on drops.
- **Regular checks.** Incremental on the index change feed, with a watchpoint, a digest, an MCP tool and a CI check.
  LCI proposes edits; it never edits human docs itself.
- **Gold set.** A real non-code sweep of this repository.
- **Status.** Partial: documentation sections as nodes and typed config/doc records. The drift watch between AI and
  human docs is new.

## Default design decisions (the spine plan may revise them)
- **Checkpoints.** The engine holds frontier state for a bounded time and returns a resumable handle.
- **Agent arms in benchmarks.** Scripted agents only; no product dependency on a generation model.
- **Non-Rust coverage.** Small hand-labelled fixture sets, with Python and TypeScript first.
- **Storage.** The existing vector tables plus files. A new database needs the owner's decision, because the project
  rules forbid one today.
- **Steering cost.** Agent tokens count against the traversal budget in every steer score.
