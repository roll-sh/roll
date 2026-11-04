#!/usr/bin/env python3
"""Validate each Cargo binary's summary against a frozen, source-reconciled map."""
import json,re,sys
from pathlib import Path
log,expected_file,out=map(Path,sys.argv[1:])
s=log.read_text();expected=json.loads(expected_file.read_text());actual={};key=None
for line in s.splitlines():
 line=line.strip()
 if line.startswith('Running '):
  m=re.match(r'Running (?:unittests )?((?:src|tests)/\S+\.rs) ',line)
  assert m,line;key=m[1]
 elif line.startswith('Doc-tests '):key='doc-tests '+line.split(None,1)[1]
 m=re.search(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored',line)
 if m:
  assert key is not None and key not in actual,(key,line)
  actual[key]=list(map(int,m.groups()));key=None
summary=dict(passed=sum(v[0] for v in actual.values()),failed=sum(v[1] for v in actual.values()),ignored=sum(v[2] for v in actual.values()),summaries=len(actual))
result=dict(summary=summary,per_binary=actual)
out.write_text(json.dumps(result,indent=2)+'\n')
assert actual==expected,dict(missing=sorted(expected.keys()-actual.keys()),unexpected=sorted(actual.keys()-expected.keys()),differing={k:(expected[k],actual[k]) for k in expected.keys()&actual.keys() if actual[k]!=expected[k]})
assert not re.search(r'^error|test result: FAILED|panicked at',s,re.M)
print(json.dumps(summary))
