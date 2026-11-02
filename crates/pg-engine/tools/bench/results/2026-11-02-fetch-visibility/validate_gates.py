"""Exercise acceptance gates against real retained outputs and deliberately broken copies."""
from pathlib import Path
import importlib.util,json,shutil,sys,tempfile
sys.dont_write_bytecode=True
ROOT=Path(__file__).resolve().parent
BASE=Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-regress-5b32046d-release/report')
def load(name):
    spec=importlib.util.spec_from_file_location(name,ROOT/(name+'.py'));mod=importlib.util.module_from_spec(spec);spec.loader.exec_module(mod);return mod
regress=load('check_regression');clippy=load('check_clippy');checks=[]
with tempfile.TemporaryDirectory(prefix='fetch-gates-') as scratch:
    report=Path(scratch)/'report';report.mkdir()
    doc=(BASE/'REPORT.md').read_text();tsv=(BASE/'results.tsv').read_text()
    def write(d=doc,t=tsv):
        (report/'REPORT.md').write_text(d);(report/'results.tsv').write_text(t)
    def rejects(name,fn):
        try:fn()
        except RuntimeError:checks.append({'case':name,'correctly_rejected':True})
        else:raise RuntimeError('Gate accepted broken input: '+name)
    write();positive=regress.verify(report,BASE/'results.tsv')
    checks.append({'case':'real retained full release baseline','passes':positive['passes']})
    write(d=doc.replace('## Changes against the baseline run','## Missing comparison'))
    rejects('missing baseline section',lambda:regress.verify(report,BASE/'results.tsv'))
    write(d=doc.replace('Newly differing units: 0;','Newly differing units: 1;'))
    rejects('report says new regression',lambda:regress.verify(report,BASE/'results.tsv'))
    write(t=tsv.replace('# schedule=full','# schedule=subset'))
    rejects('subset report',lambda:regress.verify(report,BASE/'results.tsv'))
    write(t=tsv.replace('# engine_build=release','# engine_build=debug'))
    rejects('mismatched build profile',lambda:regress.verify(report,BASE/'results.tsv'))
    lines=tsv.splitlines();i=next(i for i,l in enumerate(lines) if l.startswith('test_setup\t'))
    fields=lines[i].split('\t');fields[-1]='1:wrong result';lines[i]='\t'.join(fields)
    write(t='\n'.join(lines)+'\n')
    rejects('TSV regression despite zero headline',lambda:regress.verify(report,BASE/'results.tsv'))
    write(t=tsv.replace('test_setup\t79\t79','test_setup\t78\t78'))
    rejects('changed compared unit count',lambda:regress.verify(report,BASE/'results.tsv'))
    write(t='\n'.join(l for l in tsv.splitlines() if not l.startswith('test_setup\t'))+'\n')
    rejects('omitted file',lambda:regress.verify(report,BASE/'results.tsv'))
    warning=Path(scratch)/'clippy.log';warning.write_text(clippy.BASE.read_text()+'\nwarning: invented new warning\n --> crates/pg-engine/src/exec/sizing.rs:1:1\n')
    rejects('additional Clippy warning',lambda:clippy.verify(warning))
(ROOT/'gate-validation.json').write_text(json.dumps(checks,indent=2)+'\n')
print(f'{len(checks)} acceptance gate checks passed.')
