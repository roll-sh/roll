#!/bin/sh
# Side by side with PostgreSQL 18.6: the same workload on a PostgreSQL and an engine
# publisher, each replicated into a PostgreSQL and an engine subscriber. Every scenario runs in databases of its own; what its check
# queries print on each subscriber (md5 of every row, catalog and statistics columns) must
# equal what PostgreSQL publisher -> PostgreSQL subscriber prints.
#
#   A=a152 PORT0=53040 ENGINE=<engine> tools/heavy.sh timeout 7200 sh tools/bench/repl/e2e.sh [scenario...]
#
# Scenarios (src/test/subscription's TAP tests they follow): stream_abort (016, 018, 019),
# filter_cols (028, 031), via_root (013), twophase (021, 023), on_error (029),
# conflicts (026, 035), run_as_owner (033), failover, origin (030).
set -eu
. "$(dirname "$0")/lib.sh"
P_RPUB=$((PORT0 + 11)); P_RSUB=$((PORT0 + 12)); P_EPUB=$((PORT0 + 13)); P_ESUB=$((PORT0 + 14))
ALL="stream_abort filter_cols via_root twophase on_error conflicts run_as_owner failover origin"
SCEN=${*:-$ALL}
FAIL=0
cli_up

up() {
  pg_up "$A-xrpub" $P_RPUB -c logical_decoding_work_mem=64kB -c track_commit_timestamp=on
  pg_up "$A-xrsub" $P_RSUB -c track_commit_timestamp=on
  eng_up "$A-xepub" $P_EPUB -c logical_decoding_work_mem=64kB -c track_commit_timestamp=on
  eng_up "$A-xesub" $P_ESUB -c track_commit_timestamp=on
}
down() {
  pg_down "$A-xrpub"; pg_down "$A-xrsub"; eng_down "$A-xepub"; eng_down "$A-xesub"
}

# The four (publisher, subscriber) pairs: "<pub port> <sub port> <tag>".
pairs() {
  echo "$P_RPUB $P_RSUB rr"
  echo "$P_RPUB $P_ESUB re"
  echo "$P_EPUB $P_RSUB er"
  echo "$P_EPUB $P_ESUB ee"
}

# on_pubs DB SQL / on_subs DB SQL: on both publishers / in each subscriber's two databases
# (DB_r: from PostgreSQL, DB_e: from the engine).
on_pubs() { q $P_RPUB "$1" "$2"; q $P_EPUB "$1" "$2"; }
on_subs() { for p in $P_RSUB $P_ESUB; do q $p "${1}_r" "$2"; q $p "${1}_e" "$2"; done; }

# fresh DB: the scenario's databases.
fresh() {
  for p in $P_RPUB $P_EPUB; do q $p postgres "DROP DATABASE IF EXISTS $1 WITH (FORCE)" >/dev/null; q $p postgres "CREATE DATABASE $1" >/dev/null; done
  for p in $P_RSUB $P_ESUB; do
    for s in r e; do
      for sub in $(q $p "${1}_$s" "SELECT subname FROM pg_subscription" 2>/dev/null); do
        q $p "${1}_$s" "ALTER SUBSCRIPTION $sub DISABLE; ALTER SUBSCRIPTION $sub SET (slot_name = NONE); DROP SUBSCRIPTION $sub" >/dev/null 2>&1 || true
      done
      q $p postgres "DROP DATABASE IF EXISTS ${1}_$s WITH (FORCE)" >/dev/null; q $p postgres "CREATE DATABASE ${1}_$s" >/dev/null
    done
  done
  for p in $P_RPUB $P_EPUB; do q $p postgres "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE NOT active" >/dev/null; done
}

# subscribe DB NAME PUBS OPTS: a subscription in each subscriber database, to its publisher.
subscribe() {
  pairs | while read -r pp sp tag; do
    db=${1}_$(echo "$tag" | cut -c1)
    q "$sp" "$db" "CREATE SUBSCRIPTION ${2}_$tag CONNECTION 'host=127.0.0.1 port=$pp user=postgres dbname=$1' PUBLICATION $3 WITH ($4)" >/dev/null
  done
}

# caught_up DB: every enabled subscription of the scenario confirmed its publisher's end.
caught_up() {
  for pp in $P_RPUB $P_EPUB; do
    end=$(q $pp "$1" "SELECT pg_current_wal_lsn()")
    for _ in $(seq 1200); do
      n=$(q $pp "$1" "SELECT count(*) FROM pg_replication_slots WHERE database = '$1' AND slot_type = 'logical' AND (NOT active OR confirmed_flush_lsn < '$end') AND slot_name NOT LIKE 'pg_%_sync_%' AND slot_name NOT LIKE 'off_%'")
      [ "$n" = 0 ] && break
      sleep 0.2
    done
    [ "$n" = 0 ] || { log "$1: publisher $pp has $n slots behind"; return 1; }
  done
  sleep 1
}

# check DB NAME SQL: SQL's output on every subscriber database equal to PostgreSQL ->
# PostgreSQL's.
check() {
  ref=$(q $P_RSUB "${1}_r" "$3") || return 1
  ok=1
  for x in "$P_ESUB ${1}_r re" "$P_RSUB ${1}_e er" "$P_ESUB ${1}_e ee"; do
    set -- "$1" "$2" "$3" $x
    got=$(q "$4" "$5" "$3") || return 1
    if [ "$got" != "$ref" ]; then
      ok=0; FAIL=1
      log "DIFF $1/$2 [$6]:"; printf 'rr: %s\n%s: %s\n' "$ref" "$6" "$got" | head -20 >&2
    fi
  done
  [ $ok = 1 ] && log "same $1/$2: $(echo "$ref" | head -c 100 | tr '\n' ' ')"
}

# md5 of every row of each table, in order.
rowsums() {
  for t in "$@"; do printf "SELECT '%s', count(*), md5(coalesce(string_agg(x::text, ',' ORDER BY x::text), '')) FROM %s x;" "$t" "$t"; done
}

. "$(dirname "$0")/e2e_scenarios.sh"

up
for scenario in $SCEN; do
  log "== $scenario"
  # A conditional function call disables errexit inside every helper it calls.
  # Run each scenario with errexit active and collect its status afterwards.
  set +e
  (set -e; "sc_$scenario")
  status=$?
  set -e
  [ "$status" = 0 ] || { log "scenario $scenario failed ($status)"; FAIL=1; }
done
down
[ $FAIL = 0 ] && log "all scenarios identical" || { log "differences found"; exit 1; }
