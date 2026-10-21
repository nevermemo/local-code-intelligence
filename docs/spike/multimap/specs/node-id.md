# Spec: NodeId

Status: **v0, pre-freeze; built (S1 and S3a).** Written 2026-10-12 at slice S0, then merged with the as-built notes of S1 and S3a.
The Map trait and everything keyed by NodeId freeze only after the first end-to-end slice (S4). Labels (**Read**, **Inferred**,
**built**) are defined in [README.md](README.md).

## 1. Purpose

One identity space for every node any map can return or take as a seed: file, chunk, symbol, commit, test, config key, doc section
and string literal (the owner's core idea, [../ideas.md](../ideas.md), "Core"). Two maps that reach the same node produce the same
NodeId, so fusion, portals and steering join on it without a lookup table per map pair. Summary of the shipped result:
[../engine.md](../engine.md), "Node identity".

## 2. What exists today (Read)

Facts about the chunk and scan code that NodeId builds on, named by symbol (checked on the spike tree).

| Item | Where | What it means for NodeId |
| --- | --- | --- |
| `Chunk { relative_file_path, language, start_line, end_line, code, content_hash }` | `lci-core/src/chunk.rs` | The only stored code unit. No id field. |
| `content_hash = hash(code)` with `code` trimmed; `hash` is SHA-256 hex | `chunks_with_adapter`, `lci-core/src/workspace.rs` | Content identity of a chunk, independent of its lines. |
| Chunks in file order (spans in source order) | `chunks_with_adapter` | An ordinal "n-th chunk with this hash in this file" is well defined. |
| Relative path with `/` separators | the scanner in `chunk.rs` | The file key (N-7). |
| Per-file cache key `(content_hash, language, adapter_version)`; the file's `content_hash` hashes the whole decoded source | `scan_incremental`, `CachedFile` in `lci-core/src/manifest.rs` | Not the chunk hash. |
| Manifest compatibility check, `VERSION = 2` | `lci-core/src/manifest.rs` | A new manifest field needs a version bump; an incompatible manifest loads as empty and forces a full re-parse. |
| Fusion key `(relative_file_path, start_line, content_hash)` | `fuse_hits` in `src/app/search.rs` | Includes `start_line`, so it moves on an edit above the chunk. |
| Fused tiebreak adds `end_line` | `fusion_order` in `src/app/search.rs` | Line-based order. |
| Vector cache key `content_hash` or `content_hash \u{1f} path` | `vector_key` in `src/store.rs` | Already content-first. |
| LSP `Location { relative_file_path, start_line, .., name, kind, language, provider }` | `lci-core/src/lsp.rs` | Position-based; resolves to a chunk by "contains `start_line`". |
| Workspace id = SHA-256 of the canonical path | `lci-core/src/workspace.rs` | Scopes every NodeId; never part of the NodeId string. |

A leading BOM is dropped before chunking, so a BOM change does not change chunk ids.

## 3. Design (Inferred; built in `lci-core/src/node.rs`)

### 3.1 Shape

NodeId is a closed, versioned enum with a canonical string form. The canonical string is the identity: it is what RunRecords,
steering ops and map cards carry, and what sorts ties (`NodeId` orders by canonical string; two invalid ids that share a string
order by their debug form, so `Ord` agrees with `Eq`). In memory a snapshot interns NodeIds into dense indexes (`u32`) for the
graph projection (petgraph `NodeIndex`); an index is valid for one snapshot only and is never persisted or sent to a client.

```rust
// lci-core/src/node.rs
pub enum NodeId {
    File { path: RelPath },
    Chunk { path: RelPath, content_hash: Hash, ordinal: u32 },  // u32: a generated file can hold more than 65535 equal chunks
    Symbol(SymbolKey),
    Commit { sha: GitSha },             // full 40-hex (or 64-hex) object id
    Test { runner: TestRunner, name: String },
    ConfigKey { namespace: ConfigNamespace, key: String },
    DocSection { path: RelPath, heading_path: Vec<Slug>, ordinal: u32 },
    StringLiteral { text_hash: Hash },
}

pub struct SymbolKey {
    pub language: String,                // LanguageAdapter::identifier(); owned, parse needs it
    pub module_path: Vec<String>,        // derived from the file path at load, never stored in a bundle
    pub container: Vec<ContainerSegment>,// e.g. impl header `impl Map for LexicalMap`, trait, class
    pub name: String,
    pub kind: SymbolKind,                // fn, method, struct, enum, trait, const, field, ...
    pub disambiguator: u16,              // 0 unless the key collides inside one file
}
```

Canonical strings (all scoped by the workspace id, which is not part of the string):

| Kind | Canonical form | Example |
| --- | --- | --- |
| File | `file:<path>` | `file:src/app/search.rs` |
| Chunk | `chunk:<path>@<content_hash>#<ordinal>` | `chunk:src/app/search.rs@9f2c...#0` |
| Symbol | `sym:<lang>:<module::path>::<container>::<name>/<kind>[~<n>]` | `sym:rust:app::search::impl App::fuse_hits/method` |
| Commit | `commit:<sha>` | `commit:7f9c636c...` (full id; short ids only for display) |
| Test | `test:<runner>:<name>` | `test:cargo:app::search::tests::unit_weights_match_plain_rrf` |
| ConfigKey | `cfg:<namespace>:<key>` | `cfg:toml:fusion.max_chunks_per_file`, `cfg:env:LCI_CANDIDATE_DUMP` |
| DocSection | `doc:<path>#<slug>/<slug>[~<n>]` | `doc:AGENTS.md#hard-rules` |
| StringLiteral | `str:<text_hash>` | `str:41ab...` (SHA-256 of the literal's normalised text) |

Symbol kinds' string names: `fn`, `method`, `struct`, `enum`, `union`, `trait`, `const`, `static`, `type`, `mod`, `macro`, `field`, `variable`, `interface`, `class`. The first eleven come from the Rust extractor and the SCIP index; the last four only from language-server outlines (`lsp.live` under `places = "outline"`).

Percent-encoding (upper-case `%XX` per UTF-8 byte) applies to `%` and control characters everywhere; to `@`, `#` and `~` inside a
path (a path may contain them; `/` stays as written); to `@`, `#`, `~`, `/` and `:` inside a segment (hash, slug, container
segment); and additionally to a space inside a module segment or a symbol name. A container segment always holds a space (`impl
App`, `trait Map`) and a module segment never does, which is how the parser tells them apart in `sym:` strings. Parsing accepts
only canonical strings: a string that parses but formats differently (`%41`, `#01`, `~0`) is refused. `content_hash` is the full
stored value in the canonical form; displays may shorten it. Paths and chunk hashes are plain strings and chunk hashes are not
checked to be hex, because the stored value is the identity whatever it is; commit ids and literal hashes are checked (40 or 64,
and 64, lower-case hex). (The v0 text said no encoding inside a path, which made `chunk:a@b.rs@<hash>#0` ambiguous.)

### 3.2 Per-kind rules

- **File.** The repository-relative path as the scanner produces it. A rename or move is a new File node. Continuity across a
  rename is an edge (`renamed_from`, source label `git`), not identity. Non-code files (Markdown, TOML) get File nodes too; their
  own maps' walks produce them, because the code walker drops them.
- **Chunk.** `(path, content_hash, ordinal)`, where `ordinal` is the chunk's position among the chunks of the same file with the
  same `content_hash`, in source order (usually 0). Source order is `start_line`, then `end_line`, then the position in the given
  list, so `chunk_ids` gives the same ids for any input order of chunks except two same-hash chunks on the same lines. No
  `start_line`: lines are attributes. It is computable from the manifest's chunk lists with no new stored field (`chunk_ids`,
  `manifest_chunk_ids`). Lesson L4 in [../lessons.md](../lessons.md).
- **Symbol.** The per-declaration key from the earlier research (lesson L1): language, module path, container chain including the
  impl header, name, kind, disambiguator. Parse-time extraction stores the path-free part `(container, name, kind, disambiguator)`
  in the per-file bundle; the module path is joined at load from the manifest path (lesson L2: bundles never contain paths). For
  Rust the module path follows the `mod` tree from the crate root (N-9, N-13 to N-15); for other languages it is the file path
  without extension until a language adapter defines better (N-8). A symbol binds to its defining chunk(s) through an edge
  (`defined_in`), resolved per snapshot. Built extractor (`lci-core/src/node/rust.rs`, declaration-only): functions, methods,
  structs, enums, unions, traits, consts, statics, type aliases, macros and modules (inline modules join the module path). Not
  walked: function bodies, `extern` blocks and `const _ = { .. }` blocks. An impl header is rebuilt from its tokens up to the
  body, comments dropped, with fixed spacing (one space between tokens, none before `,` `;` `:` `::` `>` `)` `]` `?`, none after
  `<` `::` `(` `[` `&` `*` `!` `'` `#`, none between a word and a following `<` or `(`), so whitespace and comments never change a
  key: `impl<T,U>Foo<T,U>` and `impl<T, U> Foo<T, U>` are both `impl<T, U> Foo<T, U>`, and a header always holds a space after
  `impl`. A token change (a trailing comma in a `where` clause) does change it.
- **Commit.** The full object id. Immutable by construction.
- **Test.** The runner's own test name (`cargo` test path for Rust; `pytest` node id, `jest` file-and-title later). A test also
  has a Symbol node for its function; the two are joined by a `test_of_symbol` edge. A test name is stable while the function
  keeps its name and module.
- **ConfigKey.** Namespace (`toml` for LCI's config keys as in `config.example.toml`, `env` for `LCI_*` variables, `cargo` for
  `Cargo.toml` keys, `repo` for other repositories' config files) and the dotted key. The identity is the key name, not where it
  is written or read; writer and reader sites are edges.
- **DocSection.** The document path plus the path of heading slugs from the top heading down, with an ordinal for duplicate slugs
  under one parent. Its body hash is an attribute.
- **StringLiteral.** The SHA-256 of the literal's text after a fixed normalisation: unescape the language's escapes, keep format
  placeholders (`{}`, `%s`) as written, no case folding. The node is global: the same message in a logger, a raise site and a test
  is one node with three occurrence edges (`occurs_in` to chunks, labelled by role: raise, wrap, log, assert; idea S6). Aliases
  (idea S12) are separate edges, never merged identities.

### 3.3 Stability across edits

What happens to an existing NodeId when the tree changes. "Same" = the id is unchanged.

| Change | File | Chunk | Symbol | DocSection | StringLiteral | ConfigKey | Test |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Lines inserted above, chunk text unchanged | same | **same** (today's fusion key changes) | same | same | same | same | same |
| Edit inside a chunk | same | new id (new hash) | same (body edits keep the key) | n/a | new id if the literal changed | same | same |
| Whitespace-only edit inside a chunk | same | new id (the hash covers interior whitespace; only the ends are trimmed) | same | same if only the body changed | same | same | same |
| File renamed or moved | new id | new id | same only if the module path is unchanged (Rust `foo.rs` to `foo/mod.rs`); otherwise new id | new id | same | same | same only if the module path is unchanged |
| Symbol renamed | same | new id | new id (`renamed_from` edge when a map can prove it) | n/a | same | n/a | new id |
| Duplicate chunk text inserted earlier in the same file | same | the later duplicates' ordinals shift: new ids | same | n/a | same | n/a | same |
| Adapter `cache_version` bump | same | new ids where boundaries change | same | n/a | same | n/a | same |
| Heading text edited | same | n/a | n/a | new id (body edits keep it) | n/a | n/a | n/a |
| BOM added or removed | same | same (a leading BOM is dropped before chunking) | same | same | same | same | same |

Invariants (each is a test, see section 6):

1. **Determinism.** The same tree, adapter versions and producers give the same NodeId set, whatever the scan order or the
   previous manifest.
2. **Incremental equals full.** The NodeIds after an incremental scan equal those of a full scan of the same tree (contract C4).
3. **Locality.** Editing file A changes no NodeId whose canonical form names another file, except global kinds by construction (a
   StringLiteral or ConfigKey disappears only when its last occurrence goes).
4. **Line independence.** No NodeId contains a line or byte offset.

### 3.4 Where it lives

- `NodeId`, `SymbolKey`, canonical parse/format, `chunk_ids` and `legacy_fusion_key`: `lci-core/src/node.rs` (with
  `lci-core/src/node/rust.rs` and tests in `lci-core/src/node/tests.rs`), because the parse-time extraction hook in `chunk.rs`
  must emit symbol keys, and `lci-core` cannot depend on `lci-maps`.
- The interning table (`NodeId` to dense index) belongs to a snapshot view built at search time (`SnapshotView` in `lci-maps`); it
  is not persisted.
- Nothing NodeId-related changes the manifest (`VERSION = 2` stays). Symbol data goes into the path-free per-file bundle sidecar
  (3.6), keyed by `(file content_hash, language, adapter_version, producer)`.
- Nothing on the search path outside `lci-maps` uses it. The only changes to existing `lci-core` code are `pub mod node;` in
  `lci-core/src/lib.rs` and `chunk::decode_source` being `pub` for the history tool `lci-core/examples/node_stability.rs` (result:
  [../s1-stability.md](../s1-stability.md)).

### 3.5 Relation to existing keys

| Existing key | Mapping |
| --- | --- |
| Fusion key `(path, start_line, content_hash)` (`fuse_hits`) | Within one snapshot it maps to exactly one Chunk NodeId, except two chunks of the same file with the same hash and the same start line (today merged into one fusion entry, with NodeId two). The T0 replay test detects the case (N-5). |
| Vector cache key (`vector_key`) | Unchanged. NodeId does not replace it. |
| `CandidateTrace` (`src/app/search.rs`) | Gains an optional `node_id` field later (S5); legacy fields stay so old dumps replay. |
| LSP `Location` | Resolved to the containing Chunk NodeId as today, and to a Symbol NodeId when the extraction map knows the declaration at that range. |

### 3.6 Extraction hook and bundle store (built, S3a)

- **Hook.** `chunk::extract_with_adapter` / `extract_for_path` (`lci-core/src/chunk.rs`, tests in
  `lci-core/src/chunk/extraction_tests.rs`): one parse yields `Extraction { chunks, declarations }`. `chunks_with_adapter` and
  `extract_with_adapter` are calls of one private `extract` (one `hash(code)`, one `ranges` walk), so the chunk output of a plain
  scan cannot differ. A golden record of the every-language tree's 44 chunks
  (`lci-core/src/chunk/golden/every_language_chunks.json`, test `chunks_of_a_tree_of_every_language_match_the_record`) and a
  whole-repository test keep chunks, declarations and uncovered bytes checked. Declarations come from
  `node::rust_declarations_in`; other languages give `None`. The declaration walk stays opt-in (`extract_with_adapter` only), so
  `chunks_for_path` pays nothing new.
- **Bundle store** (`lci-core/src/bundle.rs`, `lci-core/src/bundle/produce.rs`, tests in `lci-core/src/bundle/tests.rs`).
  `<data_dir>/bundles/<first 16 hex of the workspace id>/<2 hex>/<30 hex>.json`, where the 32 hex digits are the start of SHA-256
  of the key `(content_hash, language, adapter_version, producer)` joined by NUL. The file holds `{format, key, payload}`; a file
  of another format or key (a digest collision), or one that does not parse, reads as missing; a file that exists but cannot be
  read is an error. Writes go to a temporary file in the same directory and are renamed into place. The relative path is 63
  characters (a temporary file's name is longer, up to about 70 characters), so a `data_dir` under the 150-character limit stays
  under 260 on Windows. When the rename fails but a bundle of the same key is in place (another writer, or Windows refusing to
  replace a file held open), that bundle is kept: a bundle is a function of its key. `BundleStore::retain(keys)` deletes the
  bundle files of one workspace whose key is not in `keys`, and temporary files older than an hour (a killed writer); nothing
  else. A file name does not say which producer wrote it, so `keys` must be the union over every producer that uses the store
  (today only `sym.defs@1`).
- **Producers and sync.** A `Producer` names itself with a version (`sym.defs@1`), says which languages it applies to, and derives
  a payload from an `Extraction`. `bundle::sync(store, producer, root, files)` visits a scan's files in path order: a stored
  bundle is reused without reading the file; otherwise the file is read, must still hash to the scan's `content_hash` and still
  map to the scanned adapter and version, is extracted with the hook and stored. Any failure fails the whole sync, so a caller
  keeps its previous data (contract C3). Nothing wires the hook into `scan_incremental` yet (S4), so a file whose bundle is
  missing is parsed twice (once by the scan, once by the sync); a file whose content is already bundled (unchanged, moved or
  copied) is not read at all.
- **Path-free payloads.** The `sym.defs` payload is `StoredDeclaration` (inline modules, container, name, kind name,
  disambiguator, lines); the module path is joined at read time from the file's current path (`Declaration::key`).
- **Crate names (N-9 settled).** See the decision rows below. Names are read from the tree when bundles are joined
  (`lci_maps::crates::CrateNames`), never stored in a bundle. `Declaration::key` and `rust_module_path` keep the directory form;
  the crate name is applied by `sym.defs` at join time. S1's stability measurement ([../s1-stability.md](../s1-stability.md)) used
  the directory form; renaming a crate's package now changes its symbol ids, renaming its directory alone does not (the alias
  still resolves only for the current directory).

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| N-1 | Canonical string is the identity; dense indexes are per snapshot. | A 128-bit hash of the canonical string as the identity (smaller, but opaque in logs and collisions need a policy). | S1, if RunRecords grow too large. |
| N-2 | `NodeId` lives in `lci-core`. | Put it in `lci-maps` and have the extraction hook emit plain structs that `lci-maps` converts. | S1/S2. |
| N-3 | Test is its own kind, joined to its function's Symbol by an edge. | A Test is a Symbol with a `test` role attribute (one fewer kind, but no id for non-function tests such as `pytest` parametrised cases). | S4 (test-to-code map). |
| N-4 | StringLiteral identity is global (text hash), occurrences are edges. | Per-occurrence ids (`chunk + literal ordinal`), with text-hash equality as an edge. | W2 (error-string map). |
| N-5 | Two same-hash chunks starting on the same line become two NodeIds, where fusion merges them today. Measured: no same-line twins in any of 201 snapshots of this repository ([../s1-stability.md](../s1-stability.md)). | Keep the legacy merge by adding `start_line` to the fusion key of legacy channels only. Taken only if T0 shows a real dump that differs. | S2 (T0 test). |
| N-6 | File rename = new id plus a `renamed_from` edge. | A rename-stable file id (first-seen path plus git rename chain). Rejected for MVP: needs runtime git for every File node. | W3 (history maps). |
| N-7 | Path normalisation follows the scanner. | Normalise NodeId paths separately. Rejected: two path conventions would break joins. | When the scanner's path rules change. |
| N-8 | Module path for non-Rust languages = file path without extension. | Per-language module resolution (package for Java/Go, dotted module for Python). | When each language's symbol map lands. |
| N-9 | (S1) Rust module path = the components after the last `src` directory, without a final `mod` and without a `lib` directly under `src`, prefixed with the crate's directory when the crate is not at the repository root (`lci-core/src/chunk.rs` -> `lci-core::chunk`). Without the prefix two crates' `chunk` modules collide. A binary root `src/main.rs` keeps `main`, so its `mod tests` does not collide with the library root's. **Settled 2026-10-12 (owner: "crate name fusion"):** the canonical module path of a file in a crate that is not at the repository root uses the Cargo crate name, `[package].name` from `<crate directory>/Cargo.toml` with `-` turned into `_` (`lci-core/src/chunk.rs` -> `lci_core::chunk`). The directory form (`lci-core::chunk`) stays a valid alias: `SymIndex::resolve` maps it to the canonical id and `SymIndex::lookup` finds the same declarations for either form, so the two never become two nodes. There is no edge store yet, so the alias is a resolution rule, not a stored edge. | Follow `#[path]` attributes. | Not scheduled (only `#[path]` remains open). |
| N-10 | One bundle store per workspace, so `retain` can delete unused bundles without knowing other workspaces. | One content-addressed store shared by all workspaces of a `data_dir` (a file indexed in two workspaces is extracted once; needs reference counting to delete). | When a `data_dir` commonly holds several large workspaces. |
| N-11 | A sync fails as a whole on the first file it cannot read or that changed since the scan. | Skip that file and mark the map stale for it. | When the hook runs inside the scan (S4): the race then disappears. |
| N-12 | A producer's id carries a version that is bumped by hand when its output changes, the extractor's code included. | Derive the version from a hash of the extractor's source. | First accidental reuse of a stale bundle. |
| N-13 | The root crate's files keep their S1 path without a crate-name prefix (`src/app/search.rs` -> `app::search`), as `crate::app::search` is written inside the crate; only the directory prefix of a non-root crate is replaced. | Prefix every file with its crate name (`local_code_intelligence::app::search`), which changes every root-crate symbol id. | S4 trait freeze. |
| N-14 | A crate directory without a readable `[package].name` (no manifest, `name.workspace = true`) keeps the directory form. | Fall back to the directory name with `-` turned into `_`. | First workspace with inherited names. |
| N-15 | A crate name that another crate also uses, or that is itself some crate's directory, is not applied: those crates keep the directory form, so two crates never share a first segment and resolving an id twice gives the same id. | A discriminator suffix on colliding names. | First workspace with such a collision. |

## 5. What this spec does not decide

Edge types beyond the ones named above, the bundle file format beyond 3.6, and how a map discovers nodes: those belong to the map
that produces them.

## 6. How a test proves it

All tests run offline with the fake model service (`AGENTS.md`, Hard rules). Built in `lci-core/src/node/tests.rs` unless noted.
"Determinism" is tested by scanning from an empty and from a full previous manifest and by shuffling the chunk list; the walk
order itself cannot vary, because the scanner sorts files by path before chunking.

| Property | Test |
| --- | --- |
| Round trip | Property test: for generated NodeIds of every kind, `parse(format(id)) == id` and `format` is injective on the generated set. |
| Line independence | Fixture: a Rust file; insert ten blank lines above its third function (blank lines only: a comment or attribute is a prefix that attaches to the next declaration's chunk, so it would change that chunk). Chunk NodeIds of untouched chunks are equal before and after; the legacy fusion key of those chunks differs (documents why NodeId drops `start_line`). |
| Determinism | Scan the same fixture tree twice with shuffled walk order (or from an empty and a full previous manifest); the NodeId sets are equal. |
| Incremental equals full | Replay a scripted edit sequence (edit, insert above, rename, duplicate chunk, adapter version bump, delete) with `scan_incremental` and compare with a full scan after each step: equal NodeId sets (shares the C4 auditor). |
| Locality | After editing file A, the set difference of NodeIds names only file A, plus global kinds whose last occurrence was in A. |
| Ordinal rule | Fixture with two identical chunks in one file: ordinals 0 and 1 in source order; inserting a third identical chunk above both shifts them to 1 and 2 (documented, accepted). |
| Symbol key and impl header | Fixture with `impl A for X { fn f() }` and `impl B for X { fn f() }`: two distinct Symbol NodeIds (lesson L1); dropping the impl header from the key would make them collide, which the test asserts against a deliberately reduced key. |
| No paths in bundles | Serialise a bundle for a file, move the file, rescan: the bundle is reused unchanged (same key) and the joined NodeIds use the new path. Built in `bundle::tests::sync::a_moved_file_reuses_its_bundle_unchanged_and_joins_the_new_path`: after a move the sync reuses both bundles (0 extracted), their files are byte-identical, the keys are equal, no bundle text contains a path fragment, and the joined symbol ids use the new path and equal a fresh extraction's. |
| Legacy mapping | Within one snapshot, every legacy fusion key maps to exactly one Chunk NodeId, except the N-5 case, which the test enumerates and reports. |
