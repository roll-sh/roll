"""Prepare reviewable session-only correction in cache. Does not edit source or refs."""
from pathlib import Path
import difflib,subprocess,json,hashlib
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner')
W='/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536'
head='a6fe3e4e626b0552c14320c18211886d1b16ad38';base='c8618834caf45bc0a34cf9985c07f9bc13b498ef'
def git(*a):return subprocess.check_output(['git','-C',W,*a]).decode()
def source(ref,p):return git('show',ref+':'+p)
assert git('rev-parse','HEAD').strip()==head
assert not git('diff','--','crates/pg-engine/src')
paths=git('diff','--name-only',base,head).splitlines()
assert len(paths)==6
proposed={p:source(base,p) for p in paths}
p='crates/pg-engine/src/sql/paths.rs';current=source(head,p)
start=current.index('/// The plan\'s total cost as EXPLAIN shows it, from the cost the planner kept')
stop=current.index('/// A locked relation of the level',start)
helper=current[start:stop]
body=helper[helper.index('pub fn planned_total'):]
assert 'let (_, total) = s.est.0?;' in body
body=body.replace('    let (_, total) = s.est.0?;\n','').replace('    let cfg = parallel::ParCfg::read(ctx);','    let total = describe(ctx, s)?.total;\n    let cfg = parallel::ParCfg::read(ctx);')
helper='''/// The current total cost of a lone scan under column-only projections, where EXPLAIN
/// places no Gather. Other shapes keep the full EXPLAIN costing path. Read lazily when
/// a prepared plan choice first needs its cost, under that call's settings and estimates.
'''+body
marker='/// A locked relation of the level'
assert proposed[p].count(marker)==1
proposed[p]=proposed[p].replace(marker,helper+marker,1)
p='crates/pg-engine/src/session/mod.rs';s=source(head,p)
old='''                    let (t, found) = crate::sql::explain::total_and_gathers(&bq.plan, ctx);
                    let gathers = crate::exec::parallel_part::of_explain(&bq.plan, found);'''
new='''                    let (t, found) = match crate::sql::paths::planned_total(&bq.plan, ctx) {
                        Some(t) => (t, Vec::new()),
                        None => crate::sql::explain::total_and_gathers(&bq.plan, ctx),
                    };
                    let gathers = crate::exec::parallel_part::of_explain(&bq.plan, found);'''
assert s.count(old)==1;proposed[p]=s.replace(old,new)
patch=''.join(''.join(difflib.unified_diff(source(head,p).splitlines(True),s.splitlines(True),fromfile='a/'+p,tofile='b/'+p))for p,s in sorted(proposed.items()))
(D/'171-LAZY-PROPOSED-NOT-APPLIED.patch').write_text(patch)
summary=dict(status='CACHE ONLY; NOT APPLIED OR TESTED',pinned_source=head,base=base,correction_paths=list(sorted(proposed)),net_production_paths_vs_base=[p for p,s in sorted(proposed.items())if s!=source(base,p)],parallel_part_byte_identical=True,no_cache_generations=True,patch_sha256=hashlib.sha256(patch.encode()).hexdigest(),execution_hold='RootPGoracle and boundedc861control before production repair')
(D/'171-LAZY-PROPOSED-NOT-APPLIED.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(summary,indent=2))
