#!/bin/sh
# Shared helpers of the logical replication benchmarks and side-by-side checks (BENCH.md
# "Replication, round 5"). Sourced by apply.sh, pubn.sh, pgb.sh
# and e2e.sh. Everything an agent starts is named "$A-..." and uses ports $PORT0.. $PORT0+19
# (agent NN: 50000 + 20 * NN), which must be free; PostgreSQL runs on the host network.
#
#   A=a152 PORT0=53040 ENGINE=<roll-engine> WORK=~/.cache/a152/bench tools/heavy.sh timeout 7200 sh tools/bench/repl/apply.sh
#
# Settings (environment):
#   A          container / data prefix (required)          PORT0   first port of the range
#   ENGINE     the engine binary under test                 WORK    scratch directory
#   PG_CPUS    cpuset of the PostgreSQL containers (20-23)  E_CPUS  taskset of engines (24-27)
#   C_CPUS     cpuset of the pgbench / psql container (28-29)
set -eu
: "${A:?set A to your agent prefix, e.g. a152}"
PORT0=${PORT0:?set PORT0 to the first port of your range}
WORK=${WORK:-$HOME/.cache/$A/bench}
PG_CPUS=${PG_CPUS:-20-23}
E_CPUS=${E_CPUS:-24-27}
C_CPUS=${C_CPUS:-28-29}
IMG=${IMG:-postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873}
CLI=$A-rcli
mkdir -p "$WORK"
WT=$(basename "$(git rev-parse --show-toplevel)")
RUN_ID=$(date +%s)-$$
PG_STARTED=""
ENG_STARTED=""
CLI_STARTED=0
ARCHIVE=0

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

port_free() { ! ss -ltn "sport = :$1" | grep -q LISTEN; }

need_free() {
  for p in "$@"; do
    port_free "$p" || { log "port $p is in use; refusing to start there"; exit 1; }
  done
}

# The psql / pgbench client: a container of the pinned image on the host network.
cli_up() {
  if ! docker inspect "$CLI" >/dev/null 2>&1; then
    docker run -d --init --name "$CLI" --network host --label "agent.worktree=$WT" --memory 2g \
      --cpuset-cpus "$C_CPUS" --user "$(id -u):$(id -g)" -v "$WORK:$WORK" --entrypoint sleep "$IMG" infinity >/dev/null
    CLI_STARTED=1
  fi
  [ "$(docker inspect -f '{{index .Config.Labels "agent.worktree"}}' "$CLI")" = "$WT" ] || {
    log "client $CLI belongs to another worktree; choose another A"; exit 1;
  }
  if [ "$(docker inspect -f '{{.State.Running}}' "$CLI")" != true ]; then
    docker start "$CLI" >/dev/null
    CLI_STARTED=1
  fi
}

# q PORT DB SQL: one result, unaligned, no headers.
q() { docker exec "$CLI" psql -X -q -At -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$1" -U postgres -d "$2" -c "$3"; }
# qf PORT DB < file
qf() { docker exec -i "$CLI" psql -X -q -At -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$1" -U postgres -d "$2"; }
expect_sqlstate() {
  local result
  if result=$(docker exec "$CLI" psql -X -q -At -v ON_ERROR_STOP=1 -v VERBOSITY=sqlstate \
      -h 127.0.0.1 -p "$1" -U postgres -d "$2" -c "$4" 2>&1); then
    log "expected SQLSTATE $3, but query succeeded: $4"; return 1
  fi
  case "$result" in
    *"ERROR:  $3"*) log "expected SQLSTATE $3 on $1/$2" ;;
    *) log "expected SQLSTATE $3 on $1/$2, got: $result"; return 1 ;;
  esac
}
pgbench() { docker exec "$CLI" nice -n 19 pgbench -h 127.0.0.1 -U postgres "$@"; }

# Refuse measurements of partial or rejected work. expected=0 is a timed run.
check_pgbench() {
  awk -v expected="${2:-0}" '
    /^number of transactions actually processed:/ { n=$NF; sub(/\/.*/, "", n) }
    /^number of failed transactions:/ { if ($5 != 0) bad=1 }
    END { exit(bad || n == "" || n == 0 || (expected > 0 && n != expected)) }
  ' "$1" || { log "pgbench did not complete its workload; see $1"; return 1; }
}

rows_digest() {
  q "$1" "$2" "SELECT (SELECT md5(string_agg(a::text, ',' ORDER BY id)) FROM acc a) || ' ' || (SELECT coalesce(md5(string_agg(h::text, ',' ORDER BY hid)), '') FROM hist h)"
}

# pg_up NAME PORT [-c setting...]: PostgreSQL 18.6, trust auth on 127.0.0.1 only.
pg_up() {
  local n p
  n=$1; p=$2; shift 2
  if docker inspect "$n" >/dev/null 2>&1; then
    [ "$(docker inspect -f '{{index .Config.Labels "agent.repl.run"}}' "$n")" = "$RUN_ID" ] || {
      log "container $n already exists; choose another A to preserve its data"; exit 1;
    }
    pg_down "$n"
    ARCHIVE=$((ARCHIVE + 1))
    docker rename "$n" "$n-saved-$RUN_ID-$ARCHIVE"
  fi
  need_free "$p"
  docker run -d --name "$n" --network host --cpuset-cpus "$PG_CPUS" --memory 2g \
    --label "agent.worktree=$WT" --label "agent.repl.run=$RUN_ID" -e POSTGRES_HOST_AUTH_METHOD=trust \
    -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" "$IMG" \
    -c listen_addresses=127.0.0.1 -c port="$p" -c wal_level=logical -c max_wal_senders=40 \
    -c max_replication_slots=40 -c max_logical_replication_workers=40 -c max_worker_processes=60 \
    -c max_active_replication_origins=80 \
    -c max_prepared_transactions=10 "$@" >/dev/null
  PG_STARTED="$PG_STARTED $n"
  for _ in $(seq 120); do q "$p" postgres "SELECT 1" >/dev/null 2>&1 && return 0; sleep 0.5; done
  log "PostgreSQL $n did not start"; exit 1
}
pg_down() { docker stop "$1" >/dev/null; }

# eng_up NAME PORT [engine args...]: a fresh data directory; its pid in $WORK/NAME.pid, its
# stderr (the server log) in $WORK/NAME.log.
eng_up() {
  local n p
  n=$1; p=$2; shift 2
  eng_down "$n"
  need_free "$p"
  if [ -e "$WORK/$n.data" ]; then
    ARCHIVE=$((ARCHIVE + 1))
    mv "$WORK/$n.data" "$WORK/$n.data-saved-$RUN_ID-$ARCHIVE"
    if [ -e "$WORK/$n.log" ]; then
      mv "$WORK/$n.log" "$WORK/$n.log-saved-$RUN_ID-$ARCHIVE"
    fi
  fi
  nice -n 19 taskset -c "$E_CPUS" "${ENGINE:?set ENGINE}" --data "$WORK/$n.data" --listen "127.0.0.1:$p" \
    -c wal_level=logical -c max_replication_slots=40 -c max_wal_senders=40 \
    -c max_logical_replication_workers=40 --max-prepared-transactions 10 --allow-outbound '127.0.0.1' \
    "$@" > "$WORK/$n.log" 2>&1 &
  echo $! > "$WORK/$n.pid"
  ENG_STARTED="$ENG_STARTED $n"
  for _ in $(seq 120); do q "$p" postgres "SELECT 1" >/dev/null 2>&1 && return 0; sleep 0.5; done
  log "engine $n did not start"; exit 1
}
eng_down() {
  local pid
  [ -f "$WORK/$1.pid" ] || return 0
  pid=$(cat "$WORK/$1.pid")
  case " $ENG_STARTED " in *" $1 "*) ;; *) log "unowned pid file $WORK/$1.pid; refusing to signal it"; return 1 ;; esac
  # (SIGINT: a fast shutdown; a smart one waits for every session, walsenders included.)
  kill -INT "$pid" 2>/dev/null || true
  for _ in $(seq 60); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
  kill -0 "$pid" 2>/dev/null && { log "engine $1 ($pid) still running: SIGKILL"; kill -9 "$pid"; sleep 1; }
  rm -f "$WORK/$1.pid"
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  result=$?
  trap - EXIT INT TERM
  for server in $ENG_STARTED; do eng_down "$server" || true; done
  for server in $PG_STARTED; do pg_down "$server" || true; done
  [ "$CLI_STARTED" = 0 ] || docker stop "$CLI" >/dev/null
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# CPU seconds so far: an engine (its pid) or a container (its cgroup).
cpu_pid() { awk '{printf "%.2f\n", ($14 + $15) / 100}' "/proc/$1/stat"; }
cpu_ctr() {
  id=$(docker inspect -f '{{.Id}}' "$1")
  f=/sys/fs/cgroup/system.slice/docker-$id.scope/cpu.stat
  awk '/^usage_usec/ {printf "%.2f\n", $2 / 1e6}' "$f"
}
rss_pid() { awk '/^VmRSS/ {print int($2 / 1024)}' "/proc/$1/status"; }

# peak_start PID OUTFILE: sample VmRSS every 0.1 s until "$OUTFILE.stop" exists;
# stop and join the sampler with peak_stop OUTFILE.
peak_start() {
  rm -f "$2" "$2.stop"
  peak_rss "$1" "$2" &
  PEAK_PID=$!
}
peak_rss() {
  m=0
  while [ ! -f "$2.stop" ] && kill -0 "$1" 2>/dev/null; do
    r=$(rss_pid "$1" 2>/dev/null || echo 0); [ "$r" -gt "$m" ] && m=$r; sleep 0.1
  done
  echo "$m" > "$2"
}
peak_stop() { touch "$1.stop"; wait "$PEAK_PID"; cat "$1"; }

# wait_confirmed PORT SLOT LSN: until the publisher's slot confirmed LSN (3600 s at most).
wait_confirmed() {
  for _ in $(seq 36000); do
    [ "$(q "$1" postgres "SELECT confirmed_flush_lsn >= '$3' FROM pg_replication_slots WHERE slot_name = '$2'")" = t ] && return 0
    sleep 0.1
  done
  log "slot $2 never confirmed $3"; return 1
}

now() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(now)" 'BEGIN {printf "%.2f", b - a}'; }
