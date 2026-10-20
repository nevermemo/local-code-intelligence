# Testing guide

Use the smallest suite that covers the changed contract while iterating. Run
the full deterministic gates once after the complete change is ready.

## The fastest loop: `lci-core`-scoped checks

The workspace is split into `lci-core` (chunking, language adapters, filters,
lexical search, manifest, workspace identity, config, models, LSP adapters --
everything with no dependency on LanceDB/arrow), `lci-maps` (the map layer and
traversal engine), the root `local-code-intelligence` crate (`App`, `Store`, the
MCP server, `main`) and `xtask` (see [architecture.md](../architecture.md#crates)).
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

## Prerequisites for the test gate

Beyond the build prerequisites in the [README](../../README.md#build-and-run):

- ripgrep (`rg`) on `PATH` is required for `cargo xtask test --suite Full` and for the integration suites, because the
  lexical channel runs it and the integration cases exercise that channel; without it they fail with "Lexical retrieval
  unavailable" (CI installs it with the system package manager and prints `rg --version` first, see
  `.github/workflows/ci.yml`). It stays optional for serving, where a missing `rg` only fails the lexical channel open.
- `git` on `PATH` for the cases noted in the table below.
- No model service, language server or network is needed: the tests use the fakes under `tests/support`.

## Suite-to-command mapping

| Change | Focused command |
| --- | --- |
| Any unit test, all crates | `cargo xtask test --suite Unit` (`cargo test --workspace --lib`) |
| Chunking or scanning | `cargo xtask test --suite Chunk` (or `cargo test -p lci-core --lib chunk::` while iterating) |
| Retrieval filters or source roles | `cargo xtask test --suite Filter` (or `cargo test -p lci-core --lib filter::`) |
| Configuration layers, the local overlay, `lci config`, or `lci init` | `cargo xtask test --suite Config` (or `cargo test -p lci-core --lib config::` / `cargo test -p local-code-intelligence --lib init::` while iterating) |
| Evaluation definitions or metrics | `cargo xtask test --suite Evaluation` |
| Mined-query generator (`xtask/src/mine.rs`) | `cargo test -p xtask mine::` (xtask is a binary crate, so the `--lib` unit suites do not run it); `--suite Evaluation` also checks the checked-in mined set |
| Development-tooling resolver (`xtask/src/env.rs`) | `cargo test -p xtask env::` (its worktree cases need `git` on `PATH`) |
| Machine-value lint (`xtask/src/lint.rs`) | `cargo test -p xtask lint::` (its repository cases need `git` on `PATH`); `cargo xtask lint-machine-paths` runs the lint itself and is the first step of `--suite Full` |
| Documentation link check (`xtask/src/docs.rs`) | `cargo test -p xtask docs::` (its repository case needs `git` on `PATH`); `cargo xtask docs check` runs the check itself and is the second step of `--suite Full` |
| Index lifecycle or persistence | `cargo xtask test --suite Indexing` (runs with the `map-equivalence` feature) |
| Readiness and health | `cargo xtask test --suite Readiness` |
| MCP transport and tools | `cargo xtask test --suite Mcp` |
| Watched refresh | `cargo xtask test --suite Watching` |
| LSP/navigation behavior | `cargo xtask test --suite Lsp` (or `cargo test -p lci-core --lib lsp::` for adapter-only logic) |

The unit-test suites (`Unit`, `Chunk`, `Filter`, and the unit halves of
`Evaluation` and `Lsp`) run `cargo test --workspace --lib <module>::`, so a
module filter reaches unit tests in both the root crate and `lci-core`, and
they share `--suite Full`'s feature resolution and build cache. Every focused
suite counts the tests it executed and fails when its filter matched none, so
a renamed or moved module can't make a suite pass vacuously.

`cargo xtask test --suite Full` runs, in order: the machine-value lint, the documentation link check, `cargo fmt --all -- --check`, `cargo test
--workspace --locked --features local-code-intelligence/map-equivalence`, and Clippy with the same `--locked` and feature
flags. `--locked` makes a pull request that edits `Cargo.toml` without its lockfile fail here instead of in a locked
build later; the focused suites stay unlocked. The `map-equivalence` feature compiles the test-only map entry points (the T0 bit-for-bit map check and the `RunRecord` paths); it was named `spike-maps` before decision d366, with no alias, so external CI scripts must use the new name.
Live model acceptance (`cargo xtask acceptance <name>`) remains separate because
it depends on locally running services. Generated reports belong under
`test-results` and are never committed.

The integration cases remain one Cargo test binary. Their source is divided by
behavior so agents can read and run a narrow group without paying the compile
and link cost of many separate integration binaries.

## Optional: cargo-nextest

`.config/nextest.toml` defines a `ci` profile for
[cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`):
`cargo nextest run --workspace --locked --features local-code-intelligence/map-equivalence --profile ci`
retries a failed test once and reports it as flaky, ends a test that runs past
5 minutes, and runs the language-server, model-client and process tests at
most two at a time. `cargo xtask test` does not use it.

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

Estimate, undated and unsourced: on a machine with both set up, a full-workspace
`cargo xtask test --suite Full` after touching a file outside `lci-core` dropped
from 8-9 minutes to under a minute -- most of that from sccache reusing previously
compiled objects across the large `lancedb`/`arrow` dependency tree, the rest
from lld linking faster than the default MSVC linker on a binary this size.

## Dependency policy

Rules for what goes into a `Cargo.toml` of this workspace (decisions d396, d397):

- Reuse a proven library where it fills a slot; the licence must be one the project accepts for code (permissive, MPL-2.0, GPL and LGPL families).
- A crate whose resolved version in `Cargo.lock` is `0.0.x` is pinned exactly (`=0.0.x`) in the manifest that names it, because Cargo treats every `0.0.x` release as breaking. Today `Cargo.lock` holds no `0.0.x` crate, so nothing is pinned for this rule; add the pin in the change that introduces one. Other `=` pins in a manifest keep the reason written beside them.
- Heavy third-party code (a large library or its transitive tree) is an optional dependency behind a Cargo feature that is off by default. The default build compiles none of it and stays unchanged (an optional dependency still appears in `Cargo.lock`).
- A dependency change that alters the default build is a deliberate change: say so in the commit message and check `git diff Cargo.lock` before committing.
