"""Validate schema v1 invariants independently of the gold builder (read-only)."""
import collections, importlib.util, json, pathlib, re, sys
sys.dont_write_bytecode=True  # keep the machine-path scan below free of .pyc files
ROOT=pathlib.Path(__file__).resolve().parents[1]
_spec=importlib.util.spec_from_file_location('structured_split',ROOT/'_work'/'split.py')
split_mod=importlib.util.module_from_spec(_spec); _spec.loader.exec_module(split_mod)  # the split algorithm xtask also checks
TYPES={'setting_location','behaviour_to_setting','setting_to_reader'}
TOP={'version','repo','revision','seed','families','questions'}
FAMILY={'id','split','format','key','assignments','readers','readers_scope','excluded_readers','behaviour','docs'}
BASE={'path','line','end_line','sha256'}
QUESTION={'id','family','type','text','names_key'}
def strict_object(pairs):
    out={}
    for k,v in pairs:
        assert k not in out, 'Duplicate JSON key: '+k
        out[k]=v
    return out
inventory=json.loads((ROOT/'_work/verification.json').read_text(encoding='utf-8'))
files={(f['repo'],f['path']):f for f in inventory['files']}
seen=set(); total=0
for repo in ('lci','vite','home-assistant'):
    raw=(ROOT/(repo+'.json')).read_text(encoding='utf-8')
    data=json.loads(raw,object_pairs_hook=strict_object)
    assert set(data)==TOP and data['repo']==repo and data['version']==1 and data['seed']==20261018
    assert re.fullmatch('[a-f0-9]{40}',data['revision'])
    assert len(data['families'])==8 and len(data['questions'])==24
    families={f['id']:f for f in data['families']}
    assert len(families)==8
    dev=set(split_mod.order(list(families))[:4])
    assert len(dev)==4 and all(f['split']==('dev' if i in dev else 'heldout') for i,f in families.items()), 'Split differs from the SplitMix64 assignment'
    reader_counts=collections.Counter()
    assignments=[]
    for f in families.values():
        assert set(f)==FAMILY and f['id']==repo+'.'+f['key']
        assert f['id'] not in seen; seen.add(f['id'])
        assert len(f['key'].split('.')[-1])>=6
        reader_counts.update({s['path'] for s in f['readers']})
        for span in f['assignments']:
            assert all(span['path']!=other['path'] or span['end_line']<other['line'] or other['end_line']<span['line'] for other in assignments)
            assignments.append(span)
        assert all(d['path']!=a['path'] or d['end_line']<a['line'] or a['end_line']<d['line'] for d in f['docs'] for a in f['assignments'])
        assert f['format'] in ('json','yaml','toml')
        assert f['readers_scope'].startswith('component:') or f['readers_scope']=='repo'
        assert f['assignments'] and f['readers'] and f['docs'] and f['behaviour']
        for group in ('assignments','readers','docs'):
            for s in f[group]:
                extra={'value','kind'} if group=='assignments' else {'how'} if group=='readers' else set()
                assert set(s)==BASE|extra
                assert 1<=s['line']<=s['end_line'] and type(s['line']) is int and type(s['end_line']) is int
                path=pathlib.PurePosixPath(s['path'])
                assert not path.is_absolute() and '..' not in path.parts and '\\' not in s['path'] and ':' not in s['path']
                assert re.fullmatch('[a-f0-9]{64}',s['sha256'])
                assert s['sha256']==files[repo,s['path']]['sha256']
                if group=='assignments':
                    assert s['kind'] in ('assignment','default','example','fixture') and isinstance(s['value'],str)
                    assert path.suffix in ('.json','.toml','.yaml','.yml')
                if group=='readers': assert s['how'] in ('literal','serde','ast','constant')
        for s in f['excluded_readers']:
            assert set(s)=={'path','line','reason'} and s['line']>=1 and s['reason']
            assert (repo,s['path']) in files
    assert max(reader_counts.values())<=2
    if repo=='home-assistant': assert max(collections.Counter(a['path'] for a in assignments).values())<=2
    by_family=collections.defaultdict(list)
    for q in data['questions']:
        assert set(q)==QUESTION and q['id'] not in seen
        seen.add(q['id']); by_family[q['family']].append(q)
        assert q['family'] in families and q['type'] in TYPES
        assert type(q['names_key']) is bool and q['text'].endswith('?')
        f=families[q['family']]
        if q['type']=='behaviour_to_setting':
            assert q['names_key'] is False
            for part in f['key'].split('.'):
                if len(part)>=4: assert part.lower() not in q['text'].lower()
        else:
            assert q['names_key'] is True and f['key'] in q['text']
        if q['type']=='setting_location':
            assert all(s['path'] not in q['text'] for s in f['assignments']), 'Question gives away the exact assignment path'
    for qs in by_family.values():
        assert len(qs)==3 and {q['type'] for q in qs}==TYPES
    assert not re.search(r'(?<![A-Za-z])[A-Za-z]:[\\/]|/Us[e]rs/|/hom[e]/',raw)
    total+=len(data['questions'])
    print(repo+': 8 families, 24 questions, 4 dev / 4 heldout (SplitMix64); schema and independence valid')
notes=(ROOT/'NOTES.md').read_text(encoding='utf-8')
assert not re.search(r'(?<![A-Za-z])[A-Za-z]:[\\/]|/Us[e]rs/|/hom[e]/',notes)
assert all('### '+fid in notes for fid in seen if not re.search(r'\.q[123]$',fid))
print(str(total)+' questions validated; names_key, evidence hashes, portable paths and family notes pass.')

for path in ROOT.rglob("*"):
    if path.is_file():
        assert not re.search(r"(?<![A-Za-z])[A-Za-z]:[\\/]|/Us[e]rs/|/hom[e]/",path.read_text(encoding="utf-8")), path.name
print("Every structured artifact passes the machine-path scan.")
