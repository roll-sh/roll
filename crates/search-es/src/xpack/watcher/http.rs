//! HTTP requests of watches: the `http` input and the `webhook` action (`HttpRequestTemplate`,
//! `HttpClient`).
//!
//! A request template is stored the way Elasticsearch stores it (a `url` split into scheme,
//! host, port, path and params; methods in lower case), printed with its secrets redacted,
//! rendered with the execution's mustache model, and sent.
//!
//! The URL is chosen by the tenant, so roll's rule for such URLs applies (AGENTS.md): a client
//! that resolves names to public addresses only ([`roll_net::PublicOnly`]), refuses private IP
//! literals and `localhost` ([`roll_net::literal_private_host`]), follows no redirects, uses no
//! proxy and reads at most `xpack.http.max_response_size` (10 MiB). The node setting
//! [`ALLOW_PRIVATE`] lifts the address check, for test clusters whose watches call the node
//! itself; it is off unless set.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// Lets watches reach private addresses (loopback, private ranges, `localhost`).
pub const ALLOW_PRIVATE: &str = "roll.watcher.allow_private_hosts";

pub const REDACTED: &str = "::es_redacted::";

const MAX_RESPONSE: usize = 10 * 1024 * 1024;

fn parse_err(msg: impl Into<String>) -> EsError {
    EsError::new(400, "parse_exception", msg)
}

/// Parses a request template into its stored form (`HttpRequestTemplate.Parser`). `previous`:
/// the stored template this one replaces, whose password a redacted one keeps (only when the
/// caller allows it: an update with `if_seq_no`).
pub fn normalize(v: &Value, previous: Option<&Value>) -> EsResult<Map> {
    let Value::Obj(r) = v else { return Err(parse_err("could not parse http request template. expected an object")) };
    let mut scheme = None;
    let mut host = None;
    let mut port = None;
    let mut path = None;
    let mut params = Map::new();
    if let Some(u) = r.get("url") {
        let u = u.to_es_string();
        if u.is_empty() {
            return Err(parse_err("Configured URL is empty, please configure a valid URL"));
        }
        let url = reqwest::Url::parse(&u).ok().filter(|x| x.host_str().is_some_and(|h| !h.is_empty())).ok_or_else(|| parse_err(format!("Malformed URL [{u}]")))?;
        scheme = Some(url.scheme().to_string());
        host = url.host_str().map(str::to_string);
        port = url.port_or_known_default().map(i64::from);
        if !url.path().is_empty() {
            path = Some(Value::from(url.path()));
        }
        let keys: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let mut m = Map::new();
        for (k, v) in keys {
            m.insert(k, Value::from(v));
        }
        params = crate::json::java_hash_map(m, 16);
    }
    let scheme = r.get("scheme").map(|s| s.to_es_string().to_ascii_lowercase()).or(scheme).unwrap_or_else(|| "http".into());
    if scheme != "http" && scheme != "https" {
        return Err(EsError::illegal_argument(format!("unsupported http scheme [{scheme}]")));
    }
    let host = r.get("host").map(Value::to_es_string).or(host).ok_or_else(|| parse_err("could not parse http request template. missing required [host] string field"))?;
    let port = r.get("port").and_then(Value::as_i64).or(port).ok_or_else(|| parse_err("could not parse http request template. missing required [port] numeric field"))?;
    let method = r.get("method").map(|m| m.to_es_string().to_ascii_lowercase()).unwrap_or_else(|| "get".into());
    if !matches!(method.as_str(), "get" | "post" | "put" | "delete" | "head") {
        return Err(EsError::illegal_argument(format!("unsupported http method [{}]", method.to_ascii_uppercase())));
    }
    let mut out = Map::new();
    out.insert("scheme".into(), scheme.into());
    out.insert("host".into(), host.into());
    out.insert("port".into(), port.into());
    out.insert("method".into(), method.into());
    if let Some(p) = r.get("path").cloned().or(path) {
        out.insert("path".into(), p);
    }
    if let Some(Value::Obj(p)) = r.get("params") {
        for (k, v) in p {
            params.insert(k.clone(), v.clone());
        }
    }
    out.insert("params".into(), Value::Obj(params));
    // Only the cluster seals values: one given sealed would be opened when the watch runs.
    if let Some(Value::Obj(h)) = r.get("headers")
        && h.iter().any(|(k, v)| k.eq_ignore_ascii_case("authorization") && crate::seal::is_sealed(&v.to_es_string()))
    {
        return Err(parse_err("found a sealed value in header [Authorization]"));
    }
    out.insert("headers".into(), r.get("headers").cloned().unwrap_or_else(Value::object));
    if let Some(a) = r.get("auth") {
        let user = a.pointer(&["basic", "username"]).map(Value::to_es_string);
        let pass = a.pointer(&["basic", "password"]).map(Value::to_es_string);
        let (Some(user), Some(mut pass)) = (user, pass) else { return Err(parse_err("could not parse http request template. unknown auth type")) };
        if crate::seal::is_sealed(&pass) {
            return Err(parse_err("found a sealed value in field [password]"));
        }
        if pass == REDACTED {
            match previous.and_then(|p| p.pointer(&["auth", "basic", "password"])).map(Value::to_es_string) {
                Some(old) => pass = old,
                None => return Err(parse_err("found redacted password in field [password]")),
            }
        }
        out.insert("auth".into(), obj! { "basic" => obj! { "username" => user, "password" => pass } });
    }
    for k in ["body", "proxy"] {
        if let Some(v) = r.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    for (k, out_k) in [("connection_timeout", "connection_timeout_in_millis"), ("read_timeout", "read_timeout_millis")] {
        match (r.get(out_k), r.get(k)) {
            (Some(v), _) => {
                out.insert(out_k.into(), v.clone());
            }
            (None, Some(v)) => {
                out.insert(out_k.into(), crate::units::TimeValue::parse(&v.to_es_string(), k)?.as_millis().into());
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Kinds of sealed values ([`crate::seal`]).
const PASSWORD: &str = "watch password";
const AUTH_HEADER: &str = "watch authorization header";

/// A normalized template as the cluster stores it: its password and `Authorization` headers
/// sealed ([`crate::seal`]).
pub fn seal(cluster: &Cluster, stored: &Value) -> Value {
    let mut v = stored.clone();
    if let Some(Value::Obj(a)) = v.as_object_mut().and_then(|m| m.get_mut("auth")).and_then(|a| a.as_object_mut()).and_then(|a| a.get_mut("basic"))
        && let Some(p) = a.get("password").map(Value::to_es_string)
    {
        a.insert("password".into(), crate::seal::seal(cluster, PASSWORD, &p).into());
    }
    if let Some(Value::Obj(h)) = v.as_object_mut().and_then(|m| m.get_mut("headers")) {
        for (k, val) in h.iter_mut() {
            if k.eq_ignore_ascii_case("authorization") && let Value::Str(s) = val {
                *val = Value::from(crate::seal::seal(cluster, AUTH_HEADER, s));
            }
        }
    }
    v
}

/// A stored template as it is printed: the password and any `Authorization` header redacted.
pub fn redact(stored: &Value) -> Value {
    let mut v = stored.clone();
    if let Some(Value::Obj(a)) = v.as_object_mut().and_then(|m| m.get_mut("auth")).and_then(|a| a.as_object_mut()).and_then(|a| a.get_mut("basic")) {
        a.insert("password".into(), REDACTED.into());
    }
    if let Some(Value::Obj(h)) = v.as_object_mut().and_then(|m| m.get_mut("headers")) {
        for (k, val) in h.iter_mut() {
            if k.eq_ignore_ascii_case("authorization") {
                *val = Value::from(REDACTED);
            }
        }
    }
    v
}

/// A request ready to send (its secrets in the clear: `Debug` prints them redacted).
#[derive(Clone)]
pub struct Request {
    pub scheme: String,
    pub host: String,
    pub port: i64,
    pub method: String,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub auth: Option<(String, String)>,
    pub body: Option<String>,
    pub connect_timeout_ms: i64,
    pub read_timeout_ms: i64,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_record().to_json())
    }
}

/// Renders a stored template with the execution model (`render`: one text template). Its
/// sealed `Authorization` headers and password are opened first: a value that only renders to
/// sealed text (from the payload) is sent as it is, never opened.
pub fn render(cluster: &Cluster, stored: &Map, render: &dyn Fn(&Value) -> EsResult<(String, bool)>) -> EsResult<Request> {
    let text = |k: &str| -> EsResult<Option<String>> { stored.get(k).map(|v| render(v).map(|r| r.0)).transpose() };
    let mut headers = Vec::new();
    for (k, v) in stored.get("headers").and_then(Value::as_object).into_iter().flatten() {
        let v = match v {
            Value::Str(t) if k.eq_ignore_ascii_case("authorization") => Value::from(crate::seal::open(cluster, AUTH_HEADER, t)?),
            other => other.clone(),
        };
        headers.push((k.clone(), render(&v)?.0));
    }
    let mut params = Vec::new();
    for (k, v) in stored.get("params").and_then(Value::as_object).into_iter().flatten() {
        params.push((k.clone(), render(v)?.0));
    }
    let body = match stored.get("body") {
        None => None,
        Some(b) => {
            let (s, json) = render(b)?;
            // A template whose source is JSON renders JSON, and says so.
            if json && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                headers.push(("Content-Type".into(), "application/json;charset=utf-8".into()));
            }
            Some(s)
        }
    };
    let auth = match stored.get("auth").and_then(|a| a.get("basic")) {
        Some(b) => Some((b.get("username").map(Value::to_es_string).unwrap_or_default(), crate::seal::open(cluster, PASSWORD, &b.get("password").map(Value::to_es_string).unwrap_or_default())?)),
        None => None,
    };
    Ok(Request {
        scheme: stored.get("scheme").map(Value::to_es_string).unwrap_or_else(|| "http".into()),
        host: text("host")?.unwrap_or_default(),
        port: stored.get("port").and_then(Value::as_i64).unwrap_or(80),
        method: stored.get("method").map(Value::to_es_string).unwrap_or_else(|| "get".into()),
        path: text("path")?.unwrap_or_default(),
        params,
        headers,
        auth,
        body,
        connect_timeout_ms: stored.get("connection_timeout_in_millis").and_then(Value::as_i64).unwrap_or(10_000),
        read_timeout_ms: stored.get("read_timeout_millis").and_then(Value::as_i64).unwrap_or(10_000),
    })
}

impl Request {
    /// The request as a watch record prints it (`HttpRequest.toXContent`, secrets hidden).
    pub fn to_record(&self) -> Value {
        let mut m = Map::new();
        m.insert("host".into(), self.host.clone().into());
        m.insert("port".into(), self.port.into());
        m.insert("scheme".into(), self.scheme.clone().into());
        m.insert("method".into(), self.method.clone().into());
        if !self.path.is_empty() {
            m.insert("path".into(), self.path.clone().into());
        }
        if !self.params.is_empty() {
            m.insert("params".into(), Value::Obj(self.params.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect()));
        }
        if !self.headers.is_empty() {
            m.insert(
                "headers".into(),
                Value::Obj(self.headers.iter().map(|(k, v)| (k.clone(), Value::from(if k.eq_ignore_ascii_case("authorization") { REDACTED.to_string() } else { v.clone() }))).collect()),
            );
        }
        if let Some((u, _)) = &self.auth {
            m.insert("auth".into(), obj! { "basic" => obj! { "username" => u.clone(), "password" => REDACTED } });
        }
        if let Some(b) = &self.body {
            m.insert("body".into(), b.clone().into());
        }
        Value::Obj(m)
    }

    /// The URL: the path with characters a URL can't hold escaped (an already escaped `%xx`
    /// stays), params form-encoded.
    fn url(&self) -> String {
        let mut path = String::new();
        for c in self.path.chars() {
            if c.is_ascii_alphanumeric() || "/-._~!$&'()*+,;=:@%".contains(c) {
                path.push(c);
            } else {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    path.push_str(&format!("%{b:02X}"));
                }
            }
        }
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        let mut url = format!("{}://{}:{}{path}", self.scheme, self.host, self.port);
        if !self.params.is_empty() {
            url.push('?');
            let q: Vec<String> = self.params.iter().map(|(k, v)| format!("{}={}", form(k), form(v))).collect();
            url.push_str(&q.join("&"));
        }
        url
    }
}

fn form(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// Header names in lower case, each with its values.
    pub headers: Vec<(String, Vec<String>)>,
    pub body: String,
}

impl Response {
    pub fn headers_value(&self) -> Value {
        let m: Map = self.headers.iter().map(|(k, v)| (k.clone(), Value::Arr(v.iter().map(|x| Value::from(x.clone())).collect()))).collect();
        Value::Obj(crate::json::java_hash_map(m, 16))
    }

    /// The response as a webhook result prints it.
    pub fn to_record(&self) -> Value {
        let mut m = Map::new();
        m.insert("status".into(), i64::from(self.status).into());
        if !self.headers.is_empty() {
            m.insert("headers".into(), self.headers_value());
        }
        if !self.body.is_empty() {
            m.insert("body".into(), self.body.clone().into());
        }
        Value::Obj(m)
    }

    /// Its content type is JSON.
    pub fn is_json(&self) -> bool {
        self.headers.iter().any(|(k, v)| k == "content-type" && v.iter().any(|x| x.contains("json")))
    }
}

fn connect_error(e: &reqwest::Error, host: &str, port: i64) -> EsError {
    if e.is_timeout() {
        return EsError::java(500, "socket_timeout_exception", "Read timed out");
    }
    let detail = {
        let mut s = e.to_string();
        let mut src = std::error::Error::source(e);
        while let Some(x) = src {
            s = x.to_string();
            src = x.source();
        }
        s
    };
    EsError::java(500, "http_host_connect_exception", format!("Connect to {host}:{port} failed: {detail}")).caused_by(EsError::java(500, "connect_exception", detail))
}

/// Sends a request.
pub async fn send(cluster: &Cluster, req: &Request) -> EsResult<Response> {
    let allow_private = cluster.config.node_setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let url = req.url();
    let parsed = reqwest::Url::parse(&url).map_err(|_| EsError::illegal_argument(format!("Malformed URL [{url}]")))?;
    if !allow_private && roll_net::literal_private_host(&parsed) {
        return Err(EsError::illegal_argument(format!("host [{}] is a private address, which watches may not connect to unless [{ALLOW_PRIVATE}] is set", req.host)));
    }
    let mut b = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(req.connect_timeout_ms.max(1) as u64))
        .read_timeout(std::time::Duration::from_millis(req.read_timeout_ms.max(1) as u64))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if !allow_private {
        b = b.dns_resolver(std::sync::Arc::new(roll_net::PublicOnly));
    }
    let client = b.build().map_err(|e| EsError::illegal_state(e.to_string()))?;
    let method = reqwest::Method::from_bytes(req.method.to_ascii_uppercase().as_bytes()).map_err(|_| EsError::illegal_argument(format!("unsupported http method [{}]", req.method)))?;
    let mut rb = client.request(method, parsed).header("Accept-Charset", "UTF-8");
    for (k, v) in &req.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if let Some((u, p)) = &req.auth {
        rb = rb.basic_auth(u, Some(p));
    }
    if let Some(body) = &req.body {
        if !req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            rb = rb.header("Content-Type", "application/json;charset=utf-8");
        }
        rb = rb.body(body.clone());
    }
    let mut resp = rb.send().await.map_err(|e| connect_error(&e, &req.host, req.port))?;
    let status = resp.status().as_u16();
    let mut headers: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in resp.headers() {
        let k = k.as_str().to_ascii_lowercase();
        let v = String::from_utf8_lossy(v.as_bytes()).into_owned();
        match headers.iter_mut().find(|(n, _)| *n == k) {
            Some((_, vals)) => vals.push(v),
            None => headers.push((k, vec![v])),
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| connect_error(&e, &req.host, req.port))? {
        if body.len() + chunk.len() > MAX_RESPONSE {
            return Err(EsError::illegal_state(format!("Maximum limit of [{MAX_RESPONSE}] bytes reached")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Response { status, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_redaction() {
        let t = normalize(&obj! { "url" => "http://h.example/p?a=b", "auth" => obj! { "basic" => obj! { "username" => "u", "password" => "s" } } }, None).unwrap();
        assert_eq!(t.get("port"), Some(&Value::from(80)));
        assert_eq!(t.get("path"), Some(&Value::from("/p")));
        assert_eq!(redact(&Value::Obj(t.clone())).pointer(&["auth", "basic", "password"]), Some(&Value::from(REDACTED)));
        let e = normalize(&obj! { "url" => "" }, None).unwrap_err();
        assert_eq!(e.reason(), "Configured URL is empty, please configure a valid URL");
        assert_eq!(normalize(&obj! { "url" => "https://" }, None).unwrap_err().reason(), "Malformed URL [https://]");
        let again = obj! { "host" => "h", "port" => 1, "auth" => obj! { "basic" => obj! { "username" => "u", "password" => REDACTED } } };
        assert_eq!(normalize(&again, None).unwrap_err().reason(), "found redacted password in field [password]");
        let kept = normalize(&again, Some(&Value::Obj(t))).unwrap();
        assert_eq!(kept.get("auth").and_then(|a| a.pointer(&["basic", "password"])), Some(&Value::from("s")));
        let r = Request {
            scheme: "http".into(),
            host: "h".into(),
            port: 9,
            method: "put".into(),
            path: "/a b/%3Cx%3E".into(),
            params: vec![("q".into(), "a b".into())],
            headers: vec![],
            auth: None,
            body: None,
            connect_timeout_ms: 1,
            read_timeout_ms: 1,
        };
        assert_eq!(r.url(), "http://h:9/a%20b/%3Cx%3E?q=a+b");
    }
}
