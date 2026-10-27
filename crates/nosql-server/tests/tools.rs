//! Tools against the `roll/nosql` image, unchanged (docs/nosql/compatibility.md, layer 5):
//! Testcontainers' Cassandra and ScyllaDB modules for Python, Java, Go and Node with the roll
//! image swapped in; golang-migrate's cassandra driver; cassandra-stress from the Cassandra
//! image; scylla-bench; and the cqlsh of both reference images (DESCRIBE, SELECT, COPY TO/FROM),
//! whose output must match what it prints against the reference.
//!
//! Needs ROLL_NOSQL_IMAGE; one tool container at a time. The cqlsh comparison also needs the
//! shared `nosql-ref-scylla` / `nosql-ref-cassandra` containers (it skips a dialect without).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::docker::{self, Container, RunOpts};

fn tools() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tools")
}

fn out(o: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// `docker run --rm` of a tool container: ≤ 1.5 GB, on the default bridge unless `extra` says.
fn tool(extra: &[&str], image: &str, args: &[&str]) -> std::process::Output {
    docker::wait_mem(6);
    let mut a = vec!["run", "--rm", "--memory", "1536m"];
    a.extend_from_slice(extra);
    a.push(image);
    a.extend_from_slice(args);
    docker::docker(&a)
}

fn cqlsh_in(c: &Container, cql: &str) {
    let o = c.exec(&["cqlsh", "-e", cql]);
    assert!(o.status.success(), "{cql}: {}", out(&o));
}

/// A Testcontainers project: it starts the roll image through the Docker socket (host network,
/// so mapped ports are on localhost), runs its checks and prints PASS/FAIL lines. Containers it
/// leaves behind are removed.
fn testcontainers(dir: &str, tag: &str) {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    docker::build(&tools().join(dir), tag, &[]);
    let before = docker::docker(&["ps", "-aq", "--filter", &format!("ancestor={image}")]).stdout;
    let o = tool(
        &[
            "--network", "host", "-v", "/var/run/docker.sock:/var/run/docker.sock", "-e", &format!("ROLL_NOSQL_IMAGE={image}"),
            "-e", "TESTCONTAINERS_HOST_OVERRIDE=localhost", "-e", "TC_HOST=localhost", "-e", "TESTCONTAINERS_RYUK_DISABLED=true",
        ],
        tag,
        &[],
    );
    let after = docker::docker(&["ps", "-aq", "--filter", &format!("ancestor={image}")]).stdout;
    let before = String::from_utf8_lossy(&before).to_string();
    for id in String::from_utf8_lossy(&after).split_whitespace().filter(|id| !before.contains(id)) {
        let _ = docker::docker(&["rm", "-f", "-v", id]);
    }
    let text = out(&o);
    eprintln!("{}", text.lines().filter(|l| l.starts_with("PASS") || l.starts_with("FAIL")).collect::<Vec<_>>().join("\n"));
    assert!(o.status.success() && text.contains("PASS") && !text.lines().any(|l| l.starts_with("FAIL")), "{text}");
}

#[test]
fn testcontainers_python() {
    testcontainers("testcontainers-python", "roll-nosql-tool-tc-python:4.15.0");
}

#[test]
fn testcontainers_java() {
    testcontainers("testcontainers-java", "roll-nosql-tool-tc-java:2.0.5");
}

#[test]
fn testcontainers_go() {
    testcontainers("testcontainers-go", "roll-nosql-tool-tc-go:0.44.0");
}

#[test]
fn testcontainers_node() {
    testcontainers("testcontainers-node", "roll-nosql-tool-tc-node:12.2.1");
}

/// golang-migrate (cassandra driver): up, version table, down, in both dialects.
#[test]
fn golang_migrate() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let mig = tools().join("migrations");
    for opts in [RunOpts { env: vec![("CASSANDRA_CLUSTER_NAME", "mig".into())], ..Default::default() }, RunOpts { args: vec!["--smp".into(), "1".into()], ..Default::default() }] {
        let c = Container::run(&image, &opts);
        c.wait_cql(Duration::from_secs(30));
        cqlsh_in(&c, "CREATE KEYSPACE mig WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}");
        let db = format!("cassandra://{}:9042/mig?x-multi-statement=true", c.ip());
        let mount = format!("{}:/migrations:ro", mig.display());
        let up = tool(&["-v", &mount], "migrate/migrate:v4.18.3", &["-path=/migrations", "-database", &db, "up"]);
        assert!(up.status.success(), "migrate up: {}", out(&up));
        let v = tool(&["-v", &mount], "migrate/migrate:v4.18.3", &["-path=/migrations", "-database", &db, "version"]);
        assert_eq!(out(&v).trim(), "3", "migrate version");
        let o = c.exec(&["cqlsh", "-e", "SELECT kind FROM mig.events; SELECT version, dirty FROM mig.schema_migrations; DESCRIBE TABLE mig.users"]);
        let s = out(&o);
        assert!(s.contains("migrated") && s.contains("tags set<text>") && s.contains("address frozen<address>") && s.contains("False"), "{s}");
        let down = tool(&["-v", &mount], "migrate/migrate:v4.18.3", &["-path=/migrations", "-database", &db, "down", "-all"]);
        assert!(down.status.success(), "migrate down: {}", out(&down));
        let o = c.exec(&["cqlsh", "-e", "DESCRIBE KEYSPACE mig"]);
        let s = out(&o);
        assert!(!s.contains("CREATE TABLE mig.users") && !s.contains("CREATE TYPE"), "{s}");
    }
}

/// cassandra-stress (from cassandra:5.0.9): a write then a read phase over CQL.
#[test]
fn cassandra_stress() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let c = Container::run(&image, &RunOpts { env: vec![("CASSANDRA_CLUSTER_NAME", "stress".into())], ..Default::default() });
    c.wait_cql(Duration::from_secs(30));
    let ip = c.ip();
    for phase in ["write", "read"] {
        let o = tool(
            &["-e", "JVM_OPTS=-Xmx512m", "--entrypoint", "/opt/cassandra/tools/bin/cassandra-stress"],
            docker_ref_image("cassandra"),
            &[phase, "n=20000", "no-warmup", "cl=ONE", "-rate", "threads=8", "-node", &ip, "-mode", "native", "cql3", "protocolVersion=4"],
        );
        let s = out(&o);
        eprintln!("cassandra-stress {phase}: {}", s.lines().filter(|l| l.starts_with("Op rate") || l.starts_with("Latency mean")).collect::<Vec<_>>().join("; "));
        assert!(o.status.success() && s.contains("Op rate") && s.contains("END") && !s.contains("FAILURE"), "cassandra-stress {phase}: {s}");
    }
}

fn docker_ref_image(dialect: &str) -> &'static str {
    if dialect == "scylla" { common::SCYLLA_IMAGE } else { common::CASSANDRA_IMAGE }
}

/// scylla-bench (shard-aware gocql): a sequential write of every partition, then timed reads.
#[test]
fn scylla_bench() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    docker::build(&tools().join("scylla-bench"), "roll-nosql-tool-scylla-bench:1.3.3", &[]);
    let c = Container::run(&image, &RunOpts { args: vec!["--smp".into(), "2".into(), "--developer-mode".into(), "1".into()], ..Default::default() });
    c.wait_cql(Duration::from_secs(30));
    let ip = c.ip();
    let common = ["-nodes", ip.as_str(), "-replication-factor", "1", "-partition-count", "2000", "-clustering-row-count", "5", "-concurrency", "8"];
    let mut write = vec!["-workload", "sequential", "-mode", "write"];
    write.extend_from_slice(&common);
    let o = tool(&[], "roll-nosql-tool-scylla-bench:1.3.3", &write);
    let s = out(&o);
    assert!(o.status.success() && s.contains("Results") && !s.to_lowercase().contains("error:"), "scylla-bench write: {s}");
    let mut read = vec!["-workload", "uniform", "-mode", "read", "-duration", "5s"];
    read.extend_from_slice(&common);
    let o = tool(&[], "roll-nosql-tool-scylla-bench:1.3.3", &read);
    let s = out(&o);
    eprintln!("scylla-bench read: {}", s.lines().filter(|l| l.contains("Operations/s") || l.contains("Rows/s")).collect::<Vec<_>>().join("; "));
    assert!(o.status.success() && s.contains("Results"), "scylla-bench read: {s}");
    // The per-second lines: `time ops/s rows/s errors …`.
    let seconds: Vec<Vec<&str>> = s.lines().map(|l| l.split_whitespace().collect::<Vec<_>>()).filter(|f| f.len() > 4 && f[0].ends_with('s') && f[0].trim_end_matches('s').parse::<u32>().is_ok()).collect();
    assert!(!seconds.is_empty() && seconds.iter().all(|f| f[3] == "0"), "read errors: {s}");
}

/// The cqlsh of each reference image, run against roll and against the reference: DESCRIBE,
/// SELECT and a COPY TO / TRUNCATE / COPY FROM round trip must print the same.
#[test]
fn cqlsh_from_reference_images() {
    let Some(image) = docker::roll_image() else { return eprintln!("skipped: set ROLL_NOSQL_IMAGE") };
    let _g = docker::serial();
    let ks = format!("cqlsh_tool_{}", std::process::id());
    let schema = format!(
        "CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}};\
         CREATE TYPE {ks}.pt (x int, y int);\
         CREATE TABLE {ks}.t (k int, c text, v map<text, int>, p frozen<pt>, d decimal, PRIMARY KEY (k, c)) WITH comment = 'cqlsh' AND CLUSTERING ORDER BY (c DESC);\
         INSERT INTO {ks}.t (k, c, v, p, d) VALUES (1, 'a', {{'x': 1}}, {{x: 1, y: 2}}, 1.50);\
         INSERT INTO {ks}.t (k, c, v, p, d) VALUES (1, 'b', {{'y': 2}}, {{x: 3, y: 4}}, -0.001);\
         INSERT INTO {ks}.t (k, c) VALUES (2, 'z');"
    );
    let script = format!(
        "DESCRIBE KEYSPACE {ks}; SELECT * FROM {ks}.t; COPY {ks}.t TO '/tmp/t.csv' WITH HEADER = true; TRUNCATE {ks}.t; \
         COPY {ks}.t FROM '/tmp/t.csv' WITH HEADER = true; SELECT * FROM {ks}.t; SELECT k, c, d FROM {ks}.t WHERE k = 1 ORDER BY c ASC;"
    );
    // COPY's progress lines carry rates and timings.
    let normal = |s: &str| {
        s.lines()
            .filter(|l| !(l.contains("rows/s") || l.contains("Rate") || l.contains("seconds") || l.starts_with("Processed") || l.starts_with("Starting copy") || l.contains("child process")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut skipped = 0;
    for (dialect, reference, opts) in [
        ("cassandra", "nosql-ref-cassandra", RunOpts { env: vec![("CASSANDRA_CLUSTER_NAME", "Test Cluster".into())], ..Default::default() }),
        ("scylla", "nosql-ref-scylla", RunOpts::default()),
    ] {
        let Some(ref_ip) = docker::reference(reference) else {
            skipped += 1;
            eprintln!("skipped {dialect}: {reference} is not running");
            continue;
        };
        let c = Container::run(&image, &opts);
        c.wait_cql(Duration::from_secs(30));
        let cqlsh = |ip: &str, cql: &str| {
            let o = tool(&["--entrypoint", "cqlsh"], docker_ref_image(dialect), &[ip, "9042", "--request-timeout", "60", "-e", cql]);
            (o.status.success(), out(&o))
        };
        let mut outputs = Vec::new();
        for ip in [ref_ip.as_str(), &c.ip()] {
            let (ok, s) = cqlsh(ip, &schema);
            assert!(ok, "{dialect} schema on {ip}: {s}");
            let (ok, s) = cqlsh(ip, &script);
            assert!(ok, "{dialect} script on {ip}: {s}");
            let _ = cqlsh(ip, &format!("DROP KEYSPACE {ks}"));
            outputs.push(normal(&s));
        }
        assert!(outputs[1].contains("CREATE TABLE") && outputs[1].contains("(3 rows)"), "{}", outputs[1]);
        assert_eq!(outputs[1], outputs[0], "{dialect}: cqlsh prints differently against roll (left) than against {reference} (right)");
    }
    assert!(skipped < 2, "no reference container running");
}
