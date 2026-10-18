//! Test harness: an in-process broker with the AMQP server and the management API on
//! ephemeral ports.

#![allow(dead_code)]

use mq_core::log::MemStore;
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use mq_mgmt::{Config, Mgmt};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct Harness {
    pub broker: Arc<Broker>,
    pub mgmt: Arc<Mgmt>,
    pub base: String,
    pub addr: SocketAddr,
    pub amqp: String,
    pub http: reqwest::Client,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for s in &self.servers {
            s.abort();
        }
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub text: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text))
    }
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_on("127.0.0.1").await
    }

    /// Listen on `ip` (`0.0.0.0` for tests driven from Docker containers).
    pub async fn start_on(ip: &str) -> Self {
        let _ = tracing_subscriber::fmt().with_env_filter("mq_core=warn,mq_mgmt=info").with_test_writer().try_init();
        let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        Self::with_broker(broker, ip).await
    }

    pub async fn with_broker(broker: Arc<Broker>, ip: &str) -> Self {
        let amqp_listener = tokio::net::TcpListener::bind(format!("{ip}:0")).await.unwrap();
        let amqp_addr = amqp_listener.local_addr().unwrap();
        let cfg = mq_amqp::ServerConfig { listen: amqp_addr, amqp10: None };
        let b = broker.clone();
        let amqp_server = tokio::spawn(async move {
            let _ = mq_amqp::serve_listener(b, amqp_listener, cfg).await;
        });
        let listener = tokio::net::TcpListener::bind(format!("{ip}:0")).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgmt = Mgmt::new(broker.clone(), Config { listen: addr, dashboard_url: Some("http://localhost:3000/queues".into()), ..Default::default() });
        let m = mgmt.clone();
        let server = tokio::spawn(async move {
            let _ = mq_mgmt::serve_mgmt(m, listener).await;
        });
        let port = addr.port();
        Harness {
            broker,
            mgmt,
            base: format!("http://127.0.0.1:{port}"),
            addr,
            amqp: format!("amqp://guest:guest@127.0.0.1:{}/%2f", amqp_addr.port()),
            http: reqwest::Client::new(),
            servers: vec![amqp_server, server],
        }
    }

    pub async fn req_as(&self, method: Method, path: &str, body: Option<Value>, creds: Option<(&str, &str)>) -> Resp {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
        if let Some((u, p)) = creds {
            r = r.basic_auth(u, Some(p));
        }
        if let Some(b) = body {
            r = r.header("content-type", "application/json").body(b.to_string());
        }
        let resp = r.send().await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        Resp { status, headers, text: resp.text().await.unwrap() }
    }

    pub async fn req(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        self.req_as(method, path, body, Some(("guest", "guest"))).await
    }

    /// GET expecting 200; returns the JSON body.
    pub async fn get(&self, path: &str) -> Value {
        let r = self.req(Method::GET, path, None).await;
        assert_eq!(r.status, StatusCode::OK, "GET {path}: {}", r.text);
        r.json()
    }

    pub async fn status(&self, method: Method, path: &str, body: Option<Value>) -> StatusCode {
        self.req(method, path, body).await.status
    }

    pub async fn put(&self, path: &str, body: Value) -> StatusCode {
        self.status(Method::PUT, path, Some(body)).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Resp {
        self.req(Method::POST, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> StatusCode {
        self.status(Method::DELETE, path, None).await
    }

    pub async fn amqp(&self) -> lapin::Connection {
        lapin::Connection::connect(&self.amqp, lapin::ConnectionProperties::default()).await.unwrap()
    }
}

/// Names of the items in a JSON array.
pub fn names(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().filter_map(|i| i["name"].as_str().map(String::from)).collect()
}
