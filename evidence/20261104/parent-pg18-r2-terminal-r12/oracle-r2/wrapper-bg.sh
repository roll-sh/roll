#!/bin/sh
# Run a heavy job fully detached, so the time it waits for a heavy.sh slot can't hit a caller's
# timeout (a session's background commands stop at 2 hours, queue wait included). Output goes to
# <log>; when the job ends the last line is "HEAVY-EXIT <status>". Wait for that line instead of
# the process, e.g. `until grep -q '^HEAVY-EXIT' log; do sleep 60; done`, and restart the wait
# if it times out: the job keeps running.
#   crates/pg-engine/tools/heavy-bg.sh ~/.cache/a149/regress.log cargo test --release ...
set -eu
log=${1:?usage: heavy-bg.sh <log> <command...>}; shift
here=/home/meszmate/roll/.claude/worktrees/agent-a3512214927146502/crates/pg-engine/tools
: > "$log"
setsid nohup sh -c '"$0" "$@"; echo "HEAVY-EXIT $?"' "$here/heavy.sh" "$@" >> "$log" 2>&1 < /dev/null &
echo "started pid $! -> $log"
