# Spec: NodeId

Status: **v0, pre-freeze.** Written 2026-10-12T10:02Z (`date -u`) at slice S0. Docs only; no
code exists for anything below. The Map trait and everything keyed by NodeId freeze only after
the first end-to-end slice (S4).

Code read at spike head `df7faaa` (its code is the code of `7f9c636`). Line references are for
that tree; the drift table in [README.md](README.md) maps the main ones onto
`origin/fix/verified-bugs`.

Labels: **Read** = stated by the code or a doc at that tree. **Inferred** = a design choice or
consequence drawn here.

## 1. Purpose

One identity space for every node any map can return or take as a seed: file, chunk, symbol,
commit, test, config key, doc section and string literal (the owner's core idea,
[../ideas.md](../ideas.md), "Core"). Two maps that reach the same node must produce the same
NodeId, so that fusion, portals and steering can join on it without a lookup table per map
pair.

## 2. What exists today (Read)

| Item | Where | What it means for NodeId |
| --- | --- | --- |
| `Chunk { relative_file_path, language, start_line, end_line, code, content_hash }` | `lci-core/src/chunk.rs:42-50` | The only stored code unit. No id field. |
| `content_hash = hash(code)` with `code` trimmed | `lci-core/src/chunk.rs:159`, `:171`; `hash` is SHA-256 hex at `lci-core/src/workspace.rs:6-8` | Content identity of a chunk, independent of its lines. |
| Chunks in file order | `chunks_with_adapter`, `lci-core/src/chunk.rs:144-177` (spans in source order, `:155-157`) | An ordinal "n-th chunk with this hash in this file" is well defined. |
| Relative path with `/` separators | `lci-core/src/chunk.rs:222-227` | The file key. Note: on `origin/fix/verified-bugs`, commit `96e8b73` keeps backslashes in Unix file names, so the normalisation changes when that branch is merged. |
| Per-file cache key `(content_hash, language, adapter_version)` | `lci-core/src/chunk.rs:300-308`; `CachedFile` at `lci-core/src/manifest.rs:11-17` | The file's `content_hash` hashes the whole decoded source (`chunk.rs:237`), not the chunks. |
| Manifest compatibility check | `lci-core/src/manifest.rs:39-53`, `VERSION = 2` at `:9` | Any new manifest field needs a version bump; an incompatible manifest loads as empty (`:140-148`), which forces a full re-parse. |
| Fusion key `(relative_file_path, start_line, content_hash)` | `src/app/search.rs:1304-1315` | Includes `start_line`, so it moves on an edit above the chunk. |
| Fused tiebreak adds `end_line` | `fusion_order`, `src/app/search.rs:337-344` | Line-based order. |
| Vector cache key `content_hash` or `content_hash \u{1f} path` | `vector_key`, `src/store.rs:93-99` | Already content-first. |
| LSP `Location { relative_file_path, start_line, .., name, kind, language, provider }` | `lci-core/src/lsp.rs:47-58` | Position-based; resolves to a chunk by "contains `start_line`" (`src/app/search.rs:1259-1287`). |
| Workspace id = SHA-256 of the canonical path | `lci-core/src/workspace.rs:17-30` | Scopes every NodeId; never part of the NodeId string itself. |

## 3. Design (Inferred)

### 3.1 Shape

NodeId is a closed, versioned enum with a canonical string form. The canonical string is the
identity: it is what RunRecords, steering ops and map cards carry, and what sorts ties. In
memory a snapshot interns NodeIds into dense indexes (`u32`) for the graph projection
(petgraph `NodeIndex`); an index is valid for one snapshot only and is never persisted or sent
to a client.

```rust
// lci-core/src/node.rs (proposed; S1)
pub enum NodeId {
    File { path: RelPath },
    Chunk { path: RelPath, content_hash: Hash, ordinal: u32 },  // S1: u32, see section 7
    Symbol(SymbolKey),
    Commit { sha: GitSha },             // full 40-hex (or 64-hex) object id
    Test { runner: TestRunner, name: String },
    ConfigKey { namespace: ConfigNamespace, key: String },
    DocSection { path: RelPath, heading_path: Vec<Slug>, ordinal: u32 },
    StringLiteral { text_hash: Hash },
}

pub struct SymbolKey {
    pub language: String,                // LanguageAdapter::identifier(); S1: owned, parse needs it
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

Percent-encoding (upper-case `%XX` per UTF-8 byte) applies to `%` and control characters
everywhere; to `@`, `#` and `~` inside a path (a path may contain them; `/` stays as
written); to `@`, `#`, `~`, `/` and `:` inside a segment (hash, slug, container segment);
and additionally to a space inside a module segment or a symbol name. A container segment
always holds a space (`impl App`, `trait Map`) and a module segment never does, which is how
the parser tells them apart in `sym:` strings. Parsing accepts only canonical strings: a
string that parses but formats differently (`%41`, `#01`, `~0`) is refused. `content_hash`
is the full stored value in the canonical form; displays may shorten it. (S1: the v0 text
said no encoding inside a path, which made `chunk:a@b.rs@<hash>#0` ambiguous.)

### 3.2 Per-kind rules

- **File.** The repository-relative path as the scanner produces it. A rename or move is a
  new File node. Continuity across a rename is an edge (`renamed_from`, source label `git`),
  not identity. Non-code files (Markdown, TOML) get File nodes too; they are produced by
  their own maps' walks, because the code walker drops them (`chunk.rs:219-221`).
- **Chunk.** `(path, content_hash, ordinal)`, where `ordinal` is the chunk's position among
  the chunks of the same file with the same `content_hash`, in source order (usually 0). No
  `start_line`: lines are attributes. It is computable from the manifest's chunk lists with no
  new stored field, because each `CachedFile.chunks` is in source order. Lesson L4 in
  [../lessons.md](../lessons.md).
- **Symbol.** The per-declaration key from the earlier research (lesson L1): language,
  module path, container chain including the impl header, name, kind, disambiguator. Parse-time
  extraction stores the path-free part `(container, name, kind, disambiguator)` in the
  per-file bundle; the module path is joined at load from the manifest path (lesson L2:
  bundles never contain paths). For Rust the module path follows the `mod` tree from the
  crate root; for other languages it is the file path without extension until a language
  adapter defines better (Inferred; per-language rules are owned by the language adapter
  skill). A symbol binds to its defining chunk(s) through an edge (`defined_in`), resolved per
  snapshot.
- **Commit.** The full object id. Immutable by construction.
- **Test.** The runner's own test name (`cargo` test path for Rust; `pytest` node id,
  `jest` file-and-title later). A test also has a Symbol node for its function; the two are
  joined by a `test_of_symbol` edge. A test name is stable while the function keeps its name
  and module.
- **ConfigKey.** Namespace (`toml` for LCI's config keys as in `config.example.toml`, `env`
  for `LCI_*` variables, `cargo` for `Cargo.toml` keys, `repo` for other repositories'
  config files) and the dotted key. The identity is the key name, not where it is written or
  read; writer and reader sites are edges.
- **DocSection.** The document path plus the path of heading slugs from the top heading
  down, with an ordinal for duplicate slugs under one parent. Its body hash is an attribute.
- **StringLiteral.** The SHA-256 of the literal's text after a fixed normalisation: unescape
  the language's escapes, keep format placeholders (`{}`, `%s`) as written, no case folding.
  The node is global: the same message in a logger, a raise site and a test is one node with
  three occurrence edges (`occurs_in` to chunks, labelled by role: raise, wrap, log, assert;
  idea S6). Aliases (idea S12) are separate edges, never merged identities.

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
| BOM added or removed | same | same on the fix branch (`83e1164` drops one leading BOM); on the spike tree the file's first chunk gets a new id (U+FEFF is not trimmed), the others stay | same | same | same | same | same |

Invariants (each is a test, see section 6):

1. **Determinism.** The same tree, adapter versions and producers give the same NodeId set,
   whatever the scan order or the previous manifest.
2. **Incremental equals full.** The NodeIds after an incremental scan equal those of a full
   scan of the same tree (contract C4).
3. **Locality.** Editing file A changes no NodeId whose canonical form names another file,
   except global kinds by construction (a StringLiteral or ConfigKey disappears only when its
   last occurrence goes).
4. **Line independence.** No NodeId contains a line or byte offset.

### 3.4 Where it lives

- `NodeId`, `SymbolKey`, canonical parse/format and the chunk-id derivation: `lci-core`
  (proposed `lci-core/src/node.rs`), because the parse-time extraction hook in
  `chunks_with_adapter` (`chunk.rs:144`) must emit symbol keys, and `lci-core` cannot depend on
  the new `lci-maps` crate.
- The interning table (`NodeId` to dense index) belongs to a snapshot view built at search
  time; it is not persisted.
- Nothing NodeId-related changes the manifest (`VERSION = 2` stays). Symbol and edge data go
  into the path-free per-file bundle sidecar planned for S1, keyed by
  `(file content_hash, language, adapter_version, producer)`.

### 3.5 Relation to existing keys

| Existing key | Mapping |
| --- | --- |
| Fusion key `(path, start_line, content_hash)` (`search.rs:1304`) | Within one snapshot it maps to exactly one Chunk NodeId, except two chunks of the same file with the same hash and the same start line (today merged into one fusion entry, with NodeId two). The T0 replay test detects the case; see Open decisions. |
| Vector cache key (`store.rs:93`) | Unchanged. NodeId does not replace it. |
| `CandidateTrace` (`search.rs:255-325`) | Gains an optional `node_id` field later (S5); legacy fields stay so old dumps replay. |
| LSP `Location` (`lsp.rs:47`) | Resolved to the containing Chunk NodeId as today (`search.rs:1269-1274`), and to a Symbol NodeId when the extraction map knows the declaration at that range. |

## 4. Open decisions

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| N-1 | Canonical string is the identity; dense indexes are per snapshot. | A 128-bit hash of the canonical string as the identity (smaller, but opaque in logs and collisions need a policy). | S1, if RunRecords grow too large. |
| N-2 | `NodeId` lives in `lci-core`. | Put it in `lci-maps` and have the extraction hook emit plain structs that `lci-maps` converts. | S1/S2. |
| N-3 | Test is its own kind, joined to its function's Symbol by an edge. | A Test is a Symbol with a `test` role attribute (one fewer kind, but no id for non-function tests such as `pytest` parametrised cases). | S4 (test-to-code map). |
| N-4 | StringLiteral identity is global (text hash), occurrences are edges. | Per-occurrence ids (`chunk + literal ordinal`), with text-hash equality as an edge. | W2 (error-string map). |
| N-5 | Two same-hash chunks starting on the same line become two NodeIds, where fusion merges them today. | Keep the legacy merge by adding `start_line` to the fusion key of legacy channels only. Taken only if T0 shows a real dump that differs. | S2 (T0 test). |
| N-6 | File rename = new id plus a `renamed_from` edge. | A rename-stable file id (first-seen path plus git rename chain). Rejected for MVP: needs runtime git for every File node. | W3 (history maps). |
| N-7 | Path normalisation follows the scanner, whatever the merged fix branch does with backslashes. | Normalise NodeId paths separately. Rejected: two path conventions would break joins. | S1 merge of the fix branch. |
| N-8 | Module path for non-Rust languages = file path without extension. | Per-language module resolution (package for Java/Go, dotted module for Python). | When each language's symbol map lands. |
| N-9 | (S1) Rust module path = the components after the last `src` directory, without a final `mod` and without a `lib` directly under `src`, prefixed with the crate's directory when the crate is not at the repository root (`lci-core/src/chunk.rs` -> `lci-core::chunk`). Without the prefix two crates' `chunk` modules collide. A binary root `src/main.rs` keeps `main`, so its `mod tests` does not collide with the library root's. | Resolve the crate name from `Cargo.toml` (`lci_core::chunk`); follow `#[path]` attributes. | When `sym.defs` lands (S3). |

## 5. What this spec does not decide

Edge types beyond the ones named above, the bundle file format, and how a map discovers
nodes: those belong to the map that produces them and to the S1 sidecar design.

## 6. How a test proves it

All tests run offline with the fake model service (`AGENTS.md`, Hard rules). Proposed names;
none exists yet.

| Property | Test |
| --- | --- |
| Round trip | Property test: for generated NodeIds of every kind, `parse(format(id)) == id` and `format` is injective on the generated set. |
| Line independence | Fixture: a Rust file; insert ten blank lines above its third function (blank lines only: a comment or attribute is a prefix that attaches to the next declaration's chunk, `lci-core/src/language.rs:299-302`, `chunk.rs:81-85`, so it would change that chunk). Chunk NodeIds of untouched chunks are equal before and after; the legacy fusion key of those chunks differs (documents why NodeId drops `start_line`). |
| Determinism | Scan the same fixture tree twice with shuffled walk order (or from an empty and a full previous manifest); the NodeId sets are equal. |
| Incremental equals full | Replay a scripted edit sequence (edit, insert above, rename, duplicate chunk, adapter version bump) with `scan_incremental` (`chunk.rs:270`) and compare with a full scan after each step: equal NodeId sets (shares the C4 auditor). |
| Locality | After editing file A, the set difference of NodeIds names only file A, plus global kinds whose last occurrence was in A. |
| Ordinal rule | Fixture with two identical chunks in one file: ordinals 0 and 1 in source order; inserting a third identical chunk above both shifts them to 1 and 2 (documented, accepted). |
| Symbol key and impl header | Fixture with `impl A for X { fn f() }` and `impl B for X { fn f() }`: two distinct Symbol NodeIds (lesson L1); dropping the impl header from the key would make them collide, which the test asserts against a deliberately reduced key. |
| No paths in bundles | Serialise a bundle for a file, move the file, rescan: the bundle is reused unchanged (same key) and the joined NodeIds use the new path. |
| Legacy mapping | Within one snapshot, every legacy fusion key maps to exactly one Chunk NodeId, except the N-5 case, which the test enumerates and reports. |

## 7. S1 as built (2026-10-12)

Code: `lci-core/src/node.rs` (types, canonical strings, `chunk_ids`, `legacy_fusion_key`),
`lci-core/src/node/rust.rs` (a declaration-only Rust extractor for symbol keys), tests in
`lci-core/src/node/tests.rs`, and the history tool `lci-core/examples/node_stability.rs`
with its result in [../s1-stability.md](../s1-stability.md). Nothing on the search path
uses it; the manifest is unchanged (`VERSION = 2`). The only changes to existing code are
`pub mod node;` in `lci-core/src/lib.rs` and `chunk::decode_source` becoming `pub` for the
history tool. Line references re-checked on
`origin/fix/verified-bugs` at `66f999d`: `Chunk` `lci-core/src/chunk.rs:43`,
`chunks_with_adapter` `:159` (hash at `:186`), `scan_incremental` `:312` (reuse check
`:344`), fusion key `src/app/search.rs:1314`, `fusion_order` `:337`.

Deviations from sections 3 and 6, each taken as the conservative choice:

- Ordinals are `u32`, not `u16`: a generated file can hold more than 65535 equal chunks,
  and an overflow would make two chunks share an id.
- Paths and hashes are plain strings. Chunk hashes are not checked to be hex, because the
  stored value is the identity whatever it is (unit fixtures use readable hashes); commit
  ids and literal hashes are checked (40 or 64, and 64, lower-case hex).
- "Source order" for ordinals is `start_line`, then `end_line`, then the position in the
  given list (byte order for a scan or a manifest file), so `chunk_ids` gives the same ids
  for any input order of chunks except two same-hash chunks on the same lines.
- Symbol keys in S1 come from a minimal Rust extractor: functions, methods, structs,
  enums, unions, traits, consts, statics, type aliases, macros and modules (inline modules
  join the module path). Not walked: function bodies, `extern` blocks and `const _ = { .. }`
  blocks. An impl header is rebuilt from its tokens up to the body, comments dropped, with
  fixed spacing (one space between tokens, none before `,` `;` `:` `::` `>` `)` `]` `?`, none
  after `<` `::` `(` `[` `&` `*` `!` `'` `#`, none between a word and a following `<` or `(`),
  so whitespace and comments never change a key: `impl<T,U>Foo<T,U>` and
  `impl<T, U> Foo<T, U>` are both `impl<T, U> Foo<T, U>`, and a header always holds a space
  after `impl`. A token change (a trailing comma in a `where` clause) does change it. The
  per-file bundle that stores the keys is S3.
- Kinds' string names: `fn`, `method`, `struct`, `enum`, `union`, `trait`, `const`,
  `static`, `type`, `mod`, `macro`.
- Canonical form of a symbol: `sym:<lang>:<modules and containers joined by ::>::<name>/<kind>[~<n>]`,
  exactly the shape of the section 3.1 example; see the encoding paragraph there for how
  module and container segments are told apart.
- `NodeId` orders by canonical string (`Ord`), as the tie rule for non-chunk nodes needs;
  two invalid ids that share a string (an empty heading slug and none) then order by their
  debug form, so `Ord` agrees with `Eq`.

Test status per row of section 6 ("determinism" is tested by scanning from an empty and from
a full previous manifest and by shuffling the chunk list; the walk order itself cannot vary,
because the scanner sorts files by path before chunking): round trip, line independence, determinism, incremental
equals full (edit, insert above, rename, duplicate chunk, adapter version bump, delete),
locality, ordinal rule, symbol key and impl header, and legacy mapping are unit tests in
`lci-core/src/node/tests.rs`. "No paths in bundles" is covered only for its path-free half
(declarations hold no path, a moved file's keys join the new path); the bundle store and its
reuse are deferred to S3. Decision N-5 measured: no same-line twins in any of 201 snapshots of
this repository (s1-stability.md).

## 8. S3 as built (2026-10-12, slice S3a)

Code: the extraction hook `chunk::extract_with_adapter` / `extract_for_path`
(`lci-core/src/chunk.rs`, tests in `lci-core/src/chunk/extraction_tests.rs`), the bundle store
and its sync (`lci-core/src/bundle.rs`, `lci-core/src/bundle/produce.rs`, tests in
`lci-core/src/bundle/tests.rs`). Nothing on the scan or search path calls either; the
manifest is unchanged (`VERSION = 2`).

- **Hook.** One parse yields `Extraction { chunks, declarations }`. The chunks come from the
  same `ranges` walk and trimming as `chunks_with_adapter`, which is left untouched (the loop
  is repeated in the hook, so the existing function cannot change); a test keeps the two
  byte-identical on a tree with one file per registered extension and on every Rust and
  Python file of this repository. Declarations come from the S1 Rust extractor run on the
  same tree (`node::rust_declarations_in`, split out of `rust_declarations`); other languages
  give `None`. S4 can make `chunks_with_adapter` call the hook and drop the repeated loop.
- **Bundle store.** `<data_dir>/bundles/<first 16 hex of the workspace id>/<2 hex>/<30
  hex>.json`, where the 32 hex digits are the start of SHA-256 of the key
  `(content_hash, language, adapter_version, producer)` joined by NUL. The file holds
  `{format, key, payload}`; a file of another format or key (a digest collision), or one that
  does not parse, reads as missing; a file that exists but cannot be read is an error. Writes
  go to a temporary file in the same directory and are renamed into place. The relative path
  is 63 characters, so a `data_dir` under the 150-character limit stays under 260 on Windows.
  `BundleStore::retain(keys)` deletes the bundle files of one workspace that no current file
  uses, and nothing else.
- **Producers and sync.** A `Producer` names itself with a version (`sym.defs@1`), says which
  languages it applies to, and derives a payload from an `Extraction`. `bundle::sync(store,
  producer, root, files)` visits a scan's files in path order: a stored bundle is reused
  without reading the file; otherwise the file is read, must still hash to the scan's
  `content_hash` and still map to the scanned adapter and version, is extracted with the hook
  and stored. Any failure fails the whole sync, so a caller keeps its previous data
  (contract C3). Until S4 wires the hook into `scan_incremental`, a file whose bundle is
  missing is parsed twice (once by the scan, once by the sync); a file whose content is
  already bundled (unchanged, moved or copied) is not read at all.
- **Path-free payloads.** The `sym.defs` payload is `StoredDeclaration` (inline modules,
  container, name, kind name, disambiguator, lines); the module path is joined at read time
  from the file's current path (`Declaration::key`).

Section 6 "No paths in bundles": done
(`bundle::tests::sync::a_moved_file_reuses_its_bundle_unchanged_and_joins_the_new_path`):
after a move the sync reuses both bundles (0 extracted), their files are byte-identical, the
keys are equal, no bundle text contains a path fragment, and the joined symbol ids use the new
path and equal a fresh extraction's.

Open decisions added here:

| # | Decision taken here | Alternative | Revisit at |
| --- | --- | --- | --- |
| N-10 | One bundle store per workspace, so `retain` can delete unused bundles without knowing other workspaces. | One content-addressed store shared by all workspaces of a `data_dir` (a file indexed in two workspaces is extracted once; needs reference counting to delete). | When a `data_dir` commonly holds several large workspaces. |
| N-11 | A sync fails as a whole on the first file it cannot read or that changed since the scan. | Skip that file and mark the map stale for it. | When the hook runs inside the scan (S4): the race then disappears. |
| N-12 | A producer's id carries a version that is bumped by hand when its output changes, the extractor's code included. | Derive the version from a hash of the extractor's source. | First accidental reuse of a stale bundle. |
