#!/usr/bin/env bash
# Smoke test of the roll search images (deploy/search/Dockerfile) as AWS runs them: a node fleet
# and a search-proxy from the image, the node started with the CloudFormation template's own
# command line, metadata in Postgres, the proxy waking the fleet through a fake ECS endpoint.
#
#   scripts/search-image-smoke.sh [--build] [--install-binfmt] [--s3] [amd64] [arm64]   (default: both)
#
# Per architecture (arm64 runs under qemu-user through binfmt_misc on an x86 host):
#   1. two nodes (`sh -c <NodeTask Command from roll-search.yaml>`, TLS with boot-time
#      certificates pinned by the proxy) and one proxy (`search-proxy`, the ProxyTask command);
#   2. clusters created through the proxy's API port, Elasticsearch requests through its REST
#      port (index, bulk, get, search with aggregations, count, errors, _cat), checked;
#   3. clusters going to sleep after ROLL_SEARCH_IDLE_UNLOAD and waking on the next request;
#   4. a Spot-style stop (`docker stop -t 120`: SIGTERM, then SIGKILL after the task's
#      StopTimeout) of one node under a steady write load: it must exit 0 within the node's
#      100 s budget, and every acknowledged write must be readable from the other node;
#   5. scale to zero (desired count 0, the last node stopped) and wake: a request held by the
#      proxy, which reads the fake ECS service (task-role credentials, SigV4 checked) and raises
#      its desired count to 1; the test then starts a fresh node (empty disk) and the held
#      request answers, with every acknowledged write still there;
#   6. the proxy's own SIGTERM exits 0.
#
# Object storage is a shared Docker volume, or with --s3 an S3 bucket on LocalStack 4.9 (the
# nodes' ROLL_S3_BUCKET as in the template, plus ROLL_S3_ENDPOINT), reached with task-role
# credentials from the ECS agent's endpoint (169.254.170.2, a relative URI) as on ECS, node b
# from a full URI with an authorization token as other container runtimes set it: that covers the
# image's own S3 client (object_store, pg-storage's credential providers, endpoint handling) end to end.
#
# Images: roll-search:<arch> (IMAGE=…), built first with --build (buildx, --provenance=false, as
# for Lambda). Env: PORT (base of three loopback ports, default 19280), SMOKE_DIR (scratch,
# default ~/.cache/roll-search/image-smoke), KEEP=1 leaves the containers for inspection.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
IMAGE=${IMAGE:-roll-search}
PORT=${PORT:-19280}
TEMPLATE=deploy/search/aws/roll-search.yaml
TOKEN=smoke-api-token

docker_() {
  if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "$(printf '%q ' docker "$@")"; fi
}

build=0 binfmt=0 s3=0 archs=()
for a in "$@"; do
  case "$a" in
    --build) build=1 ;;
    --install-binfmt) binfmt=1 ;;
    --s3) s3=1 ;;
    amd64 | arm64) archs+=("$a") ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done
[[ ${#archs[@]} -gt 0 ]] || archs=(amd64 arm64)

host_arch=$(uname -m)
needs_qemu() { [[ $1 == arm64 && $host_arch != aarch64 ]] || [[ $1 == amd64 && $host_arch != x86_64 ]]; }
for arch in "${archs[@]}"; do
  needs_qemu "$arch" || continue
  q=$([[ $arch == arm64 ]] && echo aarch64 || echo x86_64)
  if [[ ! -e /proc/sys/fs/binfmt_misc/qemu-$q ]]; then
    if [[ $binfmt == 1 ]]; then
      docker_ run --privileged --rm tonistiigi/binfmt --install "$arch" >/dev/null
    else
      echo "search-image-smoke: no qemu-$q binfmt handler; pass --install-binfmt (registers qemu-user with the kernel)" >&2
      exit 1
    fi
  fi
done

if [[ $build == 1 ]]; then
  for arch in "${archs[@]}"; do
    docker_ buildx build --platform "linux/$arch" --provenance=false -f deploy/search/Dockerfile -t "$IMAGE:$arch" --load .
  done
fi

# The node task's command exactly as the template has it (a folded `>-` scalar under `sh -c`).
NODE_CMD=$(python3 - "$TEMPLATE" <<'EOF'
import sys
lines = open(sys.argv[1]).read().splitlines()
i = next(n for n, l in enumerate(lines) if l.strip() == "- Name: node")
i = next(n for n in range(i, len(lines)) if lines[n].strip() == "Command:")
assert [l.strip() for l in lines[i + 1:i + 4]] == ["- sh", "- -c", "- >-"], lines[i + 1:i + 4]
indent = len(lines[i + 4]) - len(lines[i + 4].lstrip())
out = []
for l in lines[i + 4:]:
    if len(l) - len(l.lstrip()) < indent:
        break
    out.append(l.strip())
print(" ".join(out))
EOF
)
grep -q 'exec rolld' <<<"$NODE_CMD" || { echo "search-image-smoke: can't read the node command from $TEMPLATE" >&2; exit 1; }
grep -q 'Command: \[search-proxy\]' "$TEMPLATE" || { echo "search-image-smoke: the proxy task's command changed; update this test" >&2; exit 1; }

now() { date +%s.%N; }
secs() { python3 -c "import sys; print(f'{float(sys.argv[2]) - float(sys.argv[1]):.1f}')" "$1" "$2"; }
fail() { echo "search-image-smoke: FAILED ($arch): $*" >&2; exit 1; }

run_arch() {
  arch=$1
  # Global, not local: the EXIT trap's cleanup may run after this function has unwound.
  P=roll-smoke-$arch-$$ scratch=
  local img=$IMAGE:$arch
  scratch=${SMOKE_DIR:-$HOME/.cache/roll-search/image-smoke}/$arch-$$
  local rest=http://127.0.0.1:$PORT api=http://127.0.0.1:$((PORT + 1))
  local meta="postgres://roll:roll@$P-pg:5432/search?sslmode=disable"
  local platform=(--platform "linux/$arch")
  docker_ image inspect "$img" >/dev/null 2>&1 || fail "no image $img (pass --build)"
  [[ $(docker_ image inspect "$img" --format '{{.Architecture}}') == "$arch" ]] || fail "$img is not an $arch image"
  mkdir -p "$scratch/ecs"
  echo 2 > "$scratch/ecs/desired"

  cleanup() {
    if [[ ${KEEP:-0} == 1 ]]; then echo "search-image-smoke: kept containers $P-*"; return; fi
    docker_ rm -f "$P-node-a" "$P-node-b" "$P-node-c" "$P-proxy" "$P-ecs" "$P-pg" "$P-s3" >/dev/null 2>&1 || true
    docker_ volume rm "$P-objects" "$P-data-a" "$P-data-b" "$P-data-c" >/dev/null 2>&1 || true
    docker_ network rm "$P" >/dev/null 2>&1 || true
    rm -rf "$scratch"
  }
  trap cleanup EXIT

  # With --s3 the fake ECS endpoint also plays the ECS agent's task credentials endpoint, which
  # SDKs reach at the fixed link-local address 169.254.170.2 (AWS_CONTAINER_CREDENTIALS_RELATIVE_URI).
  local ecs_ip=()
  if [[ $s3 == 1 ]]; then
    # Containers get addresses from the upper half, so the agent's address stays free.
    docker_ network create --subnet 169.254.170.0/24 --ip-range 169.254.170.128/25 "$P" >/dev/null
    ecs_ip=(--ip 169.254.170.2)
  else
    docker_ network create "$P" >/dev/null
  fi
  docker_ run -d --rm --name "$P-pg" --network "$P" -m 512m -e POSTGRES_USER=roll -e POSTGRES_PASSWORD=roll \
    -e POSTGRES_DB=search postgres:18-alpine >/dev/null
  docker_ run -d --rm --name "$P-ecs" --network "$P" "${ecs_ip[@]}" -m 128m --user "$(id -u):$(id -g)" \
    -v "$ROOT/tests/search/aws:/app:ro" -v "$scratch/ecs:/state" python:3.13-slim python3 -I /app/fake_ecs.py 8080 /state 80 >/dev/null
  # Shared object storage: S3 (LocalStack) with --s3, else a volume; the nodes run as uid 10001.
  local storage=(-v "$P-objects:/objects" -e ROLL_SEARCH_STORAGE_DIR=/objects)
  if [[ $s3 == 1 ]]; then
    docker_ run -d --rm --name "$P-s3" --network "$P" -m 1g -e SERVICES=s3 localstack/localstack:4.9 >/dev/null
    storage=(-e ROLL_S3_BUCKET=roll-smoke -e "ROLL_S3_ENDPOINT=http://$P-s3:4566" -e AWS_REGION=us-east-1)
    local ready=0
    for _ in $(seq 1 120); do
      if docker_ exec "$P-s3" awslocal s3 mb s3://roll-smoke >/dev/null 2>&1; then ready=1; break; fi
      sleep 0.5
    done
    [[ $ready == 1 ]] || fail "LocalStack S3 never came up"
  else
    docker_ volume create "$P-objects" >/dev/null
    docker_ run --rm -v "$P-objects:/objects" alpine:latest chown 10001:10001 /objects
  fi
  for _ in $(seq 1 100); do
    docker_ exec "$P-pg" pg_isready -q -h 127.0.0.1 -U roll -d search 2>/dev/null && break
    sleep 0.3
  done

  start_node() {
    # The node task as the template defines it (env, stop timeout, ulimits), on a disk of its own.
    # With --s3, the task role's credentials as ECS gives every task (the agent's endpoint at a
    # relative URI); node b instead gets a full URI plus an authorization token, as other container
    # runtimes set it (pg-storage's own provider; object_store alone would fall back to IMDS).
    local creds=()
    if [[ $s3 == 1 && $1 == b ]]; then
      creds=(-e "AWS_CONTAINER_CREDENTIALS_FULL_URI=http://$P-ecs:8080/creds/node-b" -e AWS_CONTAINER_AUTHORIZATION_TOKEN=smoke-container-auth)
    elif [[ $s3 == 1 ]]; then
      creds=(-e AWS_CONTAINER_CREDENTIALS_RELATIVE_URI=/v2/credentials/smoke-task)
    fi
    docker_ run -d --name "$P-node-$1" --network "$P" "${platform[@]}" -m 2g --stop-timeout 120 \
      --ulimit nofile=65536:65536 -v "$P-data-$1:/data" "${storage[@]}" "${creds[@]}" \
      -e ROLL_SEARCH_ONLY=1 -e ROLL_API=0.0.0.0:7070 -e ROLL_SEARCH_HTTP=0.0.0.0:9200 \
      -e ROLL_SEARCH_META="$meta" -e ROLL_SEARCH_SYNC_OBJECTS=1 \
      -e ROLL_SEARCH_IDLE_UNLOAD=3 -e ROLL_SEARCH_SHUTDOWN_TIMEOUT=100 -e ROLL_SEARCH_CACHE_GIB=21 \
      -e ROLL_SEARCH_TASK_MEMORY_MB=2048 -e ROLL_SEARCH_TLS_SELF_SIGNED=1 -e ROLL_SEARCH_API_TOKEN=$TOKEN \
      -e ROLL_SEARCH_NODE_ID="$1" -e ROLL_SEARCH_AZ="az-$1" -e ROLL_SEARCH_PUBLIC_URL="$rest" \
      "$img" sh -c "$NODE_CMD" >/dev/null
  }
  start_node a
  start_node b
  docker_ run -d --name "$P-proxy" --network "$P" "${platform[@]}" -m 512m --stop-timeout 60 \
    -p "127.0.0.1:$PORT:9200" -p "127.0.0.1:$((PORT + 1)):7070" -p "127.0.0.1:$((PORT + 2)):8081" \
    -e ROLL_SEARCH_PROXY_LISTEN=0.0.0.0:9200 -e ROLL_SEARCH_PROXY_API_LISTEN=0.0.0.0:7070 \
    -e ROLL_SEARCH_PROXY_HEALTH=0.0.0.0:8081 -e ROLL_SEARCH_PROXY_DRAIN=45 -e ROLL_SEARCH_PROXY_HOLD_TIMEOUT=120 \
    -e ROLL_SEARCH_PROXY_PROTOCOL=false -e ROLL_SEARCH_META="$meta" -e ROLL_SEARCH_AZ=az-a \
    -e ROLL_SEARCH_PROXY_WAKE_ECS_SERVICE=smoke/nodes -e ROLL_SEARCH_PROXY_WAKE_COUNT=1 -e AWS_REGION=us-east-1 \
    -e AWS_ENDPOINT_URL_ECS="http://$P-ecs:8080" -e AWS_CONTAINER_CREDENTIALS_FULL_URI="http://$P-ecs:8080/creds" \
    -e AWS_CONTAINER_AUTHORIZATION_TOKEN=smoke-container-auth \
    "$img" search-proxy >/dev/null

  for _ in $(seq 1 120); do
    curl -fs "http://127.0.0.1:$((PORT + 2))/" >/dev/null 2>&1 && break
    sleep 0.5
  done
  curl -fs "http://127.0.0.1:$((PORT + 2))/" >/dev/null || fail "the proxy's health port never answered"

  local names=(smoke-a smoke-b smoke-c smoke-d smoke-e smoke-f)
  python3 -I tests/search/aws/smoke.py create "$api" "$TOKEN" "${names[@]}" > "$scratch/clusters.json" || fail "creating clusters"
  python3 -I tests/search/aws/smoke.py es "$rest" "$scratch/clusters.json" || fail "Elasticsearch requests"

  # Clusters sleep after ROLL_SEARCH_IDLE_UNLOAD seconds and wake on their next request.
  local state=""
  for _ in $(seq 1 60); do
    state=$(python3 -I tests/search/aws/smoke.py state "$api" "$TOKEN" smoke-a)
    [[ $state == asleep ]] && break
    sleep 1
  done
  [[ $state == asleep ]] || fail "cluster smoke-a never went to sleep (state $state)"
  local woke
  woke=$(python3 -I tests/search/aws/smoke.py count "$rest" "$scratch/clusters.json" smoke-a 60) || fail "waking smoke-a"
  [[ $(jq -r .count <<<"$woke") == 200 ]] || fail "smoke-a after its wake: $woke"
  echo "search-image-smoke: $arch: cluster slept and woke: $woke"

  # A Spot-style stop of node a under a steady write load.
  rm -f "$scratch/stop"
  python3 -I tests/search/aws/smoke.py writer "$rest" "$scratch/clusters.json" "$scratch/stop" "$scratch/acked-1.json" &
  local writer=$!
  sleep 4
  local t0 t1 drain
  t0=$(now)
  docker_ stop -t 120 "$P-node-a" >/dev/null
  t1=$(now)
  drain=$(secs "$t0" "$t1")
  sleep 4
  touch "$scratch/stop"
  wait "$writer" || fail "the writer"
  [[ $(docker_ inspect "$P-node-a" --format '{{.State.ExitCode}}') == 0 ]] || { docker_ logs --tail 30 "$P-node-a" >&2; fail "node a exited non-zero after SIGTERM"; }
  python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) < 100 else 1)" "$drain" || fail "node a took ${drain}s to stop (budget 100 s)"
  echo "search-image-smoke: $arch: node a drained in ${drain}s under writes"
  python3 -I tests/search/aws/smoke.py verify "$rest" "$scratch/clusters.json" "$scratch/acked-1.json" 200 || fail "writes after node a stopped"

  # Scale to zero: ToZeroPolicy sets the desired count to 0 and ECS stops the last node.
  echo 0 > "$scratch/ecs/desired"
  t0=$(now)
  docker_ stop -t 120 "$P-node-b" >/dev/null
  drain=$(secs "$t0" "$(now)")
  [[ $(docker_ inspect "$P-node-b" --format '{{.State.ExitCode}}') == 0 ]] || fail "node b exited non-zero after SIGTERM"
  echo "search-image-smoke: $arch: fleet at zero (node b drained in ${drain}s)"
  t0=$(now)
  python3 -I tests/search/aws/smoke.py count "$rest" "$scratch/clusters.json" smoke-c 150 > "$scratch/held.json" &
  local held=$!
  # Play the ECS scheduler: start a node once the proxy has raised the desired count.
  for _ in $(seq 1 120); do
    [[ $(cat "$scratch/ecs/desired") == 1 ]] && break
    sleep 0.5
  done
  [[ $(cat "$scratch/ecs/desired") == 1 ]] || { docker_ logs --tail 30 "$P-proxy" >&2; fail "the proxy never woke the fleet (desired $(cat "$scratch/ecs/desired"))"; }
  local asked
  asked=$(secs "$t0" "$(now)")
  start_node c
  wait "$held" || fail "the request held through the wake"
  echo "search-image-smoke: $arch: woke from zero: UpdateService after ${asked}s, held request answered $(cat "$scratch/held.json")"
  [[ $(jq -r .count "$scratch/held.json") -ge 200 ]] || fail "the held request: $(cat "$scratch/held.json")"
  python3 -I tests/search/aws/smoke.py verify "$rest" "$scratch/clusters.json" "$scratch/acked-1.json" 200 || fail "writes after the wake"
  python3 - "$scratch/ecs/calls" "$s3" <<'EOF' || fail "fake ECS calls"
import json, sys
calls = [json.loads(l) for l in open(sys.argv[1])]
if sys.argv[2] == "1":
    assert any(c["action"] == "task-creds" and c["ok"] for c in calls), "the nodes never fetched task-role credentials for S3"
    assert any(c["action"] == "creds" and c.get("path") == "/creds/node-b" and c["ok"] for c in calls), "node b never fetched credentials from its full URI"
bad = [c for c in calls if not c["ok"]]
assert not bad, f"rejected ECS calls: {bad}"
assert any(c["action"] == "creds" for c in calls), "the proxy never fetched task-role credentials"
updates = [c for c in calls if c["action"] == "UpdateService"]
assert updates and all(u["desiredCount"] == 1 and u["cluster"] == "smoke" and u["service"] == "nodes" for u in updates), updates
first = next(i for i, c in enumerate(calls) if c["action"] == "UpdateService")
assert any(c["action"] == "DescribeServices" for c in calls[:first]), "UpdateService without DescribeServices first"
print(f"search-image-smoke: ECS calls: {len(calls)}, all signed correctly; UpdateService desiredCount 1")
EOF

  if [[ $s3 == 1 ]]; then
    local objects
    objects=$(docker_ exec "$P-s3" awslocal s3 ls s3://roll-smoke --recursive | wc -l)
    [[ $objects -gt 0 ]] || fail "nothing in the S3 bucket"
    echo "search-image-smoke: $arch: S3 bucket holds $objects objects (segments, WAL, manifests)"
  fi
  docker_ stop -t 60 "$P-proxy" >/dev/null
  [[ $(docker_ inspect "$P-proxy" --format '{{.State.ExitCode}}') == 0 ]] || fail "the proxy exited non-zero after SIGTERM"
  for c in node-a node-b node-c proxy; do
    if docker_ logs "$P-$c" 2>&1 | grep -q 'panicked'; then docker_ logs "$P-$c" 2>&1 | grep -A5 panicked >&2; fail "$c panicked"; fi
  done
  docker_ stop -t 120 "$P-node-c" >/dev/null
  echo "search-image-smoke: $arch: ok"
  cleanup
  trap - EXIT
}

for arch in "${archs[@]}"; do
  run_arch "$arch"
done
echo "search-image-smoke: all green (${archs[*]})"
