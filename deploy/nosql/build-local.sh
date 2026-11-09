#!/usr/bin/env bash
# Builds the roll/nosql image on a development machine: rolld and roll are compiled in a
# memory-capped bookworm container (so they run on the image's glibc), under the machine-wide
# build lock, into target/nosql-image/, then packed with Dockerfile.prebuilt. CI builds
# deploy/nosql/Dockerfile instead (the same final stage, a multi-stage build).
#
#   deploy/nosql/build-local.sh [--tag roll/nosql:dev] [--profile fast|release]
#
# `fast` (default) is release without LTO and with incremental builds, for iterating; `release`
# is the profile published images use.
set -euo pipefail
cd "$(dirname "$0")/../.."
tag=roll/nosql:dev profile=fast mem=${ROLL_BUILD_MEMORY:-8g}
while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag) tag=$2; shift 2 ;;
    --profile) profile=$2; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
out=target/nosql-image
mkdir -p "$out/target" "$out/cargo" "$out/ctx" "$HOME/.cache"
wait_mem() { # GB
  while [[ $(awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo) -lt $1 ]]; do echo "build-local: waiting for $1 GB free RAM..." >&2; sleep 10; done
}
builder=roll-nosql-builder:bookworm
if ! docker image inspect "$builder" >/dev/null 2>&1; then
  printf 'FROM rust:1-bookworm\nRUN apt-get update && apt-get install -y --no-install-recommends libclang-dev && rm -rf /var/lib/apt/lists/*\n' \
    | docker build -t "$builder" -
fi
env=(-e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4})
if [[ $profile == fast ]]; then
  env+=(-e CARGO_PROFILE_RELEASE_LTO=false -e CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 -e CARGO_PROFILE_RELEASE_INCREMENTAL=true)
fi
dir=$([[ $profile == fast ]] && echo "$out/target-fast" || echo "$out/target")
mkdir -p "$dir"
(
  flock 9
  wait_mem 8
  docker run --rm --memory "$mem" --user "$(id -u):$(id -g)" "${env[@]}" \
    -v "$PWD:/src:ro" -v "$PWD/$dir:/target" -v "$PWD/$out/cargo:/cargo" -w /src "$builder" \
    cargo build --locked --release -p rolld -p roll-cli --bin rolld --bin roll
) 9>"$HOME/.cache/roll-build.lock"
cp deploy/nosql/docker-entrypoint.sh deploy/nosql/cqlsh deploy/nosql/healthcheck deploy/nosql/Dockerfile.prebuilt "$out/ctx/"
for b in rolld roll; do
  cp "$dir/release/$b" "$out/ctx/$b.tmp" && strip "$out/ctx/$b.tmp" && mv "$out/ctx/$b.tmp" "$out/ctx/$b"
done
docker build -q -f "$out/ctx/Dockerfile.prebuilt" -t "$tag" "$out/ctx"
echo "built $tag ($(docker image inspect -f '{{.Size}}' "$tag" | awk '{printf "%.0f MB", $1/1e6}'))"
