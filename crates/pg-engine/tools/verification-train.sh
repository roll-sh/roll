#!/bin/sh
# The verification train's full suite on origin's head (HANDOFF machine rules; tests/difftest/
# STATUS.md, "Verification train"), in ONE heavy job: start it with tools/heavy-bg.sh, e.g.
#
#   tools/heavy-bg.sh ~/.cache/aNNN/train.log env TRAIN_DIR=~/.cache/aNNN/train \
#     TRAIN_PREFIX=aNNN TRAIN_PORTS=53252,53253,53255 tools/verification-train.sh
#
# then run tools/difftest-guard.sh (base: the last guarded head) with the difftest binary this
# job copies to $TRAIN_DIR/bin/dt-<head>. When the slot comes it fetches origin and checks out
# its head (if the checkout has no uncommitted changes), so a job queued for hours tests the
# newest head. Steps, each logged to $TRAIN_DIR/<step>.log, results in $TRAIN_DIR/status:
# release crate tests, clippy (the warning count), crash_random seeds 1-30 x 4, then the power
# stage with `--features powerloss` (HANDOFF "Crash recovery", power loss): the power_cut tests
# and seeds 1-30 x 4 with CRASH_POWER=1, with and without CRASH_ASYNC=1; debug crate tests, the full regression suite on a
# private reference, isolation, pg_dump_tap. Containers (made beforehand, HANDOFF "How to
# test"): <prefix>-rref (the regress reference, default settings), <prefix>-psql (the engine
# side's psql, host network, $TRAIN_DIR/pgr mounted), <prefix>-iref (isolation reference on the
# host network at port 53250-style $TRAIN_ISO_REF, max_prepared_transactions=5),
# <prefix>-dclient (pg_dump client, host network, $TRAIN_DIR/pgdump mounted). The idle timer
# stops containers quiet for 3 hours: the job starts them itself.
# $TRAIN_DIR/pgr/src-repo must have src/bin/pg_dump (a sparse copy without it fails pg_dump_tap).
set -u
D=${TRAIN_DIR:?TRAIN_DIR: a scratch directory on disk}
P=${TRAIN_PREFIX:?TRAIN_PREFIX: the prefix of your containers}
PORTS=${TRAIN_PORTS:?TRAIN_PORTS: three free ports of yours, regress,isolation,pg_dump}
RPORT=${PORTS%%,*}; DPORT=${PORTS##*,}; IPORT=${PORTS#*,}; IPORT=${IPORT%%,*}
ISO_REF=${TRAIN_ISO_REF:-127.0.0.1:53250}
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$ROOT" || exit 1
mkdir -p "$D/bin" "$D/pgr" "$D/iso" "$D/pgdump"; : >"$D/status"
st() { echo "$1 $2 $(date +%T)" >>"$D/status"; }
up() { sg docker -c "docker start $P-rref $P-iref $P-psql $P-dclient" >/dev/null 2>&1; sleep 10; }
if [ -z "$(git status --porcelain --untracked-files=no)" ]; then
  git fetch -q origin && git checkout -q --detach origin/docs/postgres-architecture-strategy
fi
H=$(git rev-parse --short=9 HEAD); st head "$H"
T=${CARGO_TARGET_DIR:-$ROOT/target}
cargo test --release --no-fail-fast -p pg-engine >"$D/release.log" 2>&1; st release $?
cp "$(ls -t "$T"/release/deps/difftest-* | grep -v '\.d$' | head -1)" "$D/bin/dt-$(git rev-parse --short=8 HEAD)"
cargo clippy -p pg-engine --all-targets >"$D/clippy.log" 2>&1; st clippy "$? $(grep -c '^warning' "$D/clippy.log")w"
SEEDS=$(seq -s, 1 30)
CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 cargo test --release -p pg-engine --test crash_random >"$D/crash.log" 2>&1; st crash $?
# (The power stage builds the engine with the powerloss feature; the regress step after it
# builds it again without.)
PW="cargo test --release -p pg-engine --features powerloss --test crash_random"
if grep -q 'feature = "powerloss"' crates/pg-engine/tests/crash_random.rs; then
  $PW -- power_cut >"$D/power-cut.log" 2>&1
  st power-cut "$? $(grep -o '[0-9]* passed' "$D/power-cut.log" | tail -1)"
  CRASH_POWER=1 CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 $PW -- --exact random_crashes >"$D/crash-power.log" 2>&1
  st crash-power $?
  CRASH_POWER=1 CRASH_ASYNC=1 CRASH_SEEDS=$SEEDS CRASH_ROUNDS=4 $PW -- --exact random_crashes >"$D/crash-power-async.log" 2>&1
  st crash-power-async $?
fi
cargo test --no-fail-fast -p pg-engine >"$D/debug.log" 2>&1; st debug $?
[ -d "$D/pgr/src-repo" ] || echo "no $D/pgr/src-repo: the regress harness clones it" >>"$D/status"
up
PGREGRESS_DIR=$D/pgr PGREGRESS_DB=regression_$P PGREGRESS_PORT=$RPORT PGREGRESS_CONTAINER=$P-rref \
  PGREGRESS_ENGINE_CONTAINER=$P-psql PGREGRESS_REPORT_DIR=$D/pgr/report \
  cargo test --release -p pg-engine --test pg_regress -- pg_regress --exact --ignored --nocapture >"$D/regress.log" 2>&1
st regress "$? $(grep -o 'Newly differing units: [0-9]*' "$D/pgr/report/REPORT.md" 2>/dev/null)"
up
PGISOLATION_DIR=$D/iso PGISOLATION_PORT=$IPORT PGISOLATION_REF=$ISO_REF PGISOLATION_MAX_PREPARED=5 \
  PGISOLATION_REPORT_DIR=$D/iso/report cargo test --release -p pg-engine --test pg_isolation -- --ignored --nocapture >"$D/iso.log" 2>&1
st isolation "$? $(grep -o '[0-9]*/[0-9]* permutations' "$D/iso.log")"
PGDUMP_DIR=$D/pgdump PGDUMP_CLIENT=$P-dclient PGDUMP_PORT=$DPORT PGDUMP_REPORT_DIR=$D/pgdump/report PGDUMP_SRC=$D/pgr/src-repo \
  cargo test --release -p pg-engine --test pg_dump -- --ignored --nocapture --exact pg_dump_tap >"$D/dump.log" 2>&1
st dump "$? $(grep -o '002_pg_dump.pl: [0-9]* passed, [0-9]* failed' "$D/dump.log")"
st done 0
