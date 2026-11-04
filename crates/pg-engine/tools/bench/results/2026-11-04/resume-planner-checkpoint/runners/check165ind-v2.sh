#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/check165ind-v2
mkdir -p "$ev"
test ! -e "$ev/exits.tsv"
git -C "$W" rev-parse HEAD > "$ev/source.txt"
git -C "$W" rev-parse 'HEAD^{tree}' > "$ev/tree.txt"
git -C "$W" status --porcelain > "$ev/initial.status"
test ! -s "$ev/initial.status"
run() {
 local n=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 if [[ "$status" -ne 0 ]]; then return "$status"; fi
 if [[ "$n" == debug-test || "$n" == release-test ]]; then
  python3 /home/meszmate/.cache/pg-engine-resume-20261104/session_planner/audit165-summary.py "$ev/$n.log" /home/meszmate/.cache/pg-engine-resume-20261104/session_planner/ind165-expected-per-binary.json "$ev/$n-summary.json"
 fi
 if [[ "$n" == clippy || "$n" == build ]]; then
  grep -q 'Finished .*profile' "$ev/$n.log"
  if grep -Eq '^error|could not compile' "$ev/$n.log"; then return 1; fi
 fi
 if [[ "$n" == postgres-unit ]]; then
  grep -Eq '^postgres: (all green|no changes since the last green run; skipping)' "$ev/$n.log"
  if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 fi
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
 git -C "$W" status --porcelain > "$ev/current.status"
 cmp "$ev/initial.status" "$ev/current.status"
}
run debug-test cargo test -p pg-engine
run release-test cargo test --release -p pg-engine
run clippy cargo clippy --release -p pg-engine --all-targets
run build cargo build --profile dist -p pg-engine --bin roll-engine
mkdir -p /home/meszmate/.cache/a165/bin/independent
cp "$CARGO_TARGET_DIR/dist/roll-engine" /home/meszmate/.cache/a165/bin/independent/roll-engine
sha256sum /home/meszmate/.cache/a165/bin/independent/roll-engine > "$ev/engine.sha256"
run postgres-unit pnpm test:pg --unit
python3 - "$ev/postgres-unit.log" "$ev/postgres-unit-status.json" <<'UNIT'
import json,re,sys
from pathlib import Path
s=Path(sys.argv[1]).read_text()
skip='postgres: no changes since the last green run; skipping' in s
assert skip or 'postgres: all green' in s
r=dict(status='SKIP' if skip else 'PASS',executed_unit_checks=not skip,service_stack_acceptance=False,pg_engine_execution=False,reason='stamp matched; no checks executed' if skip else '--unit explicitly disables Docker e2e; separate final forced service-stack gate held')
Path(sys.argv[2]).write_text(json.dumps(r,indent=2)+'\n');print(json.dumps(r))
UNIT
echo CHECK165INDEPENDENT-PASS
