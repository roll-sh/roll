from pathlib import Path
import json,hashlib
base=Path('/home/meszmate/.cache/a163/v/iso/out');rows=[]
for side in ['reference','engine']:
 p=base/side/'roll-bitmap-recheck.out';s=p.read_text()
 row={'side':side,'path':str(p),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'permutations':s.count('starting permutation:'),'waiting':s.count('<waiting ...>'),'bitmap_heap':s.count('Bitmap Heap Scan'),'bitmap_or':s.count('BitmapOr'),'error':('ERROR' in s or 'FATAL' in s)}
 rows.append(row)
 assert row['permutations']==3,row
 assert row['waiting']==3,row
 assert row['bitmap_heap']>=3 and row['bitmap_or']>=1,row
 assert not row['error'],row
 assert '99' in s
assert (base/'reference/roll-bitmap-recheck.out').read_bytes()==(base/'engine/roll-bitmap-recheck.out').read_bytes()
Path('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/index163_followup/bitmap-outcome-audit.json').write_text(json.dumps(rows,indent=2))
print(json.dumps(rows))
