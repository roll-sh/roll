#!/usr/bin/env bash
# Run pika's own acceptance tests (tests/acceptance) against a broker.
# Usage: run.sh <host> <port> [extra pytest args...]
#   host as seen from containers, normally host.docker.internal.
# Source checkout lives in the named volume roll-qtest-pika-src, pip cache in roll-qtest-pip.
set -euo pipefail

HOST=${1:?usage: run.sh <host> <port> [pytest args...]}
PORT=${2:?usage: run.sh <host> <port> [pytest args...]}
shift 2
HERE=$(cd "$(dirname "$0")" && pwd)

PIKA_TAG=1.4.4
IMAGE=python:3.13-slim
DEPS="pytest==9.1.1 pytest-timeout==2.4.0 gevent==26.9.0 tornado==6.5.10 twisted==26.4.0"

docker volume create roll-qtest-pika-src >/dev/null
docker volume create roll-qtest-pip >/dev/null

# excluded.txt: "<pytest node id prefix>  # reason"; blank lines and full-line comments ignored.
DESELECT=()
while IFS= read -r line; do
  id=${line%%#*}
  id=$(echo "$id" | xargs)
  [ -n "$id" ] && DESELECT+=("--deselect" "$id")
done < "$HERE/excluded.txt"

docker run --rm -i \
  --add-host=host.docker.internal:host-gateway \
  -v roll-qtest-pika-src:/src \
  -v roll-qtest-pip:/root/.cache/pip \
  -v "$HERE":/suite:ro \
  -e PIKA_TAG="$PIKA_TAG" -e DEPS="$DEPS" \
  "$IMAGE" bash -s -- "$HOST" "$PORT" ${DESELECT[@]+"${DESELECT[@]}"} "$@" <<'EOF'
set -euo pipefail
HOST=$1 PORT=$2; shift 2
SRC=/src/pika-$PIKA_TAG
if [ ! -f "$SRC/.fetched" ]; then
  echo "fetching pika $PIKA_TAG into volume" >&2
  rm -rf "$SRC"
  python - <<PY
import io, tarfile, urllib.request
url = 'https://codeload.github.com/pika/pika/tar.gz/refs/tags/$PIKA_TAG'
tarfile.open(fileobj=io.BytesIO(urllib.request.urlopen(url).read())).extractall('/src', filter='data')
PY
  touch "$SRC/.fetched"
fi
pip install -q --disable-pip-version-check --root-user-action=ignore $DEPS >&2

python /suite/forward.py "$HOST" "$PORT" >&2 &
for _ in $(seq 50); do python -c 'import socket;socket.create_connection(("127.0.0.1",5672),1).close()' 2>/dev/null && break; sleep 0.1; done

cd "$SRC"
set +e
python -m pytest tests/acceptance -p no:cacheprovider -o addopts= -rfE --timeout=120 \
  --junitxml=/tmp/junit.xml "$@" 2>&1 | tee /tmp/pytest.out
set -e
python - <<'PY'
import re, xml.etree.ElementTree as ET
r = ET.parse('/tmp/junit.xml').getroot()
s = r if r.tag == 'testsuite' else r.find('testsuite')
t, f, e, k = (int(s.get(a, 0)) for a in ('tests', 'failures', 'errors', 'skipped'))
m = re.findall(r'(\d+) deselected', open('/tmp/pytest.out').read())
print('SUITE pika passed=%d failed=%d skipped=%d excluded=%d' % (t - f - e - k, f + e, k, int(m[-1]) if m else 0))
raise SystemExit(1 if f + e else 0)
PY
EOF
