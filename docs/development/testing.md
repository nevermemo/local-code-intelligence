# Testing guide

Use the smallest suite that covers the changed contract while iterating. Run
the full deterministic gates once after the complete change is ready.

| Change | Focused command |
| --- | --- |
| Chunking or scanning | `scripts/Test.ps1 -Suite Chunk` |
| Retrieval filters or source roles | `scripts/Test.ps1 -Suite Filter` |
| Evaluation definitions or metrics | `scripts/Test.ps1 -Suite Evaluation` |
| Index lifecycle or persistence | `scripts/Test.ps1 -Suite Indexing` |
| Readiness and health | `scripts/Test.ps1 -Suite Readiness` |
| MCP transport and tools | `scripts/Test.ps1 -Suite MCP` |
| Watched refresh | `scripts/Test.ps1 -Suite Watching` |
| LSP/navigation behavior | `scripts/Test.ps1 -Suite LSP` |

`scripts/Test.ps1 -Suite Full` runs formatting, all workspace tests, and Clippy.
Live model acceptance remains separate because it depends on locally running
services. Generated reports belong under `test-results` and are never committed.

The integration cases remain one Cargo test binary. Their source is divided by
behavior so agents can read and run a narrow group without paying the compile
and link cost of many separate integration binaries.
