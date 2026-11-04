# Immutable launcher adds durable stability attestations without changing frozen runner.
import importlib.util, json, traceback, time, sys, os, pathlib as P
R=P.Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('storage_runner',R/'runner-v3.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
# Resolve the missing host perf executable and its dependencies entirely inside the owned lane.
original_run=m.run
perf_prefix=['/lib64/ld-linux-x86-64.so.2','--library-path',str(R/'tools/usr/lib/x86_64-linux-gnu'),str(R/'tools/usr/bin/perf')]
def owned_perf_run(cmd,*args,**kwargs):
    if 'perf' in cmd:
      i=cmd.index('perf');cmd=cmd[:i]+perf_prefix+cmd[i+1:]
      if 'record' in cmd:cmd.insert(cmd.index('record')+1,'--no-buildid-cache')
    return original_run(cmd,*args,**kwargs)
m.run=owned_perf_run
original=m.stable

def checked_stable():
    event=dict(at=time.time(),phase='source-binary-stability')
    try:
      original();event['passed']=True
      event['sources']={name:dict(head=commit,manifest_sha256=m.sha(R/(name+'.before.json'))) for name,src,commit,target in m.SIDES}
      event['binaries']={x['side']:dict(path=x['binary'],sha256=m.sha(x['binary'])) for x in json.loads((R/'builds.json').read_text())}
    except BaseException:
      event.update(passed=False,error=traceback.format_exc());raise
    finally:m.emit(R/'stability.jsonl',event)
m.stable=checked_stable
if __name__=='__main__':
  rc=0
  try:
    assert (R/'BUILD-PASS').exists()
    (R/'ancestor-controls.json').write_text(json.dumps({str(p):{n:(p/n).read_text() for n in ['cpu.max','cpu.weight','cpuset.cpus.effective','memory.max','memory.swap.max'] if (p/n).exists()} for p in [P.Path('/sys/fs/cgroup'),P.Path('/sys/fs/cgroup/system.slice')]},indent=2))
    m.main()
  except BaseException:
    traceback.print_exc();(R/'RUN-FAIL').write_text(traceback.format_exc());rc=1
  finally:
    try:
      m.cleanup();checked_stable()
      assert not m.OWN,m.OWN
    except BaseException:
      traceback.print_exc();rc=1
    if rc==0:
      (R/'RUN-PASS').write_text('all bounded planned invocations, validations, windows, closes, cleanup and source/binary stability passed\n')
    (R/'actual-runner-exit.json').write_text(json.dumps(dict(exit=rc,remaining_owned_ids=m.OWN.copy(),at=time.time(),runner_sha256=m.sha(R/'runner-v3.py'),launcher_sha256=m.sha(__file__))))
  sys.exit(rc)
