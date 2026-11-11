from pathlib import Path
import datetime,hashlib,json
r=Path(__file__).resolve().parent
h=hashlib.md5();n=200000;owners=balances=0
for g in range(1,n+1):
 owner=g%50000;balance=g*7919%100000;owners+=owner;balances+=balance
 day=datetime.date(2024,1,1)+datetime.timedelta(days=g%365)
 if g>1:h.update(b',')
 h.update(f'{g}:{owner}:{balance}:note {g}:{day}'.encode())
expected=f'{n}|{n*(n+1)//2}|{owners}|{balances}|{h.hexdigest()}'
config=json.loads((r/'checked-baseline-vs-pg.json').read_text());assert expected==config['expected_fixture'],expected
(r/'independent-fixture.json').write_text(json.dumps({'rows':n,'expected':expected,'method':'Python integer arithmetic, datetime, incremental MD5 in ID order; no database input'},indent=2)+'\n')
print(expected)
