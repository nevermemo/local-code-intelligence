# Local live acceptance (no GitHub Actions)

`cargo xtask live-acceptance` runs the exact same commands as
[`.github/workflows/live-acceptance.yml`](../../.github/workflows/live-acceptance.yml)
-- `cargo xtask acceptance <name>` and `evaluate ./evaluations/core.toml`
against the real embedding/reranker services and real language-server
executables -- but directly in a normal terminal on this machine, with no
GitHub repo, no self-hosted runner registration, and no dependency on
GitHub's control plane at all. Like the rest of `xtask` (`AGENTS.md`: "Dev/
build/test/acceptance tasks run through the `xtask` crate ... not shell
scripts, so the workflow stays portable across operating systems"), it's
plain cross-platform Rust (`xtask/src/live_acceptance.rs`), not a
Windows-only PowerShell script -- process spawning and tree-killing go
through `std::process`/`sysinfo`, the same toolkit
`xtask::acceptance::ManagedServer` already uses elsewhere in this crate.

## Why this exists alongside the GitHub Actions path

Two independent reasons surfaced in the same evening of actually running
the GitHub Actions version for the first time:

1. **No GitHub account/billing dependency.** Self-hosted runner minutes
   aren't billed by GitHub regardless of repo visibility, so cost was never
   actually the issue -- but registering, reconnecting, and keeping a
   self-hosted runner's session alive turned out to be its own source of
   friction (stale sessions after an ungraceful process kill, a PATH that
   silently differs from your interactive shell's, needing a terminal
   window kept open). None of that exists when the command just runs in
   your own already-correctly-configured shell.
2. **Per-step timeouts instead of one blind ceiling.** GitHub Actions'
   `timeout-minutes` is a single wall-clock bound for the *entire* job --
   it has no concept of "this step is still making real progress, give it
   more room" vs. "this step has been dead for an hour." The first fully
   green run of `live-acceptance.yml` was itself killed by exactly this:
   a 60-minute job timeout cut off a run that had already passed every
   acceptance step and was just finishing evaluation/cleanup. Bumping the
   number (now 150 minutes) helps, but it's still one blind guess covering
   ~19 steps of wildly different expected duration (a small-fixture
   provider-isolation check should fail fast if it's stuck; a cold
   embedding pass against a real external workspace legitimately needs
   room to run long). `cargo xtask live-acceptance` gives each step its
   *own* timeout instead, so a genuinely stuck fast step is caught in
   minutes, not after burning through a budget sized for the slowest step.

GitHub Actions isn't being retired by this -- `live-acceptance.yml` still
works and is still useful as a from-anywhere trigger (`workflow_dispatch`
from GitHub's UI) if the runner is up. This is simply the lower-friction
default for day-to-day use.

## What "per-step timeout" actually buys you (and what it doesn't)

Each step is polled every 30 seconds. If a step exceeds *its own* timeout,
only that step's process tree is killed -- not the whole run -- and it is
reported as `TimedOut` before the run moves on. While a step runs, the
30-second poll also logs its accumulated CPU time, purely as information
for whoever's watching the log.

That CPU number is deliberately **not** used to decide anything
automatically. A step legitimately waiting on a slow embedding-service HTTP
response, or a cold-indexing LSP server, can sit at near-zero CPU growth
for minutes while being completely healthy -- the wait is I/O-bound, not
CPU-bound. Auto-killing on "no CPU activity" would just trade one kind of
false failure (too-short blind timeout) for another (killing healthy
network-bound work). So the per-step timeout is the only thing that
actually terminates a step; CPU/elapsed-time logging is there so a human
watching the log can tell "still working, be patient" from "this has been
identical for 20 minutes, something's actually wrong" -- a *hybrid* of a
hard ceiling (the real safety net) and an activity signal (for human
judgment), rather than pretending activity alone can be fully automated
here.

This command is a supervisor *outside* the process it's watching, so it can
only ever see wall-clock time and coarse OS-level signals like CPU time --
it has no way to see "real progress" inside an opaque blocking call that
reports none. The genuinely "smart" fix for that lives in the product code
itself, not in a wrapper: `lci-core/src/lsp/transport.rs`'s per-request
LSP timeout and `lci-core/src/models.rs`'s embedding/reranker HTTP timeouts
are the same shape of blind timeout this command works around, and the real
fix there is resetting the deadline on genuine progress signals the
protocol already provides (e.g. LSP's `$/progress` notifications) rather
than one fixed wait for the whole call.

## Usage

```bash
# Full suite, same order as live-acceptance.yml
cargo xtask live-acceptance

# Skip the initial `cargo build --workspace` (assumes an up-to-date build)
cargo xtask live-acceptance --skip-build

# Only specific steps, by step name or xtask acceptance subcommand substring
cargo xtask live-acceptance --only python,core --skip-build
```

Logs land under `test-results/live-acceptance-logs/<timestamp>/` (gitignored
via `/test-results/`): one `<step>.stdout.log` / `<step>.stderr.log` pair
per step, plus a `summary.txt`. The command exits non-zero if any step
didn't pass.

Same prerequisites as the GitHub Actions path -- see the table in
[`live-acceptance-ci.md`](live-acceptance-ci.md) -- except here they need to
be on **your interactive shell's PATH**, not a runner process's. A tool
installed mid-session (e.g. via `npm install -g` or `winget install`) is
usually visible immediately in a *fresh* terminal -- but not in one that was
already open before the install, since a shell's PATH is captured at
startup and doesn't pick up later machine-wide PATH changes on its own.
(This bit a real run once already: a long-lived shell reported both Java
LSP steps failing with "start java: program not found" even though `java`
was correctly installed and on PATH -- just not on *that* shell's cached
copy of it. Re-running from a freshly opened terminal passed cleanly.)

## Optional: run automatically on push

A `pre-push` git hook is included at [`.githooks/pre-push`](../../.githooks/pre-push)
but **not enabled by default** -- enabling a hook changes what happens on
every future `git push`, so that's your call, not something to switch on
silently. To enable it:

```bash
git config core.hooksPath .githooks
```

Once enabled, pushing anything that includes `main` starts
`cargo xtask live-acceptance` in the background (so `git push` itself isn't
blocked for the suite's runtime) and prints a one-line note. The hook
doesn't block the push on failure or report back to git in any way --
check `test-results/live-acceptance-logs/` (or just run the command directly
instead of relying on the hook) for results. To disable again:

```bash
git config --unset core.hooksPath
```
