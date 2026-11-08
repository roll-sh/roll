#!/usr/bin/env bash
# Builds, ships and rolls back the roll cloud fleet (deploy/cloud/README.md). The deploy workflow
# (.github/workflows/deploy.yml) runs it; so can you, with AWS credentials for the stack.
#
#   deploy.sh push [tag]      build the image for linux/arm64 and push it to ECR (default tag:
#                             the commit; skipped if it's there already)
#   deploy.sh deploy [tag]    canary (one node of the new version) → bake → replace the fleet one
#                             node at a time → canary to 0. Any failure puts the previous
#                             revision back.
#   deploy.sh rollback [td]   put back the revision the last deploy replaced (or the task
#                             definition ARN given), canary to 0
#   deploy.sh current         the image tag the fleet runs (pass it as ImageTag to stack updates)
#   deploy.sh status          services, deployments and alarms
#
# Settings: ROLL_CLOUD_STACK (roll-cloud), AWS_REGION (us-east-1), BAKE_SECONDS (600),
# ROLL_DEPLOY_STATE (where the previous revision is kept for `rollback`), CARGO_BUILD_JOBS.
set -euo pipefail

STACK=${ROLL_CLOUD_STACK:-roll-cloud}
export AWS_REGION=${AWS_REGION:-us-east-1}
export AWS_PAGER=""
BAKE_SECONDS=${BAKE_SECONDS:-600}
STATE=${ROLL_DEPLOY_STATE:-${RUNNER_TEMP:-${XDG_STATE_HOME:-$HOME/.local/state}}/roll-cloud-deploy-$STACK}
# Polling: a step that doesn't finish in time fails the deploy (and rolls it back).
POLL=${POLL:-15}
CANARY_TIMEOUT=${CANARY_TIMEOUT:-1500}
NODE_TIMEOUT=${NODE_TIMEOUT:-900}

log() { printf '%s deploy: %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }
die() { log "error: $*"; exit 1; }

# ── the stack's outputs ──
outputs() {
  [ -n "${CLUSTER:-}" ] && return 0
  local json
  json=$(aws cloudformation describe-stacks --stack-name "$STACK" --query 'Stacks[0].Outputs' --output json)
  out() { jq -r --arg k "$1" '.[] | select(.OutputKey == $k) | .OutputValue' <<<"$json"; }
  CLUSTER=$(out ClusterName)
  SERVICE=$(out NodeServiceName)
  CANARY=$(out CanaryServiceName)
  REPO=$(out RepositoryUri)
  CACHE=$(out CacheRepositoryUri)
  ALARMS=$(out DeployAlarms)
  SUPPLIED=$(out SuppliedSecrets)
  [ -n "$CLUSTER" ] && [ -n "$REPO" ] || die "stack $STACK has no outputs (is it created?)"
}

service() { aws ecs describe-services --cluster "$CLUSTER" --services "$1" --query 'services[0]' --output json; }
task_def_of() { service "$1" | jq -r '.taskDefinition'; }
image_of() {
  aws ecs describe-task-definition --task-definition "$1" --output json |
    jq -r '.taskDefinition.containerDefinitions[] | select(.name == "rolld") | .image'
}
default_tag() { git rev-parse HEAD 2>/dev/null || die "no tag given and not in a git checkout"; }
image_exists() { aws ecr describe-images --repository-name "${REPO#*/}" --image-ids "imageTag=$1" >/dev/null 2>&1; }

# ── push ──
cmd_push() {
  outputs
  local tag=${1:-$(default_tag)} registry=${REPO%%/*}
  if image_exists "$tag"; then
    log "$REPO:$tag is already in ECR"
    return 0
  fi
  aws ecr get-login-password | docker login --username AWS --password-stdin "$registry" >/dev/null
  # Registry caches need BuildKit in a container (the workflow's setup-buildx-action makes one).
  local builder=()
  if [ "$(docker buildx inspect 2>/dev/null | awk '/^Driver:/ { print $2; exit }')" = docker ]; then
    docker buildx inspect roll-cloud >/dev/null 2>&1 || docker buildx create --name roll-cloud --driver docker-container >/dev/null
    builder=(--builder roll-cloud)
  fi
  local jobs=()
  [ -z "${CARGO_BUILD_JOBS:-}" ] || jobs=(--build-arg "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS")
  local root
  root=$(cd "$(dirname "$0")/../.." && pwd)
  log "building $REPO:$tag (linux/arm64)"
  docker buildx build "${builder[@]}" "${jobs[@]}" --platform linux/arm64 \
    -f "$root/deploy/cloud/Dockerfile" \
    --cache-from "type=registry,ref=$CACHE:main" \
    --cache-to "type=registry,ref=$CACHE:main,mode=max,image-manifest=true,oci-mediatypes=true" \
    --label "org.opencontainers.image.revision=$tag" \
    -t "$REPO:$tag" --push "$root"
  log "pushed $REPO:$tag"
}

# ── waiting ──

# Waits until service $1's deployment of task definition $2 has rolled out with every task
# running (ECS counts a task once its health checks pass). Fails if ECS gives up on it.
wait_rolled_out() {
  local name=$1 td=$2 timeout=$3 start=$SECONDS s state running desired
  while :; do
    s=$(service "$name")
    state=$(jq -r --arg td "$td" '[.deployments[] | select(.taskDefinition == $td)][0].rolloutState // "GONE"' <<<"$s")
    running=$(jq -r '.runningCount' <<<"$s")
    desired=$(jq -r '.desiredCount' <<<"$s")
    case "$state" in
      COMPLETED)
        if [ "$(jq '.deployments | length' <<<"$s")" = 1 ] && [ "$running" = "$desired" ]; then
          log "$name: $td rolled out ($running/$desired)"
          return 0
        fi
        ;;
      FAILED | GONE)
        log "$name: deployment of $td $state: $(jq -r '.deployments[0].rolloutStateReason // ""' <<<"$s")"
        return 1
        ;;
    esac
    if (( SECONDS - start > timeout )); then
      log "$name: $td still $state after ${timeout}s ($running/$desired running)"
      return 1
    fi
    log "$name: $state, $running/$desired running"
    sleep "$POLL"
  done
}

# Prints the deploy alarms that are in ALARM, if any.
alarming() {
  local names
  IFS=, read -r -a names <<<"$ALARMS"
  aws cloudwatch describe-alarms --alarm-names "${names[@]}" --state-value ALARM --query 'MetricAlarms[].AlarmName' --output text
}

# ── deploy ──

# Registers task definition $1 again with every container on image $2; prints the new ARN.
register_with_image() {
  aws ecs describe-task-definition --task-definition "$1" --include TAGS --output json |
    jq --arg image "$2" '
      (.tags // []) as $tags
      | .taskDefinition
      | .containerDefinitions |= map(.image = $image)
      | del(.taskDefinitionArn, .revision, .status, .requiresAttributes, .compatibilities,
            .registeredAt, .registeredBy, .deregisteredAt)
      | if ($tags | length) > 0 then .tags = $tags else . end
    ' >"$STATE.td.json"
  aws ecs register-task-definition --cli-input-json "file://$STATE.td.json" --query 'taskDefinition.taskDefinitionArn' --output text
}

# One extra task at a time, whatever the fleet's size: ceil(100 × (n + 1) / n), at most 200.
max_percent() {
  local n=$1 p
  (( n >= 1 )) || n=1
  p=$(( (100 * (n + 1) + n - 1) / n ))
  (( p <= 200 )) || p=200
  echo "$p"
}

on_deploy_exit() {
  local rc=$?
  trap - EXIT
  if [ "$rc" -ne 0 ]; then
    log "deploy failed; rolling back"
    cmd_rollback "$ROLLBACK_TO" || log "rollback failed: deploy.sh rollback $ROLLBACK_TO"
  fi
  exit "$rc"
}

cmd_deploy() {
  outputs
  local tag=${1:-$(default_tag)} image
  image="$REPO:$tag"

  # Preflight: nothing half-done, the image is there, the fleet is bootstrapped, secrets filled.
  image_exists "$tag" || die "$image isn't in ECR (deploy.sh push $tag)"
  local s prev
  s=$(service "$SERVICE")
  [ "$(jq -r '.desiredCount' <<<"$s")" -gt 0 ] || die "$SERVICE runs no nodes: bootstrap the stack first (deploy/cloud/README.md)"
  [ "$(jq '.deployments | length' <<<"$s")" = 1 ] || die "$SERVICE has a deployment in progress"
  [ "$(service "$CANARY" | jq -r '.desiredCount')" = 0 ] || die "$CANARY is running: another deploy, or one that didn't clean up (deploy.sh rollback)"
  local secret
  for secret in ${SUPPLIED//,/ }; do
    [ "$(aws secretsmanager list-secret-version-ids --secret-id "$secret" --query 'length(Versions)' --output text)" -gt 1 ] ||
      die "secret $secret still has its placeholder (deploy/cloud/README.md, \"Fill in the secrets\")"
  done
  prev=$(jq -r '.taskDefinition' <<<"$s")
  if [ "$(image_of "$prev")" = "$image" ]; then
    log "$SERVICE already runs $image"
    return 0
  fi
  local current
  current=$(alarming)
  [ -z "$current" ] || die "alarms already in ALARM, not deploying: $current"

  # From here on, a failure (or a cancelled job) puts the previous revision back.
  mkdir -p "$(dirname "$STATE")"
  printf '%s\n' "$prev" >"$STATE"
  ROLLBACK_TO=$prev
  trap on_deploy_exit EXIT
  trap 'exit 130' INT TERM

  local next
  next=$(register_with_image "$prev" "$image")
  log "registered $next ($image), replacing $prev"

  # 1. canary: one node of the new version joins the fleet and takes its share.
  log "canary: $CANARY → 1 × $next"
  aws ecs update-service --cluster "$CLUSTER" --service "$CANARY" --task-definition "$next" --desired-count 1 >/dev/null
  wait_rolled_out "$CANARY" "$next" "$CANARY_TIMEOUT"

  # 2. bake: the alarms stay quiet and the canary stays up.
  log "baking for ${BAKE_SECONDS}s, watching: $ALARMS"
  local until=$(( SECONDS + BAKE_SECONDS )) bad
  while (( SECONDS < until )); do
    bad=$(alarming)
    [ -z "$bad" ] || die "in ALARM during the bake: $bad"
    [ "$(service "$CANARY" | jq -r '.runningCount')" = 1 ] || die "the canary stopped during the bake"
    sleep "$(( POLL * 2 ))"
  done
  log "bake passed"

  # 3. roll: replace the fleet one node at a time. MinimumHealthyPercent 100 keeps every old node
  # until its replacement is healthy; MaximumPercent allows exactly one extra.
  local n config
  s=$(service "$SERVICE")
  n=$(jq -r '.desiredCount' <<<"$s")
  config=$(jq -c --argjson max "$(max_percent "$n")" '.deploymentConfiguration | .maximumPercent = $max | .minimumHealthyPercent = 100' <<<"$s")
  log "rolling $SERVICE ($n nodes) to $next"
  aws ecs update-service --cluster "$CLUSTER" --service "$SERVICE" --task-definition "$next" --deployment-configuration "$config" >/dev/null
  wait_rolled_out "$SERVICE" "$next" $(( NODE_TIMEOUT * (n + 1) ))

  # 4. the canary's node goes; the fleet runs the new version.
  aws ecs update-service --cluster "$CLUSTER" --service "$CANARY" --desired-count 0 >/dev/null
  trap - EXIT INT TERM
  log "deployed $image"
}

cmd_rollback() {
  outputs
  local prev=${1:-}
  if [ -z "$prev" ]; then
    [ -s "$STATE" ] || die "no previous revision recorded in $STATE: pass a task definition ARN"
    prev=$(cat "$STATE")
  fi
  log "rollback: $CANARY → 0"
  aws ecs update-service --cluster "$CLUSTER" --service "$CANARY" --desired-count 0 >/dev/null
  if [ "$(task_def_of "$SERVICE")" != "$prev" ]; then
    log "rollback: $SERVICE → $prev"
    aws ecs update-service --cluster "$CLUSTER" --service "$SERVICE" --task-definition "$prev" >/dev/null
  fi
  wait_rolled_out "$SERVICE" "$prev" $(( NODE_TIMEOUT * 4 ))
  log "rolled back to $prev"
}

cmd_current() {
  outputs
  local image
  image=$(image_of "$(task_def_of "$SERVICE")")
  echo "${image##*:}"
}

cmd_status() {
  outputs
  local name names
  for name in "$SERVICE" "$CANARY"; do
    service "$name" | jq -r '"\(.serviceName): \(.runningCount)/\(.desiredCount) running", (.deployments[] | "  \(.status) \(.rolloutState) \(.taskDefinition | split("/")[-1]) \(.runningCount)/\(.desiredCount)")'
  done
  IFS=, read -r -a names <<<"$ALARMS"
  aws cloudwatch describe-alarms --alarm-names "${names[@]}" --query 'MetricAlarms[].[AlarmName, StateValue]' --output text
}

case "${1:-}" in
  push | deploy | rollback | current | status)
    cmd=$1
    shift
    "cmd_$cmd" "$@"
    ;;
  *)
    sed -n '2,16p' "$0" >&2
    exit 2
    ;;
esac
