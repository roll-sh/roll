#!/bin/sh
# The verification train's full suite (tests/difftest/STATUS.md, "Verification train") on one
# checkout. tools/batch-train.sh runs it for every candidate at once; agents run
# tools/quick-check.sh and don't start it themselves. By hand, as one heavy job:
#
#   tools/heavy-bg.sh ~/.cache/aNNN/train.log env TRAIN_DIR=~/.cache/aNNN/train TRAIN_PREFIX=aNNN \
#     tools/verification-train.sh
#
# (under `sg docker`). Each build is made once and its test binaries run directly by every stage
# that needs them, three lanes side by side (each lane's stages in order):
#   build    `--profile verify` (release without LTO) every test binary, crash_random again with
#            `--features powerloss`, and `--profile verify-checked` (debug assertions and overflow
#            checks at release speed; it replaced the debug build); clippy alongside
#   lane 1   tests: every crate test binary (verify); crash: crash_random seeds 1-30 x 4 rounds;
#            power: the power_cut tests, then seeds 1-30 x 4 with CRASH_POWER=1, with and without
#            CRASH_ASYNC=1 (HANDOFF "Crash recovery", power loss)
#   lane 2   checked: every crate test binary (verify-checked)
#   lane 3   regress: the full regression suite on a private reference (0 newly differing);
#            iso: isolation (1,647 / 1,648); dump: pg_dump_tap (14,609 passed)
# Results in $TRAIN_DIR/status, one line a stage: "<stage> <exit> <detail> <time>" (then
# "done 0"); the detail of a failed stage says why (the failing tests, the newly differing units,
# "engine did not start", a panic's message). Logs in $TRAIN_DIR/<stage>.log, the tests' per
# binary in $TRAIN_DIR/tests/; each failing test, one "<name>\t<message>" line, in
# $TRAIN_DIR/<stage>.fails (tools/train-why.py; the batch train's flake ledger reads them). A
# train clears the last one's logs first. A reference suite whose engine did not start (a port
# taken for a moment) runs again on a fresh port within the train (tools/ref-suites.sh): a line
# per such run in $TRAIN_DIR/infra.
# Containers: the train makes or starts them with tools/ref-containers.sh (<prefix>-rref, -iref
# on $TRAIN_ISO_REF's port, -psql, -dclient; $TRAIN_DIR/pgr/src-repo).
#
#   TRAIN_PORTS    preferred engine ports for regress, isolation, pg_dump; TRAIN_ISO_REF the
#                  isolation reference's (127.0.0.1:<port>). From tools/ports.sh (below the
#                  ephemeral range, free, held while the train runs): a preferred port that isn't
#                  is replaced; unset, the last train's ($TRAIN_DIR/ports) or any. $TRAIN_DIR/ports
#                  has the ones used.
#   TRAIN_STAGES   the stages to run (default all: build,clippy,tests,checked,crash,power,regress,iso,dump);
#                  build always runs for what the others need
#   TRAIN_TESTS    what tests and checked run (default every test binary; space-separated): a
#                  binary (lib for the library's unit tests, else a tests/*.rs target) or one test
#                  in it as the status lines name it, <binary>::<test> (lib::server::tests::x
#                  builds --lib and runs server::tests::x --exact; wire::protocol_cases builds
#                  --test wire and runs protocol_cases --exact)
#   TRAIN_PLAN=1   print the cargo targets and each binary's tests for TRAIN_TESTS, run nothing
#   TRAIN_THREADS  each test binary's threads (default heavy.sh's cap, else 6: two crate-test
#                  lanes on 12 CPUs)
#   TRAIN_NO_CHECKOUT=1  test the checkout as it is; otherwise it checks out origin's head when
#                  the checkout has no uncommitted changes
#   TRAIN_ENGINE_BIN=<roll-engine>  regress, isolation and pg_dump_tap against that binary (a PGO
#                  build, HANDOFF "Profile-guided build"), and with the crash stage
#                  tools/crash-binary.py SIGKILLs that binary under concurrent writes (seeds 1-3,
#                  7 rounds each: status line crash-binary, log crash-binary.log; ~4 min);
#                  TRAIN_PGO_PROFDATA=<.profdata> builds crash_random for crash and power with
#                  `--profile dist`, -Cprofile-use of it and tools/dist-flags.sh's flags (as the
#                  shipped binary is built)
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
TOOLS=$ROOT/crates/pg-engine/tools
cd "$ROOT" || exit 1
ALL=$(ls crates/pg-engine/tests/*.rs | sed 's,.*/,,; s,\.rs$,,')
SEL=${TRAIN_TESTS:-lib $ALL}
BINS=$(for s in $SEL; do echo "${s%%::*}"; done | awk '!seen[$0]++')
TARGETS=$(for s in $BINS; do [ "$s" = lib ] && echo --lib || echo "--test $s"; done)
# The tests TRAIN_TESTS names in binary $1 (none: all of it).
filters() {
  for s in $SEL; do [ "$s" = "$1" ] && return; done
  for s in $SEL; do case $s in "$1"::*) echo "${s#*::}" ;; esac; done
}
if [ -n "${TRAIN_PLAN:-}" ]; then
  echo "build:" $TARGETS
  for s in $BINS; do f=$(filters "$s"); echo "run: $s" ${f:+--exact $f}; done
  exit 0
fi
D=${TRAIN_DIR:?TRAIN_DIR: a scratch directory on disk}
P=${TRAIN_PREFIX:?TRAIN_PREFIX: the prefix of your containers}
STAGES=",${TRAIN_STAGES:-build,clippy,tests,checked,crash,power,regress,iso,dump},"
THREADS=${TRAIN_THREADS:-${RUST_TEST_THREADS:-6}}
mkdir -p "$D"
# Ports, held while this train runs (tools/ports.sh); the isolation reference keeps the port its
# container runs on when that one is safe.
pick() { "$TOOLS/ports.sh" take $$ 1 "$@"; }
set -- $(echo "${TRAIN_PORTS:-$(cut -d, -f1-3 "$D/ports" 2>/dev/null)}" | tr , ' ') '' '' ''
RPORT=$(pick $1) IPORT=$(pick $2) DPORT=$(pick $3)
IRUN=$("$TOOLS/ref-containers.sh" port "$P" 2>/dev/null)
IREF=$(pick ${IRUN:+=$IRUN} ${TRAIN_ISO_REF:+${TRAIN_ISO_REF##*:}} $(cut -d, -f4 "$D/ports" 2>/dev/null))
echo "$RPORT,$IPORT,$DPORT,$IREF" >"$D/ports"
export PATH="$HOME/.cargo/bin:$PATH"
# A fresh build each train: incremental caches would only cost time and disk.
export CARGO_INCREMENTAL=0 CARGO_PROFILE_VERIFY_CODEGEN_UNITS=16 CARGO_PROFILE_VERIFY_CHECKED_CODEGEN_UNITS=16
mkdir -p "$D/bin" "$D/pgr" "$D/iso" "$D/pgdump" "$D/tests"; : >"$D/status"
# The last train's logs go: a stage this one doesn't run leaves no stale log behind.
rm -rf "$D"/*.log "$D"/*.fails "$D/infra" "$D/tests" "$D/pgr/report" "$D/iso/report" "$D/pgdump/report" \
  "$D"/pgdump/tap-*.out "$D"/pgdump/*.log "$D/pgr/out/engine.log" "$D/iso/out/engine.log"
mkdir -p "$D/tests"
echo "ports: regress $RPORT, isolation $IPORT, pg_dump $DPORT, isolation reference $IREF"
st() { echo "$1 $2 $(date +%T)" >>"$D/status"; }
want() { case $STAGES in *",$1,"*) return 0 ;; esac; return 1; }
if [ -n "${TRAIN_ENGINE_BIN:-}" ]; then
  export PGREGRESS_ENGINE_BIN=$TRAIN_ENGINE_BIN PGISOLATION_ENGINE_BIN=$TRAIN_ENGINE_BIN PGDUMP_ENGINE_BIN=$TRAIN_ENGINE_BIN
  echo "engine-bin $(sha256sum "$TRAIN_ENGINE_BIN" | cut -c1-16) $(date +%T)" >>"$D/status"
fi
if [ -z "${TRAIN_NO_CHECKOUT:-}" ] && [ -z "$(git status --porcelain --untracked-files=no)" ]; then
  git fetch -q origin && git checkout -q --detach origin/docs/postgres-architecture-strategy
fi
H=$(git rev-parse --short=9 HEAD); st head "$H"

# --- build -----------------------------------------------------------------------------------
# "name path" of each test binary a cargo build made.
artifacts() { python3 -I -c '
import json, sys
for l in sys.stdin:
    if not l.startswith("{"): continue
    m = json.loads(l)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["profile"]["test"]:
        print("lib" if m["target"]["kind"][0] == "lib" else m["target"]["name"], m["executable"])
'; }
REFT="--test pg_regress --test pg_isolation --test pg_dump"
NEED=
want tests && NEED="$NEED $TARGETS"
{ want regress || want iso || want dump; } && NEED="$NEED $REFT"
{ want crash || want power; } && [ -z "${TRAIN_PGO_PROFDATA:-}" ] && NEED="$NEED --test crash_random"
POWER=0
want power && grep -q 'feature = "powerloss"' crates/pg-engine/tests/crash_random.rs && POWER=1
B0=$(date +%s)
if want clippy; then
  cargo clippy -p pg-engine --all-targets --message-format=short >"$D/clippy.log" 2>&1 &
  CLIPPY=$!
fi
if want checked; then
  cargo test --profile verify-checked -p pg-engine --no-run $TARGETS --message-format=json-render-diagnostics \
    2>"$D/build-checked.log" | artifacts >"$D/bins-checked" &
  CHECKED=$!
fi
: >"$D/bins"; rc=0
if [ -n "$NEED" ]; then
  cargo test --profile verify -p pg-engine --no-run $NEED --message-format=json-render-diagnostics \
    2>"$D/build.log" | artifacts >"$D/bins"
  grep -q '^error' "$D/build.log" && rc=1
fi
[ -n "${TRAIN_PGO_PROFDATA:-}" ] && . "$ROOT/crates/pg-engine/tools/dist-flags.sh"
if [ -n "${TRAIN_PGO_PROFDATA:-}" ] && { want crash || want power; }; then
  cargo test --profile dist -p pg-engine --no-run --test crash_random \
    --config "build.rustflags=['-Cprofile-use=$TRAIN_PGO_PROFDATA','-Cllvm-args=-no-pgo-warn-mismatch'${DIST_RUSTFLAGS:+,'$DIST_RUSTFLAGS'}]" \
    --message-format=json-render-diagnostics 2>>"$D/build.log" | artifacts >>"$D/bins"
fi
if [ $POWER = 1 ]; then
  if [ -n "${TRAIN_PGO_PROFDATA:-}" ]; then
    set -- --profile dist --config "build.rustflags=['-Cprofile-use=$TRAIN_PGO_PROFDATA','-Cllvm-args=-no-pgo-warn-mismatch'${DIST_RUSTFLAGS:+,'$DIST_RUSTFLAGS'}]"
  else
    set -- --profile verify
  fi
  cargo test "$@" -p pg-engine --features powerloss --no-run --test crash_random \
    --message-format=json-render-diagnostics 2>>"$D/build.log" | artifacts | sed 's/^crash_random /crash_random@power /' >>"$D/bins"
fi
if want checked; then
  wait "$CHECKED"
  grep -q '^error' "$D/build-checked.log" && rc=1
fi
st build "$rc $(( $(date +%s) - B0 ))s"
D8=$(git rev-parse --short=8 HEAD)
DT=$(awk '$1 == "difftest" {print $2}' "$D/bins")
[ -n "$DT" ] && cp "$DT" "$D/bin/dt-$D8"
if want clippy; then
  wait "$CLIPPY"; rc=$?
  grep ': warning: ' "$D/clippy.log" >"$D/clippy-warnings.txt"
  st clippy "$rc $(wc -l <"$D/clippy-warnings.txt")w"
fi
bin() { awk -v n="$1" '$1 == n {print $2}' "$2"; }
# Run test binaries (from the package's directory, as cargo does), all of them however many
# fail: "<stage> <exit> <failed binary::test ...> <seconds>s".
crate_tests() {
  stage=$1 bins=$2 t=$(date +%s) bad=
  for s in $BINS; do
    b=$(bin "$s" "$bins"); [ -n "$b" ] || { bad="$bad $s:not-built"; continue; }
    f=$(filters "$s")
    (cd crates/pg-engine && RUST_TEST_THREADS=$THREADS "$b" ${f:+--exact $f}) >"$D/tests/$stage-$s.log" 2>&1 || {
      python3 -I "$TOOLS/train-why.py" libtest "$s" "$D/tests/$stage-$s.log" "$D/$stage.fails"
      bad="$bad $(grep -E '^test .* FAILED$' "$D/tests/$stage-$s.log" | awk -v s="$s" '{printf "%s::%s ", s, $2}')$(grep -q '^test .* FAILED$' "$D/tests/$stage-$s.log" || echo "$s:exit")"
    }
  done
  cat "$D/tests/$stage"-*.log >"$D/$stage.log" 2>/dev/null
  st "$stage" "$([ -z "$bad" ] && echo 0 || echo 101) $(( $(date +%s) - t ))s$bad"
}
run() { b=$(bin "$1" "$D/bins"); shift; [ -n "$b" ] || { echo "not built"; return 1; }; (cd crates/pg-engine && "$b" "$@"); }

# --- lane 3: the reference suites --------------------------------------------------------------
lane3() {
  { want regress || want iso || want dump; } || return 0
  "$TOOLS/ref-containers.sh" up "$P" "$D" "$IREF" >"$D/containers.log" 2>&1
  rc=$?
  if [ $rc = 3 ] && new=$(pick); then
    # The isolation reference's port was taken: another one.
    echo "containers: $(tail -1 "$D/containers.log"); the isolation reference on port $new" >>"$D/infra"
    IREF=$new; echo "$RPORT,$IPORT,$DPORT,$IREF" >"$D/ports"
    "$TOOLS/ref-containers.sh" up "$P" "$D" "$IREF" >>"$D/containers.log" 2>&1; rc=$?
  fi
  [ $rc = 0 ] || echo "containers: $(tail -1 "$D/containers.log")" >>"$D/status"
  . "$TOOLS/ref-suites.sh"
  if want regress; then
    [ -d "$D/pgr/src-repo" ] || echo "no $D/pgr/src-repo: the regress harness clones it" >>"$D/status"
    ref_suite regress "$RPORT" "$IREF"
    st regress "$([ $REF_OK = 1 ] && echo 0 || echo "$REF_RC") $REF_DETAIL"
  fi
  if want iso; then
    ref_suite iso "$IPORT" "$IREF"
    st isolation "$([ $REF_OK = 1 ] && echo 0 || echo "$REF_RC") $REF_DETAIL"
  fi
  if want dump; then
    ref_suite dump "$DPORT" "$IREF"
    st dump "$([ $REF_OK = 1 ] && echo 0 || echo "$REF_RC") $REF_DETAIL"
  fi
}

# --- lane 1: release tests and the crash stages ------------------------------------------------
lane1() {
  want tests && crate_tests tests "$D/bins"
  SEEDS=$(seq -s, 1 30)
  if want crash; then
    CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 run crash_random >"$D/crash.log" 2>&1; rc=$?
    st crash "$rc$([ $rc = 0 ] || echo " $(python3 -I "$TOOLS/train-why.py" panic "$D/crash.log" crash_random "$D/crash.fails")")"
  fi
  if want crash && [ -n "${TRAIN_ENGINE_BIN:-}" ]; then
    rc=0; : >"$D/crash-binary.log"
    for seed in 1 2 3; do
      python3 -I "$TOOLS/crash-binary.py" "$TRAIN_ENGINE_BIN" --seed $seed >>"$D/crash-binary.log" 2>&1 || { rc=1; break; }
    done
    st crash-binary "$rc $([ $rc = 0 ] && echo "seeds 1-3 x 7 rounds" || grep -m1 'FAILED' "$D/crash-binary.log" | cut -c1-300)"
  fi
  if [ $POWER = 1 ]; then
    run crash_random@power power_cut >"$D/power-cut.log" 2>&1; rc=$?
    [ $rc = 0 ] || python3 -I "$TOOLS/train-why.py" libtest crash_random@power "$D/power-cut.log" "$D/power-cut.fails"
    st power-cut "$rc $(grep -o '[0-9]* passed' "$D/power-cut.log" | tail -1)$([ $rc = 0 ] || echo "; $(cut -f1 "$D/power-cut.fails" | paste -sd' ' -)")"
    CRASH_POWER=1 CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 run crash_random@power --exact random_crashes >"$D/crash-power.log" 2>&1; rc=$?
    st crash-power "$rc$([ $rc = 0 ] || echo " $(python3 -I "$TOOLS/train-why.py" panic "$D/crash-power.log" crash-power "$D/crash-power.fails")")"
    CRASH_POWER=1 CRASH_ASYNC=1 CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 run crash_random@power --exact random_crashes >"$D/crash-power-async.log" 2>&1; rc=$?
    st crash-power-async "$rc$([ $rc = 0 ] || echo " $(python3 -I "$TOOLS/train-why.py" panic "$D/crash-power-async.log" crash-power-async "$D/crash-power-async.fails")")"
  fi
}

lane3 & L3=$!
if want checked; then crate_tests checked "$D/bins-checked" & L2=$!; fi
lane1
want checked && wait "$L2"
wait "$L3"
st done 0
