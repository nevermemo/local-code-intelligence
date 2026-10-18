"""Build and verify structured gold from pinned Git blobs, without retrieval.
Run with python -I. Paths are command-line inputs, never written into artifacts.
Requires the already-installed PyYAML; no packages are installed by this script.
"""
import argparse, hashlib, json, pathlib, re, subprocess, sys, tomllib
import yaml

OUT = pathlib.Path(__file__).resolve().parents[1]
SEED = 20261018
REVS = {'lci':'579a5bec69309d98c78825654aabe8f03ab64421', 'vite':'7a89794331526a67bc9b29047b86957c581c128c', 'home-assistant':'bd92409b6ade6e2873cac603104f23e5d9e7ec61'}
p = argparse.ArgumentParser()
p.add_argument('--lci', required=True); p.add_argument('--vite-tree', required=True); p.add_argument('--vite-git', required=True)
p.add_argument('--ha-tree', required=True); p.add_argument('--ha-git', required=True)
p.add_argument('--rg', required=True)
p.add_argument('--check', action='store_true', help='Verify reproducibility without rewriting gold or notes')
a = p.parse_args()
roots = {'lci':pathlib.Path(a.lci), 'vite':pathlib.Path(a.vite_tree), 'home-assistant':pathlib.Path(a.ha_tree)}
gits = {'lci':['git','-C',a.lci], 'vite':['git','--git-dir='+a.vite_git], 'home-assistant':['git','--git-dir='+a.ha_git]}
assert subprocess.check_output(gits['lci']+['rev-parse','HEAD']).decode().strip() == REVS['lci']
cache={}; witnesses=[]; inventory=[]
def blob(repo,path):
    assert not pathlib.PurePosixPath(path).is_absolute() and '\\' not in path and '..' not in pathlib.PurePosixPath(path).parts
    if (repo,path) not in cache:
        b=subprocess.check_output(gits[repo]+['show',REVS[repo]+':'+path])
        oid=subprocess.check_output(gits[repo]+['rev-parse',REVS[repo]+':'+path]).decode().strip()
        assert hashlib.sha1(b'blob '+str(len(b)).encode()+b'\0'+b).hexdigest()==oid
        local=(roots[repo]/path).read_bytes()
        if repo=='lci': assert local.replace(b'\r\n',b'\n')==b.replace(b'\r\n',b'\n'), path
        else: assert local==b, 'Export blob mismatch: '+path
        cache[repo,path]=b
        inventory.append({'repo':repo,'path':path,'git_blob':oid,'sha256':hashlib.sha256(b).hexdigest(),'export_check':'exact' if repo!='lci' else 'line-ending-normalized'})
    return cache[repo,path]
def lines(repo,path): return blob(repo,path).decode('utf-8-sig').splitlines()
def locate(repo,path,needle,occurrence=0):
    hits=[i+1 for i,s in enumerate(lines(repo,path)) if needle in s]
    assert hits, (repo,path,needle)
    if occurrence==0: assert len(hits)==1, (repo,path,needle,hits)
    return hits[occurrence-1 if occurrence else 0]
def span(repo,path,start,end=None,how=None):
    ls=lines(repo,path)
    if isinstance(start,str): start=locate(repo,path,start)
    if isinstance(end,str): end=locate(repo,path,end)
    end=end or start
    assert 1<=start<=end<=len(ls), (path,start,end)
    result={'path':path,'line':start,'end_line':end,'sha256':hashlib.sha256(blob(repo,path)).hexdigest()}
    if how: result['how']=how
    witnesses.append(f'{repo}:{path}:{start}-{end}\n'+'\n'.join(f'{n}: {ls[n-1]}' for n in range(start,end+1)))
    return result

def descend(node,parts):
    # YAML sequences are transparent: these selected assignment questions identify
    # one named fixture/example, and each requested key must occur exactly once.
    if isinstance(node,yaml.SequenceNode):
        found=[]
        for child in node.value:
            try: found.append(descend(child,parts))
            except KeyError: pass
        assert len(found)==1, parts
        return found[0]
    if not isinstance(node,yaml.MappingNode): raise KeyError(parts)
    for k,v in node.value:
        if k.value==parts[0]:
            return (k,v) if len(parts)==1 else descend(v,parts[1:])
    raise KeyError(parts)
def assignment(repo,path,key,kind,fmt):
    text=blob(repo,path).decode('utf-8-sig')
    if fmt=='toml':
        value=tomllib.loads(text)
        for part in key.split('.'): value=value[part]
        table=''; hits=[]
        for n,line in enumerate(text.splitlines(),1):
            if re.fullmatch(r'\[[^\[\]]+\]',line.strip()): table=line.strip()[1:-1]
            if re.match(r'^'+re.escape(key.split('.')[-1])+r'\s*=',line) and table==key.rpartition('.')[0]: hits.append(n)
        assert len(hits)==1
        result=span(repo,path,hits[0]); val=text.splitlines()[hits[0]-1].split('=',1)[1].strip()
    else:
        if fmt=='json': json.loads(text)  # Deliberately reject JSONC.
        node=yaml.compose(text,Loader=yaml.SafeLoader) # Nodes only: !input tags never execute.
        k,v=descend(node,key.split('.'))
        # A block collection's end mark is the start of the next token (possibly a sibling at
        # a nonzero column), so take the last line that holds content of the node itself.
        seg=text[v.start_mark.index:v.end_mark.index].split('\n')
        keep_comments=isinstance(v,yaml.ScalarNode) and v.style in ('|','>')
        while len(seg)>1 and (not seg[-1].strip() or (not keep_comments and seg[-1].lstrip().startswith('#'))): seg.pop()
        end=v.start_mark.line+len(seg)
        result=span(repo,path,k.start_mark.line+1,max(k.start_mark.line+1,end))
        val=text[v.start_mark.index:v.end_mark.index].strip()
    result.update(value=val,kind=kind)
    return result

all_families={r:[] for r in REVS}; notes=[]
def add(repo,key,fmt,apath,kind,scope,reader_specs,doc_specs,behaviour,q2,note,excluded=()):
    fid=repo+'.'+key
    readers=[span(repo,*s) for s in reader_specs]
    docs=[span(repo,*s) for s in doc_specs]
    family={'id':fid,'split':'pending','format':fmt,'key':key,'assignments':[assignment(repo,apath,key,kind,fmt)],'readers':readers,'readers_scope':'component:'+scope,'excluded_readers':list(excluded),'behaviour':behaviour,'docs':docs}
    for part in key.split('.'):
        if len(part)>=4: assert part.lower() not in q2.lower(), (fid,part,q2)
    assert readers and docs
    family['_q2']=q2
    all_families[repo].append(family)
    notes.append(f'### {fid}\n\n{note}\n\nAssignment scope: `{apath}` only ({kind}). Reader scope: `{scope}`; no repository-wide completeness claim.\n')

for definition in ('families.py', 'finish_gold.py'):
    exec(compile((OUT/'_work'/definition).read_text(encoding='utf-8'), definition, 'exec'))
