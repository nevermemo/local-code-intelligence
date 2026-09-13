# Syntax chunking contract

Prefer coherent syntax nodes over arbitrary text windows.

- Preserve imports, declarations, functions, classes, methods, assignments, executable top-level statements, and language-specific declaration forms that carry useful meaning.
- Attach leading documentation, comments, attributes, annotations, and decorators to the declaration they describe when the grammar exposes a reliable boundary.
- Preserve exact source bytes represented by the selected range. Do not synthesize or normalize code stored in the chunk.
- Keep ordinary declarations whole. Split oversized declarations only at named syntax boundaries, keeping the declaration header with useful child groups.
- Do not truncate a large indivisible syntax leaf merely to meet a target size.
- Keep parser-recovered source searchable. A local syntax error must not discard unrelated valid chunks.
- Ensure chunk order and hashes are deterministic for unchanged source and adapter version.
- Add focused fixtures for nested declarations, leading metadata, top-level statements, oversized syntax, recovered syntax, and exact line ranges.
