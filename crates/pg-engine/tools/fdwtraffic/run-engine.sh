#!/bin/sh
# The fdw traffic workload (pgtraffic.py) on a fresh engine whose postgres_fdw connects to
# itself through the counting proxy (fdwproxy.py): round trips, bytes, wall time, the engine's
# peak RSS and CPU. BENCH.md "postgres_fdw, round 6".
#
#   tools/fdwtraffic/run-engine.sh <roll-engine binary> <scratch dir> <psql container> <port>
#
# The psql container runs on the host network with <scratch dir> mounted at the same path (the
# script's `\!` reads the proxy's counters file there). <port> and <port>+1 must be free. For
# PostgreSQL: start the pinned image on the host network at a port P, the proxy at P+1 in front
# of it, `pgtraffic.py gen P+1 <counters path as the container sees it>`, run it with psql and
# summarize with `pgtraffic.py sum`.
eng=$1; T=$2; C=$3; E=$4
H=$(cd "$(dirname "$0")" && pwd)
L=$(printf '%d.%d.%d.%d' 127 0 0 1)
P=$((E + 1))
rm -rf "$T/data"; mkdir -p "$T/data"
nice -n 19 "$eng" --data "$T/data" --listen $L:$E --allow-outbound $L:$P --password pw > "$T/engine.log" 2>&1 &
epid=$!
nice -n 19 python3 -I "$H/fdwproxy.py" $P $E "$T/counters" > "$T/proxy.log" 2>&1 &
ppid=$!
for i in $(seq 1 100); do ss -ltn | grep -q ":$E " && ss -ltn | grep -q ":$P " && break; sleep 0.2; done
python3 -I "$H/pgtraffic.py" gen $P "$T/counters" > "$T/work.sql"
s=$(date +%s%N)
docker exec -i -e PGPASSWORD=pw "$C" psql -X -h $L -p $E -U postgres -d postgres < "$T/work.sql" > "$T/out.txt" 2>&1
e=$(date +%s%N)
python3 -I "$H/pgtraffic.py" sum < "$T/out.txt"
echo "wall_ms $(( (e - s) / 1000000 ))"
grep VmHWM /proc/$epid/status
awk '{print "cpu_s", ($14+$15)/100}' /proc/$epid/stat
grep -i 'error' "$T/out.txt" | head -3
kill $ppid $epid; wait $epid 2>/dev/null
