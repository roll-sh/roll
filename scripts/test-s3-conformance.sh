#!/usr/bin/env bash
# roll storage against ceph/s3-tests, the S3 conformance suite (boto3 functional tests).
#
# Starts a fresh roll storage server (crates/s3-server/examples/serve.rs: a main admin key and
# two other keys standing in for other accounts), runs the pinned suite in Docker, and compares
# the outcome with tests/s3-conformance/expected.txt, which lists every test known not to pass
# and why. Fails on a regression (a failure that isn't listed) and on an unexpected pass (a
# listed test passing: remove it from the file), and if the server panicked.
#
# Two backends (`--backend <name>`, or S3_CONFORMANCE_BACKEND):
# - `local` (default): blobs in a local directory. No SSE-KMS, and SSE-S3 is only reported on
#   the write that asks for it.
# - `localstack`: the backing bucket on LocalStack S3 (pinned, in Docker), SSE-KMS passed through
#   with keys from LocalStack KMS (ROLL_S3_KMS), and ROLL_S3_SSE_S3_AT_REST set, since
#   LocalStack's S3 encrypts every object at rest as AWS S3 does.
# A line of expected.txt whose category ends in `@local` or `@localstack` only applies to that
# backend.
#
# Only runs when something storage-related changed since that backend's last green run;
# `--force` runs regardless. S3TESTS_WORKERS sets the pytest-xdist worker count (8). Every
# outcome ends up in target/s3-conformance[-localstack]/results.txt, the server's log in
# server.log next to it.
set -euo pipefail
cd "$(dirname "$0")/.."

# ceph/s3-tests, pinned. Bump it on purpose and update expected.txt in the same change.
S3TESTS_COMMIT=5b3987116eca0b4b8fa57d6b10f3db57bab196b6
# What real-AWS-compatible implementations run: no tests AWS itself fails (`fails_on_aws`), no
# Ceph extensions (append, S3 Select, IAM/STS/SNS, cloud tiering, storage classes for
# transitions, RGW's logging journal and rollover).
MARKS="not fails_on_aws and not appendobject and not s3select and not lifecycle_transition \
and not cloud_transition and not cloud_restore and not target_by_bucket and not bucket_logging_cleanup \
and not fails_without_logging_rollover and not sns and not s3control and not iam_account and not iam_user \
and not iam_role and not iam_cross_account and not iam_tenant and not user_policy and not role_policy \
and not group and not group_policy and not session_policy and not test_of_sts and not webidentity_test \
and not abac_test"
SUITE=(s3tests/functional/test_s3.py s3tests/functional/test_headers.py)
# Tests that see every bucket of the main key, so they can't share the server with other
# workers' buckets: they run first, one at a time, on the fresh server.
SERIAL=(s3tests/functional/test_s3.py::test_list_buckets_paginated)
# The same LocalStack as scripts/test-storage-aws.sh.
LOCALSTACK_IMAGE=localstack/localstack:4.9

PATHS=(crates/s3-proto crates/s3-store crates/s3-server Cargo.toml Cargo.lock tests/s3-conformance scripts/test-s3-conformance.sh)

force=0
backend=${S3_CONFORMANCE_BACKEND:-local}
while [[ $# -gt 0 ]]; do
  case "$1" in
    --force) force=1 ;;
    --backend) backend=${2:-}; shift ;;
    --backend=*) backend=${1#--backend=} ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done
case "$backend" in
  local) STAMP=target/.test-s3-conformance.hash; OUT=target/s3-conformance ;;
  localstack) STAMP=target/.test-s3-conformance-localstack.hash; OUT=target/s3-conformance-localstack ;;
  *) echo "unknown backend '$backend' (local or localstack)" >&2; exit 2 ;;
esac

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "s3-conformance ($backend): no changes since the last green run; skipping (--force to run anyway)"
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
localstack=
cleanup() {
  if [[ -n $server_pid ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ -n $localstack ]]; then docker rm -f "$localstack" >/dev/null 2>&1 || true; fi
  rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$work/suite" "$work/out" "$work/data"
curl -fsSL "https://github.com/ceph/s3-tests/archive/$S3TESTS_COMMIT.tar.gz" | tar -xz -C "$work/suite" --strip-components=1

image="roll-s3-tests:$(shasum -a 256 tests/s3-conformance/Dockerfile | cut -c1-12)"
docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" tests/s3-conformance >/dev/null

# Free ports (the suite reaches the server through the host network).
free_port() {
  for _ in $(seq 20); do
    local p=$((20000 + RANDOM % 20000))
    if ! (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then echo "$p"; return; fi
  done
  echo "s3-conformance: no free port" >&2
  exit 1
}
port=$(free_port)

serve_args=()
if [[ $backend == localstack ]]; then
  docker image inspect "$LOCALSTACK_IMAGE" >/dev/null 2>&1 || docker pull -q "$LOCALSTACK_IMAGE" >/dev/null
  # LocalStack (4.9 to at least 4.12) matches a trailer name by substring, so roll's
  # `x-amz-checksum-crc32c` trailer also counts as CRC32 and the upload is refused ("Multiple
  # checksum Types are not allowed"). Real S3 has no such problem; this boot hook (the same as
  # crates/s3-server/tests/aws.rs's) makes LocalStack compare the names exactly.
  mkdir -p "$work/boot"
  {
    echo '#!/bin/sh'
    echo 'f=/opt/code/localstack/localstack-core/localstack/services/s3/utils.py'
    echo "sed -i 's/if f\"x-amz-checksum-{algo.lower()}\" in trailing_headers/if f\"x-amz-checksum-{algo.lower()}\" in [h.strip().lower() for h in trailing_headers.split(\",\")]/' \"\$f\""
  } >"$work/boot/fix-crc32c-trailer.sh"
  chmod 755 "$work/boot" "$work/boot/fix-crc32c-trailer.sh"
  ls_port=$(free_port)
  localstack="roll-s3-conformance-$ls_port"
  docker run -d --rm --name "$localstack" -p "127.0.0.1:$ls_port:4566" -e SERVICES=s3,kms \
    -v "$work/boot:/etc/localstack/init/boot.d:ro" "$LOCALSTACK_IMAGE" >/dev/null
  ls_url="http://127.0.0.1:$ls_port"
  up=0
  for _ in $(seq 120); do
    health=$(curl -fsS "$ls_url/_localstack/health" 2>/dev/null || true)
    if grep -Eq '"s3": ?"(available|running)"' <<<"$health" && grep -Eq '"kms": ?"(available|running)"' <<<"$health"; then up=1; break; fi
    sleep 1
  done
  [[ $up == 1 ]] || { echo "s3-conformance: LocalStack didn't come up" >&2; docker logs "$localstack" >&2 || true; exit 1; }
  # LocalStack doesn't check signatures; the credential scope only routes the request.
  auth() { echo "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/$1/aws4_request, SignedHeaders=host, Signature=00"; }
  curl -fsS -X PUT -H "Authorization: $(auth s3)" "$ls_url/roll-conformance" >/dev/null
  # The suite's kms_keyid and kms_keyid2, by ARN (S3 reports a key by its ARN).
  kms_keys=()
  for _ in 1 2; do
    arn=$(curl -fsS -X POST -H "Authorization: $(auth kms)" -H "X-Amz-Target: TrentService.CreateKey" \
      -H "Content-Type: application/x-amz-json-1.1" -d '{}' "$ls_url/" | grep -Eo '"Arn": ?"[^"]+"' | cut -d'"' -f4)
    [[ -n $arn ]] || { echo "s3-conformance: LocalStack didn't create a KMS key" >&2; exit 1; }
    kms_keys+=("$arn")
  done
  serve_args=(--s3-bucket roll-conformance --s3-endpoint "$ls_url" --s3-region us-east-1 --kms true --sse-s3-at-rest true
              --kms-key-ids "${kms_keys[0]},${kms_keys[1]}")
  export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1
fi

target/debug/examples/serve --host 127.0.0.1 --port "$port" --dir "$work/data" --conf "$work/out/s3tests.conf" "${serve_args[@]}" >"$OUT/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 100); do
  [[ -f $work/out/s3tests.conf ]] && break
  kill -0 "$server_pid" 2>/dev/null || { cat "$OUT/server.log" >&2; exit 1; }
  sleep 0.1
done
[[ -f $work/out/s3tests.conf ]] || { echo "s3-conformance: the server didn't start" >&2; exit 1; }

suite() {
  docker run --rm --network host --user "$(id -u):$(id -g)" --cap-drop ALL --security-opt no-new-privileges \
    -e HOME=/tmp -e PYTHONDONTWRITEBYTECODE=1 -e S3TEST_CONF=/out/s3tests.conf \
    -v "$work/suite:/src:ro" -v "$PWD/tests/s3-conformance:/harness:ro" -v "$work/out:/out" -w /src \
    "$image" "$@"
}
deselect=()
for t in "${SERIAL[@]}"; do deselect+=(--deselect "$t"); done
# pytest's own exit status is the suite's failures; compare.py judges them.
suite python -m pytest -p no:cacheprovider -q -m "$MARKS" --junitxml=/out/serial.xml -o junit_family=xunit1 "${SERIAL[@]}" >"$OUT/pytest-serial.log" 2>&1 || true
suite python -m pytest -p no:cacheprovider -q -m "$MARKS" -n "${S3TESTS_WORKERS:-8}" --junitxml=/out/parallel.xml -o junit_family=xunit1 \
  "${deselect[@]}" "${SUITE[@]}" >"$OUT/pytest.log" 2>&1 || true
tail -n 1 "$OUT/pytest.log"
cp "$work"/out/*.xml "$OUT"/ 2>/dev/null || true
[[ -f $OUT/serial.xml && -f $OUT/parallel.xml ]] || { echo "s3-conformance: the suite didn't run (see $OUT/pytest*.log)" >&2; exit 1; }

status=0
suite python -I /harness/compare.py --backend "$backend" /harness/expected.txt /out/results.txt /out/serial.xml /out/parallel.xml || status=$?
cp "$work/out/results.txt" "$OUT/results.txt"
if grep -q "panicked" "$OUT/server.log"; then
  echo "s3-conformance: the server panicked (see $OUT/server.log)" >&2
  status=1
fi
if [[ $status != 0 ]]; then
  echo "s3-conformance ($backend): FAILED. Fix the regressions; for unexpected passes, remove the tests from tests/s3-conformance/expected.txt" >&2
  exit "$status"
fi
mkdir -p target && echo "$hash" > "$STAMP"
echo "s3-conformance ($backend): all green"
