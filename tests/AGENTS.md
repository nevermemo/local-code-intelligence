# Test guidance

- Shared fakes and fixtures live in `tests/support`: the fake model HTTP
  service (`mod.rs`) and the fake LSP (`fake_lsp.rs`, built as the
  `fake-lsp-server` binary from `fake_lsp_bin.rs`). Behavioral cases live in
  `tests/cases/*.rs` and are registered as modules of the single
  `tests/integration.rs` binary (`evaluation`, `indexing`, `mcp`,
  `navigation`, `readiness`, `watching`). `cargo xtask test --suite <name>`
  filters on those module names.
- Automated tests never need live services on ports 8765-8767. Use the fakes
  and temporary data directories; never contact 8765 from any test.
- Iterate with the focused suite (`cargo xtask test --suite Indexing`, or
  `cargo test -p lci-core --lib <module>::` for `lci-core`-only logic). Run
  `cargo xtask test --suite Full` once when the slice is complete. Every
  focused suite fails if its filter matches zero tests.
- Test observable contracts and recovery: parse and embedding request
  counts for incremental work, preserved snapshots after a failed update,
  fail-open paths, filtered-empty results. Do not test implementation
  details or documentation wording.
- A failing gate is fixed, or the change is reverted. Never weaken, delete,
  or `#[ignore]` a test to make a gate pass.
- Live acceptance (`cargo xtask acceptance <name>`,
  `cargo xtask live-acceptance`) is separate evidence. Its reports go to
  `test-results/` and are never committed.
