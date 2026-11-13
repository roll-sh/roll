#!/usr/bin/env bash
# What a roll apps cold start is made of (docs/apps/README.md, "Measured"): docker start → first
# HTTP byte of busybox httpd, seven alternating rounds of five containers that differ from roll's
# flags in one way each. Variant a is roll's (the limit, --init, the local log driver, a project
# network); change the others to test a flag. Everything it makes is named bench-* or
# roll-apps-bench-net and removed at the end. Run with Docker: scripts/bench-apps-coldstart.sh
set -u
IMG=busybox:1.37
NET=roll-apps-bench-net
docker network create $NET >/dev/null 2>&1 || true
mk() { # name port extra...
  local n=$1 p=$2; shift 2
  docker rm -f "$n" >/dev/null 2>&1
  docker create --name "$n" -p 127.0.0.1:$p:8080 "$@" $IMG sh -c 'mkdir -p /s && echo ok > /s/index.html && exec httpd -f -p 8080 -h /s' >/dev/null
}
base=(--memory 256m --memory-swap 256m --cpus 0.25 --pids-limit 4096 --security-opt no-new-privileges --cap-drop NET_RAW --cap-drop MKNOD --cap-drop AUDIT_WRITE --cap-drop SETFCAP)
nocpu=(--memory 256m --memory-swap 256m --pids-limit 4096 --security-opt no-new-privileges --cap-drop NET_RAW --cap-drop MKNOD --cap-drop AUDIT_WRITE --cap-drop SETFCAP)
nomem=(--cpus 0.25 --pids-limit 4096 --security-opt no-new-privileges --cap-drop NET_RAW --cap-drop MKNOD --cap-drop AUDIT_WRITE --cap-drop SETFCAP)
mk bench-a 18901 "${base[@]}" --init --log-driver local --network $NET
mk bench-b 18902 "${nocpu[@]}" --init --log-driver local --network $NET
mk bench-c 18903 "${nomem[@]}" --init --log-driver local --network $NET
mk bench-d 18904 "${base[@]}" --cpus 2 --init --log-driver local --network $NET
mk bench-e 18905 --init --log-driver local --network $NET
once() { # name port → ms
  local n=$1 p=$2
  docker stop -t 1 "$n" >/dev/null 2>&1
  local t0=$(date +%s%N)
  curl -s --unix-socket /var/run/docker.sock -X POST "http://d/containers/$n/start" >/dev/null
  until curl -s -m 0.2 -o /dev/null "http://127.0.0.1:$p/"; do :; done
  echo $(( ($(date +%s%N) - t0) / 1000000 ))
}
for round in $(seq 7); do
  line="$round"
  for v in a:18901 b:18902 c:18903 d:18904 e:18905; do line="$line ${v%%:*}=$(once bench-${v%%:*} ${v##*:})"; done
  echo "$line"
done
for v in a b c d e; do docker rm -f bench-$v >/dev/null; done
docker network rm $NET >/dev/null
