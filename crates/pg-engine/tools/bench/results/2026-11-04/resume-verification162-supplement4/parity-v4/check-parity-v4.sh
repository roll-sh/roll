#!/usr/bin/env bash
set -euo pipefail
if [[ ${A162_DOCKER_GROUP:-} != 1 ]]; then
 exec sg docker -c 'A162_DOCKER_GROUP=1 bash /home/meszmate/.cache/pg-engine-resume-20261104/verification/parity-v4/check-parity-v4.sh'
fi
W=/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/parity-v4
C=/home/meszmate/.cache/a162
export PATH=/home/meszmate/.cargo/bin:$PATH
export CARGO_TARGET_DIR=$C/target TMPDIR=$C/tmp
cd "$W"
[[ $(git -C "$W" rev-parse HEAD) == 3997cf00a2a301fed65f00469625ff14f223f227 ]]
[[ -z $(git -C "$W" status --porcelain) ]]
[[ ! -e $V/status.tsv ]]
: > "$V/status.tsv"
REF_STARTED=0
cleanup() {
 local rc=$?
 trap - EXIT
 if ((REF_STARTED)); then
  set +e
  docker stop -t 30 a162-rref > "$V/reference-cleanup.log" 2>&1
  local clean_rc=$?
  printf 'reference-cleanup\t%s\t%s\n' "$clean_rc" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
  ((rc == 0 && clean_rc != 0)) && rc=$clean_rc
 fi
 printf 'TERMINAL\t%s\t%s\n' "$rc" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
 exit "$rc"
}
trap cleanup EXIT
phase() {
 local name=$1; shift
 printf '%s\tSTART\t%s\n' "$name" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
 local rc
 set +e
 ( set -euo pipefail; "$@" ) > "$V/$name.log" 2>&1
 rc=$?
 set -e
 printf '%s\t%s\t%s\n' "$name" "$rc" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
 if ((rc != 0)); then tail -35 "$V/$name.log"; exit "$rc"; fi
}
phase fixtures python3 "$V/prepare-fixtures.py"
phase array-validator-controls python3 "$V/prove-array.py"
phase reference-start docker start a162-rref
REF_STARTED=1
phase docker-preflight python3 "$V/preflight.py"
phase copy-reproduction python3 "$V/reproduce-copy.py"
R=$C/pgr/resume-3997cf00-r4
I=$C/iso/resume-3997cf00-r4
D=$C/pgdump/resume-3997cf00-r4
phase regress env PGREGRESS_DIR="$R" PGREGRESS_DB=regression_a162_resume \
 PGREGRESS_PORT=53252 PGREGRESS_CONTAINER=a162-rref PGREGRESS_ENGINE_CONTAINER=a162-psql \
 PGREGRESS_ENGINE_BIN="$R/roll-engine-pinned.bin" PGREGRESS_REPORT_DIR="$V/regress-report" \
 cargo test --release -p pg-engine --test pg_regress -- pg_regress --exact --ignored --nocapture
phase regress-units rg -q 'pg_regress: 51148/51150 units match' "$V/regress.log"
phase regress-compare python3 "$V/../compare-regress.py" "$C/pgr/report" "$V/regress-report"
phase isolation env PGISOLATION_DIR="$I" PGISOLATION_PORT=53253 \
 PGISOLATION_ENGINE_BIN="$R/roll-engine-pinned.bin" \
 PGISOLATION_REF=127.0.0.1:53250 PGISOLATION_MAX_PREPARED=5 PGISOLATION_REPORT_DIR="$V/isolation-report" \
 cargo test --release -p pg-engine --test pg_isolation -- --ignored --nocapture
phase isolation-count rg -q '121/122 specs, 1647/1648 permutations' "$V/isolation.log"
phase dump-preflight docker exec -w "$D/tap" a162-dclient sh -eu -c 'test -w .; pg_dump --version; pg_restore --version; psql --version'
phase dump-version rg -q 'pg_dump (PostgreSQL) 18.6' "$V/dump-preflight.log"
phase dump env PGDUMP_DIR="$D" PGDUMP_CLIENT=a162-dclient PGDUMP_PORT=53255 \
 PGDUMP_ENGINE_BIN="$R/roll-engine-pinned.bin" \
 PGDUMP_REPORT_DIR="$V/dump-report" PGDUMP_SRC="$R/src-repo" \
 cargo test --release -p pg-engine --test pg_dump -- --ignored --nocapture --exact pg_dump_tap
phase dump-count python3 /home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/check-tap.py "$D"
phase dump-audit python3 "$V/../audit-tap.py" pg-dump "$V/dump.log" "$D"
phase dump-report rg -q '\| total \| 14817 \| 0 \|' "$V/dump-report/TAP.md"
phase identities python3 "$V/verify-final.py"
printf 'COMPLETE\t0\t%s\n' "$(date -u +%FT%TZ)" >> "$V/status.tsv"
