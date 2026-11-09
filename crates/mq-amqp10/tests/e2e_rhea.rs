//! The JavaScript `rhea` client in Docker against an in-process broker. Runs only with
//! `ROLL_E2E=1`.

use mq_core::log::MemStore;
use mq_core::meta::MemMeta;
use mq_core::vhost::DeclareQueue;
use mq_core::{Broker, BrokerConfig, FieldTable};
use std::process::Command;

const SCRIPT: &str = r#"
const container = require('rhea');
const N = 20;
let sent = 0, accepted = 0, received = 0;
const fail = (m) => { console.error(m); process.exit(1); };
container.on('connection_open', (ctx) => {
  ctx.connection.open_receiver({ source: { address: '/queues/rhea' }, autoaccept: true });
  ctx.connection.open_sender({ target: { address: '/queues/rhea' } });
});
container.on('sendable', (ctx) => {
  while (ctx.sender.sendable() && sent < N) {
    ctx.sender.send({ message_id: sent, subject: 'roll', application_properties: { i: sent }, body: 'msg-' + sent });
    sent++;
  }
});
container.on('accepted', () => { accepted++; });
container.on('message', (ctx) => {
  const m = ctx.message;
  if (m.body !== 'msg-' + received) fail('unexpected body ' + JSON.stringify(m.body));
  if (m.application_properties.i !== received) fail('unexpected i ' + JSON.stringify(m.application_properties));
  if (m.subject !== 'roll') fail('unexpected subject ' + m.subject);
  received++;
  if (received === N) {
    const wait = () => (accepted === N ? ctx.connection.close() : setTimeout(wait, 20));
    wait();
  }
});
container.on('connection_close', () => {
  console.log('rhea roundtrip ok: sent', sent, 'accepted', accepted, 'received', received);
  process.exit(received === N && accepted === N ? 0 : 1);
});
container.on('error', (e) => fail('error: ' + e));
container.on('disconnected', (ctx) => { if (received !== N) fail('disconnected: ' + (ctx.error || '')); });
container.connect({ host: 'host.docker.internal', port: +process.env.PORT, username: 'guest', password: 'guest', reconnect: false });
setTimeout(() => fail('timeout: received ' + received), 30000);
"#;

#[tokio::test(flavor = "multi_thread")]
async fn rhea_roundtrip() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set ROLL_E2E=1 to run the rhea Docker test");
        return;
    }
    let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
    let vh = broker.vhost("/").unwrap();
    let d = DeclareQueue { name: "rhea".into(), durable: true, exclusive_owner: None, auto_delete: false, args: FieldTable::new(), passive: false };
    vh.declare_queue(d).await.unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = mq_amqp::ServerConfig { listen: addr, amqp10: Some(mq_amqp10::handoff(mq_amqp10::Config::default())) };
    tokio::spawn(mq_amqp::serve_listener(broker.clone(), listener, cfg));

    let out = tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args(["run", "--rm", "--add-host=host.docker.internal:host-gateway", "-e"])
            .arg(format!("PORT={}", addr.port()))
            .args(["-e", &format!("SCRIPT={SCRIPT}"), "-w", "/app", "node:22-alpine", "sh", "-c"])
            .arg("npm i --silent --no-audit --no-fund rhea >/dev/null && node -e \"$SCRIPT\"")
            .output()
            .expect("docker run")
    })
    .await
    .unwrap();
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "rhea client failed");
    assert_eq!(vh.queue("rhea").unwrap().shared.messages(), 0);
}
