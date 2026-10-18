"""Artifact generation and exact premise-rg descriptive counts; builder context."""
from collections import Counter

header='''# Structured configuration gold: round 2

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

'''

outputs={}
for repo,families in all_families.items():
 assert len(families)==8
 families.sort(key=lambda x:x['id'])
 readers=Counter()
 used=[]
 questions=[]
 for f in families:
  assert f['split']=='pending' and len(f['key'].split('.')[-1])>=6
  readers.update({s['path'] for s in f['readers']})
  for s in f['assignments']:
   assert all(s['path']!=t['path'] or s['end_line']<t['line'] or t['end_line']<s['line'] for t in used)
   used.append(s)
  assert all(d['path']!=s['path'] or d['end_line']<s['line'] or s['end_line']<d['line'] for d in f['docs'] for s in f['assignments'])
  q2=f.pop('_q2'); scope=f['readers_scope'].removeprefix('component:')
  contexts={
   'lci':'the shipped sample configuration',
   'vite':{'sideEffects':'the HTML side-effects playground package','exports':'the legacy-entry-fallback resolution playground package','compilerOptions.experimentalDecorators':'the Oxc decorator-metadata fixture','compilerOptions.jsxImportSource':'the Oxc react-jsx fixture','workspaces':'the Yarn root-discovery fixture','workspace':'the Deno root-discovery fixture','browser':'the client-path-mapping resolution playground package','imports':'the main resolution playground manifest'},
   'home-assistant':{'command_line.binary_sensor.command_timeout':'the shell-backed binary-sensor YAML test fixture','blueprint.domain':'the shipped motion-light blueprint','blueprint.input.no_motion_wait.default':'the shipped motion-light blueprint','sensor.filters.window_size':'the outlier entry of the filter YAML test fixture','sensor.filters.precision':'the low-pass entry of the filter YAML test fixture','rest.resource':'the top-level REST YAML test fixture','sensor.entities':'the numeric sensor-group YAML test fixture','climate.target_sensor':'the generic thermostat YAML test fixture'}}
  context=contexts[repo] if repo=='lci' else contexts[repo][f['key']]
  prompts=[('setting_location',f"Where is `{f['key']}` set in {context}?",True),('behaviour_to_setting',q2,False),('setting_to_reader',f"What code in `{scope}` reads `{f['key']}`?",True)]
  for n,(typ,txt,names) in enumerate(prompts,1): questions.append({'id':f['id']+f'.q{n}','family':f['id'],'type':typ,'text':txt,'names_key':names})
 assert max(readers.values())<=2, readers
 if repo=='home-assistant': assert max(Counter(s['path'] for s in used).values())<=2
 outputs[repo+'.json']=json.dumps({'version':1,'repo':repo,'revision':REVS[repo],'seed':SEED,'families':families,'questions':questions},ensure_ascii=False,indent=2)+'\n'

locknames={'package-lock.json','npm-shrinkwrap.json','composer.lock','Cargo.lock','yarn.lock','pnpm-lock.yaml','Pipfile.lock','poetry.lock','uv.lock','bun.lock','bun.lockb'}
strata=[]
rg_version=subprocess.check_output([a.rg,'--version']).decode().splitlines()[0]
for repo,families in all_families.items():
 for f in families:
  leaf=f['key'].split('.')[-1]
  command=[a.rg,'--no-config','-n','-w','--hidden','-g','!.git','--sort','path','--max-filesize','1M','--',leaf,'.']
  result=subprocess.run(command,cwd=roots[repo],capture_output=True)
  assert result.returncode in (0,1), result.stderr.decode(errors='replace')
  matches=result.stdout.decode('utf-8').splitlines()
  flags=[pathlib.PurePosixPath(line.split(':',1)[0].replace('\\','/')).name in locknames for line in matches]
  strata.append({'family':f['id'],'matched_lines':len(matches),'lockfile_lines':sum(flags),'lockfile_lines_first_50':sum(flags[:50])})
table='''\n## Descriptive lockfile stratum (not a selection rule)

After finalizing the families, ran exactly `rg --no-config -n -w --hidden -g '!.git' --sort path --max-filesize 1M -- <last segment> .` from each tree root. Basenames are exactly PLAN section 2: `package-lock.json`, `npm-shrinkwrap.json`, `composer.lock`, `Cargo.lock`, `yarn.lock`, `pnpm-lock.yaml`, `Pipfile.lock`, `poetry.lock`, `uv.lock`, `bun.lock`, `bun.lockb`. Counts are matching output lines, not token occurrences. First 50 means the first 50 output lines of the unfiltered command. Default ignore handling remains in force. These measurements describe whether lockfile removal could move the A/B result; they do not select or reject families. Local generated gold may contribute non-lockfile LCI matches because the command is deliberately unchanged. Counts freeze the inspected working-tree state, not a clean-tree simulation.

| Family | All matched lines | Lockfile lines | Lockfile lines in first 50 |
| --- | ---: | ---: | ---: |
'''
for s in strata: table+=f"| {s['family']} | {s['matched_lines']} | {s['lockfile_lines']} | {s['lockfile_lines_first_50']} |\n"
outputs['NOTES.md']=header+'\n'.join(notes)+table
outputs['_work/witnesses.txt']='\n\n'.join(witnesses)+'\n'
outputs['_work/verification.json']=json.dumps({'python':sys.version.split()[0],'yaml':yaml.__version__,'seed':SEED,'split':'pending','rg_version':rg_version,'strata':strata,'files':sorted(inventory,key=lambda x:(x['repo'],x['path']))},indent=2)+'\n'
for name,text in outputs.items():
 assert not re.search(r'(?<![A-Za-z])[A-Za-z]:[\\/]|/Us[e]rs/|/hom[e]/',text), 'Nonportable artifact: '+name
 dest=OUT/name
 if a.check: assert dest.read_text(encoding='utf-8')==text, 'Reproduction mismatch: '+name
 else: dest.write_text(text,encoding='utf-8',newline='\n')
print(('Verified' if a.check else 'Wrote')+f': 24 families, 72 questions, all splits pending; {len(inventory)} revision files verified.')
