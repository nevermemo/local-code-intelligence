# Live acceptance in CI

Status: no self-hosted runner is registered today. `.github/workflows/live-acceptance.yml` runs only if the owner
registers one (below); the default way to run the suite is [local-live-acceptance.md](local-live-acceptance.md), which
needs no GitHub account, runner or repository.

## What the workflow runs

`.github/workflows/live-acceptance.yml` runs `cargo xtask acceptance <name>` and `evaluate ./evaluations/core.toml`
against the real embedding and reranker services (ports 8766 and 8767) and real language-server executables. It starts
with `cargo build --workspace` (the local orchestrator uses `build --workspace --exclude xtask`). The step list is
`acceptance_steps` in `xtask/src/live_acceptance.rs`.

`.github/workflows/ci.yml` is separate: its hosted matrix runs only `cargo xtask test --suite Full` against mocks on
GitHub-hosted runners, which have no route to services on a developer machine.

Why a self-hosted runner is the only complete option: hosted runners cannot reach `localhost` services, and the
`*-lsp` acceptance subcommands spawn and inspect real `rust-analyzer`, `csharp-ls`, `pyright` and
`typescript-language-server` processes, which a port-forwarding tunnel would not cover.

## Workflow properties

- Triggers: `push` to `main` and manual `workflow_dispatch`; never `pull_request`, because a self-hosted runner must
  not execute code from a fork PR. With no runner registered, a push to `main` leaves the job queued.
- `runs-on: [self-hosted, lci-live]`.
- `concurrency: { group: live-acceptance, cancel-in-progress: false }`: pushes queue on the one runner.
- `timeout-minutes: 150`. The first real run took a little over an hour end to end, and the original 60-minute bound
  cut off a run that had passed every acceptance step; a single job bound cannot tell a slow healthy step from a dead
  one, which is why the local orchestrator gives each step its own timeout.
- A first step sets `LCI_ACCEPTANCE_FIXTURE_ROOT` to a run-unique directory (`lci-live-run-<run id>-<attempt>` under
  `RUNNER_TEMP`), where `Fixture::create` puts every fixture. A final `if: always()` step removes only that directory
  (fixtures left by a crashed run), never another run's fixtures or any other `lci-*` directory. It does not kill processes by name: the runner is a developer machine, and a blanket
  `Stop-Process -Name rust-analyzer` would also kill editor language servers and a running `serve`. An orphaned
  process after a crash needs a human look at the process list.

## One-time setup (needs the repository owner's GitHub account)

An agent cannot do these steps:

1. Repository: `origin` is the project's GitHub repository (`git remote -v`); Actions runs only on a hosted repository.
2. Register a self-hosted runner on the machine that hosts the services and language-server tooling: repo Settings,
   Actions, Runners, "New self-hosted runner", or `gh api repos/<owner>/<repo>/actions/runners/registration-token`,
   then the download and config steps GitHub shows, with `--labels lci-live` so only this workflow schedules onto it.
   List existing runners first with `gh api repos/<owner>/<repo>/actions/runners`.
3. Install the runner as a persistent service so it survives reboots and logouts, by the procedure in GitHub's
   self-hosted runner documentation ("Configuring the self-hosted runner application as a service"). On Windows the
   service is set up with the runner's own `config.cmd` (it offers to run the runner as a service); on Linux and macOS
   the runner directory has `svc.sh` (`install`, then `start`). The runner uses the machine's account, so check that
   account can see the tools in the Prerequisites table on its `PATH`.
4. The repository is public (checked 2026-10-11 with `gh api repos/<owner>/<repo> --jq .visibility`): review the
   fork-PR protections under Settings, Actions, General before registering, and keep the `pull_request`-less trigger.
   Self-hosted runners on public repositories are a code-execution risk if that default changes.

## Prerequisites

Checked on the development machine 2026-10-11 (`Get-Command` and `--version` per tool): every tool below was present.
Every `*-lsp`, `*-missing` and `*-recovery` subcommand prints `PREREQUISITE_UNAVAILABLE` and skips rather than fails
when its tool is not on `PATH`, so a missing tool degrades one step. The workflow runs steps one by one, so a skip ends
a step successfully there; `cargo xtask live-acceptance` lists it as SKIPPED and ends with "Completed with skipped
steps."; its `--require-all` flag turns a skip into a failure.

| Tool | Needed for | Notes |
| --- | --- | --- |
| `rust-analyzer` | `cargo xtask acceptance lsp` | |
| `dotnet` + `csharp-ls` | `acceptance csharp-lsp`, `csharp-missing`, `csharp-recovery` | |
| `pyright` (`pyright-langserver`) | `acceptance python-lsp`, `python-missing`, `python-recovery` | |
| `typescript-language-server` | `acceptance typescript-lsp`, `typescript-missing`, `typescript-recovery` | |
| `go` + `gopls` | `acceptance go-lsp`, `go-missing`, `go-recovery` | `go install golang.org/x/tools/gopls@latest` |
| JDK 21+ and a jdtls install | `acceptance java-lsp`, `java-missing`, `java-recovery` | Temurin 25 was used. jdtls has no `PATH` lookup: set `LCI_ACCEPTANCE_JDTLS` or pass `--java <dir>` (see `xtask/src/acceptance/java_lsp.rs`); either names the jdtls install directory, which `[java].path` also points at, not an executable. Without one the Java steps print `PREREQUISITE_UNAVAILABLE` |
| `clangd` | `acceptance c-lsp`, `c-missing`, `c-recovery`, `cpp-lsp`, `cpp-missing`, `cpp-recovery` | `winget install LLVM.clangd`; one instance navigates both `c` and `cpp` |
