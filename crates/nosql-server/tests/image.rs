//! The `roll/nosql` image (deploy/nosql): startup, the log lines and health check Testcontainers
//! and compose wait for, the in-container `cqlsh`, and every mapping of the reference images'
//! configuration — cassandra:5.0's environment, `cassandra` command and cassandra.yaml,
//! scylladb/scylla's flags and scylla.yaml — onto roll's settings; persistence across restarts,
//! seeding from a branch (`ROLL_NOSQL_FROM`) and graceful stop; TLS with the Python drivers and
//! both cqlsh (`--ssl`).
//!
//! Needs ROLL_NOSQL_IMAGE (deploy/nosql/build-local.sh builds `roll/nosql:dev`); skips without.

mod common;

use std::net::IpAddr;
use std::time::{Duration, Instant};

use common::docker::{self, Container, RunOpts};
use nosql_proto::client::{Client, ClientConfig, ClientError};
use nosql_proto::{QueryParams, ResultBody};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

async fn connect(ip: &str, creds: Option<(&str, &str)>) -> Result<Client, ClientError> {
    let mut cfg = ClientConfig::new(format!("{ip}:9042"));
    if let Some((u, p)) = creds {
        cfg = cfg.credentials(u, p);
    }
    Client::connect(cfg).await
}

/// One row's cells (`None` for null).
async fn row(c: &Client, cql: &str) -> Vec<Option<bytes::Bytes>> {
    match c.query(cql, QueryParams::default()).await.unwrap_or_else(|e| panic!("{cql}: {e:?}")) {
        ResultBody::Rows(r) => r.to_vec().into_iter().next().unwrap_or_else(|| panic!("{cql}: no rows")),
        other => panic!("{cql}: {other:?}"),
    }
}

fn text(b: &Option<bytes::Bytes>) -> String {
    String::from_utf8_lossy(b.as_deref().unwrap_or_default()).into_owned()
}

/// What `system.local` and SUPPORTED say about the node.
#[derive(Debug)]
struct Identity {
    release_version: String,
    cluster_name: String,
    data_center: String,
    rack: String,
    rpc_address: IpAddr,
    tokens: usize,
    shards: Option<String>,
    shard_aware_port: Option<String>,
}

async fn identity(c: &Client) -> Identity {
    let r = row(c, "SELECT release_version, cluster_name, data_center, rack, rpc_address, tokens FROM system.local").await;
    let ip = r[4].as_deref().unwrap();
    let rpc_address = match ip.len() {
        4 => IpAddr::from(<[u8; 4]>::try_from(ip).unwrap()),
        _ => IpAddr::from(<[u8; 16]>::try_from(ip).unwrap()),
    };
    let tokens = r[5].as_deref().map(|b| i32::from_be_bytes(b[..4].try_into().unwrap()) as usize).unwrap_or(0);
    let sup = c.supported();
    Identity {
        release_version: text(&r[0]),
        cluster_name: text(&r[1]),
        data_center: text(&r[2]),
        rack: text(&r[3]),
        rpc_address,
        tokens,
        shards: sup.get("SCYLLA_NR_SHARDS").and_then(|v| v.first().cloned()),
        shard_aware_port: sup.get("SCYLLA_SHARD_AWARE_PORT").and_then(|v| v.first().cloned()),
    }
}

fn start(image: &str, opts: RunOpts) -> Container {
    let c = Container::run(image, &opts);
    c.wait_cql(Duration::from_secs(30));
    c
}

fn args(a: &[&str]) -> RunOpts<'static> {
    RunOpts { args: a.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

fn env(e: &[(&'static str, &str)]) -> RunOpts<'static> {
    RunOpts { env: e.iter().map(|(k, v)| (*k, v.to_string())).collect(), ..Default::default() }
}

fn tcp_open(ip: &str, port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&format!("{ip}:{port}").parse().unwrap(), Duration::from_millis(500)).is_ok()
}

/// Defaults: ScyllaDB dialect, the reference's identity (empty cluster name, datacenter1/rack1,
/// 256 tokens, the container's address, one shard on 19042), every port, the log lines,
/// a healthy health check, a non-root user, `cqlsh` inside, and start-up time.
#[test]
fn defaults_ports_logs_and_startup() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let t0 = Instant::now();
    let c = Container::run(&image, &RunOpts::default());
    let ready = c.wait_cql(Duration::from_secs(30));
    let ip = c.ip();
    eprintln!("roll/nosql accepted CQL {:?} after `docker run` returned ({:?} in all)", ready, t0.elapsed());
    // From the container's start to its "Startup complete" line, by the container's clock.
    let started = docker::docker(&["inspect", "-f", "{{.State.StartedAt}}", &c.name]);
    let started = String::from_utf8_lossy(&started.stdout).trim().to_string();
    let logs = c.logs();
    let ready_line = logs.lines().find(|l| l.contains("Startup complete")).expect("Startup complete logged");
    let ts = |s: &str| chrono::DateTime::parse_from_rfc3339(s.split_whitespace().next().unwrap()).unwrap();
    let startup = ts(ready_line) - ts(&started);
    eprintln!("container start to `Startup complete`: {} ms", startup.num_milliseconds());
    assert!(startup.num_milliseconds() < 1000, "startup took {startup}");
    for line in [
        "Starting listening for CQL clients on 0.0.0.0:9042 (unencrypted, non-shard-aware)",
        "Starting listening for CQL clients on 0.0.0.0:19042 (unencrypted, shard-aware)",
        "init - serving",
        "Startup complete",
        "initialization completed.",
    ] {
        assert!(logs.contains(line), "missing log line {line:?}:\n{logs}");
    }
    assert!(!logs.contains('\x1b'), "ANSI escapes in the logs");
    rt().block_on(async {
        let cl = connect(&ip, None).await.unwrap();
        let id = identity(&cl).await;
        assert_eq!(id.release_version, "3.0.8");
        assert_eq!((id.cluster_name.as_str(), id.data_center.as_str(), id.rack.as_str()), ("", "datacenter1", "rack1"));
        assert_eq!(id.rpc_address.to_string(), ip);
        assert_eq!(id.tokens, 256);
        assert_eq!((id.shards.as_deref(), id.shard_aware_port.as_deref()), (Some("1"), Some("19042")));
        // AllowAllAuthenticator: credentials are ignored, as by the reference.
        connect(&ip, Some(("nobody", "x"))).await.unwrap();
    });
    for port in [9042, 19042, 8000, 10000, 9180, 7070] {
        assert!(tcp_open(&ip, port), "port {port} closed");
    }
    let metrics = reqwest::blocking_get(&format!("http://{ip}:9180/metrics"));
    assert!(metrics.contains("scylla_transport_cql_errors_total"), "{metrics}");
    let rest = reqwest::blocking_get(&format!("http://{ip}:10000/storage_service/host_id"));
    assert!(rest.contains('-'), "{rest}");
    let api = reqwest::blocking_get(&format!("http://{ip}:7070/v1/nosql"));
    assert!(api.contains("\"single\""), "{api}");
    // Health check, user, data directory, in-container cqlsh (Testcontainers' readiness probes).
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let h = docker::docker(&["inspect", "-f", "{{.State.Health.Status}}", &c.name]);
        if h.stdout.starts_with(b"healthy") {
            break;
        }
        assert!(Instant::now() < end, "not healthy: {}", String::from_utf8_lossy(&h.stdout));
        std::thread::sleep(Duration::from_millis(250));
    }
    let id = c.exec(&["id", "-u"]);
    assert_eq!(String::from_utf8_lossy(&id.stdout).trim(), "999");
    assert!(c.exec(&["test", "-d", "/var/lib/roll/nosql"]).status.success(), "no data under /var/lib/roll");
    let o = c.exec(&["cqlsh", "-e", "SELECT bootstrapped FROM system.local"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("COMPLETED"), "{o:?}");
    let o = c.exec(&["cqlsh", "-u", "cassandra", "-p", "cassandra", "-e", "SELECT release_version FROM system.local;"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("3.0.8"), "{o:?}");
    let o = c.exec(&["cqlsh", &ip, "9042", "--request-timeout", "30", "--debug", "-e", "DESCRIBE KEYSPACES"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("system_schema"), "{o:?}");
    // A command instead of flags runs as is.
    let o = docker::docker(&["run", "--rm", "--memory", "256m", &image, "cqlsh", "--version"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "cqlsh 6.2.0");
    // docker stop: SIGTERM reaches rolld through tini, which exits cleanly and quickly.
    let t = Instant::now();
    assert!(docker::docker(&["stop", "-t", "10", &c.name]).status.success());
    assert!(t.elapsed() < Duration::from_secs(5), "stop took {:?}", t.elapsed());
    let code = docker::docker(&["inspect", "-f", "{{.State.ExitCode}}", &c.name]);
    assert_eq!(String::from_utf8_lossy(&code.stdout).trim(), "0");
}

/// cassandra:5.0's environment: the dialect, cluster name, token count, broadcast address; dc
/// and rack only with GossipingPropertyFileSnitch (dc1/rack1 as cassandra-rackdc.properties
/// ships), the JVM's variables ignored; the `cassandra -f` command.
#[test]
fn cassandra_environment() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let c = start(
        &image,
        env(&[
            ("CASSANDRA_CLUSTER_NAME", "My Cluster"),
            ("CASSANDRA_DC", "dc-x"),
            ("CASSANDRA_RACK", "rack-x"),
            ("CASSANDRA_ENDPOINT_SNITCH", "GossipingPropertyFileSnitch"),
            ("CASSANDRA_NUM_TOKENS", "4"),
            ("CASSANDRA_BROADCAST_RPC_ADDRESS", "10.1.2.3"),
            ("CASSANDRA_SEEDS", "elsewhere"),
            ("CASSANDRA_LISTEN_ADDRESS", "auto"),
            ("MAX_HEAP_SIZE", "1G"),
            ("HEAP_NEWSIZE", "200M"),
            ("JVM_OPTS", "-Dcassandra.skip_wait_for_gossip_to_settle=0"),
        ]),
    );
    rt.block_on(async {
        let id = identity(&connect(&c.ip(), None).await.unwrap()).await;
        assert_eq!(id.release_version, "5.0.9");
        assert_eq!((id.cluster_name.as_str(), id.data_center.as_str(), id.rack.as_str()), ("My Cluster", "dc-x", "rack-x"));
        assert_eq!(id.tokens, 4);
        assert_eq!(id.rpc_address.to_string(), "10.1.2.3");
        assert_eq!(id.shards, None, "Cassandra advertises no shards");
    });
    drop(c);
    // CASSANDRA_DC without GossipingPropertyFileSnitch: SimpleSnitch's datacenter1/rack1.
    let c = start(&image, env(&[("CASSANDRA_DC", "dc-x"), ("CASSANDRA_RACK", "rack-x")]));
    rt.block_on(async {
        let id = identity(&connect(&c.ip(), None).await.unwrap()).await;
        assert_eq!((id.release_version.as_str(), id.cluster_name.as_str()), ("5.0.9", "Test Cluster"));
        assert_eq!((id.data_center.as_str(), id.rack.as_str(), id.tokens), ("datacenter1", "rack1", 16));
    });
    drop(c);
    // GossipingPropertyFileSnitch without CASSANDRA_DC: the shipped dc1/rack1.
    let c = start(&image, env(&[("CASSANDRA_ENDPOINT_SNITCH", "org.apache.cassandra.locator.GossipingPropertyFileSnitch")]));
    rt.block_on(async {
        let id = identity(&connect(&c.ip(), None).await.unwrap()).await;
        assert_eq!((id.data_center.as_str(), id.rack.as_str()), ("dc1", "rack1"));
    });
    drop(c);
    // The Cassandra image's command.
    let c = start(&image, args(&["cassandra", "-f"]));
    rt.block_on(async {
        let id = identity(&connect(&c.ip(), None).await.unwrap()).await;
        assert_eq!((id.release_version.as_str(), id.cluster_name.as_str()), ("5.0.9", "Test Cluster"));
    });
}

/// scylladb/scylla's flags, both `--flag value` and `--flag=value`.
#[test]
fn scylla_flags() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let c = start(
        &image,
        args(&[
            "--smp", "3", "--memory=1G", "--overprovisioned", "1", "--developer-mode=1", "--cluster-name", "sc", "--dc", "d1", "--rack=r1",
            "--num-tokens", "8", "--broadcast-rpc-address", "10.9.9.9", "--api-address", "0.0.0.0", "--alternator-port", "8001",
            "--alternator-address", "0.0.0.0", "--alternator-write-isolation", "always", "--seeds", "x", "--skip-wait-for-gossip-to-settle=0",
            "--experimental-features", "udf", "--enable-user-defined-functions", "1", "--authenticator", "PasswordAuthenticator",
            "--authorizer", "CassandraAuthorizer", "--prometheus-port", "9181", "--reserve-memory", "0", "--io-setup", "0",
            "--disable-version-check", "--tombstone-warn-threshold", "500", "--alternator-ttl-period-in-seconds", "1",
            "--no-such-option", "7",
        ]),
    );
    let ip = c.ip();
    rt.block_on(async {
        // PasswordAuthenticator with the image's superuser cassandra/cassandra.
        let Err(e) = connect(&ip, None).await else { panic!("authentication required") };
        assert!(format!("{e:?}").contains("authenticat"), "{e:?}");
        assert!(connect(&ip, Some(("cassandra", "wrong"))).await.is_err());
        let cl = connect(&ip, Some(("cassandra", "cassandra"))).await.unwrap();
        let id = identity(&cl).await;
        assert_eq!(id.release_version, "3.0.8");
        assert_eq!((id.cluster_name.as_str(), id.data_center.as_str(), id.rack.as_str()), ("sc", "d1", "r1"));
        assert_eq!((id.tokens, id.rpc_address.to_string()), (8, "10.9.9.9".to_string()));
        assert_eq!(id.shards.as_deref(), Some("3"));
        // CassandraAuthorizer: a fresh role can't read what it wasn't granted.
        cl.query("CREATE KEYSPACE flags WITH replication = {'class': 'NetworkTopologyStrategy', 'd1': 1}", QueryParams::default()).await.unwrap();
        cl.query("CREATE TABLE flags.t (k int PRIMARY KEY)", QueryParams::default()).await.unwrap();
        cl.query("CREATE ROLE bob WITH PASSWORD = 'bob' AND LOGIN = true", QueryParams::default()).await.unwrap();
        let bob = connect(&ip, Some(("bob", "bob"))).await.unwrap();
        assert!(bob.query("SELECT * FROM flags.t", QueryParams::default()).await.is_err(), "authorizer not enforced");
        // --enable-user-defined-functions: Lua UDFs.
        cl.query("CREATE FUNCTION flags.twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x * 2'", QueryParams::default())
            .await
            .unwrap();
        // --tombstone-warn-threshold (a scylla.yaml option given as a flag).
        let s = row(&cl, "SELECT value FROM system.config WHERE name = 'tombstone_warn_threshold'").await;
        assert_eq!(text(&s[0]), "500");
    });
    assert!(tcp_open(&ip, 8001) && !tcp_open(&ip, 8000), "--alternator-port");
    assert!(tcp_open(&ip, 9181) && !tcp_open(&ip, 9180), "--prometheus-port");
    assert!(tcp_open(&ip, 10000), "--api-address 0.0.0.0");
    let logs = c.logs();
    assert!(logs.contains("--no-such-option"), "unknown options are reported:\n{logs}");
    drop(c);
    // --alternator-write-isolation forbid_rmw: a conditional write is refused (AllowAll: the
    // signature isn't checked).
    let c = start(&image, args(&["--alternator-port", "8000", "--alternator-write-isolation", "forbid_rmw"]));
    let ip = c.ip();
    let body = r#"{"TableName":"tbl","KeySchema":[{"AttributeName":"k","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"k","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST"}"#;
    let (_, r) = alternator(&ip, 8000, "CreateTable", body);
    assert!(r.contains("TableDescription"), "{r}");
    let (status, r) = alternator(&ip, 8000, "PutItem", r#"{"TableName":"tbl","Item":{"k":{"S":"a"}},"ConditionExpression":"attribute_not_exists(k)"}"#);
    assert!(status == 400 && r.contains("forbid_rmw"), "{status} {r}");
    drop(c);
    // --dc with a snitch other than GossipingPropertyFileSnitch is refused, as ScyllaDB's entrypoint does.
    let c = Container::run(&image, &args(&["--endpoint-snitch", "SimpleSnitch", "--dc", "x"]));
    let end = Instant::now() + Duration::from_secs(20);
    while c.running() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!c.running() && c.logs().contains("Cannot use dc and rack parameters"), "{}", c.logs());
    drop(c);
    // ROLL_NOSQL_DIALECT wins over the hints; a bad value stops the container.
    let c = start(&image, RunOpts { env: vec![("ROLL_NOSQL_DIALECT", "cassandra".into())], args: vec!["--smp".into(), "2".into()], ..Default::default() });
    rt.block_on(async { assert_eq!(identity(&connect(&c.ip(), None).await.unwrap()).await.release_version, "5.0.9") });
    drop(c);
    let o = docker::docker(&["run", "--rm", "--memory", "256m", "-e", "ROLL_NOSQL_DIALECT=dynamo", &image]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("must be scylla or cassandra"), "{o:?}");
}

/// An Alternator request with an access key the AllowAll database doesn't check.
fn alternator(ip: &str, port: u16, op: &str, body: &str) -> (u16, String) {
    let now = chrono::Utc::now();
    let auth = format!("AWS4-HMAC-SHA256 Credential=cassandra/{}/us-east-1/dynamodb/aws4_request, SignedHeaders=host;x-amz-date, Signature=00", now.format("%Y%m%d"));
    let date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let target = format!("DynamoDB_20120810.{op}");
    let r = reqwest::blocking_post(&format!("http://{ip}:{port}/"), &[("X-Amz-Target", &target), ("Content-Type", "application/x-amz-json-1.0"), ("Authorization", &auth), ("X-Amz-Date", &date)], body);
    (r.0, r.1)
}

/// Configuration files mounted the way Testcontainers' `withConfigurationOverride` and compose
/// files do: cassandra.yaml (with Cassandra 5's `class_name` form), cassandra-rackdc.properties,
/// and scylla.yaml.
#[test]
fn configuration_files() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let dir = tempfile::tempdir().unwrap();
    let cyaml = dir.path().join("cassandra.yaml");
    std::fs::write(
        &cyaml,
        "# a trimmed cassandra.yaml\ncluster_name: 'Yaml Cluster'\nnum_tokens: 2\nauthenticator:\n  class_name : org.apache.cassandra.auth.PasswordAuthenticator\n\
         authorizer: AllowAllAuthorizer\nendpoint_snitch: GossipingPropertyFileSnitch\nmaterialized_views_enabled: true  # views on\n\
         user_defined_functions_enabled: true\n",
    )
    .unwrap();
    let rackdc = dir.path().join("cassandra-rackdc.properties");
    std::fs::write(&rackdc, "# file\ndc=yaml-dc\nrack=yaml-rack\n").unwrap();
    let c = start(
        &image,
        RunOpts {
            mounts: vec![format!("{}:/etc/cassandra/cassandra.yaml:ro", cyaml.display()), format!("{}:/etc/cassandra/cassandra-rackdc.properties:ro", rackdc.display())],
            ..Default::default()
        },
    );
    rt.block_on(async {
        assert!(connect(&c.ip(), None).await.is_err(), "authenticator from cassandra.yaml");
        let cl = connect(&c.ip(), Some(("cassandra", "cassandra"))).await.unwrap();
        let id = identity(&cl).await;
        assert_eq!((id.release_version.as_str(), id.cluster_name.as_str(), id.tokens), ("5.0.9", "Yaml Cluster", 2));
        assert_eq!((id.data_center.as_str(), id.rack.as_str()), ("yaml-dc", "yaml-rack"));
        cl.query("CREATE KEYSPACE y WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}", QueryParams::default()).await.unwrap();
        cl.query("CREATE TABLE y.t (k int PRIMARY KEY, v int)", QueryParams::default()).await.unwrap();
        cl.query("CREATE MATERIALIZED VIEW y.v AS SELECT * FROM y.t WHERE v IS NOT NULL AND k IS NOT NULL PRIMARY KEY (v, k)", QueryParams::default())
            .await
            .expect("materialized_views_enabled from cassandra.yaml");
    });
    drop(c);
    let syaml = dir.path().join("scylla.yaml");
    std::fs::write(&syaml, "cluster_name: yaml-scylla\nnum_tokens: 3\nauthenticator: PasswordAuthenticator\n").unwrap();
    let c = start(&image, RunOpts { mounts: vec![format!("{}:/etc/scylla/scylla.yaml:ro", syaml.display())], ..Default::default() });
    rt.block_on(async {
        assert!(connect(&c.ip(), None).await.is_err(), "authenticator from scylla.yaml");
        let id = identity(&connect(&c.ip(), Some(("cassandra", "cassandra"))).await.unwrap()).await;
        assert_eq!((id.release_version.as_str(), id.cluster_name.as_str(), id.tokens), ("3.0.8", "yaml-scylla", 3));
    });
}

/// Data survives `docker restart` and a new container on the same volume; settings follow each
/// start's configuration (as a node's do), identity stays what it was created with.
#[test]
fn volume_persistence_and_restart() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let vol = docker::unique("roll-nosql-test-vol");
    let _cleanup = VolumeGuard(vol.clone());
    let c = start(&image, RunOpts { volume: Some(vol.clone()), args: vec!["--authenticator".into(), "PasswordAuthenticator".into()], ..Default::default() });
    rt.block_on(async {
        let cl = connect(&c.ip(), Some(("cassandra", "cassandra"))).await.unwrap();
        cl.query("CREATE KEYSPACE p WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", QueryParams::default()).await.unwrap();
        cl.query("CREATE TABLE p.t (k int PRIMARY KEY, v text)", QueryParams::default()).await.unwrap();
        cl.query("INSERT INTO p.t (k, v) VALUES (1, 'kept')", QueryParams::default()).await.unwrap();
    });
    c.restart();
    rt.block_on(async {
        let cl = connect(&c.ip(), Some(("cassandra", "cassandra"))).await.unwrap();
        assert_eq!(text(&row(&cl, "SELECT v FROM p.t WHERE k = 1").await[0]), "kept");
    });
    drop(c);
    // A new container on the volume, now without --authenticator: AllowAll again, data kept;
    // a different cluster name is reported in the log and the stored one kept.
    let c = start(&image, RunOpts { volume: Some(vol.clone()), args: vec!["--cluster-name".into(), "other".into()], ..Default::default() });
    rt.block_on(async {
        let cl = connect(&c.ip(), None).await.expect("settings follow the new start");
        assert_eq!(text(&row(&cl, "SELECT v FROM p.t WHERE k = 1").await[0]), "kept");
        assert_eq!(identity(&cl).await.cluster_name, "");
    });
    assert!(c.logs().contains("keeps its cluster name"), "{}", c.logs());
}

struct VolumeGuard(String);

impl Drop for VolumeGuard {
    fn drop(&mut self) {
        let _ = docker::docker(&["volume", "rm", "-f", &self.0]);
    }
}

/// ROLL_NOSQL_FROM: a new container seeds itself from another roll's database, by API and
/// name (`<api url>/<branch>`) or from a live cluster (`cql://`), before it serves; once.
#[test]
fn seed_from_branch() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let src = start(&image, RunOpts::default());
    let sip = src.ip();
    rt.block_on(async {
        let cl = connect(&sip, None).await.unwrap();
        cl.query("CREATE KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", QueryParams::default()).await.unwrap();
        cl.query("CREATE TABLE app.users (id int PRIMARY KEY, name text)", QueryParams::default()).await.unwrap();
        for i in 0..100 {
            cl.query(&format!("INSERT INTO app.users (id, name) VALUES ({i}, 'u{i}') USING TTL 86400"), QueryParams::default()).await.unwrap();
        }
    });
    for from in [format!("http://{sip}:7070/default"), format!("cql://{sip}:9042")] {
        let vol = docker::unique("roll-nosql-test-vol");
        let _cleanup = VolumeGuard(vol.clone());
        let c = start(&image, RunOpts { volume: Some(vol.clone()), env: vec![("ROLL_NOSQL_FROM", from.clone()), ("ROLL_NOSQL_FROM_KEYSPACES", "app".into())], ..Default::default() });
        rt.block_on(async {
            let cl = connect(&c.ip(), None).await.unwrap();
            let r = row(&cl, "SELECT count(*), max(ttl(name)) FROM app.users").await;
            assert_eq!(i64::from_be_bytes(r[0].as_deref().unwrap().try_into().unwrap()), 100, "{from}");
            let ttl = i32::from_be_bytes(r[1].as_deref().unwrap().try_into().unwrap());
            assert!(ttl > 80_000 && ttl <= 86_400, "TTLs kept: {ttl}");
            cl.query("INSERT INTO app.users (id, name) VALUES (1000, 'local')", QueryParams::default()).await.unwrap();
        });
        assert!(c.logs().contains("single-mode database seeded"), "{}", c.logs());
        // Restarted on the same volume: not seeded again, local writes kept.
        c.restart();
        rt.block_on(async {
            let cl = connect(&c.ip(), None).await.unwrap();
            let r = row(&cl, "SELECT count(*) FROM app.users").await;
            assert_eq!(i64::from_be_bytes(r[0].as_deref().unwrap().try_into().unwrap()), 101);
        });
        assert!(c.logs().contains("already seeded"), "{}", c.logs());
    }
    // An unreachable source fails the start instead of serving an empty database.
    let o = docker::docker(&["run", "--rm", "--memory", "512m", "-e", "ROLL_NOSQL_FROM=http://127.0.0.1:1/nope", &image]);
    assert!(!o.status.success(), "{o:?}");
}

/// The reference's memory settings size roll's budgets: ScyllaDB's `--memory`, Cassandra's
/// `MAX_HEAP_SIZE`, else the container's limit (1 GB here); a quarter each for memtables and
/// the block cache. Cassandra's `system_views.settings` shows the memtable space; roll's own
/// variables win.
#[test]
fn memory_budgets() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let memory = |c: &Container| -> serde_json::Value {
        c.wait_cql(Duration::from_secs(30));
        let v: serde_json::Value = serde_json::from_str(&reqwest::blocking_get(&format!("http://{}:7070/v1/nosql", c.ip()))).unwrap();
        v["memory"].clone()
    };
    let budgets = |m: &serde_json::Value| (m["memtable_budget_bytes"].as_u64().unwrap() >> 20, m["cache_memory_budget_bytes"].as_u64().unwrap() >> 20);
    let c = start(&image, args(&["--memory", "512M"]));
    assert_eq!(budgets(&memory(&c)), (128, 128));
    assert!(c.logs().contains("memory from --memory 512M: memtables 134217728, block cache 134217728"), "{}", c.logs());
    drop(c);
    let c = start(&image, env(&[("MAX_HEAP_SIZE", "4G"), ("CASSANDRA_CLUSTER_NAME", "c")]));
    // A quarter of the heap; the cache at most 512 MiB, as Cassandra's file_cache_size.
    assert_eq!(budgets(&memory(&c)), (1024, 512));
    rt.block_on(async {
        let client = connect(&c.ip(), None).await.unwrap();
        let r = row(&client, "SELECT value FROM system_views.settings WHERE name = 'memtable_heap_space'").await;
        assert_eq!(text(&r[0]), "1024MiB");
    });
    drop(c);
    // No memory settings: the container's limit.
    let c = start(&image, RunOpts::default());
    assert_eq!(budgets(&memory(&c)), (256, 256));
    drop(c);
    let c = start(&image, RunOpts { env: vec![("ROLL_NOSQL_MEMTABLE_BUDGET", "64MiB".into())], args: vec!["--memory".into(), "2G".into()], ..Default::default() });
    assert_eq!(budgets(&memory(&c)), (64, 512));
}

mod reqwest {
    //! Tiny blocking HTTP helpers over reqwest's async client.
    pub fn blocking_get(url: &str) -> String {
        super::rt().block_on(async { ::reqwest::get(url).await.unwrap().text().await.unwrap() })
    }

    pub fn blocking_post(url: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
        super::rt().block_on(async {
            let mut r = ::reqwest::Client::new().post(url).body(body.to_string());
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            let resp = r.send().await.unwrap();
            (resp.status().as_u16(), resp.text().await.unwrap())
        })
    }
}

/// TLS (`ROLL_NOSQL_TLS_CERT` / `ROLL_NOSQL_TLS_KEY`) on both CQL ports, with
/// PasswordAuthenticator and two shards: roll's client by server name, the Python drivers
/// (DataStax's, and ScyllaDB's fork over `SCYLLA_SHARD_AWARE_PORT_SSL`), Cassandra 5.0's cqlsh
/// with `--ssl` and the image's own `cqlsh --ssl`. Plain text is refused.
#[test]
fn tls_clients() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let ck = rcgen::generate_simple_self_signed(vec!["roll-tls".into(), "127.0.0.1".into()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tls.crt"), ck.cert.pem()).unwrap();
    std::fs::write(dir.path().join("tls.key"), ck.signing_key.serialize_pem()).unwrap();
    // The container's user isn't ours.
    for (p, mode) in [(dir.path().to_path_buf(), 0o755), (dir.path().join("tls.crt"), 0o644), (dir.path().join("tls.key"), 0o644)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let opts = RunOpts {
        env: vec![("ROLL_NOSQL_TLS_CERT", "/tls/tls.crt".into()), ("ROLL_NOSQL_TLS_KEY", "/tls/tls.key".into())],
        args: ["--authenticator", "PasswordAuthenticator", "--smp", "2"].iter().map(|s| s.to_string()).collect(),
        mounts: vec![format!("{}:/tls:ro", dir.path().display())],
        ..Default::default()
    };
    let c = start(&image, opts);
    let ip = c.ip();
    assert!(c.logs().contains("Starting listening for CQL clients on 0.0.0.0:9042 (encrypted, non-shard-aware)"), "{}", c.logs());
    rt.block_on(async {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.add(ck.cert.der().clone()).unwrap();
        let tls = nosql_proto::client::TlsConfig::with_roots(roots, "roll-tls").unwrap();
        let cl = Client::connect(ClientConfig::new(format!("{ip}:9042")).tls(tls.clone()).credentials("cassandra", "cassandra")).await.unwrap();
        cl.query("CREATE KEYSPACE tls WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", QueryParams::default()).await.unwrap();
        cl.query("CREATE TABLE tls.t (k int PRIMARY KEY, v text)", QueryParams::default()).await.unwrap();
        cl.query("INSERT INTO tls.t (k, v) VALUES (1, 'secret')", QueryParams::default()).await.unwrap();
        assert_eq!(cl.supported().get("SCYLLA_SHARD_AWARE_PORT_SSL").map(|v| v.join(",")).as_deref(), Some("19042"));
        assert!(cl.supported().get("SCYLLA_SHARD_AWARE_PORT").is_none(), "no plain-text shard-aware port");
        let aware = Client::connect(ClientConfig::new(format!("{ip}:19042")).tls(tls).credentials("cassandra", "cassandra")).await.unwrap();
        assert_eq!(text(&row(&aware, "SELECT v FROM tls.t WHERE k = 1").await[0]), "secret");
        assert!(connect(&ip, Some(("cassandra", "cassandra"))).await.is_err(), "plain text is refused");
    });

    let tls_mount = format!("{}:/tls:ro", dir.path().display());
    let host = format!("roll-tls:{ip}");
    let script = r#"
import ssl, time
from cassandra.cluster import Cluster
from cassandra.auth import PlainTextAuthProvider
ctx = ssl.create_default_context(cafile='/tls/tls.crt')
cluster = Cluster(['roll-tls'], ssl_context=ctx, ssl_options={'server_hostname': 'roll-tls'}, auth_provider=PlainTextAuthProvider('cassandra', 'cassandra'))
s = cluster.connect()
print('value', s.execute("SELECT v FROM tls.t WHERE k = 1").one().v)
time.sleep(2)  # the per-shard connections open in the background
rows = list(s.execute("SELECT shard_id, ssl_enabled, port FROM system.clients"))
print('ssl', all(r.ssl_enabled for r in rows), 'shards', sorted({r.shard_id for r in rows}))
cluster.shutdown()
"#;
    for (tag, shards) in [("roll-nosql-drv-python:cassandra-3.30.1", None), ("roll-nosql-drv-python:scylla-3.29.12", Some("[0, 1]"))] {
        if !docker::docker(&["image", "inspect", tag]).status.success() {
            eprintln!("{tag}: not built (drivers.rs's docker_drivers builds it); skipped");
            continue;
        }
        let o = docker::docker(&["run", "--rm", "--memory", "256m", "--add-host", &host, "-v", &tls_mount, "--entrypoint", "python", tag, "-c", script]);
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success(), "{tag}: {out}{}", String::from_utf8_lossy(&o.stderr));
        assert!(out.contains("value secret"), "{tag}: {out}");
        // DataStax's driver opens one connection; ScyllaDB's one per shard, through the
        // TLS shard-aware port.
        assert!(out.contains("ssl True shards "), "{tag}: {out}");
        if let Some(shards) = shards {
            assert!(out.contains(&format!("shards {shards}")), "{tag}: {out}");
        }
    }

    // Cassandra 5.0's cqlsh, validating the certificate.
    let o = docker::docker(&[
        "run", "--rm", "--memory", "256m", "--add-host", &host, "-v", &tls_mount, "-e", "SSL_CERTFILE=/tls/tls.crt", "--entrypoint", "cqlsh", "cassandra:5.0.9",
        "--ssl", "-u", "cassandra", "-p", "cassandra", "roll-tls", "-e", "SELECT v FROM tls.t WHERE k = 1",
    ]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && out.contains("secret"), "{out}{}", String::from_utf8_lossy(&o.stderr));
    // Without --ssl it can't connect.
    let o = docker::docker(&["run", "--rm", "--memory", "256m", "--add-host", &host, "--entrypoint", "cqlsh", "cassandra:5.0.9", "-u", "cassandra", "-p", "cassandra", "--connect-timeout", "3", "roll-tls", "-e", "SELECT v FROM tls.t"]);
    assert!(!o.status.success());
    // The image's cqlsh (Testcontainers runs it in the container).
    let o = c.exec(&["env", "SSL_CERTFILE=/tls/tls.crt", "cqlsh", "--ssl", "--server-name", "roll-tls", "-u", "cassandra", "-p", "cassandra", "-e", "SELECT v FROM tls.t WHERE k = 1"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && out.contains("secret"), "{out}{}", String::from_utf8_lossy(&o.stderr));
}

/// A CA, the server certificate for `roll-tls` and client certificates, as PEM files in a
/// directory the container's user can read.
struct TlsFiles {
    dir: tempfile::TempDir,
    ca: rcgen::Issuer<'static, rcgen::KeyPair>,
    ca_der: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
}

impl TlsFiles {
    fn new() -> TlsFiles {
        use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair};
        let dir = tempfile::tempdir().unwrap();
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, "roll image CA");
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let ca_cert = p.self_signed(&key).unwrap();
        let ca = Issuer::new(p, key);
        let sk = KeyPair::generate().unwrap();
        let server = CertificateParams::new(vec!["roll-tls".into()]).unwrap().signed_by(&sk, &ca).unwrap();
        let f = TlsFiles { dir, ca, ca_der: ca_cert.der().clone() };
        f.write("ca.pem", &ca_cert.pem());
        f.write("server.crt", &server.pem());
        f.write("server.key", &sk.serialize_pem());
        // Cassandra's PEM keystore: the key and its certificate chain in one file.
        f.write("keystore.pem", &format!("{}{}", sk.serialize_pem(), server.pem()));
        f
    }

    fn write(&self, name: &str, content: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(self.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// A client: TLS to `roll-tls` trusting the CA, presenting a certificate for `cn` (with a
    /// SPIFFE id) or none.
    fn client(&self, addr: &str, cn: Option<&str>) -> ClientConfig {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
        use tokio_rustls::rustls;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca_der.clone()).unwrap();
        let b = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots);
        let config = match cn {
            Some(cn) => {
                let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
                p.distinguished_name = DistinguishedName::new();
                p.distinguished_name.push(DnType::OrganizationName, "roll");
                p.distinguished_name.push(DnType::CommonName, cn);
                p.subject_alt_names = vec![SanType::URI(format!("spiffe://roll.test/{cn}").try_into().unwrap())];
                let key = KeyPair::generate().unwrap();
                let cert = p.signed_by(&key, &self.ca).unwrap();
                let der = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
                b.with_client_auth_cert(vec![cert.der().clone()], der).unwrap()
            }
            None => b.with_no_client_auth(),
        };
        let tls = nosql_proto::client::TlsConfig { config: std::sync::Arc::new(config), server_name: "roll-tls".into() };
        let mut c = ClientConfig::new(addr.to_string()).tls(tls);
        c.connect_timeout = Duration::from_secs(5);
        c
    }
}

/// `docker run` that should fail at start: its output.
fn refused(image: &str, mounts: &[String]) -> String {
    let mut a = vec!["run".to_string(), "--rm".into(), "--memory".into(), "256m".into()];
    for m in mounts {
        a.push("-v".into());
        a.push(m.clone());
    }
    a.push(image.into());
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let o = docker::docker(&a);
    assert!(!o.status.success(), "the container should exit");
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// TLS and client-certificate authentication from a mounted cassandra.yaml
/// (`client_encryption_options` with PEMBasedSslContextFactory key material, `optional`,
/// `require_client_auth`, MutualTlsWithPasswordFallbackAuthenticator; a JKS keystore and an
/// mTLS authenticator without client auth refused at start) and scylla.yaml (`certificate`,
/// `keyfile`, `truststore`, `require_client_auth`, `native_transport_port_ssl`,
/// `native_shard_aware_transport_port_ssl`, CertificateAuthenticator with
/// `auth_certificate_role_queries`).
#[test]
fn tls_from_configuration_files() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let rt = rt();
    let f = TlsFiles::new();
    let certs = format!("{}:/certs:ro", f.dir.path().display());
    let cassandra_yaml = |auth: &str, keystore: &str, client_auth: bool| {
        format!(
            "authenticator:\n  class_name : org.apache.cassandra.auth.{auth}\n  parameters :\n    validator_class_name: org.apache.cassandra.auth.SpiffeCertificateValidator\n\
             client_encryption_options:\n  enabled: true\n  optional: true\n  keystore: {keystore}\n  truststore: /certs/ca.pem\n  require_client_auth: {client_auth}\n\
             \x20 ssl_context_factory:\n    class_name: org.apache.cassandra.security.PEMBasedSslContextFactory\n"
        )
    };
    f.write("cassandra.yaml", &cassandra_yaml("MutualTlsWithPasswordFallbackAuthenticator", "/certs/keystore.pem", true));
    let c = start(&image, RunOpts { mounts: vec![certs.clone(), format!("{}:/etc/cassandra/cassandra.yaml:ro", f.path("cassandra.yaml"))], ..Default::default() });
    let ip = c.ip();
    assert!(c.logs().contains("TLS from client_encryption_options: certificate /certs/keystore.pem, client certificates true"), "{}", c.logs());
    rt.block_on(async {
        // `optional`: plain text with a password on the same port.
        let cl = connect(&ip, Some(("cassandra", "cassandra"))).await.unwrap();
        cl.query("CREATE ROLE alice WITH LOGIN = true", QueryParams::default()).await.unwrap();
        cl.query("ADD IDENTITY 'spiffe://roll.test/alice' TO ROLE 'alice'", QueryParams::default()).await.unwrap();
        // A certificate: its identity's role, whatever the credentials.
        let tls = Client::connect(f.client(&format!("{ip}:9042"), Some("alice")).credentials("any", "thing")).await.unwrap();
        let users = match tls.query("SELECT username, ssl_enabled FROM system_views.clients", QueryParams::default()).await.unwrap() {
            ResultBody::Rows(r) => r.to_vec(),
            other => panic!("{other:?}"),
        };
        assert!(users.iter().any(|r| text(&r[0]) == "alice" && r[1].as_deref() == Some(&[1u8][..])), "{users:?}");
        let e = Client::connect(f.client(&format!("{ip}:9042"), Some("mallory")).credentials("cassandra", "cassandra")).await.unwrap_err();
        assert!(e.to_string().contains("Certificate identity 'spiffe://roll.test/mallory' not authorized"), "{e}");
        // require_client_auth: TLS without a certificate is refused.
        assert!(Client::connect(f.client(&format!("{ip}:9042"), None).credentials("cassandra", "cassandra")).await.is_err());
    });
    drop(c);
    // Refused at start, as Cassandra refuses to start.
    f.write("bad.yaml", &cassandra_yaml("MutualTlsAuthenticator", "/certs/keystore.pem", false));
    let out = refused(&image, &[certs.clone(), format!("{}:/etc/cassandra/cassandra.yaml:ro", f.path("bad.yaml"))]);
    assert!(out.contains("MutualTlsAuthenticator requires client_encryption_options.enabled to be true & client_encryption_options.require_client_auth to be true"), "{out}");
    f.write("keystore.jks", "\u{fe}\u{ed}\u{fe}\u{ed} not pem");
    f.write("jks.yaml", &cassandra_yaml("MutualTlsAuthenticator", "/certs/keystore.jks", true));
    let out = refused(&image, &[certs.clone(), format!("{}:/etc/cassandra/cassandra.yaml:ro", f.path("jks.yaml"))]);
    assert!(out.contains("client_encryption_options.keystore /certs/keystore.jks is not PEM") && out.contains("keytool -importkeystore"), "{out}");

    // ScyllaDB: dedicated TLS ports beside the plain ones, CertificateAuthenticator.
    f.write(
        "scylla.yaml",
        "authenticator: com.scylladb.auth.CertificateAuthenticator\nauth_certificate_role_queries:\n  - source: SUBJECT\n    query: CN=([^,\\s]+)\n\
         native_transport_port_ssl: 9142\nnative_shard_aware_transport_port_ssl: 19142\n\
         client_encryption_options:\n  enabled: true\n  certificate: /certs/server.crt\n  keyfile: /certs/server.key\n  truststore: /certs/ca.pem\n  require_client_auth: true\n",
    );
    let c = start(&image, RunOpts { mounts: vec![certs, format!("{}:/etc/scylla/scylla.yaml:ro", f.path("scylla.yaml"))], ..Default::default() });
    let ip = c.ip();
    let logs = c.logs();
    assert!(logs.contains("on 0.0.0.0:9042 (unencrypted, non-shard-aware)") && logs.contains("on 0.0.0.0:9142 (encrypted, non-shard-aware)"), "{logs}");
    assert!(logs.contains("on 0.0.0.0:19142 (encrypted, shard-aware)"), "{logs}");
    rt.block_on(async {
        // The superuser's certificate (CN=cassandra) is ready without SASL.
        let admin = Client::connect(f.client(&format!("{ip}:9142"), Some("cassandra"))).await.unwrap();
        assert_eq!(admin.supported().get("SCYLLA_SHARD_AWARE_PORT_SSL").map(|v| v.join(",")).as_deref(), Some("19142"));
        assert_eq!(admin.supported().get("SCYLLA_SHARD_AWARE_PORT").map(|v| v.join(",")).as_deref(), Some("19042"));
        admin.query("CREATE ROLE alice WITH LOGIN = true", QueryParams::default()).await.unwrap();
        let alice = Client::connect(f.client(&format!("{ip}:19142"), Some("alice"))).await.unwrap();
        assert_eq!(text(&row(&alice, "SELECT role FROM system.roles WHERE role = 'alice'").await[0]), "alice");
        let e = Client::connect(f.client(&format!("{ip}:9142"), Some("bob"))).await.unwrap_err();
        assert!(e.to_string().contains("bob is not permitted to log in"), "{e}");
        // The plain port asks for a certificate it can't get.
        assert!(matches!(connect(&ip, None).await, Err(ClientError::Auth(_))));
        assert!(Client::connect(f.client(&format!("{ip}:9142"), None)).await.is_err(), "require_client_auth");
    });
}
