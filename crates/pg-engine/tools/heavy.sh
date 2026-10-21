#!/bin/sh
# Run heavy jobs (cargo build/test/clippy, a regress or golden run) a few at a time across all
# agents on this machine, and keep the disk from filling up.
#
#   crates/pg-engine/tools/heavy.sh cargo test -p pg-engine --test golden
#
# Several builds running at once ran a machine out of RAM; these slots cap concurrent heavy
# jobs (HEAVY_SLOTS, default 1) however many agents are reading and editing.
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
LOCK=${HEAVY_LOCK:-/tmp/pg-engine-heavy.lock}
SLOTS=${HEAVY_SLOTS:-1}
# Slot 1 is $LOCK itself, so a single-slot run shares the lock with older copies of this script.
slot_path() { if [ "$1" -eq 1 ]; then echo "$LOCK"; else echo "$LOCK.$1"; fi; }
HELD=
while [ -z "$HELD" ]; do
  i=1
  while [ "$i" -le "$SLOTS" ]; do
    S=$(slot_path "$i")
    if mkdir "$S" 2>/dev/null; then HELD=$S; break; fi
    # Take over a slot whose holder died.
    if [ -f "$S/pid" ] && ! kill -0 "$(cat "$S/pid")" 2>/dev/null; then rm -rf "$S"; continue; fi
    i=$((i + 1))
  done
  [ -n "$HELD" ] || sleep 5
done
echo $$ > "$HELD/pid"
trap 'rm -rf "$HELD"' EXIT INT TERM
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
