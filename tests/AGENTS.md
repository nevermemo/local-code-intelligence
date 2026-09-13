# Test guidance

Keep shared fake services and fixtures in `tests/support`. Put behavioral cases
in `tests/cases` and register them from the single `tests/integration.rs` binary.
Run the focused suite while iterating and the full deterministic suite once at
the end. Automated tests must not require live services on ports 8765-8767.
