# Testing guide

Use the smallest suite that covers the changed contract while iterating. Run
the full deterministic gates once after the complete change is ready.

## The fastest loop: `lci-core`-scoped checks

The workspace is split into `lci-core` (chunking, language adapters, filters,
lexical search, manifest, workspace identity, config, models, LSP adapters --
everything with no dependency on LanceDB/arrow) and the root
`local-code-intelligence` crate (`App`, `Store`, the MCP server, `main`).
While a change stays inside `lci-core`, scope checks to that package instead
of the whole workspace:

```bash
cargo check -p lci-core                    # type-check only, no codegen -- seconds
cargo test -p lci-core --lib chunk::       # or language::, filter::, lsp::, ...
```

Both finish in single-digit seconds once dependencies are warm, because
neither touches the `lancedb`/`arrow`/`datafusion` dependency tree at all --
that tree is the reason a full-workspace build/test cycle can take several
minutes. Reach for the full `cargo xtask test --suite <name>` commands below
once a change also needs `App`, `Store`, or MCP coverage.

Pick one scoping strategy per session rather than mixing them: alternating
between a `-p`-scoped command and an unscoped `cargo test --test integration
<name>` (or `cargo build` vs `cargo xtask build`) can trigger a full,
unexpectedly slow rebuild instead of reusing the previous run's cache, since
the two invocations don't always share build artifacts. If a command that
should be instant suddenly starts recompiling `ring`/`rustls`/`lance*` from
scratch, that's the likely cause -- let it finish once rather than
interrupting it, and prefer `cargo xtask test --suite Full` for the next full
check instead of another narrow invocation.

## Suite-to-command mapping

| Change | Focused command |
| --- | --- |
| Chunking or scanning | `cargo xtask test --suite Chunk` (or `cargo test -p lci-core --lib chunk::` while iterating) |
| Retrieval filters or source roles | `cargo xtask test --suite Filter` (or `cargo test -p lci-core --lib filter::`) |
| Evaluation definitions or metrics | `cargo xtask test --suite Evaluation` |
| Index lifecycle or persistence | `cargo xtask test --suite Indexing` |
| Readiness and health | `cargo xtask test --suite Readiness` |
| MCP transport and tools | `cargo xtask test --suite Mcp` |
| Watched refresh | `cargo xtask test --suite Watching` |
| LSP/navigation behavior | `cargo xtask test --suite Lsp` (or `cargo test -p lci-core --lib lsp::` for adapter-only logic) |

`cargo xtask test --suite Full` runs formatting, all workspace tests, and Clippy.
Live model acceptance (`cargo xtask acceptance <name>`) remains separate because
it depends on locally running services. Generated reports belong under
`test-results` and are never committed.

The integration cases remain one Cargo test binary. Their source is divided by
behavior so agents can read and run a narrow group without paying the compile
and link cost of many separate integration binaries.

## Optional: sccache and a faster linker

Neither is required, and neither belongs in a commit: `rustc-wrapper` and
`linker` overrides are machine-specific (an absolute toolchain path, a tool
that may not be installed elsewhere) and would break another contributor's
build or CI's runner if placed in the project's own `.cargo/config.toml`. Set
them in your *personal* global `~/.cargo/config.toml` instead:

```toml
[build]
rustc-wrapper = "sccache"   # after `winget install Mozilla.sccache` or `cargo install sccache`

[target.x86_64-pc-windows-msvc]
linker = "<path to your toolchain's bundled rust-lld.exe>"   # bundled under
# <rustup toolchain dir>/lib/rustlib/<target>/bin/rust-lld.exe; use the
# equivalent target triple's rust-lld on macOS/Linux if you want the same win.
```

On a machine with both set up, a full-workspace `cargo xtask test --suite
Full` after touching a file outside `lci-core` dropped from 8-9 minutes to
under a minute in practice -- most of that from sccache reusing previously
compiled objects across the large `lancedb`/`arrow` dependency tree, the rest
from lld linking faster than the default MSVC linker on a binary this size.
