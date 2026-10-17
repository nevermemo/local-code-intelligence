# Local live acceptance (no GitHub Actions)

`cargo xtask live-acceptance` (`xtask/src/live_acceptance.rs`) runs the commands of
[`live-acceptance.yml`](../../.github/workflows/live-acceptance.yml), `cargo xtask acceptance <name>` and
`evaluate ./evaluations/core.toml`, against the real embedding and reranker services and real language-server
executables, from a normal terminal on this machine. It needs no GitHub repository and no self-hosted runner. It is
plain Rust (`std::process` and `sysinfo` for spawning and killing process trees, as `xtask::acceptance::ManagedServer`
does), like every other dev, build, test and acceptance task: `cargo xtask <command>`, never shell scripts.

## Relation to the GitHub Actions workflow

Cost was never the reason for a local default: self-hosted runner minutes are not billed by GitHub, whatever the
repository's visibility. `live-acceptance.yml` is not retired; it remains a from-anywhere trigger (`workflow_dispatch`
from GitHub's UI) when a runner is up (see [live-acceptance-ci.md](live-acceptance-ci.md)).

## Steps and timeouts

Each step has its own timeout, so a stuck fast step fails in minutes instead of waiting out one job-wide bound
(`live-acceptance.yml` has a single `timeout-minutes: 150`). The run has 27 steps in this order:

| Step | Timeout |
| --- | --- |
| Build: `cargo build --workspace --exclude xtask` (omitted by `--skip-build`) | 20 min |
| Acceptance `core` (index, search, reindex smoke test; cold-embeds this repository) | 20 min |
| Acceptance `multilingual` (fixture indexing, search, decoy ranking) | 15 min |
| Acceptance `lsp` (rust-analyzer) | 30 min |
| 21 small-fixture LSP steps: `<lang>-lsp`, `<lang>-missing`, `<lang>-recovery` for python, typescript, go, java, csharp, c, cpp | 10 min each |
| `lci config --json` (records the configuration layers the evaluation will use) | 1 min |
| `evaluate ./evaluations/core.toml --workspace self=<repo> --output test-results/evaluation.json` | 15 min |

The build excludes `xtask` because the orchestrator is itself `xtask.exe`: on Windows the link step of a full
`--workspace` build cannot replace the running executable. The acceptance steps run the already-built xtask binary
for the same reason.

A step that exceeds its own timeout has its process tree killed, is reported as `TimedOut`, and the run moves on.
Each step is polled every 2 seconds; every 30 seconds the log prints elapsed time and the step's accumulated CPU time.
That CPU number only informs a human reading the log and never kills anything: a step waiting on a slow embedding
response or a cold-indexing language server can sit near zero CPU while healthy, so only the timeout terminates a step.
The supervisor sees wall-clock time and coarse OS signals only. The LSP transport (`lci-core` `lsp/transport.rs`)
already bounds a request by inactivity; the embedding and reranker HTTP calls (`lci-core` `models.rs`) keep a fixed
timeout with retries.

## Usage

```bash
cargo xtask live-acceptance                              # full suite, same order as live-acceptance.yml
cargo xtask live-acceptance --skip-build                 # assume an up-to-date build
cargo xtask live-acceptance --only python,core --skip-build   # steps whose name or subcommand contains a substring
cargo xtask live-acceptance --require-all                # a skipped step (missing tool) becomes a failure
```

A step whose tool is missing is listed as SKIPPED and the run ends with "Completed with skipped steps." Logs go to
`test-results/live-acceptance-logs/<timestamp>/` (gitignored via `/test-results/`): a `<step>.stdout.log` and
`<step>.stderr.log` per step and a `summary.txt`. The exit code is non-zero if any step fails or times out; a skipped step
(missing tool) leaves the exit code zero unless `--require-all` is given, which counts each skip as a failure. Use
`--require-all` wherever the whole matrix must have run.

The prerequisites are the table in [live-acceptance-ci.md](live-acceptance-ci.md#prerequisites), but the tools must be on
the interactive shell's `PATH`, not a runner process's. A shell captures `PATH` at startup: a tool installed during the
session (for example with `npm install -g` or `winget install`) is visible in a fresh terminal but not in one opened
before the install. A long-lived shell once failed both Java LSP steps with "start java: program not found" although
`java` was installed; a fresh terminal passed.

## Optional: run on push

[`.githooks/pre-push`](../../.githooks/pre-push) is not enabled by default, because enabling a hook changes every
future `git push`. To enable it:

```bash
git config core.hooksPath .githooks
```

A push that includes `main` then starts `cargo xtask live-acceptance` in the background, so the push is not blocked for
the suite's runtime (about 50 minutes or more), and prints a one-line note. The hook neither blocks the push on failure
nor reports back; read `test-results/live-acceptance-logs/` for the result. To disable it:

```bash
git config --unset core.hooksPath
```
