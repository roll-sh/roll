#!/bin/sh
# bulk.sh <target> <tag> : the 1M-row bulk steps (BENCH "Bulk writes"), each through step.sh.
# Prints one line per step, then peak memory and disk. target: engine name or pg.
S=$HOME/.cache/a148/bench/step.sh
A=$HOME/.cache/a148
t=$1; tag=$2
if [ "$t" = pg ]; then port=${PG_PORT_T:-52960}; else port=$(cat "$A/eng-$t/port"); fi
q() { docker exec -e PGPASSWORD=pw a148-cli psql -X -q -At -h 127.0.0.1 -p "$port" -U postgres -d "$1" -c "$2"; }
q postgres "DROP DATABASE IF EXISTS bench" >/dev/null 2>&1
q postgres "CREATE DATABASE bench" >/dev/null
q bench "SET maintenance_work_mem = '256MB'" >/dev/null
q bench "ALTER DATABASE bench SET maintenance_work_mem = '256MB'" >/dev/null
q bench "CREATE TABLE bulk (id int NOT NULL, a int NOT NULL, b text, c float8) WITH (autovacuum_enabled = off); CREATE TABLE bulk2 (LIKE bulk) WITH (autovacuum_enabled = off)" >/dev/null
st() { "$S" "$t" "$1" bench "$2" | sed "s/^/$tag\t/"; }
st copy_in "\\copy bulk from '/data/bulk1m.tsv'"
st create_index "CREATE INDEX bulk_a ON bulk (a)"
st create_unique_index "CREATE UNIQUE INDEX bulk_id ON bulk (id)"
st copy_out "\\copy bulk to '/dev/null'"
st insert_select "INSERT INTO bulk2 SELECT * FROM bulk"
st update_half "UPDATE bulk SET c = c + 1 WHERE id % 2 = 0"
st delete_quarter "DELETE FROM bulk WHERE id % 4 = 1"
st vacuum "VACUUM bulk"
q bench "SELECT 'autovac', autovacuum_count, vacuum_count, last_autovacuum IS NOT NULL FROM pg_stat_user_tables WHERE relname = 'bulk'" | sed "s/^/$tag\t/"
st analyze "ANALYZE bulk"
st count_after "SELECT count(*), sum(c) FROM bulk"
if [ "$t" = pg ]; then
  du=$(docker exec ${PG_NAME_T:-a148-pg} sh -c 'du -sm $PGDATA' | cut -f1)
else
  du=$(du -sm "$A/eng-$t/data" | cut -f1)
fi
st truncate "TRUNCATE bulk2"
st drop_table "DROP TABLE bulk"
if [ "$t" = pg ]; then
  id=$(docker inspect -f '{{.Id}}' ${PG_NAME_T:-a148-pg})
  peak=$(( $(cat /sys/fs/cgroup/system.slice/docker-$id.scope/memory.peak) / 1048576 ))
  du2=$(docker exec ${PG_NAME_T:-a148-pg} sh -c 'du -sm $PGDATA' | cut -f1)
else
  peak=$(( $(awk '/VmHWM/ {print $2}' /proc/$(cat "$A/eng-$t/pid")/status) / 1024 ))
  du2=$(du -sm "$A/eng-$t/data" | cut -f1)
fi
printf '%s\t%s\tpeak_mb %s\tdisk_before_drop %s\tdisk_end %s\n' "$tag" "$t" "$peak" "$du" "$du2"
