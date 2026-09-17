# Testing guide

Use the smallest suite that covers the changed contract while iterating. Run
the full deterministic gates once after the complete change is ready.

| Change | Focused command |
| --- | --- |
| Chunking or scanning | `cargo xtask test --suite Chunk` |
| Retrieval filters or source roles | `cargo xtask test --suite Filter` |
| Evaluation definitions or metrics | `cargo xtask test --suite Evaluation` |
| Index lifecycle or persistence | `cargo xtask test --suite Indexing` |
| Readiness and health | `cargo xtask test --suite Readiness` |
| MCP transport and tools | `cargo xtask test --suite Mcp` |
| Watched refresh | `cargo xtask test --suite Watching` |
| LSP/navigation behavior | `cargo xtask test --suite Lsp` |

`cargo xtask test --suite Full` runs formatting, all workspace tests, and Clippy.
Live model acceptance (`cargo xtask acceptance <name>`) remains separate because
it depends on locally running services. Generated reports belong under
`test-results` and are never committed.

The integration cases remain one Cargo test binary. Their source is divided by
behavior so agents can read and run a narrow group without paying the compile
and link cost of many separate integration binaries.
