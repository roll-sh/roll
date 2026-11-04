#!/usr/bin/env bash
set -euo pipefail
if [[ ${A162_DOCKER_GROUP:-} != 1 ]]; then
 exec sg docker -c 'A162_DOCKER_GROUP=1 bash /home/meszmate/.cache/pg-engine-resume-20261104/verification/dump-only-v5/check-dump-only.sh'
fi
W=/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/dump-only-v5
C=/home/meszmate/.cache/a162
R=$C/pgr/resume-3997cf00-r4
D=$C/pgdump/resume-3997cf00-dump5
export PATH=/home/meszmate/.cargo/bin:$PATH CARGO_TARGET_DIR=$C/target TMPDIR=$C/tmp
cd "$W"
[[ ! -e $V/status.tsv ]]
: > "$V/status.tsv"
cleanup() {
 local rc=$?
 trap - EXIT
 set +e
 python3 "$V/cleanup.py" > "$V/cleanup.log" 2>&1
 local cr=$?
 printf 'cleanup\t%s\t%s\n' "$cr" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
 if ((rc == 0 && cr != 0)); then rc=$cr; fi
 if ((rc == 0)); then printf 'COMPLETE\t0\t%s\n' "$(date -u +%FT%TZ)" >> "$V/status.tsv"; fi
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
phase prepare python3 "$V/prepare.py"
phase preflight python3 "$V/preflight.py"
phase dump env PGDUMP_DIR="$D" PGDUMP_CLIENT=a162-dclient PGDUMP_PORT=53255 \
 PGDUMP_ENGINE_BIN="$R/roll-engine-pinned.bin" PGDUMP_REPORT_DIR="$V/dump-report" PGDUMP_SRC="$R/src-repo" \
 cargo test --release -p pg-engine --test pg_dump -- --ignored --nocapture --exact pg_dump_tap
phase dump-count python3 "$V/check-tap.py" "$D"
phase dump-audit python3 "$V/audit-tap.py" pg-dump "$V/dump.log" "$D"
phase dump-report rg -F -x -q '| total | 14817 | 0 |' "$V/dump-report/TAP.md"
phase final-audit python3 "$V/final-audit.py"
