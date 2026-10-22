#!/usr/bin/env bash
# ScyllaDB's conformance suites (test/cqlpy and test/alternator) against a CQL / Alternator
# endpoint: roll, or a reference image this script starts itself.
#
# The suites are under ScyllaDB's source-available licence, so they are never committed: they
# are fetched at a pinned commit (sparse, blobless) into target/nosql-conformance/scylladb/<commit>
# and mounted read-only into a runner image built from scripts/nosql-conformance/.
#
#   scripts/nosql-conformance.sh --reference scylla                      # cqlpy on ScyllaDB
#   scripts/nosql-conformance.sh --reference cassandra                   # cqlpy on Cassandra
#   scripts/nosql-conformance.sh --reference scylla --suite alternator
#   scripts/nosql-conformance.sh --reference scylla --reference-config suite   # the suites' own flags
#   scripts/nosql-conformance.sh --roll scylla                           # roll, built and started here
#   scripts/nosql-conformance.sh --roll cassandra --compare crates/nosql-server/tests/conformance/reference-cassandra-cqlpy.txt
#   scripts/nosql-conformance.sh --target-container my-roll              # a running container
#   scripts/nosql-conformance.sh --host 127.0.0.1 --port 9042 --filter 'test_select'
#   scripts/nosql-conformance.sh --suite alternator --alternator-url http://127.0.0.1:8000
#   scripts/nosql-conformance.sh ... --compare crates/nosql-server/tests/conformance/reference-scylla-cqlpy.txt
#   scripts/nosql-conformance.sh ... -- cqlpy/test_json.py -x          # extra args go to pytest
#
# Output: a sorted `test_id PASS|FAIL|SKIP|XFAIL|XPASS|ERROR` list (--results, default
# target/nosql-conformance/<suite>-<target>.txt), a summary, and optionally JUnit XML (--junit).
# --compare FILE fails when a test that PASSes in FILE does not PASS here, unless it is listed in
# the known-failures file (--known-failures FILE, `none` for no list; with --roll <dialect> the
# default is crates/nosql-server/tests/conformance/roll-known-failures-<dialect>.txt, and
# roll-known-failures-<dialect>-alternator.txt for --suite alternator). That file
# has one `test_id  # note` per line; listed tests that now pass are reported so the list can be
# pruned (it may only shrink). XPASS is a strict-xfail test that passes: the suite marks a ScyllaDB
# bug there, and roll doesn't copy those bugs, so XPASS where FILE has XFAIL is accepted and listed
# apart (any other XPASS counts as not passing).
#
# --reference starts nosql-refs-<dialect> (--rm, memory-capped) and stops it on exit. By default
# (`--reference-config suite`) it uses the flags of the suites' own `run`/`run-cassandra` scripts
# (PasswordAuthenticator with cassandra/cassandra, UDFs, MVs, SASI, tablets, ...), which is how the
# committed baselines were made; `--reference-config default` runs the image as documented in
# docs/nosql/compatibility.md instead. --target-container NAME joins NAME's network namespace and
# targets its IP (for an already running roll or reference container).
#
# --roll <dialect> builds `crates/nosql-server/examples/conformance_server.rs` (with $CARGO, default
# cargo, which may be a wrapper script; or uses $ROLL_CONFORMANCE_SERVER) and runs it on
# $ROLL_CONFORMANCE_ROLL_IP (default 127.0.0.2, so its REST API can take port 10000 next to
# anything on 127.0.0.1) with the same --reference-config, then stops it. Its log is
# target/nosql-conformance/out/roll.log; the run fails when roll exits early or panics. With
# --suite alternator it also serves Alternator on <ip>:8000 (ScyllaDB dialect only).
#
# Env: ROLL_CONFORMANCE_RUNNER_MEMORY (default 1g) caps the runner container;
# ROLL_CONFORMANCE_TIMEOUT (default 900) is the per-test timeout in seconds; ROLL_CASSANDRA_HEAP /
# ROLL_CASSANDRA_HEAP_NEW (default 640M / 128M) size the Cassandra reference;
# ROLL_CONFORMANCE_MIN_MEM_GB (default 0) waits for that much MemAvailable before each docker run;
# ROLL_CONFORMANCE_RUN_DIR (default target/nosql-conformance) holds the run's out/ and roll-data/,
# so concurrent runs (with different ROLL_CONFORMANCE_ROLL_IPs) don't share them.
#
# Notes:
# - Tests reach the ScyllaDB REST API at <host>:10000 (hard-wired in the suites). When it is not
#   there, REST-dependent tests skip themselves; against Cassandra they use $NODETOOL instead
#   (reference mode sets it to `docker exec <container> nodetool`).
# - Alternator tests read a role's salted hash over CQL on <alternator host>:9042 to sign requests.
# - Without --reference the runner uses host networking, so --host 127.0.0.1 reaches a local roll.
set -euo pipefail
cd "$(dirname "$0")/.."

# scylla-2026.3.3, the same build as the scylla reference image.
COMMIT=${ROLL_CONFORMANCE_COMMIT:-d80b3d6ca26b5457c1b59b47046fab27fa1953f3}
REPO=${ROLL_CONFORMANCE_REPO:-https://github.com/scylladb/scylladb}
SCYLLA_IMAGE=${ROLL_SCYLLA_IMAGE:-scylladb/scylla:2026.3.3}
CASSANDRA_IMAGE=${ROLL_CASSANDRA_IMAGE:-cassandra:5.0.9}
RUNNER_IMAGE=roll-nosql-conformance:$(cat scripts/nosql-conformance/* | shasum -a 256 | cut -c1-12)
NETWORK=nosql-conformance
WORK=target/nosql-conformance
# Per-run files (out/, roll-data/): ROLL_CONFORMANCE_RUN_DIR keeps concurrent runs apart.
RUN=${ROLL_CONFORMANCE_RUN_DIR:-$WORK}
[[ $RUN == /* ]] || RUN=$PWD/$RUN

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2; }

suite=cqlpy host='' port=9042 alt_url='' filter='' junit='' results='' reference='' compare='' network=''
ref_config=suite target='' roll='' known=''
pytest_args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --suite) suite=$2; shift 2 ;;
    --host) host=$2; shift 2 ;;
    --port) port=$2; shift 2 ;;
    --alternator-url) alt_url=$2; shift 2 ;;
    --filter) filter=$2; shift 2 ;;
    --junit) junit=$2; shift 2 ;;
    --results) results=$2; shift 2 ;;
    --reference) reference=$2; shift 2 ;;
    --compare) compare=$2; shift 2 ;;
    --known-failures) known=$2; shift 2 ;;
    --network) network=$2; shift 2 ;;
    --reference-config) ref_config=$2; shift 2 ;;
    --target-container) target=$2; shift 2 ;;
    --roll) roll=$2; shift 2 ;;
    --) shift; pytest_args=("$@"); break ;;
    -h|--help) usage ;;
    *) echo "unknown argument $1" >&2; usage ;;
  esac
done
[[ $suite == cqlpy || $suite == alternator ]] || { echo "--suite must be cqlpy or alternator" >&2; exit 2; }
[[ -z $reference || $reference == scylla || $reference == cassandra ]] || { echo "--reference must be scylla or cassandra" >&2; exit 2; }
[[ $suite == alternator && $reference == cassandra ]] && { echo "Cassandra has no Alternator" >&2; exit 2; }
[[ -z $roll || $roll == scylla || $roll == cassandra ]] || { echo "--roll must be scylla or cassandra" >&2; exit 2; }
[[ -n $roll && -n $reference ]] && { echo "--roll and --reference are exclusive" >&2; exit 2; }
[[ -n $roll && $suite == alternator && $roll == cassandra ]] && { echo "Cassandra has no Alternator" >&2; exit 2; }
[[ $ref_config == default || $ref_config == suite ]] || { echo "--reference-config must be default or suite" >&2; exit 2; }
RUNNER_MEMORY=${ROLL_CONFORMANCE_RUNNER_MEMORY:-1g}
if [[ -z $known && -n $roll ]]; then
  known=crates/nosql-server/tests/conformance/roll-known-failures-$roll.txt
  [[ $suite == alternator ]] && known=crates/nosql-server/tests/conformance/roll-known-failures-$roll-alternator.txt
  [[ -f $known ]] || known=''
fi
[[ $known == none ]] && known=''
[[ -z $known || -f $known ]] || { echo "--known-failures: no such file $known" >&2; exit 2; }
wait_memory() {
  local want=${ROLL_CONFORMANCE_MIN_MEM_GB:-0}
  while (( $(awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo) < want )); do
    echo "nosql-conformance: waiting for ${want} GB of available memory"; sleep 10
  done
}
docker info >/dev/null 2>&1 || { echo "nosql-conformance: Docker is not running" >&2; exit 1; }

# 1. The suites at the pinned commit.
src=$WORK/scylladb/$COMMIT
if [[ ! -f $src/.fetched ]]; then
  echo "nosql-conformance: fetching scylladb test/ at $COMMIT"
  rm -rf "$src" && mkdir -p "$src"
  git -C "$src" init -q
  git -C "$src" remote add origin "$REPO"
  git -C "$src" sparse-checkout set --no-cone '/test/*.py' /test/pytest.ini /test/cqlpy/ /test/alternator/ \
    /test/pylib/ /test/cluster/util.py
  git -C "$src" fetch -q --depth 1 --filter=blob:none origin "$COMMIT"
  git -C "$src" checkout -q FETCH_HEAD
  touch "$src/.fetched"
fi

# 2. The runner image.
if ! docker image inspect "$RUNNER_IMAGE" >/dev/null 2>&1; then
  echo "nosql-conformance: building $RUNNER_IMAGE"
  docker build -q -t "$RUNNER_IMAGE" scripts/nosql-conformance >/dev/null
fi

# 3. The reference, when asked for.
container='' runner_net=(--network host) env_args=() mounts=()
roll_pid=''
cleanup() {
  if [[ -n $roll_pid ]]; then kill "$roll_pid" 2>/dev/null || true; wait "$roll_pid" 2>/dev/null || true; fi
  if [[ -n $container ]]; then
    docker logs "$container" > "$RUN/out/reference.log" 2>&1 || true
    docker rm -f "$container" >/dev/null 2>&1 || true
  fi
  [[ -n ${runner_name:-} ]] && docker rm -f "$runner_name" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

if [[ -n $network ]]; then runner_net=(--network "$network"); fi
if [[ -n $target ]]; then
  runner_net=(--network "container:$target")
  host=${host:-$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$target")}
  [[ $suite == alternator && -z $alt_url ]] && alt_url=http://$host:8000
  if docker inspect -f '{{.Config.Image}}' "$target" | grep -q cassandra; then
    env_args+=(-e NODETOOL=nodetool-docker -e "ROLL_NODETOOL_CONTAINER=$target")
    mounts+=(-v /var/run/docker.sock:/var/run/docker.sock)
  fi
fi
if [[ -n $reference ]]; then
  docker network inspect "$NETWORK" >/dev/null 2>&1 || docker network create "$NETWORK" >/dev/null
  runner_net=(--network "$NETWORK")
  container=nosql-refs-$reference
  # Several jobs may share a small machine: wait while two reference containers already run.
  for _ in $(seq 1 120); do
    n=$(docker ps -q --filter ancestor="$SCYLLA_IMAGE" --filter ancestor="$CASSANDRA_IMAGE" | wc -l)
    (( n < ${ROLL_CONFORMANCE_MAX_REFS:-2} )) && break
    echo "nosql-conformance: $n reference containers running; waiting"; sleep 15
  done
  wait_memory
  docker rm -f "$container" >/dev/null 2>&1 || true
  if [[ $reference == scylla ]]; then
    # Seastar subtracts its own reserve from the cgroup limit, so --memory 1G needs a 3g cap.
    # Fewer AIO control blocks (as test/cqlpy/run.py) so it starts next to other ScyllaDB processes.
    args=(--smp 1 --memory 1G --overprovisioned 1 --developer-mode 1 --api-address 0.0.0.0
      --max-networking-io-control-blocks=1000)
    if [[ $suite == alternator ]]; then
      args+=(--alternator-address 0.0.0.0 --alternator-port 8000)
      [[ $ref_config == default ]] && args+=(--alternator-write-isolation always_use_lwt)
    fi
    if [[ $ref_config == suite ]]; then
      # test/cqlpy/run.py; the salted hash is the password "cassandra".
      # The image's entrypoint sources its arguments through bash, so $ must be escaped.
      salted='\$6\$x7IFjiX5VCpvNiFk\$2IfjTvSyGL7zerpV.wbY7mJjaRCrJ/68dtT3UpT.sSmNYz1bPjtn3mH.kJKFvaZ2T4SbVeBijjmwGjcb83LlV/'
      args+=(--ring-delay-ms 0 --unsafe-bypass-fsync 1 --kernel-page-cache=1 --commitlog-use-o-dsync 0
        --flush-schema-tables-after-modification=false --auto-snapshot 0 --num-tokens 16
        --query-tombstone-page-limit=1000
        --range-request-timeout-in-ms 300000 --read-request-timeout-in-ms 300000
        --counter-write-request-timeout-in-ms 300000 --cas-contention-timeout-in-ms 300000
        --truncate-request-timeout-in-ms 300000 --write-request-timeout-in-ms 300000
        --request-timeout-in-ms 300000 --user-defined-function-time-limit-ms 1000
        --group0-raft-op-timeout-in-ms=300000
        --experimental-features=udf --experimental-features=views-with-tablets
        --enable-tablets=true --enable-user-defined-functions 1 --rf-rack-valid-keyspaces=1
        --authenticator PasswordAuthenticator --authorizer CassandraAuthorizer
        --auth-superuser-name=cassandra "--auth-superuser-salted-password=$salted"
        --strict-allow-filtering=true --strict-is-not-null-in-views=true
        --permissions-update-interval-in-ms 100 --permissions-validity-in-ms 5
        --shutdown-announce-in-ms 0 --service-levels-interval-ms=500 --tablets-initial-scale-factor=1
        --minimum-replication-factor-warn-threshold=-1 --alternator-ttl-period-in-seconds=0.5)
      # test/alternator/run
      [[ $suite == alternator ]] && args+=(--alternator-enforce-authorization 1
        --alternator-write-isolation only_rmw_uses_lwt --alternator-streams-time-window-s 0
        --alternator-timeout-in-ms 30000 --alternator-allow-system-table-write=1
        --tablet-load-stats-refresh-interval-in-seconds=1)
    fi
    docker run -d --rm --name "$container" --network "$NETWORK" --memory 3g "$SCYLLA_IMAGE" "${args[@]}" >/dev/null
  else
    setup='exec docker-entrypoint.sh cassandra -f'
    if [[ $ref_config == suite ]]; then
      # test/cqlpy/run-cassandra's cassandra.yaml settings, applied to the image's own file.
      setup='y=/etc/cassandra/cassandra.yaml
        sed -i -E "s/^authenticator:.*/authenticator: PasswordAuthenticator/;
          s/^authorizer:.*/authorizer: CassandraAuthorizer/;
          s/^user_defined_functions_enabled:.*/user_defined_functions_enabled: true/;
          s/^materialized_views_enabled:.*/materialized_views_enabled: true/;
          s/^sasi_indexes_enabled:.*/sasi_indexes_enabled: true/;
          s/^auto_snapshot:.*/auto_snapshot: false/;
          s/^permissions_validity:.*/permissions_validity: 100ms/" $y
        echo "permissions_update_interval: 100ms" >> $y
        export JVM_EXTRA_OPTS=-Dcassandra.sai.vector.allow_custom_parameters=true
        exec docker-entrypoint.sh cassandra -f'
    fi
    docker run -d --rm --name "$container" --network "$NETWORK" --memory 2560m \
      -e MAX_HEAP_SIZE="${ROLL_CASSANDRA_HEAP:-640M}" -e HEAP_NEWSIZE="${ROLL_CASSANDRA_HEAP_NEW:-128M}" --entrypoint sh "$CASSANDRA_IMAGE" -c "$setup" >/dev/null
    env_args+=(-e NODETOOL=nodetool-docker -e "ROLL_NODETOOL_CONTAINER=$container")
    mounts+=(-v /var/run/docker.sock:/var/run/docker.sock)
  fi
  # Address the reference by IP: some tests compare the endpoint host with the node's address.
  host=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$container")
  [[ $suite == alternator ]] && alt_url=http://$host:8000
fi
if [[ -n $roll ]]; then
  server=${ROLL_CONFORMANCE_SERVER:-}
  if [[ -z $server ]]; then
    # Ask cargo where the binary is: $CARGO may be a wrapper with its own CARGO_TARGET_DIR.
    server=$("${CARGO:-cargo}" build -q -p nosql-server --example conformance_server \
      --message-format=json-render-diagnostics | grep -o '"executable":"[^"]*/conformance_server"' | tail -1 | cut -d'"' -f4)
    [[ -x $server ]] || { echo "nosql-conformance: could not build conformance_server" >&2; exit 1; }
  fi
  host=${ROLL_CONFORMANCE_ROLL_IP:-127.0.0.2}
  rm -rf "$RUN/roll-data" && mkdir -p "$RUN/roll-data" "$RUN/out"
  RUST_BACKTRACE=${RUST_BACKTRACE:-1} RUST_LOG=${RUST_LOG:-warn} \
    "$server" --dialect "$roll" --ip "$host" --cql-port "$port" --config "$ref_config" --data "$RUN/roll-data" \
    > "$RUN/out/roll.log" 2>&1 &
  roll_pid=$!
  [[ $suite == alternator && -z $alt_url ]] && alt_url=http://$host:8000
fi
host=${host:-127.0.0.1}
[[ -n ${NODETOOL:-} ]] && env_args+=(-e "NODETOOL=$NODETOOL")

# 4. Wait for the endpoint (and log in as cassandra/cassandra when auth is on).
echo "nosql-conformance: waiting for $host:$port${alt_url:+ and $alt_url}"
wait_memory
docker run --rm --memory "$RUNNER_MEMORY" "${runner_net[@]}" "$RUNNER_IMAGE" python -c '
import sys, time, requests
from cassandra.cluster import Cluster
from cassandra.auth import PlainTextAuthProvider
host, port, url = sys.argv[1], int(sys.argv[2]), sys.argv[3]
deadline = time.time() + 300
while True:
    try:
        # Credentials are ignored by AllowAllAuthenticator and used by PasswordAuthenticator.
        c = Cluster([host], port=port, auth_provider=PlainTextAuthProvider("cassandra", "cassandra"), connect_timeout=5)
        c.connect().execute("SELECT key FROM system.local"); c.shutdown()
        if url: requests.get(url, timeout=5)
        break
    except Exception as e:
        if time.time() > deadline: sys.exit(f"endpoint not ready: {e}")
        time.sleep(3)
' "$host" "$port" "$alt_url"

# 5. Run the suite.
label=${reference:-${roll:+roll-$roll}}
label=${label:-${target:-target}}
results=${results:-$WORK/$suite-$label.txt}
mkdir -p "$RUN/out" "$(dirname "$results")"
# Paths go first: pytest only loads the suites' conftest options for paths it has already seen.
args=()
[[ ${#pytest_args[@]} -gt 0 ]] && args+=("${pytest_args[@]}") || args+=("$suite")
args+=(-p results -p no:cacheprovider -o junit_family=xunit2 --timeout=900 -rN -q --junitxml=/out/junit.xml)
args=("${args[@]/--timeout=900/--timeout=${ROLL_CONFORMANCE_TIMEOUT:-900}}")
if [[ $suite == cqlpy ]]; then
  args+=("--host=$host" "--port=$port")
else
  # Include the handful of tests the suite marks veryslow (about 10).
  args+=("--url=${alt_url:-http://$host:8000}" --runveryslow)
fi
[[ -n $filter ]] && args+=(-k "$filter")

# Tests that take the endpoint away for a while (disable/enable the native transport, ...). The
# suites' conftest declares the server crashed when a test leaves it unreachable and skips every
# later test, so on a full run these go last, in their own pytest process.
# test_create_index_oversized_name makes Cassandra hit a file-system error (a 500-character
# directory name) and stop its native transport under the default disk_failure_policy.
DISRUPTIVE=(cqlpy/test_native_transport.py::test_enable_disable_binary
            cqlpy/test_secondary_index.py::test_create_index_oversized_name)
full_run=$([[ -z $filter && ${#pytest_args[@]} == 0 ]] && echo 1 || echo 0)
late=() opts=("${args[@]:1}")
if [[ $full_run == 1 ]]; then
  for t in "${DISRUPTIVE[@]}"; do
    [[ $t == "$suite"/* ]] || continue
    late+=("$t"); args+=(--deselect "$t")
  done
fi

run_pytest() { # <name> <pytest args...>
  local name=$1; shift
  runner_name=nosql-conformance-run-$$
  rm -f "$RUN/out/results-$name.txt"
  wait_memory
  docker run --rm --name "$runner_name" --memory "$RUNNER_MEMORY" "${runner_net[@]}" "${env_args[@]}" "${mounts[@]}" \
    -v "$PWD/$src:/scylladb:ro" -v "$RUN/out:/out" -w /scylladb/test \
    -e ROLL_CONFORMANCE_RESULTS="/out/results-$name.txt" -e PYTHONPATH=/runner:/scylladb \
    "$RUNNER_IMAGE" python -m pytest "$@" > "$RUN/out/pytest-$name.log" 2>&1
}
run_late() { # each disruptive test in its own process, so one that kills the server can't hide another
  local i=0 t
  rm -f "$RUN/out/results-late.txt" "$RUN/out/pytest-late.log"; : > "$RUN/out/results-late.txt"; : > "$RUN/out/pytest-late.log"
  for t in "${late[@]}"; do
    i=$((i + 1))
    run_pytest "late$i" "$t" "${opts[@]/--junitxml=\/out\/junit.xml/--junitxml=/out/junit-late$i.xml}"
    late_code=$?
    cat "$RUN/out/results-late$i.txt" >> "$RUN/out/results-late.txt" 2>/dev/null
    cat "$RUN/out/pytest-late$i.log" >> "$RUN/out/pytest-late.log"
    (( late_code > 1 && code <= 1 )) && code=$late_code
  done
}

started=$(date +%s)
echo "nosql-conformance: running $suite against ${reference:-${target:-$host}} (commit ${COMMIT:0:12})"
set +e
run_pytest main "${args[@]}"
code=$?
[[ ${#late[@]} -gt 0 ]] && run_late
set -e
elapsed=$(( $(date +%s) - started ))
runner_name=''

[[ -f $RUN/out/results-main.txt ]] || { tail -40 "$RUN/out/pytest-main.log" >&2; echo "nosql-conformance: no results (pytest exit $code)" >&2; exit 1; }
cat "$RUN/out"/results-main.txt ${late:+"$RUN/out"/results-late.txt} 2>/dev/null | LC_ALL=C sort > "$results"
cat "$RUN/out"/pytest-main.log ${late:+"$RUN/out"/pytest-late.log} > "$RUN/out/pytest.log" 2>/dev/null || true
[[ -n $junit ]] && cp "$RUN/out/junit.xml" "$junit"
grep -E '^(=+ )?[0-9]+ (passed|failed|skipped|error)|in [0-9.]+s' "$RUN/out/pytest.log" | tail -2
echo "nosql-conformance: $suite on $label: $(awk '{print $NF}' "$results" | sort | uniq -c | awk '{printf "%s %s  ", $2, $1}')total $(wc -l < "$results"), ${elapsed}s"
echo "nosql-conformance: results in $results, log in $RUN/out/pytest.log"

status=0
(( code > 1 )) && { echo "nosql-conformance: pytest exited with $code" >&2; status=$code; }
if [[ -n $roll_pid ]]; then
  if ! kill -0 "$roll_pid" 2>/dev/null; then
    rc=0; wait "$roll_pid" || rc=$?
    echo "nosql-conformance: roll exited during the run (status $rc); tail of $RUN/out/roll.log:" >&2
    tail -30 "$RUN/out/roll.log" >&2; status=1; roll_pid=''
  fi
  panics=$(grep -c 'panicked at' "$RUN/out/roll.log" || true)
  if (( panics > 0 )); then
    echo "nosql-conformance: roll panicked $panics times (see $RUN/out/roll.log):" >&2
    grep -A1 'panicked at' "$RUN/out/roll.log" | grep -v '^--$' | sort | uniq -c | sort -rn | head -20 >&2
    status=1
  fi
fi
if [[ -n $compare ]]; then
  # Known failures: `test_id  # note` (or a bare id); blank lines and `#` lines are ignored.
  known_ids=$(mktemp); trap 'rm -f "$known_ids"; cleanup' EXIT
  [[ -n $known ]] && sed -E 's/[[:space:]]+#.*$//; /^#/d; /^[[:space:]]*$/d' "$known" > "$known_ids"
  diff=$(awk -v full="$full_run" '
    FILENAME == ARGV[1] { kf[$0] = 1; next }
    { r = $NF; t = substr($0, 1, length($0) - length(r) - 1) }
    FILENAME == ARGV[2] { res[t] = r; next }
    r != "PASS" { next }
    !(t in res) && full != 1 { next }
    { got = (t in res) ? res[t] : "MISSING" }
    got != "PASS" && !(t in kf) { print "REGRESSION " t " " got }
    got == "PASS" && (t in kf) { print "FIXED " t }
    got != "PASS" && (t in kf) { print "KNOWN " t }
  ' "$known_ids" "$results" "$compare")
  # Strict xfails of the reference (its bugs) that pass here.
  xpass=$(awk '
    { r = $NF; t = substr($0, 1, length($0) - length(r) - 1) }
    FILENAME == ARGV[1] { res[t] = r; next }
    r == "XFAIL" && res[t] == "XPASS" { print t }
  ' "$results" "$compare")
  regressions=$(grep '^REGRESSION ' <<< "$diff" | cut -d' ' -f2- || true)
  if [[ -n $xpass ]]; then
    echo "nosql-conformance: $(wc -l <<< "$xpass") tests the suite marks as a ScyllaDB bug (strict xfail, XFAIL in $compare) pass here; roll doesn't copy those bugs:"
    echo "$xpass"
  fi
  fixed=$(grep '^FIXED ' <<< "$diff" | cut -d' ' -f2- || true)
  nknown=$(grep -c '^KNOWN ' <<< "$diff" || true)
  [[ -n $known ]] && echo "nosql-conformance: $nknown tests that pass in $compare still fail here and are listed in $known"
  if [[ -n $fixed ]]; then
    echo "nosql-conformance: $(wc -l <<< "$fixed") tests listed in $known pass now; remove them from the list:"
    echo "$fixed"
  fi
  if [[ -n $regressions ]]; then
    echo "nosql-conformance: $(wc -l <<< "$regressions") tests pass in $compare but not here${known:+ and are not in $known}:" >&2
    echo "$regressions" >&2
    status=1
  else
    echo "nosql-conformance: every test that passes in $compare passes here${known:+ or is a known failure}"
  fi
fi
exit $status
