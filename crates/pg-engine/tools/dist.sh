#!/bin/sh
# The shipped roll-engine without a profile: `cargo build --profile dist` with the flags of
# tools/dist-flags.sh (HANDOFF "Build flags"). Benchmarks and gates of "plain dist" use this
# build; a bare `cargo build --profile dist` lacks the flags and its loops sit differently.
#
#   crates/pg-engine/tools/dist.sh [more cargo build args]      -> <target>/dist/roll-engine
#
# Runs under tools/heavy.sh. RUSTFLAGS from the environment come first (e.g. -Cprofile-use).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
if [ -z "${DIST_IN_HEAVY:-}" ]; then DIST_IN_HEAVY=1 exec "$HERE/heavy.sh" "$0" "$@"; fi
export PATH="$HOME/.cargo/bin:$PATH"
. "$HERE/dist-flags.sh"
cd "$HERE/../../.."
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$DIST_RUSTFLAGS" exec cargo build --profile dist -p pg-engine --bin roll-engine "$@"
