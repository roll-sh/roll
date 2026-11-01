# prometheus-community postgres_exporter 0.17.1 (its own image), every collector on (the
# default ones plus database_wraparound, long_running_transactions, postmaster,
# process_idle, stat_activity_autovacuum, stat_checkpointer, stat_statements,
# stat_wal_receiver, statio_user_indexes, xlog_location) and its pg_settings metrics,
# scraped twice through its HTTP endpoint. One id per collector (`collector:<name>`, FAIL
# when pg_scrape_collector_success is 0), per metric family with its type
# (`metric:<name>:<type>`) and per family's label names (`labels:<name>{a,b}`), so a query
# the server answers differently (an error, a column missing or renamed, NULLs a family drops)
# shows as an id of one server only. Collector timings go to $OUT/durations.txt.
# Argument: the exporter's listen port (default 9187).
# image: quay.io/prometheuscommunity/postgres-exporter:v0.17.1
# entrypoint: sh
# src:
set -eu
PORT=${1:-9187}
export DATA_SOURCE_NAME="postgresql://$PGUSER:$PGPASSWORD@$PGHOST:$PGPORT/$PGDATABASE?sslmode=disable"
/bin/postgres_exporter --web.listen-address="127.0.0.1:$PORT" --log.level=warn \
  --collector.database_wraparound --collector.long_running_transactions --collector.postmaster \
  --collector.process_idle --collector.stat_activity_autovacuum --collector.stat_checkpointer \
  --collector.stat_statements --collector.stat_wal_receiver --collector.statio_user_indexes \
  --collector.xlog_location > /tmp/exporter.log 2>&1 &
PID=$!
i=0
until wget -qO /dev/null "http://127.0.0.1:$PORT/metrics" 2>/dev/null; do
  i=$((i + 1)); [ $i -lt 100 ] || { cat /tmp/exporter.log; exit 1; }
  sleep 0.1
done
# (The first scrape warms the exporter's caches; the second is the one compared.)
wget -qO /dev/null "http://127.0.0.1:$PORT/metrics"
wget -qO /tmp/metrics.txt "http://127.0.0.1:$PORT/metrics"
kill $PID
grep -v '^pg_settings\|^# ' /tmp/exporter.log | sed 's/ts=[^ ]* //' | sort -u > "$OUT/exporter-errors.txt" || true
awk '
  /^# TYPE / { print "PASS metric:" $3 ":" $4; next }
  /^#/ { next }
  /^pg_scrape_collector_success\{/ {
    match($0, /collector="[^"]*"/); c = substr($0, RSTART + 11, RLENGTH - 12)
    print (($NF + 0) == 1 ? "PASS" : "FAIL") " collector:" c; next
  }
  /^pg_scrape_collector_duration_seconds\{/ { print > "/tmp/durations.txt" }
  /\{/ {
    name = substr($0, 1, index($0, "{") - 1); rest = substr($0, index($0, "{") + 1)
    rest = substr(rest, 1, index(rest, "}") - 1); keys = ""
    n = split(rest, kv, "\",")
    for (i = 1; i <= n; i++) { k = kv[i]; sub(/=.*/, "", k); keys = keys (keys == "" ? "" : ",") k }
    print "PASS labels:" name "{" keys "}"
  }
' /tmp/metrics.txt | sort -u > "$OUT/results.lst"
cp /tmp/durations.txt "$OUT/durations.txt" 2>/dev/null || true
cp /tmp/metrics.txt "$OUT/metrics.txt"
grep -c . "$OUT/results.lst"
