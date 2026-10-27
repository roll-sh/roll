#!/usr/bin/env bash
# Benchmarks roll nosql against ScyllaDB 2026.3.3 and Cassandra 5.0.9 on one machine, with the
# same CPU budget for every server: rolld runs under `taskset -c $CPUS`, each reference in its own
# container with `--cpuset-cpus $CPUS`. Load generators run in containers on $LOAD_CPUS.
#
#   scripts/nosql-bench.sh                       # every target, every workload
#   TARGETS="roll-scylla" WORKLOADS="read" scripts/nosql-bench.sh
#   scripts/nosql-bench.sh cold                  # scale-to-zero cold start (roll) and restarts (references)
#   scripts/nosql-bench.sh profile               # perf profile of rolld under each workload
#   scripts/nosql-bench.sh table [OUT...]        # print the results table (mean over rounds)
#
# Targets: roll-scylla, roll-cassandra (rolld --nosql-only --nosql-single, local-directory storage),
# scylla, cassandra (dedicated containers started and stopped here, one at a time).
# Workloads: write read unprepared mixed lwt batch wide (see `run_workload`).
# Raw tool output goes to $OUT/<target>/<workload>.txt.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
HERE=$ROOT/scripts/nosql-bench
OUT=${OUT:-$ROOT/target/bench-nosql}
ROLLD=${ROLLD:-$ROOT/target/release/rolld}
CPUS=${CPUS:-4,6}
LOAD_CPUS=${LOAD_CPUS:-8-15}
NCPU=$(awk -F, '{print NF}' <<<"$CPUS")
ROWS=${ROWS:-1000000}
DURATION=${DURATION:-30}
THREADS=${THREADS:-64}
REF_MEMORY=${REF_MEMORY:-4g}
TARGETS=${TARGETS:-"roll-scylla roll-cassandra scylla cassandra"}
WORKLOADS=${WORKLOADS:-"write read unprepared mixed lwt batch wide"}
SCYLLA_IMAGE=scylladb/scylla:2026.3.3
CASSANDRA_IMAGE=cassandra:5.0.9
PORT_ROLL=49042 PORT_SCYLLA=49142 PORT_CASSANDRA=49242
API=127.0.0.1:47070
mkdir -p "$OUT"

log() { echo "[nosql-bench $(date +%T)] $*" >&2; }
wait_mem() {
    local want=$1
    while [ "$(awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo)" -lt "$want" ]; do
        log "waiting for $want GB free RAM"
        sleep 10
    done
}

ROLL_PID= CONTAINER= SAMPLER= STATS=
cleanup() {
    [ -n "$SAMPLER" ] && kill "$SAMPLER" 2>/dev/null || true
    [ -n "$ROLL_PID" ] && kill "$ROLL_PID" 2>/dev/null && wait "$ROLL_PID" 2>/dev/null || true
    [ -n "$CONTAINER" ] && docker stop -t 30 "$CONTAINER" >/dev/null 2>&1 || true
    docker ps -q --filter name=bench-nosql-load | xargs -r docker stop >/dev/null 2>&1 || true
    ROLL_PID= CONTAINER= SAMPLER= STATS=
}
trap cleanup EXIT INT TERM

port_of() {
    case $1 in roll-*) echo $PORT_ROLL ;; scylla) echo $PORT_SCYLLA ;; cassandra) echo $PORT_CASSANDRA ;; scylla-shared) echo 19042 ;; esac
}

# Load generators: host network, pinned away from the server CPUs, memory-capped.
load() { wait_mem 6; docker run --rm --name "bench-nosql-load-$$" --network host --cpuset-cpus "$LOAD_CPUS" --memory 2g "$@"; }
cs() { load -v "$HERE:/bench:ro" --entrypoint /opt/cassandra/tools/bin/cassandra-stress $CASSANDRA_IMAGE "$@"; }
latte() { load -v "$HERE:/bench:ro" -w /tmp --entrypoint latte scylladb/latte:latest "$@"; }
sb() { load scylladb/scylla-bench:latest "$@"; }
cql() { # cql <port> <statement>
    load --entrypoint cqlsh $CASSANDRA_IMAGE 127.0.0.1 "$1" --request-timeout 120 -e "$2"
}

wait_cql() {
    local port=$1 i
    for i in $(seq 1 180); do
        if cql "$port" "SELECT release_version FROM system.local" >/dev/null 2>&1; then return 0; fi
        sleep 2
    done
    log "no CQL on $port"
    return 1
}

start_target() {
    local t=$1
    mkdir -p "$OUT/$t"
    case $t in
    roll-*)
        local dialect=${t#roll-} home=$OUT/home-$t
        rm -rf "$home"
        wait_mem 6
        taskset -c "$CPUS" "$ROLLD" --home "$home" --api $API --nosql-only --nosql-single bench --nosql-dialect "$dialect" \
            --nosql-cql 127.0.0.1:$PORT_ROLL --nosql-cql-shard-aware 127.0.0.1:$((PORT_ROLL + 1)) \
            --nosql-alternator 127.0.0.1:48000 --nosql-rest 127.0.0.1:49100 >"$OUT/$t/rolld.log" 2>&1 &
        ROLL_PID=$!
        ;;
    scylla)
        wait_mem 8
        CONTAINER=bench-nosql-scylla STATS=$CONTAINER
        # io_uring (which needs the seccomp profile off) instead of linux-aio: other ScyllaDB
        # containers on the host can use up fs.aio-max-nr, and then this one doesn't start.
        # Drivers that connect to the advertised address (latte, scylla-bench) need it to be the
        # published one, with the contact point's port; the shard-aware port is published as is.
        docker run -d --rm --name $CONTAINER --cpuset-cpus "$CPUS" --memory "$REF_MEMORY" \
            -p 127.0.0.1:$PORT_SCYLLA:9042 -p 127.0.0.1:$((PORT_SCYLLA + 1)):$((PORT_SCYLLA + 1)) \
            --security-opt seccomp=unconfined $SCYLLA_IMAGE --smp "$NCPU" --memory 2G --overprovisioned 0 --developer-mode 1 \
            --reactor-backend io_uring --broadcast-rpc-address 127.0.0.1 --native-shard-aware-transport-port $((PORT_SCYLLA + 1)) >/dev/null
        ;;
    scylla-shared)
        # An already running reference (`--smp 1 --memory 1G --overprovisioned 1`), not started or
        # stopped here: for hosts whose fs.aio-max-nr has no room for another ScyllaDB.
        STATS=nosql-ref-scylla
        ;;
    cassandra)
        wait_mem 8
        CONTAINER=bench-nosql-cassandra STATS=$CONTAINER
        docker run -d --rm --name $CONTAINER --cpuset-cpus "$CPUS" --memory "$REF_MEMORY" -p 127.0.0.1:$PORT_CASSANDRA:9042 \
            -e MAX_HEAP_SIZE=2G -e HEAP_NEWSIZE=400M -e CASSANDRA_BROADCAST_RPC_ADDRESS=127.0.0.1 $CASSANDRA_IMAGE >/dev/null
        ;;
    esac
    [ -n "$CONTAINER" ] && { docker logs -f "$CONTAINER" >"$OUT/$t/server.log" 2>&1 & }
    wait_cql "$(port_of "$t")"
}

# Peak and current RSS of the server (rolld, or the container's cgroup memory), sampled each second.
sample_rss() {
    local t=$1 file=$2
    while :; do
        case $t in
        roll-*) awk '/VmRSS/ {print $2 * 1024}' /proc/"$ROLL_PID"/status 2>/dev/null ;;
        *) docker stats --no-stream --format '{{.MemUsage}}' "$STATS" 2>/dev/null | awk '{print $1}' ;;
        esac
        sleep 1
    done >>"$file"
}

run_workload() {
    local t=$1 w=$2 port node
    port=$(port_of "$t")
    node="-node 127.0.0.1 -port native=$port -mode protocolVersion=4"
    local pop="-pop dist=uniform(1..$ROWS)" ks="-schema keyspace=bench_cs replication(strategy=NetworkTopologyStrategy,datacenter1=1)"
    case $w in
    write) cs write n="$ROWS" cl=ONE $ks -pop seq=1..$ROWS -rate threads=$THREADS $node -log interval=5s ;;
    read) cs read duration="${DURATION}s" cl=ONE $ks $pop -rate threads=$THREADS $node -log interval=5s ;;
    mixed) cs "mixed ratio(write=1,read=1)" duration="${DURATION}s" cl=ONE $ks $pop -rate threads=$THREADS $node -log interval=5s ;;
    unprepared) cs read duration="${DURATION}s" cl=ONE $ks $pop -rate threads=$THREADS $node unprepared -log interval=5s ;;
    batch) cs user profile=/bench/batch.yaml "ops(insert=1)" duration="${DURATION}s" cl=ONE -rate threads=$THREADS $node -log interval=5s ;;
    lwt)
        latte schema /bench/lwt.rn 127.0.0.1:$port
        latte load /bench/lwt.rn 127.0.0.1:$port -P rows=100000 -t 2 -q
        latte run /bench/lwt.rn 127.0.0.1:$port -P rows=100000 -d "${DURATION}s" -t 2 -p 32 -q
        ;;
    wide)
        # 20 partitions of 10k 100-byte rows; each request reads one whole partition in pages of 5000.
        local args=(-nodes 127.0.0.1:$port -keyspace bench_wide -replication-factor 1 -partition-count 20 -clustering-row-count 10000 -clustering-row-size 100)
        sb "${args[@]}" -workload sequential -mode write -concurrency 16 >/dev/null
        sb "${args[@]}" -workload uniform -mode read -rows-per-request 10000 -no-lower-bound -page-size 5000 -concurrency 8 -duration "${DURATION}s"
        ;;
    esac
}

drop_keyspaces() {
    local port=$1 k
    for k in bench_cs bench_batch bench_lwt bench_wide bench_cold; do cql "$port" "DROP KEYSPACE IF EXISTS $k" >/dev/null 2>&1 || true; done
}

# rolld's data directory (several GB after a run) unless KEEP_DATA is set.
drop_data() {
    case $1 in roll-*) [ -n "${KEEP_DATA:-}" ] || rm -rf "$OUT/home-$1" ;; esac
}

bench() {
    local t w
    for t in $TARGETS; do
        mkdir -p "$OUT/$t"
        log "starting $t on CPUs $CPUS"
        if ! start_target "$t"; then
            log "$t did not start; skipped"
            cleanup
            continue
        fi
        : >"$OUT/$t/rss.txt"
        sample_rss "$t" "$OUT/$t/rss.txt" &
        SAMPLER=$!
        for w in $WORKLOADS; do
            log "$t: $w"
            local start
            start=$(wc -l <"$OUT/$t/rss.txt")
            run_workload "$t" "$w" >"$OUT/$t/$w.txt" 2>&1 || log "$t: $w failed (see $OUT/$t/$w.txt)"
            tail -n +"$((start + 1))" "$OUT/$t/rss.txt" >"$OUT/$t/$w.rss"
        done
        drop_keyspaces "$(port_of "$t")"
        cleanup
        drop_data "$t"
    done
}

# Scale to zero: unload the database through the control API (memtables flushed, memory
# released), then time a new driver connection to its first row (`cold.py`: Cluster.connect()
# plus one query, Python driver 3.30). "warm" is the same with the database loaded. Small: one
# row; 1g: ~1 GB of cassandra-stress rows, first row of a scan. The references can't unload:
# their number is the time from the container's (re)start to the first row, polled every ~1 s.
# COLD_TARGETS picks among roll, scylla and cassandra.
cold() {
    local t=roll-scylla id i out size q
    mkdir -p "$OUT/cold"
    py() { load -v "$HERE:/bench:ro" --entrypoint python roll-nosql-drv-python:cassandra-3.30.1 /bench/cold.py "$@"; }
    note() { echo "$*" | tee -a "$OUT/cold/results.txt"; }
    if [[ " ${COLD_TARGETS:-roll scylla cassandra} " == *" roll "* ]]; then
    start_target $t
    cql $PORT_ROLL "CREATE KEYSPACE bench_cold WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}; CREATE TABLE bench_cold.t (id int PRIMARY KEY, v text); INSERT INTO bench_cold.t (id, v) VALUES (1, 'x');"
    id=$(curl -s http://$API/v1/nosql/databases | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')
    for size in small 1g; do
        q="SELECT v FROM bench_cold.t WHERE id = 1"
        if [ $size = 1g ]; then
            # 5.4M rows of a 10-byte key and 5 x 34-byte columns.
            cs write n=5400000 cl=ONE -schema keyspace=bench_cs "replication(strategy=NetworkTopologyStrategy,datacenter1=1)" -pop seq=1..5400000 -rate threads=$THREADS \
                -node 127.0.0.1 -port native=$PORT_ROLL -mode protocolVersion=4 -log interval=30s >"$OUT/cold/load-1g.txt" 2>&1
            q='SELECT "C0" FROM bench_cs.standard1 LIMIT 1'
        fi
        for i in 1 2 3 4 5; do
            note "roll $size warm: $(py 127.0.0.1 $PORT_ROLL "$q")"
        done
        for i in 1 2 3 4 5; do
            local t0 t1
            t0=$(date +%s%N)
            curl -s -X POST http://$API/v1/nosql/databases/"$id"/unload >/dev/null
            t1=$(date +%s%N)
            sleep 2
            local idle
            idle=$(awk '/VmRSS/ {print int($2/1024)}' /proc/"$ROLL_PID"/status)
            out=$(py 127.0.0.1 $PORT_ROLL "$q")
            note "roll $size unloaded: $out unload_ms=$(((t1 - t0) / 1000000)) rss_unloaded=${idle}MiB rss_after=$(awk '/VmRSS/ {print int($2/1024)}' /proc/"$ROLL_PID"/status)MiB"
        done
        # Flush and compaction ran on blocking-pool threads; their allocator heaps go back to the
        # OS when the threads exit (tokio's 10 s keep-alive).
        curl -s -X POST http://$API/v1/nosql/databases/"$id"/unload >/dev/null
        sleep 20
        note "roll $size rss 20 s after unload: $(awk '/VmRSS/ {print int($2/1024)}' /proc/"$ROLL_PID"/status)MiB"
        curl -s http://$API/v1/nosql/databases/"$id"/schema | python3 -c '
import json,sys
d=json.load(sys.stdin)
print("roll '$size' live sstable bytes:", sum(t.get("stats",{}).get("live_bytes",0) for ks in d["keyspaces"] for t in ks.get("tables",[])))' | tee -a "$OUT/cold/results.txt"
    done
    drop_keyspaces $PORT_ROLL
    cleanup
    drop_data $t
    fi
    for t in ${COLD_TARGETS:-roll scylla cassandra}; do
        [ "$t" = roll ] && continue
        start_target $t
        local port
        port=$(port_of $t)
        cql "$port" "CREATE KEYSPACE bench_cold WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}; CREATE TABLE bench_cold.t (id int PRIMARY KEY, v text); INSERT INTO bench_cold.t (id, v) VALUES (1, 'x');"
        for i in 1 2 3; do
            note "$t small warm: $(py 127.0.0.1 "$port" "SELECT v FROM bench_cold.t WHERE id = 1")"
        done
        for i in 1 2 3; do
            local t0 t1
            docker restart -t 30 "$CONTAINER" >/dev/null
            t0=$(date -d "$(docker inspect -f '{{.State.StartedAt}}' "$CONTAINER")" +%s%N)
            until py 127.0.0.1 "$port" "SELECT v FROM bench_cold.t WHERE id = 1" 2 >/dev/null 2>&1; do sleep 0.2; done
            t1=$(date +%s%N)
            note "$t small start to first row: $(((t1 - t0) / 1000000)) ms"
        done
        cleanup
    done
}

# CPU profile of rolld (scylla dialect) under each workload: `perf record` from a container
# (roll-nosql-bench-perf: debian + linux-perf; needs PERFMON), LBR call stacks, 15 s per workload.
profile() {
    local t=roll-scylla w
    mkdir -p "$OUT/profile" "$OUT/$t"
    docker image inspect roll-nosql-bench-perf >/dev/null 2>&1 || printf 'FROM debian:trixie-slim\nRUN apt-get update && apt-get install -y --no-install-recommends linux-perf && rm -rf /var/lib/apt/lists/*\nENTRYPOINT ["perf"]\n' |
        docker build -q -t roll-nosql-bench-perf - >/dev/null
    perf() { docker run --rm --pid=host --cap-add PERFMON --cap-add SYS_PTRACE --cap-add SYS_ADMIN --security-opt seccomp=unconfined \
        -v "$OUT/profile:/out" -v "$(dirname "$ROLLD"):$(dirname "$ROLLD"):ro" roll-nosql-bench-perf "$@"; }
    start_target $t
    for w in ${PROFILE_WORKLOADS:-write read mixed unprepared lwt batch wide}; do
        DURATION=25 run_workload $t "$w" >"$OUT/profile/$w.load.txt" 2>&1 &
        local job=$!
        sleep "${PROFILE_DELAY:-8}"
        perf record -F 1999 --call-graph lbr -p "$ROLL_PID" -o "/out/$w.data" -- sleep 15 >/dev/null 2>&1 || true
        wait $job || true
        perf report -i "/out/$w.data" --no-children --percent-limit 0.4 --stdio -g none --sort symbol 2>/dev/null | grep -v '^#' | grep -v '^$' >"$OUT/profile/$w.txt" || true
        perf report -i "/out/$w.data" --children --percent-limit 2 --stdio -g none --sort symbol 2>/dev/null | grep -v '^#' | grep -v '^$' >"$OUT/profile/$w.children.txt" || true
        log "profile $w: $OUT/profile/$w.txt"
    done
    cleanup
    drop_data $t
}

# One markdown row per target and workload from the raw outputs (the mean over several
# result directories: `scripts/nosql-bench.sh table OUT1 OUT2`).
table() {
    python3 "$HERE/table.py" "${@:-$OUT}"
}

case ${1:-bench} in
bench) bench ;;
cold) cold ;;
profile) profile ;;
table) shift; table "$@" ;;
*) echo "usage: $0 [bench|cold|profile|table]" >&2; exit 2 ;;
esac
