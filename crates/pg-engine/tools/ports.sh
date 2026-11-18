#!/bin/sh
# Ports for the engines and references the harnesses start (quick-check.sh, verification-train.sh,
# batch-train.sh, ref-containers.sh), from below the kernel's ephemeral range.
#
#   tools/ports.sh take <owner pid> <n> [port|=port ...]   # n ports, comma-separated
#   tools/ports.sh free <port>                              # exit 0 when it can be bound now
#   tools/ports.sh release <port>...
#
# Every outgoing connection on this machine takes a local port from the ephemeral range
# (/proc/sys/net/ipv4/ip_local_port_range, 32768-60999 here). An engine told to listen on a port
# in that range fails with os error 98 when a client socket holds it at that moment, or when one
# takes it while the engine restarts. That is why the agents' blocks (55050-55069...) and the
# batch train's old defaults (54860-54863) failed now and then. A port below the range is never
# taken that way.
#
# take: each preferred port is kept when it is below the ephemeral range, nobody else holds it
# and 127.0.0.1:<port> can be bound now (with "=port": without the bind test, for a port a
# container of the caller's already listens on). The rest come from PORTS_RANGE (default
# 22000-32700, above every listener on this machine in 2026-11; the bind test skips ones in use).
# A port is held for the owner while that process lives: a directory in PORTS_DIR (default
# ~/.cache/pg-engine-ports) with the owner's pid and start time. A dead owner's ports are free
# again; release frees them earlier.
set -u
DIR=${PORTS_DIR:-$HOME/.cache/pg-engine-ports}
RANGE=${PORTS_RANGE:-22000-32700}
mkdir -p "$DIR"

# The bind test, with SO_REUSEADDR as the engine's listener (Rust's std, tokio) sets it.
free() {
  python3 -I -c '
import socket, sys
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
    s.listen()
except OSError:
    sys.exit(1)' "$1"
}
# (dash's read gets one byte of a /proc/sys file.)
EPH=$(cat /proc/sys/net/ipv4/ip_local_port_range)
ephemeral() { set -- "$1" $EPH; [ "$1" -ge "$2" ] && [ "$1" -le "$3" ]; }
# "pid start-time" of a live process (start time: /proc/<pid>/stat field 22, so a reused pid
# doesn't keep a dead owner's ports).
ident() { [ -r "/proc/$1/stat" ] && echo "$1 $(sed 's/.*) //' "/proc/$1/stat" | cut -d' ' -f20)"; }
alive() { o=$(cat "$1/owner" 2>/dev/null) || return 1; [ "$(ident "${o%% *}")" = "$o" ]; }
# Hold a port for OWNER: 0 when held.
hold() {
  d=$DIR/$1
  if ! mkdir "$d" 2>/dev/null; then
    alive "$d" && return 1
    # A dead owner's (or a half-made) reservation: take it over, once.
    rm -rf "$d"; mkdir "$d" 2>/dev/null || return 1
  fi
  echo "$OWNER" >"$d/owner"
}

cmd=${1:-}
case $cmd in
free) free "${2:?port}" ;;
release)
  shift
  for p in "$@"; do rm -rf "${DIR:?}/$p"; done ;;
take)
  pid=${2:?owner pid}; n=${3:?count}; shift 3
  OWNER=$(ident "$pid") || { echo "ports.sh: no process $pid" >&2; exit 2; }
  got=
  for p in "$@"; do
    [ "$(echo "$got" | wc -w)" -lt "$n" ] || break
    q=${p#=}
    case $q in ''|*[!0-9]*) continue ;; esac
    case " $got " in *" $q "*) continue ;; esac
    ephemeral "$q" && continue
    hold "$q" || continue
    if [ "$p" = "$q" ] && ! free "$q"; then rm -rf "${DIR:?}/$q"; continue; fi
    got="$got $q"
  done
  lo=${RANGE%-*}; hi=${RANGE#*-}; span=$((hi - lo + 1))
  # From a point that differs per owner, so agents taking ports at once rarely meet.
  i=$(( (pid * 7919 + $(date +%s)) % span )); tried=0
  while [ "$(echo "$got" | wc -w)" -lt "$n" ]; do
    [ $tried -lt $span ] || { echo "ports.sh: no free port in $RANGE" >&2; exit 1; }
    q=$((lo + i)); i=$(( (i + 1) % span )); tried=$((tried + 1))
    case " $got " in *" $q "*) continue ;; esac
    ephemeral "$q" && continue
    hold "$q" || continue
    free "$q" || { rm -rf "${DIR:?}/$q"; continue; }
    got="$got $q"
  done
  echo $got | tr ' ' , ;;
*) echo "usage: ports.sh take <owner pid> <n> [port|=port ...] | free <port> | release <port>..." >&2; exit 2 ;;
esac
