#!/bin/sh
# The optimisers' guard: origin's head against a previous head, on the difftest smoke and rounds
# 6-8 (tests/difftest/STATUS.md, "Guarding the optimisers"). Builds each commit's difftest binary
# from an exported tree of its own (a target dir per tree, removed after the build), runs them
# one after the other with the same seed partition (case file names depend on it), and prints
# the mismatch lines new on head and how many are gone.
#
#   tools/difftest-guard.sh <base commit> <head commit>
#
# Environment: GUARD_DIR (scratch: trees, targets, binaries, outputs; on disk), DIFFTEST_REF
# (a private reference, never roll-engine-ref), DIFFTEST_WIRE_PORTS (ports of your own),
# GUARD_SEEDS (default 6100-6249,7000-7149,8000-8149), GUARD_N (150). Builds go through heavy.sh.
set -eu
BASE=$1; HEAD=$2
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
D=${GUARD_DIR:?GUARD_DIR: a scratch directory on disk}
SEEDS=${GUARD_SEEDS:-6100-6249,7000-7149,8000-8149}
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=${TMPDIR:-$D/tmp}
mkdir -p "$D/bin" "$D/out" "$TMPDIR"
bin() { # bin <commit>: its difftest binary, built once
  c=$(git -C "$REPO" rev-parse --short=8 "$1")
  if [ ! -x "$D/bin/dt-$c" ]; then
    rm -rf "$D/tree-$c"; mkdir -p "$D/tree-$c"
    git -C "$REPO" archive "$c" | tar -x -C "$D/tree-$c"
    (cd "$D/tree-$c" && CARGO_TARGET_DIR=$D/target-$c crates/pg-engine/tools/heavy.sh \
      cargo test --release -p pg-engine --test difftest --no-run >"$D/out/build-$c.log" 2>&1)
    cp "$(ls -t "$D"/target-"$c"/release/deps/difftest-* | grep -v '\.d$' | head -1)" "$D/bin/dt-$c"
    # (The tree stays: the smoke reads its smoke_known.txt from there.)
    rm -rf "$D/target-$c"
  fi
  echo "$D/bin/dt-$c"
}
run() { # run <binary> <label> <test> [seeds]
  o=$D/out/$2; rm -rf "$o"; mkdir -p "$o"
  (cd "$REPO/crates/pg-engine" && env DIFFTEST_DB="guard_$2" DIFFTEST_OUT="$o" ${4:+DIFFTEST_SEEDS="$4"} \
    DIFFTEST_N="${GUARD_N:-150}" RUST_TEST_THREADS=1 nice -n 19 "$1" --ignored --exact "$3" --nocapture >"$o/run.log" 2>&1) || true
  grep -h '^  \[' "$o/run.log" | sort -u >"$o/lines" || true
}
B=$(bin "$BASE"); H=$(bin "$HEAD")
run "$H" head_smoke smoke
echo "smoke: $(grep "^test result" "$D/out/head_smoke/run.log" | tail -1)"
run "$B" base run "$SEEDS"
run "$H" head run "$SEEDS"
grep '^difftest:' "$D/out/head/run.log" | tail -1
echo "new on head:"; comm -13 "$D/out/base/lines" "$D/out/head/lines"
echo "gone on head:"; comm -23 "$D/out/base/lines" "$D/out/head/lines" | wc -l
