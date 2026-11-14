#!/usr/bin/env bash
# Runs a search crate's tests for aarch64 (Graviton) under qemu-user, so the NEON / dotprod
# kernels are checked against their scalar fallbacks on the architecture they are written for.
#
#   scripts/search-arm-test.sh [crate] [cargo test args...]     (default crate: search-codec)
#
# The host's toolchain (~/.cargo, ~/.rustup, with `rustup target add aarch64-unknown-linux-gnu`)
# cross-compiles inside a small Debian image that adds the aarch64 linker and qemu-user
# (roll-arm-cross, built on first use); qemu emulates a CPU with every Armv8.x feature, dotprod
# included. Set QEMU_CPU (e.g. `cortex-a72`, no dotprod) to exercise the widening fallbacks.
# Env: CARGO_TARGET_DIR (default target/agents/arm), JOBS (4), CPUSET (pin the container, e.g.
# away from CPUs a benchmark is using).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
crate=${1:-search-codec}
shift || true
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target/agents/arm}
IMAGE=roll-arm-cross

docker_() {
  if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "$(printf '%q ' docker "$@")"; fi
}

if [[ -z $(docker_ images -q "$IMAGE") ]]; then
  printf '%s\n' 'FROM debian:trixie-slim' \
    'RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev gcc-aarch64-linux-gnu libc6-dev-arm64-cross qemu-user ca-certificates && rm -rf /var/lib/apt/lists/*' |
    docker_ build -q -t "$IMAGE" - >/dev/null
fi

mkdir -p "$TARGET_DIR"
docker_ run --rm -m 6g --cpus "${JOBS:-4}" ${CPUSET:+--cpuset-cpus "$CPUSET"} --user "$(id -u):$(id -g)" \
  -e HOME="$HOME" -e PATH="$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
  -e CARGO_TARGET_DIR="$TARGET_DIR" -e QEMU_CPU="${QEMU_CPU:-max}" \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER="qemu-aarch64 -L /usr/aarch64-linux-gnu" \
  -v "$HOME/.cargo:$HOME/.cargo" -v "$HOME/.rustup:$HOME/.rustup:ro" -v "$ROOT:$ROOT" -w "$ROOT" \
  "$IMAGE" cargo test -j "${JOBS:-4}" --target aarch64-unknown-linux-gnu -p "$crate" "$@"
