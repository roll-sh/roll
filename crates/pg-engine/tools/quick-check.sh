#!/bin/sh
# The gate an agent runs before it offers a change (tests/difftest/STATUS.md, "Verification
# train"): the tests that catch most of what the full train catches, on the agent's own
# incremental build, in about 20 minutes. Green here, the change goes to the batch train as a
# cand/<agent> branch (tools/batch-train.sh); agents don't run the full train themselves.
#
#   QC_PREFIX=a242 QC_PORTS=54850,54851,54852,54853 crates/pg-engine/tools/heavy.sh \
#     crates/pg-engine/tools/quick-check.sh
#
# One line per step ("qc <step> ok|FAIL <seconds>s <detail>"); the first failure stops it with a
# non-zero exit, and its log is $QC_DIR/<step>.log. Steps:
#   diff     what changed against the merge base with $QC_BASE (commits, staged, unstaged and
#            untracked files), and the test binaries that cover it (tests_for below)
#   build    those test binaries and the reference harnesses (profile $QC_PROFILE)
#   clippy   `--all-targets` (so also the examples), alongside the build: fails on a warning in
#            a changed file that the base doesn't have (the batch train keeps each head's
#            warnings; without them, a warning on a line the diff touched)
#   tests    golden, wire, wire_waits, the binaries the diff maps to, and the lib's unit tests when
#            a changed source file has some
#   crash    when storage, WAL or commit paths changed: crash_random seeds 1-3 x 2 rounds, then
#            one power cut case (power_cut_at_every_point, built `--features powerloss`)
#   regress  the full regression suite on the reference: 0 newly differing units
#   iso      isolation: at least 1,647 of 1,648 permutations
#   dump     pg_dump_tap: at least 14,609 passed, 0 failed
# regress, iso and dump run one after another alongside the crate tests (they wait on the
# reference more than on CPU). Test binaries run directly (cargo would hold the build lock).
#
# Environment:
#   QC_PREFIX  your containers' prefix (tools/ref-containers.sh makes them here when missing)
#   QC_PORTS   four free ports: regress engine, isolation engine, pg_dump engine, isolation reference
#   QC_DIR     scratch (default ~/.cache/qc-$QC_PREFIX); QC_BASE (default origin's branch)
#   QC_PROFILE cargo profile (default verify: release without LTO, as the train builds)
#   QC_SKIP    steps to skip, comma-separated (regress,iso,dump without containers; clippy; crash)
#   QC_TESTS   more test binaries (names under tests/, space-separated); QC_ALL=1 runs all of them
#   QC_DRY=1   print the diff step's choice of tests and stop
set -u
export PATH="$HOME/.cargo/bin:$PATH"
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$ROOT" || exit 1
TOOLS=$ROOT/crates/pg-engine/tools
P=${QC_PREFIX:-}
D=${QC_DIR:-$HOME/.cache/qc-${P:-$(id -un)}}
PROF=${QC_PROFILE:-verify}
SKIP=",${QC_SKIP:-},"
mkdir -p "$D"
now() { date +%s; }
skip() { case $SKIP in *",$1,"*) return 0 ;; esac; return 1; }
# The path of a built test binary ("lib", a tests/ name, or crash_random@power).
bin() { awk -v n="$1" '$1 == n {print $2}' "$D/bins"; }
# Run one: as cargo does, from the package's directory.
run() { b=$(bin "$1"); shift; [ -n "$b" ] || { echo "no binary built"; return 1; }; (cd "$ROOT/crates/pg-engine" && "$b" "$@"); }

# The reference suites, in their own process group (re-run of this script with QC_REF_CHAIN=1):
# one status line each to $D/ref.status.
if [ "${QC_REF_CHAIN:-}" = 1 ]; then
  RPORT=$(echo "$QC_PORTS" | cut -d, -f1); IPORT=$(echo "$QC_PORTS" | cut -d, -f2)
  DPORT=$(echo "$QC_PORTS" | cut -d, -f3); IREF=$(echo "$QC_PORTS" | cut -d, -f4)
  if ! skip regress; then
    t=$(now); rm -rf "$D/pgr/report"
    PGREGRESS_DIR=$D/pgr PGREGRESS_DB=regression_$P PGREGRESS_PORT=$RPORT PGREGRESS_CONTAINER=$P-rref \
      PGREGRESS_ENGINE_CONTAINER=$P-psql PGREGRESS_REPORT_DIR=$D/pgr/report \
      run pg_regress pg_regress --exact --ignored --nocapture >"$D/regress.log" 2>&1
    rc=$?
    nd=$(grep -o 'Newly differing units: [0-9]*' "$D/pgr/report/REPORT.md" 2>/dev/null | grep -o '[0-9]*$')
    m=$(grep -o '[0-9]*/[0-9]* units match' "$D/regress.log" | tail -1)
    if [ $rc = 0 ] && [ "${nd:-x}" = 0 ]; then echo "regress ok $(( $(now) - t )) $m, 0 newly differing"
    else echo "regress FAIL $(( $(now) - t )) exit $rc, newly differing ${nd:-?} (report: $D/pgr/report/REPORT.md)"; fi >>"$D/ref.status"
  fi
  if ! skip iso; then
    t=$(now)
    PGISOLATION_DIR=$D/iso PGISOLATION_PORT=$IPORT PGISOLATION_REF=127.0.0.1:$IREF PGISOLATION_MAX_PREPARED=5 \
      PGISOLATION_REPORT_DIR=$D/iso/report run pg_isolation --ignored --nocapture >"$D/iso.log" 2>&1
    rc=$?
    m=$(grep -o '[0-9]*/[0-9]* permutations' "$D/iso.log" | tail -1)
    if [ $rc = 0 ] && [ "${m%%/*}" -ge 1647 ] 2>/dev/null; then echo "iso ok $(( $(now) - t )) $m"
    else echo "iso FAIL $(( $(now) - t )) exit $rc, ${m:-no count} (1,647 needed)"; fi >>"$D/ref.status"
  fi
  if ! skip dump; then
    t=$(now)
    PGDUMP_DIR=$D/pgdump PGDUMP_CLIENT=$P-dclient PGDUMP_PORT=$DPORT PGDUMP_REPORT_DIR=$D/pgdump/report \
      PGDUMP_SRC=$D/pgr/src-repo run pg_dump --ignored --nocapture --exact pg_dump_tap >"$D/dump.log" 2>&1
    rc=$?
    m=$(grep -o '002_pg_dump.pl: [0-9]* passed, [0-9]* failed' "$D/dump.log" | tail -1)
    if [ $rc = 0 ] && [ "$(echo "$m" | awk '{print ($2 >= 14609 && $4 == 0)}')" = 1 ]; then echo "dump ok $(( $(now) - t )) ${m#002_pg_dump.pl: }"
    else echo "dump FAIL $(( $(now) - t )) exit $rc, ${m:-no count} (14,609 passed, 0 failed needed)"; fi >>"$D/ref.status"
  fi
  echo done >>"$D/ref.status"
  exit 0
fi

T0=$(now)
line() { echo "qc $1 $2 $(( $(now) - $3 ))s ${4:-}"; }
KILLGRPS=
cleanup() { for g in $KILLGRPS; do kill -TERM -- "-$g" 2>/dev/null; done; KILLGRPS=; }
fail() { line "$1" FAIL "$2" "${3:-} (log: $D/$1.log)"; cleanup; exit 1; }
trap 'cleanup; exit 130' INT TERM

# --- diff ----------------------------------------------------------------------------------
t=$(now)
BASE=$(git merge-base "${QC_BASE:-origin/docs/postgres-architecture-strategy}" HEAD) || fail diff "$t" "no merge base"
{ git diff --name-only "$BASE"; git ls-files --others --exclude-standard; } | sort -u >"$D/changed"
# Test binaries per changed path, beside the core set (golden, wire, wire_waits: what the train's
# crate tests caught, STATUS.md "Verification train"). CRASH adds the crash stage.
tests_for() {
  case $1 in
    crates/pg-engine/Cargo.toml|Cargo.toml|Cargo.lock) echo ALL CRASH ;;
    # A shared test module: the test files that include it by #[path], else every one.
    crates/pg-engine/tests/common/*)
      u=$(grep -l "#\[path = \"common/${1##*/}\"\]" crates/pg-engine/tests/*.rs 2>/dev/null | sed 's,.*/,,; s,\.rs$,,')
      echo "${u:-ALL}" ;;
    crates/pg-engine/tests/data/protocol_cases*) echo wire ;;
    crates/pg-engine/tests/sql/*|crates/pg-engine/tests/data/*) echo golden ;;
    crates/pg-engine/tests/*.rs) basename "$1" .rs ;;
    crates/pg-engine/src/storage/*|crates/pg-engine/src/txn/*|crates/pg-engine/src/engine.rs)
      echo CRASH crash_bulk crash_ctids crash_async_commit crash_unlogged storage_format \
        storage_linked_gc storage_read_once storage_upgrade_batches toast_storage compact_locks \
        drop_storage index_key_format twophase txn_spill app_concurrency ;;
    crates/pg-engine/src/catalog/btsim*|crates/pg-engine/src/catalog/heapsim*|crates/pg-engine/src/exec/sizing*|crates/pg-engine/src/catalog/relsize.rs)
      echo models_kept big_tables heap_order_scan kept_plans ;;
    crates/pg-engine/src/catalog/*) echo catalog_format catalog_storage catalog_lazy_snapshot ddl_scaling poisoned_caches models_kept ;;
    crates/pg-engine/src/sql/paths*|crates/pg-engine/src/sql/costs.rs|crates/pg-engine/src/sql/plan.rs|crates/pg-engine/src/sql/joins.rs|crates/pg-engine/src/session/litplan.rs|crates/pg-engine/src/session/prepared.rs)
      echo kept_plans models_kept parallel_scan heap_order_scan big_tables query_spill ;;
    crates/pg-engine/src/exec/parscan.rs|crates/pg-engine/src/exec/parallel_part.rs|crates/pg-engine/src/exec/partial.rs) echo parallel_scan ;;
    crates/pg-engine/src/exec/spill.rs|crates/pg-engine/src/exec/hashspill.rs|crates/pg-engine/src/exec/extsort.rs|crates/pg-engine/src/exec/workmem.rs|crates/pg-engine/src/exec/tuplestore.rs)
      echo query_spill txn_spill memory_load ;;
    crates/pg-engine/src/exec/vector.rs|crates/pg-engine/src/ext/vector*) echo pgvector_equality pgvector_hnsw pgvector_ivfflat ;;
    crates/pg-engine/src/exec/*) echo parallel_scan query_spill big_tables heap_order_scan stored_filters pk_clustered ;;
    crates/pg-engine/src/sql/deparse*|crates/pg-engine/src/sql/ruleutils*) echo deparse_identity ;;
    crates/pg-engine/src/sql/parse.rs|crates/pg-engine/src/stack.rs|crates/pg-engine/src/plpgsql/*) echo deep_input ;;
    crates/pg-engine/src/repl/*) echo logical_apply ;;
    crates/pg-engine/src/remote/*|crates/pg-engine/src/ext/fdw.rs|crates/pg-engine/src/ext/postgres_fdw*|crates/pg-engine/src/ext/dblink.rs)
      echo fdw_loopback fdw_traffic outbound ;;
    crates/pg-engine/src/log/*|crates/pg-engine/src/log.rs|crates/pg-engine/src/session/serverlog.rs)
      echo server_log lifecycle_log log_collector connection_log autovacuum_log ;;
    crates/pg-engine/src/autovacuum.rs|crates/pg-engine/src/session/ddl/vacuum*) echo autovacuum autovacuum_log vacuum_toast_measure ;;
    crates/pg-engine/src/server.rs|crates/pg-engine/src/hba.rs|crates/pg-engine/src/auth.rs|crates/pg-engine/src/session/state.rs|crates/pg-engine/src/configuration.rs)
      echo backend_lifecycle backend_status hba connection_log session_state standalone_defaults app_concurrency ;;
    crates/pg-engine/src/server_files.rs|crates/pg-engine/src/server_programs.rs) echo server_files ;;
    crates/pg-engine/src/types/xml*) echo xml_memory ;;
    crates/pg-engine/src/types/collation*|crates/pg-engine/src/types/text.rs) echo locale ;;
  esac
}
SEL="golden wire wire_waits ${QC_TESTS:-}"
[ -n "${QC_ALL:-}" ] && SEL="$SEL ALL"
ENGINE=0
while read -r f; do
  case $f in crates/pg-engine/*|Cargo.toml|Cargo.lock) ENGINE=1 ;; esac
  SEL="$SEL $(tests_for "$f")"
  # The lib's unit tests cost a second build of the engine (cfg(test)): only for a changed file
  # that has some.
  case $f in crates/pg-engine/src/*.rs) grep -q '#\[test\]' "$f" 2>/dev/null && SEL="$SEL lib" ;; esac
done <"$D/changed"
if [ $ENGINE = 0 ]; then
  line diff ok "$t" "$(wc -l <"$D/changed") files changed, none in crates/pg-engine: nothing to check"
  exit 0
fi
CRASH=0
case " $SEL " in *" CRASH "*) skip crash || CRASH=1 ;; esac
case " $SEL " in *" ALL "*)
  SEL="lib $(ls crates/pg-engine/tests/*.rs | sed 's,.*/,,; s,\.rs$,,' | grep -v -x -e pg_regress -e pg_isolation -e pg_dump -e pg_contrib)" ;;
esac
SEL=$(for s in $SEL; do case $s in lib) echo lib ;; *) [ -f "crates/pg-engine/tests/$s.rs" ] && echo "$s" ;; esac; done | sort -u)
ARGS=$(for s in $SEL; do [ "$s" = lib ] && echo --lib || echo "--test $s"; done)
line diff ok "$t" "$(wc -l <"$D/changed") files against ${BASE%"${BASE#?????????}"}; tests: $(echo $SEL)$([ $CRASH = 1 ] && echo ', crash')"
[ -n "${QC_DRY:-}" ] && exit 0

# --- containers, clippy and the build, side by side ----------------------------------------
REF=1
if skip regress && skip iso && skip dump; then REF=0; fi
if [ $REF = 1 ]; then
  [ -n "$P" ] && [ -n "${QC_PORTS:-}" ] || fail diff "$T0" "QC_PREFIX and QC_PORTS (four ports) needed, or QC_SKIP=regress,iso,dump"
  "$TOOLS/ref-containers.sh" up "$P" "$D" "$(echo "$QC_PORTS" | cut -d, -f4)" >"$D/containers.log" 2>&1 &
  CONT=$!
fi
if ! skip clippy; then
  tc=$(now)
  setsid cargo clippy -p pg-engine --all-targets --message-format=short >"$D/clippy.log" 2>&1 &
  CLIPPY=$!; KILLGRPS="$KILLGRPS $CLIPPY"
fi
t=$(now)
artifacts() { python3 -I -c '
import json, sys
for l in sys.stdin:
    if not l.startswith("{"): continue
    m = json.loads(l)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["profile"]["test"]:
        k = m["target"]["kind"][0]
        print(("lib" if k == "lib" else m["target"]["name"]) + sys.argv[1], m["executable"])
' "$1"; }
build() { cargo test --profile "$PROF" -p pg-engine --no-run --message-format=json-render-diagnostics "$@" 2>>"$D/build.log"; }
: >"$D/build.log"; : >"$D/bins"
# The engine and the reference harnesses first, so the reference suites start while the rest links.
if [ $REF = 1 ]; then
  build --test pg_regress --test pg_isolation --test pg_dump | artifacts "" >>"$D/bins"
  [ -n "$(bin pg_dump)" ] || fail build "$t" "$(grep -m1 -E '^error' "$D/build.log")"
  wait "$CONT" || fail containers "$T0" "$(tail -1 "$D/containers.log")"
  : >"$D/ref.status"
  export QC_REF_CHAIN=1 QC_PREFIX QC_PORTS QC_DIR="$D" QC_PROFILE="$PROF" QC_SKIP="${QC_SKIP:-}"
  setsid "$TOOLS/quick-check.sh" </dev/null >"$D/ref-chain.log" 2>&1 &
  REFPID=$!; KILLGRPS="$KILLGRPS $REFPID"
  unset QC_REF_CHAIN
fi
build $ARGS | artifacts "" >>"$D/bins"
[ -n "$(bin wire)" ] && ! grep -q '^error' "$D/build.log" || fail build "$t" "$(grep -m1 -E '^error' "$D/build.log")"
if [ $CRASH = 1 ]; then
  build --test crash_random | artifacts "" >>"$D/bins"
  build --features powerloss --test crash_random | artifacts @power >>"$D/bins"
  [ -n "$(bin crash_random@power)" ] || fail build "$t" "$(grep -m1 -E '^error' "$D/build.log")"
fi
line build ok "$t" "profile $PROF, $(wc -l <"$D/bins") test binaries"

if ! skip clippy; then
  wait "$CLIPPY"; rc=$?
  [ $rc = 0 ] || fail clippy "$tc" "$(grep -m1 -E '^error' "$D/clippy.log")"
  CACHE=${QC_CLIPPY_CACHE:-$HOME/.cache/pg-engine-train/clippy}
  python3 -I - "$D/clippy.log" "$D/changed" "$CACHE/$BASE.txt" "$BASE" >"$D/clippy-new.txt" <<'PY'
import os, re, subprocess, sys
log, changed, cache, base = sys.argv[1:5]
files = set(open(changed).read().split())
pat = re.compile(r'^(\S+?):(\d+):\d+: warning: (.*)$')
now = [(m[1], int(m[2]), m[3]) for m in map(pat.match, open(log, errors='replace')) if m and m[1] in files]
if os.path.exists(cache):
    # The base head's warnings (kept by the batch train): new when a file has more warnings
    # with that message than the base had.
    old = {}
    for m in map(pat.match, open(cache, errors='replace')):
        if m: old[(m[1], m[3])] = old.get((m[1], m[3]), 0) + 1
    seen = {}
    for f, n, msg in now:
        seen[(f, msg)] = seen.get((f, msg), 0) + 1
        if seen[(f, msg)] > old.get((f, msg), 0): print(f'{f}:{n}: {msg}')
else:
    # No record of the base: new when on a line the diff added or changed (or in a new file).
    touched, cur = {}, None
    diff = subprocess.run(['git', 'diff', '-U0', base, '--', *sorted(files)], capture_output=True, text=True).stdout
    for l in diff.splitlines():
        if l.startswith('+++ '): cur = l[6:] if l.startswith('+++ b/') else None
        elif l.startswith('@@') and cur:
            m = re.search(r'\+(\d+)(?:,(\d+))?', l)
            touched.setdefault(cur, set()).update(range(int(m[1]), int(m[1]) + int(m[2] or 1)))
    for f, n, msg in now:
        if n in touched.get(f, ()) or f not in touched: print(f'{f}:{n}: {msg}')
PY
  NEW=$(wc -l <"$D/clippy-new.txt")
  [ "$NEW" = 0 ] || fail clippy "$tc" "$NEW new warning(s) in changed files ($D/clippy-new.txt): $(head -1 "$D/clippy-new.txt")"
  line clippy ok "$tc" "$(grep -c ': warning: ' "$D/clippy.log") warnings, none new in changed files"
fi

# --- crate tests and the crash stage -------------------------------------------------------
t=$(now); : >"$D/tests.log"
for s in $SEL; do
  echo "=== $s" >>"$D/tests.log"
  run "$s" >>"$D/tests.log" 2>&1 ||
    fail tests "$t" "$s: $(sed -n "/^=== $s\$/,\$p" "$D/tests.log" | grep -E '^test .* FAILED$' | head -3 | tr '\n' ' ')"
done
line tests ok "$t" "$(grep -o '[0-9]* passed' "$D/tests.log" | awk '{s += $1} END {print s}') passed in $(echo $SEL | wc -w) binaries"
if [ $CRASH = 1 ]; then
  t=$(now)
  CRASH_SEEDS=1,2,3 CRASH_ROUNDS=2 run crash_random >"$D/crash.log" 2>&1 ||
    fail crash "$t" "seeds 1-3 x 2: $(grep -m1 'panicked' "$D/crash.log")"
  run crash_random@power --exact power_cut_at_every_point >>"$D/crash.log" 2>&1 ||
    fail crash "$t" "power_cut_at_every_point: $(grep 'panicked' "$D/crash.log" | tail -1)"
  line crash ok "$t" "seeds 1-3 x 2 rounds, power_cut_at_every_point"
fi

# --- the reference suites' results, as they come -------------------------------------------
if [ $REF = 1 ]; then
  printed=0
  while :; do
    alive=1; kill -0 "$REFPID" 2>/dev/null || alive=0
    while [ "$printed" -lt "$(wc -l <"$D/ref.status")" ]; do
      printed=$((printed + 1))
      set -- $(sed -n "${printed}p" "$D/ref.status")
      [ "$1" = done ] && break 2
      step=$1 res=$2 secs=$3; shift 3
      [ "$res" = ok ] || { echo "qc $step FAIL ${secs}s $* (log: $D/$step.log)"; cleanup; exit 1; }
      echo "qc $step ok ${secs}s $*"
    done
    [ $alive = 1 ] || fail regress "$T0" "the reference suites stopped early ($D/ref-chain.log)"
    sleep 5
  done
  KILLGRPS=
fi
line all ok "$T0" "$(git rev-parse --short=9 HEAD)$([ -n "$(git status --porcelain --untracked-files=no)" ] && echo ' + uncommitted changes')"
