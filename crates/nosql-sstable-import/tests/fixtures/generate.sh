#!/usr/bin/env bash
# Generates SSTable backup fixtures from a reference container:
#
#   generate.sh <container> <keyspace> <out-dir> [scylla-version ...]
#
# 1. Runs generate.cql (keyspace imp_fx renamed to <keyspace>) plus bulk rows (wide partitions,
#    compression variants) through cqlsh in the container.
# 2. Flushes, takes `nodetool snapshot -t <keyspace>`, and copies the snapshot directories out to
#    <out-dir>/<keyspace>/<table>-<id>/snapshots/<keyspace>/ (the `nodetool snapshot` layout).
# 3. For ScyllaDB, each extra version (`me`, `ms`) is also written with `scylla sstable upgrade
#    --version <v>` into <out-dir>-<v>/ with the same layout.
# 4. Clears the snapshot. The keyspace is left for the caller to compare against and drop.
set -euo pipefail
c=$1 ks=$2 out=$3
shift 3
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# A write time one day ahead (ScyllaDB refuses more than three days).
future=$(( ($(date +%s) + 86400) * 1000000 ))
sed -e "s/imp_fx/$ks/g" -e "s/FUTURE_TS/$future/g" "$here/generate.cql" > "$tmp/gen.cql"
{
  echo "CREATE TABLE $ks.wide (k int, c int, v text, PRIMARY KEY (k, c));"
  for i in $(seq 1 1500); do echo "INSERT INTO $ks.wide (k, c, v) VALUES (1, $i, 'row $i of a wide partition');"; done
  echo "DELETE FROM $ks.wide WHERE k = 1 AND c > 500 AND c <= 550;"
  for t in c_lz4 c_snappy c_deflate c_zstd c_none; do
    for i in $(seq 1 150); do echo "INSERT INTO $ks.$t (k, c, v) VALUES ($((i % 7)), $i, 'value $i $t lorem ipsum dolor sit amet $((i * 7919))');"; done
  done
} >> "$tmp/gen.cql"
# Scylla spells "compression disabled" differently.
if docker exec "$c" sh -c 'command -v scylla' >/dev/null 2>&1; then
  sed -i -e "s/WITH compression = {'enabled': 'false'}/WITH compression = {'sstable_compression': ''}/" \
    -e "s/WITH compression = {'class': /WITH compression = {'sstable_compression': /" "$tmp/gen.cql"
fi
docker cp "$tmp/gen.cql" "$c:/tmp/$ks.cql"
docker exec "$c" cqlsh -f "/tmp/$ks.cql" >/dev/null
docker exec -u root "$c" rm -f "/tmp/$ks.cql"
sleep 2 # let the 1 s TTLs expire
docker exec "$c" nodetool flush "$ks" >/dev/null
docker exec "$c" nodetool snapshot -t "$ks" "$ks" >/dev/null

data=$(docker exec "$c" sh -c 'ls -d /var/lib/scylla/data 2>/dev/null || ls -d /var/lib/cassandra/data')
mkdir -p "$out/$ks"
for d in $(docker exec "$c" sh -c "ls -d $data/$ks/*/snapshots/$ks"); do
  t=$(basename "$(dirname "$(dirname "$d")")")
  mkdir -p "$out/$ks/$t/snapshots"
  docker cp "$c:$d" "$out/$ks/$t/snapshots/$ks"
  for v in "$@"; do
    w=/tmp/$ks-$v-$t
    docker exec "$c" sh -c "rm -rf $w && mkdir -p $w && cd $d && scylla sstable upgrade --sstable-version $v --all --output-dir $w --memory 256M --smp 1 \$(ls *-Data.db | sed 's|^|./|') >/dev/null 2>&1 && cp schema.cql $w/"
    mkdir -p "$out-$v/$ks/$t/snapshots"
    docker cp "$c:$w" "$out-$v/$ks/$t/snapshots/$ks"
    docker exec -u root "$c" rm -rf "$w"
  done
done
docker exec "$c" nodetool clearsnapshot -t "$ks" -- "$ks" >/dev/null 2>&1 || docker exec "$c" nodetool clearsnapshot -t "$ks" >/dev/null
