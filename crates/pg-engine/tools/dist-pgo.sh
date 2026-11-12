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
# PGO_CLIENT_CPUS; PGO_LLVM_ARGS (space-separated LLVM options for the optimised build). Plain `cargo build --profile dist` is unchanged: the flags go through
# RUSTFLAGS of these builds only.
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
  RUSTFLAGS=$2 CARGO_TARGET_DIR=$1 $(tb) cargo build --profile dist -p pg-engine --bin roll-engine
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
  "$HERE/pgo-train.sh" "$W/roll-engine-instrumented" "$CLIENT" "$W/raw"
  T3=$(t)
  "$PROFDATA" merge -o "$OUT/roll-engine.profdata" "$W/raw"/*.profraw
  rm -rf "$W/raw"
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
note "rustflags $USE"
build "$W/t-use" "$USE"
cp "$W/t-use/dist/roll-engine" "$OUT/roll-engine"
[ -n "${PGO_KEEP:-}" ] || rm -rf "$W/t-use"
T5=$(t)
note "optimised build $((T5 - T4)) s, total $((T5 - T0)) s"
note "profile $(sha256sum "$PROF" | cut -d' ' -f1) $(du -h "$PROF" | cut -f1)"
note "binary  $(sha256sum "$OUT/roll-engine" | cut -d' ' -f1) $(du -h "$OUT/roll-engine" | cut -f1)"
echo "dist-pgo: $OUT/roll-engine"
