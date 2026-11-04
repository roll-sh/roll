#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/embedded165v3
mkdir -p "$ev"
test ! -e "$ev/exits.tsv"
git -C "$W" rev-parse HEAD > "$ev/head.txt"
git -C "$W" diff --binary > "$ev/production.patch"
cp "$E/tests/cursor_replay_probe.rs" "$ev/"
run() {
 local n=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 return "$status"
}
ref=4c7dfb3163fe4fc80a56903c187c33bee8e67d48cee599baa4e3ec66bad53edd
test "$(docker inspect -f '{{.Id}}' a165-gref)" = "$ref"
if [[ "$(docker inspect -f '{{.State.Running}}' "$ref")" != true ]]; then docker start "$ref" > "$ev/reference-start.log"; fi
docker inspect -f '{{.Id}} {{.Image}} {{json .Config.Cmd}}' "$ref" > "$ev/reference.txt"
docker exec "$ref" psql -U postgres -p 53310 -Atc 'select version()' >> "$ev/reference.txt"
run shape cargo test -p pg-engine --test cursor_replay_probe -- --nocapture
grep -q 'domain projection rows retained' "$ev/shape.log"
run record env GOLDEN_RECORD=127.0.0.1:53310 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a165_cursor_embedded cargo test -p pg-engine --test golden -- --nocapture
if grep -q '^ERROR' "$E/tests/sql/a165_cursor_embedded.out"; then exit 1; fi
cp "$E/tests/sql/a165_cursor_embedded."{sql,out} "$ev/"
status=0
run compare env GOLDEN_FILTER=a165_cursor_embedded cargo test -p pg-engine --test golden -- --nocapture || status=$?
test "$status" -eq 101
grep -q 'test result: FAILED' "$ev/compare.log"
if grep -Eq '^error\[|could not compile' "$ev/compare.log"; then exit 1; fi
cp "$E/tests/sql/a165_cursor_embedded.actual" "$ev/caab.actual"
sha256sum "$E/tests/sql/a165_cursor_embedded."{sql,out} > "$ev/golden.sha256"
echo EMBEDDED165-RED-CONFIRMED
