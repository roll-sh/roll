#!/bin/bash
set -u
cd /home/meszmate/.cache/pg-engine-resume-20261104/storage_perf169 || exit 90
[ -f BUILD-PASS ] || exit 91
[ ! -e actual-resume1-exit.json ] || exit 92
nice -n 19 taskset -c 20-21 python3 resume-1.py
rc=$?
printf 'RUNNER-EXIT %s\n' "$rc"
exit "$rc"
