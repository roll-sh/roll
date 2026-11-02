#!/bin/bash
set -euo pipefail
if ss -ltnH '( sport = :53210 )' | rg -q .; then exit 1; fi
printf '%s\n' "$$" > '/home/meszmate/.cache/pg-engine-perf-20261102/btree-selectivity/baseline.pid'
exec taskset -c 28-31 '/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/histogram-bin/roll-engine' --data '/home/meszmate/.cache/pg-engine-perf-20261102/btree-selectivity/baseline-data' --listen 127.0.0.1:53210 --password pw --cache-mb 2048 -c maintenance_work_mem=256MB -c max_connections=100 >> '/home/meszmate/.cache/pg-engine-perf-20261102/btree-selectivity/baseline-server.log' 2>&1
