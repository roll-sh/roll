#!/usr/bin/env bash
# Self-signed certificates for the local fleet (deploy/cloud/local): a throwaway CA and one
# certificate covering every edge hostname of a domain, as Let's Encrypt's wildcards do in the cloud
# (docs/cloud/README.md, "The edge").
#
#   gen-certs.sh <dir> [domain] [site domain] [region]
#
# Writes <dir>/ca.crt (what clients trust) and <dir>/edge/edge.{crt,key} (ROLL_EDGE_CERT_DIR on
# every node; the CA's key stays out of that directory, which serves every .crt/.key pair in it).
# Defaults: roll.localhost, rollsite.localhost, us-east-1. `*.localhost` resolves to loopback
# (RFC 6761), so clients on this machine reach the load balancer's published ports by name, and
# Kafka clients can follow the broker hostnames the edge advertises.
# Keeps what's there when it already covers the names.
set -euo pipefail

dir=${1:?usage: gen-certs.sh <dir> [domain] [site domain] [region]}
domain=${2:-roll.localhost}
site=${3:-rollsite.localhost}
region=${4:-us-east-1}

# The edge's two certificate sets (roll_edge::Hostnames::cert_sets), in one certificate.
names=("$domain" "api.$domain" "git.$domain" "tasks.$region.$domain")
for p in pg redis kafka nosql mq; do names+=("*.$p.$region.$domain"); done
names+=("*.$site" "s3.$region.$site" "*.s3.$region.$site")
san=$(printf 'DNS:%s,' "${names[@]}")
san=${san%,}

mkdir -p "$dir/edge"
stamp="$dir/edge/names"
if [[ -f $dir/ca.crt && -f $dir/edge/edge.crt && -f $dir/edge/edge.key && "$(cat "$stamp" 2>/dev/null)" == "$san" ]]; then
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
  -subj "/CN=roll local fleet CA" -keyout "$tmp/ca.key" -out "$tmp/ca.crt" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj "/CN=$domain" \
  -keyout "$tmp/edge.key" -out "$tmp/edge.csr" 2>/dev/null
printf 'subjectAltName=%s\nbasicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' "$san" >"$tmp/ext"
openssl x509 -req -in "$tmp/edge.csr" -CA "$tmp/ca.crt" -CAkey "$tmp/ca.key" -CAcreateserial -days 30 \
  -extfile "$tmp/ext" -out "$tmp/leaf.crt" 2>/dev/null
cat "$tmp/leaf.crt" "$tmp/ca.crt" >"$dir/edge/edge.crt"
# The nodes run as uid 10001 and read it from a read-only mount: test material only.
install -m 0644 "$tmp/edge.key" "$dir/edge/edge.key"
install -m 0644 "$tmp/ca.crt" "$dir/ca.crt"
printf '%s' "$san" >"$stamp"
echo "certificates for $domain and $site ($region) in $dir"
