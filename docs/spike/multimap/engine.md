# Multimap engine reference

Facts about the code that ships on this branch (crates `lci-core`, `lci-maps`, root `src/app`). Facts only: no plans (see [specs/](specs/README.md) for contracts and [ideas.md](ideas.md) for ideas), no results (see
[results.md](results.md)). Symbols, not line numbers. Defaults are those of `impl Default for TraverseConfig` in `lci-core/src/config.rs`, restated in `config.example.toml`.

## Node identity

One closed, versioned enum `NodeId` (`lci-core/src/node.rs`); the canonical string is the identity everywhere (maps, traces, handles).

| Kind | Canonical string |
| --- | --- |
| file | `file:<path>` |
| chunk | `chunk:<path>@<hash>#<ordinal>` (content hash plus ordinal among equal hashes in the file, `u32`; no `start_line`, so inserting lines above does not change it) |
| symbol | `sym:<lang>:...` |
| commit, test, config key, doc section, string literal | `commit:`, `test:`, `cfg:`, `doc:`, `str:` |

A Rust module path uses the Cargo crate name (`lci_core::chunk`); the directory form is an alias resolved by `SymIndex::resolve` and `lookup`. The extraction hook (`chunk::extract_with_adapter`) and the bundle store
`<data_dir>/bundles/<16 hex ws>/<2>/<30>.json` (`BundleStore::retain`, `Producer`, `bundle::sync`, payload `sym.defs@1` `StoredDeclaration`) persist declarations per file; the manifest `VERSION` stays 2. Spec:
[specs/node-id.md](specs/node-id.md).

## Maps

A map is a `Map` impl in `lci-maps` ([specs/map-trait.md](specs/map-trait.md)): `describe()`, `query()`, `expand(node, budget, view) -> edge::Expanded` (default: a seeded query), `card() -> edge::MapCard`
([specs/map-card.md](specs/map-card.md)). `Contribution`: RankList, Prior, Filter, Expansion (the registry refuses Filter maps, `RegisterError::FilterUnsupported`). `FreshnessModel`: PerFile, PerCommit, Live,
Global. `MapStatus`: Ok, Partial{note,warning}, Skipped{reason}, Failed{warning} (`Partial{None,None}` is rejected). `Need`: Embedding, Reranker, Lsp, Git, Ripgrep. `SourceLabel`: vector, lexical, lsp, scip, tags,
heuristic, git, config, doc, agent, prior (checked by `labels::check_labels` inside `Registry::run`). `EXPAND_LIMIT` is 256 edges per expansion; stored maps run through `off_thread`/`spawn_blocking` (512 blocking
threads). `EdgeKind` (14): query_match, prior, calls_or_refs, contains, imports, implements, tests, reads_key, mentions, touched, cochange, compile_error_at, near, defined_in. `WitnessClass` order: compiler, lsp,
exact, text, vector, heuristic, agent. Edge confidence is 0 to `CONF_MAX` 100 (`CONF_UNKNOWN` 50); edges sort by confidence, then dst; `Site{path,blob,line,col}`.

Traversal maps (ids as in `--maps`). Default set `MAPS`; the rest are opt-in (`OPT_IN_MAPS`), named in the maps list, and all need the tree (`--root`) except `seeds.bm25` (the SCIP graph only) and `seeds.literal` (the application; without it the call says so and goes on).

| Map id | Role | Source | Freshness | Module |
| --- | --- | --- | --- | --- |
| `seeds.hybrid` (default) | start nodes | top `seed_k` hits of the hybrid search, mapped to SCIP symbols | PerFile | `src/app/traverse/seeds.rs` (`SeedMap`) |
| `seeds.scip_names` (default) | start nodes | question words matched to symbol names; stand-in when the hybrid search is unavailable or empty | PerFile | `src/app/traverse/seeds.rs` |
| `seeds.bm25` (opt-in; in the default set only with `[run_profile.seeds] bm25`; needs no `--root`) | start nodes | BM25 (`lexical::score_chunks`) over the tokens of each SCIP node's name, qualified path and documentation; its top `seed_k` nodes go after the hybrid search's, or alone when that gave none | PerFile | `src/app/traverse/seeds.rs` (`Bm25Seeds`) |
| `seeds.literal` (opt-in; in the default set only with `[run_profile.seeds] literal`; needs the application's ripgrep, so a question with `--root`) | start nodes | `rg --fixed-strings --word-regexp` (`lexical::search_literals`) for the last segments of the names in the question's code spans; each line placed by `SymbolPlaces` (`ScipPlaces`): the symbol defined there, else the one around it, else the file of a declaration line; its nodes go after the hybrid and `seeds.bm25` ones, or alone when those gave none | PerFile | `src/app/traverse/seeds.rs` (`literal_seeds`) |
| `graph.scip` (default) | expansion | the SCIP index (`rust-analyzer scip`, file `scip_index`), read and cached in memory by file stamp; with `--root`, also an `implements` edge (kind prior 0.9) from each trait-impl member to its trait method declaration | PerFile | `scip_graph.rs` |
| `structure.contains` (default) | expansion | containment derived from the SCIP graph; lets a chunk or file start node reach its symbols | PerFile | `contains.rs` |
| `grep.live` | expansion | a search of the tree's current bytes | Live | `live_grep.rs` |
| `config.key` | expansion | config keys and the declarations that read them (needs `rg`) | Global | `config_key.rs`, `reads_key.rs` (M1.2) |
| `git.history` | expansion | commits and co-change from git history up to HEAD | PerCommit | `git_history.rs` |
| `probe.compile` | expansion | compiler errors of a patch to a function, by `cargo check` on a scratch copy (seconds per node) | Live | `probe_map.rs`, `probe.rs` (M1.3) |
| `search.lexical` | expansion | the lexical search from a node's own name | Live | `search_maps.rs` |
| `search.vector` | expansion | chunks nearest to a node's stored embedding (embedder only for a node with no stored vector) | Global | `search_maps.rs` |
| `scip.impact` | expansion | declarations that reference a symbol (reverse closure of the SCIP impact index) | PerFile | `scip_impact.rs`, `impact.rs` (M1.4) |
| `scip.implements` | expansion | the traits a type implements, the types that implement a trait, the members that implement a trait method; read from the `impl .. for ..` headers of the tree and the SCIP reference occurrences (rust-analyzer writes no SCIP relationships); traits and types the index does not define, derives and `macro_rules!` impls give no edge | PerFile | `scip_implements.rs`, `scip_graph.rs` (`derive_impls`) |
| `lsp.live` | expansion | definition, references and document symbols from a running language server (starts at the first expansion) | Live | `lsp_live.rs` |
| `exact.closure` | first stage only | declarations that reference the symbol the question names, most references first | n/a | `src/app/traverse/first_stage.rs` |
| `exact.pickaxe` | first stage only | commits found by `git log -G/-S/--grep` for a history question (20 s timeout per call) | n/a | `exact_history.rs`, `first_stage.rs` |

Other maps: `sym.defs` (RankList, PerFile; declarations from the bundle store, `symdefs.rs`) and the legacy maps `vec.chunk.code`, `lexical`, `lsp.symbols` that wrap today's three search channels
(`src/app/search/legacy_maps.rs`, `legacy_maps` and `record_through_maps`/`fuse_through_maps` behind Cargo feature `spike-maps`; `LEGACY_ORDER` keeps the f32 fusion order bit-exact). Support modules: `walk.rs`
(M0.4, hop scorer constants `W_BASE` 0.1, `W_INDEGREE_IDF` 0.3, `W_OVERLAP` 0.6), `crates.rs`, `fresh.rs`, `audit.rs`, `labels.rs`, `kit.rs`, `kit_c3.rs`, `run_record.rs`.

What persists between index passes (chunk store): per chunk the trimmed `code`, line range, `content_hash`, language and path (manifest and LanceDB columns with the vector); per file `language`, `adapter_version`,
`content_hash`; per workspace the manifest fingerprint and the table metadata `embedding_identity`, `workspace_path`, `indexed_at`. Syntax trees are not kept; a file whose hash, language and adapter version match is
not parsed. Concatenated chunks are not a lossless file (code is trimmed, empty or `{};` chunks are dropped, bytes between spans belong to no chunk); a map that needs exact text rereads the file. No ANN index is
built (no `create_index`; `Store::search` is a flat cosine scan), and every commit rewrites the workspace table (`Store::replace`, `CreateTableMode::Overwrite`). Update vocabulary: **per-file** (recomputed when the
file is reparsed: content hash, language or adapter `cache_version` changes), **per-commit** (when HEAD moves), **live** (no stored index, computed per request), **global** (a change in one file can change entries
keyed by others; needs a dirty-set recompute or an accepted staleness window).

Candidate maps from the 2026-10-12 inventory that are not built (costs are that inventory's estimates, not measurements; the reference size was 378 tracked files, 98 Rust files of about 2.1 MB, about 54k Rust lines,
188 commits, roughly 1-2k chunks from `TARGET_BYTES` 6000 and `MAX_DECLARATION_BYTES` 24000; "large repo" meant about 100x: around 10k files and 100k-200k chunks):

| Row | Map | Data source | Update | Minimal MVP |
| --- | --- | --- | --- | --- |
| 2b, 2c | vector over path+code; file-level vector | stored chunks with another `embedding_document_template` (same size as the code vectors, a second embedding pass); one row per file (about 1/10 of the rows) | per-file | second LanceDB table with its own identity; head of each file plus symbol names |
| 2d, 2e | vector over symbol signatures; second embedder | declarations extracted in the parse (1.5-3x the embedding time, estimate); any OpenAI-compatible model | per-file | needs the extraction hook and a per-file cache; second embedder waits for a service that serves two models |
| 6 | module and import graph | `mod`/`use` and import statements (reparse a few seconds here, estimate) | per-file | Tree-sitter queries run in the parse, cached per file, file-to-file edges |
| 8, 19 | agent instruction files; doc-to-code links | `AGENTS.md`, `CLAUDE.md`, `.agents/skills/**`, `README.md`, `docs/**` (about 240 Markdown files); backticked paths and identifiers | per-file | a Markdown chunker split on headings with its own table; resolve links against the manifest and symbol map |
| 11 | git blame | `git blame --porcelain` (expensive: seconds per large file) | per-commit | lazy blame for one chunk, cached by (path, blob sha) |
| 13, 15, 16 | test-to-code links; error and log strings to raise sites; entrypoints | `#[test]` functions (about 40 files here); string literals in `bail!`/`ensure!`/`anyhow!`/`warn!`; `#[tool]` handlers, REST routes, clap | per-file | name linking over test chunks (<1 s); regex over macro calls; a (kind, name, handler) list |
| 17, 18 | build graph; trait/impl hierarchy | `cargo metadata` (about 1 s); `impl Trait for Type` and `textDocument/implementation` | per-file | crate edges only; edges resolved by name through the symbol map |
| 20, 21 | ownership; cross-language boundaries | `git shortlog` (personal names: keep out of committed output); FFI, scripts (10 `.sh`, 1 `.py` here) | per-commit; per-file | skip for this repository |

Built counterparts of the other inventory rows: lexical (1) = `search.lexical` and the legacy `lexical`; vector (2a) = `search.vector` and `vec.chunk.code`; LSP symbols, definition, references (3, 3b, 4) =
`lsp.live` and `lsp.symbols`; call graph (5) = `graph.scip` (SCIP, not the Tree-sitter approximation); git co-change, recency, commit messages (9, 10, 12) = `git.history` and `exact.pickaxe`; config key (14) =
`config.key`; language, chunk metadata and source role (7, 7b, 7c) are manifest fields and the path classifier `classify_source_role`, not maps (7c is path-only, 7 roles, computed per row each time rows are
read, never stored; free, a path match per row; live, a path rename changes it; a test-role chunk is the seed set for row 13). Repository-profile gates from the inventory (a cheap pass needs only the manifest,
`git rev-list --count HEAD` and `cargo metadata`): language mix gates maps 3-6 and 18 per language server or grammar; file and chunk counts gate the expensive vector rows (2b-2e) and blame (11); commit count and
median files per commit gate co-change (9), and below about 50 commits it is noise (188 at the base commit); presence of instruction files, CODEOWNERS, `Cargo.toml`/`package.json` and test directories gates maps 8,
20, 17 and 13. The inventory's sensible MVP set for the spike: the maps that already exist (1, 2a, 3 and 3b live, 7, 7b, 7c) plus those needing one cheap new pass (6, 9, 13, 15, 16); 6, 13 (by calls) and 16 need
parse-time extraction with a per-file cache or a reparse, and 9 needs runtime git code; everything else comes in later waves.

Pre-build estimates from that inventory for the existing and proposed rows (estimates, not measurements; "here" is the reference repository above):

| Row | Map | Estimate |
| --- | --- | --- |
| 1 | lexical | no stored data; one ripgrep pass per query, roughly 10-100 ms here, seconds on a large repo; an optional stored inverted index (for example tantivy) would be about 10-30% of source size |
| 2a | vector | a 2560-dim f32 vector (the default `qwen3-embedding-4b` is 2560-dim: from the model card, not checked) is about 10 KB, so 15-20 MB here and 1-2 GB on a large repo; embedding time dominates |
| 3 | LSP symbols | server start takes seconds to minutes (rust-analyzer here an estimated 10-60 s), then each query about 10-500 ms; a snapshot of symbols would be a few hundred KB here |
| 3b | definition | one LSP request per call, about 10-500 ms once the server is up; keep live as a traversal step from a position |
| 4 | LSP references | live calls of about 50 ms to 2 s each; precomputing all references (one request per symbol) estimated minutes here; keep live from a chosen symbol, not a prebuilt map |
| 5 | call graph | Tree-sitter approximation: reparse of every file (estimated <5 s here) or an extraction hook plus a per-file cache, edge list under 1 MB; an LSP-exact graph costs minutes; outgoing edges per-file, incoming global |
| 9 | git co-change | one `git log` pass takes <1 s here; pairs are O(sum of files-per-commit squared), so commits above N files need a cap; storage a sparse pair table, KB to MB; per-commit |
| 10 | git recency | <1 s here and tiny; per-commit, the working tree's mtime is the fallback for uncommitted files; a last-touched timestamp used as a prior or filter, never as a hidden score (retrieval contract) |

## First stage

`src/app/traverse/first_stage.rs`. The first stage is the cards of the exact operators the call names (`exact.closure`, `exact.pickaxe`; each abstains with a note when nothing the question names resolves), in the
order of the maps list, then, with `answer_seeds`, the start nodes. Cards say where they came from (`origin`: exact, seed or walk) and a node is listed once, at its first place. `first_stage` joins it with the walk:
`append` (default; first stage capped at `first_k`, then the walk), `interleave` (alternate, first stage first, cut to `top_k`), `off` (the walk alone). `exact_seeds` also adds the exact cards the SCIP graph holds
to the walk's start nodes (those come from the SCIP impact index and git, not from a search of the tree: ripgrep for the names a question backticks is the separate `seeds.literal`). A continuation (call with `handle`) returns the walk alone. `exact.pickaxe` asks for `PICKAXE_UNCAPPED` 100 commits when `first_k` is 0.

Start nodes (`seed_source`): `given` (caller's node ids), `hybrid` (top `seed_k` hits, each mapped to the SCIP symbols its chunk defines, at most `MAX_SYMBOLS_PER_HIT` 3, else the innermost function around it, else
its chunk node), `hybrid+bm25` (those, then the nodes of `seeds.bm25` that the hybrid search did not give, with `[run_profile.seeds] bm25`), `bm25` (`seeds.bm25` alone answered: the hybrid search failed, found nothing or was not asked; the name-token stand-in then does not run, and a failed search's reason is in `seed_fallback`), `+literal` appended to those (`hybrid+literal`, `hybrid+bm25+literal`, `bm25+literal`) or `literal` alone (the nodes of `seeds.literal`, with `[run_profile.seeds] literal`; each with a `seed_hits` record `how: "definition" | "occurrence" | "file"`), `name_tokens` (`SEED_COUNT` 3 from the question's words; used when the caller asked for `seeds.scip_names` alone, or as the visible fallback, reason in `seed_fallback`). With `scip.implements` the source is prefixed `traits` or `traits+<source>`, where `<source>` is `hybrid`, `hybrid+bm25`, `bm25`, one of the `+literal` sources or `name_tokens`.

## Walk

Best-first search over path states (`lci-maps/src/traverse/mod.rs`). One pop expands one state through an `EdgeSource`; priority is the product of the hop values along the path; ties pop in discovery order. A node
becomes a state once and is expanded at most once. A start node is never "found by a cross-map hop".

- Hop (`frontier::hop`): a node with at most 8 offered edges (the gate) is followed whole, each edge at `kind_prior` (call 1, containment 0.5); above the gate a `HopScorer` orders them, the best `beam` (16; 1 to
  256) are kept and the edge at rank r gets `1/(1+r)` times `kind_prior`. With `[run_profile.walk] pricing = "relative"` (default `rank`, as here) rank r of n candidates (counted before the beam; a trait seed's implementors are `a` edges ranked ahead) gets `1/(1+a+r*gate/n)` instead: equal to `rank` at n = gate, so continuous at the gate for every node, and flatter for a wide fan-out (80 edges: rank 8 is 1/1.8, not 1/9); at or below the gate nothing changes, and the handle neither carries nor checks the pricing (a walk can mix pricings across resumes). Learned priors replace the guess (`priors.rs`, `VERSION` 1, `cargo xtask maps fit-priors`); a fitted tie-break (`tiebreak.rs`, `VERSION` 1,
  `cargo xtask maps fit-tiebreak`) orders results tied at the same rank under `boost` fusion only and never lifts a result over a higher one.
- Relax and alt paths: a node not yet expanded reached by a route with higher potential (relative 1e-9) is relaxed (trace `relax`; a node held at `max_depth` is `unblocked` by any route below it); a popped node
  keeps its state and the route is traced once as `alt_path`.
- Corroboration (fusion `boost`, default): rank = potential * (1 + boost * (n_maps - 1)), `corroboration_boost` 0.1 (0 to 10). Maps that found the seeds are not evidence; dependent maps (cards say `dependent`:
  `structure.contains`, `graph.scip`, `scip.impact`, one graph) count once.
- Fusion `rrf`: per evidence source rank by best potential; score = sum of `weight/(rank+2)` (`RRF_K` 2.0); equal scores order by node id; the score bound is off under `rrf`.
- Fusion `log_odds`: the same sources and weights; score = sum over sources of `weight * clamp(ln(p / (1 - p)), -CAP, CAP)`, `p` the best potential the source delivered the node with, `CAP` = `LOG_ODDS_CAP` 3.0; a source that delivered other results but not this node costs `weight * CAP`; equal scores order by node id; the score bound is off under `log_odds`.
- Budgets (all checked, none exceeded): `pops` (200) and `deadline` before every pop and inside every map call, `est_tokens` (`TOKENS_PER_EDGE` 12 per adopted hop plus the maps' own estimate) and `external_calls`
  (`EXTERNAL_CALLS` 100, shared among maps in registry order) around every expansion. A pop cut by a budget is requeued and pays for its calls once. `pops`, `est_tokens` and `external_calls` are absolute for the
  traversal; `deadline` and `max_depth` belong to one call, and the deadline excludes the hybrid seed search (its time is added once it returns; index load still counts, d362); a state at `max_depth` is recorded, not expanded. Only the outer agent can add pops and tokens (`RAISE_MAX_POPS` 200, `RAISE_MAX_TOKENS` 20000 in total).
  Physical work is counted apart (`physical_map_calls`, `physical_external_calls`).
- Score bound (`score_bound`, with `top_k`): the walk ends with stop reason `bound` (status `complete`, no handle) once `k` results exist and neither a queued state nor a result below the top `k` can rise above the
  `k`-th: `max(best frontier potential, best potential below top k) * (1 + boost * (n_maps - 1)) <= ranked potential of the k-th`, with `<` when a tie-break exists. `ANSWER_K` is 8. Other stop reasons: `complete`,
  `pops`, `deadline`, `budget`, `steer`, or a stop trigger.
- `pass_through` (off): popping a connective node (file, commit, config key) is free; the pop cap applies to the free pops too (at most twice the cap); counted in `Totals::pass_through_pops`.
- Reranker frontier (`rerank_frontier`, off): one batched call per expansion multiplies each kept hop by `rerank_floor + (1 - rerank_floor) * s` (floor 0.25; factor in [floor, 1], so the bound stays sound); a failed
  call leaves factor 1 and is counted by reason; `max_model_calls` caps the calls (absent: no cap). `lci-maps` has no HTTP; the caller supplies the rescorer.
- Edge bandit: removed (decision d365; the N2 experiment was dropped, d252, and its code is kept at tag `keep/n2-bandit`). The `[traverse]` keys `edge_bandit` and `bandit_*`, the `lci traverse` and bench flags `--edge-bandit` and `--bandit-*`, `totals.bandit*` and the trace event `bandit` are gone; a config with a bandit key fails to load naming it, and a handle that carries `bandit` is refused.
- Edge sources: `OnTheFly` (asks every enabled map whose `accepts` holds; noisy-OR of the best confidence per map) and `PreMerged` (`merged.rs`, a materialised union built once; a node it never reached gets one call
  named `GAP_MAP` = `merged.adjacency`; `audit_edge_sources` checks equality on fixtures).

Presets per question type (`src/app/traverse/presets/{explore,flow,history,impact,where-to}.json`; the steer of every preset is empty, i.e. untuned):

| Policy | Shipped default | Measured basis (2026-10-15) |
| --- | --- | --- |
| `impact` | walk off, `exact.closure` added, `first_k` 20, `seed_k` 20 | first stage alone beats the walk at 20 (N0: -.043 with the walk) |
| `history` | walk off, `git.history` and `exact.pickaxe` added, `first_k` 20, `seed_k` 20 | walk +.018, Holm .27 |
| `flow` | walk on | +.294 at 20, +.131 at 10 |
| `where-to` | walk when `first_k + top_k >= 20` | +.069 at 20, +.012 (n.s.) at 10 |
| `explore` or no policy | `[traverse]` keys alone: first stage, then the walk | unchanged |

"Walk off" means one pop (the response still has a handle) and the walk's cards are not returned. "Answers" = cards asked for, `first_k + top_k`. Over MCP the default `top_k` is 8: 16 answers without `steer` (8 seeds and 8
walk) and 8 under `where-to`, which does not walk with the default `top_k` and gets its walk from `top_k` 10. Measured in d389: the default and the threshold stay; routing where-to questions to the preset is roadmap row N100. A `[traverse.policies.<name>]`
table overrides any built-in value; `lci traverse --first-k` wins over the policy's. Recipes pin the first stage off. Defaults `first_stage = append`, `answer_seeds = true`, `exact_seeds = true` come from N0
(exact_seeds neutral). Verification and reading rules: [v2/engine-results.md](v2/engine-results.md), "Shipped defaults" and "Reading rules". Bench deltas on flow and impact predict answer quality for a card reader
(pv2: Spearman +0.699, [+0.596, +0.784], 181 items); a where-to or impact gain still needs an agent arm: [results.md](results.md).

## Steering

Ladder (`ladder.rs`): at a stop the first handler whose decision has `confidence >= tau` and was not cut short decides; rung 1 `RuleList` (the agent's own rules), rung 2 System One (a slot; a test double until S8),
rung 3 the outer agent (when no rung is confident the call ends `Stopped` with a `checkpoint` and a `handle`). Nothing of a lower rung is applied when the ladder escalates. A handler cannot raise a budget;
directives: `continue`, `prune`, `expand`, `boost`, `map_on`, `map_off`, `map_weight`, `redirect`, `emit`, plus `budget` only from the outer agent.

Rules (`rules.rs`): JSON array of `{"when": predicate, "then": action}` parsed into a closed AST (no eval). Predicates: `kind`, `path_glob`, `witness`, `map`, `score`, `fanout`, `depth`, `budget_left` (percent of
the pop budget; each takes `lt`, `le`, `eq`, `ge` or `gt`), `seen_before`, `all`, `any`, `not`. Limits: `MAX_RULES` 64, `MAX_NESTING` 8, `MAX_STEPS` 200000 evaluations per stop, `EVAL_CAP` 250 ms, `BOOST_MAX` 8,
`REDIRECT_MAX` 16, `EMIT_MAX` 200, `MAX_STEER_ACTIONS` 64. First match per frontier node wins among `continue`, `prune`, `expand`, `boost`; the other actions fire once. Confidence is 1.0 when a rule matched, 0.0
when none did, time ran out, or two matched rules contradict. `lint.rs` adds warnings (a rule that prunes everything or cannot fire, opposite rules, `stop_every` above the pops, a zero stop count); none is an error.

Stops (`StopPolicy`; 0 switches a trigger off; all bounded by `max_stops` 4): `every_pops`, `budget_share`, `on_disagreement` (a fuzzy map offered a node a structural map did not), `on_stagnation`, `likely_complete`
(Lincoln-Petersen estimate over the two most productive maps; a hint, not a bound). A checkpoint holds cards (`CARD_K` 12, about 40 tokens each, `CARD_CHARS` 160), map stats, regions and disagreements
(`CHECKPOINT_FRONTIER` 256).

Paths on cards (`[run_profile.traverse] card_paths`, default off): a card has no ancestors, so by default a reader sees only the last hop (`via`, `witness`, `depth`).
With the key on, every card of `results` and of `checkpoint.answer` that the walk reached also has `kind` (the edge kind of the hop that found the node, `State::kind`) and `path`, every hop
from its start node with its map, edge kind and witness (`depth + 1` entries). `kind` is on `State` (set at discovery and on a relax, left by an `alt_path`); a snapshot writes it only under the key
(`Traversal::card_paths`), so a continuation under the key from a handle made with it off shows `kind: null` for the earlier hops. `card_json`, the checkpoint hash, the trace and, with the key off, the handle are untouched;
`visited_cards` and `frontier` (steering views) and first-stage cards carry no path.

### Written meanings

Three names that appear in arguments, traces and responses, with what each does and does not promise.

- `likely_complete` has three faces.
  - Argument: `steer.likely_complete`, a whole percent 1 to 100 (`StopPolicy::likely_complete`, 0 is off; echoed in `effective_policy.stops`). Given alone it starts the ladder like `stop_every`.
  - Trigger: holds when the estimate's `fraction * 100 >= P`, once per traversal (`Tally::complete_fired`, kept in the handle), counted in `max_stops`. Like every trigger it consults the ladder; with no matching rule the call ends `stopped` with `stop_reason: likely_complete`, a checkpoint and a handle.
  - Estimate (`stop::likely_complete`): Lincoln-Petersen over the two maps with the largest `found` sets (ties by map id): `estimate = n1 * n2 / refound`, `fraction = found / estimate`, absent until the two share a node (so the trigger cannot fire before then). `fraction` is at most 1 by construction (`n1 + n2 - refound <= n1 * n2 / refound`).
  - `found` is what a map offered, adopted or not: a seed counts for the map that found it, and every destination offered at an expansion counts after the cleared filter and before the on-path filter, the gate and the beam. Maps built on one graph (`graph.scip`, `structure.contains`, `scip.impact`) are separate finders here though their samples are not independent, so the fraction reads high. It is a hint, not a bound.
  - Shown only in a checkpoint: `checkpoint.likely_complete {maps, n1, n2, refound, found, estimate, fraction}` and `checkpoint.regions.unknown` (`round(estimate - found)`, absent without an estimate). A checkpoint exists when the call ends `stopped` or at a cap with work left; an answer that ends `complete` or `bound` carries no estimate.
- Score bound (`stop_reason: bound`): the walk ended because no state in the frontier and no result below the top `k` can rise above the `k`-th result (the formula is in the Walk section). The `k` results returned are the final top-`k` set under this profile; a longer walk could only reorder them, not change them. The status is `complete`, with no handle, checkpoint or estimate; it is checked before the stop triggers, so a stop due at that pop is not consulted. `complete` with stop reason `complete` is another end: the frontier ran dry or every target was reached, no estimate. Off under `score_bound = false` (`lci traverse --no-score-bound`), `rrf` fusion, `claim` groups and targets; a resume must repeat the same `k`.
- `tau` (`[traverse] tau`, `steer.tau`): the confidence a ladder rung needs to decide a stop. The product builds the ladder with the rules rung alone (System One is a test double until S8), whose confidence is 1.0 when a rule matched, the list was not cut short and no two matched rules conflict, else 0.0. So `tau` is a switch today: every `tau` in `(0, 1]` behaves the same (a clean rule match decides, any other stop escalates to the caller), and `tau = 0` makes an unmatched list decide with no actions, so no stop escalates to the caller (a rule's own `stop` and a list cut short still end the call). A graded `tau` needs a graded rung (System One).

`steer` object of `search_code` (`SteerArgs`): `policy` (a preset name), `rules`, `script` (reserved, not available), `ladder` (only `rules` and `agent`), `max_stops`, `tau`, `stop_every` (default spacing
`pops/(max_stops+1)` when rules are given), `on_disagreement`, `likely_complete` (1 to 100), `actions` (only with `handle`, the answer to a checkpoint: `[{"node"?, "action"}]`; node actions need a `node`). The
response's `effective_policy` shows what ran. `BudgetArgs` {`pops`, `max_depth`, `est_tokens`, `deadline_ms`} can only lower the `[traverse]` caps.

Handles (`handle.rs`, whose module doc is the spec): `h` and 16 lowercase hex digits (the first 16 of the SHA-256 of the snapshot JSON; the app refuses an empty or over-64-character handle before the store sees it), file `<data_dir>/traverse/handles/<handle>.json`, written once and never removed, bound to the workspace and index it was made on (`check_identity`: the canonical root, and the index path, size and modified time, so a re-index refuses it). A snapshot
holds states, queue, held-back states, counters, cleared set, map weights and switches, pop count, paid costs, traced `alt_path` pairs; not the event log, edge source, scorer or handler. A resume must repeat the
boost, `top_k`, priors, tie-break file or reranker identity, or is refused; no TTL, no cap on live handles. Stable policies: [v2/policies/README.md](v2/policies/README.md). Design of the unbuilt ops
and tools: [specs/steering-ops.md](specs/steering-ops.md).

## Recipes and compose

`src/app/recipes.rs`; five fixed chains over the SCIP index and the config-key map, off by default (`[recipes] enabled`): `config_readers(key)`, `impact_rings(symbol, rings)` (ring 1 = the exact reverse closure,
same index as `exact.closure`), `flow_path(from, to, max_depth)` (shortest call paths in `graph.scip`), `flow_from(entry, k)` (the walk of `lci traverse` with the `flow` policy), `config_neighbours(symbol)`. A name
resolves through the index's declarations; more than one match stops with `ambiguous_anchor` and the candidates. Each answer lists nodes, steps, a witness per result and a stop reason; every cut is listed in
`stop.unexplored`. `lci_compose` (`recipes/compose.rs`, `[compose] enabled`): at most 6 steps with `$sN.field` references and no model between them; operators `seeds.scip_names`, `exact.closure`, `scip.impact`,
`graph.scip`, `config.key`, `set.union`, `set.intersect`, `set.minus`, `pick`. `flow_judge` (off) adds only a note: no judge is wired in. Verdicts: N3 "recipes survive" on recipe-shaped tasks only; N3b NULL (0 of 80
runs called a recipe tool off-shape, +11.6% input tokens); recipes stay opt-in ([v2/n3-results.md](v2/n3-results.md)).

## RunRecord and traces

Schema `lci.runrecord/0` (`lci-maps/src/run_record.rs`, schema file `specs/runrecord-0.schema.json`; hand-written checker): a spike-feature writer and an offline harness are built, no production writer
([specs/run-record.md](specs/run-record.md)); every `run` line carries a `profile` stamp (run-profile hash and key names, embedding-identity hash, hashes of the traverse priors and tie-break files). The traversal writes its own trace, JSONL at `<data_dir>/traces/<name>.jsonl` (`trace.rs`): no wall-clock values (equal inputs give byte-identical files), hashes are the
first 16 hex digits of SHA-256. Events: `start`, `expand`, `hop`, `relax`, `alt_path`, `stop` (reason and all triggers that held, hash of the checkpoint), `steer` (`rung`, `handler`, `confidence`, `actions`,
`outcome` `applied`, `uncertain` or `returned`; cites `inputs_hash`), `return` (`escalated`, `handler_stop`, `budget`), `bound`, `resume` (names the handle), `end` (totals). `--out` writes `traces/`
and `handles/` under another directory.

## Configuration keys

`[traverse]` (`TraverseConfig`; comments and ranges in `config.example.toml`): `enabled` false, `scip_index` "index.scip", `max_pops` 200, `max_depth` 6, `max_est_tokens` 20000, `deadline_ms` 5000, `max_stops` 4,
`tau` 0.8, `corroboration_boost` 0.1, `fusion` "boost", `pass_through` false, `rerank_frontier` false, `rerank_floor` 0.25, `rerank_scores` "probability" (or "logit"), `max_model_calls` absent, `score_bound` true,
`priors` and `question_type` (both or neither, absent), `tie_break` (absent), `seed_k` 8 (1 to 40), `first_stage` "append", `first_k` 10 (0 to 200), `answer_seeds` true, `exact_seeds` true, `beam` 16.
`[traverse.policies.<name>]`:
`walk`, `walk_min_answers`, `extra_maps`, `first_k`, `seed_k`. `[recipes]`: `enabled` false, `flow_judge` false. `[compose]`: `enabled` false. Requests can lower the caps, never raise them. Embedding default model
`qwen3-embedding-4b`; ports and the reserved-port rule are in the top-level README and `AGENTS.md`.

## Surfaces

- `search_code` with `mode = "traverse"` (only when `[traverse] enabled`; otherwise schema and instructions are those of a server without the mode and the request is an error): extra args `budget`, `steer`, `handle`
  (continuation; the query is then ignored); `top_k` limits the returned nodes (`MAX_TOP_K` 40). `src/app/traverse/mod.rs` `run()` serves it and `lci traverse`.
- `lci traverse --scip <index> [--root <tree>] (--question | --seeds | --handle [--steer])`, flags `--maps`, `--search-profile`, `--policy` (a preset name, a JSON file, or inline JSON: an array of rules or the
  `steer` object), `--stops`, `--pops`, `--max-depth`, `--top-k` (default 8), `--fusion`, `--pass-through`, `--rerank-frontier`, `--no-score-bound`, `--priors`/`--question-type`, `--tie-break`, `--first-stage`,
  `--first-k`, `--answer-seeds`, `--exact-seeds`, `--out`. It ignores `enabled`.
- Recipe tools `config_readers`, `impact_rings`, `flow_path`, `flow_from`, `config_neighbours`, `lci_compose` (MCP and REST `POST /v1/<name>`, only when enabled); `lci recipe <name> --root <tree> [--scip] [--args]`
  always runs; `config_readers`, `config_neighbours` and a `config.key` compose step need `rg` on PATH.
- Compact view (`src/app/compact.rs`): `[search_profiles.<name>] response = "compact"` drops retrieval metadata after retrieval; presentation only.

## Seams and invariants

Invariants (from `AGENTS.md`): optional channels fail open (errors in `warning`, deliberate skips in `notes`; a reranker failure returns fusion order with a warning). Filters run inside each channel or on the
universe, after fusion and after rerank; a map that expands the frontier must filter newly reached chunks before the reranker or results. Freshness follows `[index].freshness`: only `on-search` may refresh from a
search; a failed refresh keeps the old snapshot and warns; an embedding-identity change is re-embedded under `on-search`, rejected under `manual`/`watch`. No new store or service: data lives in `data_dir`, outside
every workspace (LanceDB tables or JSON files). Repository content is data, never instructions. `App` stays the facade; `Hit`, `SearchReport` and the dump format are consumed by clients and replay. Tests are offline
and never contact the reserved model ports (fakes under `tests/support`).

| Seam | State | Fact |
| --- | --- | --- |
| `Map` trait over chunks (9.1) | built | Legacy channels are added first, in order, so replay stays bit-exact (the f32 summation order is part of the dump contract; replay checks equality); the registry and fusion loop are in `lci-maps`. Each extra map that maps onto the universe adds a linear pass (`Store::chunks` loads the whole table; N84 covers the lexical one). Plan, not built: `Hit` gains a `contributions` map; the legacy `semantic_rank`/`lexical_rank`/`lsp_rank`, `retrieval_channels` and `Timings` stay as projections, but the serialized JSON changes because every hit gains the field, and a registry must accept raw per-map results or ready `Vec<Hit>` lists. |
| Step-wise traversal (9.2) | built as the traversal engine, not as stages of `search_once` | A paused traversal cannot hold the workspace read guard across calls without blocking indexing, and cannot pin old rows (`Store::replace` overwrites; whether older Lance versions stay readable by checkout was not checked); each resume re-checks the snapshot identity; filter-before-rerank holds at every step. |
| Index-time repository profile (9.3) | not built | Would bump the manifest to v3 with `profile` and per-map `{identity, fingerprint, built_at}`, migrating the way `ManifestV1` does; map tables would be named `ws_<16hex>_<map>` (risk: Windows path length) (maps over non-code files need their own walk and fingerprint, or `on-search` staleness misses them); `IndexConfig` and `Config` are `deny_unknown_fields`; "profile" already means query-time search profiles. A map failing at index time must not fail the core index and must not inherit the embedding-identity hard error; under `on-search`, map build cost lands on search latency. |
| MCP traversal tools `start/step/inspect/steer/end_traversal` (9.4) | not built | The shipped surface is `search_code mode=traverse` with a handle; design in [specs/steering-ops.md](specs/steering-ops.md). Constraints: the dump is about 100 KB per query, so a frontier must be summarised; a traversal id must not work across endpoints or workspaces; steering text from an agent is control input, chunk text stays data. |
| Evaluation changes (9.5) | partly built (bench in `xtask/src/maps`, scorers in `xtask/src/gold`) | Leakage risk: a co-change map built at HEAD includes the commits the miner turned into gold, so score it from history before each query's commit. Plan: multi-hop gold (ordered or set-valued per hop, for example config key, then reader, then test) with a scripted steering policy so runs are reproducible and pairable by `compare` (a free agent loop belongs to the agent A/B); per-map contribution (`CandidateTrace`/`CandidateArm` keyed by map, legacy columns kept) with per-map unique gold (gold reached only through map M) and leave-one-map-out replays; step metrics (frontier gold recall at each checkpoint, steps and rerank tokens spent); bit-exact replay of old dumps must keep working. |

Merge conditions M1 to M7 are met on the integration branch (2026-10-13; log: d212, d213, d214, d204). Still true as constraints: one LSP orchestration (`App::applicable_lsp_adapters`, `lsp_symbol_queries`,
`App::lsp_channel`, `lsp_over_budget_note`, `lsp_failure_warning`) and one chunker, with a golden record of 44 chunks; spike entry points sit behind Cargo feature `spike-maps` (`#[cfg(any(test, feature =
"spike-maps"))]`, Full test run 812 = 811 + 1); stored maps run on `spawn_blocking` (300 ms budget, 250 ms slack, 512 blocking threads); C5 labels are checked inside `Registry::run`; the hand-written schema checker
is kept. `SnapshotView::new` timing (M4, partly met; example `lci-maps/examples/snapshot_view_timing.rs`; release, 50 runs, 4 cores, cloud machine): 4048 chunks p50 3.94 ms, p95 4.24 ms, max 5.60 ms; ten times
(40480 chunks) p50 74.67 ms, p95 89.75 ms, max 91.88 ms; debug 22.18/26.03/28.49 ms and 246.67/293.69/339.35 ms (about 19x release, 11x debug for ten times the chunks). The 50 constructions run back to back on the same input, so caches are warm; one construction per search may cost more than these p50s. Not measured: per-query timing over
`evaluations/mined/self.toml` and the A/B's largest repo, and the owner-set acceptable share. Before the registry replaces `fuse_hits`, one real candidate dump should replay with zero refusals (owner's machine); the
C4 replay auditor is ignored in the Full run (CI has no history). Owner decisions: the record path stays in `legacy_maps.rs` until merge; full query text is kept in local RunRecords; no profile or reranker step.

## Known gaps

- No exact-literal seed channel: traverse cannot start from an exact string match (a grep-style step); `grep.live` only expands from nodes the walk already has. A building block to add (owner direction, 2026-10-17).
- No SCIP index in the R1 and external-repository evaluation trees, so `traverse` there falls back to chunks (R1 stage 2 never measured the walk, d333); the frozen tree `bfaff44` has one, and the walk benches in [v2/engine-results.md](v2/engine-results.md) used it. "Which types implement this trait" was never measured (d315); its hypotheses (no SCIP in
  the trees, implementation relationship not ingested, no `textDocument/implementation` request) are unchecked.
- Only Markdown (`.md`) is indexed as documentation: chunks per heading section, no `doc:` nodes in any map; no `.rst`, `.adoc` or `.txt` adapter (18 supported extensions).
- The hop `kind_prior` table (call 1, containment 0.5) is a guess unless a fitted table is given (`priors`); the System One rung (S8) is a slot with a test double; the pre-merged edge source has no `[traverse]` key.
