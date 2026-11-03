#!/bin/sh
# Logical replication apply: one publisher's backlog of pgbench-like transactions, applied
# by each subscriber alone (BENCH.md "Logical replication apply").
#
#   A=a152 PORT0=53040 ENGINE=<head> BASE=<base binary, optional> PUB=pg|engine \
#     ROUNDS=2 TXNS=60000 tools/heavy.sh timeout 7200 sh tools/bench/repl/apply.sh
#
# The publisher (PostgreSQL 18.6 or the engine) holds TXNS transactions of 4 pgbench clients:
# UPDATE of a 10,000-row table by key and INSERT into an indexed history table, one in three
# also DELETEs the history's oldest row. Each subscriber (PostgreSQL 18.6, the engine, and the
# BASE engine when given) is created disabled with copy_data = false on a slot of its own
# made before the backlog, then enabled alone and timed until its slot confirmed the end.
# CPU: the engine process / PostgreSQL's whole container; memory: the engine's RSS peak over
# its RSS before / the PostgreSQL apply worker's RSS peak. Rounds start fresh, and the order
# of the subscribers rotates. Every subscriber must end with the publisher's md5 of every row.
set -eu
. "$(dirname "$0")/lib.sh"
ROUNDS=${ROUNDS:-2}
TXNS=${TXNS:-60000}
[ "$TXNS" -gt 0 ] && [ $((TXNS % 4)) -eq 0 ] || { log "TXNS must be positive and divisible by 4"; exit 1; }
PUB=${PUB:-pg}
P_PUB=$((PORT0 + 1)); P_PG=$((PORT0 + 2)); P_HEAD=$((PORT0 + 3)); P_BASE=$((PORT0 + 4))
SUBS="pg head"; [ -n "${BASE:-}" ] && SUBS="$SUBS base"
cli_up

schema="CREATE TABLE acc (id int PRIMARY KEY, bal int NOT NULL DEFAULT 0, pad text);
INSERT INTO acc SELECT g, 0, repeat('x', 80) FROM generate_series(1, 10000) g;
CREATE TABLE hist (hid bigint PRIMARY KEY, aid int, delta int, t timestamptz);
CREATE INDEX hist_aid ON hist (aid);"
cat > "$WORK/apply.pgb" <<'EOF'
\set id random(1, 10000)
\set d random(-500, 500)
\set r random(1, 3)
BEGIN;
UPDATE acc SET bal = bal + :d WHERE id = :id;
INSERT INTO hist (hid, aid, delta, t) VALUES (nextval('hist_seq'), :id, :d, now());
\if :r = 1
DELETE FROM hist WHERE hid = (SELECT min(hid) FROM hist);
\endif
END;
EOF

port_of() { case $1 in pg) echo $P_PG ;; head) echo $P_HEAD ;; base) echo $P_BASE ;; esac; }
md5s() { rows_digest "$1" postgres; }

# The PostgreSQL apply worker's host pid (pg_stat_subscription's is the container's).
pg_apply_pid() {
  root=$(docker inspect -f '{{.State.Pid}}' "$A-rsub")
  for c in $(cat /proc/"$root"/task/*/children); do
    tr '\0' ' ' < /proc/"$c"/cmdline | grep -q 'logical replication apply worker' && { echo "$c"; return; }
  done
  return 0
}

r=0
while [ $r -lt "$ROUNDS" ]; do
  r=$((r + 1))
  if [ "$PUB" = pg ]; then pg_up "$A-rpub" $P_PUB; else ENGINE=${ENGINE} eng_up "$A-epub" $P_PUB; fi
  pg_up "$A-rsub" $P_PG
  ENGINE=$ENGINE eng_up "$A-ehead" $P_HEAD
  [ -n "${BASE:-}" ] && ENGINE=$BASE eng_up "$A-ebase" $P_BASE
  for p in $P_PUB $(for s in $SUBS; do port_of "$s"; done); do q "$p" postgres "$schema" >/dev/null; done
  q $P_PUB postgres "CREATE SEQUENCE hist_seq; CREATE PUBLICATION p FOR TABLE acc, hist" >/dev/null
  for s in $SUBS; do
    q "$(port_of "$s")" postgres "CREATE SUBSCRIPTION s_$s CONNECTION 'host=127.0.0.1 port=$P_PUB user=postgres dbname=postgres' PUBLICATION p WITH (copy_data = false, enabled = false, slot_name = 's_$s')" >/dev/null
  done
  t=$(now)
  bench_log="$WORK/apply-round-$r-pgbench.log"
  pgbench -p $P_PUB -n -c 4 -t $((TXNS / 4)) -f "$WORK/apply.pgb" postgres > "$bench_log" 2>&1
  check_pgbench "$bench_log" "$TXNS"
  log "round $r: backlog of $TXNS on $PUB in $(since "$t") s"
  end=$(q $P_PUB postgres "SELECT pg_current_wal_lsn()")
  want=$(md5s $P_PUB)
  # Rotate the order each round.
  order=$SUBS; i=1; while [ $i -lt $r ]; do order="$(echo "$order" | cut -d' ' -f2-) $(echo "$order" | cut -d' ' -f1)"; i=$((i + 1)); done
  for s in $order; do
    p=$(port_of "$s")
    if [ "$s" = pg ]; then c0=$(cpu_ctr "$A-rsub"); else pid=$(cat "$WORK/$A-e$s.pid"); c0=$(cpu_pid "$pid"); m0=$(rss_pid "$pid"); peak_start "$pid" "$WORK/peak"; fi
    # PERF=<a privileged container with perf, --pid=host, $WORK mounted>: a profile of the
    # head engine's catch-up in $WORK/perf-<round>.data (PERF_SECS seconds, default 4).
    if [ -n "${PERF:-}" ] && [ "$s" = head ]; then
      docker exec "$PERF" perf record -F 999 --call-graph "${PERF_CALLGRAPH:-fp}" -C "$E_CPUS" -o "$WORK/perf-$r.data" -- sleep "${PERF_SECS:-4}" >/dev/null 2>&1 &
      sleep 1
    fi
    t=$(now)
    q "$p" postgres "ALTER SUBSCRIPTION s_$s ENABLE" >/dev/null
    if [ "$s" = pg ]; then
      m=0
      until [ "$(q $P_PUB postgres "SELECT confirmed_flush_lsn >= '$end' FROM pg_replication_slots WHERE slot_name = 's_$s'")" = t ]; do
        w=$(pg_apply_pid); [ -n "$w" ] && { x=$(rss_pid "$w" 2>/dev/null || echo 0); [ "$x" -gt "$m" ] && m=$x; }
        sleep 0.1
      done
      el=$(since "$t"); c1=$(cpu_ctr "$A-rsub"); mem="apply worker ${m} MB"
    else
      wait_confirmed $P_PUB "s_$s" "$end"
      el=$(since "$t"); c1=$(cpu_pid "$pid"); peak_stop "$WORK/peak" > "$WORK/peak-result"; mem="+$(( $(cat "$WORK/peak-result") - m0 )) MB"
    fi
    got=$(md5s "$p"); same=identical
    [ "$got" = "$want" ] || { log "subscriber $s differs ($got vs $want)"; exit 1; }
    printf 'round %s pub=%s sub=%s time %s s cpu %.2f s mem %s rows %s\n' "$r" "$PUB" "$s" "$el" "$(awk -v a="$c1" -v b="$c0" 'BEGIN {print a - b}')" "$mem" "$same"
    q "$p" postgres "ALTER SUBSCRIPTION s_$s DISABLE" >/dev/null
  done
  for s in $SUBS; do q "$(port_of "$s")" postgres "DROP SUBSCRIPTION s_$s" >/dev/null 2>&1 || true; done
  pg_down "$A-rsub"; eng_down "$A-ehead"; eng_down "$A-ebase"
  if [ "$PUB" = pg ]; then pg_down "$A-rpub"; else eng_down "$A-epub"; fi
done
