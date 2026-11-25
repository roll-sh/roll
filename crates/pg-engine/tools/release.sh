#!/bin/sh
# The release build of roll-engine: build the shipped binary, check it, and write it out with
# its sha256 and build facts. Nothing is published, tagged or uploaded: a pipeline takes <out>.
#
#   crates/pg-engine/tools/release.sh --out DIR [--built DIR] [--profdata FILE]
#
# Steps (each prints "release: <step> ok|FAIL ..."; the first failure stops it, exit 1):
#   1. build    tools/dist-pgo.sh --out <out>/build: the PGO build of this checkout (a fresh
#               training: 3-3.5 hours here; HANDOFF "The shipped build"). It refuses a checkout
#               with uncommitted changes (RELEASE_DIRTY=1 allows one; BUILD.txt then says so) and
#               a recipe dist-pgo.sh marks "NOT THE SHIPPED RECIPE". --profdata FILE builds with
#               that merged profile instead of training (dist-pgo.sh --profdata: the optimised
#               build only, ~50 min); --built DIR takes an earlier dist-pgo.sh --out DIR of this
#               same commit (a pipeline's build job) and skips the build.
#   2. stale    tools/pgo-stale.py over the profile and the optimised build's log: refuses a
#               profile that misses more than RELEASE_STALE_MAX percent (default 1, dist-pgo.sh's
#               warning) of the hot functions' counts. A fresh training reads 0.00%.
#   3. smoke    the binary starts on an empty directory, answers a query and stops cleanly.
#   4. gate     quick-check against the binary (QC_PGO=<out>/build: regress, isolation and
#               pg_dump_tap against roll-engine itself, the crash step with the profile, then
#               crash-binary.py's kills; HANDOFF "The shipped build"), its own containers named
#               $RELEASE_PREFIX (default "release"); about 1.5 hours. RELEASE_SKIP=gate leaves it
#               out (the release's BUILD.txt then says "gate: skipped").
#   5. power    tools/crash-binary.py --power on the binary: a few seeds of power cuts
#               (RELEASE_POWER, default "drop:1 keep:2 drop:3:off": mode, seed, and "off" for
#               synchronous_commit=off), ~3-6 minutes each.
#   6. out      <out>/roll-engine, <out>/roll-engine.sha256 (sha256sum's format: `sha256sum -c`
#               in <out>), <out>/BUILD.txt (dist-pgo.sh's facts, then each step's line, the host
#               and the date), and <out>/roll-engine.profdata (the profile it was built with).
#
# A pipeline would run, on a Linux x86_64 host with Docker, the rustup toolchain with
# llvm-tools, ~10 GB of free disk and 4 hours:
#     crates/pg-engine/tools/release.sh --out "$RUNNER_TEMP/roll-engine"
# and upload <out>/roll-engine, roll-engine.sha256 and BUILD.txt as the job's artifacts (a
# release job then attaches them, as .github/workflows/release-cli.yml does for the CLI). To
# split it, a build job runs `tools/dist-pgo.sh --out B` and a check job `release.sh --built B
# --out OUT`. Exit 0 only when every step passed.
#
# Environment: RELEASE_PREFIX, RELEASE_SKIP (comma-separated: gate, power), RELEASE_POWER,
# RELEASE_STALE_MAX, RELEASE_DIRTY; dist-pgo.sh's (PGO_DIR, PGO_CLIENT, PGO_BUILD_CPUS, ...) and
# quick-check.sh's (QC_PORTS, QC_DIR, ...) pass through. Don't run it under heavy.sh: dist-pgo.sh
# and quick-check take their own slots (a nested heavy.sh deadlocks on the worktree lock).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
OUT=; BUILT=; PROF=
while [ $# -gt 0 ]; do
  case $1 in
    --out) OUT=$2; shift 2 ;;
    --built) BUILT=$(realpath "$2"); shift 2 ;;
    --profdata) PROF=$(realpath "$2"); shift 2 ;;
    *) echo "usage: release.sh --out DIR [--built DIR | --profdata FILE]" >&2; exit 2 ;;
  esac
done
[ -n "$OUT" ] || { echo "usage: release.sh --out DIR [--built DIR | --profdata FILE]" >&2; exit 2; }
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p "$OUT"; OUT=$(realpath "$OUT")
SKIP=",${RELEASE_SKIP:-},"
skip() { case $SKIP in *",$1,"*) return 0 ;; esac; return 1; }
LINES=$OUT/.steps
: >"$LINES"
say() { echo "release: $*"; echo "$*" >>"$LINES"; }
fail() { say "$1 FAIL $2"; exit 1; }
now() { date +%s; }
cd "$ROOT"
REV=$(git rev-parse --short=9 HEAD)
if [ -n "$(git status --porcelain --untracked-files=no | head -1)" ] && [ -z "${RELEASE_DIRTY:-}" ]; then
  fail build "uncommitted changes in $ROOT (RELEASE_DIRTY=1 to build them anyway)"
fi

# 1. build
t=$(now)
if [ -n "$BUILT" ]; then
  B=$BUILT
  [ -x "$B/roll-engine" ] && [ -f "$B/BUILD.txt" ] || fail build "$B has no roll-engine and BUILD.txt"
  say "build ok 0s taken from $B"
else
  B=$OUT/build
  rm -rf "$B"
  "$HERE/dist-pgo.sh" ${PROF:+--profdata "$PROF"} --out "$B" >"$OUT/build.log" 2>&1 \
    || { tail -20 "$OUT/build.log" >&2; fail build "$(($(now) - t))s dist-pgo.sh failed ($OUT/build.log)"; }
  say "build ok $(($(now) - t))s dist-pgo.sh${PROF:+ --profdata $PROF}"
fi
BUILD_REV=$(sed -n 's/^commit \([0-9a-f]*\).*/\1/p' "$B/BUILD.txt" | head -1)
case $REV in "$BUILD_REV"*) ;; *) fail build "$B was built from $BUILD_REV, the checkout is $REV" ;; esac
if grep -q 'NOT THE SHIPPED RECIPE' "$B/BUILD.txt"; then fail build "$(grep 'NOT THE SHIPPED RECIPE' "$B/BUILD.txt")"; fi
if grep -q '^commit .*(uncommitted changes)' "$B/BUILD.txt" && [ -z "${RELEASE_DIRTY:-}" ]; then
  fail build "$B was built with uncommitted changes"
fi
BIN=$B/roll-engine

# 2. stale
t=$(now)
MAX=${RELEASE_STALE_MAX:-1}
STALE=$(python3 -I "$HERE/pgo-stale.py" "$B/roll-engine.profdata" "$B/build-use.log" "$MAX") || fail stale "pgo-stale.py failed"
STALE=$(echo "$STALE" | head -1)
case $STALE in WARNING*) fail stale "$STALE (more than $MAX%: train again, without --profdata)" ;; esac
say "stale ok $(($(now) - t))s $STALE"

# 3. smoke
t=$(now)
SM=$(mktemp -d "${TMPDIR:-/tmp}/release-smoke-XXXXXX")
PORT=$("$HERE/ports.sh" take $$ 1)
"$BIN" --data "$SM/d" --listen "127.0.0.1:$PORT" >"$SM/log" 2>&1 &
PID=$!
ok=
for _ in $(seq 1 600); do
  if V=$(python3 -I - "$PORT" <<'EOF' 2>/dev/null
import socket, struct, sys
s = socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=5)
b = struct.pack('!i', 196608) + b'user\0postgres\0database\0postgres\0\0'
s.sendall(struct.pack('!i', len(b) + 4) + b)
q = b"SELECT 6 * 7, version()\0"
buf = b''
sent = False
while True:
    d = s.recv(65536)
    if not d:
        sys.exit(1)
    buf += d
    while len(buf) >= 5:
        n = struct.unpack('!i', buf[1:5])[0]
        if len(buf) < n + 1:
            break
        t, m, buf = buf[:1], buf[5:n + 1], buf[n + 1:]
        if t == b'E':
            sys.exit(1)
        if t == b'D' and b'42' in m:
            print(m[m.index(b'PostgreSQL'):].split(b' on ')[0].decode())
            sys.exit(0)
        if t == b'Z' and not sent:
            s.sendall(b'Q' + struct.pack('!i', len(q) + 4) + q)
            sent = True
EOF
  ); then ok=1; break; fi
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.1
done
kill -INT "$PID" 2>/dev/null || true
rc=0; wait "$PID" || rc=$?
"$HERE/ports.sh" release "$PORT" >/dev/null || true
if [ -z "$ok" ] || [ "$rc" -ne 0 ]; then tail -20 "$SM/log" >&2; rm -rf "$SM"; fail smoke "$(($(now) - t))s answered: ${ok:-no}, exit $rc"; fi
rm -rf "$SM"
say "smoke ok $(($(now) - t))s $V answered SELECT 6 * 7, stopped with 0"

# 4. gate
t=$(now)
if skip gate; then
  say "gate skipped (RELEASE_SKIP)"
else
  QC_PGO=$B QC_PREFIX=${RELEASE_PREFIX:-release} "$HERE/heavy.sh" "$HERE/quick-check.sh" >"$OUT/gate.log" 2>&1 \
    || { grep '^qc ' "$OUT/gate.log" | tail -5 >&2; fail gate "$(($(now) - t))s quick-check with QC_PGO ($OUT/gate.log)"; }
  say "gate ok $(($(now) - t))s quick-check QC_PGO: $(grep '^qc ' "$OUT/gate.log" | awk '{print $2 "=" $3}' | tr '\n' ' ')"
fi

# 5. power
if skip power; then
  say "power skipped (RELEASE_SKIP)"
else
  for spec in ${RELEASE_POWER:-drop:1 keep:2 drop:3:off}; do
    t=$(now)
    mode=${spec%%:*}; rest=${spec#*:}; seed=${rest%%:*}; off=
    case $rest in *:off) off=1 ;; esac
    log=$OUT/power-$mode-$seed${off:+-off}.log
    python3 -I "$HERE/crash-binary.py" "$BIN" --power "$mode" --seed "$seed" ${off:+--set synchronous_commit=off} >"$log" 2>&1 \
      || { tail -15 "$log" >&2; fail power "$(($(now) - t))s --power $mode --seed $seed${off:+ synchronous_commit=off} ($log)"; }
    say "power ok $(($(now) - t))s --power $mode --seed $seed${off:+ synchronous_commit=off}: $(grep -c '^round ' "$log") rounds and a clean shutdown, each cut and checked"
  done
fi

# 6. out
cp "$BIN" "$OUT/roll-engine.new" && mv "$OUT/roll-engine.new" "$OUT/roll-engine"
cp "$B/roll-engine.profdata" "$OUT/roll-engine.profdata"
(cd "$OUT" && sha256sum roll-engine >roll-engine.sha256)
{
  cat "$B/BUILD.txt"
  echo "release $(date -u +%Y-%m-%dT%H:%M:%SZ) on $(uname -srm), $(nproc) CPUs"
  cat "$LINES"
  echo "sha256 $(cut -d' ' -f1 "$OUT/roll-engine.sha256")"
} >"$OUT/BUILD.txt"
rm -f "$LINES"
[ -n "$BUILT" ] || [ -n "${RELEASE_KEEP_BUILD:-}" ] || rm -rf "$B"
echo "release: $OUT/roll-engine $(cut -d' ' -f1 "$OUT/roll-engine.sha256")"
