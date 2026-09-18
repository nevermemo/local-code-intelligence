---
name: LCI test and acceptance evidence
description: Testing rules for unit, integration, evaluation, and live acceptance work.
applyTo: "{src/**/*test*.rs,lci-core/src/**/*test*.rs,tests/**/*.rs,xtask/src/acceptance/**/*.rs,evaluations/**/*}"
---

- Test observable contracts and failure recovery. Avoid tests that merely reproduce the implementation or match documentation wording.
- Use the existing mock model service and temporary external LanceDB directories for automated tests. Automated tests must not require ports 8765, 8766, or 8767.
- Keep live model acceptance separate from automated tests. Live retrieval may use 8766 and 8767; it must not contact 8765.
- Verify incremental work with actual parse and embedding request counts rather than timing alone.
- Preserve previous-snapshot searchability after failed updates and ensure later retries can recover.
- For retrieval changes, cover semantic, lexical, LSP, fusion, reranking, filtered-empty, and fail-open paths that the change affects.
- Save live reports under `test-results`; never stage them.
- Run focused checks during correction, then run `cargo fmt --all -- --check`, `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings` once for the completed slice.
- While iterating on `lci-core`-only code (chunk, language, filter, lexical, manifest, workspace, config, models, lsp), scope focused checks to that crate: `cargo check -p lci-core` and `cargo test -p lci-core --lib <module>::` compile and run in seconds instead of minutes, because they never touch LanceDB/arrow. Use `cargo xtask test --suite <name>` (see `docs/development/testing.md`) once the change also needs `App`/`Store`/MCP coverage, and always finish with the full `cargo xtask test --suite Full` gate. Prefer that scoped-package form consistently rather than mixing it with an unscoped `cargo test --test integration <name>` mid-session -- the two invocations don't reliably share build cache and switching between them can trigger a surprise full rebuild.
