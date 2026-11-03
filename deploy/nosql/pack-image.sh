#!/usr/bin/env bash
# Pack already-built binaries with a runtime matching their build host's libc.
set -euo pipefail
cd "$(dirname "$0")/../.."
bin_dir=${1:?binary directory required}
runtime=${2:?matching runtime image required}
tag=${3:-roll/nosql:test}
ctx=$(mktemp -d)
trap 'rm -rf "$ctx"' EXIT
for b in rolld roll; do
  cp "$bin_dir/$b" "$ctx/$b"
  strip "$ctx/$b"
done
cp deploy/nosql/{docker-entrypoint.sh,cqlsh,healthcheck,Dockerfile.prebuilt} "$ctx/"
docker build --build-arg "RUNTIME=$runtime" -f "$ctx/Dockerfile.prebuilt" -t "$tag" "$ctx"
# Fail here if the binaries cannot run on the selected libc, before starting any suites.
docker run --rm --entrypoint /usr/local/bin/rolld "$tag" --version
docker run --rm --entrypoint /usr/local/bin/roll "$tag" --version
