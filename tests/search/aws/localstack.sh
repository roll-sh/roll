#!/usr/bin/env bash
# The LocalStack part of scripts/search-aws-rehearsal.sh (run it through that script, which sets
# IMAGE, LOCALSTACK_PORT, SCRATCH and PYIMAGE).
#
# 1. LocalStack 4.9 (community) on a private network, its Lambda containers on the same network.
# 2. tests/search/aws/localstack.py deploys every resource of roll-search.yaml LocalStack
#    implements and checks them (the rest are listed: those only AWS can check).
# 3. Behind the function: Postgres, a node and a search-proxy from the image, configured from the
#    stack's own secrets the way the ECS task definitions are (the API token, the proxy TLS seed),
#    the proxy reachable under the name the function's upstream uses.
# 4. The function as Lambda runs it: LocalStack's community edition doesn't run image functions,
#    so the deployed function's image, EntryPoint, Command and environment (read back from
#    LocalStack) run under AWS's Lambda Runtime Interface Emulator (arm64, under qemu on x86),
#    behind a function URL front (tests/search/aws/function_url.py, payload format 2.0).
#    Through it: clusters created over the control API, the Elasticsearch requests of the image
#    smoke test, a wrong password refused; the function reaches the proxy over TLS pinned to the
#    certificate derived from the stack's seed secret.
set -euo pipefail
cd "$(dirname "$0")/../../.."
ROOT=$PWD
: "${IMAGE:?}" "${LOCALSTACK_PORT:?}" "${SCRATCH:?}" "${PYIMAGE:?}"

docker_() {
  if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "$(printf '%q ' docker "$@")"; fi
}

P=roll-rehearsal-$$
NET=$P
LS=$P-localstack
PROXY_HOST=$P-front
for arch in arm64 amd64; do
  docker_ image inspect "$IMAGE:$arch" >/dev/null 2>&1 || { echo "localstack: no image $IMAGE:$arch (scripts/search-image-smoke.sh --build)" >&2; exit 1; }
done
[[ -e /proc/sys/fs/binfmt_misc/qemu-aarch64 || $(uname -m) == aarch64 ]] || { echo "localstack: the arm64 function needs qemu binfmt (scripts/search-image-smoke.sh --install-binfmt arm64)" >&2; exit 1; }

cleanup() {
  # LocalStack's function containers carry its name.
  local c
  c=$(docker_ ps -aq --filter "name=$LS" 2>/dev/null || true)
  [[ -z $c ]] || docker_ rm -f $c >/dev/null 2>&1 || true
  docker_ rm -f "$P-node" "$PROXY_HOST" "$P-pg" "$P-fn" "$P-url" >/dev/null 2>&1 || true
  docker_ volume rm "$P-objects" "$P-data" >/dev/null 2>&1 || true
  docker_ network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker_ network create "$NET" >/dev/null
docker_ run -d --rm --name "$LS" --network "$NET" -m 3g -p "127.0.0.1:$LOCALSTACK_PORT:4566" localstack/localstack:4.9 >/dev/null
for _ in $(seq 1 120); do
  curl -fs "http://127.0.0.1:$LOCALSTACK_PORT/_localstack/health" 2>/dev/null | grep -q '"cloudformation": "available"' && break
  sleep 1
done
# The CloudFormation resource types this LocalStack has providers for.
docker_ exec "$LS" sh -c 'grep -ho "name = \"AWS::[A-Za-z0-9:]*\"" /opt/code/localstack/localstack-core/localstack/services/*/resource_providers/*_plugin.py' |
  sed -E 's/name = "(.*)"/\1/' | sort -u > "$SCRATCH/types"
[[ -s $SCRATCH/types ]] || { echo "localstack: no CloudFormation providers found" >&2; exit 1; }

docker_ run --rm --network host -v "$ROOT:/r:ro" -v "$SCRATCH:/s" -w /r "$PYIMAGE" python3 -I tests/search/aws/localstack.py \
  "http://127.0.0.1:$LOCALSTACK_PORT" deploy/search/aws/roll-search.yaml /s/types /s/localstack.json "$IMAGE:arm64" "$PROXY_HOST"
token=$(jq -r .token "$SCRATCH/localstack.json")
seed=$(jq -r .seed "$SCRATCH/localstack.json")
url=http://127.0.0.1:$((LOCALSTACK_PORT + 1))

# Behind the function: what the ECS services run, from the amd64 image (the arm64 one is the
# function's), with the stack's secrets.
meta="postgres://roll:roll@$P-pg:5432/search?sslmode=disable"
docker_ run -d --rm --name "$P-pg" --network "$NET" -m 512m -e POSTGRES_USER=roll -e POSTGRES_PASSWORD=roll -e POSTGRES_DB=search postgres:18-alpine >/dev/null
docker_ volume create "$P-objects" >/dev/null
docker_ run --rm -v "$P-objects:/objects" alpine:latest chown 10001:10001 /objects
for _ in $(seq 1 100); do
  docker_ exec "$P-pg" pg_isready -q -h 127.0.0.1 -U roll -d search 2>/dev/null && break
  sleep 0.3
done
docker_ run -d --name "$P-node" --network "$NET" -m 2g -v "$P-data:/data" -v "$P-objects:/objects" \
  -e ROLL_SEARCH_ONLY=1 -e ROLL_API=0.0.0.0:7070 -e ROLL_SEARCH_HTTP=0.0.0.0:9200 -e ROLL_SEARCH_META="$meta" \
  -e ROLL_SEARCH_STORAGE_DIR=/objects -e ROLL_SEARCH_SYNC_OBJECTS=1 -e ROLL_SEARCH_TLS_SELF_SIGNED=1 \
  -e ROLL_SEARCH_API_TOKEN="$token" -e ROLL_SEARCH_NODE_ID=n1 -e ROLL_SEARCH_PUBLIC_URL="$url" "$IMAGE:amd64" rolld >/dev/null
docker_ run -d --name "$PROXY_HOST" --network "$NET" -m 512m \
  -e ROLL_SEARCH_PROXY_LISTEN=0.0.0.0:9200 -e ROLL_SEARCH_PROXY_API_LISTEN=0.0.0.0:7070 -e ROLL_SEARCH_PROXY_HEALTH=0.0.0.0:8081 \
  -e ROLL_SEARCH_PROXY_PROTOCOL=false -e ROLL_SEARCH_META="$meta" -e ROLL_SEARCH_PROXY_TLS_SEED="$seed" "$IMAGE:amd64" search-proxy >/dev/null

# The function: the deployed configuration under the Runtime Interface Emulator.
rie=$SCRATCH/rie
mkdir -p "$rie"
curl -fsSL -o "$rie/aws-lambda-rie" https://github.com/aws/aws-lambda-runtime-interface-emulator/releases/latest/download/aws-lambda-rie-arm64
chmod +x "$rie/aws-lambda-rie"
fnenv=()
while IFS= read -r kv; do fnenv+=(-e "$kv"); done < <(jq -r '.function.environment | to_entries[] | "\(.key)=\(.value)"' "$SCRATCH/localstack.json")
mapfile -t fncmd < <(jq -r '.function.entrypoint[], .function.command[]' "$SCRATCH/localstack.json")
docker_ run -d --name "$P-fn" --network "$NET" --platform linux/arm64 -m "$(jq -r .function.memory "$SCRATCH/localstack.json")m" \
  -v "$rie:/aws-lambda:ro" --entrypoint /aws-lambda/aws-lambda-rie "${fnenv[@]}" \
  -e AWS_LAMBDA_FUNCTION_TIMEOUT="$(jq -r .function.timeout "$SCRATCH/localstack.json")" \
  "$(jq -r .function.image "$SCRATCH/localstack.json")" "${fncmd[@]}" >/dev/null
docker_ run -d --name "$P-url" --network "$NET" -m 128m -p "127.0.0.1:$((LOCALSTACK_PORT + 1)):8080" \
  -v "$ROOT/tests/search/aws:/app:ro" python:3.13-slim python3 -I /app/function_url.py 8080 \
  "http://$P-fn:8080/2015-03-31/functions/function/invocations" >/dev/null
for _ in $(seq 1 60); do
  curl -s -o /dev/null "$url/" 2>/dev/null && break
  sleep 1
done

# Through the function URL: the control API, then Elasticsearch.
if ! python3 -I tests/search/aws/smoke.py create "$url" "$token" ls-a ls-b > "$SCRATCH/clusters.json"; then
  docker_ logs --tail 20 "$PROXY_HOST" >&2
  docker_ logs --tail 30 "$P-fn" >&2
  exit 1
fi
python3 -I tests/search/aws/smoke.py es "$url" "$SCRATCH/clusters.json"
docker_ logs "$P-fn" 2>&1 | grep -q 'panicked' && { docker_ logs "$P-fn" >&2; echo "localstack: the function panicked" >&2; exit 1; }
echo "localstack: function URL -> ${fncmd[*]} (arm64 image, Runtime Interface Emulator) -> TLS pinned to the stack's seed -> proxy -> node: ok"
# A proxy serving another stack's certificate gets nothing from the function.
docker_ rm -f "$PROXY_HOST" >/dev/null
docker_ run -d --name "$PROXY_HOST" --network "$NET" -m 512m \
  -e ROLL_SEARCH_PROXY_LISTEN=0.0.0.0:9200 -e ROLL_SEARCH_PROXY_API_LISTEN=0.0.0.0:7070 -e ROLL_SEARCH_PROXY_PROTOCOL=false \
  -e ROLL_SEARCH_META="$meta" -e ROLL_SEARCH_PROXY_TLS_SEED=another-stacks-seed-0123456789 "$IMAGE:amd64" search-proxy >/dev/null
sleep 2
status=$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: Bearer $token" "$url/v1/search/clusters")
[[ $status != 200 ]] || { echo "localstack: the function talked to a proxy with another certificate" >&2; exit 1; }
echo "localstack: a proxy with another stack's certificate is refused (HTTP $status): ok"
echo "localstack: all green"
