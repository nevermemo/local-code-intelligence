---
name: lci-retrieval-evaluation
description: Change or assess local-code-intelligence retrieval filters, candidate channels, fusion, source-role priors, reranking, query/document formatting, or evaluation metrics. Use for ranking-quality and latency work; do not use for syntax-only language additions.
---

# LCI retrieval evaluation

Make ranking changes against evidence rather than one favorable query.

Any change to the query instruction, document representation, embedding identity, candidate counts, fusion, priors, reranker input, or source-role classification needs before/after evidence from the same frozen procedure. No exceptions for "obviously better".

1. Read [the retrieval contract](references/retrieval-contract.md) before changing a candidate channel, filter, fusion, or reranker input.
2. Read [the evaluation workflow](references/evaluation-workflow.md) before running a comparison or making a quality or latency claim.
3. Keep scores and contributions separately visible. Do not hide heuristics inside semantic or reranker scores.
4. Apply requested filters before reranker input and defensively before final output.
5. Preserve valid empty filtered results and independent fail-open channels.
6. Keep evaluation expectations outside production ranking code.
7. The embedder-plus-reranker retrieval sets (`evaluations/core.toml`, the mined set) are regression checks only: run them to show a change did not hurt, and do not add new embedder or reranker comparison arms. Where the project's measuring effort goes (traversal on a code graph, the three metric tiers, the simulated agent) is in [docs/spike/multimap/evaluation.md](../../../docs/spike/multimap/evaluation.md).

Where things live: retrieval pipeline in `src/app/search.rs`; filters and source roles in `lci-core/src/filter.rs` and `lci-core/src/filter/`; model clients in `lci-core/src/models.rs`; lexical channel in `lci-core/src/lexical.rs`; evaluation runner, definitions, and metrics in `src/evaluate/`; the checked-in definition in `evaluations/core.toml`.

Do not tune for GUST or LCI paths specifically. Do not contact port 8765.
