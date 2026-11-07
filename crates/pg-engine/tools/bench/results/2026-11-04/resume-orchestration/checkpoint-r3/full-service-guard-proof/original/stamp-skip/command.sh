set -euo pipefail
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/full-service-guard-proof/original/stamp-skip
rc=0
if ((rc != 0)); then tail -50 "$V/pnpm-test-pg-full.log"; exit "$rc"; fi
rg -q '^postgres: all green$' "$V/pnpm-test-pg-full.log"
rg -q 'Running tests/e2e.rs' "$V/pnpm-test-pg-full.log"
! rg -q 'test result: FAILED|^error:|no changes since the last green run' "$V/pnpm-test-pg-full.log"
printf 'COMPLETE\t0\t%s\n' "$(date -u +%FT%TZ)" >> "$V/status.tsv"
