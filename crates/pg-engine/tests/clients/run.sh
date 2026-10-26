#!/bin/sh
# Run a client library's own test suite against the engine or a reference PostgreSQL 18.6.
#
#   tests/clients/run.sh <suite> engine|pg [extra args passed to the suite]
#   tests/clients/run.sh <suite> compare      # tests whose outcome differs between the two
#
# (docker needs the docker group: `sg docker -c 'tests/clients/run.sh ...'`.)
#
# Each suite is suites/<suite>.sh, run inside a container on the host network (the host
# firewall drops container->host traffic over the docker bridge; a host-network container
# reaches 127.0.0.1 directly). Its header names the image (`# image:`) and the pinned source
# tarball (`# src:`), fetched once into $CLIENTS_CACHE/src/<suite>. A suite writes JUnit XML
# (or `PASS|FAIL|SKIP <id>` lines in *.lst) into $OUT.
#
# Environment:
#   CLIENTS_CACHE  sources, package caches and results (default ~/.cache/pg-clients)
#   ENGINE_PORT    the engine on 127.0.0.1 (default 55839); start it yourself, e.g.
#                  roll-engine --data <dir> --listen 127.0.0.1:55839 --password pw
#   PG_PORT        a PostgreSQL 18.6 on 127.0.0.1 (default 55838), password pw:
#                  docker run -d --name clients-pg -p 127.0.0.1:55838:5432 -e POSTGRES_PASSWORD=pw \
#                    -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" <PG_IMAGE below> \
#                    (default settings, as roll-engine-ref)
#   CLIENTS_CPUS   docker --cpus (default 4)
#   CLIENTS_NAME   the suite container's name prefix (default clients; agents sharing a
#                  machine use their own, so two runs of one suite don't collide)
#
# Results: $CLIENTS_CACHE/results/<suite>-<target>.txt (one `PASS|FAIL|SKIP <test id>` per
# line, sorted) and the suite's output in <suite>-<target>.log.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=${1:?suite}
TARGET=${2:?engine, pg or compare}
shift 2
CACHE=${CLIENTS_CACHE:-$HOME/.cache/pg-clients}
RES="$CACHE/results"
mkdir -p "$CACHE/src" "$RES" "$CACHE/home"
PG_IMAGE=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873

if [ "$TARGET" = compare ]; then
  exec python3 -I "$HERE/compare.py" "$RES/$SUITE-engine.txt" "$RES/$SUITE-pg.txt"
fi
case $TARGET in
  engine) PORT=${ENGINE_PORT:-55839} ;;
  pg) PORT=${PG_PORT:-55838} ;;
  *) echo "target must be engine, pg or compare" >&2; exit 2 ;;
esac

SCRIPT="$HERE/suites/$SUITE.sh"
[ -f "$SCRIPT" ] || { echo "no suite $SUITE" >&2; exit 2; }
IMAGE=$(sed -n 's/^# image: *//p' "$SCRIPT" | head -1)
SRC=$(sed -n 's/^# src: *//p' "$SCRIPT" | head -1)
if [ -n "$SRC" ]; then
  "$HERE/fetch.sh" "$CACHE/src/$SUITE" "$SRC"
fi
mkdir -p "$CACHE/src/$SUITE"

# Databases and roles a suite expects, made through the pinned image's psql.
if [ -f "$HERE/suites/$SUITE.setup.sql" ]; then
  docker run --rm -i --network host -e PGPASSWORD=pw "$PG_IMAGE" \
    psql -X -h 127.0.0.1 -p "$PORT" -U postgres -d postgres \
    < "$HERE/suites/$SUITE.setup.sql" > "$RES/$SUITE-$TARGET.setup.log" 2>&1 || true
fi

OUT="$RES/$SUITE-$TARGET"
rm -rf "$OUT.d" "$OUT.txt"
mkdir -p "$OUT.d"
echo "running $SUITE against $TARGET (127.0.0.1:$PORT), log: $OUT.log" >&2
set +e
docker run --rm --network host --cpus="${CLIENTS_CPUS:-4}" --name "${CLIENTS_NAME:-clients}-$SUITE-$TARGET" \
  --user "$(id -u):$(id -g)" -e HOME=/cache/home \
  -v "$CACHE:/cache" -v "$HERE:/harness:ro" \
  -e PGHOST=127.0.0.1 -e PGPORT="$PORT" -e PGUSER=postgres -e PGPASSWORD=pw -e PGDATABASE=postgres \
  -e TARGET="$TARGET" -e SRC="/cache/src/$SUITE" -e OUT="/cache/results/$SUITE-$TARGET.d" \
  -w "/cache/src/$SUITE" "$IMAGE" sh /harness/suites/"$SUITE".sh "$@" > "$OUT.log" 2>&1
STATUS=$?
set -e
python3 -I "$HERE/collect.py" "$OUT.d" > "$OUT.txt"
echo "$SUITE $TARGET: exit $STATUS, $(grep -c '^PASS' "$OUT.txt") pass, $(grep -c '^FAIL' "$OUT.txt") fail, $(grep -c '^SKIP' "$OUT.txt") skip" >&2
