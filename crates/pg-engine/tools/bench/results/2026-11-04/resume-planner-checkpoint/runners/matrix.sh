#!/bin/bash
# matrix.sh <out.tsv> <rounds> <servers...>: pgbench select-only / simple-update / tpcb-like
# (simple, extended, prepared; 1, 8, 32 clients) and the 100-row range read, PostgreSQL 18.6 and
# engines (labels with a ~/.cache/a165/bin/<label>/roll-engine) each in its own container on the
# same cores, interleaved, order reversed every other round. CPU ms per transaction from each
# server's cgroup. Env: SECS (10), WORKLOADS, MODES, CLIENTS, SCALE (10).
set -euo pipefail
out=$1; rounds=$2; shift 2; servers=("$@")
E=/home/meszmate/roll/.claude/worktrees/agent-a640c14a8f76f8ea4/crates/pg-engine
D=${MATRIX_DATA:-$HOME/.cache/a165/bench}; mkdir -p "$D"
RAW=${out}.raw; mkdir -p "$RAW"
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
EIMG=debian@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
SC=${SERVER_CPUS:-8-11}; CC=${CLIENT_CPUS:-12-13,18-19}; SECS=${SECS:-10}; SCALE=${SCALE:-10}
WORKLOADS=${WORKLOADS:-select-only simple-update tpcb-like range}
MODES=${MODES:-simple extended prepared}; CLIENTS=${CLIENTS:-1 8 32}
printf '\\set aid random(1, 100000 * :scale - 99)\nSELECT sum(abalance) FROM pgbench_accounts WHERE aid BETWEEN :aid AND :aid + 99;\n' >"$D/range.sql"
port_of() { local i=0; for s in "${servers[@]}"; do [ "$s" = "$1" ] && { echo $((53300 + i)); return; }; i=$((i+1)); done; }
cg_of() { echo "/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "a165-$1").scope"; }
cpu() { awk '/^usage_usec/ {print $2}' "$(cg_of "$1")/cpu.stat"; }
mem() { awk '$1 == "anon" || $1 == "shmem" {s += $2} END {print s + 0}' "$(cg_of "$1")/memory.stat"; }
psqlc() { docker run --rm --network host -e PGPASSWORD=pw "$IMG" psql -h 127.0.0.1 -p "$(port_of "$1")" -U postgres -d postgres -Atc "$2"; }
start() {
  local s=$1 p; p=$(port_of "$s")
  docker rm -f "a165-$s" >/dev/null 2>&1; docker volume rm "a165-$s-data" >/dev/null 2>&1
  if ss -ltn | grep -q ":$p "; then echo "port $p busy" >&2; exit 1; fi
  if [ "$s" = pg ]; then
    docker run -d --memory 4g --label agent.worktree=agent-a640c14a8f76f8ea4 --name a165-pg --cpuset-cpus "$SC" --cpu-shares 2 --shm-size=1g --network host \
      -v a165-pg-data:/var/lib/postgresql -e POSTGRES_PASSWORD=pw -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" \
      "$IMG" -c shared_buffers=2GB -c maintenance_work_mem=256MB -c max_wal_size=8GB -c checkpoint_timeout=30min \
      -c max_connections=300 -c port="$p" -c listen_addresses=127.0.0.1 >/dev/null
  else
    rm -rf "$D/data-$s"; mkdir -p "$D/data-$s"
    local bin=$HOME/.cache/a165/bin/$s
    docker run -d --memory 4g --label agent.worktree=agent-a640c14a8f76f8ea4 --name "a165-$s" --cpuset-cpus "$SC" --cpu-shares 2 --network host --user "$(id -u):$(id -g)" \
      -v "$bin:$bin:ro" -v "$D/data-$s:$D/data-$s" -v /usr/share/zoneinfo:/usr/share/zoneinfo:ro -e TMPDIR="$D/data-$s" \
      "$EIMG" "$bin/roll-engine" --data "$D/data-$s" --listen 127.0.0.1:"$p" --password pw --cache-mb 2048 >/dev/null
  fi
  local i=0; until psqlc "$s" "select 1" >/dev/null 2>&1; do sleep 1; i=$((i+1)); [ $i -lt 90 ] || { echo "$s did not start" >&2; exit 1; }; done
  docker run --rm --network host --cpuset-cpus "$CC" -e PGPASSWORD=pw "$IMG" pgbench -h 127.0.0.1 -p "$p" -U postgres -i -q -s "$SCALE" postgres >"$RAW/init-$s-${r:-setup}.log" 2>&1
  psqlc "$s" "vacuum analyze" >/dev/null 2>&1
}
stop() { docker logs "a165-$1" >"$RAW/server-$1-${r:-setup}.log" 2>&1 || true; docker rm -f "a165-$1" >/dev/null 2>&1; docker volume rm "a165-$1-data" >/dev/null 2>&1; rm -rf "$D/data-$1"; }
# (0 rounds: start the servers with fresh pgbench data and leave them up; "stop" rounds: stop them.)
if [ "$rounds" = 0 ]; then for s in "${servers[@]}"; do start "$s"; done; exit 0; fi
if [ "$rounds" = stop ]; then for s in "${servers[@]}"; do stop "$s"; done; exit 0; fi
trap 'for s in "${servers[@]}"; do stop "$s"; done' EXIT
run() { # run <server> <round> <workload> <mode> <clients>
  local s=$1 r=$2 w=$3 m=$4 c=$5 p c0 c1 o tps n args status=0
  p=$(port_of "$s")
  case $w in select-only) args="-S" ;; simple-update) args="-N" ;; tpcb-like) args="" ;; range) args="-f $D/range.sql" ;; esac
  c0=$(cpu "$s")
  o=$(docker run --rm --network host --cpuset-cpus "$CC" -e PGPASSWORD=pw -v "$D:$D" "$IMG" \
      pgbench -h 127.0.0.1 -p "$p" -U postgres -n -M "$m" -c "$c" -j "$(( c < 4 ? c : 4 ))" -T "$SECS" $args postgres 2>&1) || status=$?
  printf '%s\n' "$o" > "$RAW/$r-$s-$w-$m-$c-$(date +%s%N).pgbench"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$r" "$s" "$w" "$m" "$c" "$status" >> "$RAW/exits.tsv"
  test "$status" -eq 0
  c1=$(cpu "$s")
  tps=$(echo "$o" | sed -n 's/^tps = \([0-9.]*\).*/\1/p' | head -1)
  n=$(echo "$o" | sed -n 's/^number of transactions actually processed: \([0-9]*\).*/\1/p')
  fail=$(echo "$o" | sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p')
  [[ "${n:-0}" =~ ^[0-9]+$ && "${n:-0}" -gt 0 && "${fail:-?}" = 0 && -n "${tps:-}" ]] || { echo "invalid measurement: $r $s $w $m $c" >&2; return 1; }
  printf '%s\t%s\t%s\t%s\t%s\ttps=%s\tn=%s\tfailed=%s\tcpu_ms=%s\tmem_mb=%s\n' "$r" "$s" "$w" "$m" "$c" "${tps:-0}" "${n:-0}" "${fail:-?}" \
    "$(awk -v a="$c0" -v b="$c1" -v n="${n:-0}" 'BEGIN { if (n > 0) printf "%.4f", (b - a) / 1000 / n; else print "NA" }')" \
    "$(( $(mem "$s") / 1048576 ))" | tee -a "${WARMUP_OUT:-$out}"
}
for r in $(seq "$rounds"); do
  for s in "${servers[@]}"; do start "$s"; done
  order=("${servers[@]}"); if [ $((r % 2)) -eq 0 ]; then order=(); for ((i=${#servers[@]}-1; i>=0; i--)); do order+=("${servers[i]}"); done; fi
  for s in "${order[@]}"; do WARMUP_OUT="$RAW/warmup.tsv" run "$s" "$r" select-only prepared 8 >/dev/null; done  # warm-up
  for w in $WORKLOADS; do for m in $MODES; do for c in $CLIENTS; do
    for s in "${order[@]}"; do run "$s" "$r" "$w" "$m" "$c"; done
  done; done; done
  for s in "${servers[@]}"; do
    expected=$(awk -F '\t' -v r="$r" -v s="$s" '$1==r && $2==s && ($3=="simple-update" || $3=="tpcb-like") {split($7,a,"="); n+=a[2]} END {printf "%.0f",n}' "$out")
    check=$(psqlc "$s" "SELECT (SELECT count(*) FROM pgbench_history) = $expected AND (SELECT sum(abalance) FROM pgbench_accounts) = (SELECT coalesce(sum(delta),0) FROM pgbench_history)")
    printf '%s\t%s\texpected_transactions=%s\tconsistent=%s\n' "$r" "$s" "$expected" "$check" | tee -a "$RAW/validation.tsv"
    test "$check" = t
    stop "$s"
  done
done
