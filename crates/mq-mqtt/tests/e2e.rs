//! Docker e2e: the unmodified Python `paho-mqtt` client (MQTT 3.1.1 and 5.0) against the
//! listener. Runs only with `ROLL_E2E=1`.

mod common;

use common::Harness;
use mq_core::BrokerConfig;

const SCRIPT: &str = r#"
import sys, threading, paho.mqtt.client as mqtt
from paho.mqtt.packettypes import PacketTypes
from paho.mqtt.properties import Properties

host, port = sys.argv[1], int(sys.argv[2])

def roundtrip(proto, tag):
    got, ready = [], threading.Event()
    done = threading.Event()
    c = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="paho-" + tag, protocol=proto)
    c.username_pw_set("guest", "guest")
    def on_connect(cl, ud, flags, rc, props=None):
        assert not rc.is_failure, rc
        cl.subscribe("e2e/" + tag + "/#", qos=1)
    def on_subscribe(cl, ud, mid, rcs, props=None):
        ready.set()
    def on_message(cl, ud, m):
        got.append(m)
        done.set()
    c.on_connect, c.on_subscribe, c.on_message = on_connect, on_subscribe, on_message
    c.connect(host, port, keepalive=30)
    c.loop_start()
    assert ready.wait(10), "no SUBACK"
    props = None
    if proto == mqtt.MQTTv5:
        props = Properties(PacketTypes.PUBLISH)
        props.UserProperty = [("k", "v")]
    c.publish("e2e/" + tag + "/x", b"hello " + tag.encode(), qos=1, properties=props).wait_for_publish(10)
    assert done.wait(10), "no message"
    m = got[0]
    assert m.topic == "e2e/" + tag + "/x" and m.payload == b"hello " + tag.encode() and m.qos == 1, (m.topic, m.payload, m.qos)
    if proto == mqtt.MQTTv5:
        assert ("k", "v") in m.properties.UserProperty, m.properties
    c.disconnect()
    c.loop_stop()
    print("ok", tag)

roundtrip(mqtt.MQTTv311, "v311")
roundtrip(mqtt.MQTTv5, "v5")
"#;

#[tokio::test(flavor = "multi_thread")]
async fn paho_python_roundtrip() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        return;
    }
    let h = Harness::start_with(mq_mqtt::Config::default(), BrokerConfig::default(), "0.0.0.0:0").await;
    let port = h.port().to_string();
    let out = tokio::process::Command::new("docker")
        .args(["run", "--rm", "--add-host=host.docker.internal:host-gateway", "python:3.13-alpine", "sh", "-c"])
        .arg(format!("pip install -q --disable-pip-version-check --root-user-action=ignore paho-mqtt >/dev/null && python -c '{SCRIPT}' host.docker.internal {port}"))
        .output()
        .await
        .expect("docker");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "paho failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("ok v311") && stdout.contains("ok v5"), "{stdout}");
}
