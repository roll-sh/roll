//! The HTTP (Upstash-compatible) API.

use redis_engine::Config;
use redis_server::{Database, Directory, Single};
use serde_json::{Value, json};
use std::sync::Arc;

async fn start(password: Option<&str>) -> (String, Arc<Database>) {
    let db = Database::new("t", Config::default(), 1);
    let dir: Arc<dyn Directory> = Arc::new(Single::new(db.clone(), password.map(String::from)));
    let addr = redis_server::http::start(dir).await.unwrap();
    (format!("http://{addr}"), db)
}

async fn post(url: &str, token: Option<&str>, body: Value) -> (u16, Value) {
    let mut req = reqwest::Client::new().post(url).json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

#[tokio::test]
async fn single_commands() {
    let (url, _db) = start(None).await;
    assert_eq!(post(&url, None, json!(["SET", "k", "v"])).await, (200, json!({"result": "OK"})));
    assert_eq!(post(&url, None, json!(["GET", "k"])).await, (200, json!({"result": "v"})));
    assert_eq!(post(&url, None, json!(["GET", "missing"])).await, (200, json!({"result": null})));
    assert_eq!(post(&url, None, json!(["INCRBY", "n", 5])).await, (200, json!({"result": 5})));
    assert_eq!(post(&url, None, json!(["MGET", "k", "n", "x"])).await, (200, json!({"result": ["v", "5", null]})));
    let (status, body) = post(&url, None, json!(["INCR", "k"])).await;
    assert_eq!(status, 400);
    assert_eq!(body, json!({"error": "ERR value is not an integer or out of range"}));
    let (status, body) = post(&url, None, json!(["NOPE"])).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().starts_with("ERR unknown command"));
    let (status, _) = post(&url, None, json!("SET")).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn path_style() {
    let (url, _db) = start(None).await;
    let c = reqwest::Client::new();
    let r: Value = c.get(format!("{url}/set/a%20b/hello?EX=100")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"result": "OK"}));
    let r: Value = c.get(format!("{url}/ttl/a%20b")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"result": 100}));
    // POST body becomes the last argument, binary safe.
    let r: Value = c.post(format!("{url}/set/bin")).body(vec![0u8, 1, 2, 255]).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"result": "OK"}));
    let r: Value = c.get(format!("{url}/get/bin")).header("Upstash-Encoding", "base64").send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"result": "AAEC/w=="}));
    let r: Value = c.get(format!("{url}/set/k/v?NX")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"result": "OK"}));
    let r: Value = c.get(format!("{url}/set/k/v?NX")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"result": null}));
}

#[tokio::test]
async fn pipeline_and_transactions() {
    let (url, _db) = start(None).await;
    let (status, body) = post(&format!("{url}/pipeline"), None, json!([["SET", "a", "1"], ["INCR", "a"], ["LPUSHX"], ["GET", "a"]])).await;
    assert_eq!(status, 200);
    assert_eq!(body[0], json!({"result": "OK"}));
    assert_eq!(body[1], json!({"result": 2}));
    assert!(body[2]["error"].is_string());
    assert_eq!(body[3], json!({"result": "2"}));

    let (status, body) = post(&format!("{url}/multi-exec"), None, json!([["SET", "t", "x"], ["INCR", "t"], ["GET", "t"]])).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!([{"result": "OK"}, {"error": "ERR value is not an integer or out of range"}, {"result": "x"}]));

    // A queueing error aborts the whole transaction.
    let (status, body) = post(&format!("{url}/multi-exec"), None, json!([["SET", "t", "y"], ["GET"]])).await;
    assert_eq!(status, 400);
    assert_eq!(body, json!({"error": "ERR wrong number of arguments for 'get' command"}));
    assert_eq!(post(&url, None, json!(["GET", "t"])).await.1, json!({"result": "x"}));
}

#[tokio::test]
async fn auth() {
    let (url, _db) = start(Some("s3cret")).await;
    let (status, body) = post(&url, None, json!(["PING"])).await;
    assert_eq!((status, body), (401, json!({"error": "Unauthorized"})));
    assert_eq!(post(&url, Some("wrong"), json!(["PING"])).await.0, 401);
    assert_eq!(post(&url, Some("s3cret"), json!(["PING"])).await, (200, json!({"result": "PONG"})));
    let r: Value = reqwest::get(format!("{url}/echo/hi?_token=s3cret")).await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"result": "hi"}));
}

#[tokio::test]
async fn resp2_format() {
    let (url, _db) = start(None).await;
    let r = reqwest::Client::new()
        .post(&url)
        .header("Upstash-Response-Format", "resp2")
        .json(&json!(["ECHO", "hi"]))
        .send()
        .await
        .unwrap();
    assert_eq!(&r.bytes().await.unwrap()[..], b"$2\r\nhi\r\n");
}

#[tokio::test]
async fn subscribe_sse() {
    let (url, _db) = start(None).await;
    let mut r = reqwest::get(format!("{url}/subscribe/news")).await.unwrap();
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    let mut buf = String::new();
    while !buf.contains("subscribe,news,1") {
        buf.push_str(&String::from_utf8_lossy(&r.chunk().await.unwrap().unwrap()));
    }
    let (_, n) = post(&url, None, json!(["PUBLISH", "news", "hello"])).await;
    assert_eq!(n, json!({"result": 1}));
    while !buf.contains("message,news,hello") {
        buf.push_str(&String::from_utf8_lossy(&r.chunk().await.unwrap().unwrap()));
    }
    drop(r);
    // The subscription goes away with the stream.
    for _ in 0..50 {
        if post(&url, None, json!(["PUBSUB", "NUMSUB", "news"])).await.1 == json!({"result": ["news", 0]}) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("subscription leaked");
}
