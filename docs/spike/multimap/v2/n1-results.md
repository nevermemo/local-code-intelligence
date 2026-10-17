# N1 results: will agents write chained-query plans? (2026-10-16)

Wizard of Oz study for the chained-query idea (`next-slices.md`, N1). Agents (Sonnet 5.5 and Haiku 5.5) got a
draft chained-query schema, a short recipe list and 27 tasks on the frozen tree (bfaff44): 20 derived from the gold
sets, 7 written from reading the tree. Each wrote 3 plans per task. Plans were executed offline by a deterministic
replayer over today's operators, then judged blind (two Sonnet judges, Fable on disagreement) against the gold.
Prompts, scripts, tasks and the kill rule were frozen before any plan was written.

## Verdict: killed

Unit = task x model (54 units). A unit passes when at least 2 of its 3 plans are executable (E) and on target (T).
Kill if fewer than 27 of 54 pass.

| measure | value |
| --- | --- |
| units passing E and T | **12 / 54 (22%)**, Wilson 95% 13% to 35%, task-cluster bootstrap 11% to 35% |
| Sonnet / Haiku | 8 / 27, 4 / 27 |
| units passing E alone | 46 / 54 (85%) |
| plans passing T given E | 45 / 133 (34%) |
| judge agreement (J1 vs J2) | 98.5%, kappa 0.97 |

By family: impact 8/20, flow 3/12, history 1/10, where-to 0/12. Tasks with two or more dependent steps 10/38.

So agents write plans that run (85%) but mostly do not reach the answer (34% of runnable plans). Executability is
not the problem; picking the right chain and the right node is.

## Why plans miss

- A third of the executability failures are one habit: a bare `$sN` reference without `.field`.
- Of the runnable plans, 60 of 133 stopped on a break the plan itself declared (ambiguous, empty, over budget).
- Name resolution: plans that name a type (for example `IndexConfig`) get only function candidates back, so they
  stop as ambiguous at step 1. Recorded as a candidate operator gap; not yet checked in the operator code.
- Fan-outs over many types (`<Type>::enabled` over six or seven types) return display names that the next step
  drops, or nothing.
- History tasks: the pickaxe step returned commits, never the gold commit.

Four tasks had gold reduced before the run because no operator can reach it (string-literal TOML keys, a struct
declaration under `config.key`, a document section by commit). These are operator gaps, reported separately and
neutral for the agents.

## Sensitivity (added after the verdict; not the frozen result)

| variant | units passing | Wilson 95% |
| --- | --- | --- |
| frozen | 12 / 54 | 13% to 35% |
| breaks ignored | 15 / 54 | 18% to 41% |
| bare refs repaired | 17 / 54 | 21% to 45% |
| both | 22 / 54 | 29% to 54% |

The kill holds under every variant. Not varied: the replayer's name resolution, the judges' model family (Sonnet,
same family as one planner), the task set.

## Secondary measures

- Plan size: median 5 plan steps per unit against a median 5.7 tool calls for the same agent without the schema
  (cued control). A plan is written blind and a control run sees results, so this compares size, not quality.
- Cued uptake (lci tool descriptions in the system prompt): 64% of control runs used at least one lci tool;
  traverse in 3 of 162.
- **Uncued uptake** (lci MCP server attached, no cue): 4 of 54 runs used any lci tool, 0 used traverse. Answer
  correctness in that arm was 34 / 54 by two blinded judges, mostly without lci. Agents do not reach for lci tools
  on their own.

## Consequences

- The chained-query schema stays internal. N3 builds recipes only (named server-side plans), as the kill rule says.
- Uptake is a separate, larger problem than plan quality: without a cue agents almost never call lci. N3's A/B must
  report uncued uptake, and recipe names and descriptions are the lever to measure.
- Operator gaps from the task design (type-name resolution, string-literal TOML keys, struct declarations under
  `config.key`, documents by commit) go to the operator backlog.

Cost: $14.84 of a $40 cap on the API (local SDK runner).
