#!/usr/bin/env python3
"""Preserve the active release check; stop only superseded subsequent owned work."""
from pathlib import Path
import json, os, signal, time
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner')
E=D/'check165fixed'
ROOT=3651715
WRAPPER=3651674

def live(pid):
    try:return Path(f'/proc/{pid}/stat').read_text().rpartition(') ')[2].split()[0]!='Z'
    except FileNotFoundError:return False

def descendants():
    rows={}
    for p in Path('/proc').iterdir():
        if not p.name.isdigit():continue
        try:
            stat=(p/'stat').read_text().rpartition(') ')[2].split()
            rows[int(p.name)]=(int(stat[1]),stat[19],(p/'cmdline').read_bytes())
        except (FileNotFoundError,ProcessLookupError):pass
    ids={ROOT}
    for _ in range(12):ids.update(pid for pid,v in rows.items() if v[0] in ids)
    return {pid:rows[pid] for pid in ids if pid in rows}

end=time.monotonic()+7200
while time.monotonic()<end:
    if not live(ROOT):
        (D/'stop165-followon.status').write_text('ALREADY_TERMINAL\n');break
    summary=E/'release-test-summary.json'
    if summary.exists():
        result=json.loads(summary.read_text())
        assert result==dict(passed=448,failed=0,ignored=18,summaries=60),result
        assert 'release-test\t0\n' in (E/'exits.tsv').read_text()
        rows=descendants()
        assert b'/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/check165fixed.sh' in rows[ROOT][2]
        record=dict(status='SUPERSEDED',stage='follow-on-clippy/dist/postgres',
                    reason='R2 confirms local cursor predicate coverage gap; retain completed debug/release, avoid obsolete subsequent work before bounded actualPG probe.',
                    completed_release=result,owned_pids=list(rows))
        (E/'SUPERSEDED.json').write_text(json.dumps(record,indent=2)+'\n')
        # Each process is from this verified owned ancestry; check its start identity again.
        for pid,(_,start,_) in rows.items():
            try:
                if Path(f'/proc/{pid}/stat').read_text().rpartition(') ')[2].split()[19]==start:os.kill(pid,signal.SIGTERM)
            except (FileNotFoundError,ProcessLookupError):pass
        for _ in range(100):
            remaining=[pid for pid in [*rows,WRAPPER,3651681] if live(pid)]
            if not remaining:break
            time.sleep(.1)
        (D/'stop165-followon.status').write_text('STOPPED\n' if not remaining else 'STILL_LIVE '+repr(remaining)+'\n')
        assert not remaining,remaining
        break
    time.sleep(5)
else:
    (D/'stop165-followon.status').write_text('TIMED_OUT_NO_MUTATION\n')
