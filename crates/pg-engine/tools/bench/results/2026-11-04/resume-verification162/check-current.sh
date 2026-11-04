#!/usr/bin/env bash
set -euo pipefail
W=/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification
export PATH=/home/meszmate/.cargo/bin:$PATH
export CARGO_TARGET_DIR=/home/meszmate/.cache/a162/target TMPDIR=/home/meszmate/.cache/a162/tmp
export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
cd "$W"
EXPECTED=3997cf00
[[ $(git -C "$W" rev-parse --short=8 HEAD) == "$EXPECTED" ]]
[[ -z $(git -C "$W" status --porcelain) ]]
git -C "$W" rev-parse HEAD > "$V/source-head.txt"
git -C "$W" diff 63f7c0b4 HEAD > "$V/source.patch"
sha256sum "$V/source.patch" > "$V/source.patch.sha256"
: > "$V/status.tsv"
phase() {
  local name=$1; shift
  printf '%s\tSTART\t%s\n' "$name" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
  local rc=0
  "$@" > "$V/$name.log" 2>&1 || rc=$?
  printf '%s\t%s\t%s\n' "$name" "$rc" "$(date -u +%FT%TZ)" >> "$V/status.tsv"
  if (( rc != 0 )); then tail -35 "$V/$name.log"; exit "$rc"; fi
}
phase build cargo build --release -p pg-engine --bin roll-engine
sha256sum "$CARGO_TARGET_DIR/release/roll-engine" > "$V/roll-engine.sha256"
phase release cargo test --release --no-fail-fast -p pg-engine
rg -q '^test result: ok\.' "$V/release.log"
! rg -q '^test result: FAILED|^error:' "$V/release.log"
phase debug cargo test --no-fail-fast -p pg-engine
rg -q '^test result: ok\.' "$V/debug.log"
! rg -q '^test result: FAILED|^error:' "$V/debug.log"
phase clippy cargo clippy -p pg-engine --all-targets
phase postgres-unit pnpm test:pg --unit
rg -q 'postgres: (all green|no changes since the last green run)' "$V/postgres-unit.log"
phase dtbuild cargo test --release -p pg-engine --test difftest --no-run
python3 - "$CARGO_TARGET_DIR" "$V" <<'PY'
import glob,os,shutil,sys
p=max((p for p in glob.glob(sys.argv[1]+'/release/deps/difftest-*') if os.access(p,os.X_OK) and not p.endswith('.d')),key=os.path.getmtime)
shutil.copy2(p,sys.argv[2]+'/difftest-current')
PY
sha256sum "$V/difftest-current" > "$V/difftest-current.sha256"
printf 'COMPLETE\t0\t%s\n' "$(date -u +%FT%TZ)" >> "$V/status.tsv"
