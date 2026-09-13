---
name: lci-retrieval-evaluation
description: Change or assess local-code-intelligence retrieval filters, candidate channels, fusion, source-role priors, reranking, query/document formatting, or evaluation metrics. Use for ranking-quality and latency work; do not use for syntax-only language additions.
---

# LCI retrieval evaluation

Make ranking changes against evidence rather than one favorable query.

1. Read [the retrieval contract](references/retrieval-contract.md) before changing a candidate channel, filter, fusion, or reranker input.
2. Read [the evaluation workflow](references/evaluation-workflow.md) before making a quality or latency claim.
3. Keep scores and contributions separately visible. Do not hide heuristics inside semantic or reranker scores.
4. Apply requested filters before reranker input and defensively before final output.
5. Preserve valid empty filtered results and independent fail-open channels.
6. Keep evaluation expectations outside production ranking code.

Do not tune for GUST or LCI paths specifically. Do not contact port 8765.
