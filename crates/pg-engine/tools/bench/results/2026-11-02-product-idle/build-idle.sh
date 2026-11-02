#!/bin/bash
set -euo pipefail
cd /home/meszmate/worktrees/roll/t3-d4020b98
root=/home/meszmate/.cache/pg-engine-product-20261102
export TMPDIR="$root/tmp" CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
mkdir -p "$TMPDIR"
mkdir "$root/candidate-gates"
git rev-parse HEAD > "$root/candidate-gates/source.txt"
git status --porcelain > "$root/candidate-gates/git-status.txt"
test ! -s "$root/candidate-gates/git-status.txt"
run_check() {
 name=$1
 shift
 set +e
 timeout 7200 crates/pg-engine/tools/heavy.sh "$@" > "$root/candidate-gates/$name.log" 2>&1
 result=$?
 set -e
 printf '%s\t%s\n' "$name" "$result" >> "$root/candidate-gates/status.tsv"
 if [ "$result" -ne 0 ]; then tail -50 "$root/candidate-gates/$name.log"; return "$result"; fi
}
run_check build cargo build --release -p pg-engine --bin roll-engine --example bench --example scan_bench
run_check socket cargo test --release -p pg-engine --lib server::tests::idle_read_disarms_rearms_and_keeps_deadlines -- --exact --nocapture
run_check hook cargo test --release -p pg-engine --test wire idle_hook_runs_and_the_connection_goes_on -- --exact --nocapture
run_check waits cargo test --release -p pg-engine --test wire_waits waits_end_by_timeouts_and_cancel -- --exact --nocapture
python3 - <<'PY'
from pathlib import Path
from datetime import datetime,timezone
import hashlib,json,shutil,subprocess
root=Path('/home/meszmate/.cache/pg-engine-product-20261102');out=root/'candidate-bin';out.mkdir()
shutil.copy2('target/release/roll-engine',out/'roll-engine')
r={'at_utc':datetime.now(timezone.utc).isoformat(),'source':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'production_tree':subprocess.check_output(['git','rev-parse','HEAD:crates/pg-engine/src'],text=True).strip(),'binary_sha256':hashlib.sha256((out/'roll-engine').read_bytes()).hexdigest(),'rustc':subprocess.check_output(['rustc','--version','--verbose'],text=True),'build_command':'cargo build --release -p pg-engine --bin roll-engine --example bench --example scan_bench','git_status':subprocess.check_output(['git','status','--porcelain'],text=True)}
assert not r['git_status'];(out/'manifest.json').write_text(json.dumps(r,indent=2)+'\n')
print('Candidate release and focused wire checks passed',r['binary_sha256'])
PY
