# Bench set v1

`bench-set-v1.json` is the frozen task set of the spike bench
([../bench.md](../bench.md)). Data only: repository URLs, commit ids, query text taken
from commit messages, and gold file paths and declaration names. No code from these
repositories is stored here; the runner clones them at run time.

## Repositories

Chosen by licence (permissive), size (each larger than this repository, which has about
62k lines of `.rs`/`.py`/`.ts`/`.go` at `b87b16a`) and language (three languages LCI
supports). Line counts are of `.rs`, `.py` and `.go` files at the snapshot (`wc -l`).

| Key | URL | Licence (checked by hand) | Language | Snapshot | Source files / lines |
| --- | --- | --- | --- | --- | --- |
| tokio | https://github.com/tokio-rs/tokio | MIT | Rust | `e279adbfcd88` | 810 / 187k |
| caddy | https://github.com/caddyserver/caddy | Apache-2.0 | Go | `d293dcfe86c1` | 382 / 121k |
| black | https://github.com/psf/black | MIT | Python | `11cff1312bf9` | 378 / 138k |

## Size and filtering

60 tasks: the 20 most recent qualifying commits of each repository, walking
`git log --first-parent` back from the snapshot (tokio 317 commits scanned, caddy 88,
black 157). Mined on 2026-10-12 with:

```bash
cargo xtask bench mine \
  --source tokio=<clone> --source caddy=<clone> --source black=<clone> \
  --license tokio=MIT --license caddy=Apache-2.0 --license black=MIT \
  --max 20 --out docs/spike/multimap/bench/bench-set-v1.json
```

A commit qualifies when it is not a merge or a revert; its subject has at least four
words and is not a formatting, lint or typo fix; its subject or body contains `fix`,
`fixes`, `fixed`, `fixing`, `bug`, `bugfix` or `regression` as a word (anywhere in the
body, so a `Fixes #n` trailer counts and so does "add regression tests"); and it modifies one to
three non-test source files (supported extension, not vendored or generated) and adds,
deletes or renames none. Duplicate query text is dropped. The skip counts per reason are
in each repository's `skipped` block; most skipped commits are not fixes.

Tags: 42 `keyword` and 18 `keyword-agnostic` tasks (the query names a gold file stem or
has an identifier shape, or not), 14 `names-symbol` (the query names a gold symbol).
121 gold symbols in all; three tasks have none.

Gold symbols are approximate by construction. Per hunk of `git diff -U0 base fix`: the
declaration on the first changed code line if there is one (a changed signature, a new
function); otherwise, when that line is indented (a change inside a body), the name in
git's funcname hunk header, which is the nearest declaration above the hunk; hunks that
change only comments or blank lines, hunks whose first code line is top-level code that
is not a declaration, and names under three characters give none. A first version that
took every hunk header was checked against the commits by the independent review and
named the preceding declaration for doc-comment, appended-code and attribute changes;
the rules above replace it (before any result used the set). Level B scores symbols only
as a labelled proxy; level C scores files.

## Using it

- Evaluation data, never ranking input: do not tune retrieval to these tasks.
- A new version is a new file (`bench-set-v2.json`); v1 does not change, so results
  stay comparable.
