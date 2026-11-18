#!/usr/bin/env bash
# roll secrets' databases outside roll against real servers (docs/secrets.md, "Databases outside
# roll"): Elasticsearch, OpenSearch, Cassandra and ScyllaDB in Docker, each with authentication on
# and TLS from a CA made here, then rolld's ignored tests for them (register, rotate, a dynamic
# login with its group's grants, delete, the admin's own password; for CQL, that tracing and
# ScyllaDB's audit log keep no password roll sent), then the same servers through a gateway that
# allows only them. Containers are named roll-secrets-outside-check-… and removed at the end;
# images are used as they are (pulled if missing, never removed here).
#
#   scripts/secrets-outside-check.sh [es] [os] [cassandra] [scylla]   (all four by default)
#
# Docker needs the docker group (`sg docker -c`); cargo honours CARGO_TARGET_DIR; CHECK_PREFIX
# renames the containers.
set -euo pipefail
cd "$(dirname "$0")/.."

ES_IMAGE=${ES_IMAGE:-docker.elastic.co/elasticsearch/elasticsearch:9.5.5}
OS_IMAGE=${OS_IMAGE:-opensearchproject/opensearch:3.9.0}
CASSANDRA_IMAGE=${CASSANDRA_IMAGE:-cassandra:5.0.9}
SCYLLA_IMAGE=${SCYLLA_IMAGE:-scylladb/scylla:2026.3.3}
WHICH=("${@:-es os cassandra scylla}")
WHICH=(${WHICH[*]})
PREFIX=${CHECK_PREFIX:-roll-secrets-outside-check}
# OpenSearch's demo installer wants a strong password with a symbol, unlike the user name.
PW=Tq7-vW2pLx9_mZ4rKc8s
DIR=$(mktemp -d)

dk() { if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "docker $(printf '%q ' "$@")"; fi; }
cleanup() {
  for n in es os cassandra scylla; do dk rm -f "$PREFIX-$n" >/dev/null 2>&1 || true; done
  rm -rf "$DIR"
}
trap cleanup EXIT
want() { [[ " ${WHICH[*]} " == *" $1 "* ]]; }
port() { dk port "$PREFIX-$1" "$2/tcp" | head -1 | sed 's/.*://'; }
wait_for() { # name, what, command...
  local name=$1 what=$2; shift 2
  for _ in $(seq 1 120); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  echo "$name: $what never came up" >&2
  dk logs --tail 40 "$PREFIX-$name" >&2 || true
  exit 1
}

# A CA and a server certificate for localhost (server and client use, as OpenSearch's transport wants).
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=roll-check-ca -keyout "$DIR/ca.key" -out "$DIR/ca.crt" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj /CN=localhost -keyout "$DIR/server.key" -out "$DIR/server.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth,clientAuth\nkeyUsage=digitalSignature,keyEncipherment\n' >"$DIR/ext"
openssl x509 -req -in "$DIR/server.csr" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" -CAcreateserial -days 2 -extfile "$DIR/ext" -out "$DIR/server.crt" 2>/dev/null
openssl pkcs8 -topk8 -nocrypt -in "$DIR/server.key" -out "$DIR/server.pk8"
openssl pkcs12 -export -in "$DIR/server.crt" -inkey "$DIR/server.key" -name server -passout pass:$PW -out "$DIR/server.p12"
chmod 644 "$DIR"/*
chmod 755 "$DIR" # the servers run as their own users
export ROLL_E2E_OUTSIDE_CA="$DIR/ca.crt"
TESTS=()

if want es; then
  dk run -d --name "$PREFIX-es" -p 127.0.0.1::9200 -m 2g \
    -v "$DIR:/usr/share/elasticsearch/config/certs:ro" \
    -e discovery.type=single-node -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e ELASTIC_PASSWORD=$PW \
    -e xpack.security.enabled=true -e xpack.security.http.ssl.enabled=true \
    -e xpack.security.http.ssl.key=certs/server.key -e xpack.security.http.ssl.certificate=certs/server.crt \
    -e cluster.routing.allocation.disk.threshold_enabled=false \
    "$ES_IMAGE" >/dev/null
fi
if want os; then
  dk run -d --name "$PREFIX-os" -p 127.0.0.1::9200 -m 2g \
    -v "$DIR:/usr/share/opensearch/config/rollcerts:ro" \
    -e discovery.type=single-node -e OPENSEARCH_JAVA_OPTS="-Xms512m -Xmx512m" -e OPENSEARCH_INITIAL_ADMIN_PASSWORD=$PW \
    -e plugins.security.ssl.http.pemcert_filepath=rollcerts/server.crt -e plugins.security.ssl.http.pemkey_filepath=rollcerts/server.pk8 \
    -e plugins.security.ssl.http.pemtrustedcas_filepath=rollcerts/ca.crt \
    -e cluster.routing.allocation.disk.threshold_enabled=false \
    "$OS_IMAGE" >/dev/null
fi
if want cassandra; then
  dk run --rm --entrypoint cat "$CASSANDRA_IMAGE" /etc/cassandra/cassandra.yaml >"$DIR/cassandra.yaml"
  python3 - "$DIR/cassandra.yaml" "$PW" <<'EOF'
import sys
p, pw = sys.argv[1], sys.argv[2]
s = open(p).read()
for old, new in [
    ("authenticator: AllowAllAuthenticator", "authenticator: PasswordAuthenticator"),
    ("authorizer: AllowAllAuthorizer", "authorizer: CassandraAuthorizer"),
]:
    assert s.count(old) == 1, old
    s = s.replace(old, new)
# Only the client section's keystore (the server section has the same lines).
head, client = s.split("client_encryption_options:\n", 1)
old = "  # Enable client-to-server encryption\n  enabled: false"
assert client.startswith(old)
client = client.replace(old, "  enabled: true\n  optional: false\n  store_type: PKCS12\n  keystore_password: " + pw, 1)
old = "  keystore: conf/.keystore\n"
assert old in client
client = client.replace(old, "  keystore: /certs/server.p12\n", 1)
open(p, "w").write(head + "client_encryption_options:\n" + client)
EOF
  chmod 644 "$DIR/cassandra.yaml"
  # Copied in, not mounted: the image's entrypoint rewrites it.
  dk run -d --name "$PREFIX-cassandra" -p 127.0.0.1::9042 -m 2g -e MAX_HEAP_SIZE=1G -e HEAP_NEWSIZE=256M \
    -v "$DIR:/certs:ro" --entrypoint sh "$CASSANDRA_IMAGE" -c 'cp /certs/cassandra.yaml /etc/cassandra/cassandra.yaml && exec docker-entrypoint.sh cassandra -f' >/dev/null
fi
if want scylla; then
  cat >"$DIR/scylla-tls.yaml" <<EOF
client_encryption_options:
  enabled: true
  certificate: /certs/server.crt
  keyfile: /certs/server.key
EOF
  dk run --rm --entrypoint cat "$SCYLLA_IMAGE" /etc/scylla/scylla.yaml >"$DIR/scylla.yaml"
  cat "$DIR/scylla-tls.yaml" >>"$DIR/scylla.yaml"
  # ScyllaDB makes no default superuser with a known password: name one (crypt SHA-512).
  printf 'auth_superuser_name: cassandra\nauth_superuser_salted_password: "%s"\n' "$(openssl passwd -6 cassandra)" >>"$DIR/scylla.yaml"
  chmod 644 "$DIR/scylla.yaml"
  # No container cap: seastar sizes itself from the cgroup and wants more than --memory.
  dk run -d --name "$PREFIX-scylla" -p 127.0.0.1::9042 -v "$DIR:/certs:ro" -v "$DIR/scylla.yaml:/etc/scylla/scylla.yaml:ro" "$SCYLLA_IMAGE" \
    --smp 1 --memory 1G --overprovisioned 1 --developer-mode 1 \
    --authenticator PasswordAuthenticator --authorizer CassandraAuthorizer >/dev/null
fi

if want es; then
  P=$(port es 9200)
  wait_for es "the HTTPS API" curl -sf --cacert "$DIR/ca.crt" -u "elastic:$PW" "https://localhost:$P/_cluster/health?wait_for_status=yellow&timeout=1s"
  export ROLL_E2E_ES_URL="elasticsearch://elastic:$PW@localhost:$P"
  TESTS+=(a_real_elasticsearch)
fi
if want os; then
  P=$(port os 9200)
  wait_for os "the HTTPS API" curl -sf --cacert "$DIR/ca.crt" -u "admin:$PW" "https://localhost:$P/_plugins/_security/authinfo"
  export ROLL_E2E_OS_URL="opensearch://admin:$PW@localhost:$P"
  TESTS+=(a_real_opensearch)
fi
cql_up() { # name: the superuser logs in over TLS (ScyllaDB listens on the container's address only)
  dk exec "$PREFIX-$1" sh -c "printf '[connection]\nfactory = cqlshlib.ssl.ssl_transport_factory\n[ssl]\nvalidate = false\n' >/tmp/cqlshrc; cqlsh --cqlshrc /tmp/cqlshrc --ssl -u cassandra -p cassandra -e 'SELECT now() FROM system.local' \$(hostname -i) 9042"
}
# Every request traced (system_traces.sessions keeps the statement whole on both): the tests check
# what a statement roll sent holds.
trace_all() { dk exec "$PREFIX-$1" nodetool settraceprobability 1; }
if want cassandra; then
  wait_for cassandra "CQL with TLS and the default superuser" cql_up cassandra
  trace_all cassandra
  export ROLL_E2E_CASSANDRA_URL="cassandra://cassandra:cassandra@localhost:$(port cassandra 9042)"
  TESTS+=(a_real_cassandra)
fi
if want scylla; then
  wait_for scylla "CQL with TLS and the default superuser" cql_up scylla
  trace_all scylla
  export ROLL_E2E_SCYLLA_URL="cassandra://cassandra:cassandra@localhost:$(port scylla 9042)"
  TESTS+=(a_real_scylla)
fi

# Last, the same servers through a gateway (rolld's secrets::gateway; it uses the role `reader` the
# tests above made).
TESTS+=(real_outside_servers_through_the_gateway)
for t in "${TESTS[@]}"; do
  echo "== $t"
  cargo test -q -p rolld --bin rolld "$t" -- --ignored --nocapture --test-threads 1
done
