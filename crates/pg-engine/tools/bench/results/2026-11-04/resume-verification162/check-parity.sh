#!/usr/bin/env bash
set -euo pipefail
W=/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification
C=/home/meszmate/.cache/a162
export PATH=/home/meszmate/.cargo/bin:$PATH
export CARGO_TARGET_DIR=$C/target TMPDIR=$C/tmp
cd "$W"
[[ $(git -C "$W" rev-parse HEAD) == $(cat "$V/source-head.txt") ]]
rg -q '^COMPLETE\s+0\s' "$V/status.tsv"
: > "$V/parity-status.tsv"
REF_STARTED=0
cleanup() {
 local rc=$?
 if ((REF_STARTED)); then docker stop -t 30 a162-rref > "$V/reference-cleanup.log" 2>&1 || true; fi
 exit "$rc"
}
trap cleanup EXIT
phase() {
 local name=$1; shift
 printf '%s\tSTART\t%s\n' "$name" "$(date -u +%FT%TZ)" >> "$V/parity-status.tsv"
 local rc=0
 "$@" > "$V/$name.log" 2>&1 || rc=$?
 printf '%s\t%s\t%s\n' "$name" "$rc" "$(date -u +%FT%TZ)" >> "$V/parity-status.tsv"
 if ((rc != 0)); then tail -35 "$V/$name.log"; exit "$rc"; fi
}
dt() {
 local label=$1 bin=$2 test=$3
 local out=$V/$label
 mkdir -p "$out"
 env DIFFTEST_REF=127.0.0.1:53240 DIFFTEST_WIRE_PORTS=53246-53249 \
 DIFFTEST_DB="a162_resume_$label" DIFFTEST_OUT="$out" DIFFTEST_SEEDS=9000-9149 \
 DIFFTEST_N=150 RUST_TEST_THREADS=1 "$bin" --ignored --exact "$test" --nocapture > "$out/run.log" 2>&1
 rg -q '^test result: ok\. 1 passed; 0 failed;' "$out/run.log"
 if [[ $test == run ]]; then rg -q '^difftest: seeds=150 statements=' "$out/run.log"; fi
 { rg '^  \[' "$out/run.log" || [[ $? == 1 ]]; } | sort -u > "$out/lines"
}
array_probe() {
 local label=$1 bin=$2
 local out=$V/$label
 mkdir -p "$out"
 env DIFFTEST_REF=127.0.0.1:53240 DIFFTEST_WIRE_PORTS=53246-53249 \
 DIFFTEST_DB="a162_resume_$label" DIFFTEST_OUT="$out" DIFFTEST_CASE="$V/array-header.sql" \
 RUST_TEST_THREADS=1 "$bin" --ignored --exact probe --nocapture > "$out/run.log" 2>&1
 rg -q '^test result: ok\. 1 passed; 0 failed;' "$out/run.log"
}
phase array-base array_probe array-base "$C/guard3/bin/dt-63f7c0b4"
phase array-current array_probe array-current "$V/difftest-current"
! rg -q '^!!' "$V/array-current/run.log"
phase smoke dt smoke "$V/difftest-current" smoke
phase pair-base dt pair-base "$C/guard3/bin/dt-63f7c0b4" run
phase pair-current dt pair-current "$V/difftest-current" run
comm -13 "$V/pair-base/lines" "$V/pair-current/lines" > "$V/pair-new.txt"
comm -23 "$V/pair-base/lines" "$V/pair-current/lines" > "$V/pair-gone.txt"
[[ ! -s "$V/pair-new.txt" ]]
phase regression-reference-start docker start a162-rref
REF_STARTED=1
R=$C/pgr/resume-3997cf00
I=$C/iso/resume-3997cf00
D=$C/pgdump/resume-3997cf00
mkdir -p "$R" "$I" "$D"
ln -s "$C/pgr/src-repo" "$R/src-repo"
ln -s "$C/iso/src-repo" "$I/src-repo"
phase regress env PGREGRESS_DIR="$R" PGREGRESS_DB=regression_a162_resume \
 PGREGRESS_PORT=53252 PGREGRESS_CONTAINER=a162-rref PGREGRESS_ENGINE_CONTAINER=a162-psql \
 PGREGRESS_REPORT_DIR="$V/regress-report" \
 cargo test --release -p pg-engine --test pg_regress -- pg_regress --exact --ignored --nocapture
rg -q 'pg_regress: 51148/51150 units match' "$V/regress.log"
phase regress-compare python3 "$V/compare-regress.py" "$C/pgr/report" "$V/regress-report"
phase isolation env PGISOLATION_DIR="$I" PGISOLATION_PORT=53253 \
 PGISOLATION_REF=127.0.0.1:53250 PGISOLATION_MAX_PREPARED=5 PGISOLATION_REPORT_DIR="$V/isolation-report" \
 cargo test --release -p pg-engine --test pg_isolation -- --ignored --nocapture
rg -q '121/122 specs, 1647/1648 permutations' "$V/isolation.log"
phase dump env PGDUMP_DIR="$D" PGDUMP_CLIENT=a162-dclient PGDUMP_PORT=53255 \
 PGDUMP_REPORT_DIR="$V/dump-report" PGDUMP_SRC="$C/pgr/src-repo" \
 cargo test --release -p pg-engine --test pg_dump -- --ignored --nocapture --exact pg_dump_tap
rg -q '\| total \| 14817 \| 0 \|' "$V/dump-report/TAP.md"
phase regression-reference-stop docker stop -t 30 a162-rref
REF_STARTED=0
printf 'COMPLETE\t0\t%s\n' "$(date -u +%FT%TZ)" >> "$V/parity-status.tsv"
