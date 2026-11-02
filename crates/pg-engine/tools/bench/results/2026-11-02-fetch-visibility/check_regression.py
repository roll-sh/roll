"""Fail closed on missing, partial, mismatched-profile or newly differing regression units."""
from pathlib import Path
import csv,json,re

def check(value,message):
    if not value:raise RuntimeError(message)

def read_results(path):
    lines=Path(path).read_text().splitlines()
    metadata={k:v for line in lines if line.startswith('# ') and '=' in line for k,v in [line[2:].split('=',1)]}
    rows={row['file']:row for row in csv.DictReader([line for line in lines if not line.startswith('#')],delimiter='\t')}
    check(bool(rows),'empty regression results')
    return metadata,rows

def verify(report,baseline):
    report=Path(report)
    text=(report/'REPORT.md').read_text()
    check('## Changes against the baseline run\n' in text,'missing baseline comparison section')
    matches=re.findall(r'Newly differing units: (\d+);',text)
    check(matches==['0'],'new regression units or ambiguous comparison')
    current,rows=read_results(report/'results.tsv')
    before,base=read_results(baseline)
    for metadata in [current,before]:
        check(metadata.get('schedule')=='full','regression schedule is not full')
        check(metadata.get('engine_build')=='release','regression build is not release')
    check(rows.keys()==base.keys(),'regression files omitted or added')
    new=[]
    for name,row in rows.items():
        previous=base[name]
        check(row['units']==previous['units'],'changed compared unit count: '+name)
        check(row['engine_status']=='ok' and int(row['panics'])==0,'engine did not complete cleanly: '+name)
        gaps=lambda r:set(filter(None,r['differing'].split(',')))
        new.extend((name,gap) for gap in gaps(row)-gaps(previous))
    check(not new,'independently found new differing units: '+repr(new))
    return {'passes':True,'newly_differing_units':0,'files':len(rows),'units':sum(int(r['units']) for r in rows.values()),'builds':'release/release','schedules':'full/full','comparison':'retained historical baseline; zero newly differing units; existing differences not product compatibility acceptance'}
