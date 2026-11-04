from pathlib import Path
import sys
base,current=map(Path,sys.argv[1:])
def rows(p):
 return [x for x in (p/'results.tsv').read_text().splitlines() if not x.startswith('#')]
assert rows(base)==rows(current), 'regression per-file results changed'
a={p.name:p.read_bytes() for p in (base/'hunks').glob('*.diff')}
b={p.name:p.read_bytes() for p in (current/'hunks').glob('*.diff')}
assert a==b, 'regression differing hunks changed'
print('PASS: all per-file rows and both exact hunks match historical owner baseline')
