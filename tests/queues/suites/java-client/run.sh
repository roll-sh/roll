#!/usr/bin/env bash
# Run the RabbitMQ Java client's own broker-facing test classes (functional, server and the
# ClientTests that open connections) against a broker.
# Usage: run.sh <host> <port> [TestClass ...]
#   host as seen from containers, normally host.docker.internal; optional class names = subset.
# Source checkout lives in the named volume roll-qtest-java-client-src, Maven repo in roll-qtest-m2.
set -euo pipefail

HOST=${1:?usage: run.sh <host> <port> [TestClass ...]}
PORT=${2:?usage: run.sh <host> <port> [TestClass ...]}
shift 2
HERE=$(cd "$(dirname "$0")" && pwd)

CLIENT_TAG=v5.37.0
CODEGEN_TAG=v4.3.6 # rabbitmq-server tag providing deps/rabbitmq_codegen (AMQP 0-9-1 spec + generator)
IMAGE=roll-qtest-java-client-suite

# Upstream suites (src/test/java/.../FunctionalTestSuite, ServerTestSuite, ClientTestSuite).
FUNCTIONAL="ConnectionOpen Heartbeat Tables DoubleDeletion Routing BindingLifecycle Recover Reject
Transactions RequeueOnConnectionClose RequeueOnChannelClose DurableOnTransient NoRequeueOnCancel
Bug20004Test ExchangeDeleteIfUnused QosTests AlternateExchange ExchangeExchangeBindings
ExchangeExchangeBindingsAutoDelete ExchangeDeclare FrameMax QueueLifecycle QueueLease
QueueExclusivity QueueSizeLimit InvalidAcks InvalidAcksTx DefaultExchange UnbindAutoDeleteExchange
Confirm ConsumerNotifications UnexpectedFrames PerQueueTTL PerMessageTTL PerQueueVsPerMessageTTL
DeadLetterExchange SaslMechanisms UserIDHeader InternalExchange CcRoutes WorkPoolTests
HeadersExchangeValidation ConsumerPriorities Policies ConnectionRecovery ExceptionHandling
PerConsumerPrefetch DirectReplyTo ConsumerCount BasicGet Nack ExceptionMessages Metrics
MicrometerObservationCollectorMetrics TopologyRecoveryFiltering TopologyRecoveryRetry"
SERVER="Permissions DurableBindingLifecycle DeadLetterExchangeDurable EffectVisibilityCrossNodeTest
ExclusiveQueueDurability AlternateExchangeEquivalence MemoryAlarms MessageRecovery Firehose
PersistenceGuarantees Shutdown BlockedConnection ChannelLimitNegotiation LoopbackUsers
XDeathHeaderGrowth PriorityQueues TopicPermissions"
# ClientTestSuite members that open broker connections (the rest are pure unit tests).
CLIENT="AMQBuilderApiTest AMQConnectionRefreshCredentialsTest AMQConnectionTest BlockedConnectionTest
ChannelAsyncCompletableFutureTest ChannelNumberAllocationTests CloseInMainLoop ConnectionFactoryTest
DnsRecordIpAddressResolverTests InboundFrameMax IoDeadlockOnConnectionClosing JavaNioTest
LambdaCallbackTest MetricsCollectorTest MultiThreadedChannel NettyTest NioDeadlockOnConnectionClosing
NoAutoRecoveryWhenTcpWindowIsFullTest ProtocolVersionMismatch PublishWithByteBufferTest
QueueingConsumerTests RecoveryAwareAMQConnectionFactoryTest RefreshCredentialsTest RpcTest
RpcTopologyRecordingTest SharedThreadPoolTest StrictExceptionHandlerTest"

if [ $# -gt 0 ]; then CLASSES="$*"; else CLASSES="$FUNCTIONAL $SERVER $CLIENT"; fi

# excluded.txt: "Class" or "Class#method", then "# reason". Whole classes are dropped from the
# run, methods are passed to failsafe as "!Class#method".
EXCLUDED=0
SKIP_CLASSES=" "
PATTERNS=()
while IFS= read -r line; do
  id=$(echo "${line%%#\ *}" | xargs)
  [ -z "$id" ] && continue
  case "$id" in \#*) continue ;; esac
  EXCLUDED=$((EXCLUDED + 1))
  case "$id" in
    *'#'*) PATTERNS+=("!$id") ;;
    *) SKIP_CLASSES="$SKIP_CLASSES$id " ;;
  esac
done < "$HERE/excluded.txt"
for c in $CLASSES; do
  case "$SKIP_CLASSES" in *" $c "*) ;; *) PATTERNS=("$c" ${PATTERNS[@]+"${PATTERNS[@]}"}) ;; esac
done
IT_TEST=$(IFS=,; echo "${PATTERNS[*]}")

docker build -q -t "$IMAGE" - >/dev/null <<'DOCKERFILE'
FROM maven:3-eclipse-temurin-21
# python3 runs the AMQP code generator (codegen.py) during generate-sources.
RUN apt-get update && apt-get install -y --no-install-recommends python3 && rm -rf /var/lib/apt/lists/*
ENV PYTHON_BIN=python3
DOCKERFILE
docker volume create roll-qtest-java-client-src >/dev/null
docker volume create roll-qtest-m2 >/dev/null

docker run --rm -i \
  --add-host=host.docker.internal:host-gateway \
  -v roll-qtest-java-client-src:/src \
  -v roll-qtest-m2:/root/.m2 \
  -v "$HERE":/suite:ro \
  -e CLIENT_TAG="$CLIENT_TAG" -e CODEGEN_TAG="$CODEGEN_TAG" -e IT_TEST="$IT_TEST" -e EXCLUDED="$EXCLUDED" \
  "$IMAGE" bash -s -- "$HOST" "$PORT" <<'EOF'
set -euo pipefail
HOST=$1 PORT=$2
SRC=/src/rabbitmq-java-client-${CLIENT_TAG#v}
if [ ! -f "$SRC/.fetched" ]; then
  echo "fetching rabbitmq-java-client $CLIENT_TAG and rabbitmq_codegen ($CODEGEN_TAG) into volume" >&2
  rm -rf "$SRC" /src/codegen-tmp
  curl -fsSL "https://codeload.github.com/rabbitmq/rabbitmq-java-client/tar.gz/refs/tags/$CLIENT_TAG" | tar -xz -C /src
  git clone -q -n --depth=1 --filter=tree:0 --branch "$CODEGEN_TAG" https://github.com/rabbitmq/rabbitmq-server.git /src/codegen-tmp
  git -C /src/codegen-tmp sparse-checkout set --no-cone deps/rabbitmq_codegen
  git -C /src/codegen-tmp -c advice.detachedHead=false checkout -q
  mkdir -p "$SRC/deps" && cp -R /src/codegen-tmp/deps/rabbitmq_codegen "$SRC/deps/"
  rm -rf /src/codegen-tmp
  touch "$SRC/.fetched"
fi

java /suite/Forward.java "$HOST" "$PORT" >&2 &
for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/5672) 2>/dev/null && break; sleep 0.2; done

cd "$SRC"
rm -rf target/failsafe-reports
set +e
mvn -B -ntp verify \
  -Ddeps.dir="$SRC/deps" \
  -Drabbitmqctl.bin=/suite/rabbitmqctl-stub.sh \
  -Dit.test="$IT_TEST" \
  -Dfailsafe.failIfNoSpecifiedTests=false \
  -Dmaven.test.failure.ignore=true \
  -Djunit.jupiter.execution.timeout.default=3m \
  -Dmaven.source.skip=true -Dmaven.javadoc.skip=true -Dspotless.check.skip=true -Dgpg.skip=true \
  >/tmp/mvn.log 2>&1
rc=$?
set -e
cp /tmp/mvn.log /src/last-run-mvn.log # full log + target/failsafe-reports stay in the volume for debugging
grep -E '^\[(INFO|WARNING|ERROR)\] (Tests run|Running|BUILD|ERROR|FAIL)|^\[ERROR\]' /tmp/mvn.log | grep -v '^\[INFO\] Running' || true
if ! ls target/failsafe-reports/TEST-*.xml >/dev/null 2>&1; then
  tail -80 /tmp/mvn.log
  echo "SUITE java-client passed=0 failed=1 skipped=0 excluded=$EXCLUDED (build failed, mvn exit $rc)"
  exit 1
fi
python3 - <<'PY'
import glob, os, xml.etree.ElementTree as ET
p = f = k = 0
failed = []
for path in sorted(glob.glob('target/failsafe-reports/TEST-*.xml')):
    for tc in ET.parse(path).getroot().iter('testcase'):
        name = '%s#%s' % (tc.get('classname', '').rsplit('.', 1)[-1], tc.get('name'))
        bad = tc.find('failure') if tc.find('failure') is not None else tc.find('error')
        if bad is not None:
            f += 1
            failed.append('FAILED %s: %s' % (name, (bad.get('message') or bad.get('type') or '').splitlines()[0][:200] if (bad.get('message') or bad.get('type')) else ''))
        elif tc.find('skipped') is not None:
            k += 1
        else:
            p += 1
print('\n'.join(failed))
print('SUITE java-client passed=%d failed=%d skipped=%d excluded=%s' % (p, f, k, os.environ['EXCLUDED']))
raise SystemExit(1 if f else 0)
PY
EOF
