# Traverse call flow

Code-level picture of the flow of one `search_code mode="traverse"` / `lci traverse` call (`app::traverse::run`) and of the answer keys that the `[run_profile]` switches add (`answer_labels`, `card_paths`, `map_freshness`, `verify_on_read`); behaviour and defaults of those keys: [CLI reference](reference/cli.md). Crate layout: [architecture.md](architecture.md); the engine, maps and steering ladder: [engine.md](spike/multimap/engine.md); settings: [README.md](../README.md).

`search_code` with `mode = "traverse"` (needs `[traverse] enabled`) and `lci traverse` run one function,
`app::traverse::run`. It loads the SCIP index (map `graph.scip`, cached by file stamp; `[run_profile.scip] mode` `cache` (the default) reads it when it is there and parses an unreadable file once per file stamp, `authoritative` requires it, `off` never touches it and leaves the graph and the maps built from the index file empty, each saying so in a note), finds the start nodes
(`given`; `hybrid`: the top `seed_k` hits of the normal hybrid search mapped to SCIP symbols; `hybrid+bm25` or `bm25` with `[run_profile.seeds] bm25 = true`: the opt-in map `seeds.bm25`,
BM25 over the SCIP graph's node names, qualified paths and documentation, whose top `seed_k` nodes go after the hybrid hits or stand alone
when the search gave none; `...+literal` or `literal` with `[run_profile.seeds] literal = true`: the opt-in map `seeds.literal`, a whole-word ripgrep search (`lexical::search_literals`) for the names the question puts in code spans, each line placed in the SCIP graph (the symbol defined there, the one around it, else the file of a declaration line), appended after those sources; `...+repo_map` or `repo_map` with `[run_profile.seeds] repo_map = true`: the opt-in map `seeds.repo_map`, a personalized PageRank over the SCIP graph's call and containment edges from the entities `seeds.bm25` finds for the question, whose new top nodes go after the earlier sources' (each source form also exists with `+literal` after it); or `name_tokens`, the visible fallback), builds the steering ladder from the caller's rules, runs the `lci-maps` traversal under the
`[traverse]` caps (a request can only lower them), writes a trace under `data_dir`, and answers with the reached nodes,
totals and, only when work is left, a `handle` and `checkpoint`. The first stage (exact operators `exact.closure` and
`exact.pickaxe`, then seeds), named policies per question type (the router, chosen by the caller's `steer.policy`) and the steering ladder are described in
[engine.md](spike/multimap/engine.md). Default off: with `[traverse]`, `[recipes]` and `[compose]` disabled the tool
list, schemas and REST routes are exactly those of a server without them.

With `[run_profile.traverse] answer_labels = true` (default `false`) the answer also has `completeness` (`{complete, why, estimate}`: `complete` is `status == "complete"` with no map lost (`degraded`: a map the call asked for that could not be set up, or had `MapStat.failures`; `why` is then `degraded` unless the walk was cut), `why` the `stop_reason`, `estimate` `TraversalResult.estimate`, the `likely_complete` fraction) and every card of `results` and the checkpoint lists has `freshness` (`fresh`, `changed`, `stale` or `unverified`, read from `file_changed` and `maps`; a card that cites `lsp.live` is `unverified`, not `fresh`, when the walk found a language server running, `add_answer_labels`' `kept` argument from the walk's `WalkSessions`; a continuation also demotes the `lsp.live` cards of the nodes it restores from the checkpoint, its `restored` argument). `app::traverse::response::add_answer_labels` adds both to the response JSON after the trace and the checkpoint hash are made, so the trace, the checkpoint, the handle and `Card`/`card_json` are unchanged, nothing is withheld, and with the key off the answer is as before.

With `[run_profile.traverse] card_paths = true` (default `false`) every walked card of `results` and of `checkpoint.answer` also has `kind` and `path`, the
route from its start node with each hop's map, edge kind and witness (`app::traverse::response::add_paths`, read from the walk's state table; the engine keeps
`State::kind`, and a handle keeps it only under the key). With the key off the answer, the trace, the checkpoint and the handle are unchanged.

With `[run_profile.traverse] map_freshness = true` (default `false`) the answer also has `maps`: for each map of the call that
states a freshness, `{id, observed, stale, lag}` (`app::traverse::freshness`). The SCIP-built maps report `snapshot`;
`grep.live`, `lsp.live`, `git.history`, `exact.pickaxe` and `probe.compile` (which runs only under `rust_analyzer_trusted = true`) report `live` with `lag` 0 (`live` names the channel: `grep.live`,
`git.history`, `exact.pickaxe` and `probe.compile` read current bytes or git, placed by the index's positions for `grep.live`;
`lsp.live` asks a running server, which is fresh when this call started it and, when kept across calls under
`sessions = "shared"`, bounded by its tree stamp's coverage and labelled by a `kept session` note, with the same fields). `lag` is the number of source files (language-adapter
files, prose left out) newer than the index file plus those changed (content hash), added or removed since the process
first saw the index's current stamp (modified time and length), each counted once; `stale` means `lag` above 0 and `null`
means not compared (no tree, unreadable tree, or an index file without a modified time). The tree is read and hashed
once per call with `chunk::source_hashes` (the index scan's walk and its read, skip and hash function, no chunk made);
`[run_profile.freshness] stat_gate = true` (default `false`, in the profile hash) puts a process-wide `chunk::StatGate` per
root in front of the read, which keeps `(modified time, length, hash)` per file and reads only files whose stamp is new or
changed. A file modified within 3 seconds of the read is not trusted (racy stamp; FAT, exFAT and SMB stamps are coarse) and is read again next call. Its gap, asserted by a test: a same-length edit that keeps a modified time older than 3 seconds before the previous read is not seen until the next edit.
Search's `verify_on_read` reads through the same function. `Expanded` carries an optional
`edge::Freshness` that a wrapper around each stating map records; the walk itself does not read it. Key off: no
wrapper, no tree read, no `maps` key.

`[run_profile.freshness] verify_on_read = true` (default `false`, in the profile hash) adds, on top of `map_freshness`, a check of
what the answer cites against the tree by content hash. It reuses the one read of the tree (`freshness::tree_view` returns the
lag with the file sets it came from; no second scan), reads and labels, and never refreshes the index or touches
`[index].freshness`. `SeedSearch::snapshot_hashes` (`AppSeeds` reads the manifest of the index snapshot) is read after the walk,
because the walk's first search may refresh the snapshot; the snapshot lag is the source files (the `tree_lag` set; manifest prose
entries are left out) that differ between the manifest and the tree. `seeds.hybrid`, `search.lexical` and `search.vector` are
then labelled with it (`Observer` does not wrap them; the label is filled at report time), `grep.live` and `lsp.live` say
`observed: live` with `places: "index"` and the SCIP lag (their places are the index's), and `response::add_file_changed` gives
every card `file_changed` from the baseline of its maps (the SCIP baseline for the maps built from the index and the placed
maps and the symbol cards, the manifest for the snapshot maps and the chunk cards; `null` where none applies). Key off: none of this, and no change to any answer.
`search::App::verify_on_read` does the same for a search report the caller asked for: `SearchReport.freshness` (`snapshot`, `channels` for the channels that ran, `hits` with `file_changed` per result) from the manifest of the snapshot the search ran on and one read of the tree's content hashes (`chunk::source_hashes`, gated by `stat_gate` when on), read after the search (a refresh in between is not seen) and timed in `timings.verify_ms`. The searches inside the crate that discard the block (`AppSeeds::search`, the evaluation runner) call the `_unverified` entry points, so a traversal reads the tree once.
