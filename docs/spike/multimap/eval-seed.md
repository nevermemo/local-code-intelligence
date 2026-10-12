# Multimap traversal evaluation seed set

- Generated: `2026-10-12T04:22Z` (UTC)
- Revised: `2026-10-12T06:51Z` (UTC): fact-check corrections to q07, q08, q11, q13, q14, q15, q22 and q23; baseline expectations and the Baseline section added
- Base commit: `7f9c636` (`origin/fix/verified-bugs`, 188 commits of history). Answers, co-change counts, recency and commit answers were verified there; the spike branch adds only docs under `docs/spike/multimap/`, which no question references by line and which co-change counts exclude. History counts by revision: 188 at `7f9c636`, 191 at spike commit `842a5e9` (the count first written into q08), 192 at spike commit `b5e5851`. A shallow clone gives wrong counts; use a full clone.
- Machine-readable twin: `eval-seed.json` (same ids, answers, and verification)
- Questions: 28 (easy 4, medium 16, hard 8); `needs_model_service` is false for all

## Purpose

Seed questions for the modular multimap traversal spike. Each question is about this repository, is multi-hop, and needs at least two different kinds of map (lexical, LSP symbols/references, call graph, module graph, language map, agent instruction files, git history, test links, config keys, error/log strings, entrypoints, build graph, trait/impl hierarchy, doc-to-code). A plain grep of the question text should not be enough. Every answer location was checked against the base commit by reading the file, running a grep, or running git log; the commands are listed per question.

## Schema

Top level: `schema_version`, `generated`, `base_commit`, `repository` (`self`), `questions[]`. Each question has `id`, `question`, `maps` (map kinds needed), `hops` (expected traversal steps), `answer[]` (`path`, `symbol`, `lines`, `role`), `verification[]` (`method`: `grep` | `read` | `git_log`, `detail`), `needs_model_service`, `difficulty`, `baseline_expectation` (`guess`: `easy` | `needs-reading` | `needs-structure`, plus `basis`; a guess, see Baseline), and an optional `notes` string. The top level also has `revised`, `history_counts` and `baseline_expectation_note`.

Conventions:

- `path` is repo-relative. For a commit answer, `path` holds the short sha and `lines` is empty.
- `lines` is one-based and inclusive. It usually spans the whole item (function, impl, test). The specific line inside it is given in `role`, for example `caller (line 195 ...)`.
- Co-change answers leave `symbol` and `lines` empty and put the count in `role` as `k of n commits`. They are counted without `--follow`.
- `verification` commands use short forms (`rg` for ripgrep). The co-change command is `git log --format=%h -- <file> | xargs -n1 git show --format= --name-only | sort | uniq -c | sort -rn`.

## Baseline

The questions have real cross-file chains, but a lexical search plus reading files can recover most of them: a distinctive string from the question (an error text, a config key, a hook name) usually lands on the first hop, and reading the hit and its neighbours does the rest. The `maps` labels say which kinds of map *could* serve each hop. They do not show that graph traversal is *needed*, and they were written by the same author who picked the questions.

So the spike's evaluation must include a lexical-plus-reading baseline: an agent with ripgrep and file reads only (and, as a separate arm, plain `git` CLI for the history questions). For every question and every arm, record:

- hops: search or navigation calls made;
- files read (count and paths);
- tokens read and spent;
- whether the answer matched (per answer row, using the Line drift rules).

"Multimap helps" is then a measured difference against that baseline, not an assumption.

Each question carries a `baseline_expectation` guess. It is the author's guess, not a measurement:

- `easy`: one grep plus reading one or two files answers it;
- `needs-reading`: lexical search finds the entry point, but the answer needs following several references by reading;
- `needs-structure`: the working tree alone cannot answer it (git history counts, recency or commits). With plain `git` CLI allowed, most of these probably drop to `needs-reading`.

Current guesses: easy 1, needs-reading 13, needs-structure 14. Every `needs-structure` guess is there because of git history, none because of a code graph. That is the main reason to measure before building graph maps.

## Line drift

Line numbers are only valid at the base commit. When grading on a later commit, treat the symbol as the primary key: resolve it (LSP `search_symbols`/`find_definition`, or a grep for its `fn`/`const`/`impl` line) and accept an answer that points at the same symbol even if the lines moved. Lines are a secondary check, and a tie-breaker when a symbol name occurs more than once. Commit answers are matched by sha. Co-change counts can only grow as history grows, so on a later commit re-run the recorded command and compare rankings, not exact numbers. If a symbol has been renamed or removed, re-verify the question by hand before using it.

## Map-kind coverage

| Map kind | Count | Questions |
| --- | ---: | --- |
| `call_graph` | 22 | q01, q02, q03, q04, q05, q06, q07, q09, q10, q12, q14, q15, q16, q18, q19, q20, q21, q22, q23, q24, q25, q27 |
| `test_link` | 21 | q01, q02, q03, q04, q06, q07, q08, q11, q12, q13, q14, q16, q18, q19, q20, q21, q22, q23, q25, q27, q28 |
| `config_key` | 11 | q03, q04, q10, q11, q12, q15, q17, q18, q20, q22, q23 |
| `git_commit_message` | 9 | q04, q07, q11, q12, q16, q19, q20, q21, q25 |
| `error_string` | 7 | q03, q12, q16, q18, q21, q24, q25 |
| `agent_instructions` | 5 | q02, q14, q17, q18, q28 |
| `doc_to_code` | 5 | q02, q15, q16, q23, q27 |
| `entrypoint_xtask` | 5 | q13, q14, q15, q16, q17 |
| `module_graph` | 5 | q08, q13, q15, q24, q28 |
| `trait_impl` | 5 | q04, q05, q06, q17, q26 |
| `build_graph` | 4 | q02, q13, q24, q28 |
| `entrypoint_mcp` | 4 | q01, q20, q21, q22 |
| `language_map` | 4 | q07, q09, q17, q25 |
| `lsp_symbol` | 4 | q05, q08, q17, q21 |
| `entrypoint_cli` | 3 | q03, q11, q19 |
| `git_cochange` | 3 | q08, q09, q10 |
| `entrypoint_rest` | 2 | q01, q02 |
| `git_blame` | 1 | q27 |
| `git_recency` | 1 | q26 |
| `lsp_reference` | 1 | q06 |

Questions needing git history (14): q04, q07, q08, q09, q10, q11, q12, q16, q19, q20, q21, q25, q26, q27.

## Questions

### q01 (medium)

One REST endpoint served by LCI has no MCP tool counterpart. Which endpoint is it, which App method does its handler call, which lci-core registry method actually stops the language servers, and which integration tests exercise it over HTTP and directly?

Maps: `entrypoint_rest`, `entrypoint_mcp`, `call_graph`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: grep the route and #[tool] lists, diff them by reading two files, then follow two calls.

Hops:

1. List the REST routes in the router and the #[tool] methods of the MCP server; diff the two sets.
2. Follow the REST-only handler to the App method it calls.
3. Follow the App method into the lci-core LSP Manager.
4. Find tests that call the endpoint path or the App method.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `src/rest.rs` | `release_workspace` | 70-79 | rest_handler |
| `src/rest.rs` | `routes` | 262-279 | route_registration |
| `src/server.rs` | `impl McpServer (#[tool_router])` | 89-200 | mcp_tool_list_without_counterpart |
| `src/app/navigation.rs` | `App::release_language_servers` | 54-67 | app_method |
| `lci-core/src/lsp.rs` | `Manager::release` | 656-667 | core_function |
| `tests/cases/mcp.rs` | `rest_release_workspace_stops_the_language_servers_of_that_workspace` | 233-368 | test |
| `tests/cases/navigation.rs` | `analyzers_running_has_an_entry_per_language_server_and_follows_the_session` | 2214-2299 | test |

Verification:

- grep: `rg -n 'route\(' src/rest.rs (11 /v1 routes incl. /v1/release_workspace); rg -n '#\[tool' src/server.rs (10 tools, no release)`
- read: `src/rest.rs 70-79 (doc comment: REST only, no MCP tool); src/app/navigation.rs 54-67; lci-core/src/lsp.rs 656-667`
- grep: `rg -n 'release_workspace|release_language_servers' tests (mcp.rs 233-368, navigation.rs 2281/2292)`

### q02 (hard)

AGENTS.md tells Windows users to keep data_dir below a length limit and says /ready degrades above it. Where is the numeric limit defined, which function builds the readiness component, where is it wired into the readiness report with a compile-time platform check, which HTTP route exposes the report, and which tests pin the rule?

Maps: `agent_instructions`, `doc_to_code`, `build_graph`, `call_graph`, `entrypoint_rest`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: the AGENTS.md wording gives grep terms; the chain is followed by reading call sites.

Hops:

1. Read the AGENTS.md Working method bullet about data_dir length.
2. Find the constant holding the limit and the component builder in the readiness module.
3. Find the call inside App::service_status that passes cfg!(windows).
4. Find the /ready route in the server router that serialises service_status.
5. Find unit and integration tests for the component.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `AGENTS.md` | `Working method: Running LCI locally (data_dir length)` | 116-119 | doc_statement |
| `src/app/readiness.rs` | `WINDOWS_DATA_DIR_LENGTH_LIMIT` | 69-69 | constant |
| `src/app/readiness.rs` | `data_dir_length_component` | 76-91 | component_builder |
| `src/app/readiness.rs` | `App::service_status` | 98-278 | caller (line 195 passes cfg!(windows)) |
| `src/server.rs` | `router (/ready route)` | 271-282 | http_route |
| `src/app/readiness.rs` | `tests::data_dir_length_warns_only_on_windows_above_the_limit` | 446-472 | test |
| `src/app/readiness.rs` | `tests::data_dir_length_counts_characters_not_bytes` | 475-484 | test |
| `tests/cases/readiness.rs` | `readiness_lists_the_data_dir_length_component_as_optional` | 120-149 | test |

Verification:

- read: `AGENTS.md 114-119`
- grep: `rg -n 'data_dir_length_component|WINDOWS_DATA_DIR' src/app/readiness.rs (69, 76, 195, tests 447-478)`
- read: `src/server.rs 271-282 (/ready returns 503 when !report.ready)`
- grep: `rg -n data_dir_path_length tests/cases/readiness.rs (130, 146)`

### q03 (medium)

When the command-line search is given a search profile name that is not configured, what code path produces the failure, where is the error message built, and which integration test runs the real binary to check it?

Maps: `entrypoint_cli`, `call_graph`, `config_key`, `error_string`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: grep 'unknown search profile' finds the message and the test; the CLI path needs reading.

Hops:

1. Find the clap flag for the profile on the Search subcommand.
2. Follow the Search match arm in main() to the App method it calls.
3. Follow the App method to the Config method that resolves the profile and raises.
4. Find the integration test that spawns the CLI with an unknown profile.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `src/main.rs` | `Command::Search (search_profile flag)` | 98-118 | cli_flag |
| `src/main.rs` | `main (Command::Search arm)` | 713-736 | cli_handler |
| `src/app/search.rs` | `App::search_in_profile` | 583-601 | app_method |
| `lci-core/src/config.rs` | `Config::search_settings` | 880-895 | raise_site |
| `tests/cases/indexing.rs` | `the_command_line_search_takes_a_search_profile_and_names_the_configured_ones_when_it_is_unknown` | 3175-3263 | test |

Verification:

- grep: `rg -n 'search-profile' src/main.rs (116)`
- read: `src/main.rs 713-736; src/app/search.rs 583-601 (line 591 calls config.search_settings)`
- read: `lci-core/src/config.rs 880-895 (two bail! messages)`
- grep: `rg -n 'unknown search profile' tests/cases/indexing.rs (3049, 3258)`

### q04 (medium)

Which configuration key decides whether rust-analyzer may run repository build scripts and use the network, which adapter methods read it, where does the LSP manager hand those environment overrides to the spawned process, which unit tests cover both settings, and which commit introduced it?

Maps: `config_key`, `trait_impl`, `call_graph`, `test_link`, `git_commit_message`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep plus reading from the key name; the commit needs git history.

Hops:

1. Find the boolean field on Config and its default.
2. Find RustServer::new and the LspAdapter methods that branch on it.
3. Find the caller that passes adapter.environment() to the transport.
4. Find the unit tests in the Rust adapter module.
5. Find the commit with git log -S on the key.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/config.rs` | `Config::rust_analyzer_trusted` | 299-299 | config_field |
| `lci-core/src/lsp/rust.rs` | `RustServer::new` | 28-34 | reader |
| `lci-core/src/lsp/rust.rs` | `RustServer::environment` | 69-83 | reader |
| `lci-core/src/lsp/rust.rs` | `RustServer::initialize_params` | 85-110 | reader |
| `lci-core/src/lsp.rs` | `GenericClient::spawn` | 66-85 | caller (line 72) |
| `lci-core/src/lsp/rust.rs` | `tests::default_initialization_disables_repository_execution` | 162-209 | test |
| `lci-core/src/lsp/rust.rs` | `tests::trusted_initialization_restores_server_defaults_and_inherited_environment` | 212-234 | test |
| `8888858` | `commit: N36: rust-analyzer starts with repository execution and network access off` |  | commit |

Verification:

- grep: `rg -n rust_analyzer_trusted lci-core src README.md config.example.toml`
- read: `lci-core/src/lsp/rust.rs 1-140 and 160-235`
- grep: `rg -n '\.environment\(\)' lci-core/src/lsp.rs (72)`
- git_log: `git log -S rust_analyzer_trusted --oneline; git show --stat 8888858`

### q05 (medium)

The LspAdapter trait has an optional hook that runs before a definition or references request on a specific file. Which implementations override it, which two implementations rely on the default no-op, and where does the manager call the hook?

Maps: `trait_impl`, `lsp_symbol`, `call_graph`

Baseline expectation (guess, not measured): `needs-reading`: grep the hook name across lsp/*.rs; the default-only set is a diff over seven files.

Hops:

1. Read the trait and find the default method.
2. List every impl LspAdapter for ... and check which define the hook.
3. Find the manager call sites of the hook.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/lsp/adapter.rs` | `LspAdapter::before_position_request (default)` | 78-85 | trait_default |
| `lci-core/src/lsp/go.rs` | `GoServer::before_position_request` | 123-154 | override |
| `lci-core/src/lsp/typescript.rs` | `TypeScriptServer::before_position_request` | 123-157 | override |
| `lci-core/src/lsp/python.rs` | `PythonServer::before_position_request` | 125-156 | override |
| `lci-core/src/lsp/java.rs` | `JavaServer::before_position_request` | 221-245 | override |
| `lci-core/src/lsp/clangd.rs` | `ClangdServer::before_position_request` | 122-153 | override |
| `lci-core/src/lsp/rust.rs` | `impl LspAdapter for RustServer` | 38-155 | uses_default |
| `lci-core/src/lsp/csharp.rs` | `impl LspAdapter for CSharpServer` | 152-190 | uses_default |
| `lci-core/src/lsp.rs` | `Manager::position_request` | 808-853 | call_site (lines 826, 847) |

Verification:

- grep: `rg -n 'impl LspAdapter for|fn before_position_request' lci-core/src/lsp (7 impls, 5 overrides)`
- read: `lci-core/src/lsp/adapter.rs 14-86`
- grep: `rg -n 'before_position_request' lci-core/src/lsp.rs (826, 847 inside position_request 808-853)`

### q06 (hard)

Exactly one language-server adapter overrides both the hook that classifies a JSON-RPC error as 'still loading' and the hook that supplies child-process environment overrides. Which adapter, where are those two hooks consumed in the LSP manager, and which test pins the loading-error rule?

Maps: `trait_impl`, `call_graph`, `lsp_reference`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: grep both hook names and intersect; consumers found by grep of the method names.

Hops:

1. Read the LspAdapter defaults for is_loading_error and environment.
2. Grep all adapter files for overrides of both.
3. Find where the manager calls is_loading_error (via is_transient) and environment (spawn).
4. Find the unit test for the override.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/lsp/adapter.rs` | `LspAdapter::environment (default)` | 39-41 | trait_default |
| `lci-core/src/lsp/adapter.rs` | `LspAdapter::is_loading_error (default)` | 63-65 | trait_default |
| `lci-core/src/lsp/rust.rs` | `RustServer::is_loading_error` | 119-126 | override |
| `lci-core/src/lsp/rust.rs` | `RustServer::environment` | 69-83 | override |
| `lci-core/src/lsp.rs` | `is_transient` | 186-188 | consumer |
| `lci-core/src/lsp.rs` | `send_with_repeats` | 979-1027 | consumer (line 1003) |
| `lci-core/src/lsp.rs` | `GenericClient::spawn` | 66-85 | consumer (line 72) |
| `lci-core/src/lsp/rust.rs` | `tests::file_not_found_is_a_loading_error_only_before_quiescence` | 244-255 | test |

Verification:

- grep: `rg -n 'fn is_loading_error|fn environment' lci-core/src/lsp (only adapter.rs and rust.rs)`
- grep: `rg -n 'is_loading_error|\.environment\(\)|is_transient\(' lci-core/src/lsp.rs (72, 187, 393, 1003)`
- read: `lci-core/src/lsp/rust.rs 119-126, 244-255; lci-core/src/lsp/adapter.rs 39-41 and 52-65`

### q07 (medium)

A file with the TypeScript extension that is really a Qt Linguist translation catalog is not indexed. Which language-registry method detects it, where during the workspace scan is the file diverted to a skipped list, which tests cover it at unit and integration level, and which commit added the behaviour?

Maps: `language_map`, `call_graph`, `test_link`, `git_commit_message`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep for qt_linguist/non_source; the commit needs git history.

Hops:

1. Find the ts LanguageAdapter and its non-source check.
2. Find the caller in the chunk scanner.
3. Find unit tests in language.rs and the integration test in indexing.rs.
4. Find the commit via its N-number in the roadmap or git log.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/language.rs` | `LanguageAdapter::non_source` | 60-65 | detector |
| `lci-core/src/language.rs` | `qt_linguist` | 414-423 | detector |
| `lci-core/src/chunk.rs` | `sources` | 204-249 | caller (line 229 calls non_source, line 230 pushes SkippedFile) |
| `lci-core/src/language.rs` | `tests::qt_linguist_catalogs_are_recognized_by_their_xml_start` | 628-646 | test |
| `tests/cases/indexing.rs` | `a_qt_linguist_ts_file_is_skipped_while_typescript_is_indexed` | 3281-3343 | test |
| `df1255c` | `commit: N11: Qt Linguist .ts catalogs are skipped instead of parsed as TypeScript` |  | commit |

Verification:

- grep: `rg -n 'non_source\(' lci-core src (chunk.rs 229)`
- read: `lci-core/src/language.rs 55-66, 404-423; lci-core/src/chunk.rs 204-249`
- git_log: `git log --oneline | rg N11; git show --stat df1255c (chunk.rs, language.rs, tests/cases/indexing.rs)`

### q08 (medium)

The file that defines the JSON-RPC client used to talk to every language server: which test file has changed together with it most often in git history, and which test-support file is its most frequent support co-change? Give the counts.

Maps: `lsp_symbol`, `module_graph`, `git_cochange`, `test_link`

Baseline expectation (guess, not measured): `needs-structure`: co-change counts need git history.

Hops:

1. Locate the JsonRpcClient definition (the transport module).
2. List every commit touching that file and count co-touched files.
3. Pick the top test file and the top tests/support file.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/lsp/transport.rs` | `JsonRpcClient` | 142-142 | subject_file |
| `tests/cases/navigation.rs` |  |  | cochange_test (9 of 10 commits) |
| `tests/support/fake_lsp.rs` |  |  | cochange_support (7 of 10 commits) |

Verification:

- grep: `rg -n 'pub struct JsonRpcClient' lci-core (transport.rs 142)`
- git_log: `git log --format=%h -- lci-core/src/lsp/transport.rs | xargs -n1 git show --format= --name-only | sort | uniq -c | sort -rn -> tests/cases/navigation.rs 9/10; tests/support/fake_lsp.rs 7/10 (tied with lci-core/src/config.rs 7 and docs/development/roadmap.md 7); lci-core/src/lsp.rs 6/10; 10 commits touch the file (no --follow), history counts: 188 commits at base 7f9c636, where the counts above were taken; the earlier "191" was the spike branch at 842a5e9 (192 at b5e5851); the spike commits touch only docs/spike, so the co-change counts are the same at base and at the spike tip`

### q09 (easy)

Which source file changes most often together with the language registry (the module mapping file extensions to Tree-sitter grammars and identifiers), how many of the registry's commits include it, and which function in that file looks languages up per path during the workspace scan?

Maps: `language_map`, `git_cochange`, `call_graph`

Baseline expectation (guess, not measured): `needs-structure`: co-change counts need git history.

Hops:

1. Identify the registry module (ADAPTERS, for_extension, for_path).
2. Count co-changed files across the registry's commits.
3. Confirm the static call from the top co-change back into the registry.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/language.rs` | `ADAPTERS / for_path` | 561-565 | subject_file |
| `lci-core/src/chunk.rs` | `sources` | 204-249 | cochange_source (7 of 8 commits; calls language::for_path at line 219) |
| `tests/cases/indexing.rs` |  |  | cochange_test (6 of 8 commits) |

Verification:

- git_log: `git log --format=%h -- lci-core/src/language.rs | xargs -n1 git show --format= --name-only | sort | uniq -c | sort -rn -> chunk.rs 7, tests/cases/indexing.rs 6, README.md 5, lci-core/Cargo.toml 5, Cargo.lock 5; 8 commits (no --follow; with --follow 12, because the file moved from src/language.rs)`
- grep: `rg -n 'language::for_path' lci-core/src/chunk.rs (219)`

### q10 (medium)

Which non-documentation source file changes most often together with the search pipeline module (the one holding App::search_in_profile), what are the counts, and through which two methods of that file does a search obtain its per-query settings?

Maps: `git_cochange`, `config_key`, `call_graph`

Baseline expectation (guess, not measured): `needs-structure`: co-change counts need git history.

Hops:

1. Locate the module that defines App::search_in_profile.
2. Count co-changed files over its history; skip README/docs.
3. Find the calls from the search module into the top co-changed file.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `src/app/search.rs` | `App::search_with_options / App::search_in_profile` | 560-601 | subject_file (lines 568, 591) |
| `lci-core/src/config.rs` | `Config::base_search_settings` | 867-873 | cochange_source (9 of 15 commits) |
| `lci-core/src/config.rs` | `Config::search_settings` | 880-895 | cochange_source |
| `config.example.toml` |  |  | cochange_config (9 of 15, tie) |
| `tests/cases/indexing.rs` |  |  | cochange_test (9 of 15, tie) |

Verification:

- git_log: `git log --format=%h -- src/app/search.rs | xargs -n1 git show --format= --name-only | sort | uniq -c | sort -rn -> README.md 14, tests/cases/indexing.rs 9, lci-core/src/config.rs 9, config.example.toml 9; 15 commits (no --follow)`
- grep: `rg -n 'search_settings' src/app/search.rs (568, 591)`

### q11 (easy)

Without RUST_LOG set, why do events from the lci-core crate (LSP restarts, JSON-RPC reader events) appear in the server log? Name the constant, where main() applies it, the test that pins it, and the commit that changed it.

Maps: `git_commit_message`, `config_key`, `entrypoint_cli`, `test_link`

Baseline expectation (guess, not measured): `needs-structure`: code part is one grep for 'lci_core=info'; the commit needs git history.

Hops:

1. Find the default env-filter string in the binary.
2. Find where main() falls back to it when RUST_LOG is absent.
3. Find the unit test that parses the filter.
4. Find the fixing commit with git log -S on the filter text.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `src/main.rs` | `DEFAULT_LOG_FILTER` | 606-607 | constant |
| `src/main.rs` | `main` | 609-860 | reader (EnvFilter::try_from_default_env fallback at 612-614) |
| `src/main.rs` | `tests::default_log_filter_enables_app_and_core_at_info` | 884-904 | test |
| `182c062` | `commit: N10: the default log filter keeps lci_core events` |  | commit |

Verification:

- grep: `rg -n DEFAULT_LOG_FILTER src/main.rs (607, 614, 864, 885)`
- git_log: `git log -S 'lci_core=info' --oneline -> 182c062 (fix) and 6e73914 (roadmap row)`

### q12 (hard)

A huge lsp_timeout_seconds value used to make the first LSP request panic. Where is the value now rejected, what upper bound is used, what second guard keeps the request deadline from overflowing, which tests cover both, and which commit made the change?

Maps: `config_key`, `error_string`, `call_graph`, `test_link`, `git_commit_message`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep plus reading; the commit needs git history.

Hops:

1. Find the bound constant and the validate() check on Config.
2. Find the transport request deadline computation and its cap.
3. Find the config unit test and the navigation integration test.
4. Find the commit through git log on the transport file.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/config.rs` | `Config::MAX_LSP_TIMEOUT_SECONDS` | 672-672 | constant (86400) |
| `lci-core/src/config.rs` | `Config::validate` | 683-863 | raise_site (lines 733-737) |
| `lci-core/src/lsp/transport.rs` | `LONGEST_WAIT` | 40-40 | constant |
| `lci-core/src/lsp/transport.rs` | `JsonRpcClient::request` | 313-379 | guard (line 351) |
| `lci-core/src/config.rs` | `tests::an_lsp_timeout_that_cannot_be_added_to_the_clock_is_rejected` | 1027-1037 | test |
| `tests/cases/navigation.rs` | `a_transport_timeout_too_long_for_the_clock_does_not_panic_a_request` | 1099-1108 | test |
| `ab7de12` | `commit: N52, N51: answer server requests with a string id; bound lsp_timeout_seconds and the request deadline` |  | commit |

Verification:

- grep: `rg -n 'lsp_timeout_seconds|MAX_LSP' lci-core/src/config.rs (672, 733-737, 1028)`
- grep: `rg -n LONGEST_WAIT lci-core/src/lsp/transport.rs (40, 349-351)`
- git_log: `git log --oneline -- lci-core/src/lsp/transport.rs; git show --stat ab7de12 (config.rs, transport.rs, tests/cases/navigation.rs, tests/support/fake_lsp.rs)`

### q13 (medium)

Which test sources does `cargo xtask test --suite Lsp` execute, and how do the cargo filters it passes map onto crate modules and the integration-test binary?

Maps: `entrypoint_xtask`, `build_graph`, `module_graph`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: read the suite arm, then map cargo filters to modules by reading lib.rs and tests/integration.rs.

Hops:

1. Find the Suite::Lsp arm in the xtask test module.
2. Map the --lib filter lsp:: to modules (lci-core exports pub mod lsp; the root crate has none).
3. Map the integration filter navigation:: through tests/integration.rs to its #[path] file.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `xtask/src/test.rs` | `run (Suite::Lsp arm)` | 108-139 | suite_definition (Suite::Lsp arm at lines 126-129) |
| `lci-core/src/lib.rs` | `pub mod lsp` | 6-6 | module_root |
| `lci-core/src/lsp.rs` | `mod tests and lsp/* submodule tests` |  | unit_tests (lsp::) |
| `tests/integration.rs` | `mod navigation (#[path = "cases/navigation.rs"])` | 11-12 | test_binary_module |
| `tests/cases/navigation.rs` |  |  | integration_tests (navigation::) |

Verification:

- read: `xtask/src/test.rs 108-139`
- grep: `rg -n 'mod lsp' src lci-core/src/lib.rs (only lci-core/src/lib.rs:6)`
- read: `tests/integration.rs 1-16`

### q14 (easy)

AGENTS.md states that focused test suites fail when their filter matches zero tests. Which xtask function enforces this, how does it count executed tests, and which unit tests pin the counting? Is the rule limited to the --lib unit suites?

Maps: `agent_instructions`, `entrypoint_xtask`, `call_graph`, `test_link`

Baseline expectation (guess, not measured): `easy`: one grep lands in xtask/src/test.rs, which holds every code answer.

Hops:

1. Read the AGENTS.md Build-and-test bullet.
2. Find the xtask function that streams cargo output and bails on zero.
3. Find the summary-line parser and its tests.
4. Check which suites route through it (both --lib and integration filters do).

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `AGENTS.md` | `Working method: Build and test` | 120-126 | doc_statement |
| `xtask/src/test.rs` | `invoke_focused` | 62-86 | enforcer |
| `xtask/src/test.rs` | `executed_in_summary` | 46-58 | counter |
| `xtask/src/test.rs` | `run` | 108-139 | caller (every focused suite, incl. integration filters) |
| `xtask/src/test.rs` | `tests::a_filter_that_matches_nothing_counts_zero` | 178-185 | test |
| `xtask/src/test.rs` | `tests::counts_passed_and_failed_but_not_ignored_or_filtered` | 162-175 | test |
| `xtask/src/test.rs` | `tests::ignores_non_summary_lines` | 188-191 | test |
| `tests/AGENTS.md` | `Every focused suite fails if its filter matches zero tests` | 18-19 | doc_statement |

Verification:

- read: `AGENTS.md 120-126; xtask/src/test.rs 1-192`
- grep: `rg -n 'ran no tests' xtask/src/test.rs (80-83); rg -n 'fn ' xtask/src/test.rs (tests at 162, 178, 188)`

Note: Every code answer sits in one file (xtask/src/test.rs); only the two doc statements are elsewhere. Reading that file after one grep answers the question, so it is weak evidence that multiple maps are needed.

### q15 (medium)

What does `cargo xtask lint-machine-paths` scan and how? Name the dispatch, the module entry, how it enumerates files, the directory it exempts, the optional local pattern file, and the other xtask command that runs it automatically.

Maps: `entrypoint_xtask`, `module_graph`, `call_graph`, `config_key`, `doc_to_code`

Baseline expectation (guess, not measured): `needs-reading`: grep the command name, then read lint.rs and test.rs.

Hops:

1. Find the LintMachinePaths subcommand and its dispatch in xtask main.
2. Follow lint::run to scan_repo and tracked_files.
3. Find the exempt prefix and the extra pattern file constant.
4. Find which test suite calls lint::run.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `xtask/src/main.rs` | `Command::LintMachinePaths` | 64-69 | subcommand |
| `xtask/src/lint.rs` | `run` | 294-315 | entry |
| `xtask/src/lint.rs` | `scan_repo` | 268-292 | scanner (skips EXEMPT_PREFIX at 271) |
| `xtask/src/lint.rs` | `tracked_files` | 233-256 | file_enumeration (git ls-files -z) |
| `xtask/src/lint.rs` | `EXEMPT_PREFIX` | 26-26 | exemption (.agents/skills/rust-skills/) |
| `xtask/src/lint.rs` | `EXTRA_PATTERNS_FILE / load_extra` | 24-24, 72-79 | local_pattern_file |
| `xtask/src/test.rs` | `run (Suite::Full arm)` | 130-137 | automatic_caller |
| `docs/development/machine-values.md` | `## Lint` | 166-166 | doc |

Verification:

- read: `xtask/src/main.rs 60-70, 258; xtask/src/lint.rs 1-60, 233-315`
- grep: `rg -n 'lint::run' xtask/src (main.rs 258, test.rs 132)`

### q16 (hard)

During `cargo xtask live-acceptance`, how does a step get reported as skipped rather than passed, which acceptance engines emit the signal, how does --require-all change the outcome, which test pins it, and which commit introduced the SKIPPED reporting?

Maps: `error_string`, `call_graph`, `entrypoint_xtask`, `test_link`, `git_commit_message`, `doc_to_code`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep plus reading across xtask; the commit needs git history.

Hops:

1. Find the stdout marker parsed by the orchestrator.
2. Find which acceptance run functions print that marker.
3. Follow write_summary and the --require-all flag from xtask main.
4. Find the unit test and the commit; cross-check the docs.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `xtask/src/live_acceptance.rs` | `completed_status` | 52-63 | marker_consumer |
| `xtask/src/live_acceptance.rs` | `write_summary` | 65-94 | require_all_logic |
| `xtask/src/live_acceptance.rs` | `run` | 312-506 | orchestrator |
| `xtask/src/main.rs` | `Command::LiveAcceptance (require_all)` | 46-58 | cli_flag |
| `xtask/src/acceptance/lsp_full.rs` | `run` | 145-191 | raise_site (line 154) |
| `xtask/src/acceptance/recovery.rs` | `run` | 190-266 | raise_site (line 202) |
| `xtask/src/acceptance/csharp_lsp/mod.rs` | `run` | 40-63 | raise_site (line 53) |
| `xtask/src/live_acceptance.rs` | `tests::skipped_step_is_listed_and_requires_all_to_fail` | 546-572 | test |
| `docs/development/live-acceptance-ci.md` |  | 101-102 | doc |
| `102f118` | `commit: N56: a skipped live-acceptance step reads as SKIPPED, and --require-all fails on it` |  | commit |

Verification:

- grep: `rg -n PREREQUISITE_UNAVAILABLE xtask/src`
- read: `xtask/src/live_acceptance.rs 20-94`
- git_log: `git log --oneline -- xtask/src/live_acceptance.rs; git show --stat 102f118`

### q17 (hard)

Trace Go support end to end: from the .go extension to its language identifier, the LSP adapter that serves it, the config section that enables it, where the adapter is registered, and the xtask acceptance command (plus its live-acceptance step) that proves real navigation as the language-adapter skill requires.

Maps: `language_map`, `trait_impl`, `config_key`, `entrypoint_xtask`, `agent_instructions`, `lsp_symbol`

Baseline expectation (guess, not measured): `needs-reading`: grep go/gopls/GoServer across many files; long but lexical.

Hops:

1. Find the go entry in the language ADAPTERS table.
2. Find the LspAdapter impl whose languages() is go and its config type.
3. Find Manager::new registration.
4. Find the xtask go-lsp subcommand, its module, the shared engine and the live-acceptance step.
5. Map to row 14 of the skill's acceptance reference.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/language.rs` | `ADAPTERS (go entry)` | 518-524 | extension_map |
| `lci-core/src/lsp/go.rs` | `impl LspAdapter for GoServer` | 46-155 | lsp_adapter |
| `lci-core/src/config.rs` | `GoLspConfig / Config::go` | 486-509 | config_section ([go] path/args/disabled; field at 347) |
| `lci-core/src/lsp.rs` | `Manager::new` | 552-577 | registration |
| `xtask/src/main.rs` | `AcceptanceCommand::GoLsp` | 151-154 | xtask_subcommand (dispatch line 237) |
| `xtask/src/acceptance/go_lsp.rs` | `run` | 16-57 | acceptance_spec |
| `xtask/src/acceptance/lsp_full.rs` | `run` | 145-191 | shared_engine |
| `xtask/src/live_acceptance.rs` | `run (step list)` | 383-383 | live_step |
| `.agents/skills/lci-language-adapter/references/acceptance.md` | `row 14` | 28-28 | acceptance_bar |

Verification:

- read: `lci-core/src/language.rs 515-525; lci-core/src/lsp/go.rs 1-70; lci-core/src/config.rs 486-509`
- read: `xtask/src/acceptance/go_lsp.rs 1-57`
- grep: `rg -n 'go-lsp|GoLsp' xtask/src (main.rs 151, 237; live_acceptance.rs 383)`

### q18 (hard)

AGENTS.md says an embedding-identity change must make search fail rather than serve old vectors. What goes into the identity, where does a search under manual/watch freshness raise the error, where does the on-search path turn a failed refresh into a hard error instead of a warning, and which integration test asserts it?

Maps: `agent_instructions`, `config_key`, `error_string`, `call_graph`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: grep the error text and embedding_identity; the on-search path needs careful reading.

Hops:

1. Read the hard rule in AGENTS.md.
2. Find Config::embedding_identity and its inputs.
3. Find the identity comparison in resolve_index_for_search.
4. Follow the on-search branch to refresh_stale_snapshot and stale_refresh_fallback.
5. Find the test that reopens the App with a different model under Manual freshness.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `AGENTS.md` | `Hard rules: index freshness / embedding-identity exception` | 30-37 | doc_statement |
| `lci-core/src/config.rs` | `Config::embedding_identity` | 921-936 | identity (url\|model\|chunk::DOCUMENT_FORMAT_VERSION[\|doc=hash]) |
| `src/app/search.rs` | `App::resolve_index_for_search` | 1022-1123 | raise_site (lines 1096-1108) |
| `src/app/indexing.rs` | `App::refresh_stale_snapshot` | 306-396 | on_search_path |
| `src/app/indexing.rs` | `App::stale_refresh_fallback` | 402-419 | raise_site |
| `tests/cases/indexing.rs` | `persistence_incremental_isolation_deletion_and_fail_open` | 565-726 | test (Manual freshness, lines 719-723) |

Verification:

- read: `AGENTS.md 30-37; lci-core/src/config.rs 915-936`
- read: `src/app/search.rs 1094-1123; src/app/indexing.rs 300-330, 395-419`
- grep: `rg -n 'embedding_model = ' tests/cases/indexing.rs (720); read 565-574 (freshness = Manual)`

### q19 (medium)

The ordering of fused search hits with equal scores is implemented in two places that must agree. Where are they, which CLI subcommand depends on the second copy, which tests pin each, and which commit last changed both together?

Maps: `call_graph`, `git_commit_message`, `entrypoint_cli`, `test_link`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep for fusion_order/fused_order; the commit that changed both needs git history.

Hops:

1. Find the comparator used by hits.sort_by in the search pipeline.
2. Find the replay module's comparator.
3. Find the replay CLI entry in main.rs.
4. Find the commit touching both (git log on replay.rs).

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `src/app/search.rs` | `fusion_order` | 337-344 | comparator (used at 941) |
| `src/evaluate/replay.rs` | `fused_order` | 331-338 | comparator_copy |
| `src/main.rs` | `run_replay` | 282-296 | cli_handler (Command::Replay at 168) |
| `src/app/search.rs` | `tests::equal_score_chunks_with_the_same_path_and_start_line_have_a_stable_order` | 1802-1841 | test |
| `src/evaluate/replay.rs` | `tests::equal_score_candidates_with_the_same_path_and_start_line_have_a_stable_order` | 1199-1227 | test |
| `879a1cb` | `commit: N53: equal-score fused chunks get one stable order` |  | commit |

Verification:

- grep: `rg -n 'fusion_order|fused_order' src (search.rs 337/941, replay.rs 331)`
- git_log: `git log --oneline -- src/evaluate/replay.rs (2 commits); git show --stat 879a1cb (search.rs + replay.rs + evaluate/tests.rs)`

### q20 (medium)

When an MCP search_code call asks for more results than the search can return, the response says so in notes rather than warnings. Which config value caps the result count, where is the note text built and appended, where does the MCP tool argument document it, which tests cover it, and which commit added it?

Maps: `entrypoint_mcp`, `config_key`, `call_graph`, `test_link`, `git_commit_message`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep for the note text; the commit needs git history.

Hops:

1. Find the top_k argument doc on the MCP SearchArgs.
2. Find the cap config field.
3. Find the note builder and the call site in search_once.
4. Find unit and integration tests and the commit.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `src/server.rs` | `SearchArgs::top_k` | 30-34 | mcp_argument_doc |
| `lci-core/src/config.rs` | `Config::rerank_candidate_count` | 280-284 | config_field |
| `src/app/search.rs` | `pool_cut_note` | 462-471 | note_builder |
| `src/app/search.rs` | `App::search_once` | 637-1015 | call_site (lines 963-971) |
| `src/app/search.rs` | `tests::a_note_names_top_k_and_the_window_only_when_the_cut_leaves_fewer_results_than_asked` | 1429-1442 | test |
| `tests/cases/indexing.rs` | `a_top_k_above_the_rerank_window_is_named_in_a_note` | 2875-2951 | test |
| `cf15de4` | `commit: N69: a search whose top_k is above the rerank window says so in `notes`` |  | commit |

Verification:

- grep: `rg -n pool_cut_note src/app/search.rs (462, 968, tests 1430-1441)`
- read: `src/app/search.rs 455-471, 955-975; src/server.rs 28-35`
- git_log: `git show --stat cf15de4 (search.rs, server.rs, config.rs, tests/cases/indexing.rs)`

### q21 (medium)

If a workspace has indexed files in a language whose only language server is disabled, how does the search_symbols MCP tool tell the caller, and where is that decided? Include the registry lookup used and the integration tests.

Maps: `entrypoint_mcp`, `call_graph`, `lsp_symbol`, `error_string`, `test_link`, `git_commit_message`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep plus reading in navigation.rs; the commit needs git history.

Hops:

1. Find the search_symbols tool and the App method it calls.
2. Find where App::symbols groups adapters per indexed language and collects not-searched languages into warnings.
3. Find Manager::adapters_for_language.
4. Find the integration tests (git show of the commit that added them).

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `src/server.rs` | `McpServer::search_symbols` | 129-138 | mcp_tool |
| `src/app/navigation.rs` | `App::symbols` | 76-161 | decision (not_searched 92-115, warnings 150-155) |
| `lci-core/src/lsp.rs` | `Manager::adapters_for_language` | 585-590 | registry_lookup |
| `tests/cases/navigation.rs` | `symbols_name_an_indexed_language_whose_only_server_is_disabled` | 2400-2424 | test |
| `tests/cases/navigation.rs` | `symbols_still_search_the_enabled_language_when_another_one_is_disabled` | 2429-2449 | test |
| `3bac689` | `commit: N26 (a): search_symbols names each indexed language that no enabled server searches` |  | commit |

Verification:

- read: `src/server.rs 129-138; src/app/navigation.rs 76-161; lci-core/src/lsp.rs 583-590`
- git_log: `git show 3bac689 -- tests/cases/navigation.rs | rg '^\+.*fn '`

### q22 (medium)

How does a [search_profiles.<name>] table in the config become its own MCP endpoint? Name the config field, the name validation, the functions that build and mount the endpoint, and the integration tests for both a known and an unknown profile path.

Maps: `config_key`, `entrypoint_mcp`, `call_graph`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: grep search_profiles and mcp_path, then read server.rs and the tests.

Hops:

1. Find the search_profiles field and name validation in Config.
2. Find mcp_path and the router fold over search_profiles.
3. Find mcp_service, which binds search_code to the profile.
4. Find the MCP integration tests for /mcp/<name>.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/config.rs` | `Config::search_profiles` | 366-366 | config_field |
| `lci-core/src/config.rs` | `is_valid_search_profile_name` | 141-149 | validation (used in validate at 797-801) |
| `src/server.rs` | `mcp_path` | 213-218 | path_builder |
| `src/server.rs` | `mcp_service` | 224-251 | endpoint_builder |
| `src/server.rs` | `router` | 253-307 | mount (fold at 297-305) |
| `tests/cases/mcp.rs` | `each_search_profile_has_its_own_mcp_endpoint_with_the_same_tools` | 641-772 | test |
| `tests/cases/mcp.rs` | `an_unknown_mcp_profile_path_is_not_found_and_a_session_stays_on_its_endpoint` | 775-836 | test |

Verification:

- read: `src/server.rs 210-307; lci-core/src/config.rs 134-149, 795-801`
- grep: `rg -n '/mcp/' tests/cases/mcp.rs (658-812)`

### q23 (medium)

Which config key releases a language server nobody has used for a while, where is the background sweeper started (and when is it not), and which integration tests cover release, the zero setting, and a server that is still priming?

Maps: `config_key`, `call_graph`, `test_link`, `doc_to_code`

Baseline expectation (guess, not measured): `needs-reading`: grep lsp_idle_timeout_seconds finds nearly every answer location.

Hops:

1. Find the config field and its default.
2. Find Manager::new, which conditionally spawns the sweeper.
3. Find spawn_idle_sweeper.
4. Find navigation tests that set the key.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/config.rs` | `Config::lsp_idle_timeout_seconds` | 318-318 | config_field (default 1800 at 653) |
| `lci-core/src/lsp.rs` | `Manager::new` | 552-577 | reader (lines 564-569) |
| `lci-core/src/lsp.rs` | `spawn_idle_sweeper` | 1088-1133 | sweeper (not started when the timeout is 0, or when no Tokio runtime is running: returns at 1089-1092) |
| `tests/cases/navigation.rs` | `an_idle_session_is_shut_down_and_restarted_on_the_next_call` | 2009-2057 | test |
| `tests/cases/navigation.rs` | `idle_timeout_zero_never_releases` | 2062-2083 | test |
| `tests/cases/navigation.rs` | `a_symbol_call_waiting_for_priming_is_not_cut_off_by_the_idle_sweep` | 2705-2728 | test |
| `tests/cases/navigation.rs` | `the_idle_sweep_leaves_a_server_alone_while_it_is_priming` | 2737-2773 | test |
| `README.md` | `LSP lifecycle paragraph` | 254-254 | doc |

Verification:

- grep: `rg -n lsp_idle_timeout_seconds lci-core/src tests/cases (config.rs 318/653, lsp.rs 564-567, navigation.rs 2012/2065/2709/2741)`
- read: `lci-core/src/lsp.rs 549-577, 1088-1133`

### q24 (hard)

On Windows, language servers run inside a Job Object. Which build-graph entry makes this Windows-only, how is the module gated, where in process spawning is it applied, and what is logged (and where) when a job cannot be created or assigned?

Maps: `build_graph`, `module_graph`, `call_graph`, `error_string`

Baseline expectation (guess, not measured): `needs-reading`: grep job/JobObject and Cargo.toml target sections, then read the spawn path.

Hops:

1. Find the target-specific dependency in lci-core/Cargo.toml.
2. Find the #[cfg(windows)] mod declaration and the transport import.
3. Find JsonRpcClient::spawn calling job::prepare and job::adopt, and its caller.
4. Find the warn-once log site.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/Cargo.toml` | `[target.'cfg(windows)'.dependencies] windows-sys` | 42-49 | build_graph |
| `lci-core/src/lsp.rs` | `#[cfg(windows)] mod job` | 39-40 | module_gate |
| `lci-core/src/lsp/transport.rs` | `JsonRpcClient::spawn` | 179-274 | apply_site (lines 185-193) |
| `lci-core/src/lsp/job.rs` | `prepare` | 103-114 | job_create |
| `lci-core/src/lsp/job.rs` | `adopt` | 124-145 | job_assign |
| `lci-core/src/lsp/job.rs` | `warn_unconfined` | 266-275 | log_site |
| `lci-core/src/lsp.rs` | `GenericClient::spawn` | 66-85 | caller |

Verification:

- read: `lci-core/Cargo.toml 33-49; lci-core/src/lsp/job.rs 1-30, 100-145, 262-275`
- grep: `rg -n 'cfg\(windows\)|job::' lci-core/src/lsp.rs lci-core/src/lsp/transport.rs`

### q25 (medium)

A search filter naming an unregistered language fails with a message listing the valid identifiers. Where is the error raised and rendered, which registry function produces the list, which identifier used to appear twice and why, and which tests and commit fixed it?

Maps: `error_string`, `language_map`, `call_graph`, `test_link`, `git_commit_message`

Baseline expectation (guess, not measured): `needs-structure`: code part is grep for the error text; why an identifier appeared twice needs git history.

Hops:

1. Find FilterError::UnknownLanguage and where from_request returns it.
2. Find the Display impl that renders the identifiers.
3. Find language::identifiers and the ADAPTERS rows that share an identifier.
4. Find the tests and commit (git show --stat).

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/filter/request.rs` | `EffectiveFilter::from_request` | 224-271 | raise_site (line 234) |
| `lci-core/src/filter/request.rs` | `impl fmt::Display for FilterError` | 177-218 | message (lines 184-190) |
| `lci-core/src/language.rs` | `identifiers` | 569-579 | registry_list |
| `lci-core/src/language.rs` | `ADAPTERS (cpp and hpp both map to cpp)` | 539-552 | duplicate_source |
| `src/app/search.rs` | `App::search_once` | 637-1015 | caller (line 655) |
| `lci-core/src/filter/tests.rs` | `the_unknown_language_message_names_each_identifier_once` | 570-587 | test |
| `lci-core/src/language.rs` | `tests::each_identifier_is_listed_once_in_registration_order` | 703-720 | test |
| `33f6995` | `commit: N71: the unknown-language error lists each identifier once` |  | commit |

Verification:

- grep: `rg -n UnknownLanguage lci-core/src/filter (162, 184, 234)`
- grep: `rg -n 'identifier: "cpp"' -B2 lci-core/src/language.rs (540 cpp, 547 hpp)`
- git_log: `git show --stat 33f6995`

### q26 (easy)

Among the seven LspAdapter implementations, which adapter source files have not been modified since the commit that split lci-core out of the main crate, and which were modified most recently?

Maps: `trait_impl`, `git_recency`

Baseline expectation (guess, not measured): `needs-structure`: last-modified commits per file need git history.

Hops:

1. List the impl LspAdapter for ... files.
2. Run git log -1 per file and compare with the crate-split commit.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `lci-core/src/lsp/csharp.rs` | `impl LspAdapter for CSharpServer` | 152-190 | unchanged_since_split (0729d9a) |
| `lci-core/src/lsp/python.rs` | `impl LspAdapter for PythonServer` | 44-157 | unchanged_since_split (0729d9a) |
| `lci-core/src/lsp/typescript.rs` | `impl LspAdapter for TypeScriptServer` | 49-158 | unchanged_since_split (0729d9a) |
| `lci-core/src/lsp/rust.rs` | `impl LspAdapter for RustServer` | 38-155 | most_recent (8888858) |

Verification:

- grep: `rg -n 'impl LspAdapter for' lci-core/src/lsp`
- git_log: `for f in lci-core/src/lsp/*.rs; do git log -1 --format='%h %ad' --date=iso -- $f; done -> csharp/python/typescript 0729d9a (Split lci-core out of the main crate); go 9c15e3d; java 58ae2c2; clangd 2edf919; rust/adapter/transport 8888858`

### q27 (hard)

Which commit introduced the Windows data_dir length limit, what index-table naming change came with it, where does the store still read and later drop tables with the old name, and which integration tests did that commit add?

Maps: `git_blame`, `call_graph`, `test_link`, `doc_to_code`

Baseline expectation (guess, not measured): `needs-structure`: the introducing commit and the tests it added need git history.

Hops:

1. Run git log -S on the limit constant.
2. Read the commit's stat; find table_name and legacy_table_name.
3. Find Store methods that use the legacy name.
4. List the test functions the commit added.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `866541c` | `commit: Shorten LanceDB table names so longer data_dir paths work on Windows` |  | commit |
| `lci-core/src/workspace.rs` | `Workspace::table_name` | 40-44 | new_name (ws_ + 16 hex) |
| `lci-core/src/workspace.rs` | `Workspace::legacy_table_name` | 48-50 | old_name (ws_ + full id) |
| `src/store.rs` | `Store::snapshot` | 126-173 | legacy_reader |
| `src/store.rs` | `Store::drop_legacy_table` | 318-329 | legacy_drop (called from Store::replace at 306) |
| `src/app/readiness.rs` | `WINDOWS_DATA_DIR_LENGTH_LIMIT` | 69-69 | constant |
| `tests/cases/indexing.rs` | `legacy_long_table_name_is_searchable_and_migrated_on_refresh` | 1442-1510 | test |
| `tests/cases/readiness.rs` | `readiness_lists_the_data_dir_length_component_as_optional` | 120-149 | test |

Verification:

- git_log: `git log -S WINDOWS_DATA_DIR_LENGTH_LIMIT --oneline -> 866541c; git show --stat 866541c`
- git_log: `git show 866541c -- tests/cases/indexing.rs tests/cases/readiness.rs | rg '^\+.*fn '`
- grep: `rg -n 'legacy_table_name|table_name\(\)' src/store.rs (129, 133, 302, 319)`

### q28 (medium)

tests/AGENTS.md says the fake language server is built as a separate binary. Which manifest declares it, how does that binary reuse the fake's logic, and how do integration tests point the C# server configuration at it?

Maps: `agent_instructions`, `build_graph`, `module_graph`, `test_link`

Baseline expectation (guess, not measured): `needs-reading`: grep fake-lsp-server across Cargo.toml and tests, then read three files.

Hops:

1. Read tests/AGENTS.md.
2. Find the [[bin]] target in the root Cargo.toml.
3. Read fake_lsp_bin.rs (#[path] module plus main) and run_from_env.
4. Find the CARGO_BIN_EXE_fake-lsp-server use in tests/support.

Answer:

| Path | Symbol | Lines | Role |
| --- | --- | --- | --- |
| `tests/AGENTS.md` | `Shared fakes and fixtures` | 3-8 | doc_statement |
| `Cargo.toml` | `[[bin]] fake-lsp-server` | 36-38 | build_target |
| `tests/support/fake_lsp_bin.rs` | `main` | 12-17 | binary_entry (#[path = "fake_lsp.rs"] at 7-9) |
| `tests/support/fake_lsp.rs` | `run_from_env` | 163-258 | shared_logic |
| `tests/support/mod.rs` | `with_fake_csharp_server` | 242-255 | test_wiring |

Verification:

- read: `tests/AGENTS.md 1-12; Cargo.toml 36-38; tests/support/fake_lsp_bin.rs 1-17`
- grep: `rg -n 'CARGO_BIN_EXE_fake-lsp-server' tests (support/mod.rs 245, support/fake_lsp.rs 777, cases/config.rs 801)`
