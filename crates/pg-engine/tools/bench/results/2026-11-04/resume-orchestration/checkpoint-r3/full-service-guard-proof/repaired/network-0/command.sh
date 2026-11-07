set -euo pipefail
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/full-service-guard-proof/repaired/network-0
network_rc=0
if ((network_rc != 0)); then
    cat "$V/network-preflight.log"
    exit "$network_rc"
fi
printf 'SERVICE_START\n'
