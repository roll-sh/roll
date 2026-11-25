#!/usr/bin/env bash
# Snapshots Elasticsearch writes, for roll's restore of them (docs/search/compatibility.md,
# "Restoring snapshots Elasticsearch wrote").
#
#   scripts/search-es-snapshots.sh --write        regenerate the committed fixtures
#                                                 (crates/search-es/tests/fixtures/es-snapshots)
#   scripts/search-es-snapshots.sh [docs]         a larger set (default 5000 documents per index,
#                                                 global state too) in target/es-snapshots, also
#                                                 copied into a MinIO bucket, then the opt-in
#                                                 restore test against both
#
# Elasticsearch 9.5.5 and 8.19.7 run in Docker on 127.0.0.1 with memory caps, MinIO likewise;
# every container is removed by its exact name at the end.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD
write=0
docs=5000
case "${1:-}" in
  --write) write=1; docs=60 ;;
  "") ;;
  *) docs=$1 ;;
esac
docker() { sg docker -c "docker $(printf '%q ' "$@")"; }
names=(roll-es-snapshots-955 roll-es-snapshots-8 roll-es-snapshots-minio)
cleanup() { for n in "${names[@]}"; do docker rm -f "$n" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT
work=$(mktemp -d "${TMPDIR:-/tmp}/roll-es-snapshots.XXXXXX")
chmod 777 "$work"
start() { # name image dir
  docker run -d --name "$1" -m 2g -p 127.0.0.1::9200 -e discovery.type=single-node -e xpack.security.enabled=false \
    -e stack.templates.enabled=false -e xpack.ml.enabled=false -e ES_JAVA_OPTS='-Xms1g -Xmx1g' -e path.repo=/repo \
    -e cluster.routing.allocation.disk.threshold_enabled=false -v "$3:/repo" "docker.elastic.co/elasticsearch/elasticsearch:$2" >/dev/null
  docker port "$1" 9200 | head -1
}
wait_up() { for _ in $(seq 1 90); do curl -sf "http://$1" >/dev/null && return 0; sleep 2; done; echo "Elasticsearch at $1 didn't start" >&2; exit 1; }
mkdir -p "$work/955" "$work/8" && chmod 777 "$work/955" "$work/8"
a955=$(start roll-es-snapshots-955 9.5.5 "$work/955")
a8=$(start roll-es-snapshots-8 8.19.7 "$work/8")
wait_up "$a955"; wait_up "$a8"
global=$([ $write = 1 ] && echo none || echo global)
gen=crates/search-es/tests/fixtures/es-snapshots/generate.py
python3 -I "$gen" "http://$a955" /repo/es-9.5.5 "$docs" "$work/es-9.5.5.expected.json" "$global"
python3 -I "$gen" "http://$a8" /repo/es-8.19.7 "$docs" "$work/es-8.19.7.expected.json" "$global"
if [ $write = 1 ]; then
  python3 -I "$gen" "http://$a955" /repo/es-9.5.5-large 3 "$work/es-9.5.5-large.expected.json" large
  dest=crates/search-es/tests/fixtures/es-snapshots
  rm -rf "$dest/es-9.5.5" "$dest/es-8.19.7" "$dest/es-9.5.5-large"
  cp -r "$work/955/es-9.5.5" "$work/955/es-9.5.5-large" "$work/8/es-8.19.7" "$dest/"
  cp "$work"/es-*.expected.json "$dest/"
  du -sh "$dest"
  exit 0
fi
out=$root/target/es-snapshots
rm -rf "$out" && mkdir -p "$out"
cp -r "$work/955/es-9.5.5" "$work/8/es-8.19.7" "$out/"
cp "$work"/es-*.expected.json "$out/"
# The same repositories in a bucket: Elasticsearch's `s3` layout is its `fs` one.
docker run -d --name roll-es-snapshots-minio -m 512m -p 127.0.0.1::9000 -e MINIO_ROOT_USER=roll -e MINIO_ROOT_PASSWORD=roll-secret \
  cgr.dev/chainguard/minio:latest server /data >/dev/null
minio=$(docker port roll-es-snapshots-minio 9000 | head -1)
for _ in $(seq 1 60); do curl -sf "http://$minio/minio/health/ready" >/dev/null && break; sleep 1; done
mc() { docker run --rm --network container:roll-es-snapshots-minio -v "$out:/src:ro" -e MC_HOST_m=http://roll:roll-secret@127.0.0.1:9000 cgr.dev/chainguard/minio-client:latest "$@"; }
mc mb -q m/es
mc mirror -q /src m/es >/dev/null
ROLL_ES_SNAPSHOTS="$out" ROLL_ES_SNAPSHOTS_S3="http://$minio" cargo test -p search-es --test snapshots restores_larger_snapshots_elasticsearch_wrote -- --ignored --nocapture
