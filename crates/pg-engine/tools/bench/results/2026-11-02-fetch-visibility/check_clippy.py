"""Require no warnings beyond the retained all-target baseline."""
from pathlib import Path
import hashlib,json,re,sys
ROOT=Path(__file__).resolve().parent
BASE=Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-gates/clippy.log')
def warnings(path):
    text=Path(path).read_text()
    if not re.search(r'Finished `dev` profile',text):raise RuntimeError('Clippy did not finish: '+str(path))
    blocks=re.split(r'^warning: ',text,flags=re.M)[1:]
    result=[]
    for block in blocks:
        if block.startswith('`pg-engine`'):continue
        location=re.search(r'^\s*--> (.+?):\d+:\d+',block,re.M)
        if not location:raise RuntimeError('unlocated Clippy warning: '+block[:100])
        result.append((block.splitlines()[0],location[1]))
    return sorted(result)
def verify(path):
    before,now=warnings(BASE),warnings(path)
    if before!=now:raise RuntimeError('Clippy warnings differ from baseline: '+repr({'before':before,'now':now}))
    return {'passes':True,'baseline_log_sha256':hashlib.file_digest(BASE.open('rb'),'sha256').hexdigest(),'baseline_commit':'5b32046d','warning_count':len(now),'warnings':now,'limit':'Existing warnings permitted only with same count, messages and source paths as retained all-target baseline.'}
if __name__=='__main__':
    print(json.dumps(verify(Path(sys.argv[1])),indent=2))
