# S1 evidence: chunk identity across this repository's history
Status: frozen measurement (2026-10-12, slice S1, built); it supports decision N-5 in [specs/node-id.md](specs/node-id.md) and is indexed in [results.md](results.md).
The figures and commands below are as measured at that time; the code has moved since, so rerun before quoting a number.

Measured 2026-10-12T11:15Z (`date -u`) in the cloud run that built S1. The question: does
dropping `start_line` from chunk identity (node-id.md, decision behind lesson L4) keep ids
that today's fusion key loses? Read this as one repository (this one), 200 commit pairs.

Extended 2026-10-12T18:23Z (the spike-trim run, after the independent sanity review): the
tool now states how many commits it replayed, pairs chunks by position to count ids that
name the wrong copy, and a second example measures the symbol keys. The section "By
construction or measured" says which figure is which.

## Command (ran)

```text
cargo run -p lci-core --release --example node_stability -- --rev 66f999d --commits 200
```

`66f999d` is the head of `fix/verified-bugs` that S1 started from. The tool
(`lci-core/examples/node_stability.rs`) walks the 201 first-parent commits ending there
(the oldest is `b8b0b8f`), chunks every committed source file that the commit's own
root-level `.lciignore` keeps (the scanner's chunker, BOM handling and non-source check; not
replayed: `.lciignore` files below the root, of which none exists in any of the 201 commits, and the walker's
`.gitignore` handling, because committed files count as tracked), and
compares each consecutive pair. Wall time 27.6 s (`time`, warm debug build, the command
without `--release`); re-run at 11:34Z after the review fixes, with the
same numbers. It reads git objects
only.

Commits replayed (ran, 2026-10-12T18:20Z, full clone): the tool's first line reads
`rev 66f999d: replayed 201 first-parent commits (200 pairs) of 200 pairs requested; oldest
b8b0b8fe69; shallow clone: no`. The tool takes whatever history exists: on a shallow clone it
replays fewer pairs, says so on that line and warns on stderr (checked on a depth-50 clone:
`200 pairs requested, history has 49 (shallow clone: yes)`). A figure from this tool is
comparable to the table below only when that line says 200 pairs.

Definitions: a chunk of the older commit "did not change" when the same file in the newer
commit has a chunk with the same `content_hash`. "Kept" means the older id is present in
the newer snapshot (set membership), for the Chunk NodeId `(path, content_hash, ordinal)`
and for the legacy fusion key `(path, start_line, content_hash)`. "Files whose chunks
changed or moved" are files where any chunk's hash or line range differs between the two
commits.

## Result (ran)

| Chunks whose text did not change | Count | NodeId kept | Legacy key kept |
| --- | ---: | ---: | ---: |
| all | 470887 | 470887 (100.00%) | 463759 (98.49%) |
| in files whose chunks changed or moved | 13658 | 13658 (100.00%) | 6530 (47.81%) |

- Pairs where the two counts differ: 97 of 200.
- Unchanged chunks with ordinal above 0 (a duplicate text earlier in the same file):
  10111 of 470887 (2.1%).
- Fusion keys shared by two or more NodeIds in any one snapshot (decision N-5, two
  same-hash chunks on one start line): 0 in all 201 snapshots.

The ten pairs with the largest gap (unchanged chunks in edited files: NodeId kept versus
legacy key kept): `3c92aac` 367/84, `8888858` 452/214, `bef9772` 373/139, `ab7de12`
303/74, `10d771e` 303/84, `cf15de4` 300/115, `2edf919` 368/184, `0491f1d` 235/60,
`e4dc6f0` 217/57, `15bc4c6` 215/55.

## Ids that survive but name another copy (ran)

Set membership cannot see an id that is still present but now names a different chunk (a
duplicate inserted earlier in the file shifts the ordinals of the later copies). The tool now
pairs chunks by position: `git diff -U0` of the file's two blobs (Myers with the indent
heuristic, pinned so a user's diff settings cannot change it) maps each older chunk's lines
to the newer file, and the newer chunk with the same text on the mapped lines is the
successor. An id "points at another chunk" when it resolves to a newer chunk other than the
successor. Only files that changed and hold a repeated `content_hash` in either version are
diffed; elsewhere each text has one copy, so an id cannot name another (this restriction is
exact, not a sample). Same command, same 200 pairs, 4.2 s release build:

| Key | Points at the successor | Points at another chunk | Lost |
| --- | ---: | ---: | ---: |
| Chunk NodeId | 4064 | 53 | 0 |
| Legacy fusion key | 1266 | 70 | 2781 |

- Checked: 4118 unchanged chunks in such files; 1 had no successor on the mapped lines and
  is in neither row.
- 12 pairs hold a wrong-pointing id; the largest are `2edf919` (24 NodeId / 23 legacy),
  `9c15e3d` (12 / 9) and `58ae2c2` (12 / 11).
- `--explain` lists each case. All 53 NodeId cases are chunks whose whole text is a nameless
  header fragment: 50 are `"pub mod"`, 2 `"impl Default for"`, 1 `"impl Drop for"`. Today's
  Rust chunker emits `pub mod adapter;` as two chunks, `"pub mod"` and `"adapter;"` (checked
  with `chunks_for_path` on `lci-core/src/lsp.rs` at `66f999d`), so every such line of a file
  shares one hash, and adding a module line above others re-points the later fragments'
  ordinals. No full declaration chunk was re-pointed in these 200 pairs.
- Synthetic checks (ran): two functions `alpha`, `beta`, then a copy of `beta` inserted above
  `alpha`, gives 1 right (`alpha`) and 1 wrong (`beta`'s id now names the inserted copy);
  three identical `beta` functions with one deleted gives membership 2 of 3 (one ordinal is
  lost) and 2 right, 0 wrong.
- Blind spot: when an identical whole-line block is inserted next to its copy, the diff
  cannot tell which copy is new and aligns the insertion after the existing one, so the id
  points at the "successor" and the counter sees nothing. The successor is whatever the
  pinned diff alignment says. In the 53 cases here the full lines differ (`pub mod csharp;`
  and `pub mod go;`), so their alignment is unambiguous; 53 is a lower bound that leaves out
  repeats whose whole lines are identical.

## Symbol keys (ran)

```text
cargo run -p lci-core --release --example symbol_keys -- --rev 66f999d
```

`lci-core/examples/symbol_keys.rs` runs `rust_declarations` on every tracked `.rs` blob at the
revision and joins each declaration's Symbol NodeId with its path. 0.5 s release build.

| Revision | `.rs` files | Parse failures | Declarations | Distinct ids | Disambiguator > 0 | Ids from two or more declarations |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `66f999d` | 98 | 0 | 2524 | 2524 | 1 | 0 |
| `b87b16a` (S2 head) | 107 | 0 | 2768 | 2768 | 1 | 0 |

- Kinds at `66f999d`: fn 1425, method 445, const 248, struct 211, mod 129, enum 50, type 11,
  static 4, trait 1. These equal the sanity review's uncommitted count (2,524, 1, 0).
- The one disambiguated declaration is
  `sym:rust:tests::cases::navigation::process_is_running/fn~1` (two `#[cfg]` variants).
- A synthetic tree with `b.rs` and `src/b.rs`, each declaring `fn bar`, gives one id from two
  declarations, so the cross-file count is not vacuous.
- Not measured: agreement with rust-analyzer's symbols, other Rust repositories, `#[path]`
  (decision N-9), and macro-generated items.

## By construction or measured

| Figure | Kind |
| --- | --- |
| Chunk NodeId kept 100% (set membership) | Measurement of this history. Insertions cannot lose an id (the text and every lower ordinal stay), but deleting an earlier copy of a repeated text while a later one stays drops the highest ordinal (synthetic check above); no commit in these 200 pairs did that. |
| Chunk NodeId lost 0 in the positional pairing | Measurement of this history, for the same reason. |
| Distinct symbol ids within one file | By construction: the disambiguator counts repeats. |
| Legacy key kept 98.49% / 47.81% | Measurement of this history: any key with `start_line` breaks that often. |
| 10111 unchanged duplicates (2.1%), 0 N-5 twins | Measurement of this history and chunker. |
| 53 NodeIds and 70 legacy keys naming another chunk | Measurement of this history and chunker, against git's diff alignment. |
| 1 disambiguated declaration, 0 cross-file symbol ids, 0 parse failures | Measurement of this tree's code. |
| 201 commits, 200 pairs replayed | Printed by the tool from the history it walked. |

## What it shows (inferred)

- The NodeId's 100% is close to construction but not a guarantee: an id built from the
  unchanged text survives whenever the text does, except where a duplicate inserted earlier
  in the file shifts an ordinal (the id survives but names another copy) or an earlier copy
  is deleted (the highest ordinal is lost). No deletion of that kind happened here. Membership
  hides the shift; the positional pairing above measures it: 53 of 4118 checked chunks (1.3%), all nameless header fragments that today's chunker
  splits off. The legacy key re-points more often (70) and loses far more (2781). The ordinal
  is the failure mode the key adds over `(path, content_hash)`; it is silent (the id still
  resolves), and on this tree it hit only those fragments.
- The legacy key breaks for 1.5% of all unchanged chunks and for 52% of the unchanged
  chunks in the files a commit touches, in about half of all commits. Anything keyed by it
  across commits (a candidate dump replayed on a later tree, a paused traversal after a
  re-index, a gold label tied to a chunk, a RunRecord join) loses those chunks; NodeId does
  not.
- Within one snapshot the two keys are interchangeable here: N-5 never occurred, so
  T0's bit-exact equality does not depend on how N-5 is resolved on this repository.
- Against the key LCI already uses across commits (the vector cache key, `content_hash` or
  `content_hash + path`), the NodeId adds only the ordinal and the symbol keys; by membership
  `(path, content_hash)` also keeps 100% here (sanity review, section 2.3).
- The symbol key with the impl header is injective on this repository's Rust; it is one
  repository and one language.
- Not measured: other repositories, other languages' share of the effect, and whether
  any downstream consumer exists yet. The advantage is real but only pays off once
  something stores chunk identity across commits (S5 drift handling, replays on a newer
  tree); today nothing does.
