#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/review165
mkdir -p "$ev/fixed"
test ! -e "$ev/exits.tsv"
files=(src/session/cursor.rs src/session/litplan.rs src/txn/locks.rs src/txn/twophase.rs)
restore() {
  for f in "${files[@]}"; do cp "$ev/fixed/${f//\//_}" "$E/$f"; done
}
for f in "${files[@]}"; do cp "$E/$f" "$ev/fixed/${f//\//_}"; done
trap restore EXIT
git -C "$W" rev-parse HEAD > "$ev/head.txt"
git -C "$W" diff --binary > "$ev/fixed-initial.patch"
sha256sum "$E/tests/sql/a165_cursor_stream.out" > "$ev/stream-before.sha256"
for f in "${files[@]}"; do git -C "$W" show "HEAD:crates/pg-engine/$f" > "$E/$f"; done
git -C "$W" diff --binary > "$ev/red-source.patch"
run() {
  local name=$1 expected=$2 status=0; shift 2
  printf '%q ' "$@" > "$ev/$name.command"; printf '\n' >> "$ev/$name.command"
  "$@" > "$ev/$name.log" 2>&1 || status=$?
  printf '%s\t%s\n' "$name" "$status" | tee -a "$ev/exits.tsv"
  if [[ "$expected" == pass ]]; then
    test "$status" -eq 0
    grep -q 'test result: ok' "$ev/$name.log"
    if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$name.log"; then return 1; fi
  else
    test "$status" -ne 0
    grep -q 'test result: FAILED' "$ev/$name.log"
    grep -q "$expected" "$ev/$name.log"
    if grep -Eq '^error\[|could not compile' "$ev/$name.log"; then return 1; fi
  fi
  test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/head.txt")"
}
run record pass env GOLDEN_RECORD=127.0.0.1:53310 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a165_cursor_ cargo test -p pg-engine --test golden -- --nocapture
sha256sum --check "$ev/stream-before.sha256"
if grep -q '^ERROR' "$E/tests/sql/a165_cursor_replay.out"; then exit 1; fi
cp "$E/tests/sql/a165_cursor_replay.out" "$ev/reference-replay.out"
cp "$E/tests/sql/a165_cursor_stream.out" "$ev/reference-stream.out"
run red-lock prepared_finish_preserves_reused_backend_lock_owners cargo test -p pg-engine --test twophase prepared_finish_preserves_reused_backend_lock_owners -- --nocapture
run red-cursor a165_cursor_replay env GOLDEN_FILTER=a165_cursor_replay cargo test -p pg-engine --test golden -- --nocapture
cp "$E/tests/sql/a165_cursor_replay.actual" "$ev/red-replay.actual"
run red-memory 'cursor retained row keys' cargo test -p pg-engine --test memory_load streaming_cursor_bounds_keys_and_backward_work -- --nocapture
restore
trap - EXIT
git -C "$W" diff --binary > "$ev/green-source.patch"
run green-lock pass cargo test -p pg-engine --test twophase -- --nocapture
run green-cursor pass env GOLDEN_FILTER=a165_cursor_ cargo test -p pg-engine --test golden -- --nocapture
run green-memory pass cargo test -p pg-engine --test memory_load streaming_cursor_bounds_keys_and_backward_work -- --nocapture
git -C "$W" diff --binary > "$ev/final.patch"
cmp "$ev/green-source.patch" "$ev/final.patch"
echo REVIEW165-FOCUSED-PASS
