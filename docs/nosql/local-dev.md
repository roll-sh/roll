# roll nosql — local development and CI

`roll/nosql` is a Docker image that replaces `cassandra:5.0` and `scylladb/scylla` in local
development, CI and Testcontainers. It runs one database in single mode (`rolld --nosql-only`).

- It is ready in well under a second: about 160 ms from container start to `Startup complete`
  (`tests/image.rs` measures it). Cassandra takes about 30 s and ScyllaDB about 10 s.
- It takes the reference images' own configuration: Cassandra's environment variables and
  `cassandra.yaml`, ScyllaDB's command-line flags and `scylla.yaml`.
- It needs about 50 MB of RAM at idle, and no `--memory` tuning.

## Run it

```sh
docker run -d --name nosql -p 9042:9042 roll/nosql                     # ScyllaDB dialect
docker run -d --name nosql -p 9042:9042 -e CASSANDRA_CLUSTER_NAME=dev roll/nosql   # Cassandra dialect
docker run -d --name nosql -p 9042:9042 roll/nosql --smp 2 --authenticator PasswordAuthenticator
docker exec -it nosql cqlsh                                            # cqlsh inside, as in the reference images
```

| port | what |
|---|---|
| 9042 | CQL |
| 19042 | CQL, ScyllaDB's shard-aware port (advertised in SUPPORTED) |
| 8000 | Alternator, the DynamoDB API (always on; ScyllaDB enables it only with `--alternator-port`) |
| 10000 | ScyllaDB's REST API (`nodetool`), bound to `0.0.0.0` (ScyllaDB binds `127.0.0.1` unless `--api-address`) |
| 9180 | Prometheus metrics, under ScyllaDB's names |
| 7070 | the roll API (`/v1/nosql`), for `roll nosql …` and the dashboard |

- **Data** lives in the volume `/var/lib/roll`. It survives restarts and new containers on the
  same volume.
- **S3.** By default, object storage is a directory in that volume. Set `ROLL_S3_BUCKET` (plus
  `ROLL_S3_PREFIX`, `ROLL_S3_ENDPOINT` for MinIO, R2 and the like, `AWS_REGION` and the usual
  `AWS_*` credentials) to keep the data on S3 instead. The volume is then only a cache.
- **User.** The image runs as the non-root user `roll` (uid 999). For a bind mount, make the
  directory writable for that uid, or use `--user`.
- **Health check.** `HEALTHCHECK` reports healthy once CQL accepts connections. The listeners
  open only after the database is loaded, and seeded when `ROLL_NOSQL_FROM` is set.
- **Logs.** Once it serves, it prints the lines that Testcontainers and compose health checks
  wait for:
  - Cassandra's `Starting listening for CQL clients on …` and `Startup complete`
  - ScyllaDB's `init - serving` and `… initialization completed.`
- **cqlsh.** `cqlsh` in the image takes Apache cqlsh's command line
  (`cqlsh [options] [host [port]]`, `CQLSH_HOST`, `CQLSH_PORT`). It runs roll's built-in shell,
  which prints what the cqlsh of the database's dialect prints.
- **Commands.** An argument that is not a flag runs as a command, for example
  `docker run --rm -it roll/nosql cqlsh some-host`.

## Dialect

The dialect is chosen in this order:

1. `ROLL_NOSQL_DIALECT=scylla|cassandra`, if set.
2. `scylla`, if any ScyllaDB flag is given.
3. `cassandra`, if any `CASSANDRA_*` variable is set, the command is `cassandra` (the Cassandra
   image's `cassandra -f`), or `/etc/cassandra/cassandra.yaml` is mounted.
4. `scylla` otherwise. This is roll's default dialect.

So Testcontainers' Cassandra modules get the Cassandra dialect, because they set `CASSANDRA_DC`
and similar variables. Its ScyllaDB modules get the ScyllaDB dialect, because they pass flags.
The dialect, cluster name, datacenter and rack are fixed when the database is first created. A
later container on the same volume keeps them and logs a warning when they differ. Settings
such as the authenticator follow each start's configuration, as a node's do.

## Configuration mapping

What the reference images take, and what roll does with it. `tests/image.rs` covers every row.

### cassandra:5.0

| input | effect |
|---|---|
| `CASSANDRA_CLUSTER_NAME` | `cluster_name` (default `Test Cluster`) |
| `CASSANDRA_NUM_TOKENS` | `num_tokens` (default 16) |
| `CASSANDRA_ENDPOINT_SNITCH` | With `GossipingPropertyFileSnitch`: `CASSANDRA_DC` / `CASSANDRA_RACK`, else `cassandra-rackdc.properties`, else `dc1` / `rack1` (the file as shipped). Any other snitch: `datacenter1` / `rack1`, as Cassandra does. |
| `CASSANDRA_DC`, `CASSANDRA_RACK` | Only with `GossipingPropertyFileSnitch` (see above) |
| `CASSANDRA_BROADCAST_RPC_ADDRESS`, `CASSANDRA_BROADCAST_ADDRESS` | The address in `system.local` (default: the container's IP; `auto` too) |
| `CASSANDRA_LISTEN_ADDRESS`, `CASSANDRA_RPC_ADDRESS`, `CASSANDRA_SEEDS`, `CASSANDRA_START_RPC` | Accepted; roll listens on `0.0.0.0` and has no ring |
| `MAX_HEAP_SIZE` | Sizes roll's memory budgets as Cassandra sizes its own from the heap: memtables a quarter (`memtable_heap_space`), the block cache a quarter, at most 512 MiB (`file_cache_size`). See [Memory](#memory). |
| `HEAP_NEWSIZE`, `JVM_OPTS`, `JVM_EXTRA_OPTS` | Ignored (no JVM) |
| `cassandra [-f] [-R]` | The image's command: selects the dialect |
| `/etc/cassandra/cassandra.yaml` | These top-level keys: `cluster_name`, `num_tokens`, `endpoint_snitch`, `native_transport_port`, `authenticator` (also Cassandra 5's `class_name:` block), `authorizer`, every `settings` key roll models (`user_defined_functions_enabled`, `materialized_views_enabled`, `sasi_indexes_enabled`, batch and tombstone thresholds, …), and the memory keys `memtable_heap_space` (`_in_mb`), `file_cache_size` (`_in_mb`), `memtable_flush_writers` and `concurrent_compactors`; `client_encryption_options` and `native_transport_port_ssl` ([TLS](#tls-and-client-certificates)) |
| `/etc/cassandra/cassandra-rackdc.properties` | `dc` and `rack` under `GossipingPropertyFileSnitch` |

### scylladb/scylla

Flags take both the `--flag value` and the `--flag=value` form.

| input | effect |
|---|---|
| `--smp N` | The shard count (`SCYLLA_NR_SHARDS`), which ScyllaDB drivers open one connection each for |
| `--cluster-name` | `cluster_name` (default empty, as ScyllaDB's) |
| `--dc`, `--rack` | Datacenter and rack. They imply `GossipingPropertyFileSnitch`; another `--endpoint-snitch` is refused, as ScyllaDB's entrypoint does. |
| `--endpoint-snitch` | `GossipingPropertyFileSnitch` reads `/etc/scylla/cassandra-rackdc.properties` (default `datacenter1` / `rack1`) |
| `--num-tokens` | `num_tokens` (default 256) |
| `--authenticator`, `--authorizer` | `PasswordAuthenticator`, `CassandraAuthorizer` (or the `AllowAll*` defaults). The superuser is `cassandra`/`cassandra` (`ROLL_NOSQL_SINGLE_PASSWORD`). |
| `--enable-user-defined-functions` | Lua UDFs and UDAs |
| `--experimental-features` | Accepted; roll has no experimental gates |
| `--broadcast-rpc-address`, `--broadcast-address` | The address in `system.local` and Alternator's `/localnodes` |
| `--api-address`, `--api-port` | The REST listener (default `0.0.0.0:10000`) |
| `--alternator-port`, `--alternator-address` | The Alternator listener (default `0.0.0.0:8000`) |
| `--alternator-https-port` | Needs `ROLL_NOSQL_TLS_CERT` / `ROLL_NOSQL_TLS_KEY`; otherwise a warning |
| `--alternator-write-isolation` | Alternator's default write isolation |
| `--alternator-ttl-period-in-seconds` | How often Alternator deletes expired items |
| `--prometheus-address`, `--prometheus-port` | The metrics listener (default `0.0.0.0:9180`) |
| `--native-transport-port`, `--native-shard-aware-transport-port` | The CQL ports inside the container |
| `--native-transport-port-ssl`, `--native-shard-aware-transport-port-ssl` | Dedicated TLS ports (see [TLS](#tls-and-client-certificates)) |
| any other scylla.yaml option roll models, such as `--tombstone-warn-threshold 500` | That setting |
| `--memory` | Sizes roll's memory budgets: memtables a quarter, the block cache a quarter. See [Memory](#memory). |
| `--reserve-memory`, `--overprovisioned`, `--developer-mode`, `--cpuset`, `--io-setup`, `--seeds`, `--listen-address`, `--rpc-address`, `--skip-wait-for-gossip-to-settle`, `--disable-version-check`, `--blocked-reactor-notify-ms`, `--lock-memory`, … | Accepted, no effect |
| unknown flags | Logged and ignored (ScyllaDB would refuse to start) |
| `/etc/scylla/scylla.yaml` | The same keys as `cassandra.yaml`, plus `alternator_ttl_period_in_seconds`, `client_encryption_options`, the `_ssl` ports and `auth_certificate_role_queries` ([TLS](#tls-and-client-certificates)) |

### Memory

The image sizes roll's budgets from what the reference would use. The first of these that is
set wins:

1. `ROLL_NOSQL_MEMTABLE_BUDGET` and `ROLL_NOSQL_CACHE_MEMORY` (each on its own).
2. `memtable_heap_space` / `memtable_heap_space_in_mb` and `file_cache_size` /
   `file_cache_size_in_mb` in the configuration file.
3. ScyllaDB's `--memory`, or Cassandra's `MAX_HEAP_SIZE`. Memtables get a quarter of it, and
   the block cache a quarter (at most 512 MiB with Cassandra, as its `file_cache_size`).
4. The container's memory limit (`docker run --memory`), split the same way.
5. rolld's defaults: 512 MiB of memtables and a 256 MiB block cache.

The rest is left for connections, compaction and indexes. For example, `--memory 1G` gives
256 MiB of memtables and a 256 MiB cache; `MAX_HEAP_SIZE=2G` gives 512 MiB and 512 MiB. When
the memtables are full and flushes can't keep up, writes wait and then time out after 2 s
with the `WriteTimeout` the references send. The entrypoint logs the budgets it chose, and
`GET /v1/nosql` reports them under `memory`. An idle database (no clients for its idle
timeout, or `POST /v1/nosql/databases/<id>/unload`) gives its memory back within about a
second.

### TLS and client certificates

The configuration file's `client_encryption_options` turns TLS on, as on the reference images.
Mount the file and the key material:

```sh
docker run -v $PWD/cassandra.yaml:/etc/cassandra/cassandra.yaml:ro -v $PWD/certs:/certs:ro roll/nosql
```

roll reads PEM only. Cassandra 5 reads PEM too, through
`org.apache.cassandra.security.PEMBasedSslContextFactory`, so a configuration written for it
works unchanged:

```yaml
# cassandra.yaml
client_encryption_options:
  enabled: true
  optional: false              # true: TLS and plain text on the same port
  keystore: /certs/keystore.pem  # the private key and its certificate chain
  truststore: /certs/ca.pem      # CA certificates that sign client certificates
  require_client_auth: true
  ssl_context_factory:
    class_name: org.apache.cassandra.security.PEMBasedSslContextFactory
    # or inline: parameters: {private_key: |..., trusted_certificates: |...}
native_transport_port_ssl: 9142  # a TLS port beside the plain 9042 (deprecated in Cassandra 5)
```

A JKS or PKCS12 keystore (Cassandra's default `DefaultSslContextFactory`) stops the container
with a message that says how to convert it:

```sh
keytool -importkeystore -srckeystore keystore.jks -destkeystore keystore.p12 -deststoretype PKCS12
openssl pkcs12 -in keystore.p12 -nodes -out keystore.pem   # key + chain (or the CA certificates of a truststore)
```

An encrypted private key (`keystore_password`) has to be decrypted first:
`openssl pkcs8 -in key.pem -out key-plain.pem`.

```yaml
# scylla.yaml
client_encryption_options:
  enabled: true
  certificate: /certs/db.crt   # default /etc/scylla/scylla.crt
  keyfile: /certs/db.key       # default /etc/scylla/scylla.key
  truststore: /certs/ca.pem
  require_client_auth: true    # or optional
native_transport_port_ssl: 9142               # optional: dedicated TLS ports, the plain
native_shard_aware_transport_port_ssl: 19142  # ones stay plain (SCYLLA_SHARD_AWARE_PORT_SSL)
```

Without the `_ssl` ports, every CQL port speaks TLS only, as on ScyllaDB. Without
`client_encryption_options`, `ROLL_NOSQL_TLS_*` (below) configure the same directly.

**Client-certificate authentication.** The verified client certificate authenticates the
connection with the reference's certificate authenticators:

| dialect | `authenticator` | role |
|---|---|---|
| cassandra | `org.apache.cassandra.auth.MutualTlsAuthenticator` (with `validator_class_name: org.apache.cassandra.auth.SpiffeCertificateValidator`) | The certificate's `spiffe://` URI, mapped by `ADD IDENTITY '<spiffe id>' TO ROLE '<role>'`. Clients still get AUTHENTICATE; any credentials do. |
| cassandra | `MutualTlsWithPasswordFallbackAuthenticator` | As above with a certificate; a password without one (plain text with `optional: true`) |
| scylla | `com.scylladb.auth.CertificateAuthenticator` | The first capture of the first `auth_certificate_role_queries` entry that matches (default `SUBJECT` `CN=([^,]+)`); READY without any SASL exchange |
| scylla | `com.scylladb.auth.CertificateOrPasswordAuthenticator` | As above with a certificate; a password without one |

As Cassandra does, the image refuses to start with an mTLS authenticator unless
`require_client_auth` is true (`MutualTlsAuthenticator requires client_encryption_options.enabled
to be true & client_encryption_options.require_client_auth to be true`). The role must exist
and have `LOGIN`. With `PasswordAuthenticator`, a client certificate is only checked by TLS.

### roll's own variables

| variable | default | |
|---|---|---|
| `ROLL_NOSQL_DIALECT` | auto | `scylla` or `cassandra` |
| `ROLL_NOSQL_SINGLE` | `default` | The database's name in the roll API |
| `ROLL_NOSQL_SINGLE_PASSWORD` | `cassandra` | The superuser `cassandra`'s password when the database is created |
| `ROLL_NOSQL_SINGLE_SETTINGS` | | Extra settings as `key=value,key=value`; they win over everything above |
| `ROLL_NOSQL_FROM` | | Seeds the database once before serving (below) |
| `ROLL_NOSQL_FROM_TOKEN` | | Bearer token for `ROLL_NOSQL_FROM`'s API |
| `ROLL_NOSQL_FROM_KEYSPACES` | all | Comma-separated keyspaces to copy |
| `ROLL_PUBLIC_HOST` | the container's IP | The address in `system.local` |
| `ROLL_S3_BUCKET`, `ROLL_S3_PREFIX`, `ROLL_S3_ENDPOINT`, `AWS_REGION` | local directory | Object storage |
| `ROLL_NOSQL_TLS_CERT`, `ROLL_NOSQL_TLS_KEY` | from `client_encryption_options` | TLS on the CQL ports (PEM) |
| `ROLL_NOSQL_TLS_CLIENT_CA` | the truststore | CA certificates (PEM) client certificates must chain to |
| `ROLL_NOSQL_TLS_CLIENT_AUTH` | `require_client_auth`, else `false` | `false`, `optional` or `true` |
| `ROLL_NOSQL_TLS_OPTIONAL` | Cassandra's `optional` | TLS and plain text on the same ports |
| `ROLL_NOSQL_CQL_TLS`, `ROLL_NOSQL_CQL_SHARD_AWARE_TLS` | from the `_ssl` ports | Dedicated TLS listeners; the main ones stay plain |
| `ROLL_NOSQL_SINGLE_SETTINGS_JSON` | `auth_certificate_role_queries` from scylla.yaml | More settings as a JSON object (for values with commas) |
| `ROLL_NOSQL_MEMTABLE_BUDGET` | from `--memory` / `MAX_HEAP_SIZE`, else `512MiB` | Memtables of every database together ([Memory](#memory)) |
| `ROLL_NOSQL_MEMTABLE_DB_CAP` | the whole budget in single mode | One database's memtable cap, unless its `memtable_space` setting gives one |
| `ROLL_NOSQL_CACHE_MEMORY` | from `--memory` / `MAX_HEAP_SIZE`, else `256MiB` | Decoded SSTable blocks in memory |
| `ROLL_NOSQL_CACHE_DISK` | `4GiB` | Stored SSTable blocks on local disk (with S3 storage only) |
| `ROLL_NOSQL_FLUSH_CONCURRENCY`, `ROLL_NOSQL_COMPACTION_CONCURRENCY` | `2`, `2` | Flushes and compactions running at once |
| `ROLL_NOSQL_CQL`, `ROLL_NOSQL_CQL_SHARD_AWARE`, `ROLL_NOSQL_ALTERNATOR`, `ROLL_NOSQL_REST`, `ROLL_NOSQL_METRICS`, `ROLL_API` | see the ports table | Listener addresses |
| `RUST_LOG` | `info` | Log filter |

## Start from a branch

```sh
docker run -e ROLL_NOSQL_FROM=https://api.roll.example/pr-482 -e ROLL_NOSQL_FROM_TOKEN=$ROLL_TOKEN roll/nosql
docker run -e ROLL_NOSQL_FROM='cql://reader:secret@prod-host:9042?tls=1' -e ROLL_NOSQL_FROM_KEYSPACES=app roll/nosql
```

- **Two forms.** `ROLL_NOSQL_FROM` takes `<api url>/<branch>` (a roll API and a database name or
  id) or a live cluster as `cql://user:pass@host:port[?tls=1]`.
- **How it copies.** With an API URL, the image looks the branch up with
  `GET /v1/nosql/databases/<branch>`, which returns its CQL endpoint, TLS server name and
  credentials. It then runs a `cql://` import from it, the same import as
  `roll nosql import --from cql://…`, so write times and TTLs are kept.
- **When.** The copy runs to completion before the listeners open, so the first client sees
  the data. It runs once per volume. A failed copy stops the container.
- **Follow-up:** a zero-copy start that reads the branch's manifest and SSTables straight from
  object storage, with no CQL scan. It needs an export route in the control API.

## docker compose

```yaml
services:
  nosql:
    image: roll/nosql
    command: ["--smp", "2", "--authenticator", "PasswordAuthenticator"]   # or: environment: {CASSANDRA_DC: dc1}
    ports: ["9042:9042", "8000:8000"]
    volumes: ["nosql:/var/lib/roll"]
    healthcheck:   # the image has one; this is the same thing with compose's syntax
      test: ["CMD", "healthcheck"]
      interval: 1s
      retries: 30
  app:
    depends_on:
      nosql: {condition: service_healthy}
volumes:
  nosql: {}
```

## Testcontainers

Tell each module the roll image is compatible with the one it expects.
`tests/tools.rs` runs all four of these against the image.

**Java** (`org.testcontainers:testcontainers-cassandra` / `testcontainers-scylladb` 2.x):

```java
var roll = DockerImageName.parse("roll/nosql");
try (var cassandra = new CassandraContainer(roll.asCompatibleSubstituteFor("cassandra")).withInitScript("init.cql")) {
    cassandra.start();   // waits on `cqlsh -u cassandra -p cassandra -e "SELECT release_version …"` in the container
    var session = CqlSession.builder().addContactPoint(cassandra.getContactPoint())
        .withLocalDatacenter(cassandra.getLocalDatacenter()).build();
}
// PasswordAuthenticator the documented way: a directory with a cassandra.yaml on the classpath.
new CassandraContainer(roll.asCompatibleSubstituteFor("cassandra")).withConfigurationOverride("cassandra-auth");
try (var scylla = new ScyllaDBContainer(roll.asCompatibleSubstituteFor("scylladb/scylla")).withAlternator()) {
    scylla.start();      // waits for "initialization completed."
}
```

**Python** (`testcontainers` 4.x):

```python
from testcontainers.cassandra import CassandraContainer   # waits for "Startup complete"
from testcontainers.scylla import ScyllaContainer         # waits for "Starting listening for CQL clients"

with CassandraContainer("roll/nosql") as c:
    cluster = Cluster(c.get_contact_points(), load_balancing_policy=DCAwareRoundRobinPolicy(c.get_local_datacenter()))
with ScyllaContainer("roll/nosql") as s:
    session = s.get_cluster().connect()
```

**Go** (`testcontainers-go` modules `cassandra` and `scylladb`; both wait on
`cqlsh -e "SELECT bootstrapped FROM system.local"` in the container):

```go
ctr, err := cassandra.Run(ctx, "roll/nosql", cassandra.WithInitScripts("init.cql"))
host, _ := ctr.ConnectionHost(ctx)
sc, err := scylladb.Run(ctx, "roll/nosql", scylladb.WithShardAwareness(), scylladb.WithAlternator())
```

**Node** (`@testcontainers/cassandra`, `@testcontainers/scylladb`):

```js
const c = await new CassandraContainer('roll/nosql').withDatacenter('dc1').start();   // Docker health check with cqlsh
const client = new cassandra.Client({ contactPoints: [c.getContactPoint()], localDataCenter: c.getDatacenter(),
  authProvider: new cassandra.auth.PlainTextAuthProvider(c.getUsername(), c.getPassword()) });
const s = await new ScyllaContainer('roll/nosql').start();
```

## CI recipes

GitHub Actions service container:

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      nosql:
        image: roll/nosql
        env: {CASSANDRA_CLUSTER_NAME: ci}         # Cassandra dialect; drop it for ScyllaDB
        ports: ["9042:9042"]
        options: --health-cmd healthcheck --health-interval 1s --health-retries 30
    steps:
      - run: ./run-tests --cassandra-host 127.0.0.1
```

There are two other ways to get a database in CI:

- **A branch per job.** Create one through the API (`roll nosql branch app-prod --name ci-$RUN_ID
  --expires 2h`), then point the tests at its endpoint. See
  [preview-environments.md](preview-environments.md).
- **Realistic data in a container.** Run the image with
  `ROLL_NOSQL_FROM=<api url>/<branch>`.

## Building the image

The image's Dockerfile is in `deploy/nosql/`. There are two ways to build it:

- **CI** builds `deploy/nosql/Dockerfile`, a multi-stage release build with cargo cache mounts:

  ```sh
  docker build -f deploy/nosql/Dockerfile -t roll/nosql .
  ```

- **A development machine** uses `deploy/nosql/build-local.sh`. It compiles `rolld` and `roll`
  in a memory-capped `rust:1-bookworm` container under the machine-wide build lock, so the
  binaries run on the image's glibc. It then packs them with `Dockerfile.prebuilt`, which is
  the same final stage, and tags the result `roll/nosql:dev`.

The final image is `debian:bookworm-slim` with `tini`, `rolld`, `roll` and three small scripts
(the entrypoint, `cqlsh` and `healthcheck`). It is about 72 MB compressed.

## Tests

Each of these suites needs `ROLL_NOSQL_IMAGE=roll/nosql:dev`, and `scripts/test-nosql.sh` runs
them all:

| suite | what |
|---|---|
| `crates/nosql-server/tests/image.rs` | start-up time, ports, log lines, health check, user, in-container `cqlsh`, every configuration mapping above (TLS and client certificates from a mounted cassandra.yaml and scylla.yaml: `tls_from_configuration_files`), persistence, settings across restarts, `ROLL_NOSQL_FROM`, graceful stop |
| `crates/nosql-server/tests/tools.rs` | Testcontainers for Java, Python, Go and Node; golang-migrate; cassandra-stress; scylla-bench; the reference images' cqlsh (DESCRIBE, SELECT, COPY), compared with the references |
| `crates/nosql-server/tests/drivers.rs` (`docker_drivers`) | the Python, Java, Go and Node drivers, compared with the references (see [compatibility.md](compatibility.md)) |
