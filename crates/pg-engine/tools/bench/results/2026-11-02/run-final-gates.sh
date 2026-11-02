#!/bin/bash
set -euo pipefail
cd /home/meszmate/worktrees/roll/t3-d4020b98
export TMPDIR=/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/tmp
export CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
runroot=/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-gates
mkdir "$runroot"
git rev-parse HEAD > "$runroot/commit.txt"
git status --porcelain > "$runroot/git-status.txt"
test ! -s "$runroot/git-status.txt"
run_check() {
 name=$1
 shift
 set +e
 timeout 7200 crates/pg-engine/tools/heavy.sh "$@" > "$runroot/$name.log" 2>&1
 result=$?
 set -e
 printf '%s\t%s\n' "$name" "$result" >> "$runroot/status.tsv"
 if [ "$result" -ne 0 ]; then tail -60 "$runroot/$name.log"; return "$result"; fi
}
run_check build cargo build --release -p pg-engine --bin roll-engine --example bench --example scan_bench
run_check test cargo test --release -p pg-engine --no-fail-fast
run_check clippy cargo clippy -p pg-engine --all-targets
run_check pg-unit pnpm test:pg --unit
