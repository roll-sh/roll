#!/usr/bin/env bash
# roll storage against ceph/s3-tests, the S3 conformance suite (boto3 functional tests).
#
# Starts a fresh roll storage server (crates/s3-server/examples/serve.rs: a local directory
# backend, a main admin key and two other keys standing in for other accounts), runs the pinned
# suite in Docker, and compares the outcome with tests/s3-conformance/expected.txt, which lists
# every test known not to pass and why. Fails on a regression (a failure that isn't listed) and
# on an unexpected pass (a listed test passing: remove it from the file), and if the server
# panicked.
#
# Only runs when something storage-related changed since the last green run; `--force` runs
# regardless. S3TESTS_WORKERS sets the pytest-xdist worker count (8). Every outcome ends up in
# target/s3-conformance/results.txt, the server's log in target/s3-conformance/server.log.
set -euo pipefail
cd "$(dirname "$0")/.."

# ceph/s3-tests, pinned. Bump it on purpose and update expected.txt in the same change.
S3TESTS_COMMIT=5b3987116eca0b4b8fa57d6b10f3db57bab196b6
# What real-AWS-compatible implementations run: no tests AWS itself fails (`fails_on_aws`), no
# Ceph extensions (RGW's append API, IAM/STS/SNS, cloud tiering, storage classes for
# transitions, RGW's logging journal and rollover). S3 Select's tests run (test_s3select.py).
MARKS="not fails_on_aws and not appendobject and not lifecycle_transition \
and not cloud_transition and not cloud_restore and not target_by_bucket and not bucket_logging_cleanup \
and not fails_without_logging_rollover and not sns and not s3control and not iam_account and not iam_user \
and not iam_role and not iam_cross_account and not iam_tenant and not user_policy and not role_policy \
and not group and not group_policy and not session_policy and not test_of_sts and not webidentity_test \
and not abac_test"
SUITE=(s3tests/functional/test_s3.py s3tests/functional/test_headers.py s3tests/functional/test_s3select.py)
# Tests that see every bucket of the main key, so they can't share the server with other
# workers' buckets: they run first, one at a time, on the fresh server.
SERIAL=(s3tests/functional/test_s3.py::test_list_buckets_paginated)

PATHS=(crates/s3-proto crates/s3-select crates/s3-store crates/s3-server Cargo.toml Cargo.lock tests/s3-conformance scripts/test-s3-conformance.sh)
STAMP=target/.test-s3-conformance.hash
OUT=target/s3-conformance

force=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "s3-conformance: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi
if ! docker info >/dev/null 2>&1; then
  echo "s3-conformance: Docker is not running" >&2
  exit 1
fi

cargo build -p s3-server --example serve
mkdir -p "$OUT"
rm -f "$OUT"/*.xml "$OUT"/results.txt

# The suite is downloaded code: it lives in its own directory, is mounted read-only, and only
# ever runs inside the container.
work=$(mktemp -d)
server_pid=
cleanup() {
  if [[ -n $server_pid ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$work/suite" "$work/out" "$work/data"
curl -fsSL "https://github.com/ceph/s3-tests/archive/$S3TESTS_COMMIT.tar.gz" | tar -xz -C "$work/suite" --strip-components=1

image="roll-s3-tests:$(shasum -a 256 tests/s3-conformance/Dockerfile | cut -c1-12)"
docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" tests/s3-conformance >/dev/null

# A free port (the suite reaches the server through the host network).
port=
for _ in $(seq 20); do
  p=$((20000 + RANDOM % 20000))
  if ! (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then port=$p; break; fi
done
[[ -n $port ]] || { echo "s3-conformance: no free port" >&2; exit 1; }
target/debug/examples/serve --host 127.0.0.1 --port "$port" --dir "$work/data" --conf "$work/out/s3tests.conf" >"$OUT/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 100); do
  [[ -f $work/out/s3tests.conf ]] && break
  kill -0 "$server_pid" 2>/dev/null || { cat "$OUT/server.log" >&2; exit 1; }
  sleep 0.1
done
[[ -f $work/out/s3tests.conf ]] || { echo "s3-conformance: the server didn't start" >&2; exit 1; }

suite() {
  docker run --rm --network host --user "$(id -u):$(id -g)" --cap-drop ALL --security-opt no-new-privileges \
    -e HOME=/tmp -e PYTHONDONTWRITEBYTECODE=1 -e PYTHONPATH=/harness -e S3TEST_CONF=/out/s3tests.conf \
    -v "$work/suite:/src:ro" -v "$PWD/tests/s3-conformance:/harness:ro" -v "$work/out:/out" -w /src \
    "$image" "$@"
}
deselect=()
for t in "${SERIAL[@]}"; do deselect+=(--deselect "$t"); done
# pytest's own exit status is the suite's failures; compare.py judges them.
suite python -m pytest -p no:cacheprovider -p roll_plugin -q -m "$MARKS" --junitxml=/out/serial.xml -o junit_family=xunit1 "${SERIAL[@]}" >"$OUT/pytest-serial.log" 2>&1 || true
suite python -m pytest -p no:cacheprovider -p roll_plugin -q -m "$MARKS" -n "${S3TESTS_WORKERS:-8}" --junitxml=/out/parallel.xml -o junit_family=xunit1 \
  "${deselect[@]}" "${SUITE[@]}" >"$OUT/pytest.log" 2>&1 || true
tail -n 1 "$OUT/pytest.log"
cp "$work"/out/*.xml "$OUT"/ 2>/dev/null || true
[[ -f $OUT/serial.xml && -f $OUT/parallel.xml ]] || { echo "s3-conformance: the suite didn't run (see $OUT/pytest*.log)" >&2; exit 1; }

status=0
suite python -I /harness/compare.py /harness/expected.txt /out/results.txt /out/serial.xml /out/parallel.xml || status=$?
cp "$work/out/results.txt" "$OUT/results.txt"
if grep -q "panicked" "$OUT/server.log"; then
  echo "s3-conformance: the server panicked (see $OUT/server.log)" >&2
  status=1
fi
if [[ $status != 0 ]]; then
  echo "s3-conformance: FAILED. Fix the regressions; for unexpected passes, remove the tests from tests/s3-conformance/expected.txt" >&2
  exit "$status"
fi
mkdir -p target && echo "$hash" > "$STAMP"
echo "s3-conformance: all green"
