#!/usr/bin/env python3
from pathlib import Path
import json,os,signal,time
v=Path(__file__).resolve().parent;binary='/home/meszmate/.cache/a162/pgdump/resume-3997cf00-dump5/roll-engine.bin';records=[]
def owned():
 result=[]
 for p in Path('/proc').iterdir():
  if not p.name.isdigit():continue
  try:args=(p/'cmdline').read_bytes().split(b'\0')
  except (FileNotFoundError,PermissionError,ProcessLookupError):continue
  if args and args[0].decode(errors='replace')==binary:result.append(int(p.name))
 return result
for pid in owned():
 records.append({'pid':pid,'binary':binary,'action':'SIGTERM exact fresh owned dump engine'})
 try:os.kill(pid,signal.SIGTERM)
 except ProcessLookupError:pass
deadline=time.monotonic()+30
while owned() and time.monotonic()<deadline:time.sleep(.2)
remaining=owned()
for pid in remaining:
 try:os.kill(pid,signal.SIGKILL)
 except ProcessLookupError:pass
 records.append({'pid':pid,'action':'SIGKILL exact owned engine after30s'})
if remaining:time.sleep(.2)
result={'status':'PASS' if not owned() else 'FAIL','owned_leftovers_before':len(records),'actions':records,'owned_processes_after':owned(),'containers_created':0,'existing_client_retained':'a162-dclient'}
(v/'cleanup.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));assert result['status']=='PASS'
