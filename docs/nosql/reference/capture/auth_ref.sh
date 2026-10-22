#!/usr/bin/env bash
# auth_ref.sh <scylla|cassandra>: start a dedicated reference with the suites' settings
# (PasswordAuthenticator, UDFs, MVs, ...), run the feature-enabled catalog capture, stop it.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
d=$1; c=nosql-refs-$d
cleanup() { docker rm -f "$c" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM
while (( $(awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo) < 6 )); do sleep 10; done
docker network inspect nosql-conformance >/dev/null 2>&1 || docker network create nosql-conformance >/dev/null
if [[ $d == scylla ]]; then
  salted='\$6\$x7IFjiX5VCpvNiFk\$2IfjTvSyGL7zerpV.wbY7mJjaRCrJ/68dtT3UpT.sSmNYz1bPjtn3mH.kJKFvaZ2T4SbVeBijjmwGjcb83LlV/'
  docker run -d --rm --name $c --network nosql-conformance --memory 3g scylladb/scylla:2026.3.3 \
    --smp 1 --memory 1G --overprovisioned 1 --developer-mode 1 --api-address 0.0.0.0 --max-networking-io-control-blocks=1000 \
    --experimental-features=udf --experimental-features=views-with-tablets --enable-tablets=true \
    --enable-user-defined-functions 1 --rf-rack-valid-keyspaces=1 \
    --authenticator PasswordAuthenticator --authorizer CassandraAuthorizer \
    --auth-superuser-name=cassandra "--auth-superuser-salted-password=$salted" \
    --permissions-update-interval-in-ms 100 --permissions-validity-in-ms 5 >/dev/null
else
  setup='y=/etc/cassandra/cassandra.yaml
    sed -i -E "s/^authenticator:.*/authenticator: PasswordAuthenticator/;
      s/^authorizer:.*/authorizer: CassandraAuthorizer/;
      s/^user_defined_functions_enabled:.*/user_defined_functions_enabled: true/;
      s/^materialized_views_enabled:.*/materialized_views_enabled: true/;
      s/^sasi_indexes_enabled:.*/sasi_indexes_enabled: true/;
      s/^permissions_validity:.*/permissions_validity: 100ms/" $y
    echo "permissions_update_interval: 100ms" >> $y
    echo "dynamic_data_masking_enabled: true" >> $y
    exec docker-entrypoint.sh cassandra -f'
  docker run -d --rm --name $c --network nosql-conformance --memory 2560m -e MAX_HEAP_SIZE=640M -e HEAP_NEWSIZE=128M \
    --entrypoint sh cassandra:5.0.9 -c "$setup" >/dev/null
fi
ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $c)
img=$(docker images roll-nosql-conformance --format '{{.Repository}}:{{.Tag}}' | grep -v ':dev' | head -1)
r() { docker run --rm --memory 1g --network container:$c -e CAP_HOST=$ip -v "$here:/cap" "$img" python /cap/"$@"; }
for i in $(seq 1 60); do
  docker run --rm --memory 1g --network container:$c -e CAP_HOST=$ip -v "$here:/cap" "$img" python -c 'import os,sys; sys.path.insert(0,"/cap"); from raw import Raw; x=Raw(os.environ["CAP_HOST"], user="cassandra", pw="cassandra"); assert x.auth[0]==0x10' 2>/dev/null && break
  sleep 5
done
for i in $(seq 1 60); do
  docker exec $c sh -c 'command -v cqlsh >/dev/null && cqlsh -u cassandra -p cassandra -e "SELECT key FROM system.local" >/dev/null 2>&1' && break
  sleep 5
done
sleep 5
# Auth errors come from the shared auth-enabled references: ./cap.sh <dialect>-auth errors_auth.py ...
r catalog.py /cap/out/$d-catalog-suite.json cassandra cassandra
