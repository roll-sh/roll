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
# Full debuginfo makes every test binary ~400 MB and a worktree's target ~15 GB; line tables
# keep backtraces and gdb stacks readable at a fraction of that. Set it to override.
: "${CARGO_PROFILE_DEV_DEBUG:=line-tables-only}"
export CARGO_PROFILE_DEV_DEBUG
MIN_FREE_GB=${HEAVY_MIN_FREE_GB:-30}
MAX_INCREMENTAL_GB=${HEAVY_MAX_INCREMENTAL_GB:-3}
free_gb() { df -Pk "$ROOT" | awk 'NR==2 {print int($4/1048576)}'; }
# Don't start a build that could fill a shared disk; wait for space instead.
WAITED=0
while [ "$(free_gb)" -lt "$MIN_FREE_GB" ]; do
  [ $((WAITED % 300)) -eq 0 ] && echo "heavy.sh: $(free_gb)G free (< ${MIN_FREE_GB}G), waiting for disk space" >&2
  sleep 30; WAITED=$((WAITED + 30))
done
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
# Incremental caches keep every old session's data; drop them when they grow past
# HEAVY_MAX_INCREMENTAL_GB or the disk gets low.
INC="$ROOT/target/debug/incremental"
INC_GB=$(du -sk "$INC" 2>/dev/null | awk '{print int($1/1048576)}')
FREE_GB=$(free_gb)
if [ "${INC_GB:-0}" -ge "$MAX_INCREMENTAL_GB" ] || [ "${FREE_GB:-0}" -lt $((MIN_FREE_GB + 15)) ]; then
  rm -rf "$INC"
  echo "heavy.sh: incremental ${INC_GB:-0}G, ${FREE_GB}G free, cleared target/debug/incremental" >&2
fi
"$@"
