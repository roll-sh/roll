#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
D=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner
ev="$D/probe165-low"
mkdir -p "$ev"
test ! -e "$ev/source.txt"
python3 - <<'PY'
import os
assert sorted(os.sched_getaffinity(0)) == [8,9,10,11,12,13,18,19]
assert os.getpriority(os.PRIO_PROCESS,0) == 19
assert os.environ['CARGO_BUILD_JOBS'] == '2'
assert int(os.environ['RUST_TEST_THREADS']) <= 2
print('STARTUP-CONTROLS-PASS affinity8-13,18-19 nice19 jobs2 testthreads<=2')
PY
git -C "$W" rev-parse HEAD > "$ev/source.txt"
git -C "$W" status --porcelain > "$ev/initial.status"
test ! -s "$ev/initial.status"
fixture="$W/crates/pg-engine/tests/a165_independent_low.rs"
test ! -e "$fixture"
cp "$D/a165_independent_low.rs" "$fixture"
sha256sum "$fixture" > "$ev/fixture.sha256"
cleanup() {
 sha256sum -c "$ev/fixture.sha256" || return 1
 rm "$fixture"
 git -C "$W" status --porcelain > "$ev/restored.status"
 cmp "$ev/initial.status" "$ev/restored.status"
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
 echo RESTORED > "$ev/RESTORED"
}
trap cleanup EXIT
run() {
 local name=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$name.command"; printf '\n' >> "$ev/$name.command"
 "$@" > "$ev/$name.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$name" "$status" | tee -a "$ev/exits.tsv"
 test "$status" -eq 0 || return "$status"
 grep -q 'test result: ok. 1 passed; 0 failed;' "$ev/$name.log"
}
run locks cargo test -p pg-engine --test a165_independent_low fast_own_transaction_waiters -- --nocapture --test-threads=1
run slot cargo test -p pg-engine --test a165_independent_low same_pid_old_session_end_preserves_new_idle_statistics -- --nocapture --test-threads=1
echo LOW165-PROBE-PASS
