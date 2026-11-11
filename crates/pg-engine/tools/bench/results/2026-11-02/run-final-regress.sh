#!/bin/bash
set -euo pipefail
cd /home/meszmate/worktrees/roll/t3-d4020b98
rev=5b32046d-release
export TMPDIR=/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/tmp
export CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_TEST_DEBUG=line-tables-only
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
cache=/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102
runroot="$cache/final-regress-$rev"
test ! -e "$runroot/report/results.tsv"
mkdir -p "$TMPDIR" "$runroot"
printf '%s\n' '5b32046d' > "$runroot/commit.txt"
sha256sum '/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/histogram-bin/roll-engine' > "$runroot/engine.sha256"
run_check() {
 name=$1
 shift
 set +e
 timeout 7200 crates/pg-engine/tools/heavy.sh "$@" > "$runroot/$name.log" 2>&1
 result=$?
 set -e
 printf '%s\t%s\n' "$name" "$result" >> "$runroot/status.tsv"
 if [ "$result" -ne 0 ]; then
  tail -n 50 "$runroot/$name.log"
  return "$result"
 fi
}
image=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
ref="t3-d4020b98-regref-perf-$rev"
helper="t3-d4020b98-regpsql-perf-$rev"
ref_started=0
helper_started=0
cleanup() {
 if [ "$helper_started" -eq 1 ]; then docker stop -t 10 "$helper" >> "$runroot/cleanup.log" 2>&1 || true; fi
 if [ "$ref_started" -eq 1 ]; then docker stop -t 20 "$ref" >> "$runroot/cleanup.log" 2>&1 || true; fi
}
trap cleanup EXIT
mkdir -p "$runroot/reference-data" "$runroot/pgr"
docker run -d --rm --name "$ref" --label agent.worktree=t3-d4020b98 --memory 2g -e POSTGRES_PASSWORD=pw -e 'POSTGRES_INITDB_ARGS=--locale=C --encoding=UTF8' -v "$runroot/reference-data:/var/lib/postgresql" "$image" > "$runroot/reference-container.txt"
ref_started=1
for attempt in {1..60}; do
 if docker exec "$ref" pg_isready -U postgres >/dev/null 2>&1; then break; fi
 sleep 1
done
docker exec "$ref" psql -U postgres -Atqc 'SELECT version(), datcollate, datctype FROM pg_database WHERE datname = current_database()' > "$runroot/reference-version.txt"
docker run -d --rm --name "$helper" --label agent.worktree=t3-d4020b98 --memory 256m --network host --user "$(id -u):$(id -g)" -v "$runroot/pgr:$runroot/pgr" --entrypoint sleep "$image" infinity > "$runroot/helper-container.txt"
helper_started=1
if ss -ltnH '( sport = :52807 )' | rg -q .; then
 echo 'Port 52807 is occupied' >&2
 exit 1
fi
run_check regress env PGREGRESS_ENGINE_BIN=/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/histogram-bin/roll-engine PGREGRESS_COMMIT=5b32046d PGREGRESS_BASELINE=/home/meszmate/.cache/pg-engine-perf-20261101/final-regress-7f35a185-release/report/results.tsv PGREGRESS_CONTAINER="$ref" PGREGRESS_ENGINE_CONTAINER="$helper" PGREGRESS_DIR="$runroot/pgr" PGREGRESS_PORT=52807 PGREGRESS_DB=regression_sol_combined PGREGRESS_REPORT_DIR="$runroot/report" cargo test --release -p pg-engine --test pg_regress -- --ignored --nocapture
tail -n 18 "$runroot/regress.log"
