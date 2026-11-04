#!/usr/bin/env python3
"""Require the complete eight-case binary array probe, allowing libtest's prefix."""
from pathlib import Path
import re,sys

def audit(stream,fixture):
    expected=[line for line in fixture.splitlines() if line.startswith(r'\x2 ')]
    if len(expected)!=8 or len(set(expected))!=8:
        raise ValueError('fixture must declare eight distinct cases')
    cases=re.findall(r'(?m)^(?:test probe \.\.\. )?> (\\x2 [^\n]+)$',stream)
    if cases!=expected:
        raise ValueError('missing, duplicate or reordered executed array cases')
    if '!!' in stream or re.search(r'FAILED|panicked at|^error:',stream,re.M):
        raise ValueError('failing stream marker')
    if len(re.findall(r'^test result: ok\. 1 passed; 0 failed; 0 ignored;.*$',stream,re.M))!=1:
        raise ValueError('missing or incomplete successful libtest summary')
    if not re.search(r'\n(?:ok|test probe \.\.\. ok)\n\ntest result: ok\.',stream):
        raise ValueError('missing completed probe result')
    return {'status':'PASS','distinct_cases':len(cases),'libtest_prefix':stream.count('test probe ... > ')}

if __name__=='__main__':
    import json
    try: print(json.dumps(audit(Path(sys.argv[1]).read_text(),Path(sys.argv[2]).read_text())))
    except (ValueError,IndexError) as exc:
        print('FAIL: '+str(exc),file=sys.stderr);sys.exit(1)
