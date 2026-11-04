#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/check165
mkdir -p "$ev"
git -C "$W" rev-parse HEAD > "$ev/source.txt"
git -C "$W" diff --binary > "$ev/initial.patch"
test ! -s "$ev/initial.patch"
run() {
  local name=$1 status=0; shift
  printf '%q ' "$@" > "$ev/$name.command"; printf '\n' >> "$ev/$name.command"
  "$@" > "$ev/$name.log" 2>&1 || status=$?
  printf '%s\t%s\n' "$name" "$status" | tee -a "$ev/exits.tsv"
  test "$status" -eq 0
  if [[ "$name" == *test* || "$name" == record ]]; then
    grep -q 'test result: ok' "$ev/$name.log"
    if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$name.log"; then return 1; fi
  fi
  test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
}
: > "$ev/exits.tsv"
run record env GOLDEN_RECORD=127.0.0.1:53310 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a165_cursor_stream cargo test -p pg-engine --test golden -- --nocapture
test -s crates/pg-engine/tests/sql/a165_cursor_stream.out
sha256sum crates/pg-engine/tests/sql/a165_cursor_stream.out > "$ev/record.sha256"
git -C "$W" diff --binary > "$ev/record.patch"
run debug-test cargo test -p pg-engine --no-fail-fast
run release-test cargo test --release -p pg-engine --no-fail-fast
run clippy cargo clippy --release -p pg-engine --all-targets
run build cargo build --profile dist -p pg-engine --bin roll-engine
mkdir -p /home/meszmate/.cache/a165/bin/resume
cp "$CARGO_TARGET_DIR/dist/roll-engine" /home/meszmate/.cache/a165/bin/resume/roll-engine
sha256sum /home/meszmate/.cache/a165/bin/resume/roll-engine > "$ev/engine.sha256"
run postgres env PATH="$PATH" pnpm test:pg
git -C "$W" diff --binary > "$ev/final.patch"
cmp "$ev/record.patch" "$ev/final.patch"
echo CHECK165-PASS
