# Recycled research for the multimap spike

Status: draft notes. Everything below summarises earlier internal research (not in this repository). Numbers come from one Rust repository (this one, at one commit) and are not
reproducible from the repo alone. They are conditions for the synergy study, not verdicts on any map.

## What was recycled
- Identity: a stable per-declaration key (language, module path, container chain, name, kind, disambiguator) matched the compiler's SCIP symbols 1:1 on 2,440 declarations and collided on
  0 of 1,358 keys here. Keys that omit the impl header would collide for 3-5% of declarations in unlike code. Use one NodeId space with this kind of key; keep a binding from key to content.
- Freshness: per-file derivation keyed by (blob hash, producer) with no paths inside matched a full rebuild at every one of 114 replayed commits (publish p50 12-19 ms). Bundles that embedded paths
  failed on a commit that moved 18 files. Reverse indexes (name to referrers, path to referrers) had to be resident state. Treat the equivalence auditor as a per-map contract test.
- Precise edges per edit (Rust): `textDocument/semanticTokens` plus `definition` on function and method tokens refreshed a file's outgoing call edges at p95 0.74 s with recall 1.00 against SCIP;
  `callHierarchy` alone missed 16% of call pairs (94% of its misses were inside macro arguments). A warm rust-analyzer held 3.5 GB and took 1-2 minutes to start.

## Conditions learned from failed experiments
- Tree-sitter tags as a call/reference backbone: recall 0.12, precision 0.88 (an owned query reached 0.23 / 0.93). False positives were method calls on external types. Label call maps by edge source.
- Graph-built cards as cross-encoder input lost 0.03-0.07 MRR; the same graph context helped readers of cards (interaction +0.05 nDCG@10 mined, +0.11 core). The reader type is a condition.
- Vector-neighbour hops did not find co-edited partners (0 of 94 in the top 8 neighbours); gain over a size-matched random list was +8 points with weak structure and about 0 with precise structure.
- Co-change lost to a plain churn list when rescuing items missing from a noisy pool (MRR 0.14 vs 0.38), but beat it with clean coupled seeds (+0.09 to +0.15 MRR). Always include the churn control.
- Neighbour injection after a reranker and a pseudo-query "no embedder" mode failed against no-graph controls. The fast end was bound by the real query embedding.
- Query shape mattered in every run: keyword vs descriptive queries, short vs long queries, files that every commit touches (entry points, test harnesses).

## Method to reuse
- Pre-register the threshold, include no-graph and random controls, report paired bootstrap intervals, freeze the tree, and make leak scans code assertions (a last-change subject leaks the answer
  when the query is a commit message). Test interactions with 2x2 arms rather than cumulative arms.

## Gold sets (internal, not in the repo)
- Seven Rust gold sets (about 630 items) cover flow paths, impact, where-to, architecture, history, instructions and agent tasks, all derived from a SCIP index of one frozen commit
  (`bfaff44`, an ancestor of this branch). Maps built from the same SCIP data would be circular against them. No gold exists for other languages.

## Map and merge ideas worth testing
- Portals through shared keys; other maps (docs, commits, rules) as entry points into the code map; a vector gate on edge choice; two beams meeting for paths; typed next moves with token estimates.
- Merge variants: one typed adjacency with provenance and witness-agreement confidence (pre-merged), on-demand vector neighbours (on the fly), incrementally maintained views, and a workload-driven
  advisor that picks views by benefit per unit of RAM and maintenance (materialised on demand).

## Overlap with the spike's novel list
- Prior art exists for hierarchical routing (road-network style, rejected below ~200k chunks), as-of-commit queries, decayed relevance and PageRank, anytime reranking under a deadline, a workload-driven
  view advisor, and agent-defined traversal programs. Partial: disagreement between sources (as diagnostics), cleared/negative evidence, per-session seen sets. Not found: fork/join traversal for parallel agents,
  per-map cards, pause-and-steer checkpoints, fog-of-war style region states, flow fields, replay-trained steering on the engine's own traces.

## Storage note
- `AGENTS.md` forbids an additional database. A ledger on an embedded KV store won an internal bake-off narrowly (4.3 vs 4.2) over per-file bundles on LanceDB; the spike should stay on Lance, files and
  in-memory projections unless that rule is changed.

