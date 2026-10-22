# Live acceptance in CI

Status: no self-hosted runner is registered today. `.github/workflows/live-acceptance.yml` runs only if the owner
registers one (below); the default way to run the suite is [local-live-acceptance.md](local-live-acceptance.md), which
needs no GitHub account, runner or repository.

## What the workflow runs

`.github/workflows/live-acceptance.yml` runs `cargo xtask acceptance <name>` and `evaluate ./evaluations/core.toml`
against the real embedding and reranker services (ports 8766 and 8767) and real language-server executables. It is one
step, `cargo xtask live-acceptance --require-all` (N87): the orchestrator builds (`build --workspace --exclude xtask`),
runs the steps in `acceptance_steps` in `xtask/src/live_acceptance.rs` and the evaluation, and `--require-all` makes a
step skipped for a missing tool fail the run. A unit test pins the command in the workflow file.

`.github/workflows/ci.yml` is separate: its hosted matrix runs only `cargo xtask test --suite Full` against mocks on
GitHub-hosted runners, which have no route to services on a developer machine.

Why a self-hosted runner is the only complete option: hosted runners cannot reach `localhost` services, and the
`*-lsp` acceptance subcommands spawn and inspect real `rust-analyzer`, `csharp-ls`, `pyright` and
`typescript-language-server` processes, which a port-forwarding tunnel would not cover.

## Workflow properties

- Triggers: manual `workflow_dispatch` only (d538). With no runner registered, a push trigger only queued runs that
  never started; add `push` to `main` back once the `lci-live` runner exists. Never `pull_request`, because a
  self-hosted runner must not execute code from a fork PR.
- `runs-on: [self-hosted, lci-live]`.
- `concurrency: { group: live-acceptance, cancel-in-progress: false }`: runs queue on the one runner.
- `timeout-minutes: 150`. The first real run took a little over an hour end to end, and the original 60-minute bound
  cut off a run that had passed every acceptance step; a single job bound cannot tell a slow healthy step from a dead
  one, which is why the orchestrator also gives each step its own timeout.
- The orchestrator gives every fixture a run-unique root (`lci-live-run-<pid>-<stamp>` under the temp directory, passed
  to each step as `LCI_ACCEPTANCE_FIXTURE_ROOT`, where `Fixture::create` puts every fixture) and removes only that
  directory when the run ends, never another run's fixtures or any other `lci-*` directory. A run killed before that
  point leaves its root behind. It does not kill processes by name: the runner is a developer machine, and a blanket
  `Stop-Process -Name rust-analyzer` would also kill editor language servers and a running `serve`. An orphaned
  process after a crash needs a human look at the process list.
- Because the workflow never runs on `pull_request`, it cannot affect a merge: `test (windows-latest)` in `ci.yml` is
  the only gating job. With `--require-all` a runner missing any tool in the table below turns the run red, which is
  the intent: the runner is registered to host all of them.

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
when its tool is not on `PATH`, so a missing tool degrades one step. `cargo xtask live-acceptance` lists it as SKIPPED
and, without `--require-all`, ends with "Completed with skipped steps." and exit code zero. The workflow passes
`--require-all`, so there a skip fails the run.

| Tool | Needed for | Notes |
| --- | --- | --- |
| `rust-analyzer` | `cargo xtask acceptance lsp` | |
| `dotnet` + `csharp-ls` | `acceptance csharp-lsp`, `csharp-missing`, `csharp-recovery` | |
| `pyright` (`pyright-langserver`) | `acceptance python-lsp`, `python-missing`, `python-recovery` | |
| `typescript-language-server` | `acceptance typescript-lsp`, `typescript-missing`, `typescript-recovery` | |
| `go` + `gopls` | `acceptance go-lsp`, `go-missing`, `go-recovery` | `go install golang.org/x/tools/gopls@latest` |
| JDK 21+ and a jdtls install | `acceptance java-lsp`, `java-missing`, `java-recovery` | Temurin 25 was used. jdtls has no `PATH` lookup: set `LCI_ACCEPTANCE_JDTLS` or pass `--java <dir>` (see `xtask/src/acceptance/java_lsp.rs`); either names the jdtls install directory, which `[java].path` also points at, not an executable. Without one the Java steps print `PREREQUISITE_UNAVAILABLE` |
| `clangd` | `acceptance c-lsp`, `c-missing`, `c-recovery`, `cpp-lsp`, `cpp-missing`, `cpp-recovery` | `winget install LLVM.clangd`; one instance navigates both `c` and `cpp` |
| `marksman` | `acceptance markdown-lsp`, `markdown-missing` | A release binary from the marksman GitHub releases; no `PATH` lookup is needed: pass `--marksman <path>` or set `LCI_ACCEPTANCE_MARKSMAN`. The fixture is prose only (`[index] prose = true`) |
