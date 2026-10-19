//! Real, unmodified tools against the management API, run in Docker: `rabbitmqadmin` (v2,
//! from the `rabbitmq:4-management` image), Terraform with the `cyrilgdn/rabbitmq` provider,
//! and `promtool check metrics`. Only runs with `ROLL_E2E=1`.

mod common;

use common::Harness;
use reqwest::{Method, StatusCode};
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

fn enabled() -> bool {
    std::env::var("ROLL_E2E").as_deref() == Ok("1")
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn docker(args: &[&str], stdin: Option<&str>) -> Out {
    let mut cmd = Command::new("docker");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().expect("docker");
    if let Some(input) = stdin {
        let mut si = child.stdin.take().unwrap();
        si.write_all(input.as_bytes()).await.unwrap();
        drop(si);
    }
    let o = child.wait_with_output().await.unwrap();
    Out { ok: o.status.success(), stdout: String::from_utf8_lossy(&o.stdout).into(), stderr: String::from_utf8_lossy(&o.stderr).into() }
}

#[tokio::test]
async fn rabbitmqadmin_v2() {
    if !enabled() {
        return;
    }
    let h = Harness::start_on("0.0.0.0").await;
    let port = h.addr.port().to_string();
    let admin = async |args: &[&str]| -> String {
        let mut full = vec!["run", "--rm", "--add-host=host.docker.internal:host-gateway", "--entrypoint", "rabbitmqadmin", "rabbitmq:4-management"];
        full.extend(["--host", "host.docker.internal", "--port", &port, "--username", "guest", "--password", "guest"]);
        full.extend(args);
        let o = docker(&full, None).await;
        assert!(o.ok, "rabbitmqadmin {args:?} failed:\nstdout: {}\nstderr: {}", o.stdout, o.stderr);
        o.stdout
    };
    admin(&["show", "overview"]).await;
    admin(&["list", "queues"]).await;
    admin(&["declare", "queue", "--name", "e2e.q", "--type", "classic"]).await;
    admin(&["declare", "exchange", "--name", "e2e.x", "--type", "topic"]).await;
    admin(&["declare", "binding", "--source", "e2e.x", "--destination-type", "queue", "--destination", "e2e.q", "--routing-key", "e2e.#"]).await;
    assert!(admin(&["list", "queues"]).await.contains("e2e.q"));
    admin(&["publish", "message", "--routing-key", "e2e.q", "--payload", "hello-from-rabbitmqadmin"]).await;
    assert!(admin(&["get", "messages", "--queue", "e2e.q"]).await.contains("hello-from-rabbitmqadmin"));
    let defs = admin(&["definitions", "export", "--stdout"]).await;
    assert!(defs.contains("e2e.q") && defs.contains("e2e.x"), "{defs}");
    for what in ["exchanges", "bindings", "vhosts", "users", "permissions", "connections", "channels", "consumers", "policies", "nodes", "feature_flags"] {
        admin(&["list", what]).await;
    }
    admin(&["health_check", "local_alarms"]).await;
    admin(&["declare", "vhost", "--name", "e2e-vh"]).await;
    admin(&["declare", "user", "--name", "e2e-user", "--password", "pw", "--tags", "management"]).await;
    admin(&["declare", "permissions", "--vhost", "e2e-vh", "--user", "e2e-user", "--configure", ".*", "--write", ".*", "--read", ".*"]).await;
    admin(&["declare", "policy", "--name", "e2e-pol", "--pattern", "^e2e", "--apply-to", "queues", "--definition", r#"{"max-length": 10}"#]).await;
    assert!(admin(&["list", "policies"]).await.contains("e2e-pol"));
    admin(&["delete", "queue", "--name", "e2e.q"]).await;
    admin(&["delete", "vhost", "--name", "e2e-vh"]).await;
    assert_eq!(h.status(Method::GET, "/api/queues/%2F/e2e.q", None).await, StatusCode::NOT_FOUND);
}

fn terraform_config(port: u16) -> String {
    format!(
        r#"terraform {{
  required_providers {{
    rabbitmq = {{
      source  = "cyrilgdn/rabbitmq"
      version = "~> 1.8"
    }}
  }}
}}

provider "rabbitmq" {{
  endpoint = "http://host.docker.internal:{port}"
  username = "guest"
  password = "guest"
}}

resource "rabbitmq_vhost" "v" {{
  name = "tf"
}}

resource "rabbitmq_user" "u" {{
  name     = "tf-user"
  password = "secret"
  tags     = ["management"]
}}

resource "rabbitmq_permissions" "u" {{
  user  = rabbitmq_user.u.name
  vhost = rabbitmq_vhost.v.name
  permissions {{
    configure = "^tf\\."
    write     = ".*"
    read      = ".*"
  }}
}}

resource "rabbitmq_permissions" "guest" {{
  user  = "guest"
  vhost = rabbitmq_vhost.v.name
  permissions {{
    configure = ".*"
    write     = ".*"
    read      = ".*"
  }}
}}

resource "rabbitmq_exchange" "x" {{
  name  = "tf.orders"
  vhost = rabbitmq_permissions.guest.vhost
  settings {{
    type        = "topic"
    durable     = true
    auto_delete = false
  }}
}}

resource "rabbitmq_queue" "q" {{
  name  = "tf.orders.eu"
  vhost = rabbitmq_permissions.guest.vhost
  settings {{
    durable     = true
    auto_delete = false
    arguments = {{
      "x-queue-type" = "quorum"
    }}
  }}
}}

resource "rabbitmq_binding" "b" {{
  source           = rabbitmq_exchange.x.name
  vhost            = rabbitmq_vhost.v.name
  destination      = rabbitmq_queue.q.name
  destination_type = "queue"
  routing_key      = "eu.#"
}}

resource "rabbitmq_policy" "p" {{
  name  = "tf-ttl"
  vhost = rabbitmq_permissions.guest.vhost
  policy {{
    pattern  = "^tf\\."
    priority = 0
    apply_to = "queues"
    definition = {{
      message-ttl = 60000
    }}
  }}
}}
"#
    )
}

#[tokio::test]
async fn terraform_rabbitmq_provider() {
    if !enabled() {
        return;
    }
    let h = Harness::start_on("0.0.0.0").await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.tf"), terraform_config(h.addr.port())).unwrap();
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("terraform-plugin-cache");
    std::fs::create_dir_all(&cache).unwrap();
    let work = format!("{}:/work", dir.path().display());
    let plugins = format!("{}:/plugin-cache", cache.display());
    let tf = async |args: &[&str]| -> Out {
        let mut full = vec!["run", "--rm", "--add-host=host.docker.internal:host-gateway", "-v", &work, "-v", &plugins, "-w", "/work"];
        full.extend(["-e", "TF_PLUGIN_CACHE_DIR=/plugin-cache", "-e", "TF_IN_AUTOMATION=1", "hashicorp/terraform:latest"]);
        full.extend(args);
        docker(&full, None).await
    };
    let check = |o: Out, what: &str| {
        assert!(o.ok, "terraform {what} failed:\nstdout: {}\nstderr: {}", o.stdout, o.stderr);
    };
    check(tf(&["init", "-input=false", "-no-color"]).await, "init");
    check(tf(&["apply", "-auto-approve", "-input=false", "-no-color"]).await, "apply");

    assert_eq!(h.get("/api/vhosts/tf").await["name"], "tf");
    assert_eq!(h.get("/api/users/tf-user").await["tags"], json!(["management"]));
    assert_eq!(h.get("/api/permissions/tf/tf-user").await["configure"], "^tf\\.");
    assert_eq!(h.get("/api/exchanges/tf/tf.orders").await["type"], "topic");
    let q = h.get("/api/queues/tf/tf.orders.eu").await;
    assert_eq!((q["type"].as_str(), q["policy"].as_str()), (Some("quorum"), Some("tf-ttl")));
    assert_eq!(h.get("/api/bindings/tf/e/tf.orders/q/tf.orders.eu").await[0]["routing_key"], "eu.#");
    assert_eq!(h.get("/api/policies/tf/tf-ttl").await["definition"]["message-ttl"], 60000);

    // Reading everything back must show no drift.
    let plan = tf(&["plan", "-detailed-exitcode", "-input=false", "-no-color"]).await;
    assert!(plan.ok, "terraform plan shows drift or failed:\nstdout: {}\nstderr: {}", plan.stdout, plan.stderr);

    check(tf(&["destroy", "-auto-approve", "-input=false", "-no-color"]).await, "destroy");
    assert_eq!(h.status(Method::GET, "/api/vhosts/tf", None).await, StatusCode::NOT_FOUND);
    assert_eq!(h.status(Method::GET, "/api/users/tf-user", None).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn promtool_check_metrics() {
    if !enabled() {
        return;
    }
    let h = Harness::start().await;
    let conn = h.amqp().await;
    let ch = conn.create_channel().await.unwrap();
    ch.queue_declare("prom.q".into(), Default::default(), Default::default()).await.unwrap();
    h.mgmt.sample();
    for path in ["/metrics", "/metrics/per-object", "/metrics/detailed?family=queue_coarse_metrics&family=queue_metrics&family=queue_delivery_metrics&family=connection_coarse_metrics&family=connection_metrics&family=channel_metrics&family=channel_exchange_metrics&family=connection_churn_metrics&family=node_coarse_metrics&family=exchange_names&family=exchange_bindings&family=vhost_status"] {
        let text = h.req_as(Method::GET, path, None, None).await.text;
        let o = docker(&["run", "--rm", "-i", "--entrypoint", "promtool", "prom/prometheus:latest", "check", "metrics"], Some(&text)).await;
        assert!(o.ok, "promtool rejected {path}:\nstdout: {}\nstderr: {}\n{text}", o.stdout, o.stderr);
    }
    drop(conn);
}
