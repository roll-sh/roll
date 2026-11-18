# Sourced by quick-check.sh (its reference chain) and verification-train.sh (lane 3): the
# reference suites, each on its own engine port, and run again on a fresh port when its engine
# did not start (a port taken for a moment, os error 98: infrastructure, not a test failure).
#
#   ref_suite <regress|iso|dump> <engine port> <isolation reference port>
#
# The caller defines run (run <test binary name> <args...>) and sets P (container prefix), D (the
# suites' directory) and TOOLS. Sets REF_OK (1 when the suite met its bar: regress 0 newly
# differing units, isolation 1,647 / 1,648 permutations, pg_dump_tap 14,609 passed and 0 failed),
# REF_RC, REF_DETAIL (the counts, or why it failed: tools/train-why.py) and REF_SECS; writes
# $D/<suite>.fails (one "<test>\t<message>" line per failing test) and, per run whose engine
# did not start, a line to $D/infra (that run's log is kept as $D/<suite>.try<N>.log).
# REF_START_TRIES (default 3): runs before an engine that never starts fails the suite.
ref_suite() {
  suite=$1 port=$2 iref=$3 t0=$(date +%s) try=1 why=
  rm -f "$D/$suite.fails" "$D/$suite".try*.log
  while :; do
    case $suite in
      regress)
        rm -rf "$D/pgr/report"; rm -f "$D/pgr/out/engine.log"; elogs=$D/pgr/out/engine.log
        PGREGRESS_DIR=$D/pgr PGREGRESS_DB=regression_$P PGREGRESS_PORT=$port PGREGRESS_CONTAINER=$P-rref \
          PGREGRESS_ENGINE_CONTAINER=$P-psql PGREGRESS_REPORT_DIR=$D/pgr/report \
          run pg_regress pg_regress --exact --ignored --nocapture >"$D/regress.log" 2>&1 ;;
      iso)
        rm -rf "$D/iso/report"; rm -f "$D/iso/out/engine.log"; elogs=$D/iso/out/engine.log
        PGISOLATION_DIR=$D/iso PGISOLATION_PORT=$port PGISOLATION_REF=127.0.0.1:$iref PGISOLATION_MAX_PREPARED=5 \
          PGISOLATION_REPORT_DIR=$D/iso/report run pg_isolation --ignored --nocapture >"$D/iso.log" 2>&1 ;;
      dump)
        rm -rf "$D/pgdump/report"; rm -f "$D/pgdump/engine.log" "$D/pgdump/tap-engine.log"
        elogs="$D/pgdump/engine.log $D/pgdump/tap-engine.log"
        PGDUMP_DIR=$D/pgdump PGDUMP_CLIENT=$P-dclient PGDUMP_PORT=$port PGDUMP_REPORT_DIR=$D/pgdump/report \
          PGDUMP_SRC=$D/pgr/src-repo run pg_dump --ignored --nocapture --exact pg_dump_tap >"$D/dump.log" 2>&1 ;;
    esac
    REF_RC=$?
    why=
    [ $REF_RC != 0 ] || break
    # shellcheck disable=SC2086
    why=$(python3 -I "$TOOLS/train-why.py" start "$D/$suite.log" $elogs) || { why=; break; }
    [ $try -lt "${REF_START_TRIES:-3}" ] || break
    new=$("$TOOLS/ports.sh" take $$ 1) || break
    mv "$D/$suite.log" "$D/$suite.try$try.log"
    echo "$suite: $why, engine port $port (log $D/$suite.try$try.log); ran again on port $new" >>"$D/infra"
    port=$new try=$((try + 1))
  done
  REF_SECS=$(( $(date +%s) - t0 ))
  REF_OK=0
  case $suite in
    regress)
      nd=$(grep -o 'Newly differing units: [0-9]*' "$D/pgr/report/REPORT.md" 2>/dev/null | grep -o '[0-9]*$')
      m=$(grep -o '[0-9]*/[0-9]* units match' "$D/regress.log" | tail -1)
      [ $REF_RC = 0 ] && [ "${nd:-x}" = 0 ] && REF_OK=1 REF_DETAIL="$m, 0 newly differing" ;;
    iso)
      m=$(grep -o '[0-9]*/[0-9]* permutations' "$D/iso.log" | tail -1)
      [ $REF_RC = 0 ] && [ "${m%%/*}" -ge 1647 ] 2>/dev/null && REF_OK=1 REF_DETAIL=$m ;;
    dump)
      m=$(grep -o '002_pg_dump.pl: [0-9]* passed, [0-9]* failed' "$D/dump.log" | tail -1)
      [ $REF_RC = 0 ] && [ "$(echo "$m" | awk '{print ($2 >= 14609 && $4 == 0)}')" = 1 ] && REF_OK=1 REF_DETAIL=${m#002_pg_dump.pl: } ;;
  esac
  if [ $REF_OK = 1 ]; then
    [ $try = 1 ] || REF_DETAIL="$REF_DETAIL (the engine did not start $((try - 1)) time(s): ran again on port $port, $D/infra)"
    : >"$D/$suite.fails"
    return 0
  fi
  [ $REF_RC != 0 ] || REF_RC=1
  if [ -n "$why" ]; then
    REF_DETAIL="$why, $try time(s)"
    printf '(engine did not start)\t%s\n' "$why" >"$D/$suite.fails"
    return 1
  fi
  case $suite in
    regress) REF_DETAIL=$(python3 -I "$TOOLS/train-why.py" regress "$D/regress.log" "$D/pgr/report/REPORT.md" "$D/regress.fails") ;;
    iso) REF_DETAIL="$(python3 -I "$TOOLS/train-why.py" iso "$D/iso.log" "$D/iso/report/REPORT.md" "$D/iso.fails") (1,647 needed)" ;;
    dump) REF_DETAIL="${m:-no count}: $(python3 -I "$TOOLS/train-why.py" dump "$D/dump.log" "$D/pgdump/report/TAP.md" "$D/dump.fails") (14,609 passed, 0 failed needed)" ;;
  esac
  return 1
}
