"""One-time round-two migration of the first-round builder and validator."""
from pathlib import Path
root=Path(__file__).resolve().parent
p=root/'build_gold.py'
s=p.read_text(encoding='utf-8')
if '# LCI:' in s:
 s=s[:s.index('# LCI:')].replace('pathlib, random, re','pathlib, re')
 s=s.replace("p.add_argument('--check'", "p.add_argument('--rg', required=True)\np.add_argument('--check'")
 s+="for definition in ('families.py', 'finish_gold.py'):\n    exec(compile((OUT/'_work'/definition).read_text(encoding='utf-8'), definition, 'exec'))\n"
 p.write_text(s,encoding='utf-8',newline='\n')
p=root/'validate_gold.py'
s=p.read_text(encoding='utf-8').replace('pathlib, random, re','pathlib, re')
start=s.index('    order=sorted(families)')
end=s.index('    for f in families.values():',start)
s=s[:start]+"    assert all(f['split']=='pending' for f in families.values())\n    reader_counts=collections.Counter()\n    assignments=[]\n"+s[end:]
s=s.replace("        assert f['split']==('dev' if f['id'] in order[:4] else 'heldout')", "        assert f['split']=='pending' and len(f['key'].split('.')[-1])>=6\n        reader_counts.update({s['path'] for s in f['readers']})\n        for span in f['assignments']:\n            assert all(span['path']!=other['path'] or span['end_line']<other['line'] or other['end_line']<span['line'] for other in assignments)\n            assignments.append(span)\n        assert all(d['path']!=a['path'] or d['end_line']<a['line'] or a['end_line']<d['line'] for d in f['docs'] for a in f['assignments'])")
s=s.replace('    by_family=collections.defaultdict(list)', "    assert max(reader_counts.values())<=2\n    if repo=='home-assistant': assert max(collections.Counter(a['path'] for a in assignments).values())<=2\n    by_family=collections.defaultdict(list)")
s=s.replace('4 dev / 4 heldout; schema and split valid','pending splits; schema and independence valid')
s+='\nfor path in ROOT.rglob("*"):\n    if path.is_file():\n        assert not re.search(r"(?<![A-Za-z])[A-Za-z]:[\\\\/]|/Us[e]rs/|/hom[e]/",path.read_text(encoding="utf-8")), path.name\nprint("Every structured artifact passes the machine-path scan.")\n'
p.write_text(s,encoding='utf-8',newline='\n')
