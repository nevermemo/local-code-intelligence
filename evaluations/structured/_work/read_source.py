"""Read-only revision inspection; accepts machine locations only as arguments."""
import argparse, subprocess, pathlib, hashlib
p=argparse.ArgumentParser(); p.add_argument('git_dir'); p.add_argument('revision'); p.add_argument('path'); p.add_argument('start',type=int,nargs='?',default=1); p.add_argument('end',type=int,nargs='?',default=100000); p.add_argument('--tree'); a=p.parse_args()
b=subprocess.check_output(['git','--git-dir='+a.git_dir,'show',a.revision+':'+a.path])
if a.tree:
    export=(pathlib.Path(a.tree)/a.path).read_bytes()
    assert export==b, 'Export differs from revision blob: '+a.path
print(a.path, hashlib.sha256(b).hexdigest())
for n,line in enumerate(b.decode('utf-8-sig').splitlines(),1):
    if a.start<=n<=a.end: print(f'{n}: {line}')
