//! The Apache Kafka distribution's own tools and Java client against the broker, in Docker:
//! topics, console clients, consumer groups, configs, perf tools (idempotent, `acks=all`),
//! transactions, verifiable clients, SASL/SCRAM, TLS and a Kafka Streams app with
//! `exactly_once_v2`.
//!
//! Needs Docker, so it only runs with `ROLL_E2E=1` (`scripts/test-stream.sh` sets it).
//! - `ROLL_E2E_BIND=0.0.0.0` where containers can't reach the host's loopback (Linux)
//! - `ROLL_E2E_KAFKA_IMAGE` to pick another `apache/kafka` version

mod common;

use common::TestBroker;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn image() -> String {
    std::env::var("ROLL_E2E_KAFKA_IMAGE").unwrap_or_else(|_| "apache/kafka:4.1.0".into())
}

macro_rules! e2e {
    () => {
        if std::env::var("ROLL_E2E").is_err() {
            eprintln!("skipping: set ROLL_E2E=1 to run the Java client tests");
            return;
        }
    };
}

async fn broker(name: &str) -> TestBroker {
    TestBroker::start_with(name, |c| {
        let bind = std::env::var("ROLL_E2E_BIND").unwrap_or_else(|_| "127.0.0.1".into());
        c.listen = format!("{bind}:0").parse().unwrap();
        c.advertised_host = "host.docker.internal".into();
    })
    .await
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Out {
    fn lines(&self) -> Vec<&str> {
        self.stdout.lines().filter(|l| !l.trim().is_empty()).collect()
    }
}

/// Run a tool from the Kafka image. `tool` is a script in `/opt/kafka/bin` (or a full command
/// line for `sh -c` when it contains spaces).
struct Run<'a> {
    tb: &'a TestBroker,
    args: Vec<String>,
    stdin: Option<String>,
    mounts: Vec<(PathBuf, String)>,
    bootstrap: bool,
    root_shell: Option<String>,
}

fn kafka<'a>(tb: &'a TestBroker, tool: &str) -> Run<'a> {
    Run { tb, args: vec![format!("/opt/kafka/bin/{tool}")], stdin: None, mounts: vec![], bootstrap: true, root_shell: None }
}

impl<'a> Run<'a> {
    fn args(mut self, a: &[&str]) -> Self {
        self.args.extend(a.iter().map(|s| s.to_string()));
        self
    }
    fn stdin(mut self, s: impl Into<String>) -> Self {
        self.stdin = Some(s.into());
        self
    }
    fn mount(mut self, host: &Path, at: &str) -> Self {
        self.mounts.push((host.to_path_buf(), at.into()));
        self
    }
    fn no_bootstrap(mut self) -> Self {
        self.bootstrap = false;
        self
    }
    /// Run as root under `sh -c`, with `setup` first (e.g. installing a library).
    fn root_setup(mut self, setup: &str) -> Self {
        self.root_shell = Some(setup.into());
        self
    }

    async fn run(self) -> Out {
        let port = self.tb.addr.rsplit(':').next().unwrap().to_string();
        let mut cmd = Command::new("docker");
        cmd.args(["run", "--rm", "-i", "--add-host=host.docker.internal:host-gateway"]);
        for (host, at) in &self.mounts {
            cmd.arg("-v").arg(format!("{}:{at}:ro", host.display()));
        }
        let mut args = self.args;
        if self.bootstrap {
            args.insert(1, "--bootstrap-server".into());
            args.insert(2, format!("host.docker.internal:{port}"));
        }
        match self.root_shell {
            Some(setup) => {
                let line = args.iter().map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
                cmd.args(["--user", "root", &image(), "sh", "-c", &format!("{setup} >/dev/null 2>&1; {line}")]);
            }
            None => {
                cmd.arg(image());
                cmd.args(&args);
            }
        }
        let stdin = self.stdin;
        tokio::task::spawn_blocking(move || {
            cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = cmd.spawn().expect("docker");
            if let Some(input) = stdin {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            }
            let out = child.wait_with_output().unwrap();
            Out { ok: out.status.success(), stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::from_utf8_lossy(&out.stderr).into_owned() }
        })
        .await
        .unwrap()
    }
}

fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn topics_tool() {
    e2e!();
    let tb = broker("java-topics").await;
    let out = kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "t1", "--partitions", "3", "--replication-factor", "3", "--config", "retention.ms=3600000"]).run().await;
    assert!(out.stdout.contains("Created topic t1"), "{}", out.stderr);
    let out = kafka(&tb, "kafka-topics.sh").args(&["--describe", "--topic", "t1"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    assert!(out.stdout.contains("PartitionCount: 3"), "{}", out.stdout);
    assert!(out.stdout.contains("retention.ms=3600000"), "{}", out.stdout);
    assert_eq!(out.stdout.matches("Leader: 1").count(), 3);
    let out = kafka(&tb, "kafka-topics.sh").args(&["--alter", "--topic", "t1", "--partitions", "5"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-topics.sh").args(&["--describe", "--topic", "t1"]).run().await;
    assert!(out.stdout.contains("PartitionCount: 5"), "{}", out.stdout);
    let out = kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "t1", "--partitions", "1"]).run().await;
    assert!(out.stdout.contains("already exists") || out.stderr.contains("already exists"), "{}{}", out.stdout, out.stderr);
    let out = kafka(&tb, "kafka-topics.sh").args(&["--list"]).run().await;
    assert_eq!(out.lines(), vec!["t1"]);
    let out = kafka(&tb, "kafka-topics.sh").args(&["--delete", "--topic", "t1"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-topics.sh").args(&["--list"]).run().await;
    assert!(out.lines().is_empty(), "{}", out.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn console_clients_and_groups_tool() {
    e2e!();
    let tb = broker("java-console").await;
    kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "lines", "--partitions", "3"]).run().await;
    let input: String = (0..300).map(|i| format!("k{i}:v{i}\n")).collect();
    let out = kafka(&tb, "kafka-console-producer.sh").args(&["--topic", "lines", "--property", "parse.key=true", "--property", "key.separator=:"]).stdin(input).run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-console-consumer.sh")
        .args(&["--topic", "lines", "--from-beginning", "--group", "cg", "--max-messages", "300", "--timeout-ms", "20000", "--property", "print.key=true", "--property", "key.separator=="])
        .run()
        .await;
    let mut got: Vec<&str> = out.lines();
    got.sort();
    let mut want: Vec<String> = (0..300).map(|i| format!("k{i}=v{i}")).collect();
    want.sort();
    assert_eq!(got, want.iter().map(String::as_str).collect::<Vec<_>>(), "{}", out.stderr);

    let out = kafka(&tb, "kafka-consumer-groups.sh").args(&["--describe", "--group", "cg"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let lag: i64 = out.lines().iter().filter(|l| l.starts_with("cg ")).map(|l| l.split_whitespace().nth(5).unwrap().parse::<i64>().unwrap()).sum();
    assert_eq!(lag, 0, "{}", out.stdout);
    let out = kafka(&tb, "kafka-consumer-groups.sh").args(&["--group", "cg", "--topic", "lines", "--reset-offsets", "--to-earliest", "--execute"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-consumer-groups.sh").args(&["--describe", "--group", "cg"]).run().await;
    let lag: i64 = out.lines().iter().filter(|l| l.starts_with("cg ")).map(|l| l.split_whitespace().nth(5).unwrap().parse::<i64>().unwrap()).sum();
    assert_eq!(lag, 300, "{}", out.stdout);
    let out = kafka(&tb, "kafka-consumer-groups.sh").args(&["--list"]).run().await;
    assert_eq!(out.lines(), vec!["cg"]);
    let out = kafka(&tb, "kafka-consumer-groups.sh").args(&["--delete", "--group", "cg"]).run().await;
    assert!(out.stdout.contains("successful"), "{}{}", out.stdout, out.stderr);
    let out = kafka(&tb, "kafka-consumer-groups.sh").args(&["--list"]).run().await;
    assert!(out.lines().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn perf_tools_idempotent_acks_all() {
    e2e!();
    let tb = broker("java-perf").await;
    kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "perf", "--partitions", "6"]).run().await;
    let port = tb.addr.rsplit(':').next().unwrap().to_string();
    let props = format!("bootstrap.servers=host.docker.internal:{port}");
    let out = kafka(&tb, "kafka-producer-perf-test.sh")
        .no_bootstrap()
        .args(&["--topic", "perf", "--num-records", "200000", "--record-size", "256", "--throughput", "-1", "--producer-props", &props, "acks=all", "enable.idempotence=true", "compression.type=lz4"])
        .run()
        .await;
    assert!(out.stdout.contains("200000 records sent"), "{}{}", out.stdout, out.stderr);
    let hw: i64 = (0..6).map(|p| tb.broker.find_stream("test").unwrap().topic("perf").unwrap().partition(p).unwrap().loaded().map(|l| l.high_watermark()).unwrap_or(0)).sum();
    assert_eq!(hw, 200_000, "exactly once per record");
    let out = kafka(&tb, "kafka-consumer-perf-test.sh").args(&["--topic", "perf", "--messages", "200000", "--group", "perf", "--timeout", "30000"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let row = out.lines().last().unwrap().to_string();
    assert_eq!(row.split(", ").nth(4), Some("200000"), "{row}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configs_offsets_and_records_tools() {
    e2e!();
    let tb = broker("java-configs").await;
    kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "c", "--partitions", "2"]).run().await;
    let out = kafka(&tb, "kafka-configs.sh").args(&["--entity-type", "topics", "--entity-name", "c", "--alter", "--add-config", "retention.ms=86400000,max.message.bytes=2000000"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-configs.sh").args(&["--entity-type", "topics", "--entity-name", "c", "--describe"]).run().await;
    assert!(out.stdout.contains("retention.ms=86400000") && out.stdout.contains("max.message.bytes=2000000"), "{}", out.stdout);
    let out = kafka(&tb, "kafka-configs.sh").args(&["--entity-type", "topics", "--entity-name", "c", "--alter", "--delete-config", "max.message.bytes"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-configs.sh").args(&["--entity-type", "topics", "--entity-name", "c", "--describe"]).run().await;
    assert!(!out.stdout.contains("max.message.bytes"), "{}", out.stdout);
    let out = kafka(&tb, "kafka-configs.sh").args(&["--entity-type", "topics", "--entity-name", "c", "--alter", "--add-config", "retention.ms=soon"]).run().await;
    assert!(!out.ok || out.stderr.contains("Invalid"), "bad values are rejected: {}", out.stdout);
    let out = kafka(&tb, "kafka-configs.sh").args(&["--entity-type", "brokers", "--entity-name", "1", "--describe", "--all"]).run().await;
    assert!(out.stdout.contains("num.partitions=1"), "{}", out.stdout);

    let input: String = (0..100).map(|i| format!("{i}\n")).collect();
    kafka(&tb, "kafka-console-producer.sh").args(&["--topic", "c", "--producer-property", "partitioner.class=org.apache.kafka.clients.producer.RoundRobinPartitioner"]).stdin(input).run().await;
    let out = kafka(&tb, "kafka-get-offsets.sh").args(&["--topic", "c"]).run().await;
    let total: i64 = out.lines().iter().map(|l| l.rsplit(':').next().unwrap().parse::<i64>().unwrap()).sum();
    assert_eq!(total, 100, "{}", out.stdout);

    let dir = common::temp_dir("java-dr");
    let f = write(&dir, "dr.json", &json!({ "version": 1, "partitions": [{ "topic": "c", "partition": 0, "offset": 10 }] }).to_string());
    let out = kafka(&tb, "kafka-delete-records.sh").args(&["--offset-json-file", "/dr.json"]).mount(&f, "/dr.json").run().await;
    assert!(out.stdout.contains("low_watermark: 10"), "{}{}", out.stdout, out.stderr);
    let out = kafka(&tb, "kafka-get-offsets.sh").args(&["--topic", "c", "--partitions", "0", "--time", "-2"]).run().await;
    assert_eq!(out.lines(), vec!["c:0:10"]);

    let out = kafka(&tb, "kafka-log-dirs.sh").args(&["--describe", "--topic-list", "c"]).run().await;
    let json_line = out.lines().into_iter().find(|l| l.starts_with('{')).unwrap_or_default().to_string();
    let v: Value = serde_json::from_str(&json_line).unwrap_or(Value::Null);
    assert_eq!(v["brokers"][0]["logDirs"][0]["partitions"].as_array().map(|a| a.len()), Some(2), "{}", out.stdout);

    let out = kafka(&tb, "kafka-broker-api-versions.sh").run().await;
    assert!(out.stdout.contains("Produce(0)") && out.stdout.contains("Fetch(1)"), "{}", out.stdout);
    let out = kafka(&tb, "kafka-metadata-quorum.sh").args(&["describe", "--status"]).run().await;
    assert!(out.stdout.contains("LeaderId:") && out.stdout.contains(&format!("ClusterId:              {}", tb.broker.find_stream("test").unwrap().cluster_id)), "{}{}", out.stdout, out.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transactions_and_verifiable_clients() {
    e2e!();
    let tb = broker("java-txn").await;
    kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "tx", "--partitions", "3"]).run().await;
    let port = tb.addr.rsplit(':').next().unwrap().to_string();
    let props = format!("bootstrap.servers=host.docker.internal:{port}");
    let out = kafka(&tb, "kafka-producer-perf-test.sh")
        .no_bootstrap()
        .args(&["--topic", "tx", "--num-records", "20000", "--record-size", "100", "--throughput", "-1", "--transactional-id", "perf-tx", "--transaction-duration-ms", "100", "--producer-props", &props])
        .run()
        .await;
    assert!(out.stdout.contains("20000 records sent"), "{}{}", out.stdout, out.stderr);
    let out = kafka(&tb, "kafka-transactions.sh").args(&["list"]).run().await;
    assert!(out.stdout.contains("perf-tx") && out.stdout.contains("CompleteCommit"), "{}{}", out.stdout, out.stderr);
    let out = kafka(&tb, "kafka-transactions.sh").args(&["describe", "--transactional-id", "perf-tx"]).run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-transactions.sh").args(&["describe-producers", "--topic", "tx", "--partition", "0"]).run().await;
    assert!(out.ok && out.lines().len() >= 2, "{}{}", out.stdout, out.stderr);
    // read_committed sees all 20000 records (markers aren't records).
    let out = kafka(&tb, "kafka-console-consumer.sh").args(&["--topic", "tx", "--from-beginning", "--max-messages", "20000", "--timeout-ms", "20000", "--isolation-level", "read_committed"]).run().await;
    assert_eq!(out.lines().len(), 20000, "{}", out.stderr);

    let out = kafka(&tb, "kafka-verifiable-producer.sh").args(&["--topic", "v", "--max-messages", "3000", "--acks", "-1"]).run().await;
    let summary: Value = out.lines().iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find(|v| v["name"] == "tool_data").unwrap_or(Value::Null);
    assert_eq!((summary["sent"].as_i64(), summary["acked"].as_i64()), (Some(3000), Some(3000)), "{}", out.stdout);
    let out = kafka(&tb, "kafka-verifiable-consumer.sh").args(&["--topic", "v", "--group-id", "vg", "--max-messages", "3000", "--reset-policy", "earliest"]).run().await;
    let consumed: i64 = out.lines().iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v["name"] == "records_consumed").map(|v| v["count"].as_i64().unwrap()).sum();
    assert_eq!(consumed, 3000, "{}", out.stderr);
    let committed_ok = out.lines().iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v["name"] == "offsets_committed").all(|v| v["success"] == true);
    assert!(committed_ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sasl_scram_and_plain() {
    e2e!();
    let tb = broker("java-sasl").await;
    let (_, cred, secret) = tb.broker.create_stream("secure", Default::default()).unwrap();
    let dir = common::temp_dir("java-sasl");
    for (mech, module) in [
        ("SCRAM-SHA-512", "org.apache.kafka.common.security.scram.ScramLoginModule"),
        ("SCRAM-SHA-256", "org.apache.kafka.common.security.scram.ScramLoginModule"),
        ("PLAIN", "org.apache.kafka.common.security.plain.PlainLoginModule"),
    ] {
        let f = write(
            &dir,
            &format!("{mech}.properties"),
            &format!("security.protocol=SASL_PLAINTEXT\nsasl.mechanism={mech}\nsasl.jaas.config={module} required username=\"{}\" password=\"{secret}\";\n", cred.username),
        );
        let out = kafka(&tb, "kafka-console-producer.sh").args(&["--topic", "s", "--producer.config", "/c.properties"]).mount(&f, "/c.properties").stdin(format!("via-{mech}\n")).run().await;
        assert!(out.ok, "{mech}: {}", out.stderr);
    }
    let f = dir.join("SCRAM-SHA-256.properties");
    let out = kafka(&tb, "kafka-console-consumer.sh").args(&["--topic", "s", "--from-beginning", "--max-messages", "3", "--timeout-ms", "15000", "--consumer.config", "/c.properties"]).mount(&f, "/c.properties").run().await;
    assert_eq!(out.lines(), vec!["via-SCRAM-SHA-512", "via-SCRAM-SHA-256", "via-PLAIN"], "{}", out.stderr);
    // The default stream doesn't see the secure stream's topics.
    let out = kafka(&tb, "kafka-topics.sh").args(&["--list"]).run().await;
    assert!(out.lines().is_empty(), "{}", out.stdout);
    let bad = write(&dir, "bad.properties", &format!("security.protocol=SASL_PLAINTEXT\nsasl.mechanism=SCRAM-SHA-256\nsasl.jaas.config=org.apache.kafka.common.security.scram.ScramLoginModule required username=\"{}\" password=\"wrong\";\n", cred.username));
    let out = kafka(&tb, "kafka-topics.sh").args(&["--list", "--command-config", "/c.properties"]).mount(&bad, "/c.properties").run().await;
    assert!(out.stdout.contains("Authentication failed") || out.stderr.contains("Authentication failed"), "{}{}", out.stdout, out.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_listener() {
    e2e!();
    let dir = common::temp_dir("java-tls");
    let cert = rcgen::generate_simple_self_signed(vec!["host.docker.internal".into(), "localhost".into()]).unwrap();
    let cert_pem = write(&dir, "cert.pem", &cert.cert.pem());
    let key_pem = write(&dir, "key.pem", &cert.signing_key.serialize_pem());
    let tb = TestBroker::start_with("java-tls", |c| {
        let bind = std::env::var("ROLL_E2E_BIND").unwrap_or_else(|_| "127.0.0.1".into());
        c.listen = format!("{bind}:0").parse().unwrap();
        c.advertised_host = "host.docker.internal".into();
        c.tls = Some(stream_broker::config::TlsFiles { cert: cert_pem.clone(), key: key_pem.clone() });
    })
    .await;
    let props = write(&dir, "tls.properties", "security.protocol=SSL\nssl.truststore.type=PEM\nssl.truststore.location=/ca.pem\n");
    let out = kafka(&tb, "kafka-console-producer.sh").args(&["--topic", "secure", "--producer.config", "/c.properties"]).mount(&props, "/c.properties").mount(&cert_pem, "/ca.pem").stdin("over tls\n").run().await;
    assert!(out.ok, "{}", out.stderr);
    let out = kafka(&tb, "kafka-console-consumer.sh")
        .args(&["--topic", "secure", "--from-beginning", "--max-messages", "1", "--timeout-ms", "15000", "--consumer.config", "/c.properties"])
        .mount(&props, "/c.properties")
        .mount(&cert_pem, "/ca.pem")
        .run()
        .await;
    assert_eq!(out.lines(), vec!["over tls"], "{}", out.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kafka_streams_wordcount_exactly_once() {
    e2e!();
    let tb = broker("java-streams").await;
    kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "streams-plaintext-input", "--partitions", "2"]).run().await;
    kafka(&tb, "kafka-topics.sh").args(&["--create", "--topic", "streams-wordcount-output", "--partitions", "2"]).run().await;
    kafka(&tb, "kafka-console-producer.sh").args(&["--topic", "streams-plaintext-input"]).stdin("all streams lead to kafka\nhello kafka streams\njoin kafka summit\nroll roll roll\n").run().await;
    let port = tb.addr.rsplit(':').next().unwrap().to_string();
    let dir = common::temp_dir("java-streams");
    let props = write(&dir, "streams.properties", &format!("bootstrap.servers=host.docker.internal:{port}\nprocessing.guarantee=exactly_once_v2\ncommit.interval.ms=300\napplication.id=wordcount\n"));
    // RocksDB needs libstdc++, which the Alpine-based image leaves out.
    let app = kafka(&tb, "kafka-run-class.sh")
        .no_bootstrap()
        .args(&["org.apache.kafka.streams.examples.wordcount.WordCountDemo", "/c.properties"])
        .mount(&props, "/c.properties")
        .root_setup("apk add --no-cache libstdc++");
    let mut app = app;
    app.args.insert(0, "timeout".into());
    app.args.insert(1, "30".into());
    let run = app.run();
    let reader = async {
        tokio::time::sleep(std::time::Duration::from_secs(12)).await;
        kafka(&tb, "kafka-console-consumer.sh")
            .args(&[
                "--topic",
                "streams-wordcount-output",
                "--from-beginning",
                "--timeout-ms",
                "15000",
                "--isolation-level",
                "read_committed",
                "--property",
                "print.key=true",
                "--property",
                "value.deserializer=org.apache.kafka.common.serialization.LongDeserializer",
            ])
            .run()
            .await
    };
    let (_, out) = tokio::join!(run, reader);
    let mut last = std::collections::BTreeMap::new();
    for l in out.lines() {
        let (k, v) = l.split_once('\t').unwrap();
        last.insert(k.to_string(), v.parse::<i64>().unwrap());
    }
    assert_eq!(last.get("kafka"), Some(&3), "{}", out.stdout);
    assert_eq!(last.get("roll"), Some(&3));
    assert_eq!(last.get("streams"), Some(&2));
    let s = tb.broker.find_stream("test").unwrap();
    let changelog = s.topics().into_iter().find(|t| t.name.ends_with("-changelog")).expect("changelog topic");
    assert_eq!(changelog.configs().get("cleanup.policy").map(String::as_str), Some("compact"));
}
