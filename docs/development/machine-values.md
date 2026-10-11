# Machine values

This repository is public, so nothing machine-specific is committed: no
absolute path, user name, non-default port or launch command. Committed text
(docs, skills, roadmap, briefs, scripts) names a value by its **role** and the
**resolver** that finds it, for example "the work area (`cargo xtask env
work_area`)". The value itself lives in exactly one local home. A fresh clone
works on defaults; a configured machine overrides them locally.

## The agent rule

A missing machine value is a lookup, never a reason to guess or to stop.

1. **Resolve it.** A product value (data directory, port, MCP/REST/health
   URLs, embedder or reranker URL and model, ripgrep and language-server
   paths) comes from `lci config --json`. A development-tooling value (work
   area, plans, frozen binaries, evidence, logs, checkpoint, experiment root,
   scratch area, the repository root, the test-results directory) comes from
   `cargo xtask env` (`--json` for all of them, or `cargo xtask env KEY` for
   one bare value).
2. **Create a missing directory.** A resolved directory that does not exist
   yet is created with `cargo xtask env --ensure` (or `--ensure KEY`). That
   is the normal first use on a fresh machine, not an error.
3. **Report a missing tool or service.** If a tool is not installed or a
   service is not running, say so and continue with the rest of the decided
   flow. Never substitute another tool, and never start or stop a model
   service on your own (`AGENTS.md`, "Live services"). To see what this
   machine has, run `lci init --dry-run`: it prints what it detects and
   writes nothing. When no local overlay exists, `lci init` writes one. When
   one exists, `lci init` exits 3 and leaves it alone: report the difference
   to the human. `--force` replaces hand edits, so use it only with the
   human's go.
4. **Never invent a path.** Do not pick a directory because it looks
   plausible, copy one from an old note, or stop the work over a missing one.
   Ask the resolver; if the resolver names a directory, create it.

## Roles and resolvers

| Role (say this in committed text) | Resolver | Where the value lives |
| --- | --- | --- |
| the data directory | `lci config --json` -> `values.data_dir` | local overlay, `LCI_DATA_DIR` |
| the LCI port, the MCP URL, a profile's MCP URL, the REST base, `/health`, `/ready` | `lci config --json` -> `derived.*` | `port` in the local overlay, `LCI_PORT`, `serve --port` |
| the embedder and reranker URL and model | `lci config --json` -> `values.*` | local overlay, `LCI_*` |
| ripgrep and the language servers | `lci config --json` -> `values.*`; `lci init` finds them | local overlay, `LCI_*` |
| the repository root | `cargo xtask env repo_root` | the checkout |
| the test-results directory | `cargo xtask env test_results` | `<repo_root>/test-results` |
| the work area (durable local copies outside the repository) | `cargo xtask env work_area` | `.local/machine.toml`, `LCI_DEV_WORK_AREA` |
| the plans, frozen-binary, evidence and logs directories | `cargo xtask env plans_dir`, `frozen_bin_dir`, `evidence_dir`, `logs_dir` | follow the work area unless set |
| the run checkpoint, the development serve config (files) | `cargo xtask env checkpoint`, `dev_config` | follow the work area unless set |
| the experiment root (the A/B area) | `cargo xtask env experiment_root` | `.local/machine.toml`, follows the work area by default |
| the scratch area (true temporaries) | `cargo xtask env scratch_area` | `.local/machine.toml`; a client with its own session scratchpad may set `LCI_DEV_SCRATCH_AREA` |
| model-serving launch commands | none | memory and the run checkpoint only, never a repository file (`AGENTS.md`, "Live services") |

Product values are described in README "Configuration" (the layers, the local
overlay, `lci config`, `lci init`). The rest of this page covers the
development-tooling resolver.

## `cargo xtask env`

```text
cargo xtask env                    every role: key = value  (source: origin)
cargo xtask env KEY                one bare value (for scripts and hooks)
cargo xtask env --json [KEY]       values, sources, the machine file, the LCI_DEV_* used
cargo xtask env --ensure [KEY]     create the missing directories, print what happened
cargo xtask env --init [--force]   write .local/machine.toml for this machine
```

Resolution, lowest to highest: the built-in default, `.local/machine.toml` at
the repository root, then the `LCI_DEV_<KEY>` environment variable (an empty
value is unset). A relative value resolves against the repository root. The
command reads only those (in a linked worktree it also asks git where the main
worktree is) and creates nothing unless `--ensure` or `--init` says so.

| Key | Kind | Default |
| --- | --- | --- |
| `work_area` | directory | `<local data dir>/local-code-intelligence-dev/work` |
| `experiment_root` | directory | `<work_area>/experiments` |
| `scratch_area` | directory | `<system temp dir>/local-code-intelligence-scratch` |
| `plans_dir` | directory | `<work_area>/plans` |
| `frozen_bin_dir` | directory | `<work_area>/bin` |
| `evidence_dir` | directory | `<work_area>/evidence` |
| `logs_dir` | directory | `<work_area>/logs` |
| `checkpoint` | file | `<work_area>/run/checkpoint.md` |
| `dev_config` | file | `<work_area>/cfg/dev.toml` |
| `repo_root`, `test_results` | directory | the checkout and `<repo_root>/test-results`; fixed, not settable |

`<local data dir>` is the platform's local data directory (`%LOCALAPPDATA%` on
Windows, `~/Library/Application Support` on macOS, `$XDG_DATA_HOME` or
`~/.local/share` on Linux). The scratch directory's name deliberately does not
start with `lci-`, which `cargo xtask live-acceptance` sweeps from the system
temp directory when a run ends. An `LCI_DEV_<KEY>` variable is the key in
capitals (`LCI_DEV_WORK_AREA`, `LCI_DEV_PLANS_DIR`, `LCI_DEV_DEV_CONFIG`).
Variable names are matched case-insensitively (Windows treats them so; the
names are upper-cased on every platform, so a lower-case `lci_dev_work_area`
also works on Linux and macOS); do not set two names that differ only in case.
A variable with that prefix that names no key is listed under `env.unknown`
in `--json` and ignored.

On Windows keep any `data_dir` placed under the work area short (`AGENTS.md`,
"Working method": 150 characters).

### The machine file

`.local/machine.toml` is gitignored (`/.local/`). Every key is a path string;
an unknown key is an error that names it, and `repo_root` and `test_results`
cannot be set. It need hold only what differs from the defaults:

```toml
work_area = 'D:/lci-work'
experiment_root = 'D:/lci-ab'
```

`cargo xtask env --init` writes the file for this machine: `work_area` and
`scratch_area` set to today's defaults, every other role as a commented line
showing its current value, so those keep following the work area until a line
is uncommented. It never overwrites an existing file without `--force`.

A linked git worktree (an implementer's `git worktree add`) with no
`.local/machine.toml` of its own reads the main worktree's, found through
`git rev-parse --git-common-dir` (its parent is the main worktree root), so
every worktree resolves the same values. A worktree's own file wins, so
`--init` in a worktree that reads the main worktree's file refuses without
`--force` (a file written there would shadow the shared values); with
`--force` it writes the worktree's own file and prints a note. In
`--json`, `file.path` names the file that was used and
`file.from_main_worktree` says whether it was the main worktree's;
`file.state` is `present` or `absent` (then `file.path` is where `--init`
would write).

### `--ensure`

`--ensure` creates the directory each role lives in: the directory roles
themselves (`work_area`, `experiment_root`, `scratch_area`, `plans_dir`,
`frozen_bin_dir`, `evidence_dir`, `logs_dir`) and, for the two file roles
(`checkpoint`, `dev_config`), the directory that will hold the file. It never
creates a file, not even the checkpoint. Each line says `created` or `exists`;
a path that exists but is not a directory is reported as `failed` and left
alone. `--ensure KEY` does one role. It is idempotent, so running it before
using a role is always safe.

## Writing rule

Committed files name machine values by role with the resolver in backticks:
"the work area (`cargo xtask env work_area`)", "the MCP URL (`lci config
--json`)". Absolute paths, user names and ports other than the documented
defaults live only in the local overlay, `.local/machine.toml`, the
client's local settings, the agent's memory and the run checkpoint. Examples
in committed text are generic placeholders (`<work_area>`, `D:/lci-work`),
never a real machine's value. `cargo xtask lint-machine-paths` checks the
rule (next section).

## Lint

`cargo xtask lint-machine-paths` fails when committed text names a machine
value. It runs first in `cargo xtask test --suite Full`; its unit tests are
`cargo test -p xtask lint::`.

**What it scans.** Every tracked text file (`git ls-files`). It skips binary
files and the vendored `.agents/skills/rust-skills/**`. Untracked and
gitignored files, where machine values belong, are not scanned; a new file is
checked once it is staged, so run the lint again after `git add`.

**What it flags.** Each hit prints `file:line: <pattern name>` and the command
exits 1. The built-in patterns are generic:

| Pattern | Matches |
| --- | --- |
| drive-letter user profile path | a drive letter, a colon, a separator, the Users folder, a separator and a name; either kind of separator (an escaped, doubled backslash too), any case |
| macOS user directory | `/Users/`, a name and a slash (exact case, so a web route in lower case is fine) |
| Linux home directory | `/home/`, a name and a slash |
| `AppData` path component | the word AppData as a path component, directly followed by a separator |
| client session folder | a `Temp` component, a separator and the client's `claude` folder |

A name starts with a letter or digit. A placeholder such as `<name>`,
`%USERNAME%`, `$USER` or `{user}` is therefore not a hit, while a literal like
`you` or `someone` is a name and is a hit. `%LOCALAPPDATA%` and
`$env:LOCALAPPDATA` are variables, not paths, and are fine. A URL that happens
to contain one of the patterns is a hit too; reword it.

**Local patterns.** Owner-specific tokens (a user name, the name of a local
folder) are not generic, and committing them would publish them. Put them, one
literal per line, in `.local/lint-machine-paths.txt` at the repository root:

```text
# one literal per line; blank lines and lines starting with # are ignored
your-user-name
your-local-folder-name
```

`.local/` is gitignored, so the file stays on the machine. Each machine writes
its own, and a linked worktree reads only its own `.local/`, not the main
worktree's. A line matches anywhere in a line of text, ignoring case, so write a short
token with its separators (`\name\`, `/name/`) when it could occur inside an ordinary word. A hit on
a local pattern prints `local pattern (line N of .local/lint-machine-paths.txt)`
and never the token. Without the file only the built-in patterns apply; a
clean run ends with a line saying how many local patterns it read.

**Fixing a hit.** Replace the value with its role and resolver from the table
above, and keep everything else in the sentence (report names, commit hashes,
numbers). A launch command or start script becomes "the reranker start script
(kept in the owner's memory, never committed)". An example path becomes a
placeholder in angle brackets (`<work_area>/plans`) or a path with no
user-profile folder (`D:/lci-work`); a test fixture that needs an absolute path
uses one of those too. In a quoted historical line, reword as little as
possible. The lint has no allowlist: reword instead.

The lint scans its own source and tests, so they never spell a pattern out;
the matchers are code and the sample lines are assembled at run time.

## Live evaluation provenance

`cargo xtask live-acceptance` runs the evaluation with the inherited
environment and without `--config`, so the user-level local overlay and any
`LCI_*` variable apply to it. Just before the evaluation it records `lci
config --json` for the same binary and environment as its own step; that
step's stdout log in the run's log directory shows the layers and values the
evaluation used.
