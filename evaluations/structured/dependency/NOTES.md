# Dependency stratum (item 2, slice 2a, second stratum)

Purpose. The 24 config-semantics families in the parent directory almost never have lockfile lines among the first 50
lines of `rg` output, so they can only test harm from the lockfile exclusion rule (`docs/spike/multimap/structured-grep-prereg.md`
section 3). This stratum is where the rule can help: a package name declared in a manifest and also listed (often many times) in
a lockfile. Each family asks for one declaring line in one named manifest. Schema: parent `SCHEMA.md` plus
`"stratum": "dependency"`, `readers: []`, `readers_scope: "none:package-manager"`, `docs: []`, one `setting_location` question
with `names_key: true` that names the package and the owning package/crate by role, never by path. Validator:
`cargo xtask gold validate-structured --gold-dir evaluations/structured/dependency --tree lci=<clean export of the lci commit>
--tree vite=<export> --tree home-assistant=<export>` (the home-assistant tree is unused: no file for it, see below).

Revisions: lci `579a5bec69309d98c78825654aabe8f03ab64421` (files read from git blobs of that commit, rg run on a `git archive`
export of it, not the working tree, which holds the gold itself); vite `7a89794331526a67bc9b29047b86957c581c128c`;
home-assistant `bd92409b6ade6e2873cac603104f23e5d9e7ec61`. Hashes are sha256 over CRLF-normalised blob bytes.

## Sampling frame (`_work/frame.py`, output `_work/frame.json`)

Declarations come from tracked, non-lockfile manifests at the revision: `package.json` (dependencies, devDependencies,
peerDependencies, optionalDependencies), `Cargo.toml` ([dependencies], [dev-dependencies], [build-dependencies],
[workspace.dependencies], also under `[target.<cfg>.*]` and as sub-tables), `pyproject.toml` (project.dependencies,
project.optional-dependencies, dependency-groups, build-system.requires), `manifest.json` (`requirements`). The name is kept
if it matches `^[A-Za-z_][A-Za-z0-9_]*$`, has at least 6 characters, and is not in `COMMON_NAMES` (read from
`lci-maps/src/live_grep.rs` at the lci commit), and if the premise command
`rg --no-config -n -w --hidden -g '!.git' --sort path --max-filesize 1M -- <name> .` run at the tree root prints at least one
line from one of the 11 lockfile basenames among its first 50 output lines. To save time rg only ran for names that occur as
a whole word in some lockfile of the tree (a necessary condition for the filter, so the frame is unaffected). The kept names
are ordered by SplitMix64 seed 20261018 over the names sorted bytewise (`order` of `evaluations/structured/_work/split.py`).

| Stage | lci | vite | home-assistant |
| --- | ---: | ---: | ---: |
| tracked paths | 582 | 2,861 | 27,203 |
| manifests parsed | 4 | 309 | 1,505 |
| declarations | 73 | 492 | 1,276 |
| distinct package names | 43 | 330 | 1,182 |
| plain identifiers | 24 | 75 | 761 |
| and length >= 6, not common | 12 | 44 | 670 |
| and word-matched in a lockfile (prefilter) | 12 | 43 | 0 |
| **frame** (lockfile line in rg's first 50) | **11** | **28** | **0** |

Home Assistant: the frame is empty, so there is no `home-assistant.json`. Its only lockfile is a small
`pnpm-lock.yaml` for a Playwright test package, and its Python requirements never appear in it. This agrees with the premise
(4 of 150 config keys with lockfile noise in the first 50, none of them dependency names). Not padded.

Frame order (the walk order for acceptance):
lci: futures, reqwest, lancedb, protobuf, tempfile, globset, tracing, sysinfo, schemars, serde_json, ignore.
vite: lodash, premove, nanoid, parse5, tinyexec, foobar, mrmime, artichokie, clipboard, oxlint, globals, browserslist,
tailwindcss, tinyglobby, picomatch, miniflare, systemjs, tinyspy, slash3, slash5, sugarss, fsevents, phoenix, vitepress,
autoprefixer, tsdown, periscopic, stream.

## Acceptance rules and skipped candidates

Walk the frame order and accept a candidate when one manifest can be named unambiguously by role. If a package is declared in
two or three manifests, the gold is the manifest whose path sorts first bytewise (a fixed rule, not a choice), and the question
singles it out by the owning package or crate name. More than three manifests, or two declarations of the name in the chosen
manifest, would be skipped with that reason. Stop at 8 per repository. `build_gold.py` refuses a `decisions.json` that is
not a prefix walk of the frame order.

Skipped candidates: **0** in lci and **0** in vite. The first 8 of each frame were accepted; the rule above did not trigger
(the widest case is `foobar`, three playground manifests, exactly the allowed limit; `tailwindcss` and `tinyglobby` with four
manifests each sit later in the order and were never reached). Nothing was reordered or cherry-picked.

## Families

Counts are descriptive only (`_work/stats.py`; same command as above, lci on the clean export, vite on its export without
`.git`, so ignore files are not applied, as in the premise). "All lines" counts output lines, "Rank" is the 1-based position of
the gold line in the unfiltered output. A rank above 50 means the gold line is outside rg's first 50 lines today.

| Family | Split | Gold (manifest role, section, line) | All lines | Lockfile lines | Lockfile lines in first 50 | Rank of gold line |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| lci.dep.futures | dev | root app crate, [dependencies], 17 | 101 | 88 | 49 | 90 |
| lci.dep.reqwest | heldout | root app crate, [dependencies], 23 | 128 | 10 | 10 | 16 |
| lci.dep.lancedb | dev | root app crate, [dependencies], 22 | 57 | 2 | 2 | 3 |
| lci.dep.protobuf | heldout | lci-maps, [dependencies], 19 | 25 | 5 | 5 | 15 |
| lci.dep.tempfile | heldout | root app crate, [dev-dependencies], 43 | 216 | 10 | 10 | 11 |
| lci.dep.globset | heldout | lci-core, [dependencies], 12 | 9 | 3 | 3 | 7 |
| lci.dep.tracing | heldout | root app crate, [dependencies], 31 | 156 | 38 | 10 | 79 |
| lci.dep.sysinfo | heldout | xtask, [dependencies], 21 | 9 | 2 | 2 | 5 |
| vite.dep.lodash | dev | `@vite/test-config-sibling` fixture, devDependencies, 6 | 75 | 45 | 20 | 9 |
| vite.dep.premove | heldout | vite, devDependencies, 122 | 7 | 3 | 3 | 4 |
| vite.dep.nanoid | heldout | vite, devDependencies, 112 | 11 | 6 | 6 | 1 |
| vite.dep.parse5 | heldout | vite, devDependencies, 115 | 14 | 3 | 3 | 4 |
| vite.dep.tinyexec | heldout | create-vite, devDependencies, 40 | 20 | 11 | 11 | 3 |
| vite.dep.foobar | heldout | `@vitejs/test-dep-with-optional-peer-dep-cjs` fixture, peerDependencies, 7 | 40 | 6 | 6 | 28 |
| vite.dep.mrmime | heldout | vite, devDependencies, 111 | 20 | 4 | 4 | 3 |
| vite.dep.artichokie | dev | vite, devDependencies, 92 | 9 | 3 | 3 | 4 |

Splits: per repository, ids sorted bytewise, SplitMix64 shuffle, first floor(n/4) = 2 dev (changed from floor(n/2) before any run so the held-out half reaches 12 questions; prereg section 7) (`_work/split.py`; the validator checks it).

Manifest counts per family (declared in): futures 2 (root, lci-maps), reqwest 3 (root, lci-core, xtask), lancedb 1, protobuf 1,
tempfile 2 (root, lci-core), globset 1, tracing 2 (root, lci-core), sysinfo 1; lodash 1, premove 1, nanoid 1, parse5 1,
tinyexec 2 (create-vite, vite), foobar 3 (three optimize-deps playground packages), mrmime 1, artichokie 1.

Per-family cautions (the gold line is one specific line; neighbouring lines also match the word):
- `lci.dep.tracing`: the next line declares `tracing-subscriber`, which `-w` also matches.
- `lci.dep.protobuf`: a comment line in the same manifest mentions `protobuf`.
- `vite.dep.lodash`: the line above declares `@types/lodash`.
- `vite.dep.premove`: the vite package's `scripts` also run the `premove` command.
- `vite.dep.foobar`: each of the three playground manifests also lists `foobar` under `peerDependenciesMeta`.
- `lci.dep.futures`, `lci.dep.tempfile`, `lci.dep.tracing`, `vite.dep.tinyexec` and `vite.dep.foobar` have another declaring
  manifest with identical line text; only the question's role separates them.

## Reproduction

`python -I _work/frame.py` (arguments: the lci worktree and a clean export of its commit, the two bare repos with their
exports, the rg binary, `--out`), then `build_gold.py` (reads `frame.json` and `decisions.json`, blobs from git, `--check` to
compare), `split.py`, `stats.py` (prints the table above). All trees are arguments; no path is stored in any file here.

## Limits

- This is not a sample of the population of dependency questions; it is the first 8 of a mechanically defined frame, in a
  seeded order, so it describes where the lockfile rule could help or hurt, not how often.
- The frame needs a lockfile hit in the first 50 lines, so by construction the lockfile rule has something to remove for every
  family. It does not guarantee the rule changes the outcome: nanoid's gold line is already rank 1.
- Frame sizes depend on rg's `--sort path` order and on exports without ignore files; the lci export holds no `.git`, so
  `.gitignore` is not applied there either.
- Python requirements (`manifest.json`, `pyproject.toml`) were parsed, but Home Assistant yields no frame member.
- Dependency names are matched as written; case variants (`PyJWT` vs `pyjwt`) are not unified.
