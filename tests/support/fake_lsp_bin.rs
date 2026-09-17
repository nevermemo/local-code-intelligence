// The `fake-lsp-server` binary is a standalone `[[bin]]` target, so it cannot
// use the `include!("support/mod.rs")` path that the integration test binary
// uses. Instead it pulls in the same `fake_lsp` module directly via `#[path]`
// and delegates to the shared, loop-based `run_from_env` implementation. This
// makes the richer scenario logic in `fake_lsp.rs` the single source of truth
// for both the test binary and the spawned child process.
#[cfg(not(test))]
#[path = "fake_lsp.rs"]
mod fake_lsp;

#[cfg(not(test))]
fn main() {
    if let Err(error) = fake_lsp::run_from_env() {
        eprintln!("fake-lsp: {error}");
        std::process::exit(1);
    }
}
