# Language scope

What LCI has for each language it touches: language server, syntax tier, SCIP and gold. State of the code at the
time of writing; the extension and identifier list is in [README.md](../README.md#supported-languages), the
per-server limits are in the same section, and plans are in [the roadmap](development/roadmap.md). A cell that says
"not verified" was not checked against code or docs when this table was written.

| Language (extensions) | Language server | Syntax tier | SCIP | Gold | Notes |
| --- | --- | --- | --- | --- | --- |
| Rust (`.rs`) | `rust-analyzer`, `lsp/rust.rs`, shipped. Always enabled; the executable may be missing | `tree-sitter-rust` 0.24. Chunks `function_item`, `struct_item`, `enum_item`, `macro_definition`; containers `impl`, `mod`, `trait`; leading comments and attributes attach | `rust-analyzer scip` works (tauri, bevy, deno). It writes no relationships, so impl edges come from `impl Trait for Type` headers | Most sets: the LCI tree (the git-mined query set, impact, flow, where-to, history, bug localisation, seed questions); trait-implementation set on bevy, deno, tauri; tantivy items in where-to. Most gold files are private | CI never runs a real rust-analyzer (fake LSP only); real runs are local live acceptance. LCI starts it with build scripts, proc macros and check on save off |
| TypeScript and JavaScript (`.ts`, `.tsx`, `.js`, `.jsx`) | One shared `typescript-language-server` (wraps tsserver), `lsp/typescript.rs`, shipped. `typescript-go` adapter planned (K12b) | `tree-sitter-typescript` 0.23 (`.ts`, `.tsx`), `tree-sitter-javascript` 0.23 (`.js`, `.jsx`). Chunks functions, classes, interfaces, methods, type aliases, enums, `export` statements, variable declarations and arrow functions | None verified. `scip-typescript` is listed in prior art only | None | Fresh server has no project context until a file is opened; the workspace needs its own `typescript`. A `.ts` Qt Linguist catalog is skipped as non-source |
| Python (`.py`) | `pyright` via `pyright-langserver`, `lsp/python.rs`, shipped | `tree-sitter-python` 0.23. Chunks functions, classes, decorated definitions; `module` and `block` are containers | None verified. `scip-python` is listed in prior art only | None | Same no-project-context limit as TypeScript. Needs Node for pyright |
| C# (`.cs`) | Standalone `csharp-ls`, `lsp/csharp.rs`, shipped | `tree-sitter-c-sharp` 0.23.5. Chunks namespaces, types, methods, constructors, properties, fields and events | None verified. `scip-dotnet` is listed in prior art only | None | Acceptance needs the `dotnet` SDK |
| Go (`.go`) | `gopls`, `lsp/go.rs`, shipped | `tree-sitter-go` 0.25. Chunks top-level functions, methods, types, consts and vars; `source_file` is the only container | None verified. `scip-go` is listed in prior art only | None | Needs a `go.mod` at or above the workspace root; without one `gopls` is much weaker |
| Java (`.java`) | `jdtls`, `lsp/java.rs`, shipped. `[java].path` is the install directory; LCI spawns `java` itself | `tree-sitter-java` 0.23.5. Chunks types, methods, constructors, fields and static initializers | None verified. `scip-java` is listed in prior art only | None | Needs JDK 21+. Windows: the spawn avoids jdtls's Python launcher, which hung because the `java` grandchild survived a kill |
| C (`.c`, `.h`) | One shared `clangd`, `lsp/clangd.rs`, shipped | `tree-sitter-c` 0.24.2. Chunks functions, declarations, type definitions, struct, union and enum specifiers, preprocessor definitions | None verified. `scip-clang` is listed in prior art only | None | `.h` is always `c`, even in C++ projects. `compile_commands.json` is optional (`[clangd].args`) |
| C++ (`.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`) | The same shared `clangd` | `tree-sitter-cpp` 0.23.4. Chunks namespaces, classes, structs, unions, functions, declarations, type definitions, templates, enums, preprocessor definitions | None verified | None | A `.h` header in a C++ project is chunked with the C grammar |
| Markdown (`.md`) | None. `marksman` adapter planned (K12c, N95) | Hand-written heading splitter, no grammar. Off by default (`[index].prose = false`) | Not applicable | Instructions set (document sections). The docs-for-code set (548 items) exists; whether its documents are Markdown is not verified | The language bar's server item is waived for prose until marksman lands |

Every code language also has a chunk golden (`every_language_chunks.json`) and a fixture in the `multilingual`
acceptance. Those check indexing, not retrieval questions, so they are not counted as gold above. Live acceptance
has `*-lsp`, `*-missing` and `*-recovery` steps for Python, TypeScript, Go, Java, C#, C and C++, and only an `lsp` step
for Rust; each skips when its tool is missing ([local live acceptance](development/local-live-acceptance.md)).

Gold column: gold files are private and outside the repository except the seed questions (`eval-seed.json`) and the
bug-localisation set (`gold-bugloc.json`), both under `docs/spike/multimap`; the sets are listed in
[evaluation.md](spike/multimap/evaluation.md#sets). No set exists for C#, Go, Java, C, C++, Python or
TypeScript and JavaScript code questions. The 450-key structured-grep spike sample (LCI, Vite, Home Assistant) measured live grep on config keys and is not retrieval gold. The languages of the 15 public repositories planned for the agent A/B
pilot are not verified. External held-out gold (SWE-PolyBench Verified, LongCodeArena) is planned (K4a) and not built.

Syntax tier plans: tree-sitter 0.26 (K15a; today the runtime is 0.25 and the grammar crates are 0.23 to 0.25), Codanna parsing and resolution behind a Cargo
feature (K14a to K14d), structural grep over `ast-grep-core` (K19: built as the opt-in map `grep.structural`, pinned to `=0.40.1` because it needs tree-sitter 0.25; ceiling in test-results).
