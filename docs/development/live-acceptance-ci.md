# Live acceptance in CI

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

## What only you can do

These need your GitHub account and hands-on-machine actions -- I can't do
them for you:

1. **Create a GitHub repository and push.** This repo currently has no git
   remote at all (`git remote -v` is empty, `main` has no upstream). Actions
   only runs against a repo hosted on GitHub.
2. **Register a self-hosted runner** on this machine (or wherever the
   embedding/reranker services and language-server tooling live): repo
   Settings -> Actions -> Runners -> "New self-hosted runner", or via `gh`:
   ```bash
   gh api repos/<owner>/<repo>/actions/runners/registration-token
   ```
   then follow the download/config steps GitHub shows, and give it the
   `lci-live` label during `config.cmd`/`config.sh` (`--labels lci-live`) so
   only this workflow schedules onto it.
3. **Install the runner as a persistent Windows service**
   (`.\svc install` / `.\svc start` from the runner's install directory)
   rather than leaving it running in a terminal window, so it survives
   reboots and logouts.
4. **If this repo ever becomes public**, double-check GitHub's fork-PR
   protections are still in effect before relying on this workflow's
   `pull_request`-less trigger alone -- self-hosted runners on public repos
   are a known code-execution risk surface if that default is ever changed.

## Prerequisites checked on this machine (2026-09-19)

| Tool | Needed for | Status |
| --- | --- | --- |
| `rust-analyzer` | `cargo xtask acceptance lsp` | installed |
| `dotnet` + `csharp-ls` | `cargo xtask acceptance csharp-lsp/-missing/-recovery` | installed |
| `pyright` (`pyright-langserver`) | `cargo xtask acceptance python-lsp/-missing/-recovery` | not installed -- `npm install -g pyright` |
| `typescript-language-server` | `cargo xtask acceptance typescript-lsp/-missing/-recovery` | not installed -- `npm install -g typescript-language-server typescript` |
| `go` + `gopls` | `cargo xtask acceptance go-lsp/-missing/-recovery` | installed (`go install golang.org/x/tools/gopls@latest`) |
| JDK 21+ + jdtls install | `cargo xtask acceptance java-lsp/-missing/-recovery` | installed (Temurin 25; jdtls extracted to `C:\Users\micro\tools\jdtls` -- `[java].path`/`--java <dir>` point at that directory, not an executable) |

Every `*-lsp`/`*-missing`/`*-recovery` subcommand skips gracefully (prints
`PREREQUISITE_UNAVAILABLE`) rather than failing when its tool isn't on
`PATH`, so a missing optional tool degrades that one step rather than the
whole workflow -- but installing the two missing ones above gets full
coverage.
