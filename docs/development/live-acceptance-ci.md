# Live acceptance in CI

> Prefer running this locally instead of through GitHub Actions? See
> [`local-live-acceptance.md`](local-live-acceptance.md) -- same commands,
> same real services, driven from a normal terminal instead of a registered
> self-hosted runner. It sidesteps GitHub Actions' own blind
> `timeout-minutes` (see that doc for why a fixed wall-clock bound isn't a
> great fit for this suite) and needs no GitHub account, runner
> registration, or repo at all.

`.github/workflows/live-acceptance.yml` runs `cargo xtask acceptance <name>`
and `evaluate ./evaluations/core.toml` against the *real* embedding/reranker
services (ports 8766/8767) and real language-server executables, the same
commands documented in [`../../README.md`](../../README.md#direct-cli-and-acceptance-test).
`.github/workflows/ci.yml`'s hosted matrix never touches any of that -- it
only runs `cargo xtask test --suite Full` against mocks, on GitHub-hosted
runners that have no route to services running on your machine.

There is no way around that: GitHub-hosted runners cannot reach `localhost`
services on your machine, and several acceptance subcommands (`csharp-lsp`,
`python-lsp`, `typescript-lsp`, ...) spawn and inspect real
`rust-analyzer`/`csharp-ls`/`pyright`/`typescript-language-server` *processes*,
not just HTTP calls -- a port-forwarding tunnel would only cover the
embedding/reranker calls, not those. A
[self-hosted runner](https://docs.github.com/en/actions/hosting-your-own-runners)
registered on this machine (or another one you control that already has these
services and tools installed) is the only complete option, and that's what
`live-acceptance.yml` targets (`runs-on: [self-hosted, lci-live]`).

## What's already prepared

- The workflow file itself (`.github/workflows/live-acceptance.yml`):
  triggers on `push` to `main` and manual `workflow_dispatch` only -- **never**
  `pull_request`, since a self-hosted runner must not execute arbitrary code
  from a fork PR.
- `concurrency: { group: live-acceptance, cancel-in-progress: false }` so
  pushes queue on the one runner instead of piling up concurrently.
- A final `if: always()` step that sweeps `%TEMP%\lci-*` fixture directories
  left behind by a crashed run (each acceptance fixture is named
  `lci-<fixture>-<pid>` by `Fixture::create`, so this pattern can't match
  anything else). It deliberately does **not** force-kill processes by name:
  unlike a hosted runner's fresh VM per run, this runner *is* your machine,
  and a blanket `Stop-Process -Name rust-analyzer` (etc.) would just as
  happily kill your editor's language servers or the long-running
  `local-code-intelligence serve` process as any genuinely orphaned one from
  a crashed job. If a crashed run does leave an orphaned process behind,
  that needs a human glance at Task Manager, not a workflow step guessing by
  process name alone.

## One-time setup (needs the repository owner's GitHub account)

These steps need the owner's GitHub account and hands-on access to the
machine that hosts the services, so an agent cannot do them:

1. **Repository.** Done: `origin` is the project's GitHub repository
   (`git remote -v`). Actions only runs against a repo hosted on GitHub.
2. **Register a self-hosted runner** on this machine (or wherever the
   embedding/reranker services and language-server tooling live): repo
   Settings -> Actions -> Runners -> "New self-hosted runner", or via `gh`:
   ```bash
   gh api repos/<owner>/<repo>/actions/runners/registration-token
   ```
   then follow the download/config steps GitHub shows, and give it the
   `lci-live` label during `config.cmd`/`config.sh` (`--labels lci-live`) so
   only this workflow schedules onto it. Check
   `gh api repos/<owner>/<repo>/actions/runners` before registering a runner,
   to see which runners already exist and whether they are online.
3. **Install the runner as a persistent Windows service**
   (`.\svc install` / `.\svc start` from the runner's install directory)
   rather than leaving it running in a terminal window, so it survives
   reboots and logouts.
4. **The repository is public** (as of 2026-10-11, per
   `gh api repos/<owner>/<repo> --jq .visibility`), so check GitHub's fork-PR
   protections (repo Settings -> Actions -> General) before registering the
   runner, and keep relying on this workflow's `pull_request`-less trigger
   too -- self-hosted runners on public repos are a known code-execution risk
   surface if that default is ever changed.

## Prerequisites

Last checked on the development machine 2026-10-11 (`Get-Command` and
`--version` for each tool): every tool below was present; jdtls is the one
that needs its directory named explicitly.

| Tool | Needed for | Status |
| --- | --- | --- |
| `rust-analyzer` | `cargo xtask acceptance lsp` | installed |
| `dotnet` + `csharp-ls` | `cargo xtask acceptance csharp-lsp/-missing/-recovery` | installed |
| `pyright` (`pyright-langserver`) | `cargo xtask acceptance python-lsp/-missing/-recovery` | installed |
| `typescript-language-server` | `cargo xtask acceptance typescript-lsp/-missing/-recovery` | installed |
| `go` + `gopls` | `cargo xtask acceptance go-lsp/-missing/-recovery` | installed (`go install golang.org/x/tools/gopls@latest`) |
| JDK 21+ + jdtls install | `cargo xtask acceptance java-lsp/-missing/-recovery` | installed (Temurin 25). jdtls has no `PATH` lookup: set `LCI_ACCEPTANCE_JDTLS` or pass `--java <dir>` (`xtask/src/acceptance/java_lsp.rs:5-11`); either names the jdtls install *directory*, which `[java].path` also points at, not an executable. Without one of them the Java steps print `PREREQUISITE_UNAVAILABLE` |
| `clangd` | `cargo xtask acceptance c-lsp/-missing/-recovery`, `cpp-lsp/-missing/-recovery` | installed (`winget install LLVM.clangd`) -- one instance navigates both `c` and `cpp` |

Every `*-lsp`/`*-missing`/`*-recovery` subcommand skips gracefully (prints
`PREREQUISITE_UNAVAILABLE`) rather than failing when its tool isn't on
`PATH`, so a missing optional tool degrades that one step rather than the
whole workflow -- but every tool in the table has to be reachable for full
coverage.
