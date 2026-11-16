#!/bin/sh
# The verification train's full suite (tests/difftest/STATUS.md, "Verification train") on one
# checkout. tools/batch-train.sh runs it for every candidate at once; agents run
# tools/quick-check.sh and don't start it themselves. By hand, as one heavy job:
#
#   tools/heavy-bg.sh ~/.cache/aNNN/train.log env TRAIN_DIR=~/.cache/aNNN/train TRAIN_PREFIX=aNNN \
#     TRAIN_PORTS=53252,53253,53255 TRAIN_ISO_REF=127.0.0.1:53250 tools/verification-train.sh
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
# "done 0"); logs in $TRAIN_DIR/<stage>.log, the tests' per binary in $TRAIN_DIR/tests/.
# Containers: the train makes or starts them with tools/ref-containers.sh (<prefix>-rref, -iref
# on $TRAIN_ISO_REF's port, -psql, -dclient; $TRAIN_DIR/pgr/src-repo).
#
#   TRAIN_STAGES   the stages to run (default all: build,clippy,tests,checked,crash,power,regress,iso,dump);
#                  build always runs for what the others need
#   TRAIN_TESTS    test binaries for tests and checked (default all; space-separated names)
#   TRAIN_THREADS  each test binary's threads (default 6: two crate-test lanes on 12 CPUs)
#   TRAIN_NO_CHECKOUT=1  test the checkout as it is; otherwise it checks out origin's head when
#                  the checkout has no uncommitted changes
#   TRAIN_ENGINE_BIN=<roll-engine>  regress, isolation and pg_dump_tap against that binary (a PGO
#                  build, HANDOFF "Profile-guided build"); TRAIN_PGO_PROFDATA=<.profdata> builds
#                  crash_random for crash and power with `--profile dist` and -Cprofile-use of it
set -u
D=${TRAIN_DIR:?TRAIN_DIR: a scratch directory on disk}
P=${TRAIN_PREFIX:?TRAIN_PREFIX: the prefix of your containers}
PORTS=${TRAIN_PORTS:?TRAIN_PORTS: three free ports of yours, regress,isolation,pg_dump}
RPORT=${PORTS%%,*}; DPORT=${PORTS##*,}; IPORT=${PORTS#*,}; IPORT=${IPORT%%,*}
ISO_REF=${TRAIN_ISO_REF:-127.0.0.1:53250}
STAGES=",${TRAIN_STAGES:-build,clippy,tests,checked,crash,power,regress,iso,dump},"
THREADS=${TRAIN_THREADS:-6}
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$ROOT" || exit 1
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p "$D/bin" "$D/pgr" "$D/iso" "$D/pgdump" "$D/tests"; : >"$D/status"
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
ALL=$(ls crates/pg-engine/tests/*.rs | sed 's,.*/,,; s,\.rs$,,')
SEL=${TRAIN_TESTS:-lib $ALL}
TARGETS=$(for s in $SEL; do [ "$s" = lib ] && echo --lib || echo "--test $s"; done)
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
CRASHB=crash_random
if [ -n "${TRAIN_PGO_PROFDATA:-}" ] && { want crash || want power; }; then
  cargo test --profile dist -p pg-engine --no-run --test crash_random \
    --config "build.rustflags=['-Cprofile-use=$TRAIN_PGO_PROFDATA','-Cllvm-args=-no-pgo-warn-mismatch']" \
    --message-format=json-render-diagnostics 2>>"$D/build.log" | artifacts >>"$D/bins"
fi
if [ $POWER = 1 ]; then
  if [ -n "${TRAIN_PGO_PROFDATA:-}" ]; then
    set -- --profile dist --config "build.rustflags=['-Cprofile-use=$TRAIN_PGO_PROFDATA','-Cllvm-args=-no-pgo-warn-mismatch']"
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
  for s in $SEL; do
    b=$(bin "$s" "$bins"); [ -n "$b" ] || { bad="$bad $s:not-built"; continue; }
    (cd crates/pg-engine && RUST_TEST_THREADS=$THREADS "$b") >"$D/tests/$stage-$s.log" 2>&1 ||
      bad="$bad $(grep -E '^test .* FAILED$' "$D/tests/$stage-$s.log" | awk -v s="$s" '{printf "%s::%s ", s, $2}')$(grep -q '^test .* FAILED$' "$D/tests/$stage-$s.log" || echo "$s:exit")"
  done
  cat "$D/tests/$stage"-*.log >"$D/$stage.log" 2>/dev/null
  st "$stage" "$([ -z "$bad" ] && echo 0 || echo 101) $(( $(date +%s) - t ))s$bad"
}
run() { b=$(bin "$1" "$D/bins"); shift; [ -n "$b" ] || { echo "not built"; return 1; }; (cd crates/pg-engine && "$b" "$@"); }

# --- lane 3: the reference suites --------------------------------------------------------------
lane3() {
  { want regress || want iso || want dump; } || return 0
  "$ROOT/crates/pg-engine/tools/ref-containers.sh" up "$P" "$D" "${ISO_REF##*:}" >"$D/containers.log" 2>&1 ||
    echo "containers: $(tail -1 "$D/containers.log")" >>"$D/status"
  if want regress; then
    [ -d "$D/pgr/src-repo" ] || echo "no $D/pgr/src-repo: the regress harness clones it" >>"$D/status"
    rm -rf "$D/pgr/report"
    PGREGRESS_DIR=$D/pgr PGREGRESS_DB=regression_$P PGREGRESS_PORT=$RPORT PGREGRESS_CONTAINER=$P-rref \
      PGREGRESS_ENGINE_CONTAINER=$P-psql PGREGRESS_REPORT_DIR=$D/pgr/report \
      run pg_regress pg_regress --exact --ignored --nocapture >"$D/regress.log" 2>&1
    rc=$?
    nd=$(grep -o 'Newly differing units: [0-9]*' "$D/pgr/report/REPORT.md" 2>/dev/null | grep -o '[0-9]*$')
    [ "${nd:-x}" = 0 ] || [ $rc != 0 ] || rc=1
    st regress "$rc Newly differing units: ${nd:-?} $(grep -o '[0-9]*/[0-9]* units match' "$D/regress.log" | tail -1)"
  fi
  if want iso; then
    PGISOLATION_DIR=$D/iso PGISOLATION_PORT=$IPORT PGISOLATION_REF=$ISO_REF PGISOLATION_MAX_PREPARED=5 \
      PGISOLATION_REPORT_DIR=$D/iso/report run pg_isolation --ignored --nocapture >"$D/iso.log" 2>&1
    rc=$?
    m=$(grep -o '[0-9]*/[0-9]* permutations' "$D/iso.log" | tail -1)
    [ "${m%%/*}" -ge 1647 ] 2>/dev/null || [ $rc != 0 ] || rc=1
    st isolation "$rc $m"
  fi
  if want dump; then
    PGDUMP_DIR=$D/pgdump PGDUMP_CLIENT=$P-dclient PGDUMP_PORT=$DPORT PGDUMP_REPORT_DIR=$D/pgdump/report \
      PGDUMP_SRC=$D/pgr/src-repo run pg_dump --ignored --nocapture --exact pg_dump_tap >"$D/dump.log" 2>&1
    rc=$?
    m=$(grep -o '002_pg_dump.pl: [0-9]* passed, [0-9]* failed' "$D/dump.log" | tail -1)
    [ "$(echo "$m" | awk '{print ($2 >= 14609 && $4 == 0)}')" = 1 ] || [ $rc != 0 ] || rc=1
    st dump "$rc $m"
  fi
}

# --- lane 1: release tests and the crash stages ------------------------------------------------
lane1() {
  want tests && crate_tests tests "$D/bins"
  SEEDS=$(seq -s, 1 30)
  if want crash; then
    CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 run crash_random >"$D/crash.log" 2>&1; st crash $?
  fi
  if [ $POWER = 1 ]; then
    run crash_random@power power_cut >"$D/power-cut.log" 2>&1
    st power-cut "$? $(grep -o '[0-9]* passed' "$D/power-cut.log" | tail -1)"
    CRASH_POWER=1 CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 run crash_random@power --exact random_crashes >"$D/crash-power.log" 2>&1
    st crash-power $?
    CRASH_POWER=1 CRASH_ASYNC=1 CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 run crash_random@power --exact random_crashes >"$D/crash-power-async.log" 2>&1
    st crash-power-async $?
  fi
}

lane3 & L3=$!
if want checked; then crate_tests checked "$D/bins-checked" & L2=$!; fi
lane1
want checked && wait "$L2"
wait "$L3"
st done 0
