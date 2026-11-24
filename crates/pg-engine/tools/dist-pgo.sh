#!/bin/sh
# The shipped roll-engine: a profile-guided build (HANDOFF "Profile-guided build", "The shipped
# build"): the `dist` profile (fat LTO, one codegen unit) and tools/dist-flags.sh, plus LLVM's
# profile of a training run. Steps:
#   1. the bench client (tools/bench/bench.rs, release), unless PGO_CLIENT names one;
#   2. an instrumented dist build (-Cprofile-generate) in its own target dir;
#   3. the training run (tools/pgo-train.sh) through every benchmarked workload class; it fails
#      when a class did not run or its profile is empty (a skipped class needs PGO_TRAIN_SKIP,
#      and BUILD.txt then says the build is not for shipping);
#   4. llvm-profdata merge of the raw profiles into <out>/roll-engine.profdata;
#   5. the optimised dist build (-Cprofile-use) in another target dir -> <out>/roll-engine, its
#      log in <out>/build-use.log; tools/pgo-stale.py reports how much of the profile no longer
#      matches the code, with a WARNING past PGO_STALE_WARN percent (default 1) of the hot
#      functions' counts (0 when the profile was trained on this commit).
# With --profdata FILE, steps 1-4 are skipped and FILE is used; with --instrumented FILE, step 2
# (FILE: an instrumented roll-engine of this commit, e.g. <PGO_DIR>/roll-engine-instrumented,
# kept from an earlier run); with --raw DIR, steps 1-3 (DIR: pgo-train.sh's raw dir of an earlier
# training of this commit, its class checks made again; no breadth phase). A profile only fits builds close to its commit (a function that
# changed loses its counts: step 5's staleness line); the script builds the checkout it is in.
# <out>/BUILD.txt records the commit, toolchain, each class's counts, the staleness, the times and
# the hashes of the profile and the binary.
#
#   crates/pg-engine/tools/dist-pgo.sh [--profdata FILE | --instrumented FILE | --raw DIR] [--out DIR]
#
# Needs: the rustup toolchain with llvm-tools (`rustup component add llvm-tools`) and Docker (the
# training's psql and pgbench, PostgreSQL 18.6's image by digest). Measured here (2026-11-23,
# load 50-140, the machine's 2-job cargo cap): instrumented build 80 min, training 14 min,
# optimised build 48 min, the release client 36-60 min (PGO_CLIENT skips it): 3-3.5 hours. Disk:
# each dist target dir ~1 GB (deleted after its binary is copied), raw profiles 0.1 GB, the
# training's data directory a few GB (pgo-train.sh prints its peak); BUILD.txt has the times and
# the peak of the scratch and output directories.
#
# Runs itself under tools/heavy.sh (one slot for the whole pipeline: builds and training).
# Environment: PGO_DIR (scratch: target dirs and raw profiles, default <root>/target/pgo; each
# target dir is deleted once its binary is copied out, PGO_KEEP=1 keeps them), PGO_CLIENT,
# PGO_BUILD_CPUS (taskset of the builds), and pgo-train.sh's PGO_PORT, PGO_SERVER_CPUS,
# PGO_CLIENT_CPUS, PGO_TPCH_SF; PGO_BREADTH=1 (pgo-train.sh's breadth phase too, merged PGO_BENCH_SHARE to 1:
# measured, not the default, HANDOFF "Profile-guided build"), PGO_REGRESS_SRC; PGO_LLVM_ARGS
# (space-separated LLVM options for the optimised build). Both builds also take the shipped
# builds' flags (tools/dist-flags.sh, as tools/dist.sh): they go through RUSTFLAGS of these
# builds only.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
if [ -z "${PGO_IN_HEAVY:-}" ]; then PGO_IN_HEAVY=1 exec "$HERE/heavy.sh" "$0" "$@"; fi
PROF=; OUT=
INSTR=; RAWIN=
while [ $# -gt 0 ]; do
  case $1 in
    --profdata) PROF=$(realpath "$2"); shift 2 ;;
    --instrumented) INSTR=$(realpath "$2"); shift 2 ;;
    --raw) RAWIN=$(realpath "$2"); shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    *) echo "usage: dist-pgo.sh [--profdata FILE | --instrumented FILE | --raw DIR] [--out DIR]" >&2; exit 2 ;;
  esac
done
export PATH="$HOME/.cargo/bin:$PATH"
W=${PGO_DIR:-$ROOT/target/pgo}
OUT=${OUT:-$W/out}
mkdir -p "$W" "$OUT"; W=$(realpath "$W"); OUT=$(realpath "$OUT")
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
. "$HERE/dist-flags.sh"
cd "$ROOT"
REV=$(git rev-parse --short=9 HEAD)
DIRTY=$(git status --porcelain --untracked-files=no | head -1)
HOST=$(rustc -vV | sed -n 's/^host: //p')
PROFDATA=$(rustc --print sysroot)/lib/rustlib/$HOST/bin/llvm-profdata
[ -x "$PROFDATA" ] || { echo "dist-pgo: no $PROFDATA (rustup component add llvm-tools)" >&2; exit 1; }
tb() { [ -n "${PGO_BUILD_CPUS:-}" ] && echo "taskset -c $PGO_BUILD_CPUS" || true; }
t() { date +%s; }
T0=$(t)
build() { # build <target dir> <rustflags>: the dist roll-engine with these flags
  RUSTFLAGS="$2${DIST_RUSTFLAGS:+ $DIST_RUSTFLAGS}" CARGO_TARGET_DIR=$1 $(tb) cargo build --profile dist -p pg-engine --bin roll-engine
}
note() { echo "dist-pgo: $*"; echo "$*" >>"$OUT/BUILD.txt"; }
# The largest the scratch and output directories got (checked after each step).
PEAK=0
disk() { k=$(du -sk "$W" "$OUT" 2>/dev/null | awk '{s += $1} END {print s + 0}'); [ "$k" -le "$PEAK" ] || PEAK=$k; }
: >"$OUT/BUILD.txt"
note "commit $REV${DIRTY:+ (uncommitted changes)} $(rustc -V)"
if [ -n "${PGO_TRAIN_SKIP:-}${PGO_BREADTH:-}${PGO_LLVM_ARGS:-}" ]; then
  note "NOT THE SHIPPED RECIPE:${PGO_TRAIN_SKIP:+ PGO_TRAIN_SKIP=$PGO_TRAIN_SKIP}${PGO_BREADTH:+ PGO_BREADTH=$PGO_BREADTH}${PGO_LLVM_ARGS:+ PGO_LLVM_ARGS=$PGO_LLVM_ARGS}"
fi
RAWD=${RAWIN:-$W/raw}
if [ -z "$PROF" ] && [ -n "$RAWIN" ]; then
  T1=$(t); T2=$T1; T3=$T1
  note "raw profiles of an earlier training: $RAWIN"
elif [ -z "$PROF" ]; then
  CLIENT=${PGO_CLIENT:-}
  if [ -z "$CLIENT" ]; then
    CARGO_TARGET_DIR=$W/t-client $(tb) cargo build --release -p pg-engine --example bench
    cp "$W/t-client/release/examples/bench" "$W/bench"; CLIENT=$W/bench
    disk; [ -n "${PGO_KEEP:-}" ] || rm -rf "$W/t-client"
  fi
  T1=$(t)
  rm -rf "$W/raw"; mkdir -p "$W/raw"
  if [ -n "$INSTR" ]; then
    [ "$INSTR" = "$W/roll-engine-instrumented" ] || cp "$INSTR" "$W/roll-engine-instrumented"
    note "instrumented build: $INSTR ($(sha256sum "$INSTR" | cut -c1-16))"
  else
    build "$W/t-gen" "-Cprofile-generate=$W/raw"
    cp "$W/t-gen/dist/roll-engine" "$W/roll-engine-instrumented"
    disk; [ -n "${PGO_KEEP:-}" ] || rm -rf "$W/t-gen"
  fi
  T2=$(t)
  rm -rf "$W/braw"; BRAW=
  [ -z "${PGO_BREADTH:-}" ] || { BRAW=$W/braw; mkdir -p "$BRAW"; }
  "$HERE/pgo-train.sh" "$W/roll-engine-instrumented" "$CLIENT" "$W/raw" ${BRAW:+"$BRAW"}
  T3=$(t); disk
fi
if [ -z "$PROF" ]; then
  # Every class ran (its mark: the engine stopped cleanly after its parts, so its counts were
  # written) and wrote counts, unless PGO_TRAIN_SKIP left it out (not for shipping).
  for c in startup point range bulk short tpch catalog regress; do
    case " ${PGO_TRAIN_SKIP:-} " in *" $c "*) continue ;; esac
    [ -f "$RAWD/$c/.ran" ] && ls "$RAWD/$c"/*.profraw >/dev/null 2>&1 \
      || { echo "dist-pgo: training class $c did not run or wrote no profile ($RAWD/$c)" >&2; exit 1; }
  done
  # Each class's raw profiles (pgo-train.sh's <raw dir>/<class>) merged, then the classes
  # merged with the same weight: each one's counts multiplied by the whole number that brings
  # its total to the largest class's (TPC-H's). LLVM takes as hot the blocks that make 99% of
  # the merged counts: unweighted, TPC-H at SF 1 is 76% of them and the short transactions'
  # paths fall out of that set (93.8% of their counts in hot blocks against 97.5% with SF 0.2);
  # weighted the same, every class has more of its counts in hot blocks than with SF 0.2
  # unweighted (HANDOFF "Analytic CPU, round 6", `hotcov`).
  tot() { "$PROFDATA" show "$1" | awk '/^Total count:/ {print $3}'; }
  MAX=0; CLASSES=
  for d in "$RAWD"/*/; do
    c=$(basename "$d")
    "$PROFDATA" merge -o "$W/class-$c.profdata" "$d"*.profraw
    n=$(tot "$W/class-$c.profdata")
    [ "${n:-0}" -gt 0 ] || { echo "dist-pgo: training class $c's profile is empty" >&2; exit 1; }
    eval "TOT_$c=$n"
    [ "$n" -le "$MAX" ] || MAX=$n
    CLASSES="$CLASSES $c"
  done
  set --
  WEIGHTS=
  for c in $CLASSES; do
    eval "n=\$TOT_$c"
    w=$(awk -v m="$MAX" -v n="$n" 'BEGIN {w = n > 0 ? int(m / n + 0.5) : 1; print (w < 1 ? 1 : w)}')
    set -- "$@" -weighted-input="$w,$W/class-$c.profdata"
    WEIGHTS="$WEIGHTS $c=$n*$w"
  done
  "$PROFDATA" merge -o "$W/bench.profdata" "$@"
  note "profile counts:$WEIGHTS"
  # (Kept, a few MB each: tools/pgo-hotcov.py <out>/classes, or another weighting without training.)
  rm -rf "$OUT/classes"; mkdir -p "$OUT/classes"; mv "$W"/class-*.profdata "$OUT/classes/"
  if [ -d "$W/braw" ]; then
    # The benchmark classes keep nearly all the counts: their profile weighted so that its total
    # is PGO_BENCH_SHARE (default 50) times the breadth phase's, about 98% of the merged counts.
    # (Not the default: every weighting measured cost TPC-H 1.4-2.2% instructions and the OLTP
    # classes about 2% cycles, and at 4x sort queries came out slower than the plain dist build;
    # HANDOFF "Profile-guided build".)
    "$PROFDATA" merge -o "$W/breadth.profdata" "$W/braw"/*.profraw
    TB=$(tot "$W/bench.profdata"); TR=$(tot "$W/breadth.profdata")
    WB=$(awk -v b="$TB" -v r="$TR" -v k="${PGO_BENCH_SHARE:-50}" 'BEGIN {w = int(k * r / b) + 1; print (w < 1 ? 1 : w)}')
    "$PROFDATA" merge -o "$OUT/roll-engine.profdata" -weighted-input="$WB,$W/bench.profdata" "$W/breadth.profdata"
    note "profile counts: benchmarks $TB x$WB, breadth $TR"
  else
    cp "$W/bench.profdata" "$OUT/roll-engine.profdata"
  fi
  # (Each phase's profile kept next to the merged one: another weighting needs no new training.)
  mv "$W/bench.profdata" "$OUT/bench.profdata"
  [ ! -f "$W/breadth.profdata" ] || mv "$W/breadth.profdata" "$OUT/breadth.profdata"
  rm -rf "$W/raw" "$W/braw"
  PROF=$OUT/roll-engine.profdata
  note "client $((T1 - T0)) s, instrumented build $((T2 - T1)) s, training $((T3 - T2)) s"
elif [ "$PROF" != "$OUT/roll-engine.profdata" ]; then
  cp "$PROF" "$OUT/roll-engine.profdata"; PROF=$OUT/roll-engine.profdata
fi
T4=$(t)
# A function whose code changed since the profile has a different hash and goes unprofiled
# (compiled as never run); one that is new has no data. LLVM warns for each: the warnings go to
# <out>/build-use.log, and pgo-stale.py sums them up against the profile's hot functions.
USE="-Cprofile-use=$PROF -Cllvm-args=-pgo-warn-missing-function"
for a in ${PGO_LLVM_ARGS:-}; do USE="$USE -Cllvm-args=$a"; done
note "rustflags $USE${DIST_RUSTFLAGS:+ $DIST_RUSTFLAGS}"
build "$W/t-use" "$USE" >"$OUT/build-use.log" 2>&1 || { tail -40 "$OUT/build-use.log" >&2; echo "dist-pgo: the optimised build failed ($OUT/build-use.log)" >&2; exit 1; }
cp "$W/t-use/dist/roll-engine" "$OUT/roll-engine"
disk; [ -n "${PGO_KEEP:-}" ] || rm -rf "$W/t-use"
T5=$(t)
STALE=$(python3 -I "$HERE/pgo-stale.py" "$PROF" "$OUT/build-use.log" "${PGO_STALE_WARN:-1}") \
  || { echo "dist-pgo: pgo-stale.py failed" >&2; exit 1; }
echo "$STALE" | while IFS= read -r l; do note "$l"; done
case $STALE in WARNING*) echo "$STALE" | head -1 >&2 ;; esac
note "optimised build $((T5 - T4)) s, total $((T5 - T0)) s, peak disk $((PEAK / 1048576)) GB in $W and $OUT"
note "profile $(sha256sum "$PROF" | cut -d' ' -f1) $(du -h "$PROF" | cut -f1)"
note "binary  $(sha256sum "$OUT/roll-engine" | cut -d' ' -f1) $(du -h "$OUT/roll-engine" | cut -f1)"
echo "dist-pgo: $OUT/roll-engine"
