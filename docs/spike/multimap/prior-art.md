# Multimap traversal: prior-art survey

Generated: 2026-10-12T04:08Z

Scope: prior art for the "modular multimap traversal" spike. The idea is to keep
many indexes ("maps") over the same code units and let a step-wise traversal
combine them, in parallel or in sequence. A thinking agent can pause the traversal
at checkpoints, look at the frontier and steer it. The maps include lexical, several
vector maps, LSP symbols and references, call, import, build and trait/impl graphs,
git co-change, test-to-code, config readers, error raise sites, entrypoints,
doc-to-code and agent instruction files. Maps are chosen at index time from a
repository profile.

Every entry is judged against LCI's constraints:

- local only, and one process;
- no new service or database beyond the embedded LanceDB and the local
  OpenAI-compatible embedding endpoint LCI already uses;
- Windows, macOS and Linux;
- Apache-2.0 or MIT preferred.

LCI already depends on `tree-sitter` 0.25 plus per-language grammars,
`lancedb` =0.38.0, `rmcp` =3.2.0, `ignore`, `globset`, `reqwest`, `tokio` and
`axum`. Lexical search runs through ripgrep.

Verdicts:

- **reuse**: take the crate, format or algorithm as is.
- **adapt**: take the idea or the format but write LCI-native code.
- **skip**: not worth it under LCI's constraints.

How things were checked:

- Licenses were read from each project's LICENSE file on its default branch, or
  from crates.io metadata for crates, on the date above.
- Crate versions are the latest crates.io releases on that date.
- arXiv IDs were checked against each project's README or arXiv search results.
- Anything not checked this way is marked **unverified**.

---

## 1. Code graph retrieval

### 1.1 CodexGraph
- **What it gives:** builds a graph database from a repository, with modules,
  classes and functions as nodes and contains/inherits/calls/uses as edges. An LLM
  agent writes graph queries (Cypher on Neo4j) to fetch structure-aware context.
  It is evaluated on CrossCodeEval, SWE-bench and EvoCodeBench. The paper's main
  argument fits this spike: similarity retrieval misses structural context, and
  task-specific hand-built tools do not generalise, so it exposes one unified
  graph schema to the agent.
- **Verdict:** adapt. Keep the idea of a typed code-unit graph with a small,
  stable edge vocabulary that the agent can query. Skip Neo4j, because it is a
  separate server and GPL-3.0 or commercial.
- **Crate:** none. Use `petgraph` or LanceDB edge tables (see section 8).
- **License:** the paper code lives under ModelScope-Agent, which is Apache-2.0.
  Which subdirectory holds it is unverified.
- **URLs:** https://arxiv.org/abs/2408.03910 , https://github.com/modelscope/modelscope-agent

### 1.2 RepoGraph
- **What it gives:** a line-level repository graph. Nodes are definition and
  reference lines extracted with tree-sitter, and edges link each reference to its
  definition and each definition to the code it contains. Retrieval is
  ego-graph expansion around keyword-matched seed nodes, and the result is fed to
  agent or procedural SWE-bench pipelines as a plug-in. It improved all four base
  methods tested (ICLR 2025).
- **Verdict:** adapt. Ego-graph expansion of k hops around seeds is exactly a
  "sequence" traversal step (seed from the lexical or vector map, then expand over
  the definition/reference map). LCI already has tree-sitter chunks, so the
  def/ref edges can be built natively.
- **Crate:** none. It is Python.
- **License:** Apache-2.0.
- **URLs:** https://arxiv.org/abs/2410.14684 , https://github.com/ozyyshr/RepoGraph

### 1.3 CodeRAG (bigraph), later retitled GraphCodeAgent
- **What it gives:** a requirement graph plus a code graph (DS-code graph), with
  a mapping from requirement nodes to code nodes. Retrieval follows
  sub-requirement and similar-requirement edges, maps them onto code anchors, and
  then lets an agent retrieve more code during generation. v1 was titled "CodeRAG:
  Supportive Code Retrieval on Bigraph"; the v2 PDF is titled "GraphCodeAgent".
- **Verdict:** adapt, conceptually. Two linked graphs, one of intent (doc or
  requirement) and one of code, is the doc-to-code map pattern. The anchoring step
  shows how one map can seed another.
- **Crate:** none.
- **License:** code license unverified.
- **URL:** https://arxiv.org/abs/2504.10046

### 1.4 LocAgent
- **What it gives:** turns the repository into a directed heterogeneous graph of
  directories, files, classes and functions, with contain, import, invoke and
  inherit edges. Graph tools (search entity, traverse graph, retrieve entity) let
  an agent do multi-hop localisation. It reports up to 92.7% file-level accuracy
  with a fine-tuned 32B model and introduces Loc-Bench.
- **Verdict:** adapt. This is the closest published shape to "agent-steered
  traversal over a typed code graph". Its tool set (search, traverse with typed
  edge filters and a depth limit, retrieve) is a good template for MCP tools.
- **Crate:** none. It is Python.
- **License:** Apache-2.0.
- **URLs:** https://arxiv.org/abs/2503.09089 , https://github.com/gersteinlab/LocAgent

### 1.5 Aider repo map
- **What it gives:** tree-sitter `tags.scm` queries pull definitions and
  references from every file. A file graph is built with an edge for each
  identifier referenced from one file and defined in another. Personalized
  PageRank (networkx) ranks the files. The personalization vector boosts files
  already in the chat and files whose paths or identifiers are mentioned. A
  binary search then fits the top-ranked definitions into a token budget.
- **Verdict:** adapt. Personalized PageRank seeded by the current frontier is a
  cheap, well-tested ranking for an "expand" step and for a repository-overview
  map. The algorithm is about 50 lines over a CSR adjacency list, so it does not
  justify a dependency. tree-sitter tag queries also give a fast def/ref
  extractor that does not need an LSP.
- **Crate:** `tree-sitter-tags` (MIT; use 0.25.x to match LCI's
  `tree-sitter` 0.25, since 0.27.x needs `tree-sitter` 0.27). PageRank is
  hand-rolled or via `petgraph`.
- **License:** Apache-2.0 (aider).
- **URLs:** https://github.com/Aider-AI/aider/blob/main/aider/repomap.py , https://aider.chat/docs/repomap.html

### 1.6 Sourcegraph / Cody context
- **What it gives:** Sourcegraph 5.3 moved Cody's enterprise context retrieval
  from embeddings to the core Sourcegraph search engine (keyword/trigram search,
  Zoekt). Its stated reasons were no third-party embedding calls and less index
  upkeep. Precise navigation comes from SCIP indexes (section 4). Several
  retrieval mechanisms are combined.
- **Verdict:** adapt, as evidence. A strong lexical channel plus precise symbol
  data competes with embedding-only retrieval, which supports LCI keeping
  ripgrep as a first-class map. Skip the product itself, which is a server stack.
- **Crate:** none. Zoekt is Go.
- **License:** the `cody-public-snapshot` repo is Apache-2.0, and `zoekt` is
  Apache-2.0. The main Sourcegraph repo snapshot carries an Enterprise license
  (proprietary).
- **URLs:** https://sourcegraph.com/docs/cody/faq , https://sourcegraph.com/blog/sourcegraph-5-3-changelog , https://github.com/sourcegraph/zoekt

### 1.7 Continue (`@codebase`)
- **What it gives:** a local index that combines embeddings retrieval (local
  model; the docs mention all-MiniLM-L6-v2 / transformers.js) with keyword search.
  It retrieves `nRetrieve` (25) candidates and reranks them, by LLM or a reranker
  provider, down to `nFinal` (5). The index lives under the user's home directory.
- **Verdict:** skip as a dependency, since it is TypeScript inside an IDE
  extension. It confirms the hybrid-then-rerank pattern LCI already uses, and
  adds no map types.
- **Crate:** none.
- **License:** Apache-2.0.
- **URLs:** https://docs.continue.dev/features/codebase-embeddings , https://github.com/continuedev/continue

### 1.8 Kythe
- **What it gives:** a language-agnostic graph schema of nodes (anchors,
  semantic nodes) and typed edges (defines, ref, childof, overrides and so on).
  Per-language extractors and indexers feed a serving table, and the system was
  built for cross-reference at Google scale.
- **Verdict:** skip the system: it needs a heavy toolchain (Bazel-era build
  extraction) and server-style serving tables. Its edge vocabulary is still a
  useful reference when naming LCI's relation types.
- **Crate:** none.
- **License:** Apache-2.0. Current maintenance level is unverified.
- **URLs:** https://kythe.io , https://github.com/kythe/kythe

---

## 2. GraphRAG-style graph plus vector retrieval

### 2.1 Microsoft GraphRAG
- **What it gives:** an LLM extracts an entity and relationship graph from text
  chunks. Leiden community detection builds a hierarchy, and an LLM writes a
  summary for each community. "Global search" map-reduces over community
  summaries, and "local search" fans out from matched entities.
- **Verdict:** skip the pipeline. LLM extraction at index time is costly and
  non-deterministic, and code already has exact structure. Adapt one idea:
  community or cluster summaries over the import or co-change graph can work as a
  coarse "module map" for wide questions.
- **Crate:** none. Leiden or Louvain in Rust is available via `graphrs` (MIT,
  Louvain); a Leiden crate is unverified.
- **License:** MIT.
- **URLs:** https://arxiv.org/abs/2404.16130 , https://github.com/microsoft/graphrag

### 2.2 LightRAG
- **What it gives:** a lighter GraphRAG that also uses LLM entity and relation
  extraction, but does dual-level retrieval: low-level keys go to entities and
  high-level keys go to relations or themes, both through vector lookups over the
  graph's nodes and edges. Incremental updates merge new subgraphs without a full
  rebuild.
- **Verdict:** adapt one idea only: embed edges as well as nodes, so a query can
  match a relation such as "X validates Y". Skip the LLM extraction and its
  storage backends.
- **Crate:** none.
- **License:** MIT.
- **URLs:** https://arxiv.org/abs/2410.05779 , https://github.com/HKUDS/LightRAG

### 2.3 HippoRAG and HippoRAG 2
- **What it gives:** an open knowledge graph is built from passages. At query
  time, matched nodes seed **Personalized PageRank**, and the PPR mass ranks
  passages, giving multi-hop retrieval in one step. HippoRAG 2 (ICML 2025) adds
  passage nodes to the graph and LLM filtering of seed triples before PPR.
- **Verdict:** adapt. "Seed nodes from vector or lexical hits, then PPR over a
  typed code graph, then rank code units" is a principled parallel-to-sequence
  fusion step. It is cheap on graphs the size of a repository and needs no LLM at
  query time when the seeds come from existing maps. The LLM-filter step maps onto
  the agent checkpoint, where the agent prunes the seeds before expansion.
- **Crate:** hand-rolled PPR, or `petgraph`.
- **License:** MIT.
- **URLs:** https://arxiv.org/abs/2405.14831 , https://arxiv.org/abs/2502.14802 , https://github.com/OSU-NLP-Group/HippoRAG

### 2.4 Neo4j-style hybrid (vector index plus Cypher expansion)
- **What it gives:** vector search returns seed nodes, and graph queries then
  expand the neighbourhood ("retrieval query"). `neo4j-graphrag-python` packages
  this pattern with hybrid (vector plus full-text) retrievers.
- **Verdict:** skip the database (a separate server, and GPL-3.0 Community or a
  commercial license). Adapt the "seed, then expand by typed edges" pattern, which
  is the same shape as RepoGraph and HippoRAG.
- **Crate:** none.
- **License:**
  - `neo4j-graphrag-python`: Apache-2.0.
  - Neo4j server: **GPL-3.0** (Community) or commercial.
- **URLs:** https://github.com/neo4j/neo4j-graphrag-python , https://github.com/neo4j/neo4j

---

## 3. Multi-vector and late interaction

### 3.1 ColBERT / ColBERTv2 / PLAID
- **What it gives:** token-level embeddings scored by MaxSim (late interaction).
  ColBERTv2 adds residual compression, and PLAID adds centroid-based candidate
  generation and pruning for fast search. It beats single-vector retrieval on
  out-of-domain and exact-term queries, which code identifiers often are.
- **Verdict:** adapt, later. It is useful as one "vector map" variant at fine
  granularity (identifier-heavy queries). The costs are storage (tens to
  hundreds of vectors per chunk) and the need for a local multi-vector embedding
  model, which the OpenAI-compatible `/embeddings` endpoint does not return in a
  standard way. Keep it out of the first spike slice.
- **Crate:** LanceDB multivector columns (see 3.4). Inference via `pylate-rs`
  (see 3.5).
- **License:** MIT (stanford-futuredata/ColBERT).
- **URLs:** https://arxiv.org/abs/2004.12832 , https://arxiv.org/abs/2112.01488 , https://arxiv.org/abs/2205.09707 , https://github.com/stanford-futuredata/ColBERT

### 3.2 ColPali-style (vision late interaction)
- **What it gives:** page-image patches are embedded with a VLM, then scored by
  MaxSim. It targets visually rich documents such as PDFs and slides.
- **Verdict:** skip. LCI indexes source text, and the only relevant case
  (diagrams in docs) is outside scope.
- **Crate:** none.
- **License:** MIT (code). Model licenses vary.
- **URLs:** https://arxiv.org/abs/2407.01449 , https://github.com/illuin-tech/colpali

### 3.3 Jina ColBERT v2
- **What it gives:** a multilingual ColBERT model with Matryoshka-style
  reducible token-embedding dimensions.
- **Verdict:** skip for redistribution or default use. The model weights are
  **CC-BY-NC-4.0 (non-commercial)**; v1-en was Apache-2.0. If late interaction
  is tried, choose an Apache or MIT checkpoint.
- **Crate:** none.
- **License:** **CC-BY-NC-4.0**, taken from Jina's model page and mirrors. The
  Hugging Face card itself was not opened, so treat as partially verified.
- **URLs:** https://arxiv.org/abs/2408.16672 , https://jina.ai/models/jina-colbert-v2/

### 3.4 LanceDB multivector
- **What it gives:** a multivector column type (a list of fixed-size vectors)
  with MaxSim scoring and cosine distance only. Its docs aim it at ColBERT and
  ColPali.
- **Verdict:** reuse if late interaction is ever tried. It is already embedded,
  so it adds no new store. Whether the Rust SDK exposes multivector query at
  LCI's pinned 0.38.0 is **unverified**; older docs said Python only.
- **Crate:** `lancedb` (already a dependency; latest 0.40.0) and `lance-index`.
- **License:** Apache-2.0.
- **URL:** https://docs.lancedb.com/search/multivector-search

### 3.5 PyLate / pylate-rs / FastPlaid
- **What it gives:** PyLate trains late-interaction models. `pylate-rs` is a
  Candle-based Rust inference engine for them, running on CPU, Metal or CUDA, with
  Rust, Python and WASM bindings. FastPlaid is a PLAID-style multi-vector index.
- **Verdict:** adapt, later. `pylate-rs` would put a model in-process, but LCI
  currently leaves embeddings to a local OpenAI-compatible server. Adding
  in-process inference is a separate architectural decision. FastPlaid would be a
  second index next to LanceDB, so skip it.
- **Crate:** `pylate-rs` 1.0.4 (MIT), `candle-core` (MIT OR Apache-2.0).
- **License:** MIT (pylate, pylate-rs, fast-plaid).
- **URLs:** https://github.com/lightonai/pylate-rs , https://github.com/lightonai/pylate , https://github.com/lightonai/fast-plaid

### 3.6 Multi-granularity and Matryoshka embeddings
- **What it gives:**
  - Matryoshka representation learning gives one embedding that can be
    truncated to smaller dimensions, so one model can serve a cheap coarse
    vector map and a precise fine one.
  - Multi-granularity indexing (file, symbol and statement-window vectors) lets
    the traversal pick its resolution.
- **Verdict:** adapt. "Granularity" is already an axis of the spike's vector
  maps (embedder x granularity x purpose). A coarse file-level map used to prune
  before a fine chunk-level search is a natural sequential step. Matryoshka lets
  both come from one embedding call, if the local model supports it; per-model
  support is unverified.
- **Crate:** none needed. Store it as extra LanceDB vector columns or tables.
- **License:** not applicable (technique).
- **URL:** https://arxiv.org/abs/2205.13147 (unverified this pass)

---

## 4. Code knowledge graphs and formats

### 4.1 SCIP (protocol, `scip` crate, rust-analyzer output, scip-* indexers)
- **What it gives:** a Protobuf schema for precise code intelligence: documents,
  occurrences with symbol roles (definition, reference, import and so on), symbol
  information, relationships (implementation, type definition, reference) and
  globally unique symbol strings.
  - The repository moved to the `scip-code` organisation.
  - The Rust bindings are the `scip` crate.
  - Indexers exist for TypeScript and JavaScript, Python, Java/Scala/Kotlin,
    C/C++ (scip-clang), Go, Ruby, .NET, and Rust (rust-analyzer has a `scip`
    subcommand; `crates/rust-analyzer/src/cli/scip.rs` exists).
- **Verdict:** reuse as an **optional import format** for the LSP-symbols,
  references, trait/impl and call-approximation maps. When a user already has an
  `index.scip`, or LCI can run an indexer, LCI reads it in-process with no
  service. Do not require it: the indexers are separate per-language toolchains,
  some (scip-python, scip-typescript) need Node, and Windows support for each
  indexer is **unverified**.
- **Crate:** `scip` 0.10.0 (Apache-2.0). It pins `protobuf` =3.7.2 (MIT), which
  is a new transitive dependency for LCI.
- **License:**
  - SCIP: Apache-2.0.
  - scip-typescript, scip-java, scip-clang, scip-go, scip-ruby, scip-dotnet:
    Apache-2.0.
  - scip-python: MIT.
  - rust-analyzer: MIT OR Apache-2.0.
- **URLs:** https://github.com/scip-code/scip , https://crates.io/crates/scip , https://github.com/sourcegraph/scip-typescript , https://github.com/sourcegraph/scip-python , https://github.com/sourcegraph/scip-clang

### 4.2 LSIF
- **What it gives:** the Language Server Index Format, a JSON-lines graph of
  vertices and edges that mirrors LSP requests (definition, references, hover)
  ahead of time. The current spec is 0.6.0.
- **Verdict:** skip. Sourcegraph replaced it with SCIP for its own use (the
  SCIP announcement describes the reasons). The graph is verbose and hard to
  update incrementally, and few indexers still target it. If a user has LSIF
  output, convert it once with external tooling rather than supporting two
  formats.
- **Crate:** `lsp-types` (MIT) has some LSIF types (unverified which version).
- **License:** spec and `lsif-node` are MIT.
- **URLs:** https://microsoft.github.io/language-server-protocol/specifications/lsif/0.6.0/specification/ , https://github.com/microsoft/lsif-node

### 4.3 GitHub stack graphs (`stack-graphs`, `tree-sitter-stack-graphs`)
- **What it gives:** incremental, per-file name-resolution graphs built from
  tree-sitter via a DSL. Each file is processed in isolation and paths are
  stitched at query time, which gives precise go-to-definition without a build or
  a language server. It powered GitHub's precise navigation for some languages.
- **Verdict:** skip as a dependency, but it is worth reading for its design
  (per-file partial graphs, so re-indexing one file never touches others, which
  matches LCI's incremental reuse).
  - **The repository is no longer supported or updated by GitHub**; its README
    says to fork it.
  - The last releases were December 2024.
  - It depends on `tree-sitter` ^0.24, which conflicts with LCI's 0.25.
  - Per-language rule sets exist only for a few languages.
- **Crate:** `stack-graphs` 0.14.1 (pulls in `rusqlite` ^0.28 for its storage)
  and `tree-sitter-stack-graphs` 0.10.0. Both are unmaintained.
- **License:** MIT OR Apache-2.0.
- **URL:** https://github.com/github/stack-graphs

### 4.4 tree-sitter-graph
- **What it gives:** a DSL that builds arbitrary graphs (nodes, edges and
  attributes) from tree-sitter query matches. It is the substrate under stack
  graphs.
- **Verdict:** adapt the idea: declarative, per-language edge-extraction
  rules kept as query files. LCI would write its own small rule format on top of
  tree-sitter queries, because the crate is pinned to `tree-sitter` ^0.24 and its
  last release was December 2024, with no newer release found.
- **Crate:** `tree-sitter-graph` 0.12.0.
- **License:** MIT OR Apache-2.0.
- **URL:** https://github.com/tree-sitter/tree-sitter-graph

### 4.5 CodeQL databases
- **What it gives:** a relational database extracted from a build, holding the
  AST, control flow, data flow and types. The Datalog-like QL language answers
  rich questions such as callers, taint paths and config-key readers.
- **Verdict:** **skip**.
  - The CodeQL CLI and engine are **proprietary**, under the GitHub CodeQL Terms
    and Conditions. Analysing closed-source code, or using it in automation
    outside the listed open-source cases, needs a commercial license.
  - Only the query and library repository is MIT.
  - Extraction also needs a working build.
- **Crate:** none.
- **License:**
  - Queries (github/codeql): MIT.
  - CLI and engine: **proprietary, use-restricted**.
- **URLs:** https://github.com/github/codeql , https://github.com/github/codeql-cli-binaries , https://docs.github.com/en/code-security/codeql-cli

### 4.6 Glean
- **What it gives:** Meta's fact store for code. Indexers emit typed facts
  under a schema, and the logic-based query language Angle derives and queries
  them. It can ingest SCIP and LSIF and runs as a server with RocksDB storage
  (storage engine from general knowledge, unverified).
- **Verdict:** skip the system: it is a separate service with a heavy Haskell
  and C++ build. Adapt the model of a **schema of typed facts plus derived
  predicates** (for example, "test covers function" derived from imports plus
  naming). This matches the spike's idea of maps as derived views over code units.
- **Crate:** none.
- **License:** BSD-3-Clause (the BSD text names Facebook).
- **URLs:** https://glean.software , https://github.com/facebookincubator/Glean , https://engineering.fb.com/2024/12/19/developer-tools/glean-open-source-code-indexing/

### 4.7 ctags / Universal Ctags
- **What it gives:** fast, regex- and parser-based extraction of definition
  tags for 100+ languages, with JSON output (`ctags-json-output(5)`).
- **Verdict:** skip as a bundled component. Universal Ctags is **GPL-2.0**.
  Shelling out to an installed binary would avoid linking, but adds an external
  tool dependency. `tree-sitter-tags` covers the same need in-process for LCI's
  languages.
- **Crate:** `tree-sitter-tags` (MIT) is the in-process replacement.
- **License:** **GPL-2.0**.
- **URL:** https://github.com/universal-ctags/ctags

---

## 5. Agentic retrieval, pause and steer, tool-in-the-loop

### 5.1 SWE-agent (agent-computer interface)
- **What it gives:** a purpose-built "agent-computer interface": compact
  search and navigation commands (find_file, search_dir, search_file, a windowed
  file viewer) with bounded, summarised output. The paper shows that interface
  design changes agent success rates materially.
- **Verdict:** adapt. MCP tools for traversal should return bounded frontier
  summaries (top-k with scores and per-map provenance, with truncation stated
  explicitly), not raw dumps. The checkpoint view of the frontier is LCI's
  agent-computer interface.
- **Crate:** none.
- **License:** MIT.
- **URLs:** https://arxiv.org/abs/2405.15793 , https://github.com/SWE-agent/SWE-agent

### 5.2 Agentless (hierarchical localisation)
- **What it gives:** a fixed, non-agent pipeline that localises from the repo
  tree to files, then classes and functions, then edit lines, with the LLM
  picking at each level. Embedding retrieval is used to complement the file
  selection. It was competitive with agents at much lower cost.
- **Verdict:** adapt. Coarse-to-fine narrowing is a scripted traversal plan
  (file map, then symbol map, then chunk map) and should be a built-in default
  "recipe" that an agent can run unattended or pause.
- **Crate:** none.
- **License:** MIT.
- **URLs:** https://arxiv.org/abs/2407.01489 , https://github.com/OpenAutoCoder/Agentless

### 5.3 AutoCodeRover
- **What it gives:** program-structure-aware search APIs (search_class,
  search_method_in_class, search_code_in_file and others) used iteratively by an
  LLM, with optional spectrum-based fault localisation from tests.
- **Verdict:** adapt the API idea: structure-typed search tools, plus a
  test-to-code signal as a map. Do not reuse code. The repository now carries the
  **Sonar Source-Available License v1.0** (not OSI open source; it has a
  non-compete clause).
- **Crate:** none.
- **License:** **Sonar Source-Available License v1.0**, read from the LICENSE
  file in both `nus-apr/auto-code-rover` and `AutoCodeRoverSG/auto-code-rover`.
- **URLs:** https://arxiv.org/abs/2404.05427 , https://github.com/nus-apr/auto-code-rover

### 5.4 OpenHands
- **What it gives:** an open agent platform with a sandboxed runtime, an event
  stream of actions and observations, and pluggable agents. Code search is mostly
  shell tools (grep or find) and file viewing.
- **Verdict:** skip as a dependency. It is a consumer of LCI-like tools, not a
  source of them. Its event-stream model (each action and observation recorded)
  is a reasonable shape for a traversal trace log.
- **Crate:** none.
- **License:** MIT (core repository).
- **URLs:** https://arxiv.org/abs/2407.16741 (ID from general knowledge; README
  check found none) , https://github.com/OpenHands/OpenHands

### 5.5 Moatless Tools and SWE-Search (MCTS)
- **What it gives:**
  - Moatless: "build good tools to insert the right context" rather than rely
    on agent reasoning. It offers semantic and code search tools with
    span-level context.
  - SWE-Search (moatless-tree-search) adds Monte Carlo Tree Search with value
    estimates and backtracking over agent trajectories.
- **Verdict:** adapt. MCTS and backtracking is the "beam or frontier search"
  form of pause and steer: the frontier is a set of partial traversals with
  scores, and the agent (or a value heuristic) chooses which to expand or
  abandon. LCI can keep a small beam of traversal states server-side and let the
  agent pick, without running any LLM itself.
- **Crate:** none.
- **License:** MIT (moatless-tools), Apache-2.0 (moatless-tree-search).
- **URLs:** https://github.com/aorwall/moatless-tools , https://github.com/aorwall/moatless-tree-search , https://arxiv.org/abs/2410.20285

### 5.6 Self-RAG, IRCoT, FLARE (iterative retrieval)
- **What it gives:**
  - Self-RAG trains reflection tokens that decide when to retrieve and critique
    what was retrieved.
  - IRCoT interleaves chain-of-thought steps with retrieval, using each
    reasoning step as the next query.
  - FLARE retrieves when the upcoming generation has low-confidence tokens,
    using the draft sentence as the query.
- **Verdict:** adapt the control pattern. Each says retrieval should be
  multi-step and conditioned on the reasoning so far, which is what checkpoints
  enable. LCI does not generate text, so the "when to retrieve" decision stays
  with the calling agent. LCI exposes cheap continuation (resume traversal with
  a new sub-query or added seeds) instead of a fresh search.
- **Crate:** none.
- **License:** MIT (self-rag), Apache-2.0 (ircot), MIT (FLARE).
- **URLs:** https://arxiv.org/abs/2310.11511 , https://arxiv.org/abs/2212.10509 , https://arxiv.org/abs/2305.06983

### 5.7 ReAct
- **What it gives:** interleaved Thought, Action and Observation loops, which
  are the base pattern for tool-using agents.
- **Verdict:** adapt, as framing only. A traversal checkpoint is the
  Observation, and steering is the next Action. The design takeaway is to keep
  each observation small and decision-relevant.
- **Crate:** none.
- **License:** not applicable (paper).
- **URL:** https://arxiv.org/abs/2210.03629 (ID from general knowledge,
  unverified this pass)

### 5.8 Claude Code and Cursor agentic search observations
- **What it gives:**
  - Claude Code is publicly described as relying on agentic grep, glob and read
    loops rather than a pre-built vector index.
  - Cursor documents an embeddings-based codebase index synced from the client.
  - Both show that an agent with fast lexical tools and iteration is a strong
    baseline, and that semantic indexes help most on vague or concept queries.
- **Verdict:** adapt, as evaluation guidance. The spike must beat "agent plus
  ripgrep loop" on steps-to-answer and tokens, not only on recall@k. Steering
  tools must be cheaper per step than a raw grep round trip.
- **Crate:** none.
- **License:** proprietary products.
- **Status:** **unverified.** No primary-source design documents were
  retrieved in this pass.
- **URL:** https://docs.cursor.com/context/codebase-indexing (unverified)

### 5.9 MCP tool design
- **What it gives:** Model Context Protocol tools with JSON Schema inputs,
  structured outputs, resources and progress notifications.
- **Verdict:** reuse through `rmcp`, which is already a dependency. A
  pause-and-steer traversal maps to:
  1. a `traverse_start` tool that returns a traversal id and the first frontier;
  2. a `traverse_step` tool (expand, filter, re-seed, switch map, prune) that
     takes the id;
  3. a `traverse_result` tool.

  State is held in-process with a TTL, so no new store is needed. Resources can
  expose the repository profile and map catalogue.
- **Crate:** `rmcp` =3.2.0 (already a dependency).
- **License:** the MCP specification repo is under MIT and Apache-2.0 (both
  texts present).
- **URL:** https://github.com/modelcontextprotocol/modelcontextprotocol

### 5.10 Beam or frontier search with a human or agent in the loop
- **What it gives:** classic best-first or beam search keeps a scored frontier
  and expands the top-w nodes. Interactive variants show the frontier to a
  person, or here an agent, who re-weights, prunes or injects nodes. MCTS (5.5)
  is the stochastic form.
- **Verdict:** adapt. It is the core execution model for the spike. Frontier
  items carry per-map provenance (which map produced them, at which hop, with
  which score), so the agent can steer with reasons. Plain RRF stays the default
  combiner when no one steers.
- **Crate:** `std::collections::BinaryHeap`. No dependency is needed.
- **License:** not applicable.
- **URL:** none (textbook technique).

---

## 6. Git co-change mining

### 6.1 Logical coupling and ROSE
- **What it gives:** ROSE mines association rules from version history
  (support and confidence over co-changed files and functions) to suggest "what
  else should change". It reported top-3 suggestions containing a correct
  location 64% of the time (ICSE 2004), and more than 70% in the TSE version.
  Logical coupling finds dependencies that static analysis cannot see.
- **Verdict:** adapt. A co-change map (file or symbol pairs with support,
  confidence and recency decay) is cheap to compute from history and is a strong
  "expand" edge type for change-impact queries. Filter out large commits (bulk
  renames and formatting) and apply a time window.
- **Crate:** `gix` or `git2` (see 6.4).
- **License:** not applicable (paper).
- **URL:** https://www.st.cs.uni-saarland.de/papers/icse2004/

### 6.2 code-maat
- **What it gives:** a CLI that mines VCS logs for coupling (shared commits
  and degree), change frequency, code age, entity ownership and churn. It is the
  reference implementation behind "Your Code as a Crime Scene" style analyses.
- **Verdict:** adapt the metrics (coupling degree, minimum revisions, maximum
  changeset size). Do not bundle the tool: it is Clojure on the JVM and
  **GPL-3.0**.
- **Crate:** none.
- **License:** **GPL-3.0**, from the README; no LICENSE file was found at the
  repository root.
- **URL:** https://github.com/adamtornhill/code-maat

### 6.3 hercules
- **What it gives:** a Go engine that runs a DAG of analyses over full commit
  history: burndown, couples (file and developer coupling matrices), and
  structural diffs.
- **Verdict:** skip as a dependency (Go binary, external process). The
  "couples" analysis is the same idea as 6.1. The last tagged Go release was
  v10.3.0 (2019); archive status is unverified. Do not confuse it with the
  unrelated Rust crate `hercules` 0.5.0 (BSD-3-Clause), which is an optimisation
  library.
- **Crate:** none.
- **License:** Apache-2.0 (src-d/hercules).
- **URL:** https://github.com/src-d/hercules

### 6.4 `gix` and `git2` crates
- **What it gives:**
  - `gix` (gitoxide) is a pure-Rust git implementation: object and commit
    walking, tree diffs, blame (support level in the current version
    unverified), with no C dependency.
  - `git2` binds libgit2 (C, built from source via `libgit2-sys`).
  - Either can walk history to build co-change, recency and authorship maps
    in-process.
- **Verdict:** reuse `gix` if LCI needs history in-process: pure Rust is the
  easiest cross-platform build on Windows with no C toolchain or OpenSSL.
  `git2` is the fallback for features `gix` lacks. Alternatively, shell out to
  an installed `git log --name-only` (zero dependencies, but relies on an
  external tool).
- **Crate:** `gix` 0.89.0, `git2` 0.21.0.
- **License:** MIT OR Apache-2.0 for both. libgit2 itself is GPL-2.0 with a
  linking exception (from general knowledge, unverified this pass).
- **URLs:** https://github.com/GitoxideLabs/gitoxide , https://github.com/rust-lang/git2-rs

---

## 7. Adaptive index selection and query routing

### 7.1 Adaptive-RAG
- **What it gives:** a small trained classifier predicts query complexity and
  routes to no retrieval, single-step retrieval or multi-step retrieval (NAACL
  2024).
- **Verdict:** adapt. A query-time router choosing between "one RRF shot" and
  "multi-step traversal" is the same decision. Start with heuristics (identifier
  present, error string present, path present, natural-language-only) before any
  learned model.
- **Crate:** none.
- **License:** Apache-2.0.
- **URLs:** https://arxiv.org/abs/2403.14403 , https://github.com/starsuzi/adaptive-rag

### 7.2 RouteLLM
- **What it gives:** learned routers (matrix factorisation, BERT classifier,
  similarity-weighted ranking) that send each query to a strong or weak LLM to
  trade cost against quality, trained on preference data.
- **Verdict:** adapt the framing: route queries to maps, or to traversal
  depth, under a latency budget. It is not reusable directly because it routes
  between LLMs. A learned router needs labelled traffic that LCI does not have,
  so use rules first and log decisions for later fitting.
- **Crate:** none.
- **License:** Apache-2.0.
- **URLs:** https://arxiv.org/abs/2406.18665 , https://github.com/lm-sys/RouteLLM

### 7.3 Sparse versus dense selection
- **What it gives:** work on hybrid retrieval shows that query features
  (rare identifiers or exact strings versus paraphrase) predict whether lexical,
  sparse or dense retrieval wins, and that fusion is robust when the router is
  unsure.
- **Verdict:** adapt. Keep RRF over all enabled maps as the safe default. Use
  routing only to skip maps that are known to be useless for a query class, such
  as skipping vectors for an exact error string. No specific paper was verified
  for this entry, so it is **unverified** as a citation.
- **Crate:** none.
- **License:** not applicable.
- **URL:** none (unverified).

### 7.4 Index advisor and per-repository profiling
- **What it gives:** database "index advisors" (classic DBMS tooling)
  recommend indexes from a workload and a cost model. The analogue for LCI:
  - profile the repository (languages and their share, file counts, presence of
    tests, CI and build files, git history depth, monorepo markers, agent
    instruction files such as AGENTS.md or CLAUDE.md);
  - enable maps whose expected benefit beats their build cost;
  - optionally let an agent confirm or override the plan.
- **Verdict:** adapt. Implement it as a deterministic profile-to-map-plan
  function with the plan written to config, plus an "advice" hook where an agent
  may add or drop maps before indexing. No external system to reuse.
- **Crate:** `ignore` and `globset` (already dependencies) for profiling.
- **License:** not applicable.
- **URL:** none (technique).

---

## 8. Rust building blocks

| Crate | Version (crates.io, 2026-10-12) | License | Use in spike | Verdict |
| --- | --- | --- | --- | --- |
| `petgraph` | 0.8.3 | MIT OR Apache-2.0 | in-memory typed graphs, BFS/DFS, SCC, toposort for import or build graphs | reuse (or plain CSR vectors if no algorithms are needed) |
| `tree-sitter-tags` | 0.27.1 latest; 0.25.10 matches LCI | MIT | def/ref extraction without an LSP | reuse, pinned to 0.25.x |
| `scip` | 0.10.0 | Apache-2.0 (pins `protobuf` =3.7.2, MIT) | optional SCIP import | reuse (optional feature) |
| `lsp-types` | 0.97.0 | MIT | LSP structs, if LCI does not hand-roll them | reuse if needed |
| `gix` | 0.89.0 | MIT OR Apache-2.0 | history walk for co-change, recency and blame | reuse |
| `git2` | 0.21.0 | MIT OR Apache-2.0 (libgit2 C) | fallback git access | adapt or fallback |
| `tantivy` | 0.26.2 | MIT | in-process BM25 index | skip for now: ripgrep is the lexical channel and LanceDB has full-text search; a second index adds upkeep |
| `fst` | 0.4.7 | Unlicense OR MIT | compact symbol-name and config-key sets, prefix or fuzzy lookup | reuse (small, stable) |
| `hnsw_rs` | 0.3.5 | MIT OR Apache-2.0 | standalone ANN | skip: LanceDB already provides ANN |
| `instant-distance` | 0.6.1 | MIT OR Apache-2.0 | standalone HNSW | skip (last release 2023; LanceDB covers it) |
| `usearch` | 2.26.4 | Apache-2.0 | standalone ANN (C++ core) | skip (C++ build; redundant) |
| `arroy` | 0.8.0 | MIT | ANN on LMDB | skip (second store) |
| `graphrs` | 0.12.0 | MIT | community detection (Louvain), centrality | adapt if module clustering is tried |
| `pylate-rs` | 1.0.4 | MIT | in-process late-interaction inference | defer |
| `stack-graphs` / `tree-sitter-stack-graphs` | 0.14.1 / 0.10.0 | MIT OR Apache-2.0 | name resolution | skip (unmaintained; `tree-sitter` ^0.24) |
| `tree-sitter-graph` | 0.12.0 | MIT OR Apache-2.0 | graph DSL | skip the crate, adapt the idea (`tree-sitter` ^0.24) |
| `rust-code-analysis` | 0.0.25 | **MPL-2.0** | code metrics | skip (weak-copyleft, last release 2023) |
| `lancedb` | 0.40.0 (LCI pins 0.38.0) | Apache-2.0 | vector maps, edge tables, multivector | reuse (already present) |

Note on graph storage. Edge maps (calls, imports, co-change, test-to-code and so
on) can be stored as LanceDB tables (`src_unit, dst_unit, kind, weight,
provenance`) and loaded into memory for traversal. That keeps "one process, no new
database". A side file per map in LCI's data directory is the alternative.

---

## Summary table

| # | Entry | Verdict | Crate | License |
| --- | --- | --- | --- | --- |
| 1.1 | CodexGraph | adapt | none | Apache-2.0 (ModelScope-Agent; subdir unverified) |
| 1.2 | RepoGraph | adapt | none | Apache-2.0 |
| 1.3 | CodeRAG / GraphCodeAgent | adapt | none | unverified |
| 1.4 | LocAgent | adapt | none | Apache-2.0 |
| 1.5 | Aider repo map (PPR) | adapt | `tree-sitter-tags` 0.25.x | Apache-2.0 (aider), MIT (crate) |
| 1.6 | Sourcegraph / Cody context | adapt (evidence) | none | Apache-2.0 (cody snapshot, zoekt); main repo **proprietary** |
| 1.7 | Continue @codebase | skip | none | Apache-2.0 |
| 1.8 | Kythe | skip (borrow vocabulary) | none | Apache-2.0 |
| 2.1 | Microsoft GraphRAG | skip (adapt module summaries) | `graphrs` | MIT |
| 2.2 | LightRAG | adapt (edge embeddings) | none | MIT |
| 2.3 | HippoRAG / HippoRAG 2 | adapt (PPR from seeds) | hand-rolled / `petgraph` | MIT |
| 2.4 | Neo4j hybrid | skip DB, adapt pattern | none | **GPL-3.0** / commercial (server); Apache-2.0 (python lib) |
| 3.1 | ColBERT / v2 / PLAID | adapt later | `lancedb` multivector | MIT |
| 3.2 | ColPali | skip | none | MIT (code) |
| 3.3 | Jina ColBERT v2 | skip | none | **CC-BY-NC-4.0** (partially verified) |
| 3.4 | LanceDB multivector | reuse if needed | `lancedb` | Apache-2.0 (Rust API at 0.38 unverified) |
| 3.5 | PyLate / pylate-rs / FastPlaid | adapt later | `pylate-rs` | MIT |
| 3.6 | Multi-granularity / Matryoshka | adapt | none | not applicable |
| 4.1 | SCIP + indexers | reuse (optional import) | `scip` | Apache-2.0 (scip-python MIT) |
| 4.2 | LSIF | skip | `lsp-types` | MIT |
| 4.3 | Stack graphs | skip (study design) | `stack-graphs`, `tree-sitter-stack-graphs` | MIT OR Apache-2.0; **unsupported by GitHub** |
| 4.4 | tree-sitter-graph | adapt idea | `tree-sitter-graph` | MIT OR Apache-2.0 |
| 4.5 | CodeQL databases | skip | none | **proprietary CLI** (queries MIT) |
| 4.6 | Glean | skip (adapt fact model) | none | BSD-3-Clause |
| 4.7 | Universal Ctags | skip | `tree-sitter-tags` instead | **GPL-2.0** |
| 5.1 | SWE-agent ACI | adapt | none | MIT |
| 5.2 | Agentless | adapt (default recipe) | none | MIT |
| 5.3 | AutoCodeRover | adapt idea only | none | **Sonar Source-Available v1.0** |
| 5.4 | OpenHands | skip | none | MIT |
| 5.5 | Moatless / SWE-Search MCTS | adapt (beam frontier) | none | MIT / Apache-2.0 |
| 5.6 | Self-RAG / IRCoT / FLARE | adapt (control pattern) | none | MIT / Apache-2.0 / MIT |
| 5.7 | ReAct | adapt (framing) | none | not applicable |
| 5.8 | Claude Code / Cursor observations | adapt (eval baseline) | none | proprietary; **unverified** |
| 5.9 | MCP tool design | reuse | `rmcp` | MIT and Apache-2.0 (spec) |
| 5.10 | Beam / frontier search, agent-in-loop | adapt (core model) | std | not applicable |
| 6.1 | Logical coupling / ROSE | adapt | `gix` | not applicable |
| 6.2 | code-maat | adapt metrics | none | **GPL-3.0** |
| 6.3 | hercules (Go) | skip | none | Apache-2.0 |
| 6.4 | gix / git2 | reuse `gix` | `gix`, `git2` | MIT OR Apache-2.0 |
| 7.1 | Adaptive-RAG | adapt (heuristic router) | none | Apache-2.0 |
| 7.2 | RouteLLM | adapt framing | none | Apache-2.0 |
| 7.3 | Sparse vs dense selection | adapt | none | not applicable (**unverified** citation) |
| 7.4 | Index advisor / repo profiling | adapt | `ignore`, `globset` | not applicable |

## Recommended reuse set for the spike

1. **Execution model.** An in-process traversal engine:
   - a beam or best-first frontier with per-item map provenance (5.10, 5.5);
   - exposed as stateful MCP tools through the existing `rmcp` (5.9);
   - a bounded, summarised frontier at each checkpoint (5.1);
   - an Agentless-style coarse-to-fine default recipe for unattended use (5.2).
2. **Fusion and expansion.**
   - Parallel step: keep RRF over the enabled maps.
   - Sequence step: Personalized PageRank seeded from the current frontier
     (2.3, 1.5) and k-hop ego expansion over typed edges (1.2, 1.4).
   - Both are hand-rolled or use `petgraph` (MIT OR Apache-2.0).
3. **Structural maps without a language server.**
   - Use `tree-sitter-tags` 0.25.x (MIT) with per-language tag queries for
     def/ref, import and call approximations (1.5).
   - Edge rules live in declarative query files, borrowing the tree-sitter-graph
     idea (4.4).
4. **Precise maps when available.** Optional `scip` (Apache-2.0) import of
   `index.scip` from rust-analyzer or the scip-* indexers (4.1), next to the
   existing LSP navigation. Never required.
5. **History maps.** `gix` (MIT OR Apache-2.0) to build co-change, with ROSE
   support and confidence, code-maat thresholds and recency decay (6.1, 6.2,
   6.4).
6. **Vector maps.**
   - Multiple LanceDB tables or columns per embedder x granularity x purpose,
     on the existing `lancedb`.
   - Coarse-to-fine through file-level then chunk-level maps (3.6).
   - Late interaction (3.1, 3.4, 3.5) is deferred until a permissively licensed
     multi-vector model and Rust multivector query support are confirmed.
7. **Map selection.**
   - A deterministic repository profile produces the map plan, with an optional
     agent-advice hook (7.4).
   - Query-time routing is heuristic first (7.1, 7.2), with RRF as the fallback.
8. **Do not use:**
   - CodeQL (proprietary CLI), Neo4j (separate server, GPL-3.0), Universal Ctags
     and code-maat (GPL), AutoCodeRover code (source-available), Jina ColBERT v2
     weights (non-commercial);
   - `stack-graphs` and `tree-sitter-graph` as dependencies (unmaintained, and
     they pin `tree-sitter` ^0.24);
   - any second vector or full-text store (`tantivy`, `hnsw_rs`, `arroy`,
     `usearch`).

## Unverified items

- CodeRAG / GraphCodeAgent code license.
- Which ModelScope-Agent subdirectory holds the CodexGraph code.
- Kythe maintenance level.
- Archive status of src-d/hercules.
- Jina ColBERT v2 license: confirmed from Jina's model page and mirrors, not
  from the Hugging Face card itself.
- Whether the LanceDB Rust SDK at 0.38.0 supports multivector queries.
- Windows support of each scip-* indexer.
- LSIF types in `lsp-types`.
- Matryoshka support in specific local embedding models.
- `gix` blame support level.
- libgit2's own license wording.
- Glean's storage engine.
- arXiv IDs for OpenHands (2407.16741), ReAct (2210.03629) and Matryoshka
  (2205.13147): from general knowledge, not checked against a README this pass.
- Claude Code and Cursor retrieval descriptions (no primary source retrieved).
- The citation for sparse versus dense selection (7.3).
