#!/bin/bash
# The container health check: GET /healthz on the node's internal port (ROLL_API) answers 200
# while the node serves and isn't draining (docs/cloud/README.md, "Node settings"). The NLB checks
# the same route from outside. `roll-health dashboard` checks the dashboard instead.
set -eu
if [ "${1:-}" = dashboard ]; then
  url="http://${DASHBOARD_HOST:-127.0.0.1}:${DASHBOARD_PORT:-3000}/"
else
  api=${ROLL_API:-0.0.0.0:7070}
  port=${api##*:}
  url="http://127.0.0.1:${port:-7070}/healthz"
fi
exec curl -sf -o /dev/null -m 3 "$url"
