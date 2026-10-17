# Retrieval contract

The pipeline is:

```text
validated query and filters
  -> query embedding, ripgrep, optional applicable LSP in parallel
  -> candidate mapping and channel-local ranks
  -> deduplication and reciprocal-rank fusion
  -> transparent source-role prior
  -> bounded reranker shortlist (path plus a bounded head of each chunk)
  -> reranker or fail-open fusion order
  -> top K structured evidence
```

- Semantic, lexical, LSP, and reranker failures remain independently observable. A reranker failure appends its warning to earlier warnings rather than replacing them.
- A requested filter applies to every channel, fusion, reranker input, and final result. A filtered request with no matching chunks is a valid empty report.
- Keep semantic score, channel ranks, lexical match count, lexical score, fusion score, source-role prior, and reranker score distinct.
- `lexical_score` is the lexical channel's own chunk score: BM25 over the credited query terms, weighted by term rarity across indexed files, with term-frequency saturation and chunk-length normalisation. It only orders the lexical channel (its rank feeds fusion as a reciprocal rank); it is never added into `fusion_score` or `reranker_score`.
- Per-channel fusion weights (`[fusion]`, default `semantic_weight = lexical_weight = lsp_weight = 1.0`, which is plain RRF) are visible only through `fusion_score`: a channel adds `weight / (rrf_k + rank)`, the prior is added afterwards unscaled, and channel ranks, scores, and the reranker input never see a weight. A weight must be finite and greater than zero; disabling a channel is `[channels]`' job. Changing a default weight needs evaluation like any fusion change.
- The optional per-file cap (`[fusion].max_chunks_per_file`, default `2`; `0` = unlimited = identity) only reorders: after fusion ordering and before the shortlist cut it stably demotes (never drops) a file's chunks beyond the cap behind the other chunks, leaves `fusion_score` unrescaled (a demoted chunk can follow a lower-scored one), feeds the reranker, reranker-off, and failure paths alike, and is not applied again to the reranker's output. Changing its default needs evaluation.
- Documentation lane (`[docs_lane]`): with no `source_roles` filter, Markdown (prose extractor) chunks are split off before the code channels run, so the code `results` list is byte-identical to the list an index without Markdown gives (test: a Markdown-plus-code fixture against a code-only fixture, fake services). The docs lane runs on its own (same query vector, prose-only semantic filter, prose-only lexical, no LSP, same fusion, own rerank) and returns `docs` (default 2); it is fail-open and reports only `docs.warning`. Evaluation scores a query on `results` (default), `docs` (`lane:docs`) or both (`lane:union`); `returned_tokens` counts both lists.
- Deduplicate by stable source identity; do not merge unrelated chunks because their text is equal.
- Bound candidate counts and the bytes of each document sent to the reranker (`rerank_candidate_count`, `rerank_document_max_bytes`). Results always carry the full chunk. Record stage and total timings.
- Preserve index lifecycle metadata (`index.action`, `wait_ms`) independently of retrieval timings.
- A source-role prior must remain generic, small, documented, and subordinate to strong relevance evidence. Explicit role filters take precedence.
- Documents are embedded as source without a query instruction (`embedding_document_template`, default `{code}`). The embedded query text is the configurable `embedding_query_template`; its default (`DEFAULT_EMBEDDING_QUERY_TEMPLATE` in `lci-core/src/config.rs`, the Qwen3-Embedding instruction plus `{query}`, shown in the README configuration table) changes only with a separately evaluated retrieval change. A different query template is a query-side setting and never invalidates an index; a non-default document template changes `embedding_identity` and re-embeds.
- Changes to the default query template, document representation, embedding identity, candidate counts, fusion, prior, reranker input, or source-role classification require before/after evaluation per [the evaluation workflow](evaluation-workflow.md).
