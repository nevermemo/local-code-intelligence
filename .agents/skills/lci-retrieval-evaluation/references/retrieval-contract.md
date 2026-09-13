# Retrieval contract

The pipeline is:

```text
validated query and filters
  -> query embedding, ripgrep, optional applicable LSP in parallel
  -> candidate mapping and channel-local ranks
  -> deduplication and reciprocal-rank fusion
  -> transparent source-role prior
  -> bounded reranker shortlist
  -> reranker or fail-open fusion order
  -> top K structured evidence
```

- Semantic, lexical, LSP, and reranker failures remain independently observable.
- A requested filter applies to every channel, fusion, reranker input, and final result. A filtered request with no matching chunks is a valid empty report.
- Keep semantic score, channel ranks, lexical match count, fusion score, source-role prior, and reranker score distinct.
- Deduplicate by stable source identity; do not merge unrelated chunks because their text is equal.
- Bound candidate counts and document sizes. Record stage and total timings.
- Preserve Milestone 2 index lifecycle metadata independently of retrieval timings.
- A source-role prior must remain generic, small, documented, and subordinate to strong relevance evidence. Explicit role filters take precedence.
- Changes to query instructions, document representation, embedding identity, candidate counts, fusion, prior, or reranker input require before/after evaluation.
