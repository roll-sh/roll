#!/bin/sh
# The reference containers quick-check.sh and the verification train compare against, and the
# directory they share with them (HANDOFF "How to test"; the pinned 18.6 image).
#
#   tools/ref-containers.sh up <prefix> <dir> <iso-ref-port>   # create, or start, all four
#   tools/ref-containers.sh rm <prefix>                        # remove them (docker rm -f -v)
#   tools/ref-containers.sh port <prefix>                      # the running iref's port
#
# <prefix>-rref     regress reference (default settings, its own network)
# <prefix>-iref     isolation reference, host network, 127.0.0.1:<iso-ref-port>, max_prepared_transactions=5
# <prefix>-psql     the engine side's psql, host network, <dir>/pgr mounted
# <prefix>-dclient  pg_dump client, host network, <dir>/pgdump and <dir>/pgr mounted
# Helpers run as this user: as root, pg_dump's directory output is unreadable to the TAP checks.
# <dir>/pgr/src-repo is a sparse REL_18_6 checkout with src/test/regress and src/bin/pg_dump
# (pg_dump_tap fails on one without the latter).
# The isolation reference's port (take one with tools/ports.sh): an iref made for another port
# is made again, and one that isn't running starts only when the port can be bound now. Exit 3
# when the port is in use (or the reference could not bind it): the caller takes another.
set -eu
if ! docker info >/dev/null 2>&1; then exec sg docker -c "exec $0 $*"; fi
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
cmd=${1:?usage: ref-containers.sh up|rm <prefix> [<dir> <iso-ref-port>]}
P=${2:?prefix}
names="$P-rref $P-iref $P-psql $P-dclient"
if [ "$cmd" = rm ]; then
  for c in $names; do docker rm -f -v "$c" >/dev/null 2>&1 || true; done
  exit 0
fi
# The port an iref container was made for (its "-c port=N" argument).
iref_port() { docker inspect -f '{{range .Args}}{{println .}}{{end}}' "$1" 2>/dev/null | sed -n 's/^port=//p'; }
running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = true ]; }
if [ "$cmd" = port ]; then
  running "$P-iref" && iref_port "$P-iref" | grep . && exit 0
  exit 1
fi
D=${3:?dir}; IREF=${4:?isolation reference port}
mkdir -p "$D/pgr" "$D/pgdump" "$D/iso"
if [ ! -f "$D/pgr/src-repo/src/bin/pg_dump/t/002_pg_dump.pl" ]; then
  rm -rf "$D/pgr/src-repo"
  git clone -q --depth 1 --filter=blob:none --sparse --branch REL_18_6 \
    https://github.com/postgres/postgres.git "$D/pgr/src-repo"
  git -C "$D/pgr/src-repo" sparse-checkout set src/test/regress src/bin/pg_dump
fi
U=$(id -u):$(id -g)
have() { docker inspect -f '{{.Name}}' "$1" >/dev/null 2>&1; }
# A helper made for another directory mounts the wrong one: make it again.
for c in "$P-psql" "$P-dclient"; do
  if have "$c" && ! docker inspect -f '{{range .Mounts}}{{.Source}} {{end}}' "$c" | grep -q "$D/pgr "; then
    docker rm -f -v "$c" >/dev/null
  fi
done
if have "$P-iref" && [ "$(iref_port "$P-iref")" != "$IREF" ]; then docker rm -f -v "$P-iref" >/dev/null; fi
if ! running "$P-iref" && ! "$(dirname "$0")/ports.sh" free "$IREF"; then
  echo "ref-containers.sh: port $IREF is in use (isolation reference)" >&2
  exit 3
fi
have "$P-rref" || docker run -d --name "$P-rref" -e POSTGRES_PASSWORD=pw \
  -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" "$IMG" >/dev/null
have "$P-iref" || docker run -d --name "$P-iref" --network host -e POSTGRES_PASSWORD=pw \
  -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" "$IMG" \
  -c port="$IREF" -c listen_addresses=127.0.0.1 -c max_prepared_transactions=5 >/dev/null
have "$P-psql" || docker run -d --name "$P-psql" --network host --user "$U" -v "$D/pgr:$D/pgr" \
  --entrypoint sleep "$IMG" infinity >/dev/null
have "$P-dclient" || docker run -d --name "$P-dclient" --network host --user "$U" \
  -v "$D/pgdump:$D/pgdump" -v "$D/pgr:$D/pgr" --entrypoint sleep "$IMG" infinity >/dev/null
# The idle timer stops containers quiet for 3 hours.
docker start $names >/dev/null
for i in $(seq 60); do
  docker exec "$P-rref" pg_isready -q 2>/dev/null && docker exec "$P-iref" pg_isready -q -p "$IREF" -h 127.0.0.1 2>/dev/null && exit 0
  # PostgreSQL exits when it can't bind its port (taken between the test above and its start).
  if ! running "$P-iref" && docker logs --tail 20 "$P-iref" 2>&1 | grep -q 'could not bind\|Address already in use'; then
    echo "ref-containers.sh: port $IREF is in use (isolation reference: $(docker logs --tail 20 "$P-iref" 2>&1 | grep -m1 'could not bind'))" >&2
    exit 3
  fi
  sleep 1
done
echo "ref-containers.sh: references not ready after 60 s" >&2
exit 1
