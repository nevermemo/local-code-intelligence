---
name: LCI documentation
description: Accuracy rules for README, evaluation definitions, configuration examples, and MCP descriptions.
applyTo: "{README.md,config.example.toml,evaluations/**/*,src/server.rs}"
---

- Describe current behavior and independently verified evidence. Label unavailable live prerequisites and fail-open fallbacks precisely.
- Keep `/health` process-only, `/ready` dependency-aware, and port 8765 outside the application in every public description.
- Distinguish syntax indexing/retrieval from LSP symbol and navigation support for each language.
- Keep MCP tool descriptions concise. Put detailed contracts and examples in the README or the applicable skill reference.
- Use portable repository-relative paths in checked-in evaluation definitions. Supply local workspace roots at runtime.
- Do not claim retrieval quality from one example alone; report the evaluation set, hit rates, MRR, fallback count, and relevant limitations.
