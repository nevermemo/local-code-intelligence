# Language adapter contract

## Registration

- Register the source extensions, written lowercase, in the central static language registry; lookup folds ASCII case (N94).
- Give every extension one stable serialized language identifier, extractor (a Tree-sitter grammar constructor and syntax family for code, or a prose kind such as Markdown), lexical glob, and adapter compatibility version.
- Keep extension selection deterministic. Do not infer a language from repository type, framework, or file contents. The one content check allowed is exclusion, never selection: `LanguageAdapter::non_source` may leave out a file whose content is a known non-source format that shares the extension (a Qt Linguist `.ts` catalog). It must require a prefix the language cannot start with, and its skips are reported, not silent.
- Update scanning, lexical globs, filtering validation, public documentation, and mixed-language fixtures together.

## Persistence

- A parse-affecting adapter change invalidates only that language's parse cache.
- Keep parser compatibility separate from embedding identity. Reuse an existing vector when exact chunk text and embedding identity (and the service's vector width) are unchanged.
- Removed files and removed chunks must disappear from the next committed snapshot.
- Build a complete replacement snapshot before committing it. A scan, parse, or embedding failure must leave the previous snapshot searchable.
- Existing manifests and LanceDB tables must migrate safely, be deliberately marked incompatible, or fail with an actionable message. Never allow an obscure missing-column failure.

## Retrieval

- Return the registered language identifier, normalized relative path, exact code, content hash, and one-based inclusive line range.
- Source-role classification is a shared path concern. Do not duplicate it inside a language adapter.
- Apply language/path/role filters consistently to semantic, lexical, LSP, fusion, reranker input, and final output.
- Keep document embeddings free of query instructions. The query text comes from `embedding_query_template`, not from an adapter; leave its default unchanged unless a separately evaluated retrieval change requires it.
