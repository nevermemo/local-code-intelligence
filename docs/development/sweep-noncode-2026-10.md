# Non-code sweep, 2026-10 (findings not fixed)

One sweep over the repository's non-code files on 2026-10-12 (UTC), branch
`cloud/sweep-noncode` from `fix/verified-bugs` at `bfbe51d`. The fixes made
during the sweep are in their own commits (references, stale facts,
duplication); this file lists only what was **not** fixed. Some items were
report-only by scope (the roadmap, `.github/workflows/**`, dated evidence, and
the meaning of a rule). Others need a decision rather than an edit.

Labels: **checked** = confirmed against the code or a command named in the
evidence; **inferred** = a judgment that was not proven.

Scope covered: `README.md`, `AGENTS.md`, `CLAUDE.md`, `tests/AGENTS.md`,
`src/app/AGENTS.md`, `docs/**`, `.agents/skills/lci-*/**`,
`.github/instructions/**`, `.github/scripts/**` comments, `.githooks/pre-push`,
`config.example.toml`, `Cargo.toml` files, `.cargo/config.toml`,
`evaluations/*.toml` headers, `.lciignore`, `.gitignore`, `.vscode/*`. Not
covered: the vendored third-party `.agents/skills/rust-skills/**` (also exempt
from `cargo xtask lint-machine-paths`, `xtask/src/lint.rs:26`).

Mechanical checks run over the covered files: relative Markdown links and
backticked repository paths resolve (or name a gitignored local file on
purpose), every `cargo xtask <sub>` and `--suite <name>` exists in
`xtask/src/main.rs` and `xtask/src/test.rs`, every MCP tool name exists in
`src/server.rs` (ten `#[tool]` functions), every documented config key with a
key-like suffix exists in `lci-core/src/config.rs` or `config/layers.rs` (the
only misses were report fields, derived URLs and planned keys in the roadmap),
README and `config.example.toml` defaults equal `Config::default`
(`lci-core/src/config.rs:620-664`), Markdown tables have consistent column
counts and fences are closed, and `codespell` reports nothing (one false
positive in the roadmap).

## Findings

| # | Where | Problem | Evidence | Suggested fix | Kind |
| --- | --- | --- | --- | --- | --- |
| 1 | `AGENTS.md:132`; `.agents/skills/lci-release-validation/SKILL.md:14`; `.../references/validation-matrix.md:25` | The rules say editor settings are never committed, but `.vscode/settings.json` and `.vscode/mcp.json` are tracked. | **checked**: `git ls-files .vscode` lists both; `.gitignore` has no `.vscode` entry. | The owner decides: either untrack them (`git rm --cached`, add `/.vscode/` to `.gitignore`), or state an exception for the shared MCP client entry (`mcp.json` holds only the default loopback URL). This is a rule-meaning question, so it was not edited. | rule / contradiction |
| 2 | `.agents/skills/lci-autonomous-improvement/references/templates.md:213` vs `SKILL.md` "Non-negotiables" and `CLAUDE.md` usage stop rule | The run-checkpoint template says "pause at 90% of the 5-hour window, stop at 90% of the weekly window". The skill and CLAUDE.md say to stop starting new work at 90% of **either** window, with no pause/resume distinction. | **checked** by reading the three lines. | The owner picks one wording. If "pause and resume after the 5-hour window resets" is the intended rule, add it to the skill's "Stopping at the usage limit" and point to it from the template and CLAUDE.md. | rule / contradiction |
| 3 | `.github/workflows/ci.yml:11-14` (comment) | The comment says live acceptance needs a real dotnet SDK and csharp-ls only "for the C# subcommands". Every `*-lsp` / `*-missing` / `*-recovery` family now needs its own server (TypeScript, Python, Go, Java, C/C++). | **checked**: `xtask/src/main.rs` `AcceptanceCommand` has seven LSP families. | Reword: "and, for each `<lang>-lsp` family, that language's real server (see docs/development/live-acceptance-ci.md)". | stale fact (workflow, report-only) |
| 4 | `.github/workflows/ci.yml:47` (comment) | "a runner without `rg` fails 24 of them" is a count that drifts as tests are added. | **inferred**: not re-counted (it needs a full test run without ripgrep). | Drop the number ("fails the lexical integration tests"), or re-measure it when the comment is next touched. | possibly stale count (workflow, report-only) |
| 5 | `docs/development/roadmap.md`, e.g. lines 320 (N54) | Backlog rows cite `file (line N)`, and some have drifted. | **checked**: N54 cites `is_absolute_pattern` at line 83 and `normalize_path` at line 347. They are now at `lci-core/src/filter/request.rs:91` and `lci-core/src/filter/source_role.rs:348`. Rows 302-343 and 574-576 have more such references, which were not all re-checked. | When an item is picked, re-resolve its references by symbol name (as the row names the function) rather than trusting the number; or drop line numbers where a function name is given. | stale reference (roadmap, report-only) |
| 6 | `CLAUDE.md:66` | Calls `.claude/settings.local.json` "git-excluded". No tracked ignore file excludes it (`.gitignore` has no such entry). | **checked** for `.gitignore`. **inferred**: the client or `.git/info/exclude` may exclude it on the owner's machine. | Add `/.claude/settings.local.json` to `.gitignore` so a fresh clone matches the claim, or say where the exclusion lives. | possibly wrong fact |
| 7 | `README.md:28-31` ("Documentation") | The list omits `docs/development/local-live-acceptance.md` (the default local path for live acceptance, which `xtask/src/main.rs` cites in the `LiveAcceptance` help text), `docs/development/agent-payload-notes.md`, and the roadmap. | **checked**: the files exist and are linked from elsewhere, not from this list. | Add one line each for the local live-acceptance guide and the roadmap; the payload notes can stay linked from the roadmap. Not edited, because it adds content rather than correcting it. | missing pointer |
| 8 | `tests/AGENTS.md:18-21` | The iteration bullet restates `AGENTS.md` "Working method", "Build and test" (focused suite, `-p lci-core`, Full once per slice, zero-match failure). | **checked** by reading both. | Shorten it to "Iterate per `AGENTS.md` 'Build and test'" plus the test-specific example (`--suite Indexing`). Left as is because the local file is read on its own when an agent works under `tests/`, and the restatement agrees with the owner. | duplication (minor) |
| 9 | `lci-core/src/config.rs:560-565` (Rust doc comment, outside this sweep's non-code scope) | `JavaLspConfig::enabled` says "a nonempty interpreter path", but `[java].path` is the jdtls installation directory. | **checked**: README "Configuration" table and `xtask/src/acceptance/java_lsp.rs:5-11`. | Change "interpreter path" to "installation directory" in the doc comment. | stale comment (code, out of scope) |

## Not findings (checked and left alone)

- `README.md:229-234` uses `C:\\code\\local-code-intelligence` as an example
  workspace. It has no user-profile folder, `cargo xtask lint-machine-paths`
  accepts it, and the README says it is an example of this repository's
  path.
- `docs/development/local-live-acceptance.md:38` "~19 steps" is part of a
  dated narrative about the first GitHub Actions run, kept as written.
- `.agents/skills/lci-retrieval-evaluation/references/evaluation-workflow.md`
  "eleven required self queries": `evaluations/core.toml` has 11 `self`
  and 3 `gust` queries (14 in all, matching "2 of 14 core queries").
- `docs/development/agent-payload-notes.md` "today" figures (for example
  `search_code` description 436 characters) describe the state before
  N28-N34, which are still open in the roadmap. It is a background record and
  was kept as written.
