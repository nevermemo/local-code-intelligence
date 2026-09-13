# Language adapter contract

## Registration

- Register exact lowercase source extensions in the central static language registry.
- Give every extension one stable serialized language identifier, Tree-sitter grammar constructor, syntax family, lexical glob, and adapter compatibility version.
- Keep extension selection deterministic. Do not infer a language from repository type, framework, or file contents.
- Update scanning, lexical globs, filtering validation, public documentation, and mixed-language fixtures together.

## Persistence

- A parse-affecting adapter change invalidates only that language's parse cache.
- Keep parser compatibility separate from embedding identity. Reuse an existing vector when exact chunk text and embedding identity are unchanged.
- Removed files and removed chunks must disappear from the next committed snapshot.
- Build a complete replacement snapshot before committing it. A scan, parse, or embedding failure must leave the previous snapshot searchable.
- Existing manifests and LanceDB tables must migrate safely, be deliberately marked incompatible, or fail with an actionable message. Never allow an obscure missing-column failure.

## Retrieval

- Return the registered language identifier, normalized relative path, exact code, content hash, and one-based inclusive line range.
- Source-role classification is a shared path concern. Do not duplicate it inside a language adapter.
- Apply language/path/role filters consistently to semantic, lexical, LSP, fusion, reranker input, and final output.
- Keep document embeddings free of query instructions. Preserve the established query instruction exactly unless a separately evaluated retrieval change requires it.
