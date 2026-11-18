#!/bin/sh
# ONE verification train for many agents' changes (tests/difftest/STATUS.md, "Verification
# train"). The coordinator starts it; agents hand in candidates and never run the full train.
#
#   crates/pg-engine/tools/batch-train.sh cand/a238 cand/a240 cand/a241
#
# A candidate is a branch (on origin, else local) whose commits since its merge base with origin's
# branch are one agent's change, rebased by that agent and green on tools/quick-check.sh. The
# script returns at once: the train runs detached in a systemd user unit (a session restart
# doesn't stop it), one batch at a time (a mkdir lock; a second batch waits for it), under
# `sg docker`, and writes <run>/result when done:
#   1. a worktree at origin's head ($BT_HOME/wt); each candidate's commits cherry-picked in the
#      given order (one that conflicts is left out and reported)
#   2. tools/verification-train.sh once on the combination
#   3. green: the combination pushed with tools/push.sh (a guarded fast-forward), and its clippy
#      warnings kept for quick-check ($BT_HOME/clippy/<head>.txt)
#   4. red: the failing stages (and the failing test binaries) run again on the whole combination
#      (a failure that doesn't come back is a flake: everything lands); otherwise on prefixes of
#      it, halving, to find the first candidate at fault; that one is left out, the rest run the
#      failing stages again and land when green (repeated for a second culprit)
# <run>/result: for each red pass "train <head> + <candidates> (pass N): failing <stages>" and the
# failing stages' status lines and first failing tests with their messages (why: N newly
# differing units and which, the tests' names and panics, "engine did not start"); "infra (pass
# N): ..." for each reference suite run again because its engine did not start (a port taken for
# a moment: infrastructure, retried within the pass, not a failure); "flake <stage> <test>: <message>
# (<k> in the ledger)" for each test that failed and then passed on the same combination;
# "candidate <branch> landed|conflict|empty|failed <stage> <detail>|held"; then "pushed <sha>" or
# "pushed nothing", then "exit 0" (something landed or nothing to land) or "exit 1".
# Every pass keeps its logs in <run>/pass.<N>/ (status, stage logs, *.fails, the reports and
# engine logs of regress, isolation and pg_dump_tap).
#
# The flake ledger, $BT_HOME/flakes.tsv: one line per flake ("<date>\t<head>\t<stage>\t<test>\t
# <message>"), so a test that flakes again and again shows. When a flake brings one test to
# BT_FLAKE_LIMIT (default 3) uncommented lines, the batch lands nothing ("candidate ... held") until
# someone has looked at it: fix the test or the bug, then comment its lines out with "#".
#
# Environment: BT_HOME (default ~/.cache/pg-engine-train: worktree, lock, runs, train directory,
# flake ledger), BT_PREFIX (containers, default bt; made with tools/ref-containers.sh), BT_PORTS
# (preferred ports: regress, isolation, pg_dump, isolation reference; default none: the train takes
# free ones below the ephemeral range, tools/ports.sh), BT_CPUS (default the build CPUs
# 0-5,18-23), BT_DRY=1 (train and judge, push nothing), BT_KEEP_TARGET=1 (keep the worktree's
# build for the next batch; it is deleted after each batch otherwise), BT_TRAIN_SCRIPT (another
# train script, to test this one).
set -u
BRANCH=docs/postgres-architecture-strategy
H=${BT_HOME:-$HOME/.cache/pg-engine-train}
P=${BT_PREFIX:-bt}
PORTS=${BT_PORTS:-}
CPUS=${BT_CPUS:-0-5,18-23}
SELF=$(cd "$(dirname "$0")" && pwd)/batch-train.sh
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin"
[ $# -gt 0 ] || { echo "usage: batch-train.sh <candidate branch>..." >&2; exit 2; }

# --- start: detach ---------------------------------------------------------------------------
if [ -z "${BT_RUN:-}" ]; then
  mkdir -p "$H/runs"
  RUN=$H/runs/$(date +%Y%m%d-%H%M%S)
  mkdir "$RUN" || exit 1
  echo "$*" >"$RUN/candidates"
  UNIT=pg-engine-batch-train-$(basename "$RUN")
  systemd-run --user --unit="$UNIT" --collect --quiet -p LimitNOFILE=524288 -p WorkingDirectory="$REPO" \
    --setenv=PATH="$PATH" --setenv=HOME="$HOME" --setenv=BT_RUN="$RUN" --setenv=BT_HOME="$H" \
    --setenv=BT_PREFIX="$P" --setenv=BT_PORTS="$PORTS" --setenv=BT_CPUS="$CPUS" \
    --setenv=BT_DRY="${BT_DRY:-}" --setenv=BT_KEEP_TARGET="${BT_KEEP_TARGET:-}" \
    --setenv=BT_TRAIN_SCRIPT="${BT_TRAIN_SCRIPT:-}" ${HEAVY_LOCK:+--setenv=HEAVY_LOCK="$HEAVY_LOCK"} \
    /usr/bin/sg docker -c "exec $SELF $* >>$RUN/log 2>&1" || exit 1
  echo "batch train $UNIT: $RUN (result in $RUN/result; progress in $RUN/log)"
  exit 0
fi

# --- detached --------------------------------------------------------------------------------
RUN=$BT_RUN
log() { echo "$(date +%T) $*"; }
res() { echo "$*" >>"$RUN/result.part"; }
LOCK=$H/lock
until mkdir "$LOCK" 2>/dev/null; do
  if [ -f "$LOCK/pid" ] && ! kill -0 "$(cat "$LOCK/pid")" 2>/dev/null; then rm -rf "$LOCK"; continue; fi
  [ -f "$RUN/waiting" ] || { log "waiting for the batch train lock ($(cat "$LOCK/run" 2>/dev/null))"; : >"$RUN/waiting"; }
  sleep 60
done
echo $$ >"$LOCK/pid"; echo "$RUN" >"$LOCK/run"
WT=$H/wt TD=$H/train
trap 'rm -rf "$LOCK"; [ -n "${BT_KEEP_TARGET:-}" ] || rm -rf "$WT/target"' EXIT
: >"$RUN/result.part"
RP=$(echo "$PORTS" | cut -d, -f1-3); IREF=$(echo "$PORTS" | cut -s -d, -f4)
LEDGER=$H/flakes.tsv LIMIT=${BT_FLAKE_LIMIT:-3} PASS=0 HELD=
git -C "$REPO" fetch -q origin || { res "exit 1 fetch failed"; mv "$RUN/result.part" "$RUN/result"; exit 1; }
[ -d "$WT" ] || git -C "$REPO" worktree add -q --detach "$WT" "origin/$BRANCH"
ORIGIN=$(git -C "$WT" rev-parse "origin/$BRANCH")
log "origin $ORIGIN; candidates: $*"

# The commits of a candidate, oldest first.
commits() {
  ref=$1
  git -C "$WT" fetch -q origin "+refs/heads/$ref:refs/remotes/origin/$ref" 2>/dev/null && ref=origin/$ref
  git -C "$WT" rev-parse -q --verify "$ref^{commit}" >/dev/null || return 1
  git -C "$WT" rev-list --reverse --no-merges "$(git -C "$WT" merge-base "$ref" "$ORIGIN")..$ref"
}
# Check out origin's head plus the given candidates; prints the ones that applied.
apply() {
  git -C "$WT" cherry-pick --abort 2>/dev/null
  git -C "$WT" checkout -q --detach -f "$ORIGIN" && git -C "$WT" clean -qfd
  for c in "$@"; do
    cs=$(commits "$c") || { echo "candidate $c failed missing-branch" >>"$RUN/apply.log"; continue; }
    [ -n "$cs" ] || { echo "candidate $c empty" >>"$RUN/apply.log"; continue; }
    if git -C "$WT" -c user.name="pg-engine batch train" -c user.email=batch-train@localhost \
      cherry-pick --allow-empty --keep-redundant-commits $cs >>"$RUN/apply.log" 2>&1; then
      echo "$c"
    else
      git -C "$WT" cherry-pick --abort
      echo "candidate $c conflict $(git -C "$WT" diff --name-only --diff-filter=U | tr '\n' ' ')" >>"$RUN/apply.log"
    fi
  done
}
# Run the train on the worktree as it is: train <stages> [test binaries]; status in $TD/status,
# and the pass's logs kept in $RUN/pass.<N> ($PD).
train() {
  PD=$RUN/pass.$PASS
  log "train pass $PASS: stages ${1:-all}${2:+, tests $2}"
  echo "=== pass $PASS" >>"$RUN/train.log"
  taskset -c "$CPUS" "$WT/crates/pg-engine/tools/heavy.sh" env TRAIN_DIR="$TD" TRAIN_PREFIX="$P" \
    ${RP:+TRAIN_PORTS="$RP"} ${IREF:+TRAIN_ISO_REF="127.0.0.1:$IREF"} TRAIN_NO_CHECKOUT=1 CARGO_TARGET_DIR="$WT/target" \
    TRAIN_STAGES="${1:-build,clippy,tests,checked,crash,power,regress,iso,dump}" TRAIN_TESTS="${2:-}" \
    "${BT_TRAIN_SCRIPT:-$WT/crates/pg-engine/tools/verification-train.sh}" >>"$RUN/train.log" 2>&1
  mkdir -p "$PD"
  (cd "$TD" && for f in status ports infra ./*.log ./*.fails tests pgr/report iso/report pgdump/report \
    pgr/out/engine.log iso/out/engine.log pgdump/engine.log pgdump/tap-engine.log pgdump/tap-*.out; do
    [ -e "$f" ] && mkdir -p "$PD/$(dirname "$f")" && cp -rp "$f" "$PD/$(dirname "$f")/"; done)
  [ -s "$TD/infra" ] && sed "s/^/infra (pass $PASS): /" "$TD/infra" >>"$RUN/result.part"
  PASS=$((PASS + 1))
  log "train: $(awk '$1 != "head" && $1 != "done" && $2 != 0 {printf "%s ", $1}' "$TD/status")"
}
# Why the last pass failed, into the result: its failing stages' status lines and each stage's
# first failing tests.
why() {
  awk '$2 ~ /^[0-9]+$/ && $2 != 0 && $1 != "head" && $1 != "done" && $1 != "engine-bin" {print "  " $0}' "$TD/status" >>"$RUN/result.part"
  grep -q '^done ' "$TD/status" || res "  (the train did not finish: $RUN/train.log)"
  for f in "$TD"/*.fails; do
    [ -s "$f" ] && head -5 "$f" | sed "s/^/    $(basename "$f" .fails): /; s/\t/: /" >>"$RUN/result.part"
  done
}
# The status name of a .fails file's stage.
stage_of() { case $1 in iso) echo isolation ;; *) echo "$1" ;; esac; }
# Tests that failed in pass $1 and passed in pass $2 (the stages run again there): flakes. Each
# goes to the ledger and the result; one at the limit holds the batch ($HELD).
flakes() {
  a=$RUN/pass.$1 b=$RUN/pass.$2 head=$(git -C "$WT" rev-parse --short=9 HEAD)
  for st in $(awk '$2 ~ /^[0-9]+$/ && $2 != 0 && $1 != "head" && $1 != "done" && $1 != "engine-bin" {print $1}' "$a/status"); do
    line=$(awk -v s="$st" '$1 == s' "$b/status")
    [ -n "$line" ] || continue
    f=$(echo "$st" | sed 's/^isolation$/iso/')
    if [ -s "$a/$f.fails" ]; then cat "$a/$f.fails"; else printf '%s\t%s\n' "$st" "$(awk -v s="$st" '$1 == s' "$a/status" | cut -d' ' -f2-)"; fi |
    while IFS=$(printf '\t') read -r name msg; do
      case $name in "(engine did not start)") continue ;; esac
      if [ -s "$b/$f.fails" ]; then cut -f1 "$b/$f.fails" | grep -qxF "$name" && continue
      elif [ "$(echo "$line" | cut -d' ' -f2)" != 0 ]; then continue; fi
      printf '%s\t%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%MZ)" "$head" "$st" "$name" "$msg" >>"$LEDGER"
      k=$(grep -v '^#' "$LEDGER" | awk -F'\t' -v s="$st" -v t="$name" '$3 == s && $4 == t' | wc -l)
      res "flake $st $name: $msg ($k in the ledger)"
      [ "$k" -lt "$LIMIT" ] || { res "flake limit: $st $name failed and passed again $k times ($LEDGER): look at it, then comment its lines out with #"; : >"$RUN/held"; }
    done
  done
}
# The failing stages of the last train, as TRAIN_STAGES names ("" when green), and the failing
# test binaries.
failing() {
  f=$(awk '$2 ~ /^[0-9]+$/ && $2 != 0 && $1 != "head" && $1 != "done" && $1 != "engine-bin" {print $1}' "$TD/status" |
    sed 's/^power-cut$/power/; s/^crash-power.*/power/; s/^isolation$/iso/' | sort -u | paste -sd, -)
  grep -q '^done ' "$TD/status" || f="${f:+$f,}unfinished"
  echo "$f"
}
fail_bins() {
  awk '($1 == "tests" || $1 == "checked") && $2 != 0' "$TD/status" |
    grep -o '[a-z][a-z_0-9]*::\|[a-z][a-z_0-9]*:[a-z-][a-z-]*' | sed 's/:.*//' | sort -u | paste -sd' ' -
}
green() { [ -z "$(failing)" ]; }

CANDS=$(apply "$@")
log "applied: $(echo $CANDS)"
FINAL=
if [ -n "$CANDS" ]; then
  train
  if green; then
    FINAL=$CANDS
  else
    S=$(failing); B=$(fail_bins)
    res "train $ORIGIN + all (pass 0): failing $S${B:+ ($B)}"; why
    case ",$S," in *,build,*|*,unfinished,*) S=clippy,tests,checked,crash,power,regress,iso,dump B= ;; esac
    # A failure that doesn't come back is a flake.
    train "build,$S" "$B"
    flakes 0 1
    if green; then
      res "flakes: $S passed on the same combination again"
      FINAL=$CANDS
    else
      S=$(failing); B=$(fail_bins)
      REST=$CANDS
      for round in 1 2; do
        set -- $REST
        lo=0 hi=$#
        # The first prefix that fails names the candidate at fault.
        while [ $((hi - lo)) -gt 1 ]; do
          mid=$(( (lo + hi) / 2 ))
          apply $(echo $REST | cut -d' ' -f1-$mid) >/dev/null
          train "build,$S" "$B"
          if green; then lo=$mid; else hi=$mid; res "train $ORIGIN + $(echo $REST | cut -d' ' -f1-$mid) (pass $((PASS - 1))): failing $(failing)"; why; fi
        done
        BAD=$(echo $REST | cut -d' ' -f$hi)
        res "candidate $BAD failed $S${B:+ ($B)}"
        REST=$(for c in $REST; do [ "$c" = "$BAD" ] || echo "$c"; done)
        [ -n "$REST" ] || break
        apply $REST >/dev/null
        train "build,$S" "$B"
        if green; then FINAL=$REST; break; fi
        S=$(failing); B=$(fail_bins)
        res "train $ORIGIN + $(echo $REST) (pass $((PASS - 1))): failing $S${B:+ ($B)}"; why
        case ",$S," in *,build,*|*,unfinished,*) S=clippy,tests,checked,crash,power,regress,iso,dump B= ;; esac
      done
    fi
  fi
fi
cat "$RUN/apply.log" 2>/dev/null | grep '^candidate' >>"$RUN/result.part"
PUSHED=
if [ -n "$FINAL" ] && [ -f "$RUN/held" ]; then
  for c in $FINAL; do res "candidate $c held (flake limit)"; done
  FINAL=
fi
if [ -n "$FINAL" ]; then
  [ "$(apply $FINAL | paste -sd' ' -)" = "$(echo $FINAL)" ] || res "re-applying the green set failed"
  HEAD=$(git -C "$WT" rev-parse HEAD)
  mkdir -p "$H/clippy"
  [ -s "$TD/clippy-warnings.txt" ] && cp "$TD/clippy-warnings.txt" "$H/clippy/$HEAD.txt"
  if [ -n "${BT_DRY:-}" ]; then
    res "dry run: would push $HEAD"
  elif (cd "$WT" && crates/pg-engine/tools/push.sh) >"$RUN/push.log" 2>&1; then
    PUSHED=$HEAD
  else
    # Origin moved (run the batch again), or push.sh's guard refused lines a candidate takes
    # back: push $HEAD from the worktree by hand with PUSH_REMOVAL_REVIEW (push.sh's header).
    res "push failed for $HEAD: $(grep -v '^ *$' "$RUN/push.log" | head -3 | paste -sd' ' -)"
  fi
  cat "$RUN/push.log" >>"$RUN/log"
  for c in $FINAL; do res "candidate $c $([ -n "$PUSHED" ] && echo landed || echo green-not-pushed)"; done
fi
res "pushed ${PUSHED:-nothing}"
rc=1
if [ -n "$PUSHED" ] || { [ -n "$FINAL" ] && [ -n "${BT_DRY:-}" ]; } || [ -z "$CANDS" ]; then rc=0; fi
res "exit $rc"
mv "$RUN/result.part" "$RUN/result"
log "done: $(tail -2 "$RUN/result" | paste -sd' ' -)"
exit $rc
