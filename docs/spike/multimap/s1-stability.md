# S1 evidence: chunk identity across this repository's history

Measured 2026-10-12T11:15Z (`date -u`) in the cloud run that built S1. The question: does
dropping `start_line` from chunk identity (node-id.md, decision behind lesson L4) keep ids
that today's fusion key loses? Read this as one repository (this one), 200 commit pairs.

## Command (ran)

```text
cargo run -p lci-core --example node_stability -- --rev 66f999d --commits 200
```

`66f999d` is the head of `fix/verified-bugs` that S1 started from. The tool
(`lci-core/examples/node_stability.rs`) walks the 201 first-parent commits ending there
(the oldest is `b8b0b8f`), chunks every committed source file that the commit's own
`.lciignore` keeps (the scanner's chunker, BOM handling and non-source check; the walker's
`.gitignore` handling is not replayed because committed files count as tracked), and
compares each consecutive pair. Wall time 27.6 s (`time`, warm build). It reads git objects
only.

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

## What it shows (inferred)

- The NodeId's 100% is by construction, not a finding: an id built from the unchanged
  text survives whenever the text does, except when a duplicate inserted earlier in the
  file shifts an ordinal. That exception did not remove any id here, but 2.1% of unchanged
  chunks are duplicates, and for them membership can hide a shift (the surviving id may
  name another copy). The measured content is the legacy side.
- The legacy key breaks for 1.5% of all unchanged chunks and for 52% of the unchanged
  chunks in the files a commit touches, in about half of all commits. Anything keyed by it
  across commits (a candidate dump replayed on a later tree, a paused traversal after a
  re-index, a gold label tied to a chunk, a RunRecord join) loses those chunks; NodeId does
  not.
- Within one snapshot the two keys are interchangeable here: N-5 never occurred, so
  T0's bit-exact equality does not depend on how N-5 is resolved on this repository.
- Not measured: other repositories, other languages' share of the effect, and whether
  any downstream consumer exists yet. The advantage is real but only pays off once
  something stores chunk identity across commits (S5 drift handling, replays on a newer
  tree); today nothing does.
