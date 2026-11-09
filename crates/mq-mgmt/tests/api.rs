//! Management API integration tests against an in-process broker.

mod common;

use common::{Harness, names};
use futures::StreamExt;
use lapin::options::*;
use lapin::types::FieldTable;
use lapin::{BasicProperties, ExchangeKind};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test]
async fn overview_nodes_and_identity() {
    let h = Harness::start().await;
    let o = h.get("/api/overview").await;
    assert_eq!(o["rabbitmq_version"], "4.1.0");
    assert_eq!(o["management_version"], "4.1.0");
    assert_eq!(o["product_name"], "RabbitMQ");
    assert_eq!(o["node"], h.broker.config.node_name.as_str());
    assert_eq!(o["object_totals"]["queues"], 0);
    assert!(o["object_totals"]["exchanges"].as_u64().unwrap() >= 7);
    assert!(o["queue_totals"]["messages_details"]["rate"].is_number());
    assert!(o["message_stats"]["publish_details"]["rate"].is_number());
    assert!(o["exchange_types"].as_array().unwrap().iter().any(|t| t["name"] == "x-consistent-hash"));
    let listeners = o["listeners"].as_array().unwrap();
    assert!(listeners.iter().any(|l| l["protocol"] == "amqp"));
    assert!(listeners.iter().any(|l| l["protocol"] == "http" && l["port"] == h.addr.port()));

    assert_eq!(h.get("/api/cluster-name").await, json!({"name": "roll"}));
    assert_eq!(h.put("/api/cluster-name", json!({"name": "east"})).await, StatusCode::NO_CONTENT);
    assert_eq!(h.get("/api/cluster-name").await["name"], "east");
    assert_eq!(h.get("/api/overview").await["cluster_name"], "east");

    let nodes = h.get("/api/nodes").await;
    let node = &nodes[0];
    assert_eq!(node["name"], h.broker.config.node_name.as_str());
    assert_eq!(node["running"], true);
    assert_eq!(node["being_drained"], false);
    assert!(node["mem_used"].as_u64().unwrap() > 0);
    assert!(node["fd_total"].as_u64().unwrap() > 0);
    let name = node["name"].as_str().unwrap();
    let one = h.get(&format!("/api/nodes/{name}?memory=true")).await;
    assert!(one["memory"]["total"]["rss"].as_u64().unwrap() > 0);
    assert_eq!(h.status(Method::GET, "/api/nodes/rabbit@elsewhere", None).await, StatusCode::NOT_FOUND);
    assert!(h.get(&format!("/api/nodes/{name}/memory")).await["memory"]["strategy"].is_string());

    let me = h.get("/api/whoami").await;
    assert_eq!(me["name"], "guest");
    assert_eq!(me["tags"], json!(["administrator"]));
    assert_eq!(h.get("/api/extensions").await, json!([]));
    let auth = h.req_as(Method::GET, "/api/auth", None, None).await;
    assert_eq!(auth.status, StatusCode::OK);
    assert_eq!(auth.json()["oauth_enabled"], false);

    let index = h.req_as(Method::GET, "/", None, None).await;
    assert!(index.text.contains("roll queues") && index.text.contains("http://localhost:3000/queues"));
    assert_eq!(h.status(Method::GET, "/api/no-such-thing", None).await, StatusCode::NOT_FOUND);
    let err = h.req(Method::GET, "/api/queues/%2F/missing", None).await;
    assert_eq!(err.status, StatusCode::NOT_FOUND);
    assert_eq!(err.json(), json!({"error": "Object Not Found", "reason": "Not Found"}));
}

#[tokio::test]
async fn authentication_and_tags() {
    let h = Harness::start().await;
    let r = h.req_as(Method::GET, "/api/overview", None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.headers.get("www-authenticate").unwrap().to_str().unwrap().starts_with("Basic"));
    assert_eq!(r.json()["error"], "not_authorized");
    let r = h.req_as(Method::GET, "/api/overview", None, Some(("guest", "wrong"))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    for (u, tags) in [("plain", ""), ("mgmt", "management"), ("mon", "monitoring"), ("pm", "policymaker")] {
        assert_eq!(h.put(&format!("/api/users/{u}"), json!({"password": "pw", "tags": tags})).await, StatusCode::CREATED);
    }
    assert_eq!(h.put("/api/vhosts/other", json!({})).await, StatusCode::CREATED);
    for u in ["mgmt", "pm"] {
        assert_eq!(h.put(&format!("/api/permissions/%2F/{u}"), json!({"configure": ".*", "write": ".*", "read": ".*"})).await, StatusCode::CREATED);
    }

    let r = h.req_as(Method::GET, "/api/overview", None, Some(("plain", "pw"))).await;
    assert_eq!((r.status, r.json()["reason"].clone()), (StatusCode::UNAUTHORIZED, json!("Not management user")));

    // management: own vhosts only, no admin or monitoring endpoints
    let mgmt = Some(("mgmt", "pw"));
    assert_eq!(h.req_as(Method::GET, "/api/overview", None, mgmt).await.status, StatusCode::OK);
    let vhosts = h.req_as(Method::GET, "/api/vhosts", None, mgmt).await.json();
    assert_eq!(names(&vhosts), vec!["/"]);
    let r = h.req_as(Method::GET, "/api/users", None, mgmt).await;
    assert_eq!((r.status, r.json()["reason"].clone()), (StatusCode::UNAUTHORIZED, json!("Not administrator user")));
    assert_eq!(h.req_as(Method::GET, "/api/nodes", None, mgmt).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.req_as(Method::GET, "/api/queues/other", None, mgmt).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.req_as(Method::PUT, "/api/queues/%2F/mine", Some(json!({})), mgmt).await.status, StatusCode::CREATED);
    assert_eq!(h.req_as(Method::PUT, "/api/vhosts/nope", Some(json!({})), mgmt).await.status, StatusCode::UNAUTHORIZED);
    let pol = json!({"pattern": "^x", "definition": {"max-length": 5}});
    assert_eq!(h.req_as(Method::PUT, "/api/policies/%2F/p", Some(pol.clone()), mgmt).await.status, StatusCode::UNAUTHORIZED);

    // monitoring: every vhost and node data, but no writes to users
    let mon = Some(("mon", "pw"));
    assert_eq!(names(&h.req_as(Method::GET, "/api/vhosts", None, mon).await.json()), vec!["/", "other"]);
    assert_eq!(h.req_as(Method::GET, "/api/nodes", None, mon).await.status, StatusCode::OK);
    assert_eq!(h.req_as(Method::PUT, "/api/users/x", Some(json!({"password": "x"})), mon).await.status, StatusCode::UNAUTHORIZED);

    // policymaker: policies and parameters in its vhosts
    let pm = Some(("pm", "pw"));
    assert_eq!(h.req_as(Method::PUT, "/api/policies/%2F/p", Some(pol), pm).await.status, StatusCode::CREATED);
    assert_eq!(h.req_as(Method::GET, "/api/policies/%2F", None, pm).await.json()[0]["name"], "p");

    // resource permissions apply to management users
    assert_eq!(h.put("/api/permissions/%2F/mgmt", json!({"configure": "^own\\.", "write": ".*", "read": ".*"})).await, StatusCode::NO_CONTENT);
    let r = h.req_as(Method::PUT, "/api/queues/%2F/theirs", Some(json!({})), mgmt).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.json()["reason"].as_str().unwrap().contains("configure access to queue 'theirs'"), "{}", r.text);
}

#[tokio::test]
async fn vhosts_users_permissions_and_limits() {
    let h = Harness::start().await;
    assert_eq!(h.put("/api/vhosts/dev", json!({"description": "Dev", "tags": "a,b", "default_queue_type": "quorum"})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/vhosts/dev", json!({"description": "Development"})).await, StatusCode::NO_CONTENT);
    let v = h.get("/api/vhosts/dev").await;
    assert_eq!(v["description"], "Development");
    assert_eq!(v["tags"], json!(["a", "b"]));
    assert_eq!(v["default_queue_type"], "quorum");
    assert_eq!(v["metadata"]["default_queue_type"], "quorum");
    assert_eq!(v["messages"], 0);
    assert_eq!(names(&h.get("/api/vhosts").await), vec!["/", "dev"]);
    assert_eq!(h.put("/api/vhosts/bad", json!({"default_queue_type": "fifo"})).await, StatusCode::BAD_REQUEST);
    let all = json!({"configure": ".*", "write": ".*", "read": ".*"});
    assert_eq!(h.put("/api/permissions/dev/guest", all).await, StatusCode::CREATED);

    // users
    assert_eq!(h.put("/api/users/alice", json!({"password": "s3cret", "tags": ["monitoring"]})).await, StatusCode::CREATED);
    let alice = h.get("/api/users/alice").await;
    assert_eq!(alice["tags"], json!(["monitoring"]));
    assert_eq!(alice["hashing_algorithm"], "rabbit_password_hashing_sha256");
    // tags-only update keeps the password
    assert_eq!(h.put("/api/users/alice", json!({"tags": "administrator"})).await, StatusCode::NO_CONTENT);
    assert_eq!(h.req_as(Method::GET, "/api/whoami", None, Some(("alice", "s3cret"))).await.json()["tags"], json!(["administrator"]));
    // password_hash (as exported by RabbitMQ) and passwordless users
    let hash = alice["password_hash"].as_str().unwrap();
    assert_eq!(h.put("/api/users/bob", json!({"password_hash": hash, "tags": "management"})).await, StatusCode::CREATED);
    assert_eq!(h.req_as(Method::GET, "/api/whoami", None, Some(("bob", "s3cret"))).await.status, StatusCode::OK);
    assert_eq!(h.put("/api/users/carol", json!({"tags": ""})).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.put("/api/users/carol", json!({"password_hash": "", "tags": ""})).await, StatusCode::CREATED);
    assert_eq!(h.req_as(Method::GET, "/api/whoami", None, Some(("carol", ""))).await.status, StatusCode::UNAUTHORIZED);
    let without = names(&h.get("/api/users/without-permissions").await);
    assert!(without.contains(&"alice".to_string()) && !without.contains(&"guest".to_string()));

    // permissions
    let p = json!({"configure": "^a", "write": ".*", "read": ".*"});
    assert_eq!(h.put("/api/permissions/dev/alice", p.clone()).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/permissions/dev/alice", p).await, StatusCode::NO_CONTENT);
    assert_eq!(h.put("/api/permissions/nope/alice", json!({"configure": "", "write": "", "read": ""})).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.put("/api/permissions/dev/alice", json!({"configure": "(", "write": "", "read": ""})).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.get("/api/permissions/dev/alice").await, json!({"user": "alice", "vhost": "dev", "configure": "^a", "write": ".*", "read": ".*"}));
    assert_eq!(h.get("/api/users/alice/permissions").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/vhosts/dev/permissions").await[0]["user"], "alice");
    assert_eq!(h.get("/api/permissions").await.as_array().unwrap().len(), 3);

    // topic permissions
    let tp = json!({"exchange": "amq.topic", "write": "^a\\.", "read": ".*"});
    assert_eq!(h.put("/api/topic-permissions/dev/alice", tp).await, StatusCode::CREATED);
    assert_eq!(h.get("/api/topic-permissions/dev/alice").await[0]["write"], "^a\\.");
    assert_eq!(h.get("/api/users/alice/topic-permissions").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/vhosts/dev/topic-permissions").await.as_array().unwrap().len(), 1);
    assert_eq!(h.delete("/api/topic-permissions/dev/alice").await, StatusCode::NO_CONTENT);
    assert_eq!(h.status(Method::GET, "/api/topic-permissions/dev/alice", None).await, StatusCode::NOT_FOUND);

    // limits
    assert_eq!(h.put("/api/vhost-limits/dev/max-connections", json!({"value": 10})).await, StatusCode::NO_CONTENT);
    assert_eq!(h.put("/api/vhost-limits/dev/max-queues", json!({"value": 2})).await, StatusCode::NO_CONTENT);
    assert_eq!(h.put("/api/vhost-limits/dev/max-cats", json!({"value": 2})).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.get("/api/vhost-limits/dev").await, json!([{"vhost": "dev", "value": {"max-connections": 10, "max-queues": 2}}]));
    assert_eq!(h.get("/api/vhost-limits").await.as_array().unwrap().len(), 1);
    assert_eq!(h.delete("/api/vhost-limits/dev/max-connections").await, StatusCode::NO_CONTENT);
    assert_eq!(h.get("/api/vhost-limits/dev").await[0]["value"], json!({"max-queues": 2}));
    for q in ["q1", "q2"] {
        assert_eq!(h.put(&format!("/api/queues/dev/{q}"), json!({})).await, StatusCode::CREATED);
    }
    let r = h.req(Method::PUT, "/api/queues/dev/q3", Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["reason"].as_str().unwrap().contains("queue limit"), "{}", r.text);

    assert_eq!(h.put("/api/user-limits/alice/max-channels", json!({"value": "3"})).await, StatusCode::NO_CONTENT);
    assert_eq!(h.get("/api/user-limits/alice").await, json!([{"user": "alice", "value": {"max-channels": 3}}]));
    assert_eq!(h.get("/api/user-limits").await.as_array().unwrap().len(), 1);
    assert_eq!(h.delete("/api/user-limits/alice/max-channels").await, StatusCode::NO_CONTENT);
    assert_eq!(h.get("/api/user-limits").await, json!([]));

    // deletes
    assert_eq!(h.delete("/api/permissions/dev/alice").await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete("/api/permissions/dev/alice").await, StatusCode::NOT_FOUND);
    assert_eq!(h.post("/api/users/bulk-delete", json!({"users": ["bob", "carol"]})).await.status, StatusCode::NO_CONTENT);
    assert_eq!(h.status(Method::GET, "/api/users/bob", None).await, StatusCode::NOT_FOUND);
    assert_eq!(h.delete("/api/users/alice").await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete("/api/users/alice").await, StatusCode::NOT_FOUND);
    assert_eq!(h.delete("/api/vhosts/dev").await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete("/api/vhosts/dev").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exchanges_queues_and_bindings() {
    let h = Harness::start().await;
    assert_eq!(h.put("/api/exchanges/%2F/orders", json!({"type": "topic", "durable": true})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/exchanges/%2F/orders", json!({"type": "topic", "durable": true})).await, StatusCode::NO_CONTENT);
    let r = h.req(Method::PUT, "/api/exchanges/%2F/orders", Some(json!({"type": "fanout"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["reason"].as_str().unwrap().starts_with("inequivalent arg 'type' for exchange 'orders'"), "{}", r.text);
    assert_eq!(h.put("/api/exchanges/%2F/nokind", json!({})).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.put("/api/exchanges/%2F/amq.custom", json!({"type": "direct"})).await, StatusCode::UNAUTHORIZED);
    // rabbitmqadmin v1 sends booleans as strings
    assert_eq!(h.put("/api/exchanges/%2F/audit", json!({"type": "fanout", "durable": "false", "auto_delete": "false"})).await, StatusCode::CREATED);

    let x = h.get("/api/exchanges/%2F/orders").await;
    assert_eq!((x["type"].as_str(), x["durable"].as_bool(), x["vhost"].as_str()), (Some("topic"), Some(true), Some("/")));
    assert!(x["message_stats"]["publish_in_details"]["rate"].is_number());
    assert_eq!(h.get("/api/exchanges/%2F/amq.default").await["name"], "");
    let all = names(&h.get("/api/exchanges/%2F").await);
    assert!(all.contains(&"".to_string()) && all.contains(&"amq.topic".to_string()) && all.contains(&"orders".to_string()));
    assert_eq!(h.get("/api/exchanges").await.as_array().unwrap().len(), all.len());

    assert_eq!(h.put("/api/queues/%2F/orders.eu", json!({"durable": true, "arguments": {"x-max-length": 100}})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/queues/%2F/orders.eu", json!({"durable": true, "arguments": {"x-max-length": 100}})).await, StatusCode::NO_CONTENT);
    let r = h.req(Method::PUT, "/api/queues/%2F/orders.eu", Some(json!({"durable": false}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["reason"], "inequivalent arg 'durable' for queue 'orders.eu' in vhost '/': received 'false' but current is 'true'");
    assert_eq!(h.put("/api/queues/%2F/votes", json!({"durable": true, "type": "quorum"})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/queues/%2F/events", json!({"arguments": {"x-queue-type": "stream"}})).await, StatusCode::CREATED);
    let q = h.get("/api/queues/%2F/orders.eu").await;
    assert_eq!((q["type"].as_str(), q["state"].as_str(), q["arguments"]["x-max-length"].as_i64()), (Some("classic"), Some("running"), Some(100)));
    assert_eq!(q["consumer_details"], json!([]));
    assert_eq!(h.get("/api/queues/%2F/votes").await["type"], "quorum");
    assert_eq!(h.get("/api/queues/%2F/votes").await["leader"], h.broker.config.node_name.as_str());
    assert_eq!(h.get("/api/queues/%2F/events").await["type"], "stream");
    assert_eq!(names(&h.get("/api/queues").await), vec!["events", "orders.eu", "votes"]);
    assert_eq!(h.get("/api/queues/detailed").await.as_array().unwrap().len(), 3);

    // bindings: e→q with routing keys that need escaping, and e→e with arguments
    let r = h.post("/api/bindings/%2F/e/orders/q/orders.eu", json!({"routing_key": "eu.#", "arguments": {}})).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let loc = r.headers.get("location").unwrap().to_str().unwrap().to_string();
    assert_eq!(loc, "../../../../%2F/e/orders/q/orders.eu/eu.%2523");
    let between = h.get("/api/bindings/%2F/e/orders/q/orders.eu").await;
    assert_eq!(between.as_array().unwrap().len(), 1);
    let b = &between[0];
    assert_eq!(b["properties_key"], "eu.%23");
    assert_eq!((b["source"].as_str(), b["destination_type"].as_str(), b["routing_key"].as_str()), (Some("orders"), Some("queue"), Some("eu.#")));
    let key_path = format!("/api/bindings/%2F/e/orders/q/orders.eu/{}", loc.rsplit('/').next().unwrap());
    assert_eq!(h.get(&key_path).await["routing_key"], "eu.#");

    let r = h.post("/api/bindings/%2F/e/orders/e/audit", json!({"routing_key": "", "arguments": {"x-tag": 1}})).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let ee = h.get("/api/bindings/%2F/e/orders/e/audit").await;
    let pk = ee[0]["properties_key"].as_str().unwrap().to_string();
    assert!(pk.starts_with('~'), "{pk}");
    assert_eq!(h.get("/api/exchanges/%2F/orders/bindings/source").await.as_array().unwrap().len(), 2);
    assert_eq!(h.get("/api/exchanges/%2F/audit/bindings/destination").await.as_array().unwrap().len(), 1);
    let qb = h.get("/api/queues/%2F/orders.eu/bindings").await;
    assert_eq!(qb.as_array().unwrap().len(), 2, "default binding plus ours: {qb}");
    assert_eq!(qb[0]["source"], "");
    let all = h.get("/api/bindings/%2F").await;
    assert_eq!(all.as_array().unwrap().len(), 5);
    assert_eq!(h.get("/api/bindings").await.as_array().unwrap().len(), 5);

    assert_eq!(h.post("/api/bindings/%2F/e/missing/q/orders.eu", json!({})).await.status, StatusCode::NOT_FOUND);
    assert_eq!(h.post("/api/bindings/%2F/e/orders/q/missing", json!({})).await.status, StatusCode::NOT_FOUND);
    assert_eq!(h.post("/api/bindings/%2F/e/amq.default/q/orders.eu", json!({})).await.status, StatusCode::UNAUTHORIZED);

    assert_eq!(h.delete(&format!("/api/bindings/%2F/e/orders/e/audit/{}", common_enc(&pk))).await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete(&key_path).await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete(&key_path).await, StatusCode::NOT_FOUND);
    assert_eq!(h.get("/api/bindings/%2F").await.as_array().unwrap().len(), 3);

    assert_eq!(h.delete("/api/queues/%2F/orders.eu?if-unused=true").await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete("/api/queues/%2F/orders.eu").await, StatusCode::NOT_FOUND);
    assert_eq!(h.delete("/api/exchanges/%2F/amq.topic").await, StatusCode::UNAUTHORIZED);
    assert_eq!(h.delete("/api/exchanges/%2F/orders").await, StatusCode::NO_CONTENT);
    assert_eq!(h.status(Method::GET, "/api/exchanges/%2F/orders", None).await, StatusCode::NOT_FOUND);
    assert_eq!(h.status(Method::GET, "/api/exchanges/nope", None).await, StatusCode::NOT_FOUND);
}

fn common_enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

#[tokio::test]
async fn publish_get_and_purge() {
    let h = Harness::start().await;
    assert_eq!(h.put("/api/queues/%2F/inbox", json!({"durable": true})).await, StatusCode::CREATED);
    let publish = |rk: &str, payload: &str, enc: &str| json!({"routing_key": rk, "payload": payload, "payload_encoding": enc, "properties": {"delivery_mode": 2, "headers": {"k": "v"}, "message_id": "m1"}});
    let r = h.post("/api/exchanges/%2F/amq.default/publish", publish("inbox", "hello", "string")).await;
    assert_eq!((r.status, r.json()), (StatusCode::OK, json!({"routed": true})));
    let r = h.post("/api/exchanges/%2F/amq.default/publish", publish("inbox", "AAEC/w==", "base64")).await;
    assert_eq!(r.json(), json!({"routed": true}));
    assert_eq!(h.post("/api/exchanges/%2F/amq.default/publish", publish("nowhere", "x", "string")).await.json(), json!({"routed": false}));
    assert_eq!(h.post("/api/exchanges/%2F/missing/publish", publish("inbox", "x", "string")).await.status, StatusCode::NOT_FOUND);
    // the empty segment is the default exchange too
    assert_eq!(h.post("/api/exchanges/%2F//publish", publish("inbox", "third", "string")).await.json(), json!({"routed": true}));

    let get = |count: i64, ackmode: &str| json!({"count": count, "ackmode": ackmode, "encoding": "auto", "truncate": 50000});
    let msgs = h.post("/api/queues/%2F/inbox/get", get(2, "ack_requeue_true")).await.json();
    assert_eq!(msgs.as_array().unwrap().len(), 2);
    assert_eq!(msgs[0]["payload"], "hello");
    assert_eq!(msgs[0]["payload_encoding"], "string");
    assert_eq!(msgs[0]["payload_bytes"], 5);
    assert_eq!(msgs[0]["message_count"], 2);
    assert_eq!(msgs[0]["routing_key"], "inbox");
    assert_eq!(msgs[0]["properties"]["headers"]["k"], "v");
    assert_eq!(msgs[0]["properties"]["message_id"], "m1");
    assert_eq!(msgs[1]["payload"], "AAEC/w==");
    assert_eq!(msgs[1]["payload_encoding"], "base64");

    // requeued: still there, now redelivered
    let again = h.post("/api/queues/%2F/inbox/get", get(10, "ack_requeue_false")).await.json();
    assert_eq!(again.as_array().unwrap().len(), 3);
    assert!(again.as_array().unwrap().iter().any(|m| m["redelivered"] == true));
    assert_eq!(h.post("/api/queues/%2F/inbox/get", get(1, "ack_requeue_false")).await.json(), json!([]));
    let r = h.post("/api/queues/%2F/inbox/get", get(1, "nope")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h.post("/api/queues/%2F/inbox/get", json!({"count": 1, "ackmode": "ack_requeue_true", "encoding": "utf9"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // truncate and base64 encoding
    h.post("/api/exchanges/%2F/amq.default/publish", publish("inbox", "abcdefgh", "string")).await;
    let m = h.post("/api/queues/%2F/inbox/get", json!({"count": 1, "ackmode": "ack_requeue_true", "encoding": "base64", "truncate": 3})).await.json();
    assert_eq!((m[0]["payload"].as_str(), m[0]["payload_bytes"].as_u64()), (Some("YWJj"), Some(8)));

    let q = h.get("/api/queues/%2F/inbox").await;
    assert_eq!(q["messages"], 1);
    assert_eq!(h.delete("/api/queues/%2F/inbox/contents").await, StatusCode::NO_CONTENT);
    assert_eq!(h.get("/api/queues/%2F/inbox").await["messages"], 0);
    let r = h.req(Method::DELETE, "/api/queues/%2F/inbox?if-empty=true", None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);

    // user_id must match the authenticated user
    assert_eq!(h.put("/api/queues/%2F/ids", json!({})).await, StatusCode::CREATED);
    let r = h.post("/api/exchanges/%2F/amq.default/publish", json!({"routing_key": "ids", "payload": "x", "payload_encoding": "string", "properties": {"user_id": "mallory"}})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.post("/api/queues/%2F/ids/actions", json!({"action": "sync"})).await.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn connections_channels_consumers_and_rates() {
    let h = Harness::start().await;
    let conn = h.amqp().await;
    let ch = conn.create_channel().await.unwrap();
    ch.queue_declare("work".into(), QueueDeclareOptions { durable: true, ..Default::default() }, FieldTable::default()).await.unwrap();
    ch.basic_qos(7, BasicQosOptions::default()).await.unwrap();
    h.mgmt.sample();
    let mut consumer = ch.basic_consume("work".into(), "c-1".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
    for i in 0..20 {
        ch.basic_publish("".into(), "work".into(), BasicPublishOptions::default(), format!("m{i}").as_bytes(), BasicProperties::default()).await.unwrap();
    }
    for _ in 0..20 {
        let d = tokio::time::timeout(Duration::from_secs(5), consumer.next()).await.unwrap().unwrap().unwrap();
        d.ack(BasicAckOptions::default()).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.mgmt.sample();

    let conns = h.get("/api/connections").await;
    assert_eq!(conns.as_array().unwrap().len(), 1);
    let c = &conns[0];
    let cname = c["name"].as_str().unwrap().to_string();
    assert_eq!((c["user"].as_str(), c["vhost"].as_str(), c["protocol"].as_str()), (Some("guest"), Some("/"), Some("AMQP 0-9-1")));
    assert!(c["recv_oct"].as_u64().unwrap() > 0);
    assert!(c["recv_oct_details"]["rate"].as_f64().unwrap() > 0.0);
    assert!(c["client_properties"]["capabilities"].is_object());
    let enc_name = common_enc(&cname);
    assert_eq!(h.get(&format!("/api/connections/{enc_name}")).await["name"], cname.as_str());

    let chans = h.get(&format!("/api/connections/{enc_name}/channels")).await;
    assert_eq!(chans.as_array().unwrap().len(), 1);
    let chan = &chans[0];
    assert_eq!(chan["prefetch_count"], 7);
    assert_eq!(chan["consumer_count"], 1);
    assert_eq!(chan["connection_details"]["name"], cname.as_str());
    assert!(chan["connection_details"]["peer_port"].as_u64().unwrap() > 0);
    assert_eq!(chan["message_stats"]["publish"], 20);
    assert_eq!(chan["message_stats"]["ack"], 20);
    let chan_name = chan["name"].as_str().unwrap().to_string();
    assert_eq!(h.get(&format!("/api/channels/{}", common_enc(&chan_name))).await["number"], 1);
    assert_eq!(h.get("/api/channels").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/vhosts/%2F/channels").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/vhosts/%2F/connections").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/connections/username/guest").await.as_array().unwrap().len(), 1);

    let consumers = h.get("/api/consumers").await;
    assert_eq!(consumers.as_array().unwrap().len(), 1);
    let k = &consumers[0];
    assert_eq!((k["consumer_tag"].as_str(), k["ack_required"].as_bool(), k["prefetch_count"].as_u64()), (Some("c-1"), Some(true), Some(7)));
    assert_eq!(k["queue"], json!({"name": "work", "vhost": "/"}));
    assert_eq!(k["channel_details"]["name"], chan_name.as_str());
    assert_eq!(h.get("/api/consumers/%2F").await.as_array().unwrap().len(), 1);
    let q = h.get("/api/queues/%2F/work").await;
    assert_eq!(q["consumers"], 1);
    assert_eq!(q["consumer_details"][0]["consumer_tag"], "c-1");
    assert_eq!(q["message_stats"]["publish"], 20);
    assert_eq!(q["message_stats"]["deliver_get"], 20);
    assert_eq!(q["message_stats"]["ack"], 20);
    assert!(q["message_stats"]["publish_details"]["rate"].as_f64().unwrap() > 0.0, "{}", q["message_stats"]);

    let o = h.get("/api/overview?msg_rates_age=60&msg_rates_incr=5&lengths_age=60&lengths_incr=5").await;
    assert_eq!(o["object_totals"]["connections"], 1);
    assert_eq!(o["object_totals"]["consumers"], 1);
    assert_eq!(o["message_stats"]["publish"], 20);
    assert!(o["message_stats"]["publish_details"]["rate"].as_f64().unwrap() > 0.0);
    assert!(o["message_stats"]["publish_details"]["samples"][0]["timestamp"].as_u64().unwrap() > 0);
    assert!(o["queue_totals"]["messages_details"]["samples"].is_array());
    assert!(o["churn_rates"]["connection_created"].as_u64().unwrap() >= 1);
    let v = h.get("/api/vhosts/%2F").await;
    assert_eq!(v["message_stats"]["publish"], 20);
    assert!(v["recv_oct"].as_u64().unwrap() > 0);

    // force-close from the management API
    let r = h.req(Method::DELETE, &format!("/api/connections/{enc_name}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    for _ in 0..100 {
        if h.get("/api/connections").await.as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.get("/api/connections").await, json!([]));
    assert!(!conn.status().connected());
    assert_eq!(h.status(Method::DELETE, &format!("/api/connections/{enc_name}"), None).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pagination_sorting_columns_and_stats_toggles() {
    let h = Harness::start().await;
    for (i, n) in ["delta", "alpha", "echo", "charlie", "bravo", "foxtrot", "golf"].iter().enumerate() {
        assert_eq!(h.put(&format!("/api/queues/%2F/{n}"), json!({"arguments": {"x-max-length": i as i64 + 1}})).await, StatusCode::CREATED);
    }
    let p = h.get("/api/queues/%2F?page=2&page_size=3").await;
    assert_eq!((p["page"].as_u64(), p["page_size"].as_u64(), p["page_count"].as_u64()), (Some(2), Some(3), Some(3)));
    assert_eq!((p["total_count"].as_u64(), p["filtered_count"].as_u64(), p["item_count"].as_u64()), (Some(7), Some(7), Some(3)));
    assert_eq!(names(&p["items"]), vec!["delta", "echo", "foxtrot"]);

    let f = h.get("/api/queues/%2F?page=1&page_size=10&name=lph").await;
    assert_eq!(names(&f["items"]), vec!["alpha"]);
    assert_eq!(f["filtered_count"], 1);
    let f = h.get("/api/queues?page=1&name=%5E(b%7Cc)&use_regex=true").await;
    assert_eq!(names(&f["items"]), vec!["bravo", "charlie"]);

    let s = h.get("/api/queues/%2F?sort=name&sort_reverse=true").await;
    assert_eq!(names(&s)[0], "golf");
    let s = h.get("/api/queues/%2F?sort=arguments.x-max-length").await;
    assert_eq!(names(&s), vec!["delta", "alpha", "echo", "charlie", "bravo", "foxtrot", "golf"]);

    let c = h.get("/api/queues/%2F?columns=name,message_stats.publish,arguments").await;
    assert_eq!(c[0], json!({"name": "alpha", "message_stats": {"publish": 0}, "arguments": {"x-max-length": 2}}));
    let one = h.get("/api/queues/%2F/alpha?columns=name,durable").await;
    assert_eq!(one, json!({"name": "alpha", "durable": true}));

    let d = h.get("/api/queues/%2F?disable_stats=true").await;
    assert!(d[0].get("message_stats").is_none() && d[0].get("messages").is_none() && d[0]["name"] == "alpha");
    let d = h.get("/api/queues/%2F?disable_stats=true&enable_queue_totals=true").await;
    assert!(d[0].get("message_stats").is_none() && d[0]["messages"] == 0 && d[0].get("messages_details").is_none());

    assert_eq!(h.status(Method::GET, "/api/queues/%2F?page=9&page_size=3", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.status(Method::GET, "/api/queues/%2F?page=1&page_size=501", None).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.status(Method::GET, "/api/queues/%2F?page=0", None).await, StatusCode::BAD_REQUEST);
    let empty = h.get("/api/exchanges/%2F?page=1&name=zzz").await;
    assert_eq!((empty["page_count"].as_u64(), empty["items"].clone()), (Some(0), json!([])));
}

#[tokio::test]
async fn policies_parameters_and_global_parameters() {
    let h = Harness::start().await;
    let pol = json!({"pattern": "^ttl\\.", "definition": {"message-ttl": 60000, "max-length": 10}, "priority": 1, "apply-to": "queues"});
    assert_eq!(h.put("/api/policies/%2F/ttl", pol.clone()).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/policies/%2F/ttl", pol).await, StatusCode::NO_CONTENT);
    assert_eq!(
        h.get("/api/policies/%2F/ttl").await,
        json!({"vhost": "/", "name": "ttl", "pattern": "^ttl\\.", "apply-to": "queues", "definition": {"message-ttl": 60000, "max-length": 10}, "priority": 1})
    );
    let r = h.req(Method::PUT, "/api/policies/%2F/bad", Some(json!({"pattern": ".*", "definition": {"bogus": 1}}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["reason"].as_str().unwrap().contains("not recognised policy settings"));
    assert_eq!(h.put("/api/policies/%2F/bad", json!({"pattern": ".*", "definition": {"max-length": 1}, "apply-to": "everything"})).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.put("/api/policies/%2F/nopattern", json!({"definition": {"max-length": 1}})).await, StatusCode::BAD_REQUEST);

    let op = json!({"pattern": ".*", "definition": {"max-length": 5}, "apply-to": "queues"});
    assert_eq!(h.put("/api/operator-policies/%2F/cap", op).await, StatusCode::CREATED);
    assert_eq!(h.get("/api/operator-policies").await[0]["name"], "cap");

    assert_eq!(h.put("/api/queues/%2F/ttl.q", json!({})).await, StatusCode::CREATED);
    let q = h.get("/api/queues/%2F/ttl.q").await;
    assert_eq!(q["policy"], "ttl");
    assert_eq!(q["operator_policy"], "cap");
    assert_eq!(q["effective_policy_definition"], json!({"message-ttl": 60000, "max-length": 5}));
    assert_eq!(h.get("/api/policies").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/policies/%2F").await.as_array().unwrap().len(), 1);
    assert_eq!(h.delete("/api/policies/%2F/ttl").await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete("/api/policies/%2F/ttl").await, StatusCode::NOT_FOUND);
    assert_eq!(h.get("/api/queues/%2F/ttl.q").await["policy"], Value::Null);
    assert_eq!(h.delete("/api/operator-policies/%2F/cap").await, StatusCode::NO_CONTENT);

    let up = json!({"value": {"uri": "amqp://upstream", "expires": 3600000}});
    assert_eq!(h.put("/api/parameters/federation-upstream/%2F/up1", up).await, StatusCode::CREATED);
    assert_eq!(h.get("/api/parameters/federation-upstream/%2F/up1").await["value"]["uri"], "amqp://upstream");
    assert_eq!(h.get("/api/parameters").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/parameters/federation-upstream").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/parameters/shovel/%2F").await, json!([]));
    assert_eq!(h.put("/api/parameters/shovel/%2F/s", json!({})).await, StatusCode::BAD_REQUEST);
    assert_eq!(h.delete("/api/parameters/federation-upstream/%2F/up1").await, StatusCode::NO_CONTENT);
    assert_eq!(h.status(Method::GET, "/api/parameters/federation-upstream/%2F/up1", None).await, StatusCode::NOT_FOUND);

    assert_eq!(h.put("/api/global-parameters/flag", json!({"name": "flag", "value": {"on": true}})).await, StatusCode::CREATED);
    assert_eq!(h.get("/api/global-parameters/flag").await, json!({"name": "flag", "value": {"on": true}}));
    assert!(names(&h.get("/api/global-parameters").await).contains(&"flag".to_string()));
    assert_eq!(h.delete("/api/global-parameters/flag").await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete("/api/global-parameters/flag").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn definitions_roundtrip() {
    let h = Harness::start().await;
    assert_eq!(h.put("/api/vhosts/shop", json!({"description": "Shop", "default_queue_type": "quorum"})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/users/svc", json!({"password": "pw", "tags": "management"})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/permissions/shop/guest", json!({"configure": ".*", "write": ".*", "read": ".*"})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/permissions/shop/svc", json!({"configure": ".*", "write": ".*", "read": ".*"})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/exchanges/shop/orders", json!({"type": "topic"})).await, StatusCode::CREATED);
    assert_eq!(h.put("/api/queues/shop/orders.all", json!({"durable": true})).await, StatusCode::CREATED);
    assert_eq!(h.post("/api/bindings/shop/e/orders/q/orders.all", json!({"routing_key": "#"})).await.status, StatusCode::CREATED);
    assert_eq!(h.put("/api/policies/shop/limit", json!({"pattern": "^orders", "definition": {"max-length": 1000}})).await, StatusCode::CREATED);

    let defs = h.get("/api/definitions").await;
    assert_eq!(defs["rabbitmq_version"], "4.1.0");
    for k in ["users", "vhosts", "permissions", "topic_permissions", "parameters", "global_parameters", "policies", "queues", "exchanges", "bindings"] {
        assert!(defs[k].is_array(), "missing {k}");
    }
    assert!(names(&defs["users"]).contains(&"svc".to_string()));
    let r = h.req(Method::GET, "/api/definitions?download=defs.json", None).await;
    assert_eq!(r.headers.get("content-disposition").unwrap(), "attachment; filename=defs.json");

    let vdefs = h.get("/api/definitions/shop").await;
    assert_eq!(vdefs["queues"][0]["name"], "orders.all");
    assert!(vdefs["queues"][0].get("vhost").is_none());
    assert_eq!(vdefs["metadata"]["description"], "Shop");
    assert!(vdefs.get("users").is_none());

    // import everything into a fresh broker
    let h2 = Harness::start().await;
    let r = h2.post("/api/definitions", defs.clone()).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    assert_eq!(h2.get("/api/vhosts/shop").await["default_queue_type"], "quorum");
    assert_eq!(h2.get("/api/queues/shop/orders.all").await["type"], "quorum");
    assert_eq!(h2.get("/api/bindings/shop/e/orders/q/orders.all").await[0]["routing_key"], "#");
    assert_eq!(h2.get("/api/policies/shop/limit").await["definition"]["max-length"], 1000);
    assert_eq!(h2.req_as(Method::GET, "/api/whoami", None, Some(("svc", "pw"))).await.status, StatusCode::OK);
    let defs2 = h2.get("/api/definitions").await;
    for k in ["queues", "exchanges", "bindings", "policies"] {
        assert_eq!(defs[k], defs2[k], "{k} differ after roundtrip");
    }

    // a vhost export imports into another vhost
    let r = h2.post("/api/definitions/copy", vdefs).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    assert_eq!(h2.get("/api/queues/copy/orders.all").await["vhost"], "copy");
    assert_eq!(h2.status(Method::GET, "/api/definitions/missing", None).await, StatusCode::NOT_FOUND);
    assert_eq!(h2.req(Method::POST, "/api/definitions", None).await.status, StatusCode::NO_CONTENT);
    let r = h2.req(Method::POST, "/api/definitions", Some(json!(["not", "an", "object"]))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn health_checks_and_operational_endpoints() {
    let h = Harness::start().await;
    assert_eq!(h.get("/api/health/checks/alarms").await, json!({"status": "ok"}));
    assert_eq!(h.get("/api/health/checks/local-alarms").await, json!({"status": "ok"}));
    assert_eq!(h.get("/api/health/checks/certificate-expiration/1/months").await["status"], "ok");
    assert_eq!(h.get(&format!("/api/health/checks/port-listener/{}", h.addr.port())).await["status"], "ok");
    let r = h.req(Method::GET, "/api/health/checks/port-listener/1", None).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json()["missing"], 1);
    assert_eq!(h.get("/api/health/checks/protocol-listener/amqp091,http").await["status"], "ok");
    let r = h.req(Method::GET, "/api/health/checks/protocol-listener/mqtt", None).await;
    assert_eq!((r.status, r.json()["missing"].clone()), (StatusCode::SERVICE_UNAVAILABLE, json!(["mqtt"])));
    for c in ["virtual-hosts", "node-is-quorum-critical", "ready-to-serve-clients", "is-in-service", "below-node-connection-limit"] {
        assert_eq!(h.get(&format!("/api/health/checks/{c}")).await["status"], "ok", "{c}");
    }
    h.broker.alarm.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = h.req(Method::GET, "/api/health/checks/alarms", None).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json()["status"], "failed");
    h.broker.alarm.store(false, std::sync::atomic::Ordering::SeqCst);
    // The disk alarm is reported as such, on the node and in the health check.
    h.broker.set_alarm(true, true);
    let r = h.req(Method::GET, "/api/health/checks/local-alarms", None).await;
    assert_eq!(r.json()["alarms"], json!([{"node": h.get("/api/overview").await["node"], "resource": "disk"}]));
    let nodes = h.get("/api/nodes").await;
    assert_eq!((nodes[0]["disk_free_alarm"].clone(), nodes[0]["mem_alarm"].clone()), (json!(true), json!(false)));
    h.broker.set_alarm(true, false);
    assert!(!h.broker.alarm.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(h.get("/api/health/checks/alarms").await, json!({"status": "ok"}));

    assert_eq!(h.get("/api/aliveness-test/%2F").await, json!({"status": "ok"}));
    assert!(names(&h.get("/api/feature-flags").await).contains(&"quorum_queue".to_string()));
    assert_eq!(h.status(Method::PUT, "/api/feature-flags/khepri_db/enable", None).await, StatusCode::NO_CONTENT);
    assert!(h.get("/api/deprecated-features").await.as_array().unwrap().iter().any(|f| f["name"] == "global_qos"));
    assert_eq!(h.get("/api/deprecated-features/used").await, json!([]));
    assert_eq!(h.post("/api/rebalance/queues", json!({})).await.status, StatusCode::NO_CONTENT);
    assert_eq!(h.get("/api/auth/attempts/rabbit@x").await, json!([]));

    assert_eq!(h.get("/api/shovels").await, json!([]));
    assert_eq!(h.get("/api/federation-links/%2F").await, json!([]));
    h.mgmt.set_shovel_status(std::sync::Arc::new(|| vec![json!({"name": "move", "vhost": "/", "type": "dynamic", "state": "running"})]));
    h.mgmt.set_federation_status(std::sync::Arc::new(|| vec![json!({"upstream": "up", "vhost": "/", "status": "running"})]));
    assert_eq!(names(&h.get("/api/shovels").await), vec!["move"]);
    assert_eq!(h.get("/api/shovels/%2F").await.as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/shovels/vhost/%2F/move").await["state"], "running");
    assert_eq!(h.delete("/api/shovels/vhost/%2F/move/restart").await, StatusCode::NOT_FOUND, "no restart hook yet");
    let restarted = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<(String, String)>::new()));
    let r = restarted.clone();
    h.mgmt.set_shovel_restart(std::sync::Arc::new(move |vh: &str, n: &str| {
        r.lock().push((vh.into(), n.into()));
        n == "move"
    }));
    assert_eq!(h.delete("/api/shovels/vhost/%2F/move/restart").await, StatusCode::NO_CONTENT);
    assert_eq!(h.delete("/api/shovels/vhost/%2F/other/restart").await, StatusCode::NOT_FOUND);
    assert_eq!(h.delete("/api/shovels/vhost/nope/move/restart").await, StatusCode::NOT_FOUND, "unknown vhost");
    assert_eq!(*restarted.lock(), vec![("/".to_string(), "move".to_string()), ("/".to_string(), "other".to_string())]);
    assert_eq!(h.get("/api/federation-links").await[0]["status"], "running");
    assert_eq!(h.status(Method::PATCH, "/api/overview", None).await, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn prometheus_metrics() {
    let h = Harness::start().await;
    let conn = h.amqp().await;
    let ch = conn.create_channel().await.unwrap();
    ch.exchange_declare("logs".into(), ExchangeKind::Fanout, ExchangeDeclareOptions::default(), FieldTable::default()).await.unwrap();
    ch.queue_declare("m.q".into(), QueueDeclareOptions::default(), FieldTable::default()).await.unwrap();
    for _ in 0..3 {
        ch.basic_publish("".into(), "m.q".into(), BasicPublishOptions::default(), b"x", BasicProperties::default()).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.mgmt.sample();
    let text = |path: &'static str| {
        let h = &h;
        async move {
            let r = h.req_as(Method::GET, path, None, None).await;
            assert_eq!(r.status, StatusCode::OK);
            assert!(r.headers.get("content-type").unwrap().to_str().unwrap().starts_with("text/plain; version=0.0.4"));
            r.text
        }
    };
    let agg = text("/metrics").await;
    for line in [
        "rabbitmq_queue_messages_ready 3",
        "rabbitmq_queue_messages_unacked 0",
        "rabbitmq_connections 1",
        "rabbitmq_channels 1",
        "rabbitmq_queues 1",
        "rabbitmq_global_messages_received_total{protocol=\"amqp091\"} 3",
        "rabbitmq_global_messages_delivered_total{protocol=\"amqp091\"} 0",
        "# TYPE rabbitmq_global_messages_received_total counter",
        "# HELP rabbitmq_connections Connections currently open",
    ] {
        assert!(agg.contains(line), "missing {line:?} in:\n{agg}");
    }
    assert!(agg.contains("rabbitmq_build_info{rabbitmq_version=\"4.1.0\""));
    assert!(agg.contains("rabbitmq_identity_info{rabbitmq_node=\""));
    assert!(!agg.contains("queue=\"m.q\""), "aggregated output has no per-queue labels");

    let per = text("/metrics/per-object").await;
    assert!(per.contains("rabbitmq_queue_messages_ready{vhost=\"/\",queue=\"m.q\"} 3"), "{per}");
    assert!(per.contains("rabbitmq_channel_messages_published_total{channel=\""));

    let det = text("/metrics/detailed?family=queue_coarse_metrics&family=exchange_names&family=vhost_status&vhost=%2F").await;
    assert!(det.contains("rabbitmq_detailed_queue_messages_ready{vhost=\"/\",queue=\"m.q\"} 3"), "{det}");
    assert!(det.contains("rabbitmq_detailed_exchange_name{vhost=\"/\",exchange=\"logs\",type=\"fanout\"} 1"));
    assert!(det.contains("rabbitmq_detailed_vhost_status{vhost=\"/\"} 1"));
    assert!(!det.contains("rabbitmq_detailed_connection_incoming_bytes_total"));
    let none = text("/metrics/detailed?family=queue_coarse_metrics&vhost=other").await;
    assert!(!none.contains("queue=\"m.q\""));
    drop(conn);
}
