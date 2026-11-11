#!/bin/bash
# The fleet image's entrypoint (under tini). Two commands:
#
#   rolld [args]   a fleet node. ROLL_NODE_ID, ROLL_ZONE and ROLL_ADVERTISE_URL come from the
#                  instance's metadata (IMDSv2) unless they're already set, so the task
#                  definition is the same on every node (docs/cloud/README.md, "Node settings").
#   dashboard      the dashboard's production server on 127.0.0.1:3000 (DASHBOARD_HOST and
#                  DASHBOARD_PORT change it); rolld proxies to it (ROLL_DASHBOARD_UPSTREAM).
#
# Anything else runs as given (`roll-cloud-entrypoint bash`).
set -euo pipefail

# ROLL_IMDS_URL is for tests.
imds=${ROLL_IMDS_URL:-http://169.254.169.254/latest}
token=

# One metadata value, or nothing if there's no instance metadata (outside EC2).
meta() {
  if [ -z "$token" ]; then
    token=$(curl -sf -m 2 -X PUT "$imds/api/token" -H 'X-aws-ec2-metadata-token-ttl-seconds: 300') || return 1
  fi
  curl -sf -m 2 -H "X-aws-ec2-metadata-token: $token" "$imds/meta-data/$1"
}

# Sets $1 from instance metadata path $2 (through $3, a printf format) if $1 is unset.
from_meta() {
  local name=$1 path=$2 format=${3:-%s} value
  [ -n "${!name:-}" ] && return 0
  if ! value=$(meta "$path") || [ -z "$value" ]; then
    # A cluster node can't join without an identity; a single node doesn't need one.
    if [ "${ROLL_CLUSTER_BACKEND:-memory}" != memory ]; then
      echo "roll-cloud: $name is unset and instance metadata ($path) is unreachable" >&2
      exit 1
    fi
    return 0
  fi
  # shellcheck disable=SC2059 # the format is ours
  printf -v "$name" "$format" "$value"
  export "${name?}"
}

[ $# -gt 0 ] || set -- rolld
api=${ROLL_API:-0.0.0.0:7070}
case "$1" in
  rolld)
    from_meta ROLL_NODE_ID instance-id
    from_meta ROLL_ZONE placement/availability-zone
    from_meta ROLL_ADVERTISE_URL local-ipv4 "http://%s:${api##*:}"
    echo "roll-cloud: node ${ROLL_NODE_ID:-?} in ${ROLL_ZONE:-?} at ${ROLL_ADVERTISE_URL:-?}" >&2
    exec "$@"
    ;;
  dashboard)
    cd /opt/roll/dashboard
    exec node server.mjs -H "${DASHBOARD_HOST:-127.0.0.1}" -p "${DASHBOARD_PORT:-3000}"
    ;;
  *)
    exec "$@"
    ;;
esac
