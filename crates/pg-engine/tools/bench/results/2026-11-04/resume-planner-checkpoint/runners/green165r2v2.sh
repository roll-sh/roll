#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/green165r2v2
mkdir -p "$ev"
test ! -e "$ev/exits.tsv"
git -C "$W" rev-parse HEAD > "$ev/head.txt"
git -C "$W" diff --binary > "$ev/source.patch"
run() {
 local n=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 test "$status" -eq 0
 grep -q 'test result: ok' "$ev/$n.log"
 if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/head.txt")"
 git -C "$W" diff --binary > "$ev/current.patch"
 cmp "$ev/source.patch" "$ev/current.patch"
}
ref=4c7dfb3163fe4fc80a56903c187c33bee8e67d48cee599baa4e3ec66bad53edd
test "$(docker inspect -f '{{.Id}}' a165-gref)" = "$ref"
docker inspect -f '{{.Id}} {{.Image}}' "$ref" > "$ev/reference.txt"
docker exec "$ref" psql -U postgres -p 53310 -Atc 'select version()' >> "$ev/reference.txt"
cp "$E/tests/sql/a165_cursor_"{embedded,edges}.sql "$ev/"
run record env GOLDEN_RECORD=127.0.0.1:53310 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a165_cursor_ cargo test -p pg-engine --test golden -- --nocapture
cp "$E/tests/sql/a165_cursor_"{embedded,edges}.{sql,out} "$ev/"
run cursor env GOLDEN_FILTER=a165_cursor_ cargo test -p pg-engine --test golden -- --nocapture
run lock cargo test -p pg-engine --test twophase -- --nocapture
run memory cargo test -p pg-engine --test memory_load cursor_ -- --nocapture
echo GREEN165R2-PASS
