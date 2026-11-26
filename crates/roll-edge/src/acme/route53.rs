//! Amazon Route 53 for DNS-01 ([`DnsProvider`]): the hosted zone of each name by
//! `ListHostedZonesByName`, the `_acme-challenge` TXT record set read (`ListResourceRecordSets`) and
//! written back with `ChangeResourceRecordSets`, and each change waited on with `GetChange` until it's `INSYNC`
//! ([`DnsProvider::settle`]), within a bound. Requests are signed with SigV4 (`aws-sigv4`, service
//! `route53`, `us-east-1`: Route 53 is global). The caller's HTTP client must not follow redirects
//! (rolld's also resolves public addresses only); answers are read up to [`Route53::MAX_RESPONSE`].
//!
//! A record set holds every value of its name: a wildcard and its apex (`*.example.com` and
//! `example.com`, one `_acme-challenge.example.com`) are one set of two values, and another ACME
//! client (cert-manager, Caddy, another roll order for an overlapping name) may have its own value
//! there at the same time. So each change reads the set as it stands and adds or removes only this
//! client's own value: UPSERT the set with it added, UPSERT the rest without it, and DELETE only when
//! nothing else is left (with the values and TTL just read, which DELETE must match exactly). A
//! DELETE that no longer matches (someone wrote in between) is read and tried again once. Route 53
//! has no compare-and-swap for an UPSERT, so two writers within the same instant can still lose one
//! value, as lego's provider can. The client is made per order: the values it removes are only the
//! ones it wrote.

use super::{DnsProvider, read_bounded, short};
use anyhow::Context as _;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// AWS credentials: an access key, or a role's session (with its token).
#[derive(Clone)]
pub struct AwsKeys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsKeys").finish_non_exhaustive()
    }
}

pub struct Route53 {
    api: String,
    keys: AwsKeys,
    http: reqwest::Client,
    /// Zone name → id (`Z…`).
    zones: Mutex<HashMap<String, String>>,
    /// Record name → its zone, and the values this client wrote there (others' aren't in it).
    records: Mutex<HashMap<String, (String, Vec<String>)>>,
    /// Changes not seen `INSYNC` yet.
    pending: Mutex<Vec<String>>,
    poll: Duration,
    within: Duration,
}

impl std::fmt::Debug for Route53 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route53").field("api", &self.api).finish_non_exhaustive()
    }
}

const VERSION: &str = "2013-04-01";
const TTL: u32 = 60;
/// Throttled calls (Route 53 takes five a second per account) are tried this many times more.
const RETRIES: u32 = 3;

impl Route53 {
    pub const API: &'static str = "https://route53.amazonaws.com";
    /// The most of an answer read: Route 53's are a few KiB.
    pub const MAX_RESPONSE: usize = 1 << 20;

    /// `api`: [`Route53::API`], or a stand-in (tests). `http` must not follow redirects.
    pub fn new(keys: AwsKeys, api: &str, http: reqwest::Client) -> anyhow::Result<Self> {
        anyhow::ensure!(!keys.access_key_id.trim().is_empty() && !keys.secret_access_key.trim().is_empty(), "Route 53: the AWS access key is empty");
        Ok(Self { api: api.trim_end_matches('/').to_string(), keys, http, zones: Mutex::default(), records: Mutex::default(), pending: Mutex::default(), poll: Duration::from_secs(2), within: Duration::from_secs(180) })
    }

    /// How often a change is looked at, and how long it may take to be `INSYNC`.
    pub fn with_poll(mut self, every: Duration, within: Duration) -> Self {
        (self.poll, self.within) = (every, within);
        self
    }

    /// One signed call: its answer's body on a 2xx, else Route 53's error code and message.
    async fn call(&self, method: &str, path_and_query: &str, body: Option<String>, what: &str) -> anyhow::Result<String> {
        match self.call_retried(method, path_and_query, body.as_deref(), what).await? {
            Ok(text) => Ok(text),
            Err((code, message)) => anyhow::bail!("Route 53: {what}: {code}: {message}"),
        }
    }

    /// [`Route53::call`], with Route 53's refusal (its code and message) as a value; throttled
    /// calls tried again.
    async fn call_retried(&self, method: &str, path_and_query: &str, body: Option<&str>, what: &str) -> anyhow::Result<Result<String, (String, String)>> {
        let mut attempt = 0;
        loop {
            match self.call_once(method, path_and_query, body, what).await? {
                Err((code, _)) if attempt < RETRIES && matches!(code.as_str(), "Throttling" | "PriorRequestNotComplete") => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
                }
                answer => return Ok(answer),
            }
        }
    }

    async fn call_once(&self, method: &str, path_and_query: &str, body: Option<&str>, what: &str) -> anyhow::Result<Result<String, (String, String)>> {
        let url = format!("{}{path_and_query}", self.api);
        let parsed = reqwest::Url::parse(&url).with_context(|| format!("Route 53: {what}: bad URL"))?;
        let host = match parsed.port() {
            Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or_default()),
            None => parsed.host_str().unwrap_or_default().to_string(),
        };
        let bytes = body.unwrap_or_default().as_bytes();
        let mut unsigned = vec![("host", host.as_str())];
        if body.is_some() {
            unsigned.push(("content-type", "text/xml"));
        }
        let k = &self.keys;
        let identity: aws_smithy_runtime_api::client::identity::Identity = aws_credential_types::Credentials::new(k.access_key_id.trim(), k.secret_access_key.trim(), k.session_token.clone(), None, "roll-acme").into();
        let params: aws_sigv4::http_request::SigningParams = aws_sigv4::sign::v4::SigningParams::builder()
            .identity(&identity)
            .region("us-east-1")
            .name("route53")
            .time(SystemTime::now())
            .settings(aws_sigv4::http_request::SigningSettings::default())
            .build()
            .map_err(|e| anyhow::anyhow!("Route 53: {what}: signing: {e}"))?
            .into();
        let signable = aws_sigv4::http_request::SignableRequest::new(method, url.as_str(), unsigned.iter().copied(), aws_sigv4::http_request::SignableBody::Bytes(bytes)).map_err(|e| anyhow::anyhow!("Route 53: {what}: signing: {e}"))?;
        let (instructions, _) = aws_sigv4::http_request::sign(signable, &params).map_err(|e| anyhow::anyhow!("Route 53: {what}: signing: {e}"))?.into_parts();
        let method = reqwest::Method::from_bytes(method.as_bytes()).context("method")?;
        let mut req = self.http.request(method, parsed);
        if body.is_some() {
            req = req.header("content-type", "text/xml").body(bytes.to_vec());
        }
        for (name, value) in instructions.headers() {
            req = req.header(name, value);
        }
        // Never the request in an error: it carries the signature.
        let resp = req.send().await.map_err(|e| anyhow::anyhow!("Route 53: {what}: {}", e.without_url()))?;
        let status = resp.status();
        let bytes = read_bounded(resp, Self::MAX_RESPONSE).await.with_context(|| format!("Route 53: {what}: HTTP {status}"))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if status.is_success() {
            return Ok(Ok(text));
        }
        let code = first(&text, "Code").unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        Ok(Err((short(&code, 60), short(&first(&text, "Message").unwrap_or_default(), 200))))
    }

    /// The id of the public hosted zone named exactly `zone`, if the credentials see one.
    pub async fn zone_id(&self, zone: &str) -> anyhow::Result<Option<String>> {
        if let Some(id) = self.zones.lock().unwrap().get(zone).cloned() {
            return Ok(Some(id));
        }
        // Zone names are DNS names: nothing in them needs escaping in a query.
        let xml = self.call("GET", &format!("/{VERSION}/hostedzonesbyname?dnsname={zone}&maxitems=10"), None, &format!("find hosted zone {zone}")).await?;
        // Zones come in name order from `zone` on: the first ones may be other names, and a
        // private zone (its VPC's view) is no use to a public CA.
        let want = format!("{zone}.");
        let found = blocks(&xml, "HostedZone").into_iter().find(|z| first(z, "Name").is_some_and(|n| n.eq_ignore_ascii_case(&want)) && first(z, "PrivateZone").as_deref() != Some("true"));
        let Some(z) = found else { return Ok(None) };
        let id = first(z, "Id").unwrap_or_default();
        let id = id.trim_start_matches("/hostedzone/");
        anyhow::ensure!(!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()), "Route 53: find hosted zone {zone}: an odd zone id");
        self.zones.lock().unwrap().insert(zone.to_string(), id.to_string());
        Ok(Some(id.to_string()))
    }

    /// The id of the public hosted zone `name` is in: its parent domains', longest first.
    pub async fn zone_for(&self, name: &str) -> anyhow::Result<String> {
        let labels: Vec<&str> = name.split('.').collect();
        for i in 1..labels.len().saturating_sub(1) {
            if let Some(id) = self.zone_id(&labels[i..].join(".")).await? {
                return Ok(id);
            }
        }
        anyhow::bail!("Route 53: no public hosted zone for {name} (do the credentials see it?)")
    }

    /// `name`'s TXT set as it stands: its TTL and values (as Route 53 has them, quoted), or `None`.
    async fn read_set(&self, zone: &str, name: &str) -> anyhow::Result<Option<(u32, Vec<String>)>> {
        // The sets from `name` on, in name and type order: the first may be another name's.
        let xml = self.call("GET", &format!("/{VERSION}/hostedzone/{zone}/rrset?name={name}.&type=TXT&maxitems=1"), None, &format!("read TXT {name}")).await?;
        let want = format!("{name}.");
        let Some(set) = blocks(&xml, "ResourceRecordSet").into_iter().find(|s| first(s, "Name").is_some_and(|n| n.eq_ignore_ascii_case(&want)) && first(s, "Type").as_deref() == Some("TXT")) else { return Ok(None) };
        let ttl = first(set, "TTL").and_then(|t| t.parse().ok()).unwrap_or(TTL);
        Ok(Some((ttl, blocks(set, "Value").iter().map(|v| unescape(v)).collect())))
    }

    /// Add (`add`) or take out this client's `value` in `name`'s TXT set, keeping every other value
    /// in it: read, then UPSERT the set changed, or DELETE it when nothing else is left. Read and
    /// tried again once when the set moved before the DELETE.
    async fn edit(&self, zone: &str, name: &str, value: &str, add: bool) -> anyhow::Result<()> {
        let ours = format!("\"{value}\"");
        let mut attempt = 0;
        loop {
            let (ttl, mut values) = self.read_set(zone, name).await?.unwrap_or((TTL, vec![]));
            let before = values.len();
            values.retain(|v| *v != ours);
            let (action, values) = match (add, values.is_empty()) {
                (true, _) => ("UPSERT", values.into_iter().chain([ours.clone()]).collect::<Vec<_>>()),
                // Gone already (someone took it down): nothing to write.
                (false, _) if values.len() == before => return Ok(()),
                (false, false) => ("UPSERT", values),
                (false, true) => ("DELETE", vec![ours.clone()]),
            };
            match self.change(zone, action, name, ttl, &values).await? {
                Ok(()) => return Ok(()),
                Err((code, _)) if attempt == 0 && code == "InvalidChangeBatch" && action == "DELETE" => attempt += 1,
                Err((code, message)) => anyhow::bail!("Route 53: {} TXT {name}: {code}: {message}", action.to_ascii_lowercase()),
            }
        }
    }

    /// Write `name`'s TXT set as `values` (each quoted, as Route 53 holds it) with `ttl` (`action`:
    /// UPSERT or DELETE), and remember the change; Route 53's refusal is its code and message.
    async fn change(&self, zone: &str, action: &str, name: &str, ttl: u32, values: &[String]) -> anyhow::Result<Result<(), (String, String)>> {
        let records: String = values.iter().map(|v| format!("<ResourceRecord><Value>{}</Value></ResourceRecord>", escape(v))).collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><ChangeResourceRecordSetsRequest xmlns="https://route53.amazonaws.com/doc/{VERSION}/"><ChangeBatch><Comment>roll ACME challenge</Comment><Changes><Change><Action>{action}</Action><ResourceRecordSet><Name>{}.</Name><Type>TXT</Type><TTL>{ttl}</TTL><ResourceRecords>{records}</ResourceRecords></ResourceRecordSet></Change></Changes></ChangeBatch></ChangeResourceRecordSetsRequest>"#,
            escape(name)
        );
        let what = format!("{} TXT {name}", action.to_ascii_lowercase());
        let xml = match self.call_retried("POST", &format!("/{VERSION}/hostedzone/{zone}/rrset"), Some(&body), &what).await? {
            Ok(xml) => xml,
            Err(refused) => return Ok(Err(refused)),
        };
        let change = first(&xml, "Id").unwrap_or_default();
        let change = change.trim_start_matches("/change/");
        anyhow::ensure!(!change.is_empty() && change.chars().all(|c| c.is_ascii_alphanumeric()), "Route 53: {what}: an odd change id");
        if first(&xml, "Status").as_deref() != Some("INSYNC") {
            self.pending.lock().unwrap().push(change.to_string());
        }
        Ok(Ok(()))
    }
}

impl DnsProvider for Route53 {
    fn create_txt<'a>(&'a self, name: &'a str, value: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let zone = self.zone_for(name).await?;
            self.edit(&zone, name, value, true).await?;
            self.records.lock().unwrap().entry(name.to_string()).or_insert_with(|| (zone, vec![])).1.push(value.to_string());
            Ok(value.to_string())
        })
    }

    fn delete_txt<'a>(&'a self, name: &'a str, id: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let zone = {
                let records = self.records.lock().unwrap();
                let (zone, ours) = records.get(name).context("Route 53: a record this client didn't write")?;
                anyhow::ensure!(ours.iter().any(|v| v == id), "Route 53: a record this client didn't write");
                zone.clone()
            };
            self.edit(&zone, name, id, false).await?;
            if let Some((_, ours)) = self.records.lock().unwrap().get_mut(name) {
                ours.retain(|v| v != id);
            }
            Ok(())
        })
    }

    fn settle(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            let started = Instant::now();
            loop {
                let pending = std::mem::take(&mut *self.pending.lock().unwrap());
                let mut left = vec![];
                for change in pending {
                    let xml = self.call("GET", &format!("/{VERSION}/change/{change}"), None, &format!("change {change}")).await?;
                    if first(&xml, "Status").as_deref() != Some("INSYNC") {
                        left.push(change);
                    }
                }
                if left.is_empty() {
                    return Ok(());
                }
                if started.elapsed() + self.poll > self.within {
                    anyhow::bail!("Route 53: change {} wasn't INSYNC within {} s", left[0], self.within.as_secs());
                }
                self.pending.lock().unwrap().extend(left);
                tokio::time::sleep(self.poll).await;
            }
        })
    }
}

/// The text of the first `<tag>` in `xml` (entities decoded).
fn first(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))?;
    Some(unescape(&xml[start..start + end]))
}

/// Each `<tag>…</tag>` element's inside, in order.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use std::sync::Arc;

    /// Route 53 as much as DNS-01 uses: zones by name (a private twin and a neighbour first), record
    /// sets listed from a name on, UPSERTed whole and DELETEd only as they stand (TTL and values),
    /// each change PENDING once before it's INSYNC.
    #[derive(Clone, Default)]
    struct Fake {
        /// Name (with its dot) → TTL and values (quoted).
        sets: Arc<Mutex<std::collections::BTreeMap<String, (u32, Vec<String>)>>>,
        log: Arc<Mutex<Vec<String>>>,
        polled: Arc<Mutex<HashMap<String, u32>>>,
    }

    fn signed(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|a| a.starts_with("AWS4-HMAC-SHA256 Credential=AKIDROLLTEST/") && a.contains("/us-east-1/route53/aws4_request")) && h.get("x-amz-security-token").is_some_and(|t| t == "session-token")
    }

    async fn zones(State(f): State<Fake>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> (StatusCode, String) {
        if !signed(&h) {
            return (StatusCode::FORBIDDEN, "<ErrorResponse><Error><Code>InvalidSignatureException</Code><Message>no</Message></Error></ErrorResponse>".into());
        }
        f.log.lock().unwrap().push(format!("zones {}", q["dnsname"]));
        let zone = |id: &str, name: &str, private: bool| format!("<HostedZone><Id>/hostedzone/{id}</Id><Name>{name}</Name><Config><PrivateZone>{private}</PrivateZone></Config></HostedZone>");
        let body = match q["dnsname"].as_str() {
            "example.com" => format!("{}{}", zone("ZPRIVATE", "example.com.", true), zone("ZPUBLIC", "example.com.", false)),
            // A neighbour in name order, not the zone asked for.
            other => zone("ZNEXT", &format!("zz{other}."), false),
        };
        (StatusCode::OK, format!("<ListHostedZonesByNameResponse><HostedZones>{body}</HostedZones></ListHostedZonesByNameResponse>"))
    }

    /// `ListResourceRecordSets`: the sets from `name` on (here: TXT only), `maxitems` of them.
    async fn list(State(f): State<Fake>, h: HeaderMap, Path(zone): Path<String>, Query(q): Query<HashMap<String, String>>) -> String {
        assert!(signed(&h));
        assert_eq!((zone.as_str(), q["type"].as_str(), q["maxitems"].as_str()), ("ZPUBLIC", "TXT", "1"));
        let sets = f.sets.lock().unwrap();
        let body: String = sets
            .range(q["name"].clone()..)
            .take(1)
            .map(|(name, (ttl, values))| {
                let records: String = values.iter().map(|v| format!("<ResourceRecord><Value>{}</Value></ResourceRecord>", escape(v))).collect();
                format!("<ResourceRecordSet><Name>{name}</Name><Type>TXT</Type><TTL>{ttl}</TTL><ResourceRecords>{records}</ResourceRecords></ResourceRecordSet>")
            })
            .collect();
        format!("<ListResourceRecordSetsResponse><ResourceRecordSets>{body}</ResourceRecordSets><IsTruncated>false</IsTruncated></ListResourceRecordSetsResponse>")
    }

    async fn rrset(State(f): State<Fake>, h: HeaderMap, Path(zone): Path<String>, body: String) -> (StatusCode, String) {
        assert!(signed(&h));
        assert_eq!(zone, "ZPUBLIC");
        let (action, name) = (first(&body, "Action").unwrap(), first(&body, "Name").unwrap());
        let ttl: u32 = first(&body, "TTL").unwrap().parse().unwrap();
        let values: Vec<String> = blocks(&body, "Value").iter().map(|v| unescape(v)).collect();
        let mut sets = f.sets.lock().unwrap();
        if action == "DELETE" && sets.get(&name) != Some(&(ttl, values.clone())) {
            return (StatusCode::BAD_REQUEST, "<ErrorResponse><Error><Code>InvalidChangeBatch</Code><Message>it doesn't match</Message></Error></ErrorResponse>".into());
        }
        match action.as_str() {
            "DELETE" => sets.remove(&name),
            _ => sets.insert(name.clone(), (ttl, values.clone())),
        };
        let n = f.log.lock().unwrap().len();
        f.log.lock().unwrap().push(format!("{action} {name} {}", values.join(" ")));
        (StatusCode::OK, format!("<ChangeResourceRecordSetsResponse><ChangeInfo><Id>/change/C{n}</Id><Status>PENDING</Status></ChangeInfo></ChangeResourceRecordSetsResponse>"))
    }

    async fn change(State(f): State<Fake>, Path(id): Path<String>) -> String {
        let mut polled = f.polled.lock().unwrap();
        let n = polled.entry(id.clone()).or_default();
        *n += 1;
        let status = if *n > 1 { "INSYNC" } else { "PENDING" };
        format!("<GetChangeResponse><ChangeInfo><Id>/change/{id}</Id><Status>{status}</Status></ChangeInfo></GetChangeResponse>")
    }

    #[tokio::test]
    async fn a_wildcard_and_its_apex_share_one_record_set_and_other_clients_values_stay() {
        let fake = Fake::default();
        let app = axum::Router::new()
            .route("/2013-04-01/hostedzonesbyname", get(zones))
            .route("/2013-04-01/hostedzone/{zone}/rrset", post(rrset).get(list))
            .route("/2013-04-01/change/{id}", get(change))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let keys = AwsKeys { access_key_id: "AKIDROLLTEST".into(), secret_access_key: "secret".into(), session_token: Some("session-token".into()) };
        let r53 = Route53::new(keys.clone(), &api, http.clone()).unwrap().with_poll(Duration::from_millis(10), Duration::from_secs(5));

        assert_eq!(r53.zone_for("_acme-challenge.www.example.com").await.unwrap(), "ZPUBLIC", "the public zone, past a neighbour and a private twin");
        // A name only this order uses: both values in one set, then the set deleted as it stood.
        let name = "_acme-challenge.example.com";
        let apex = r53.create_txt(name, "apex-value").await.unwrap();
        let wild = r53.create_txt(name, "wild-value").await.unwrap();
        r53.settle().await.unwrap();
        assert_eq!(fake.sets.lock().unwrap()[&format!("{name}.")], (60, vec!["\"apex-value\"".to_string(), "\"wild-value\"".to_string()]), "both values in one set");
        assert!(fake.polled.lock().unwrap().values().all(|n| *n == 2), "each change waited on until INSYNC");
        r53.delete_txt(name, &apex).await.unwrap();
        assert_eq!(fake.sets.lock().unwrap()[&format!("{name}.")].1, ["\"wild-value\""]);
        r53.delete_txt(name, &wild).await.unwrap();
        assert!(fake.sets.lock().unwrap().is_empty(), "the set deleted as it stood: {:?}", fake.log.lock().unwrap());

        // Another ACME client's value already there, and a second order on the same name: each
        // order adds and removes only its own value, and the foreign one (and its TTL) stays.
        let shared = "_acme-challenge.shop.example.com";
        let foreign = "\"cert-manager-value\"".to_string();
        fake.sets.lock().unwrap().insert(format!("{shared}."), (300, vec![foreign.clone()]));
        let other = Route53::new(keys.clone(), &api, http.clone()).unwrap().with_poll(Duration::from_millis(10), Duration::from_secs(5));
        let mine = r53.create_txt(shared, "order-one").await.unwrap();
        let theirs = other.create_txt(shared, "order-two").await.unwrap();
        assert_eq!(fake.sets.lock().unwrap()[&format!("{shared}.")], (300, vec![foreign.clone(), "\"order-one\"".into(), "\"order-two\"".into()]), "the union, not this order's values alone");
        r53.delete_txt(shared, &mine).await.unwrap();
        assert!(other.delete_txt(shared, &mine).await.is_err(), "a value another client wrote isn't this one's to remove");
        assert_eq!(fake.sets.lock().unwrap()[&format!("{shared}.")], (300, vec![foreign.clone(), "\"order-two\"".into()]));
        other.delete_txt(shared, &theirs).await.unwrap();
        assert_eq!(fake.sets.lock().unwrap()[&format!("{shared}.")], (300, vec![foreign]), "the foreign value kept: {:?}", fake.log.lock().unwrap());

        let bad = Route53::new(AwsKeys { session_token: None, ..keys }, &api, http).unwrap();
        let e = bad.zone_for("_acme-challenge.example.com").await.unwrap_err().to_string();
        assert!(e.contains("InvalidSignatureException") && !e.contains("secret"), "{e}");
    }
}
