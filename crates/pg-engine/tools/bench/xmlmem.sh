#!/bin/sh
# Helper for xml.sh, run by psql's \! inside the bench's psql container (--pid=host, root).
#
#   xmlmem.sh reset <backend pid>   reset the server process's peak RSS, remember RSS and CPU
#   xmlmem.sh read <backend pid>    print "<peak MB over the reset> <CPU ms since the reset>"
#
# The server process is $XB_ENGINE_PID (the engine: one process, every session a thread), or
# PostgreSQL's backend: the child of the reference's postmaster ($XB_PG_INIT, its host pid)
# whose pid in the container is <backend pid>.
set -eu
host_pid() {
  if [ -n "${XB_ENGINE_PID:-}" ]; then echo "$XB_ENGINE_PID"; return; fi
  for s in /proc/[0-9]*/status; do
    ppid=$(awk '/^PPid:/ {print $2}' "$s" 2>/dev/null) || continue
    [ "$ppid" = "$XB_PG_INIT" ] || continue
    ns=$(awk '/^NSpid:/ {print $NF}' "$s" 2>/dev/null)
    if [ "$ns" = "$1" ]; then p=${s#/proc/}; echo "${p%/status}"; return; fi
  done
  echo "no backend $1" >&2; exit 1
}
kb() { awk -v k="$2:" '$1 == k {print $2}' "/proc/$1/status"; }
ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
st=${XB_STATE:-/tmp/xmlmem}
p=$(host_pid "$2")
case $1 in
reset)
  echo 5 > "/proc/$p/clear_refs"
  echo "$(kb "$p" VmRSS) $(ticks "$p")" > "$st.$2"
  ;;
read)
  read -r rss t0 < "$st.$2"
  hwm=$(kb "$p" VmHWM)
  t1=$(ticks "$p")
  tck=$(getconf CLK_TCK)
  awk -v h="$hwm" -v r="$rss" -v a="$t0" -v b="$t1" -v c="$tck" \
    'BEGIN { printf "MEM %.1f %.0f\n", (h - r) / 1024, (b - a) * 1000 / c }'
  ;;
esac
