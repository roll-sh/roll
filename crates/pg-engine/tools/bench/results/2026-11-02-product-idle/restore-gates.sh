#!/bin/bash
set -uo pipefail
cd /home/meszmate/worktrees/roll/t3-d4020b98
root=/home/meszmate/.cache/pg-engine-product-20261102
export TMPDIR="$root/tmp" CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
mkdir -p "$root/restored-gates"
git rev-parse HEAD > "$root/restored-gates/source.txt"
run_check() {
 name=$1
 shift
 timeout 7200 crates/pg-engine/tools/heavy.sh "$@" > "$root/restored-gates/$name.log" 2>&1
 result=$?
 printf '%s\t%s\n' "$name" "$result" >> "$root/restored-gates/status.tsv"
 if [ "$result" -ne 0 ]; then tail -35 "$root/restored-gates/$name.log"; fi
 return "$result"
}
run_check build cargo build --release -p pg-engine --bin roll-engine --example bench --example scan_bench || exit $?
python3 - <<'PY'
from pathlib import Path
from datetime import datetime,timezone
import hashlib,json,shutil,subprocess
root=Path('/home/meszmate/.cache/pg-engine-product-20261102')
out=root/'restored-bin';out.mkdir(exist_ok=False);shutil.copy2('target/release/roll-engine',out/'roll-engine')
digest=hashlib.sha256((out/'roll-engine').read_bytes()).hexdigest()
expected='5cfc79043a50d0ac228bdac3c66deb5fcf0dd34d73add389bf2773237e51ffd1'
r={'at_utc':datetime.now(timezone.utc).isoformat(),'source':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'binary_sha256':digest,'baseline_sha256':expected,'binary_identical_to_verified_baseline':digest==expected,'build_command':'cargo build --release -p pg-engine --bin roll-engine --example bench --example scan_bench'}
(root/'restored-build.json').write_text(json.dumps(r,indent=2)+'\n');assert digest==expected,r
PY
result=$?
printf 'identity\t%s\n' "$result" >> "$root/restored-gates/status.tsv"
[ "$result" -eq 0 ] || exit "$result"
run_check test cargo test --release -p pg-engine --no-fail-fast
run_check clippy cargo clippy -p pg-engine --all-targets
run_check pg env ROLL_E2E_BIND=0.0.0.0 ROLL_E2E_THREADS=2 pnpm test:pg --force
