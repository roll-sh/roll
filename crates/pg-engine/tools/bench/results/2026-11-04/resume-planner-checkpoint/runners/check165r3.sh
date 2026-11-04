#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/check165r3
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
 test "$status" -eq 0
 if [[ "$n" == debug-test || "$n" == release-test ]]; then
  python3 - "$ev/$n.log" "$ev/$n-summary.json" <<'PY'
import json,re,sys
from pathlib import Path
s=Path(sys.argv[1]).read_text()
r=[tuple(map(int,v)) for v in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored',s)]
summary=dict(passed=sum(x[0] for x in r),failed=sum(x[1] for x in r),ignored=sum(x[2] for x in r),summaries=len(r))
Path(sys.argv[2]).write_text(json.dumps(summary,indent=2)+'\n')
assert summary==dict(passed=452,failed=0,ignored=18,summaries=60),summary
assert not re.search(r'^error|test result: FAILED|panicked at',s,re.M)
PY
 fi
 if [[ "$n" == cursor-compat ]]; then
  grep -q 'test result: ok. 3 passed; 0 failed' "$ev/$n.log"
  if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 fi
 if [[ "$n" == clippy || "$n" == build ]]; then
  grep -q 'Finished .*profile' "$ev/$n.log"
  if grep -Eq '^error|could not compile' "$ev/$n.log"; then return 1; fi
 fi
 if [[ "$n" == postgres ]]; then
  grep -Eq '^postgres: (all green|no changes since the last green run; skipping)' "$ev/$n.log"
  if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 fi
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
 git -C "$W" status --porcelain > "$ev/current.status"
 cmp "$ev/initial.status" "$ev/current.status"
}
run wrapper-selftest python3 /home/meszmate/.cache/pg-engine-resume-20261104/session_planner/wrapper-selftest.py
run cursor-compat env GOLDEN_FILTER=cursor cargo test -p pg-engine --test golden -- --nocapture
run debug-test cargo test -p pg-engine
run release-test cargo test --release -p pg-engine
run clippy cargo clippy --release -p pg-engine --all-targets
run build cargo build --profile dist -p pg-engine --bin roll-engine
mkdir -p /home/meszmate/.cache/a165/bin/resume
cp "$CARGO_TARGET_DIR/dist/roll-engine" /home/meszmate/.cache/a165/bin/resume/roll-engine
sha256sum /home/meszmate/.cache/a165/bin/resume/roll-engine > "$ev/engine.sha256"
run postgres env ROLL_E2E_THREADS=2 pnpm test:pg
echo CHECK165R3-PASS
