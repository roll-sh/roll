#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
wt=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
cd "$wt"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/check171
mkdir -p "$ev" "$TMPDIR"
git -C "$wt" rev-parse HEAD > "$ev/source.txt"
git -C "$wt" diff --binary > "$ev/initial.patch"
test ! -s "$ev/initial.patch"
run() {
  local name=$1 status=0; shift
  printf '%q ' "$@" > "$ev/$name.command"; printf '\n' >> "$ev/$name.command"
  "$@" > "$ev/$name.log" 2>&1 || status=$?
  printf '%s\t%s\n' "$name" "$status" | tee -a "$ev/exits.tsv"
  test "$status" -eq 0
  if [[ "$name" == *test* ]]; then
    grep -q 'test result: ok' "$ev/$name.log"
    ! grep -Eq '^error|test result: FAILED|panicked at' "$ev/$name.log"
  fi
  test "$(git -C "$wt" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
  git -C "$wt" diff --binary > "$ev/current.patch"
  cmp "$ev/initial.patch" "$ev/current.patch"
}
test ! -s "$ev/exits.tsv"
run cost-test env GOLDEN_FILTER=a145_oltp_plans cargo test -p pg-engine --test golden -- --nocapture
run kept-test cargo test -p pg-engine --test kept_plans -- --nocapture
run debug-test cargo test -p pg-engine --no-fail-fast
run release-test cargo test --release -p pg-engine --no-fail-fast
run clippy cargo clippy --release -p pg-engine --all-targets
run build cargo build --profile dist -p pg-engine --bin roll-engine
cp "$CARGO_TARGET_DIR/dist/roll-engine" "$ev/roll-engine"
sha256sum "$ev/roll-engine" > "$ev/engine.sha256"
run postgres env ROLL_E2E_THREADS=2 pnpm test:pg
echo CHECK171-PASS
