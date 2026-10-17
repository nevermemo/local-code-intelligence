---
name: LCI test and acceptance evidence
description: Testing rules for unit, integration, evaluation, and live acceptance work.
applyTo: "{src/**/*test*.rs,lci-core/src/**/*test*.rs,tests/**/*.rs,xtask/src/acceptance/**/*.rs,evaluations/**/*}"
---

- Test observable contracts and failure recovery. Avoid tests that merely reproduce the implementation or match documentation wording.
- Use the existing mock model service and temporary external LanceDB directories for automated tests; they must not require ports 8765, 8766, or 8767. Live model acceptance is separate: it may use 8766 and 8767, never 8765, and its reports go under `test-results` and are never staged.
- Verify incremental work with actual parse and embedding request counts rather than timing alone. Preserve previous-snapshot searchability after failed updates and ensure later retries can recover.
- For retrieval changes, cover semantic, lexical, LSP, fusion, reranking, filtered-empty, and fail-open paths that the change affects.
- Iterate with focused checks, then run `cargo xtask test --suite Full` once for the completed slice. Suite map, the `lci-core`-scoped fast loop and the build-cache gotcha: `docs/development/testing.md`.
