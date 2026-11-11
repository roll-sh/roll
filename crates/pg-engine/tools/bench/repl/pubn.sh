#!/bin/sh
# Walsender fan-out: N PostgreSQL 18.6 subscriptions of one publication catch up on a backlog
# (BENCH.md "Logical replication publishers", "Replication, round 5").
#
#   A=a152 PORT0=53040 ENGINE=<engine> PUB=engine|pg NS="1 4 16" ROUNDS=2 TXNS=20000 \
#     tools/heavy.sh timeout 7200 sh tools/bench/repl/pubn.sh
#
# The publisher holds TXNS transactions of 4 pgbench clients (UPDATE by key, INSERT into a
# history table); N subscriptions, each into a database of its own on one PostgreSQL
# subscriber, are created disabled on slots made before the backlog, enabled at once, and
# timed until every slot confirmed the end. CPU: the engine process / PostgreSQL's
# walsenders; memory: the engine's RSS peak over its RSS before / the walsenders' summed RSS
# peak.
set -eu
. "$(dirname "$0")/lib.sh"
ROUNDS=${ROUNDS:-2}
TXNS=${TXNS:-20000}
[ "$TXNS" -gt 0 ] && [ $((TXNS % 4)) -eq 0 ] || { log "TXNS must be positive and divisible by 4"; exit 1; }
PUB=${PUB:-engine}
NS=${NS:-1 4 16}
P_PUB=$((PORT0 + 1)); P_SUB=$((PORT0 + 2))
cli_up
schema="CREATE TABLE acc (id int PRIMARY KEY, bal int NOT NULL DEFAULT 0, pad text);
INSERT INTO acc SELECT g, 0, repeat('x', 80) FROM generate_series(1, 10000) g;
CREATE TABLE hist (hid bigint PRIMARY KEY, aid int, delta int, t timestamptz);"
cat > "$WORK/pubn.pgb" <<'EOF'
\set id random(1, 10000)
\set d random(-500, 500)
BEGIN;
UPDATE acc SET bal = bal + :d WHERE id = :id;
INSERT INTO hist (hid, aid, delta, t) VALUES (nextval('hist_seq'), :id, :d, now());
END;
EOF

walsenders() {
  root=$(docker inspect -f '{{.State.Pid}}' "$A-rpub")
  for c in $(cat /proc/"$root"/task/*/children); do
    tr '\0' ' ' < /proc/"$c"/cmdline | grep -q '^postgres: walsender' && echo "$c"
  done
}

r=0
while [ $r -lt "$ROUNDS" ]; do
  r=$((r + 1))
  for n in $NS; do
    if [ "$PUB" = pg ]; then pg_up "$A-rpub" $P_PUB; else eng_up "$A-epub" $P_PUB; fi
    # (The subscriber on cores of its own: either publisher has its four alone.)
    CPUS=${SUB_CPUS:-16-19} pg_up "$A-rsub" $P_SUB
    q $P_PUB postgres "$schema CREATE SEQUENCE hist_seq; CREATE PUBLICATION p FOR TABLE acc, hist" >/dev/null
    i=0
    while [ $i -lt "$n" ]; do
      i=$((i + 1))
      q $P_SUB postgres "CREATE DATABASE d$i" >/dev/null
      q $P_SUB "d$i" "$schema" >/dev/null
      q $P_SUB "d$i" "CREATE SUBSCRIPTION s$i CONNECTION 'host=127.0.0.1 port=$P_PUB user=postgres dbname=postgres' PUBLICATION p WITH (copy_data = false, enabled = false)" >/dev/null
    done
    bench_log="$WORK/pubn-round-$r-n$n-pgbench.log"
    pgbench -p $P_PUB -n -c 4 -t $((TXNS / 4)) -f "$WORK/pubn.pgb" postgres > "$bench_log" 2>&1
    check_pgbench "$bench_log" "$TXNS"
    end=$(q $P_PUB postgres "SELECT pg_current_wal_lsn()")
    want=$(rows_digest "$P_PUB" postgres)
    if [ "$PUB" = engine ]; then pid=$(cat "$WORK/$A-epub.pid"); c0=$(cpu_pid "$pid"); m0=$(rss_pid "$pid"); peak_start "$pid" "$WORK/peak"; fi
    # PERF=<a privileged container with perf, --pid=host, $WORK mounted>: a profile of the
    # engine publisher's catch-up in $WORK/perf-<round>-<N>.data (PERF_SECS, default 3).
    if [ -n "${PERF:-}" ] && [ "$PUB" = engine ]; then
      docker exec "$PERF" perf record -F 999 --call-graph "${PERF_CALLGRAPH:-fp}" -C "$E_CPUS" -o "$WORK/perf-$r-$n.data" -- sleep "${PERF_SECS:-3}" >/dev/null 2>&1 &
      sleep 1
    fi
    t=$(now)
    i=0; while [ $i -lt "$n" ]; do i=$((i + 1)); q $P_SUB "d$i" "ALTER SUBSCRIPTION s$i ENABLE" >/dev/null; done
    m=0
    # (Watched from the subscriber, which the publisher keeps told of its WAL end: a query on
    # the publisher every 0.1 s, a new connection each, would be counted as its cost. Its
    # slots' confirmation follows within one feedback.)
    until [ "$(q $P_SUB postgres "SELECT count(*) = $n AND bool_and(latest_end_lsn >= '$end') FROM pg_stat_subscription WHERE worker_type = 'apply'")" = t ]; do
      if [ "$PUB" = pg ]; then
        s=0; for w in $(walsenders); do x=$(rss_pid "$w" 2>/dev/null || echo 0); s=$((s + x)); done
        [ $s -gt $m ] && m=$s
      fi
      sleep 0.1
    done
    el=$(since "$t")
    wait_confirmed_all "$P_PUB" "$end"
    i=0
    while [ "$i" -lt "$n" ]; do
      i=$((i + 1)); got=$(rows_digest "$P_SUB" "d$i")
      [ "$got" = "$want" ] || { log "subscriber d$i differs ($got vs $want)"; exit 1; }
    done
    if [ "$PUB" = pg ]; then
      cpu=0; for w in $(walsenders); do cpu=$(awk -v a="$cpu" -v b="$(cpu_pid "$w")" 'BEGIN {print a + b}'); done
      mem="walsenders ${m} MB"
    else
      cpu=$(awk -v a="$(cpu_pid "$pid")" -v b="$c0" 'BEGIN {print a - b}'); peak_stop "$WORK/peak" > "$WORK/peak-result"; mem="+$(( $(cat "$WORK/peak-result") - m0 )) MB"
    fi
    printf 'round %s pub=%s N=%s time %s s cpu %s s mem %s\n' "$r" "$PUB" "$n" "$el" "$cpu" "$mem"
    pg_down "$A-rsub"
    if [ "$PUB" = pg ]; then pg_down "$A-rpub"; else eng_down "$A-epub"; fi
  done
done
