#!/usr/bin/env bash
# roll/nosql entrypoint: reads the configuration the reference images take (cassandra:5.0's
# environment and cassandra.yaml, scylladb/scylla's command line and scylla.yaml) and starts
# rolld in single mode with the equivalent settings. docs/nosql/local-dev.md has the table.
#
# - Anything that is not a flag runs as a command: `docker run roll/nosql cqlsh <host>`.
# - `cassandra [-f] [-R]` (the Cassandra image's command) selects the cassandra dialect.
# - ROLL_NOSQL_DIALECT=scylla|cassandra picks the dialect. Otherwise it is scylla when a
#   ScyllaDB flag (or SCYLLA_* variable, or /etc/scylla/scylla.yaml) is given, cassandra when a
#   CASSANDRA_* variable, the `cassandra` command or /etc/cassandra/cassandra.yaml is, and
#   scylla when nothing says.
# - Memory: ScyllaDB's --memory, Cassandra's MAX_HEAP_SIZE (or else the container's memory
#   limit) size roll's memtable budget and block cache, as the references size theirs; the
#   configuration files' memtable_heap_space and file_cache_size override them, and
#   ROLL_NOSQL_MEMTABLE_BUDGET / ROLL_NOSQL_CACHE_MEMORY override everything.
# - TLS: the configuration file's client_encryption_options (PEM key material; Cassandra's JKS
#   keystores must be converted, the error says how), native_transport_port_ssl and ScyllaDB's
#   native_shard_aware_transport_port_ssl, or ROLL_NOSQL_TLS_* directly. Client certificates
#   (require_client_auth, the truststore) feed the certificate authenticators.
# - Options that only tune a JVM or Seastar (HEAP_NEWSIZE, --smp's CPUs, --overprovisioned,
#   --developer-mode, seeds, listen addresses) are accepted and have no effect.
set -euo pipefail

log() { echo "roll/nosql: $*"; }

if [[ $# -gt 0 && $1 != -* && $1 != cassandra ]]; then
  exec "$@"
fi

hint_cassandra=0 hint_scylla=0
if [[ ${1-} == cassandra ]]; then
  hint_cassandra=1; shift
  while [[ $# -gt 0 && ( $1 == -f || $1 == -R ) ]]; do shift; done
fi
if compgen -e | grep -q '^CASSANDRA_'; then hint_cassandra=1; fi
if compgen -e | grep -q '^SCYLLA_'; then hint_scylla=1; fi
cassandra_yaml=${CASSANDRA_CONF:-/etc/cassandra}/cassandra.yaml
scylla_yaml=${SCYLLA_CONF:-/etc/scylla}/scylla.yaml
[[ -f $cassandra_yaml ]] && hint_cassandra=1
[[ -f $scylla_yaml ]] && hint_scylla=1

# The cassandra.yaml / scylla.yaml options roll models (nosql-control's settings keys).
SETTINGS=" batch_size_warn_threshold batch_size_warn_threshold_in_kb batch_size_fail_threshold
  batch_size_fail_threshold_in_kb unlogged_batch_across_partitions_warn_threshold tombstone_warn_threshold
  tombstone_failure_threshold materialized_views_enabled enable_materialized_views sasi_indexes_enabled
  enable_sasi_indexes user_defined_functions_enabled enable_user_defined_functions compact_storage_enabled
  enable_create_table_with_compact_storage drop_compact_storage_enabled max_partition_key_restrictions_per_query
  max_clustering_key_restrictions_per_query minimum_replication_factor_warn_threshold tablets_by_default
  tablets_mode_for_new_keyspaces enable_tablets allow_filtering_enabled authenticator authorizer network_authorizer
  permissions_validity permissions_validity_in_ms roles_validity roles_validity_in_ms credentials_validity
  credentials_validity_in_ms auth_bcrypt_log2_rounds role_password_update_min_interval
  role_password_update_min_interval_in_ms "
is_setting() { [[ $SETTINGS == *[[:space:]]$1[[:space:]]* ]]; }

# --- ScyllaDB's command line (scylla's docker-entrypoint.py options, then scylla's own) -------
declare -A flag=()
experimental=() ignored=() settings=()
while [[ $# -gt 0 ]]; do
  a=$1; shift
  [[ $a == --* ]] || { log "ignoring argument $a"; continue; }
  hint_scylla=1
  name=${a#--}
  if [[ $name == *=* ]]; then
    value=${name#*=}; name=${name%%=*}
  elif [[ $# -gt 0 && $1 != --* ]]; then
    value=$1; shift
  else
    value=1
  fi
  name=${name//_/-}
  case "$name" in
    experimental-features) IFS=', ' read -ra fs <<<"$value"; experimental+=("${fs[@]}") ;;
    smp|memory|reserve-memory|overprovisioned|developer-mode|cpuset|io-setup|disable-version-check| \
    blocked-reactor-notify-ms|lock-memory|skip-wait-for-gossip-to-settle|seeds|seed-provider-parameters| \
    listen-address|rpc-address|replace-node-first-boot|replace-address-first-boot|experimental| \
    max-io-requests|num-io-queues|poll-mode|default-log-level|logger-log-level|log-to-stdout|log-to-syslog| \
    abort-on-lsa-bad-alloc|abort-on-seastar-bad-alloc|unsafe-bypass-fsync|kernel-page-cache)
      flag[$name]=$value ;;
    dc|rack|endpoint-snitch|cluster-name|authenticator|authorizer|api-address|api-port|alternator-address| \
    alternator-port|alternator-https-port|alternator-write-isolation|broadcast-address|broadcast-rpc-address| \
    native-transport-port|native-shard-aware-transport-port|native-transport-port-ssl|native-shard-aware-transport-port-ssl| \
    prometheus-address|prometheus-port|num-tokens| \
    enable-user-defined-functions|user-defined-functions-enabled|alternator-ttl-period-in-seconds)
      flag[$name]=$value ;;
    *)
      # Any scylla.yaml option roll models (`--tombstone-warn-threshold 500`, …).
      if is_setting "${name//-/_}"; then settings+=("${name//-/_}=$value"); else ignored+=("--$name"); fi ;;
  esac
done
[[ ${#ignored[@]} -gt 0 ]] && log "ignoring options roll has no equivalent for: ${ignored[*]}"

if [[ -n ${ROLL_NOSQL_DIALECT:-} ]]; then
  dialect=$ROLL_NOSQL_DIALECT
elif [[ $hint_scylla == 1 && ${#flag[@]} -gt 0 ]]; then
  dialect=scylla
elif [[ $hint_cassandra == 1 ]]; then
  dialect=cassandra
else
  dialect=scylla
fi
case "$dialect" in scylla|cassandra) ;; *) log "ROLL_NOSQL_DIALECT must be scylla or cassandra, not $dialect" >&2; exit 2 ;; esac

# --- Configuration files (top-level scalars; `authenticator:` may be a class_name block) ----
yaml_get() { # file key
  [[ -f $1 ]] || return 0
  awk -v k="$2" '
    function clean(v) { sub(/[ \t]+#.*$/, "", v); gsub(/^[ \t"'\'']+|[ \t"'\'']+$/, "", v); return v }
    $0 ~ "^" k "[ \t]*:" { v = $0; sub("^" k "[ \t]*:", "", v); v = clean(v); if (v != "") { print v; exit } ; block = 1; next }
    block && /^[ \t]+class_name[ \t]*:/ { v = $0; sub(/^[ \t]+class_name[ \t]*:/, "", v); print clean(v); exit }
    block && /^[^ \t#]/ { exit }' "$1" || true
}
conf=$([[ $dialect == cassandra ]] && echo "$cassandra_yaml" || echo "$scylla_yaml")
for key in cluster_name num_tokens endpoint_snitch native_transport_port alternator_ttl_period_in_seconds $SETTINGS; do
  v=$(yaml_get "$conf" "$key")
  [[ -n $v ]] || continue
  case "$key" in
    cluster_name) yaml_cluster_name=$v ;;
    num_tokens) yaml_num_tokens=$v ;;
    endpoint_snitch) yaml_snitch=$v ;;
    native_transport_port) yaml_cql_port=$v ;;
    alternator_ttl_period_in_seconds) flag[alternator-ttl-period-in-seconds]=${flag[alternator-ttl-period-in-seconds]:-$v} ;;
    *) settings=("$key=$v" "${settings[@]}") ;;
  esac
done
# A nested option (`client_encryption_options.keystore`): a scalar, or a block scalar's text.
yaml_at() { # file dotted.path
  [[ -f $1 ]] || return 0
  awk -v want="$2" '
    function clean(v) { sub(/[ \t]+#.*$/, "", v); gsub(/^[ \t"'\'']+|[ \t"'\'']+$/, "", v); return v }
    block { if ($0 ~ /^[ \t]*$/) { print ""; next }; match($0, /^ */); if (RLENGTH <= bindent) exit; if (!bi) bi = RLENGTH; print substr($0, bi + 1); next }
    /^[ \t]*(#|$)/ || /^[ \t]*- / { next }
    { match($0, /^ */); ind = RLENGTH; line = substr($0, ind + 1); c = index(line, ":"); if (!c) next
      k = substr(line, 1, c - 1); gsub(/[ \t]+$/, "", k)
      while (n > 0 && inds[n] >= ind) n--
      n++; inds[n] = ind; keys[n] = k; p = keys[1]; for (i = 2; i <= n; i++) p = p "." keys[i]
      if (p == want) { v = clean(substr(line, c + 1)); if (v ~ /^[|>][-+]?$/) { block = 1; bindent = ind; next }; if (v != "") print v; exit } }' "$1" || true
}
# ScyllaDB's auth_certificate_role_queries list as JSON: [{"source": .., "query": ..}].
yaml_role_queries() { # file
  [[ -f $1 ]] || return 0
  awk '
    function clean(v) { sub(/[ \t]+#.*$/, "", v); gsub(/^[ \t"'\'']+|[ \t"'\'']+$/, "", v); return v }
    # Character by character: gsub replacement escaping differs between awks (mawk, gawk).
    function esc(v,   o, i, ch) { o = ""; for (i = 1; i <= length(v); i++) { ch = substr(v, i, 1); if (ch == "\\" || ch == "\"") o = o "\\"; o = o ch }; return o }
    function flush() { if (src != "" || q != "") out = out (out == "" ? "" : ",") "{\"source\":\"" esc(src) "\",\"query\":\"" esc(q) "\"}"; src = ""; q = "" }
    /^auth_certificate_role_queries[ \t]*:/ { on = 1; next }
    on && /^[^ \t#-]/ { exit }
    on { line = $0; sub(/^[ \t]*/, "", line); if (line ~ /^#/ || line == "") next
      if (line ~ /^-/) { flush(); line = substr(line, 2); sub(/^[ \t]*/, "", line) }
      c = index(line, ":"); if (!c) next
      k = substr(line, 1, c - 1); gsub(/[ \t]+$/, "", k); v = clean(substr(line, c + 1))
      if (k == "source") src = v; else if (k == "query") q = v }
    END { flush(); if (out != "") print "[" out "]" }' "$1" || true
}
rackdc_get() { # file key
  [[ -f $1 ]] || return 0
  sed -n "s/^[ \t]*$2[ \t]*=[ \t]*//p" "$1" | tail -1 | tr -d '\r' | sed 's/[ \t]*$//'
}

# --- Identity --------------------------------------------------------------------------------
if [[ $dialect == cassandra ]]; then
  cluster_name=${CASSANDRA_CLUSTER_NAME:-${yaml_cluster_name:-Test Cluster}}
  snitch=${CASSANDRA_ENDPOINT_SNITCH:-${yaml_snitch:-SimpleSnitch}}
  num_tokens=${CASSANDRA_NUM_TOKENS:-${yaml_num_tokens:-}}
  rackdc=${CASSANDRA_CONF:-/etc/cassandra}/cassandra-rackdc.properties
  # Cassandra reads dc/rack from cassandra-rackdc.properties (dc1/rack1 as shipped) only
  # with GossipingPropertyFileSnitch; SimpleSnitch is datacenter1/rack1.
  if [[ ${snitch##*.} == GossipingPropertyFileSnitch ]]; then
    dc=${CASSANDRA_DC:-$(rackdc_get "$rackdc" dc)}; dc=${dc:-dc1}
    rack=${CASSANDRA_RACK:-$(rackdc_get "$rackdc" rack)}; rack=${rack:-rack1}
  else
    dc=datacenter1 rack=rack1
  fi
  public=${CASSANDRA_BROADCAST_RPC_ADDRESS:-${CASSANDRA_BROADCAST_ADDRESS:-}}
else
  cluster_name=${flag[cluster-name]-${yaml_cluster_name:-}}
  snitch=${flag[endpoint-snitch]:-${yaml_snitch:-SimpleSnitch}}
  num_tokens=${flag[num-tokens]:-${yaml_num_tokens:-}}
  rackdc=${SCYLLA_CONF:-/etc/scylla}/cassandra-rackdc.properties
  if [[ -n ${flag[dc]:-} || -n ${flag[rack]:-} ]]; then
    if [[ ${snitch##*.} != GossipingPropertyFileSnitch && -n ${flag[endpoint-snitch]:-} ]]; then
      log "Cannot use dc and rack parameters together with endpoint snitch '$snitch'." >&2; exit 1
    fi
    snitch=GossipingPropertyFileSnitch
    dc=${flag[dc]:-datacenter1} rack=${flag[rack]:-rack1}
  elif [[ ${snitch##*.} == GossipingPropertyFileSnitch ]]; then
    dc=$(rackdc_get "$rackdc" dc); dc=${dc:-datacenter1}
    rack=$(rackdc_get "$rackdc" rack); rack=${rack:-rack1}
  else
    dc=datacenter1 rack=rack1
  fi
  public=${flag[broadcast-rpc-address]:-${flag[broadcast-address]:-}}
  [[ -n ${flag[smp]:-} ]] && settings+=("shards=${flag[smp]}")
fi
case "${snitch##*.}" in
  SimpleSnitch|GossipingPropertyFileSnitch) ;;
  *) log "endpoint snitch $snitch is served as datacenter $dc, rack $rack" ;;
esac
# The address system.local and Alternator's /localnodes report: the container's, as the images.
[[ $public == auto ]] && public=
if [[ -z $public ]]; then
  public=$(hostname -i 2>/dev/null | awk '{print $1}')
  public=${public:-127.0.0.1}
fi
[[ -n $num_tokens ]] && settings+=("num_tokens=$num_tokens")

# Authentication, authorization and UDFs.
[[ -n ${flag[authenticator]:-} ]] && settings+=("authenticator=${flag[authenticator]}")
[[ -n ${flag[authorizer]:-} ]] && settings+=("authorizer=${flag[authorizer]}")
udf=${flag[enable-user-defined-functions]:-${flag[user-defined-functions-enabled]:-}}
[[ -n $udf ]] && settings+=("user_defined_functions_enabled=$udf")
if [[ ${#experimental[@]} -gt 0 ]]; then
  log "experimental features ${experimental[*]}: roll has no experimental gates (UDFs follow enable_user_defined_functions)"
fi

# --- Memory --------------------------------------------------------------------------------------
# Sizes as Seastar's --memory, the JVM's heap and the configuration files write them (2G, 512M,
# 1024k, 512MiB, bytes); `*_in_mb` values are MiB.
to_bytes() {
  local v=${1,,}
  v=${v// /}; v=${v%b}; v=${v%i}
  local n=${v%[kmgt]} u=${v##*[0-9]}
  [[ $n =~ ^[0-9]+$ ]] || return 1
  case $u in
    k) echo $((n << 10)) ;; m) echo $((n << 20)) ;; g) echo $((n << 30)) ;; t) echo $((n << 40)) ;; "") echo "$n" ;; *) return 1 ;;
  esac
}
# The memory the reference would use: ScyllaDB's --memory, Cassandra's heap, else the
# container's limit (cgroup v2, then v1). Memtables get a quarter (Cassandra's
# memtable_heap_space default is heap/4), the block cache a quarter (Cassandra's file_cache_size:
# heap/4, at most 512MiB), and the rest is left for requests, compaction and indexes.
mem= mem_from=
if [[ $dialect == scylla && -n ${flag[memory]:-} ]]; then
  mem=$(to_bytes "${flag[memory]}") && mem_from="--memory ${flag[memory]}" || { log "ignoring --memory ${flag[memory]}: not a size"; mem=; }
elif [[ $dialect == cassandra && -n ${MAX_HEAP_SIZE:-} ]]; then
  mem=$(to_bytes "$MAX_HEAP_SIZE") && mem_from="MAX_HEAP_SIZE=$MAX_HEAP_SIZE" || { log "ignoring MAX_HEAP_SIZE=$MAX_HEAP_SIZE: not a size"; mem=; }
fi
if [[ -z $mem ]]; then
  for f in /sys/fs/cgroup/memory.max /sys/fs/cgroup/memory/memory.limit_in_bytes; do
    v=$(cat "$f" 2>/dev/null || true)
    if [[ $v =~ ^[0-9]+$ && $v -lt $((1 << 40)) ]]; then mem=$v mem_from="the container's memory limit"; break; fi
  done
fi
memtable= cache=
if [[ -n $mem ]]; then
  memtable=$((mem / 4)) cache=$((mem / 4))
  [[ $dialect == cassandra && $cache -gt $((512 << 20)) ]] && cache=$((512 << 20))
fi
v=$(yaml_get "$conf" memtable_heap_space); [[ -n $v ]] && memtable=$(to_bytes "$v" || echo "$memtable")
v=$(yaml_get "$conf" memtable_heap_space_in_mb); [[ $v =~ ^[0-9]+$ ]] && memtable=$((v << 20))
v=$(yaml_get "$conf" file_cache_size); [[ -n $v ]] && cache=$(to_bytes "$v" || echo "$cache")
v=$(yaml_get "$conf" file_cache_size_in_mb); [[ $v =~ ^[0-9]+$ ]] && cache=$((v << 20))
v=$(yaml_get "$conf" concurrent_compactors); [[ $v =~ ^[1-9][0-9]*$ ]] && export ROLL_NOSQL_COMPACTION_CONCURRENCY=${ROLL_NOSQL_COMPACTION_CONCURRENCY:-$v}
v=$(yaml_get "$conf" memtable_flush_writers); [[ $v =~ ^[1-9][0-9]*$ ]] && export ROLL_NOSQL_FLUSH_CONCURRENCY=${ROLL_NOSQL_FLUSH_CONCURRENCY:-$v}
# At least the 1 MiB rolld accepts.
[[ -n $memtable ]] && export ROLL_NOSQL_MEMTABLE_BUDGET=${ROLL_NOSQL_MEMTABLE_BUDGET:-$((memtable > (1 << 20) ? memtable : (1 << 20)))}
[[ -n $cache ]] && export ROLL_NOSQL_CACHE_MEMORY=${ROLL_NOSQL_CACHE_MEMORY:-$cache}
[[ -n $mem_from || -n $memtable || -n $cache ]] && log "memory${mem_from:+ from $mem_from}: memtables ${ROLL_NOSQL_MEMTABLE_BUDGET:-default}, block cache ${ROLL_NOSQL_CACHE_MEMORY:-default}"

# --- Listeners ---------------------------------------------------------------------------------
cql_port=${flag[native-transport-port]:-${yaml_cql_port:-9042}}
aware_port=${flag[native-shard-aware-transport-port]:-19042}
export ROLL_NOSQL_CQL=${ROLL_NOSQL_CQL:-0.0.0.0:$cql_port}
export ROLL_NOSQL_CQL_SHARD_AWARE=${ROLL_NOSQL_CQL_SHARD_AWARE:-0.0.0.0:$aware_port}
export ROLL_NOSQL_ALTERNATOR=${ROLL_NOSQL_ALTERNATOR:-${flag[alternator-address]:-0.0.0.0}:${flag[alternator-port]:-8000}}
export ROLL_NOSQL_REST=${ROLL_NOSQL_REST:-${flag[api-address]:-0.0.0.0}:${flag[api-port]:-10000}}
export ROLL_NOSQL_METRICS=${ROLL_NOSQL_METRICS:-${flag[prometheus-address]:-0.0.0.0}:${flag[prometheus-port]:-9180}}
if [[ -n ${flag[alternator-https-port]:-} ]]; then
  if [[ -n ${ROLL_NOSQL_TLS_CERT:-} ]]; then
    export ROLL_NOSQL_ALTERNATOR_HTTPS=${ROLL_NOSQL_ALTERNATOR_HTTPS:-${flag[alternator-address]:-0.0.0.0}:${flag[alternator-https-port]}}
  else
    log "--alternator-https-port needs ROLL_NOSQL_TLS_CERT and ROLL_NOSQL_TLS_KEY; Alternator stays on HTTP only"
  fi
fi
[[ -n ${flag[alternator-write-isolation]:-} ]] && export ROLL_NOSQL_ALTERNATOR_WRITE_ISOLATION=${flag[alternator-write-isolation]}
[[ -n ${flag[alternator-ttl-period-in-seconds]:-} ]] && export ROLL_NOSQL_ALTERNATOR_TTL_PERIOD=${flag[alternator-ttl-period-in-seconds]}
export ROLL_API=${ROLL_API:-0.0.0.0:7070}
export ROLL_PUBLIC_HOST=${ROLL_PUBLIC_HOST:-$public}
export ROLL_HOME=${ROLL_HOME:-/var/lib/roll}
export ROLL_NOSQL_SINGLE=${ROLL_NOSQL_SINGLE:-default}
export ROLL_NOSQL_SINGLE_PASSWORD=${ROLL_NOSQL_SINGLE_PASSWORD:-cassandra}
export ROLL_NOSQL_DIALECT=$dialect ROLL_NOSQL_DATACENTER=${ROLL_NOSQL_DATACENTER:-$dc} ROLL_NOSQL_RACK=${ROLL_NOSQL_RACK:-$rack}
# Where the in-container cqlsh and the health check connect.
cql_host=${ROLL_NOSQL_CQL%:*}
[[ $cql_host == 0.0.0.0 || $cql_host == "[::]" ]] && cql_host=127.0.0.1
echo "$cql_host:${ROLL_NOSQL_CQL##*:}" > /tmp/roll-nosql-cql 2>/dev/null || true

# --- Client encryption (client_encryption_options) ---------------------------------------------
die() { log "$*" >&2; exit 1; }
# A PEM file the configuration names (`what`: the option), checked for what roll can read.
pem_file() { # path option needs-key
  local f=$1
  [[ $dialect == cassandra && $f == conf/* ]] && f=${CASSANDRA_CONF:-/etc/cassandra}/${f#conf/}
  [[ -r $f ]] || die "client_encryption_options.$2: cannot read $f"
  if ! grep -q -- '-----BEGIN ' "$f"; then
    die "client_encryption_options.$2 $f is not PEM (a JKS or PKCS12 keystore?). roll reads PEM only, as Cassandra's \
org.apache.cassandra.security.PEMBasedSslContextFactory does. Convert it: \
keytool -importkeystore -srckeystore $f -destkeystore /tmp/store.p12 -deststoretype PKCS12, then \
openssl pkcs12 -in /tmp/store.p12 -nodes -out store.pem (a keystore: the key and its certificate chain; \
a truststore: the CA certificates), and point $2 at store.pem with ssl_context_factory: {class_name: \
org.apache.cassandra.security.PEMBasedSslContextFactory}."
  fi
  if grep -q -- '-----BEGIN ENCRYPTED PRIVATE KEY' "$f"; then
    die "client_encryption_options.$2 $f holds an encrypted private key; roll needs it unencrypted: openssl pkcs8 -in $f -out key.pem"
  fi
  [[ $3 != key ]] || grep -q -- 'PRIVATE KEY-----' "$f" || die "client_encryption_options.$2 $f holds no private key"
  echo "$f"
}
tls_on=$(yaml_at "$conf" client_encryption_options.enabled)
if [[ ${tls_on,,} =~ ^(true|yes|1)$ && -z ${ROLL_NOSQL_TLS_CERT:-} ]]; then
  tlsdir=/tmp/roll-nosql-tls
  mkdir -p "$tlsdir" && chmod 700 "$tlsdir"
  client_auth=$(yaml_at "$conf" client_encryption_options.require_client_auth)
  truststore=$(yaml_at "$conf" client_encryption_options.truststore)
  port_ssl=${flag[native-transport-port-ssl]:-$(yaml_get "$conf" native_transport_port_ssl)}
  if [[ $dialect == cassandra ]]; then
    factory=$(yaml_at "$conf" client_encryption_options.ssl_context_factory.class_name)
    keystore=$(yaml_at "$conf" client_encryption_options.keystore)
    v=$(yaml_at "$conf" client_encryption_options.ssl_context_factory.parameters.private_key)
    [[ -n $v ]] && printf '%s\n' "$v" > "$tlsdir/keystore.pem" && keystore=$tlsdir/keystore.pem
    v=$(yaml_at "$conf" client_encryption_options.ssl_context_factory.parameters.trusted_certificates)
    [[ -n $v ]] && printf '%s\n' "$v" > "$tlsdir/truststore.pem" && truststore=$tlsdir/truststore.pem
    [[ -n $keystore ]] || die "client_encryption_options.keystore is not set"
    [[ -z $factory || ${factory##*.} == PEMBasedSslContextFactory || ${factory##*.} == DefaultSslContextFactory ]] \
      || die "client_encryption_options.ssl_context_factory $factory: roll supports PEM key material only (PEMBasedSslContextFactory)"
    cert=$(pem_file "$keystore" keystore key) key=$cert
    v=$(yaml_at "$conf" client_encryption_options.optional)
    [[ ${v,,} =~ ^(true|yes|1)$ ]] && export ROLL_NOSQL_TLS_OPTIONAL=${ROLL_NOSQL_TLS_OPTIONAL:-true}
  else
    v=$(yaml_at "$conf" client_encryption_options.certificate); cert=$(pem_file "${v:-/etc/scylla/scylla.crt}" certificate cert)
    v=$(yaml_at "$conf" client_encryption_options.keyfile); key=$(pem_file "${v:-/etc/scylla/scylla.key}" keyfile key)
    aware_ssl=${flag[native-shard-aware-transport-port-ssl]:-$(yaml_get "$conf" native_shard_aware_transport_port_ssl)}
    [[ -n $aware_ssl && $aware_ssl != "$aware_port" ]] && export ROLL_NOSQL_CQL_SHARD_AWARE_TLS=${ROLL_NOSQL_CQL_SHARD_AWARE_TLS:-0.0.0.0:$aware_ssl}
  fi
  export ROLL_NOSQL_TLS_CERT=$cert ROLL_NOSQL_TLS_KEY=$key
  if [[ -n $truststore && -z ${ROLL_NOSQL_TLS_CLIENT_CA:-} ]]; then
    ca=$(pem_file "$truststore" truststore cert)
    export ROLL_NOSQL_TLS_CLIENT_CA=$ca
  fi
  case "${client_auth,,}" in
    true|yes|1) export ROLL_NOSQL_TLS_CLIENT_AUTH=${ROLL_NOSQL_TLS_CLIENT_AUTH:-true} ;;
    optional) export ROLL_NOSQL_TLS_CLIENT_AUTH=${ROLL_NOSQL_TLS_CLIENT_AUTH:-optional} ;;
  esac
  if [[ ${ROLL_NOSQL_TLS_CLIENT_AUTH:-false} != false && -z ${ROLL_NOSQL_TLS_CLIENT_CA:-} ]]; then
    die "client_encryption_options.require_client_auth needs a truststore (PEM) with the CA certificates that sign client certificates"
  fi
  [[ -n $port_ssl && $port_ssl != "$cql_port" ]] && export ROLL_NOSQL_CQL_TLS=${ROLL_NOSQL_CQL_TLS:-0.0.0.0:$port_ssl}
  log "TLS from client_encryption_options: certificate $cert, client certificates ${ROLL_NOSQL_TLS_CLIENT_AUTH:-false}${ROLL_NOSQL_CQL_TLS:+, TLS port ${ROLL_NOSQL_CQL_TLS##*:}}${ROLL_NOSQL_CQL_SHARD_AWARE_TLS:+, shard-aware TLS port ${ROLL_NOSQL_CQL_SHARD_AWARE_TLS##*:}}"
fi
# ScyllaDB's role extraction for the certificate authenticators (JSON: its values have commas).
if [[ $dialect == scylla && -z ${ROLL_NOSQL_SINGLE_SETTINGS_JSON:-} ]]; then
  rq=$(yaml_role_queries "$conf")
  [[ -n $rq ]] && export ROLL_NOSQL_SINGLE_SETTINGS_JSON="{\"auth_certificate_role_queries\": $rq}"
fi

# Settings: the reference defaults first, configured values after (later ones win), and
# ROLL_NOSQL_SINGLE_SETTINGS last.
all=("authenticator=AllowAllAuthenticator" "authorizer=AllowAllAuthorizer" "${settings[@]}")
[[ -n ${ROLL_NOSQL_SINGLE_SETTINGS:-} ]] && IFS=',' read -ra extra <<<"$ROLL_NOSQL_SINGLE_SETTINGS" && all+=("${extra[@]}")
unset ROLL_NOSQL_SINGLE_SETTINGS
declare -A seen=()
final=()
for ((i=${#all[@]}-1; i>=0; i--)); do
  k=${all[i]%%=*}
  case "$k" in enable_user_defined_functions) k=user_defined_functions_enabled ;; enable_materialized_views) k=materialized_views_enabled ;; enable_sasi_indexes) k=sasi_indexes_enabled ;; esac
  [[ -n ${seen[$k]:-} ]] && continue
  seen[$k]=1
  final=("$k=${all[i]#*=}" "${final[@]}")
done
setting_args=()
for s in "${final[@]}"; do setting_args+=("--nosql-single-setting=$s"); done
# Cassandra refuses to start with an mTLS authenticator unless client certificates are required
# (`MutualTlsAuthenticator.checkMtlsConfigurationIsValid`) and its validator is set.
for s in "${final[@]}"; do
  [[ $s == authenticator=* ]] || continue
  case "${s##*.}" in
    MutualTlsAuthenticator|MutualTlsWithPasswordFallbackAuthenticator|authenticator=MutualTlsAuthenticator|authenticator=MutualTlsWithPasswordFallbackAuthenticator)
      [[ $dialect == cassandra ]] || break
      validator=$(yaml_at "$conf" authenticator.parameters.validator_class_name)
      if [[ -f $conf ]] && [[ -z $validator ]]; then die "authenticator.parameters.validator_class_name is not set"; fi
      [[ -z $validator || ${validator##*.} == SpiffeCertificateValidator ]] || die "authenticator.parameters.validator_class_name $validator: roll has SpiffeCertificateValidator only"
      if [[ -z ${ROLL_NOSQL_TLS_CERT:-} || ${ROLL_NOSQL_TLS_CLIENT_AUTH:-false} != true ]]; then
        die "MutualTlsAuthenticator requires client_encryption_options.enabled to be true & client_encryption_options.require_client_auth to be true"
      fi ;;
  esac
done

log "dialect $dialect, cluster '$cluster_name', datacenter $ROLL_NOSQL_DATACENTER, rack $ROLL_NOSQL_RACK, address $ROLL_PUBLIC_HOST, settings ${final[*]}"
exec rolld --nosql-only "--nosql-cluster-name=$cluster_name" "${setting_args[@]}"
