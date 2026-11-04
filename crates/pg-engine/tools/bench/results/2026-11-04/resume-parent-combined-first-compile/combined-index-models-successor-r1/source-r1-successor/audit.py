import hashlib,json,subprocess
from pathlib import Path
base='fe7b0023c9df19a6dae1e6067ba8454998db48c5'
root=Path('crates/pg-engine')
records=[]
def blob(ref,path): return subprocess.check_output(['git','show',f'{ref}:{root/path}'])
def sha(b): return hashlib.sha256(b).hexdigest()
def record(label,path,before,after):
 assert before==after,label
 records.append(dict(check=label,path=str(root/path),before_sha256=sha(before),after_sha256=sha(after),equal=True))
def region(s,start,end):
 a=s.index(start);b=s.index(end,a)+len(end);return s[a:b]
def same(label,path,start,end):
 record(label,path,region(blob(base,path),start.encode(),end.encode()),region((root/path).read_bytes(),start.encode(),end.encode()))
for path in ['src/storage/sst.rs','src/repl/decode.rs','src/types/binary.rs','tests/sql/a162_stats_dead.sql','tests/sql/a162_stats_dead.out']:
 record('untouched full blob',path,blob(base,path),(root/path).read_bytes())
same('COPY loads/loading fields','src/txn/mod.rs','    /// Loads into tables this transaction emptied,','    loading: Option<Box<Loading>>,')
same('COPY Loading type','src/txn/mod.rs','struct Loading {','    load: unchanged::Load,\n}')
# These complete function spans end before the following same-indent public method.
for fn in ['put_fresh','note_load','start_load']:
 path='src/txn/mod.rs'; before=blob(base,path); after=(root/path).read_bytes()
 def function(s):
  a=s.index(('    pub fn '+fn+'(').encode()); brace=s.index(b'{',a); depth=0
  for i in range(brace,len(s)):
   depth += (s[i]==123)-(s[i]==125)
   if depth==0: return s[a:i+1]
  raise ValueError(fn)
 record('COPY '+fn+' whole function',path,function(before),function(after))
for fn in ['read_once','in_read_once']:
 path='src/storage/mod.rs'; before=blob(base,path); after=(root/path).read_bytes()
 def function(s):
  a=s.index(('fn '+fn).encode()); brace=s.index(b'{',a); depth=0
  for i in range(brace,len(s)):
   depth += (s[i]==123)-(s[i]==125)
   if depth==0: return s[a:i+1]
  raise ValueError(fn)
 record('read-once '+fn+' function',path,function(before),function(after))
same('ANALYZE accepted dead-row correction','src/session/ddl/vacuum.rs',"                        // acquire_sample_rows' deadrows:", '                            ms(started),\n                        );')
same('DML accepted failed-insert correction','src/exec/dml.rs','                            // heap_insert counted it (pgstat_count_heap_insert);','                            return Err(index_violation(t, bix, v, other.as_deref(), ctx));')
for tip,pattern in [('483b4d6e5d41e9025aaa518d5774abd7971f4204','a163_'),('c66a076833291eaad99bdcfa7ed885ed2a68cb2c','a170_')]:
 for p in sorted((root/'tests/sql').glob(pattern+'*')):
  record('ported golden exact owner blob',str(p.relative_to(root)),blob(tip,str(p.relative_to(root))),p.read_bytes())
record('ported isolation exact owner blob','tests/isolation/specs/roll-bitmap-recheck.spec',blob('483b4d6e','tests/isolation/specs/roll-bitmap-recheck.spec'),(root/'tests/isolation/specs/roll-bitmap-recheck.spec').read_bytes())
report={'base':base,'checks':records,'scope':'Static byte comparison only; no build, tests, services or performance execution.'}
Path('/tmp/combined-index-models-d62e3d3b/source-r1-successor/preservation.json').write_text(json.dumps(report,indent=2)+'\n')
print(f'{len(records)} before/after byte checks recorded')
