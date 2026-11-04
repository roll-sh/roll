#!/usr/bin/env python3
import json,pathlib,sys
p=pathlib.Path(sys.argv[1]);rows=[]
expected=['001_basic.pl','002_pg_dump.pl','004_pg_dump_parallel.pl','005_pg_dump_filterfile.pl']
for name in expected:
 f=p/f'tap-{name}.out'
 if not f.exists():raise SystemExit(f'MISSING TAP OUTPUT: {f}')
 lines=f.read_text(errors='replace').splitlines()
 good=sum(l.startswith('ok ') for l in lines);bad=sum(l.startswith('not ok ') for l in lines)
 rows.append({'file':name,'passed':good,'failed':bad})
result={'rows':rows,'passed':sum(r['passed'] for r in rows),'failed':sum(r['failed'] for r in rows),'expected_passed':14817}
(p/'outcome.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))
if result['passed']!=14817 or result['failed']!=0:raise SystemExit('TAP GATE FAILED: require 14817 passed, zero failed')
print('TAP GATE PASS: 14817/14817, zero failed')
