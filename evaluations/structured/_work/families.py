"""Revision-pinned family definitions, executed by build_gold.py."""
cfg='lci-core/src/config.rs'; ex='config.example.toml'; ix='src/app/indexing.rs'; sr='src/app/search.rs'
base='Exact serde field names map TOML through Config and its nested configuration structs. Searched the leaf and qualified field; declarations, defaults, validation and tests are excluded. Only runtime reads in the named component are scored.'
add('lci','index.freshness','toml',ex,'assignment',sr,
 [(sr,'match self.config.index.freshness {',None,'serde')],[(cfg,11,24)],
 'Selects automatic search-time refresh, watcher-managed refresh, or explicit refresh.',
 'Which setting chooses whether searches refresh stale snapshots automatically or leave updates to explicit calls or the watcher?',base)
for key,doc,behavior,q in [
 ('stale_check_interval_seconds',(31,34),'Throttles filesystem staleness checks during on-search refresh.','Which setting controls the minimum time between filesystem staleness checks on repeated searches?'),
 ('wait_for_existing_job_seconds',(35,37),'Bounds how long search waits for an ongoing refresh before using the previous snapshot.','Which setting limits how long a search waits for an ongoing refresh before using the previous snapshot?')]:
 add('lci','index.'+key,'toml',ex,'assignment',ix,
  [(ix,'Duration::from_secs(self.config.index.'+key+')',None,'serde')],[(cfg,*doc)],behavior,q,base)
add('lci','fusion.semantic_weight','toml',ex,'assignment',sr+'::fuse_hits',
 [(sr,1560,1560,'serde')],[(cfg,389,390)],
 'Scales the vector channel reciprocal-rank contribution.',
 'Which setting scales the reciprocal-rank contribution of vector similarity results?',
 base+' Config.fusion is passed as weights to fuse_hits; the arithmetic read is the consumer. Other ranking functions are outside scope.')
ready='src/app/readiness.rs'
add('lci','readiness_timeout_seconds','toml',ex,'assignment',ready+'::service_status',
 [(ready,'let timeout = std::time::Duration::from_secs(self.config.readiness_timeout_seconds);',None,'serde')],
 [(ex,'# Bounded timeout for /ready and service_status dependency probes (1-30 seconds).')],
 'Bounds dependency probes used in the readiness report.',
 'Which setting bounds how long dependency probes may take when reporting whether the service is ready?',
 base+' service_status converts the value to Duration and supplies it to dependency probes; initialization probes in src/init.rs are outside scope.')
mdl='lci-core/src/models.rs'
add('lci','embedding_query_template','toml',ex,'assignment',mdl+'::query_text',
 [(mdl,'fill_template(&self.config.embedding_query_template',None,'serde')],[(cfg,484,488)],
 'Formats a search query before embedding, substituting its query placeholder without invalidating stored vectors.',
 'Which setting customizes the instruction text sent with a search request to the vector model without rebuilding stored vectors?',base+' query_text fills the configured template; query then embeds that result.')
add('lci','embedding_document_template','toml',ex,'assignment','src/app/traverse/chunks.rs::score_chunks',
 [('src/app/traverse/chunks.rs','let template = &self.app.config.embedding_document_template;',None,'serde')],[(cfg,489,496)],
 'Formats indexed chunks with source and optional path placeholders; changing it changes vector identity.',
 'Which setting customizes text sent for each indexed chunk, optionally including its repository-relative filename and requiring new vectors when changed?',base+' Traversal formats a chunk using document_text before rescoring it. Index construction and identity checks are outside this reader scope.')
rust='lci-core/src/lsp/rust.rs'
add('lci','rust_analyzer_trusted','toml',ex,'assignment',rust+'::RustAdapter::new (handoff to adapter state)',
 [(rust,'trusted: config.rust_analyzer_trusted,',None,'serde')],[(ex,'# Restore rust-analyzer\'s execution/network defaults only for trusted','# repositories and dependencies. Applies to every workspace this process uses.')],
 'Restores the language server execution and network defaults for trusted repositories and dependencies.',
 'Which switch restores the Rust language server execution and network defaults for repositories and dependencies you trust?',
 base+' Scope ends at the constructor hand-off to trusted adapter state; downstream initialization options are explained by the mapping witness, not claimed as an additional direct TOML read.')

pkg='packages/vite/src/node/packages.ts'; es='packages/vite/src/node/plugins/esbuild.ts'; rs='packages/vite/src/node/plugins/resolve.ts'; opt='packages/vite/src/node/optimizer/resolve.ts'
add('vite','sideEffects','json','playground/html/side-effects/package.json','fixture',pkg+'::loadPackageData',
 [(pkg,'const { sideEffects } = data','hasSideEffects = () => null','literal')],[(pkg,'const { sideEffects } = data','hasSideEffects = () => null')],
 'Builds a predicate indicating which package files must be retained for effects; false allows unused files to be discarded.',
 'Which manifest field marks files as safe to discard when unused, optionally selecting exceptions with glob patterns?',
 'Searched sideEffects and followed JSON.parse -> destructuring -> boolean/array predicate. The glob comment explains behavior independently of the fixture. Downstream predicate calls and other modules are outside scope.')
add('vite','exports','json','playground/resolve/exports-legacy-fallback/package.json','fixture',opt+'::expandGlobIds',
 [(opt,'const exports = pkgData.data.exports',None,'literal')],[(opt,'// if package has exports field, get all possible export paths and apply','// glob on them with picomatch')],
 'Provides the allowed package entry paths over which dependency-optimization include globs are expanded.',
 'Which manifest field supplies allowed package entry paths when the dependency optimizer expands a wildcard inclusion?',
 'Searched pkgData.data.exports and read expandGlobIds. The map is enumerated and matched with picomatch; source comments explicitly explain this consumer. Package entry resolution in plugins/resolve.ts is outside this question.')
fixture='packages/vite/src/node/__tests__/plugins/fixtures/oxc-tsconfigs/'
for key,ap,doc,behavior,q in [
 ('experimentalDecorators','decorator-metadata',[(es,256,267)],'Enables legacy parameter annotation transformation in the compatibility compiler.','Which TypeScript switch does the compatibility compiler recommend when it rejects legacy parameter annotations?'),
 ('jsxImportSource','jsx-react-jsx',[(es,185,192),('docs/guide/features.md',110,118)],'Supplies the provider for automatic JSX runtime helpers, unless direct transform options override it.','Which TypeScript setting selects the library providing automatic element-construction helpers unless a direct transform option overrides it?')]:
 add('vite','compilerOptions.'+key,'json',fixture+ap+'/tsconfig.json','fixture',es+'::transformWithEsbuild (handoff to esbuild)',
  [(es,160,165,'constant')],doc,behavior,q,
  'Searched the leaf; resolveTsconfig -> loadedTsconfig.compilerOptions -> finite meaningfulFields list -> compilerOptionsForFile -> tsconfigRaw -> esbuild transform. Scope explicitly ends at the esbuild hand-off; native compiler semantics are outside the tree. The fixture is an Oxc test assignment, not a claim that its test invokes the compatibility path. Fixed-list indexing is resolved, not arbitrary dynamic lookup.')
root='packages/vite/src/node/server/searchRoot.ts'
for key,ap,read,behavior,q,docs in [
 ('workspaces','yarn/package.json',[(root,'return !!content.workspaces',None,'literal')], 'Marks a package directory as the enclosing workspace root for file-serving boundary discovery.', 'Which manifest field makes a multi-package project directory the root used to determine the development server file-serving boundary?', [('docs/config/server-options.md',394,399)]),
 ('workspace','deno/deno.json',[(root,'if (content.workspace) return true',None,'literal')], 'Marks a Deno configuration directory as the enclosing workspace root during upward root discovery.', 'Which Deno configuration field makes upward project-root discovery stop at a multi-package directory?', [(root,' * Search up for the nearest workspace root','  if (hasWorkspaceDenoJSON(current)) return current')])]:
 add('vite',key,'json','packages/vite/src/node/server/__tests__/fixtures/'+ap,'fixture',root,read,docs,behavior,q,
  'Searched the literal field and read both JSON detection and searchForWorkspaceRoot. JSON.parse -> truthy field test -> upward traversal stop. The consumer tests presence/truthiness; it does not enumerate package members. Other tools interpreting membership are excluded.')
add('vite','browser','json','playground/resolve/browser-field/package.json','fixture',rs+'::resolveDeepImport + mapWithBrowserField',
 [(rs,'const { exports: exportsField, browser: browserField } = data',None,'literal'),(rs,'} else if (options.mainFields.includes(\'browser\') && isObject(browserField)) {','      return browserExternalId','literal')],
 [(rs,' * given a relative path in pkg dir,',' * - Returning `false` means this id is explicitly externalized for browser')],
 'Remaps package-relative module paths, with false marking a path as external for the client environment.',
 'Which manifest field replaces package-relative module paths for client builds and can disable individual modules with false?',
 'Searched browserField and mapWithBrowserField. resolveDeepImport destructures the map, maps the relative path and handles false by returning an external stub. Scope excludes package entry resolution and the separate relative-import hook. The helper enumerates map entries, not unknown top-level keys.')
add('vite','imports','json','playground/resolve/package.json','fixture',rs+'::resolveSubpathImports + resolveExportsOrImports (handoff to resolve.exports)',
 [(rs,'  let importsPath = resolveExportsOrImports(','    \'imports\',','constant'),(rs,'  const fn = type === \'imports\' ? imports : exports','  const result = fn(pkg, key, { conditions, unsafe: true })','constant')],
 [('docs/config/shared-options.md',658,658)],
 'Remaps package-internal specifiers through the manifest mapping; the resolver hands the map and active conditions to its resolution library.',
 'Which manifest field remaps package-internal hash-prefixed module specifiers?',
 'Searched resolveSubpathImports and the fixed imports discriminator. Scope ends at the resolve.exports library hand-off: the helper passes pkg and key to imports(), then normalizes the returned path. The third-party library implementation is not claimed as a local reader. Other manifest fields and entry resolution are excluded.')

binpath='homeassistant/components/command_line/binary_sensor.py'; sens='homeassistant/components/command_line/sensor.py'; utils='homeassistant/components/command_line/utils.py'; models='homeassistant/components/blueprint/models.py'; motion='homeassistant/components/automation/blueprints/motion_light.yaml'
add('home-assistant','command_line.binary_sensor.command_timeout','yaml','tests/components/command_line/fixtures/configuration.yaml','fixture',utils+'::async_check_output_or_log',
 [(utils,57,'            stdout, _ = await proc.communicate()','ast')],[(utils,'async def async_check_output_or_log','    """Run a shell command with a timeout and return the output."""')],
 'Limits the time allowed for the shell process to complete and produce output.',
 'Which YAML setting limits how long the shell-backed binary entity waits for its process to finish?',
 'Searched command_timeout and CONF_COMMAND_TIMEOUT. The literal constant maps to binary_sensor_config -> CommandSensorData(timeout) -> self.timeout -> async_check_output_or_log(command, self.timeout) -> asyncio.timeout(timeout). The scored reader is the actual timed wait; witnesses preserve each hand-off. Other command platforms and the separate check-return-code helper are outside scope.')
add('home-assistant','blueprint.domain','yaml',motion,'assignment',models+'::Blueprint.__init__',
 [(models,'# In future, we will treat this as "incorrect"','self.domain = data_domain','constant')],[(models,'# In future, we will treat this as "incorrect"','self.domain = data_domain')],
 'Declares the reusable configuration type, checked against the integration expected by the loader.',
 'Which metadata field declares the reusable recipe type checked against the integration loading it?',
 'Searched CONF_DOMAIN and followed the nested blueprint mapping into the constructor comparison. Derived .domain reads and metadata serialization are excluded. This is an assignment in a shipped, installable blueprint, not merely an example.')
add('home-assistant','blueprint.input.no_motion_wait.default','yaml',motion,'default',models+'::BlueprintInputs.inputs_with_default',
 [(models,'if isinstance(blueprint_input, dict) and CONF_DEFAULT in blueprint_input:','inputs_with_default[inp] = blueprint_input[CONF_DEFAULT]','constant')],
 [(models,'"""Return the inputs and fallback to defaults."""','return inputs_with_default')],
 'Supplies a declared input fallback when an instance omits that input; this shipped blueprint supplies 120 seconds for its wait duration.',
 'Which YAML setting supplies the declared fallback when a motion-light instance omits its wait duration?',
 'Searched CONF_DEFAULT, inputs_with_default and no_motion_wait. Blueprint.inputs flattens nested input definitions; the property iterates missing names and takes each declared fallback. This is a default within a shipped blueprint, not an installation-wide default. Schema validation and downstream substitution are excluded.')
flt='homeassistant/components/filter/sensor.py'; fc='homeassistant/components/filter/const.py'
for key,read,behavior,q in [
 ('window_size',[(flt,'            self.states: deque[FilterState] = deque(maxlen=window_size)',None,'ast')], 'Bounds how many previous readings the outlier filter retains in its sliding buffer.', 'Which YAML setting limits the number of previous readings retained by the outlier filter?'),
 ('precision',[(flt,'            value = round(float(self.state), precision)','            self.state = int(value) if precision == 0 else value','ast')], 'Rounds the filtered numeric reading to the configured number of decimal places.', 'Which YAML setting controls the number of decimal places in the filtered numeric reading?')]:
 add('home-assistant','sensor.filters.'+key,'yaml','tests/components/filter/fixtures/configuration.yaml','fixture',flt+('::Filter.__init__' if key=='window_size' else '::FilterState.set_precision'),read,
  [(flt,'        :param window_size: size of the sliding window that holds previous values','        :param entity: used for debugging only')],behavior,q,
  'Searched the leaf and CONF_FILTER constants. The YAML schema accepts filter dictionaries; FILTERS dispatch expands each dictionary as keyword arguments to the named filter constructor. OutlierFilter passes window_size to Filter; LowPassFilter passes precision to Filter.filter_precision, which filter_state passes to FilterState.set_precision. Scored scope is the actual buffer/rounding operation, excluding other filter implementations and config-flow migration. Sequence entries are transparent and the selected leaf occurs once in this YAML fixture.')
rest='homeassistant/components/rest/__init__.py'; rd='homeassistant/components/rest/data.py'
add('home-assistant','rest.resource','yaml','tests/components/rest/fixtures/configuration_top_level.yaml','fixture',rd+'::RestData.async_update',
 [(rd,'            async with self._session.request(','                self._method, self._resource, **request_kwargs','ast')],[(rd,'    def url(self) -> str:','        return self._resource')],
 'Supplies the URL requested to retrieve REST entity data.',
 'Which YAML setting supplies the URL polled to retrieve data for HTTP-backed entities?',
 'Searched CONF_RESOURCE and _resource. create_rest_data_from_config reads the fixed constant, passes resource to RestData, and its constructor stores _resource. async_update consumes it as the request URL. The url property docstring independently identifies that attribute as the URL. Template-based URLs and legacy per-platform configuration are outside the selected top-level fixture scope.')
grp='homeassistant/components/group/sensor.py'
add('home-assistant','sensor.entities','yaml','tests/components/group/fixtures/sensor_configuration.yaml','fixture',grp+'::async_setup_platform (handoff to SensorGroup)',
 [(grp, '                config[CONF_ENTITIES],',None,'constant')],[(grp,1,4),(grp,'    def async_update_group_state(self) -> None:','        self.calculate_state_attributes(self._get_valid_entities())')],
 'Lists the member entities whose numeric readings are combined into a sensor group.',
 'Which YAML setting lists the members whose numeric readings are combined into one aggregate value?',
 'Searched CONF_ENTITIES and followed async_setup_platform -> SensorGroup(entity_ids) -> _entity_ids -> member state aggregation. Scope explicitly ends at the platform constructor hand-off. Config-entry and preview paths are excluded; those have independent inputs. The fixture has one sensor mapping.')
therm='homeassistant/components/generic_thermostat/climate.py'
add('home-assistant','climate.target_sensor','yaml','tests/components/generic_thermostat/fixtures/configuration.yaml','fixture',therm+'::async_setup_platform (handoff to GenericThermostat)',
 [(therm,'    sensor_entity_id: str = config[CONF_SENSOR]',None,'constant')],[(therm,'    async def _async_sensor_changed(self, event: Event[EventStateChangedData]) -> None:',510)],
 'Selects the entity whose temperature updates drive thermostat heating control.',
 'Which YAML setting chooses the entity whose measured temperature drives heating control?',
 'Searched CONF_SENSOR and sensor_entity_id. generic_thermostat/const.py maps target_sensor; setup passes it to GenericThermostat, which subscribes that entity to _async_sensor_changed. Its documented temperature-change callback updates current temperature and controls heating. Scope explicitly ends at the setup hand-off; runtime subscription is retained as mapping evidence.')

# Revision-verified mapping witnesses supporting the scored consumer links.
mapping_specs={
 'lci':[(cfg,27,44),(cfg,386,394),(cfg,481,500),(cfg,519,524),(sr,1085,1095),(sr,1542,1561),(rust,19,45),(rust,65,140)],
 'vite':[(es,129,173),(es,185,207),(es,210,220),(root,23,54),(root,81,97),(rs,1036,1055),(opt,44,75)],
 'home-assistant':[('homeassistant/components/command_line/const.py',20,24),(binpath,49,72),(sens,'class CommandSensorData:','self.value = await async_check_output_or_log(command, self.timeout)'),(models,96,105),('homeassistant/components/blueprint/const.py',5,12),('homeassistant/const.py',117,117),('homeassistant/const.py',130,130),('homeassistant/const.py',178,183),(fc,1,36),(flt,60,110),(flt,140,190),(flt,456,523),(flt,638,653),(rest,189,232),(rd,26,49),(grp,100,129),(grp,190,230),('homeassistant/components/generic_thermostat/const.py',1,39),(therm,140,205),(therm,255,270),(therm,299,312)]}
for repo,specs in mapping_specs.items():
 for spec in specs: span(repo,*spec)
