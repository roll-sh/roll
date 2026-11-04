import pathlib as P,json,re,statistics as T,sys,hashlib
from build import R,stable

def lines(f):return [json.loads(x) for x in (R/f).read_text().splitlines()] if (R/f).exists() else []
def stats(v):return dict(n=len(v),median=T.median(v),min=min(v),max=max(v)) if v else None

def main():
 stable();results=lines('results.jsonl');windows=lines('windows.jsonl');commands=lines('commands.jsonl');correct=lines('correctness.jsonl')
 summary={};notes=[];gaps=[]
 for w in ['copy','pgbench','index']:
  summary[w]={}
  for side in ['baseline','candidate']:
   rows=[r for r in results if re.fullmatch(w+r'-[0-4]-'+side,r['tag'])]
   assert len(rows)==5,(w,side,len(rows))
   assert all(x['exit']==0 and not x.get('measurement_errors') for x in rows)
   summary[w][side]={k:stats([getter(r) for r in rows]) for k,getter in dict(wall_s=lambda r:r['wall'],server_cpu_s=lambda r:r['server_cpu']['usage_usec']/1e6,client_cpu_s=lambda r:r['client_cpu']['usage_usec']/1e6,server_wbytes=lambda r:r['server_io'].get('wbytes',0),server_rbytes=lambda r:r['server_io'].get('rbytes',0),peak_anon_shmem=lambda r:r['sampled_peak_anon_shmem'],disk_allocated=lambda r:r['disk_after']['allocated']).items()}
   assert all(r['server_before'].get('io_selected_device')=='9:2' for r in rows),'device identity'
  b=summary[w]['baseline'];c=summary[w]['candidate'];summary[w]['candidate_change_pct']={k:100*(c[k]['median']/b[k]['median']-1) for k in ['wall_s','server_cpu_s','client_cpu_s','peak_anon_shmem','disk_allocated'] if b[k]['median']}
  for metric in ['wall_s','server_cpu_s']:
   bs=b[metric];cs=c[metric]
   if cs['min']>bs['max']:notes.append(f'{w} {metric}: separated candidate regression')
   elif cs['max']<bs['min']:notes.append(f'{w} {metric}: separated candidate improvement')
   else:notes.append(f'{w} {metric}: ranges overlap; no measurable direction from conservative spread rule')
 footprint={}
 for side in ['baseline','candidate']:
  footprint[side]={}
  for state in ['fresh','loaded','100idle']:
   ws=[w for w in windows if re.fullmatch(f'footprint-{side}-{state}-[0-2]',w['tag'])]
   assert len(ws)==3,(side,state,len(ws))
   footprint[side][state]=dict(anon_shmem=stats([w['anon_shmem_median'] for w in ws]),cpu_cores=stats([w['server_cpu']['usage_usec']/1e6/w['duration'] for w in ws]),per_window=[dict(tag=w['tag'],duration=w['duration'],anon_shmem=w['anon_shmem_median'],server_cpu=w['server_cpu'],client_cpu=w['client_cpu']) for w in ws])
 profiles=[x for x in results if x.get('profile')]
 assert len(profiles)==6 and all(x['exit']==0 for x in profiles),len(profiles)
 for x in profiles:
  usage=x['server_cpu']['usage_usec']/1e6;client=x['client_cpu']['usage_usec']/1e6
  notes.append(f"{x['tag']}: untimed server CPU/wall={usage/x['wall']:.3f} cores, client={client/x['wall']:.3f}; see perf reports for source attribution")
 closes=lines('close-fixtures.jsonl');assert len(closes)==2
 topology={}
 for cf in closes:
  side=cf['side'];cases=[]
  for name,m in cf['before'].items():
   linked=[f for f in m['l1'] if 'commit_ts' in f];ordinary=[f for f in m['l1'] if 'commit_ts' not in f]
   between=[f for f in ordinary if any(l['last_key']<f['first_key'] for l in linked) and any(l['first_key']>f['last_key'] for l in linked)]
   cases.append(dict(manifest=name,l1_linked=len(linked),l1_ordinary=len(ordinary),ordinary_between_linked=between,l0=len(m['l0'])))
  topology[side]=cases
  if side=='candidate' and not any(x['ordinary_between_linked'] for x in cases):gaps.append('candidate far-close fixture did not prove linked L1 ranges surrounding an ordinary L1 file')
  notes.append(f"{side} close wall={cf['close']['wall']:.6f}s, removed/replaced input SST bytes={cf['rewritten_or_removed_bytes']}, sampled allocated high-water={cf['close']['sampled_disk_highwater']}; exit counters may be lower bounds")
 # Failure totals include negative controls separately, never discard failed workload attempts.
 negative=[c for c in commands if c['tag'].startswith('negative-')]
 failures=[c for c in commands if c['exit']!=0 and not c['tag'].startswith('negative-')]
 if failures:gaps.append(f'{len(failures)} non-control command failures (retained)')
 if (R/'RUN-FAIL').exists():gaps.append('runner failure retained; investigate separately')
 for state in ['fresh','loaded','100idle']:
  b=footprint['baseline'][state]['anon_shmem'];c=footprint['candidate'][state]['anon_shmem']
  if c['min']>b['max']:gaps.append(f'{state} anon+shmem footprint regression separated across window ranges')
 assert (R/'RUN-PASS').exists()
 out=dict(summary=summary,footprint=footprint,topology=topology,notes=notes,gaps=gaps,failures=failures,negative_controls=negative,correctness_checks=len(correct),correctness_failures=sum(x['failures'] for x in correct),profile_runs=len(profiles),timed_runs=30,builds=json.loads((R/'builds.json').read_text()),verdict='INCONCLUSIVE' if gaps else 'REQUIRES_PROFILE_REVIEW',limits=['before/after roll only','single bounded1M/scale10 dataset','sampled peaks are lower bounds','close final cgroup counters may disappear; flagged last sample is lower bound','first-character payload sum is a checksum, not full per-row digest','ranges alone do not establish equivalence margins','host interference retained in raw samples; profile attribution must be reviewed'])
 (R/'summary.json').write_text(json.dumps(out,indent=2)+'\n');print(json.dumps(out,indent=2))
if __name__=='__main__':main()
