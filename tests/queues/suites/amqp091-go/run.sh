#!/usr/bin/env bash
# Runs rabbitmq/amqp091-go's own integration tests (go test -tags integration) against a broker.
# Usage: run.sh <host> <port>   (host as seen from containers, normally host.docker.internal)
set -euo pipefail

HOST=${1:?usage: run.sh <host> <port>}
PORT=${2:?usage: run.sh <host> <port>}
TAG=v1.15.0
REPO=https://github.com/rabbitmq/amqp091-go
IMAGE=golang:1.25-alpine
SRC=roll-qtest-amqp091-go-src
HERE=$(cd "$(dirname "$0")" && pwd)

for v in "$SRC" roll-qtest-gomod roll-qtest-gobuild; do docker volume create "$v" >/dev/null; done

# Fetch the pinned tag once into a named volume.
docker run --rm -v "$SRC":/src "$IMAGE" sh -euc "
  [ \"\$(cat /src/.tag 2>/dev/null)\" = '$TAG' ] && exit 0
  apk add --no-cache -q git >/dev/null
  rm -rf /src/repo /src/.tag
  git -c advice.detachedHead=false clone -q --depth 1 --branch '$TAG' '$REPO' /src/repo
  echo '$TAG' > /src/.tag"

# Excluded tests: first column of excluded.txt.
NAMES=$(awk '!/^[[:space:]]*(#|$)/ {print $1}' "$HERE/excluded.txt")
EXCLUDED=$(printf '%s' "$NAMES" | paste -sd'|' -)
NEXCL=$(printf '%s' "$NAMES" | awk 'END {print NR}')
WORK=$(mktemp -d -t roll-qtest-amqp091-go.XXXXXX)
trap 'rm -rf "$WORK"' EXIT
ALL=$WORK/all.log
: >"$ALL"
gorc=0
# A panic or timeout aborts the whole test binary, so re-run with every test that already
# reported (plus the one that crashed) skipped until the binary runs to completion.
for round in $(seq 1 20); do
  DONE=$(sed -nE 's/^--- (PASS|FAIL|SKIP): ([^ ]+).*/\2/p' "$ALL" | paste -sd'|' -)
  PAT=$(printf '%s\n%s\n' "$EXCLUDED" "$DONE" | grep -v '^$' | paste -sd'|' - || true)
  SKIP=()
  [ -n "$PAT" ] && SKIP=(-skip "^($PAT)\$")
  LOG=$WORK/round$round.log
  set +e
  docker run --rm --add-host=host.docker.internal:host-gateway \
    -e AMQP_URL="amqp://guest:guest@$HOST:$PORT/" -e CGO_ENABLED=0 \
    -v "$SRC":/src -v roll-qtest-gomod:/go/pkg/mod -v roll-qtest-gobuild:/root/.cache/go-build \
    -w /src/repo "$IMAGE" \
    go test -tags integration -count=1 -v -timeout 5m ${SKIP[@]+"${SKIP[@]}"} . 2>&1 | tee "$LOG"
  gorc=${PIPESTATUS[0]}
  set -e
  grep -E '^--- (PASS|FAIL|SKIP): ' "$LOG" >>"$ALL" || true
  [ "$gorc" -eq 0 ] && break
  grep -q '^panic: ' "$LOG" || break
  if grep -q '^panic: test timed out' "$LOG"; then
    victim=$(awk '/^running tests:/ {getline; print $1; exit}' "$LOG")
    [ -n "$victim" ] && echo "--- FAIL: $victim (test binary timed out)" >>"$ALL"
  elif ! awk '/^panic: / {exit !(prev ~ /^--- FAIL: /)} {prev = $0}' "$LOG"; then
    victim=$(sed -nE 's/^=== (RUN|CONT) +([^/ ]+)$/\2/p' "$LOG" | tail -n 1)
    [ -n "$victim" ] && echo "--- FAIL: $victim (test binary panicked)" >>"$ALL"
  fi
  echo ">>> test binary crashed; re-running the remaining tests" >&2
done

passed=$(grep -c '^--- PASS: ' "$ALL" || true)
failed=$(grep -c '^--- FAIL: ' "$ALL" || true)
skipped=$(grep -c '^--- SKIP: ' "$ALL" || true)
echo
if [ "$failed" -gt 0 ]; then
  echo "failed tests:"
  sed -n 's/^--- FAIL: /  /p' "$ALL"
fi
if [ "$gorc" -ne 0 ] && [ "$failed" -eq 0 ]; then
  # Build error or goleak failure after all tests passed.
  echo "go test exited $gorc without a failing test (see output above)"
  failed=1
fi
echo "SUITE amqp091-go passed=$passed failed=$failed skipped=$skipped excluded=$NEXCL"
[ "$failed" -eq 0 ]
