#!/bin/bash
set -euo pipefail
cd /home/meszmate/worktrees/roll/t3-d4020b98
r=/home/meszmate/.cache/pg-engine-perf-20261102/btree-selectivity
export TMPDIR="$r/tmp" CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
mkdir "$r/build"
git rev-parse HEAD > "$r/build/source.txt"
git status --porcelain > "$r/build/git-status.txt"
test ! -s "$r/build/git-status.txt"
run_check() {
 name=$1
 shift
 set +e
 crates/pg-engine/tools/heavy.sh "$@" > "$r/build/$name.log" 2>&1
 code=$?
 set -e
 printf '%s\t%s\n' "$name" "$code" >> "$r/build/status.tsv"
 if [ "$code" -ne 0 ]; then tail -60 "$r/build/$name.log"; return "$code"; fi
}
run_check equivalence cargo test --release -p pg-engine --lib integer_btree_selectivity_reuse_is_bitwise_equal -- --nocapture
run_check release cargo build --release -p pg-engine --bin roll-engine --example bench --example scan_bench
python3 - <<'PY'
from pathlib import Path
from datetime import datetime,timezone
import hashlib,json,shutil,subprocess,os
r=Path('/home/meszmate/.cache/pg-engine-perf-20261102/btree-selectivity');out=r/'candidate-bin';out.mkdir()
source=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip();assert source==(r/'build/source.txt').read_text().strip()
assert subprocess.check_output(['git','status','--porcelain'])==b''
shutil.copy2('target/release/roll-engine',out/'roll-engine')
manifest={'source':source,'production_tree':subprocess.check_output(['git','rev-parse','HEAD:crates/pg-engine/src'],text=True).strip(),'at_utc':datetime.now(timezone.utc).isoformat(),'binary_sha256':hashlib.sha256((out/'roll-engine').read_bytes()).hexdigest(),'git_status':'','rustc':subprocess.check_output(['rustc','--version','--verbose'],text=True),'build_command':'cargo build --release -p pg-engine --bin roll-engine --example bench --example scan_bench','build_environment':{k:os.environ[k] for k in ['TMPDIR','CARGO_BUILD_JOBS','CARGO_PROFILE_DEV_DEBUG','CARGO_PROFILE_TEST_DEBUG','CARGO_PROFILE_RELEASE_DEBUG']}}
(out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps(manifest,indent=2))
PY
