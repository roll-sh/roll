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
#   deploy.sh status          what's live (commit, since when, what the API says), services,
#                             deployments, alarms and an open deploy-failure issue
#   deploy.sh diagnose [hours]  read only: each deploy alarm's state and reason, the minutes its
#                             metric breached in the last hours (default 24, at most 360), the
#                             services' recent events and the stopped nodes' reasons
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
  # What's deployed, for GET /v1/info (and the footer): the commit and when it was made.
  local made
  made=$(git -C "$root" show -s --format=%cI "$tag" 2>/dev/null || true)
  log "building $REPO:$tag (linux/arm64)"
  docker buildx build "${builder[@]}" "${jobs[@]}" --platform linux/arm64 \
    -f "$root/deploy/cloud/Dockerfile" \
    --build-arg "ROLL_COMMIT=$tag" --build-arg "ROLL_COMMITTED_AT=$made" \
    --cache-from "type=registry,ref=$CACHE:main" \
    --cache-to "type=registry,ref=$CACHE:main,mode=max,image-manifest=true,oci-mediatypes=true" \
    --label "org.opencontainers.image.revision=$tag" \
    -t "$REPO:$tag" --push "$root"
  log "pushed $REPO:$tag"
  # roll-fetch, beside customer code in isolated tasks (the runner templates use fetch-<tag>);
  # its layers come from the build above.
  docker buildx build "${builder[@]}" "${jobs[@]}" --platform linux/arm64 \
    -f "$root/deploy/cloud/Dockerfile" --target fetch \
    --cache-from "type=registry,ref=$CACHE:main" \
    --label "org.opencontainers.image.revision=$tag" \
    -t "$REPO:fetch-$tag" --push "$root"
  log "pushed $REPO:fetch-$tag"
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
    log "deploy failed; what happened:"
    explain "$DEPLOY_STARTED" || log "couldn't read why"
    log "rolling back"
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
  DEPLOY_STARTED=$(date -u +%s)
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
  local name names td tag
  td=$(task_def_of "$SERVICE")
  tag=$(image_of "$td")
  tag=${tag##*:}
  log "live: $tag ($(git log -1 --format='%s, %cI' "$tag" 2>/dev/null || echo "not in this checkout"))"
  log "since: $(aws ecs describe-task-definition --task-definition "$td" --query 'taskDefinition.registeredAt' --output text) (${td##*/} registered)"
  log "api: $(curl -fsS --max-time 10 "https://api.${ROLL_DOMAIN:-roll.sh}/v1/info" 2>/dev/null || echo unreachable)"
  if command -v gh >/dev/null; then
    log "deploy-failure issue: $(gh issue list --label deploy-failure --state open --json number,url --jq '.[0].url // "none open"' 2>/dev/null || echo "gh can't say")"
  fi
  for name in "$SERVICE" "$CANARY"; do
    service "$name" | jq -r '"\(.serviceName): \(.runningCount)/\(.desiredCount) running", (.deployments[] | "  \(.status) \(.rolloutState) \(.taskDefinition | split("/")[-1]) \(.runningCount)/\(.desiredCount)")'
  done
  IFS=, read -r -a names <<<"$ALARMS"
  aws cloudwatch describe-alarms --alarm-names "${names[@]}" --query 'MetricAlarms[].[AlarmName, StateValue]' --output text
}

# ── why: alarms, metrics, events and stopped tasks (read only; the deploy role's own rights) ──

iso() { date -u -d "@$1" +%Y-%m-%dT%H:%M:%SZ; }

# Every deploy alarm: its state, when it changed and why.
alarm_reasons() {
  local names
  IFS=, read -r -a names <<<"$ALARMS"
  aws cloudwatch describe-alarms --alarm-names "${names[@]}" --output json |
    jq -r '.MetricAlarms[] | "  \(.AlarmName): \(.StateValue) since \(.StateUpdatedTimestamp)\n    \(.StateReason)"'
}

# For each deploy alarm, the minutes since epoch $1 its own metric (or metric math) breached its
# threshold, from the alarm's definition: what tripped it, and when.
breaches() {
  local since=$1 names json
  IFS=, read -r -a names <<<"$ALARMS"
  json=$(aws cloudwatch describe-alarms --alarm-names "${names[@]}" --output json)
  local i n
  n=$(jq '.MetricAlarms | length' <<<"$json")
  for (( i = 0; i < n; i++ )); do
    local alarm queries name
    alarm=$(jq -c ".MetricAlarms[$i]" <<<"$json")
    name=$(jq -r '.AlarmName' <<<"$alarm")
    queries=$(jq -c '
      if (.Metrics // []) | length > 0 then
        [.Metrics[] | del(.AccountId, .Period) | if .MetricStat then .MetricStat.Period = 60 else . end]
      else
        [{Id: "m", ReturnData: true, MetricStat: {Metric: {Namespace, MetricName, Dimensions: (.Dimensions // [])}, Period: 60, Stat: (.Statistic // .ExtendedStatistic)}}]
      end' <<<"$alarm")
    aws cloudwatch get-metric-data --metric-data-queries "$queries" \
      --start-time "$(iso "$since")" --end-time "$(iso "$(date -u +%s)")" --scan-by TimestampAscending --output json |
      jq -r --argjson a "$alarm" --arg name "$name" '
        def breach(v): ($a.Threshold) as $t | $a.ComparisonOperator as $op
          | if $op == "GreaterThanThreshold" then v > $t
            elif $op == "GreaterThanOrEqualToThreshold" then v >= $t
            elif $op == "LessThanThreshold" then v < $t
            else v <= $t end;
        [.MetricDataResults[] | . as $r | range(0; .Values | length) as $j
          | select(breach($r.Values[$j])) | "\($r.Timestamps[$j][0:16]) \($r.Values[$j])"] as $hits
        | "  \($name) (\($a.ComparisonOperator) \($a.Threshold)): \($hits | length) breaching minute(s)",
          ($hits | if length > 60 then .[-60:] else . end | .[] | "    \(.)")'
  done
}

# The fleet's 5xx answers by product, per minute, since epoch $1: which product the errors are.
errors_by_product() {
  local queries
  queries='[{"Id":"e","Expression":"SEARCH('"'"'{RollCloud,product} MetricName=\"Http5xx\"'"'"', '"'"'Sum'"'"', 60)","Period":60,"ReturnData":true}]'
  aws cloudwatch get-metric-data --metric-data-queries "$queries" \
    --start-time "$(iso "$1")" --end-time "$(iso "$(date -u +%s)")" --scan-by TimestampAscending --output json |
    jq -r '.MetricDataResults[] | select(any(.Values[]; . > 0)) | "  \(.Label): \([.Values[]] | add) in \([.Values[] | select(. > 0)] | length) minute(s), last at \(.Timestamps[[.Values[]] | to_entries | map(select(.value > 0)) | last.key][0:16])"'
}

# Service events since epoch $1 (ECS keeps the last 100).
service_events() {
  local name
  for name in "$SERVICE" "$CANARY"; do
    service "$name" | jq -r --argjson since "$1" '.serviceName as $s | .events | reverse[]
      | select((.createdAt | sub("\\.[0-9]+"; "") | sub("[+-][0-9]{2}:[0-9]{2}$"; "Z") | fromdateiso8601) >= $since)
      | "  \(.createdAt[0:19]) \($s): \(.message)"'
  done
}

# Nodes that stopped (ECS keeps stopped tasks about an hour): why, and each container's exit.
stopped_tasks() {
  local name arns
  for name in "$SERVICE" "$CANARY"; do
    arns=$(aws ecs list-tasks --cluster "$CLUSTER" --service-name "$name" --desired-status STOPPED --max-items 100 --query 'taskArns' --output text)
    [ -n "$arns" ] && [ "$arns" != None ] || continue
    # shellcheck disable=SC2086 # one ARN per word
    aws ecs describe-tasks --cluster "$CLUSTER" --tasks $arns --output json | jq -r --arg s "$name" '.tasks[]
      | "  \($s) \(.taskArn | split("/")[-1]) rev \(.taskDefinitionArn | split(":")[-1]) started \(.startedAt // "never") stopped \(.stoppedAt // "?"): \(.stopCode // "") \(.stoppedReason // "")",
        (.containers[] | "    \(.name): exit \(.exitCode // "none") \(.reason // "") health \(.healthStatus // "")")'
  done
}

# Everything above since epoch $1: why a deploy failed (deploy prints it before rolling back).
explain() {
  local since=$1
  log "alarms:"
  alarm_reasons
  log "breaching minutes since $(iso "$since"):"
  breaches "$since"
  log "5xx by product:"
  errors_by_product "$since"
  log "service events:"
  service_events "$since"
  log "stopped nodes:"
  stopped_tasks
}

cmd_diagnose() {
  outputs
  local hours=${1:-24}
  (( hours >= 1 && hours <= 360 )) || die "hours: 1 to 360 (CloudWatch keeps one-minute data for 15 days)"
  log "fleet:"
  cmd_status
  log "running: $(image_of "$(task_def_of "$SERVICE")")"
  explain $(( $(date -u +%s) - hours * 3600 ))
}

case "${1:-}" in
  push | deploy | rollback | current | status | diagnose)
    cmd=$1
    shift
    "cmd_$cmd" "$@"
    ;;
  *)
    sed -n '2,19p' "$0" >&2
    exit 2
    ;;
esac
