#!/bin/sh
# A profile-guided build of roll-engine (HANDOFF "Profile-guided build"): the `dist` profile
# (fat LTO, one codegen unit) plus LLVM's profile of a training run. Steps:
#   1. the bench client (tools/bench/bench.rs), plain release, unless PGO_CLIENT names one;
#   2. an instrumented dist build (-Cprofile-generate) in its own target dir;
#   3. the training run (tools/pgo-train.sh) through every benchmarked workload class;
#   4. llvm-profdata merge of the raw profiles into <out>/roll-engine.profdata;
#   5. the optimised dist build (-Cprofile-use) in another target dir -> <out>/roll-engine.
# With --profdata FILE, steps 1-4 are skipped and FILE is used. The profile only matches builds
# of the same checkout path and close to the same commit (a function that changed loses its
# counts); the script builds the checkout it is in.
#
#   crates/pg-engine/tools/dist-pgo.sh [--profdata FILE] [--out DIR]
#
# Runs itself under tools/heavy.sh (one slot for the whole pipeline: builds and training).
# Environment: PGO_DIR (scratch: target dirs and raw profiles, default <root>/target/pgo; each
# target dir is deleted once its binary is copied out, PGO_KEEP=1 keeps them), PGO_CLIENT,
# PGO_BUILD_CPUS (taskset of the builds), and pgo-train.sh's PGO_PORT, PGO_SERVER_CPUS,
# PGO_CLIENT_CPUS, PGO_TPCH_SF; PGO_TPCH_SHARE (TPC-H's percent of the merged counts, default
# 45: the classes' weighting, below); PGO_BREADTH=1 (pgo-train.sh's breadth phase too, merged PGO_BENCH_SHARE to 1:
# measured, not the default, HANDOFF "Profile-guided build"), PGO_REGRESS_SRC; PGO_LLVM_ARGS
# (space-separated LLVM options for the optimised build). Both builds also take the shipped
# builds' flags (tools/dist-flags.sh, as tools/dist.sh): they go through RUSTFLAGS of these
# builds only.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
if [ -z "${PGO_IN_HEAVY:-}" ]; then PGO_IN_HEAVY=1 exec "$HERE/heavy.sh" "$0" "$@"; fi
PROF=; OUT=
while [ $# -gt 0 ]; do
  case $1 in
    --profdata) PROF=$(realpath "$2"); shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    *) echo "usage: dist-pgo.sh [--profdata FILE] [--out DIR]" >&2; exit 2 ;;
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
: >"$OUT/BUILD.txt"
note "commit $REV${DIRTY:+ (uncommitted changes)} $(rustc -V)"
if [ -z "$PROF" ]; then
  CLIENT=${PGO_CLIENT:-}
  if [ -z "$CLIENT" ]; then
    CARGO_TARGET_DIR=$W/t-client $(tb) cargo build --release -p pg-engine --example bench
    cp "$W/t-client/release/examples/bench" "$W/bench"; CLIENT=$W/bench
    [ -n "${PGO_KEEP:-}" ] || rm -rf "$W/t-client"
  fi
  T1=$(t)
  rm -rf "$W/raw"; mkdir -p "$W/raw"
  build "$W/t-gen" "-Cprofile-generate=$W/raw"
  cp "$W/t-gen/dist/roll-engine" "$W/roll-engine-instrumented"
  [ -n "${PGO_KEEP:-}" ] || rm -rf "$W/t-gen"
  T2=$(t)
  rm -rf "$W/braw"; BRAW=
  [ -z "${PGO_BREADTH:-}" ] || { BRAW=$W/braw; mkdir -p "$BRAW"; }
  "$HERE/pgo-train.sh" "$W/roll-engine-instrumented" "$CLIENT" "$W/raw" ${BRAW:+"$BRAW"}
  T3=$(t)
  # Each class's raw profiles (pgo-train.sh's <raw dir>/<class>) merged, then the classes
  # merged with TPC-H's class at PGO_TPCH_SHARE percent (default 45) of the counts: every
  # other class weighted by the same whole number. TPC-H trains at SF 1 (the benchmarked
  # scale), where its counts are 76% of the total unweighted and the point reads' paths fall
  # out of what LLVM takes as hot; 45% is TPC-H's share in the SF 0.2 training before
  # (HANDOFF "Analytic CPU, round 6").
  tot() { "$PROFDATA" show "$1" | awk '/^Total count:/ {print $3}'; }
  TT=0; TO=0; CLASSES=
  for d in "$W/raw"/*/; do
    c=$(basename "$d")
    "$PROFDATA" merge -o "$W/class-$c.profdata" "$d"*.profraw
    n=$(tot "$W/class-$c.profdata")
    if [ "$c" = tpch ]; then TT=$n; else TO=$((TO + n)); CLASSES="$CLASSES $c"; fi
  done
  WO=$(awk -v t="$TT" -v o="$TO" -v s="${PGO_TPCH_SHARE:-45}" 'BEGIN {w = (t > 0 && o > 0) ? int(t * (100 - s) / (s * o) + 0.5) : 1; print (w < 1 ? 1 : w)}')
  set --
  for c in $CLASSES; do set -- "$@" -weighted-input="$WO,$W/class-$c.profdata"; done
  [ ! -f "$W/class-tpch.profdata" ] || set -- "$@" "$W/class-tpch.profdata"
  "$PROFDATA" merge -o "$W/bench.profdata" "$@"
  note "profile counts: tpch $TT, the other classes $TO x$WO"
  rm -f "$W"/class-*.profdata
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
# (A function whose code changed since the profile has a different hash and just goes
# unprofiled; the warnings for each would bury the build log.)
USE="-Cprofile-use=$PROF -Cllvm-args=-no-pgo-warn-mismatch"
for a in ${PGO_LLVM_ARGS:-}; do USE="$USE -Cllvm-args=$a"; done
note "rustflags $USE${DIST_RUSTFLAGS:+ $DIST_RUSTFLAGS}"
build "$W/t-use" "$USE"
cp "$W/t-use/dist/roll-engine" "$OUT/roll-engine"
[ -n "${PGO_KEEP:-}" ] || rm -rf "$W/t-use"
T5=$(t)
note "optimised build $((T5 - T4)) s, total $((T5 - T0)) s"
note "profile $(sha256sum "$PROF" | cut -d' ' -f1) $(du -h "$PROF" | cut -f1)"
note "binary  $(sha256sum "$OUT/roll-engine" | cut -d' ' -f1) $(du -h "$OUT/roll-engine" | cut -f1)"
echo "dist-pgo: $OUT/roll-engine"
