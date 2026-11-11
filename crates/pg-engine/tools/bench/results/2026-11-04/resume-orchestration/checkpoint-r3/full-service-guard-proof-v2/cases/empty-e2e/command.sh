#!/usr/bin/env bash
set -euo pipefail
W=/home/meszmate/.cache/pg-engine-resume-20261104/verification/full-service-guard-proof-v2/cases/empty-e2e/work
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/full-service-guard-proof-v2/cases/empty-e2e/out
EXPECTED=${1:?exact final source commit required}
export PATH=/home/meszmate/.cargo/bin:$PATH
export CARGO_TARGET_DIR=/home/meszmate/.cache/a162/target TMPDIR=/home/meszmate/.cache/a162/tmp
export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
export ROLL_E2E_THREADS=2 ROLL_E2E_BIND=0.0.0.0
mkdir -p "$V"
cd "$W"
[[ $(git -C "$W" rev-parse HEAD) == "$EXPECTED" ]]
[[ -z $(git -C "$W" status --porcelain) ]]
[[ ! -e "$V/status.tsv" ]]
git -C "$W" rev-parse HEAD > "$V/source-head.txt"
git -C "$W" status --porcelain > "$V/source-dirty.txt"
printf 'ROLL_E2E_BIND=%s\nROLL_E2E_THREADS=%s\nCARGO_TARGET_DIR=%s\n' "$ROLL_E2E_BIND" "$ROLL_E2E_THREADS" "$CARGO_TARGET_DIR" > "$V/environment.txt"
sg docker -c 'docker ps -a --format "{{.ID}} {{.Names}} {{.Labels}}"' > "$V/docker-before.txt"
printf 'network-preflight\tSTART\t%s\n' "$(date -u +%FT%TZ)" > "$V/status.tsv"
network_rc=0
python3 /home/meszmate/.cache/pg-engine-resume-20261104/verification/service-network-preflight.py > "$V/network-preflight.log" 2>&1 || network_rc=$?
printf 'network-preflight\t%s\t%s\n' "$network_rc" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
if ((network_rc != 0)); then
    cat "$V/network-preflight.log"
    exit "$network_rc"
fi
printf 'pnpm-test-pg-full\tSTART\t%s\n' "$(date -u +%FT%TZ)" >> "$V/status.tsv"
rc=0
pnpm test:pg --force > "$V/pnpm-test-pg-full.log" 2>&1 || rc=$?
printf 'pnpm-test-pg-full\t%s\t%s\n' "$rc" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
sg docker -c 'docker ps -a --format "{{.ID}} {{.Names}} {{.Labels}}"' > "$V/docker-after.txt"
[[ $(git -C "$W" rev-parse HEAD) == "$EXPECTED" ]]
if ((rc != 0)); then tail -50 "$V/pnpm-test-pg-full.log"; exit "$rc"; fi
rg -q '^postgres: all green$' "$V/pnpm-test-pg-full.log"
rg -q 'Running tests/e2e.rs' "$V/pnpm-test-pg-full.log"
if rg -q 'test result: FAILED|^error(\[[^]]+\])?:|panicked|no changes since the last green run|skipped: set ROLL_E2E' "$V/pnpm-test-pg-full.log"; then
    printf 'service log contains a failure or skipped execution\n' >&2
    exit 1
fi
# Require an executed, fully passing e2e binary; unit results alone are insufficient.
if ! awk '
    /^[[:space:]]*Running tests\/e2e\.rs / { active = 1; next }
    active && /^test result: / {
        if ($0 !~ /^test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;/) bad = 1;
        seen++; active = 0;
    }
    END { exit !(seen == 1 && !active && !bad) }
' "$V/pnpm-test-pg-full.log"; then
    printf 'service e2e summary missing, empty, ignored or failed\n' >&2
    exit 1
fi
printf 'COMPLETE\t0\t%s\n' "$(date -u +%FT%TZ)" >> "$V/status.tsv"
