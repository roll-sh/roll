#!/bin/sh
# Hold a benchmark lane for one command, with a time limit (HANDOFF, machine rules: the lanes).
#
#   crates/pg-engine/tools/lane.sh <a|b|c> <max-seconds> <command...>
#   LANE_CONTAINERS="a252-pg" crates/pg-engine/tools/lane.sh b 900 ./pair.sh old new
#
# Lanes (servers / clients): a 6,7,24,25 / 8,9,26,27 (point reads, short transactions); b
# 10,11,28,29 / 12,13,30,31 (range reads); c 14,15,32,33 / 16,17,34,35 (TPC-H, bulk).
#
# Takes ~/.cache/lane-<x>.lock (the flock that `flock ~/.cache/lane-a.lock ...` takes too), and
# while it waits prints who holds it and who else waits (~/.cache/lane-<x>.holder, one waiting
# file per pid in ~/.cache/lane-<x>.queue/). Holding it, it writes the holder file (pid, since,
# who: LANE_WHO or the directory's name, limit, command), runs the command in a process group and
# systemd scope of its own, and when the command outlives <max-seconds> stops its process group
# by pid (SIGTERM, SIGKILL 10 s later; then the scope, for what left the group): exit 124.
# Otherwise it exits with the command's status. One pair (old and new) per hold: builds, setup
# and analysis go outside it, so nobody waits hours.
#
# After the command, a line (also appended to ~/.cache/lane-<x>.log):
#   lane <x> <who>: held <s>s of <max>s, exit <rc>; foreign load <avg> cores (peak <p> over 5 s,
#   before <b>), own <o> cores; <command>
# Foreign load is the busy time of the lane's 8 CPUs (/proc/stat) minus the command's own CPU
# time (its scope, plus the docker containers named in LANE_CONTAINERS), per second: other
# tenants' work on those CPUs. It holds when the command's work runs on the lane's CPUs (taskset,
# docker --cpuset-cpus); own work elsewhere makes it read low. "before" is the 2 s before the
# command. Gates leave out a pair with more than 1.0 foreign core (the brief's rule).
set -u
usage() { echo "usage: lane.sh <a|b|c> <max-seconds> <command...>" >&2; exit 2; }
[ $# -ge 3 ] || usage
x=$1 max=$2; shift 2
case $max in ''|*[!0-9]*) usage ;; esac
case $x in
  a) CPUS=6,7,24,25,8,9,26,27 ;;
  b) CPUS=10,11,28,29,12,13,30,31 ;;
  c) CPUS=14,15,32,33,16,17,34,35 ;;
  *) usage ;;
esac
CPUS=${LANE_CPUS:-$CPUS}   # (another CPU set: for testing this script off the lanes)
L=$HOME/.cache/lane-$x
WHO=${LANE_WHO:-$(basename "$PWD")}
mkdir -p "$L.queue"
now() { date +%s; }
ago() { echo "$(( $(now) - $1 ))s"; }
# Who holds the lock and who waits; drops queue entries of dead waiters.
show() {
  set -- $(cat "$L.holder" 2>/dev/null)
  if [ $# -ge 4 ] && [ -d "/proc/$1" ]; then
    pid=$1 since=$2 who=$3 lim=$4; shift 4
    echo "lane $x: held by $who (pid $pid) for $(ago "$since") of its ${lim}s: $*"
  else
    p=$(lslocks -n -o PID,PATH 2>/dev/null | awk -v f="$L.lock" '$2 == f {print $1; exit}')
    [ -n "$p" ] && echo "lane $x: held by pid $p, without lane.sh: $(tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | cut -c1-160)"
  fi
  for f in "$L.queue"/*; do
    [ -f "$f" ] || continue
    set -- $(cat "$f" 2>/dev/null)
    [ $# -ge 4 ] && [ -d "/proc/$1" ] || { rm -f "$f"; continue; }
    pid=$1 since=$2 who=$3 lim=$4; shift 4
    echo "lane $x: waiting $(ago "$since"): $who (pid $pid, ${lim}s): $*"
  done
}
# Busy jiffies of the lane's CPUs and the CPU microseconds of the given cgroups, sampled every
# 0.5 s until SIGTERM; then "<avg foreign> <peak foreign over 5 s> <avg own>" in cores.
SAMPLER='
import os, signal, subprocess, sys, time
cpus = {"cpu" + c for c in sys.argv[1].split(",")}
hz = os.sysconf("SC_CLK_TCK")
scope, containers = sys.argv[2], sys.argv[3].split()
seen, ids = {}, {}
def busy():
    t = 0
    for l in open("/proc/stat"):
        f = l.split()
        if f[0] in cpus:
            v = list(map(int, f[1:]))
            t += sum(v[:8]) - v[3] - v[4]   # all but idle and iowait
    return t / hz
def cg_usage(path):
    try:
        for l in open(f"/sys/fs/cgroup{path}/cpu.stat"):
            if l.startswith("usage_usec"):
                seen[path] = max(seen.get(path, 0), int(l.split()[1]) / 1e6)
    except OSError:
        pass   # gone: the last reading stands
    return seen.get(path, 0)
def own():
    t = cg_usage(scope) if scope else 0
    for c in containers:
        if c not in ids:
            for cmd in (["docker"], ["sg", "docker", "-c"]):
                a = cmd + (["inspect", "-f", "{{.Id}}", c] if len(cmd) == 1 else [f"docker inspect -f {{{{.Id}}}} {c}"])
                r = subprocess.run(a, capture_output=True, text=True)
                if r.returncode == 0 and r.stdout.strip():
                    ids[c] = "/system.slice/docker-" + r.stdout.strip() + ".scope"
                    break
        if c in ids:
            t += cg_usage(ids[c])
    return t
stop = []
signal.signal(signal.SIGTERM, lambda *a: stop.append(1))
base = (own(), )
samples = [(time.monotonic(), busy(), 0.0)]
if len(sys.argv) > 4:
    open(sys.argv[4], "w").close()   # ready: SIGTERM ends it from here on
while not stop:
    time.sleep(0.5)
    samples.append((time.monotonic(), busy(), own() - base[0]))
samples.append((time.monotonic(), busy(), own() - base[0]))
(t0, b0, o0), (t1, b1, o1) = samples[0], samples[-1]
wall = max(t1 - t0, 1e-3)
peak, j = 0.0, 0
for i in range(len(samples)):
    while samples[i][0] - samples[j][0] > 5:
        j += 1
    if samples[i][0] - samples[j][0] >= 4.5:
        (ta, ba, oa), (tb, bb, ob) = samples[j], samples[i]
        peak = max(peak, ((bb - ba) - (ob - oa)) / (tb - ta))
avg = max(0.0, ((b1 - b0) - (o1 - o0)) / wall)
print(f"{avg:.2f} {max(peak, avg):.2f} {(o1 - o0) / wall:.2f}")
'

me="$$ $(now) $WHO $max $*"
echo "$me" >"$L.queue/$$"
WD= CPID=
cleanup() {
  rm -f "$L.queue/$$"
  [ "$(cut -d' ' -f1 "$L.holder" 2>/dev/null)" = $$ ] && : >"$L.holder"
  [ -n "$WD" ] && kill -TERM "-$WD" 2>/dev/null
}
trap cleanup EXIT
trap '[ -n "$CPID" ] && kill -TERM "-$CPID" 2>/dev/null; exit 130' INT TERM
exec 8>>"$L.lock"
if ! flock -n 8; then
  show
  echo "lane $x: waiting for the lock"
  flock 8
fi
rm -f "$L.queue/$$"
START=$(now)
echo "$$ $START $WHO $max $*" >"$L.holder"
echo "lane $x: taken by $WHO for at most ${max}s"

# The 2 s before: other tenants' load alone.
BEFORE=$(sh -c 'python3 -I -c "$1" "$2" "" "" & s=$!; sleep 2; kill -TERM $s; wait $s' _ "$SAMPLER" "$CPUS" 8>&- | cut -d' ' -f1)

UNIT=lane-$x-$$
SCOPE=
if systemd-run --user --scope --quiet --unit="$UNIT-probe" true 2>/dev/null; then
  setsid systemd-run --user --scope --quiet --collect --unit="$UNIT" -- "$@" 8>&- &
  CPID=$!
  # The scope's cgroup, once systemd-run has moved itself into it.
  for _ in $(seq 40); do
    cg=$(sed -n 's/^0:://p' "/proc/$CPID/cgroup" 2>/dev/null)
    case $cg in */"$UNIT".scope) SCOPE=$cg; break ;; esac
    [ -d "/proc/$CPID" ] || break
    sleep 0.05
  done
else
  echo "lane $x: no systemd user scope: the command's own CPU is not measured (foreign load reads high)" >&2
  setsid "$@" 8>&- &
  CPID=$!
fi
python3 -I -c "$SAMPLER" "$CPUS" "$SCOPE" "${LANE_CONTAINERS:-}" "$L.ready.$$" >"$L.sample.$$" 8>&- &
SPID=$!
# The time limit: the command's process group, by pid.
rm -f "$L.timeout.$$"
setsid sh -c 'sleep "$1"; : >"$3"; kill -TERM "-$2"; sleep 10; kill -KILL "-$2"' _ "$max" "$CPID" "$L.timeout.$$" 8>&- &
WD=$!
wait "$CPID"; rc=$?
kill -TERM "-$WD" 2>/dev/null; WD=
# (A command that ends at once: the sampler first has to be able to take SIGTERM.)
for _ in $(seq 100); do [ -e "$L.ready.$$" ] && break; sleep 0.05; done
kill -TERM "$SPID"; wait "$SPID"
rm -f "$L.ready.$$"
if [ -f "$L.timeout.$$" ]; then
  rc=124
  systemctl --user stop "$UNIT.scope" 2>/dev/null
  echo "lane $x: over the ${max}s limit: stopped the command's process group ($CPID)" >&2
fi
rm -f "$L.timeout.$$"
left=$([ -n "$SCOPE" ] && cat "/sys/fs/cgroup$SCOPE/cgroup.procs" 2>/dev/null | paste -sd' ' -)
[ -z "$left" ] || echo "lane $x: the command left processes running: $left (stop them before the next pair)" >&2
read -r fa fp own <"$L.sample.$$"
rm -f "$L.sample.$$"
msg="lane $x $WHO: held $(( $(now) - START ))s of ${max}s, exit $rc; foreign load ${fa:-?} cores (peak ${fp:-?} over 5 s, before ${BEFORE:-?}), own ${own:-?} cores; $*"
echo "$msg"
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $msg" >>"$L.log"
exit "$rc"
