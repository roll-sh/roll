#!/usr/bin/env bash
# cap.sh <scylla|cassandra> <script.py> [args...]: run a capture script in the network namespace of
# the running reference container nosql-ref-<dialect> (default image flags, see conformance.md).
# Output JSON goes to out/ (not committed); `python3 render.py <docs/nosql/reference>` renders it.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
d=$1; shift
while (( $(awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo) < 6 )); do echo "waiting for memory" >&2; sleep 10; done
img=$(docker images roll-nosql-conformance --format '{{.Repository}}:{{.Tag}}' | grep -v ':dev' | head -1)
ip=$(docker inspect -f "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}" nosql-ref-$d)
exec docker run --rm --memory 1g --network container:nosql-ref-$d -e CAP_HOST="$ip" -v "$here:/cap" "$img" python /cap/"$@"
