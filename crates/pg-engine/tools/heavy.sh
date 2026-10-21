#!/bin/sh
# Run one heavy job (cargo build/test/clippy, a regress or golden run) at a time across all
# agents on this machine, and keep the disk from filling up.
#
#   crates/pg-engine/tools/heavy.sh cargo test -p pg-engine --test golden
#
# Several agents building the engine and driving the reference container at once ran the
# machine out of RAM; this lock serialises the heavy part while the others read and edit.
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
LOCK=${HEAVY_LOCK:-/tmp/pg-engine-heavy.lock}
while ! mkdir "$LOCK" 2>/dev/null; do
  # Take over a lock whose holder died.
  if [ -f "$LOCK/pid" ] && ! kill -0 "$(cat "$LOCK/pid")" 2>/dev/null; then rm -rf "$LOCK"; continue; fi
  sleep 5
done
echo $$ > "$LOCK/pid"
trap 'rm -rf "$LOCK"' EXIT INT TERM
# macOS split debuginfo leaves every build's .o files in target/debug/deps and cargo never
# removes them (1.5 million files, ~500 GB, once). Keep only recent ones.
find "$ROOT/target/debug/deps" -name '*.o' -mmin +240 -delete 2>/dev/null
# Incremental caches regrow quickly; drop them when the disk gets low.
FREE_GB=$(df -Pk "$ROOT" | awk 'NR==2 {print int($4/1048576)}')
if [ "${FREE_GB:-0}" -lt 15 ]; then
  rm -rf "$ROOT/target/debug/incremental"
  echo "heavy.sh: ${FREE_GB}G free, cleared target/debug/incremental" >&2
fi
"$@"
