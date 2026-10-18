//! Docker e2e: the curl examples from `docs/queues/http-api.md`, run from a real container
//! against an in-process broker. Runs only with `ROLL_E2E=1`.

use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use mq_http::{Config, HttpApi};
use std::process::Command;

const SCRIPT: &str = r#"
set -eu
R="http://host.docker.internal:$PORT"
A="-sS -u guest:guest -H content-type:application/json"
curl $A -X PUT "$R/v1/vhosts/%2F/queues/orders" -d '{"type":"quorum"}' -o /dev/null -w '%{http_code}\n'
curl $A "$R/v1/vhosts/%2F/queues/orders/messages" -H 'Idempotency-Key: e2e-1' -d '{"body":{"order":1042},"id":"o-1042"}'
echo
curl $A "$R/v1/vhosts/%2F/queues/orders/messages" -H 'Idempotency-Key: e2e-1' -d '{"body":{"order":1042},"id":"o-1042"}' -D - -o /dev/null | grep -i '^idempotent-replayed'
OUT=$(curl $A "$R/v1/vhosts/%2F/queues/orders/receive" -d '{"max":10,"wait_ms":2000}')
echo "$OUT"
RECEIPT=$(echo "$OUT" | sed -n 's/.*"receipt":"\([^"]*\)".*/\1/p')
curl $A "$R/v1/vhosts/%2F/queues/orders/ack" -d "{\"receipts\":[\"$RECEIPT\"]}"
echo
curl $A "$R/v1/vhosts/%2F/queues/orders/messages" -d '{"body":"streamed"}' > /dev/null
curl -sS -N -u guest:guest --max-time 2 "$R/v1/vhosts/%2F/queues/orders/events" || true
echo
curl $A "$R/v1/vhosts/%2F/queues/orders"
"#;

#[tokio::test]
async fn curl_from_docker() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set ROLL_E2E=1 to run Docker tests");
        return;
    }
    let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), mq_core::log::MemStore::new()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let api = HttpApi::start(broker, Config { listen: listener.local_addr().unwrap(), ..Config::default() }).await.unwrap();
    tokio::spawn(mq_http::serve_api(api, listener));
    let out = tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args(["run", "--rm", "--add-host=host.docker.internal:host-gateway", "-e", &format!("PORT={port}"), "--entrypoint", "sh", "curlimages/curl:8.10.1", "-c", SCRIPT])
            .output()
            .expect("docker")
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "container failed");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "201");
    assert!(lines[1].contains(r#""id":"o-1042""#) && lines[1].contains(r#""confirmed":true"#), "{}", lines[1]);
    assert!(lines[2].to_ascii_lowercase().starts_with("idempotent-replayed: true"));
    assert!(lines[3].contains(r#""body":"{\"order\":1042}""#), "{}", lines[3]);
    assert!(lines[4].contains(r#""succeeded":1"#), "{}", lines[4]);
    assert!(stdout.contains("event: open") && stdout.contains(r#""body":"streamed""#), "SSE output");
    let depth = lines.last().unwrap();
    assert!(depth.contains(r#""messages":0"#), "{depth}");
}
