# Structured configuration gold: round 2

Eight independent families per repository; 24 families and 72 questions. All splits remain `pending`: the coordinator will run `_work/split.py` (seed 20261018, SplitMix64 Fisher-Yates, first four dev). No Python-random split is used. This is source evidence, not retrieval efficacy evidence.

Reproduce with `python -I evaluations/structured/_work/build_gold.py` and arguments `--lci`, `--vite-tree`, `--vite-git`, `--ha-tree`, `--ha-git`, `--rg`. Locations are inputs only; artifacts use repository names and relative paths. Add `--check` to compare all generated artifacts without rewriting. The builder executes the adjacent `families.py` and `finish_gold.py` definitions. Run `python -I evaluations/structured/_work/validate_gold.py` for independent structural checks. PyYAML is already installed; no downloads are performed.

Facts were read from pinned Git blobs independently of LCI, retrieval output, or candidate filtering. Read-only source searches and the exact premise rg command were used; no network, cargo, or Git writes. The first-round brief, frozen PLAN section 3 and review were read before revision. Whole-file SHA-256 hashes use revision bytes; external export bytes must match exactly, and LCI allows only checkout CRLF/LF conversion. Git blob SHA-1 is independently recomputed. `_work/verification.json` records file identities; `_work/witnesses.txt` records every assignment, reader, documentation and mapping span. JSON is parsed strictly, TOML with tomllib, YAML as inert syntax nodes. Assignment spans come from parser source marks or table-aware TOML matching.

No two families in a repository overlap assignment lines. At most two families share a reader file; Home Assistant uses at most two families per YAML file. Every terminal key segment is at least six characters. Each behaviour has documentation or a code comment outside the assignment, and each q2 omits key segments and paraphrases meaning. The two compatibility-compiler families intentionally share one bounded hand-off, but no third family uses that reader file. No lockfile count influenced selection.

Labels: playground and test files are `fixture`. Shipped blueprint metadata is `assignment`; its missing-input fallback is `default`. LCI config.example.toml is shipped configuration with explicit values, labeled `assignment` and identified in questions as the shipped sample; this does not claim it is the user's effective deployment configuration. None of these assignments claim an installation-wide default unless specifically explained.

Reader scopes name actual consuming functions, or explicitly end at a constructor/library hand-off. For hand-offs, mapping witnesses explain the next stage without claiming repository-wide completeness. Declarations, tests, defaults and validation are excluded from runtime readers. Dynamic access over a finite list or declared input definitions is resolved and documented; other functions are outside scope rather than silently scored as exhaustive. Location questions name a fixture/use case, never reveal its exact path.

## Replaced and rejected candidates

- LCI index.prose: terminal segment is five characters; the newer explicit length requirement supersedes the initial starting-evidence preference. Extra fusion keys were removed because search.rs exceeded the two-family reader cap. Freshness and one fusion consumer remain, alongside two indexing controls and separate readiness, model-formatting and LSP components.
- Vite target: under six characters. useDefineForClassFields, jsxFactory and jsxFragmentFactory: removed to break repeated esbuild evidence; only jsxImportSource remains from jsx*. New manifest and root-discovery families use different assignments and readers. exports now asks about optimizer glob expansion, supported by that consumer's own comment. imports explicitly ends at the third-party resolver hand-off.
- Home Assistant mode and blueprint.name: under six characters. actions/triggers: removed because their old q2 evidence described concrete sequence content rather than key semantics. max_exceeded: removed to keep at most two families in motion_light.yaml and broaden components. New filter, REST, group and thermostat fixtures have separate evidence chains. logger had no YAML fixture in the pinned tests inspected; Python test dictionaries were not relabeled YAML.
- Prior speculative candidates (alwaysStrict, verbatimModuleSyntax, blueprint.author/source_url) remain unselected: no complete independent local assignment/semantic-doc/consumer chain was established. The set is curated and component-scoped, not a claim of all available settings.

## Family audit

### lci.index.freshness

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `src/app/search.rs`; no repository-wide completeness claim.

### lci.index.stale_check_interval_seconds

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `src/app/indexing.rs`; no repository-wide completeness claim.

### lci.index.wait_for_existing_job_seconds

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `src/app/indexing.rs`; no repository-wide completeness claim.

### lci.fusion.semantic_weight

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored. Config.fusion is passed as weights to fuse_hits; the arithmetic read is the consumer. Other ranking functions are outside scope.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `src/app/search.rs::fuse_hits`; no repository-wide completeness claim.

### lci.readiness_timeout_seconds

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored. service_status converts the value to Duration and supplies it to dependency probes; initialization probes in src/init.rs are outside scope.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `src/app/readiness.rs::service_status`; no repository-wide completeness claim.

### lci.embedding_query_template

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored. query_text fills the configured template; query then embeds that result.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `lci-core/src/models.rs::query_text`; no repository-wide completeness claim.

### lci.embedding_document_template

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored. Traversal formats a chunk using document_text before rescoring it. Index construction and identity checks are outside this reader scope.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `src/app/traverse/chunks.rs::score_chunks`; no repository-wide completeness claim.

### lci.rust_analyzer_trusted

Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored. Scope ends at the constructor hand-off to trusted adapter state; downstream initialization options are explained by the mapping witness, not claimed as an additional direct TOML read.

Assignment scope: `config.example.toml` only (assignment). Reader scope: `lci-core/src/lsp/rust.rs::RustAdapter::new (handoff to adapter state)`; no repository-wide completeness claim.

### vite.sideEffects

Searched sideEffects and followed JSON.parse -> destructuring -> boolean/array predicate. The glob comment explains behavior independently of the fixture. Downstream predicate calls and other modules are outside scope.

Assignment scope: `playground/html/side-effects/package.json` only (fixture). Reader scope: `packages/vite/src/node/packages.ts::loadPackageData`; no repository-wide completeness claim.

### vite.exports

Searched pkgData.data.exports and read expandGlobIds. The map is enumerated and matched with picomatch; source comments explicitly explain this consumer. Package entry resolution in plugins/resolve.ts is outside this question.

Assignment scope: `playground/resolve/exports-legacy-fallback/package.json` only (fixture). Reader scope: `packages/vite/src/node/optimizer/resolve.ts::expandGlobIds`; no repository-wide completeness claim.

### vite.compilerOptions.experimentalDecorators

Searched the leaf; resolveTsconfig -> loadedTsconfig.compilerOptions -> finite meaningfulFields list -> compilerOptionsForFile -> tsconfigRaw -> esbuild transform. Scope explicitly ends at the esbuild hand-off; native compiler semantics are outside the tree. The fixture is an Oxc test assignment, not a claim that its test invokes the compatibility path. Fixed-list indexing is resolved, not arbitrary dynamic lookup.

Assignment scope: `packages/vite/src/node/__tests__/plugins/fixtures/oxc-tsconfigs/decorator-metadata/tsconfig.json` only (fixture). Reader scope: `packages/vite/src/node/plugins/esbuild.ts::transformWithEsbuild (handoff to esbuild)`; no repository-wide completeness claim.

### vite.compilerOptions.jsxImportSource

Searched the leaf; resolveTsconfig -> loadedTsconfig.compilerOptions -> finite meaningfulFields list -> compilerOptionsForFile -> tsconfigRaw -> esbuild transform. Scope explicitly ends at the esbuild hand-off; native compiler semantics are outside the tree. The fixture is an Oxc test assignment, not a claim that its test invokes the compatibility path. Fixed-list indexing is resolved, not arbitrary dynamic lookup.

Assignment scope: `packages/vite/src/node/__tests__/plugins/fixtures/oxc-tsconfigs/jsx-react-jsx/tsconfig.json` only (fixture). Reader scope: `packages/vite/src/node/plugins/esbuild.ts::transformWithEsbuild (handoff to esbuild)`; no repository-wide completeness claim.

### vite.workspaces

Searched the literal field and read both JSON detection and searchForWorkspaceRoot. JSON.parse -> truthy field test -> upward traversal stop. The consumer tests presence/truthiness; it does not enumerate package members. Other tools interpreting membership are excluded.

Assignment scope: `packages/vite/src/node/server/__tests__/fixtures/yarn/package.json` only (fixture). Reader scope: `packages/vite/src/node/server/searchRoot.ts`; no repository-wide completeness claim.

### vite.workspace

Searched the literal field and read both JSON detection and searchForWorkspaceRoot. JSON.parse -> truthy field test -> upward traversal stop. The consumer tests presence/truthiness; it does not enumerate package members. Other tools interpreting membership are excluded.

Assignment scope: `packages/vite/src/node/server/__tests__/fixtures/deno/deno.json` only (fixture). Reader scope: `packages/vite/src/node/server/searchRoot.ts`; no repository-wide completeness claim.

### vite.browser

Searched browserField and mapWithBrowserField. resolveDeepImport destructures the map, maps the relative path and handles false by returning an external stub. Scope excludes package entry resolution and the separate relative-import hook. The helper enumerates map entries, not unknown top-level keys.

Assignment scope: `playground/resolve/browser-field/package.json` only (fixture). Reader scope: `packages/vite/src/node/plugins/resolve.ts::resolveDeepImport + mapWithBrowserField`; no repository-wide completeness claim.

### vite.imports

Searched resolveSubpathImports and the fixed imports discriminator. Scope ends at the resolve.exports library hand-off: the helper passes pkg and key to imports(), then normalizes the returned path. The third-party library implementation is not claimed as a local reader. Other manifest fields and entry resolution are excluded.

Assignment scope: `playground/resolve/package.json` only (fixture). Reader scope: `packages/vite/src/node/plugins/resolve.ts::resolveSubpathImports + resolveExportsOrImports (handoff to resolve.exports)`; no repository-wide completeness claim.

### home-assistant.command_line.binary_sensor.command_timeout

Searched command_timeout and CONF_COMMAND_TIMEOUT. The literal constant maps to binary_sensor_config -> CommandSensorData(timeout) -> self.timeout -> async_check_output_or_log(command, self.timeout) -> asyncio.timeout(timeout). The scored reader is the actual timed wait; witnesses preserve each hand-off. Other command platforms and the separate check-return-code helper are outside scope.

Assignment scope: `tests/components/command_line/fixtures/configuration.yaml` only (fixture). Reader scope: `homeassistant/components/command_line/utils.py::async_check_output_or_log`; no repository-wide completeness claim.

### home-assistant.blueprint.domain

Searched CONF_DOMAIN and followed the nested blueprint mapping into the constructor comparison. Derived .domain reads and metadata serialization are excluded. This is an assignment in a shipped, installable blueprint, not merely an example.

Assignment scope: `homeassistant/components/automation/blueprints/motion_light.yaml` only (assignment). Reader scope: `homeassistant/components/blueprint/models.py::Blueprint.__init__`; no repository-wide completeness claim.

### home-assistant.blueprint.input.no_motion_wait.default

Searched CONF_DEFAULT, inputs_with_default and no_motion_wait. Blueprint.inputs flattens nested input definitions; the property iterates missing names and takes each declared fallback. This is a default within a shipped blueprint, not an installation-wide default. Schema validation and downstream substitution are excluded.

Assignment scope: `homeassistant/components/automation/blueprints/motion_light.yaml` only (default). Reader scope: `homeassistant/components/blueprint/models.py::BlueprintInputs.inputs_with_default`; no repository-wide completeness claim.

### home-assistant.sensor.filters.window_size

Searched the leaf and CONF_FILTER constants. The YAML schema accepts filter dictionaries; FILTERS dispatch expands each dictionary as keyword arguments to the named filter constructor. OutlierFilter passes window_size to Filter; LowPassFilter passes precision to Filter.filter_precision, which filter_state passes to FilterState.set_precision. Scored scope is the actual buffer/rounding operation, excluding other filter implementations and config-flow migration. Sequence entries are transparent and the selected leaf occurs once in this YAML fixture.

Assignment scope: `tests/components/filter/fixtures/configuration.yaml` only (fixture). Reader scope: `homeassistant/components/filter/sensor.py::Filter.__init__`; no repository-wide completeness claim.

### home-assistant.sensor.filters.precision

Searched the leaf and CONF_FILTER constants. The YAML schema accepts filter dictionaries; FILTERS dispatch expands each dictionary as keyword arguments to the named filter constructor. OutlierFilter passes window_size to Filter; LowPassFilter passes precision to Filter.filter_precision, which filter_state passes to FilterState.set_precision. Scored scope is the actual buffer/rounding operation, excluding other filter implementations and config-flow migration. Sequence entries are transparent and the selected leaf occurs once in this YAML fixture.

Assignment scope: `tests/components/filter/fixtures/configuration.yaml` only (fixture). Reader scope: `homeassistant/components/filter/sensor.py::FilterState.set_precision`; no repository-wide completeness claim.

### home-assistant.rest.resource

Searched CONF_RESOURCE and _resource. create_rest_data_from_config reads the fixed constant, passes resource to RestData, and its constructor stores _resource. async_update consumes it as the request URL. The url property docstring independently identifies that attribute as the URL. Template-based URLs and legacy per-platform configuration are outside the selected top-level fixture scope.

Assignment scope: `tests/components/rest/fixtures/configuration_top_level.yaml` only (fixture). Reader scope: `homeassistant/components/rest/data.py::RestData.async_update`; no repository-wide completeness claim.

### home-assistant.sensor.entities

Searched CONF_ENTITIES and followed async_setup_platform -> SensorGroup(entity_ids) -> _entity_ids -> member state aggregation. Scope explicitly ends at the platform constructor hand-off. Config-entry and preview paths are excluded; those have independent inputs. The fixture has one sensor mapping.

Assignment scope: `tests/components/group/fixtures/sensor_configuration.yaml` only (fixture). Reader scope: `homeassistant/components/group/sensor.py::async_setup_platform (handoff to SensorGroup)`; no repository-wide completeness claim.

### home-assistant.climate.target_sensor

Searched CONF_SENSOR and sensor_entity_id. generic_thermostat/const.py maps target_sensor; setup passes it to GenericThermostat, which subscribes that entity to _async_sensor_changed. Its documented temperature-change callback updates current temperature and controls heating. Scope explicitly ends at the setup hand-off; runtime subscription is retained as mapping evidence.

Assignment scope: `tests/components/generic_thermostat/fixtures/configuration.yaml` only (fixture). Reader scope: `homeassistant/components/generic_thermostat/climate.py::async_setup_platform (handoff to GenericThermostat)`; no repository-wide completeness claim.

## Descriptive lockfile stratum (not a selection rule)

After finalizing the families, ran exactly `rg --no-config -n -w --hidden -g '!.git' --sort path --max-filesize 1M -- <last segment> .` from each tree root. Basenames are exactly PLAN section 2: `package-lock.json`, `npm-shrinkwrap.json`, `composer.lock`, `Cargo.lock`, `yarn.lock`, `pnpm-lock.yaml`, `Pipfile.lock`, `poetry.lock`, `uv.lock`, `bun.lock`, `bun.lockb`. Counts are matching output lines, not token occurrences. First 50 means the first 50 output lines of the unfiltered command. Default ignore handling remains in force. These measurements describe whether lockfile removal could move the A/B result; they do not select or reject families. Local generated gold may contribute non-lockfile LCI matches because the command is deliberately unchanged. Counts freeze the inspected working-tree state, not a clean-tree simulation.

| Family | All matched lines | Lockfile lines | Lockfile lines in first 50 |
| --- | ---: | ---: | ---: |
| lci.embedding_document_template | 59 | 0 | 0 |
| lci.embedding_query_template | 61 | 0 | 0 |
| lci.fusion.semantic_weight | 78 | 0 | 0 |
| lci.index.freshness | 218 | 0 | 0 |
| lci.index.stale_check_interval_seconds | 37 | 0 | 0 |
| lci.index.wait_for_existing_job_seconds | 27 | 0 | 0 |
| lci.readiness_timeout_seconds | 31 | 0 | 0 |
| lci.rust_analyzer_trusted | 34 | 0 | 0 |
| vite.browser | 501 | 64 | 0 |
| vite.compilerOptions.experimentalDecorators | 10 | 0 | 0 |
| vite.compilerOptions.jsxImportSource | 23 | 0 | 0 |
| vite.exports | 798 | 133 | 0 |
| vite.imports | 621 | 18 | 0 |
| vite.sideEffects | 19 | 0 | 0 |
| vite.workspace | 68 | 24 | 8 |
| vite.workspaces | 9 | 0 | 0 |
| home-assistant.blueprint.domain | 35534 | 0 | 0 |
| home-assistant.blueprint.input.no_motion_wait.default | 15716 | 0 | 0 |
| home-assistant.climate.target_sensor | 83 | 0 | 0 |
| home-assistant.command_line.binary_sensor.command_timeout | 22 | 0 | 0 |
| home-assistant.rest.resource | 761 | 0 | 0 |
| home-assistant.sensor.entities | 12116 | 0 | 0 |
| home-assistant.sensor.filters.precision | 537 | 0 | 0 |
| home-assistant.sensor.filters.window_size | 66 | 0 | 0 |
