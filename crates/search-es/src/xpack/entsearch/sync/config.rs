//! The configuration of the connectors roll runs: each service's field schema (filled in as
//! Elastic's connector service fills it), reading values, and the sensitive ones sealed at rest
//! and never shown.

use crate::cluster::Cluster;
use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::obj;

/// What a sensitive value is sealed as ([`crate::seal`]).
pub const SECRET_KIND: &str = "connector configuration";
/// What a sensitive value shows as. Writing it back keeps the stored value.
pub const REDACTED: &str = "********";
/// The services roll runs.
pub const SERVICES: [&str; 3] = ["postgresql", "s3", super::crawler::SERVICE];

/// Whether roll runs this connector's syncs: an Elastic-managed connector of a service roll
/// implements. Self-managed connectors are left to the tenant's connector service.
pub fn runs(c: &Value) -> bool {
    c.get("is_native").and_then(Value::as_bool) == Some(true)
        && c.get("deleted") != Some(&Value::Bool(true))
        && c.get("service_type").and_then(Value::as_str).is_some_and(|s| SERVICES.contains(&s))
}

#[derive(Clone, Copy)]
enum Ty {
    Str,
    Int,
    Bool,
    List,
}

struct FieldDef {
    key: &'static str,
    label: &'static str,
    ty: Ty,
    value: fn() -> Value,
    required: bool,
    sensitive: bool,
    tooltip: Option<&'static str>,
    depends_on: Option<(&'static str, bool)>,
}

const fn f(key: &'static str, label: &'static str, ty: Ty, value: fn() -> Value, required: bool, sensitive: bool) -> FieldDef {
    FieldDef { key, label, ty, value, required, sensitive, tooltip: None, depends_on: None }
}

fn empty() -> Value {
    Value::from("")
}

const POSTGRES: &[FieldDef] = &[
    f("host", "Host", Ty::Str, empty, true, false),
    f("port", "Port", Ty::Int, || Value::from(5432), true, false),
    f("username", "Username", Ty::Str, empty, true, false),
    f("password", "Password", Ty::Str, empty, true, true),
    f("database", "Database", Ty::Str, empty, true, false),
    f("schema", "Schema", Ty::Str, || Value::from("public"), true, false),
    FieldDef { tooltip: Some("A comma-separated list of tables, or * for every table of the schema."), ..f("tables", "Comma-separated list of tables", Ty::List, || Value::from("*"), true, false) },
    f("fetch_size", "Rows fetched per request", Ty::Int, || Value::from(50), false, false),
    f("retry_count", "Retries per request", Ty::Int, || Value::from(3), false, false),
    f("ssl_enabled", "Enable SSL verification", Ty::Bool, || Value::Bool(true), true, false),
    FieldDef { depends_on: Some(("ssl_enabled", true)), ..f("ssl_ca", "SSL certificate", Ty::Str, empty, false, false) },
    FieldDef {
        tooltip: Some("A timestamp or ever-increasing column: incremental syncs read only rows where it is at or after the last sync's value. Empty: incremental syncs read every row."),
        ..f("incremental_column", "Incremental sync column", Ty::Str, empty, false, false)
    },
];

const S3: &[FieldDef] = &[
    FieldDef { tooltip: Some("A comma-separated list of buckets."), ..f("buckets", "AWS Buckets", Ty::List, empty, true, false) },
    f("aws_access_key_id", "AWS Access Key Id", Ty::Str, empty, false, false),
    f("aws_secret_access_key", "AWS Secret Key", Ty::Str, empty, false, true),
    FieldDef { tooltip: Some("An S3-compatible endpoint (scheme, host and port). Empty: AWS."), ..f("endpoint", "Endpoint", Ty::Str, empty, false, false) },
    f("region", "Region", Ty::Str, || Value::from("us-east-1"), false, false),
    f("read_timeout", "Read timeout", Ty::Int, || Value::from(90), false, false),
    f("connect_timeout", "Connection timeout", Ty::Int, || Value::from(90), false, false),
    f("max_attempts", "Maximum retry attempts", Ty::Int, || Value::from(5), false, false),
    f("page_size", "Maximum size of page", Ty::Int, || Value::from(100), false, false),
];

/// `roll_web_crawler` (docs/search/crawler.md, "Configuration"); the rest of Open Crawler's config
/// goes in `filtering.advanced_snippet`.
const CRAWLER: &[FieldDef] = &[
    FieldDef { tooltip: Some("Seed URLs, comma-separated. Each URL's origin becomes a domain of the crawl."), ..f("urls", "URLs", Ty::List, empty, true, false) },
    FieldDef { tooltip: Some("More sitemaps, comma-separated, on the seed domains."), ..f("sitemap_urls", "Sitemap URLs", Ty::List, empty, false, false) },
    f("sitemap_discovery", "Read the sitemaps robots.txt lists", Ty::Bool, || Value::Bool(true), false, false),
    f("max_crawl_depth", "Maximum link depth", Ty::Int, || Value::from(10), false, false),
    f("max_unique_url_count", "Maximum URLs per crawl", Ty::Int, || Value::from(10_000), false, false),
    FieldDef { tooltip: Some("The one origin (https://host[:port]) the credential is sent to, never another, never over plain http."), ..f("auth_origin", "Credential origin", Ty::Str, empty, false, false) },
    FieldDef { tooltip: Some("A whole Authorization header value, such as Bearer ..."), ..f("auth_header", "Authorization header", Ty::Str, empty, false, true) },
    // Sensitive too: some sites take a token as the username with no password.
    f("auth_username", "Basic auth username", Ty::Str, empty, false, true),
    f("auth_password", "Basic auth password", Ty::Str, empty, false, true),
];

fn schema_of(service: &str) -> &'static [FieldDef] {
    match service {
        "postgresql" => POSTGRES,
        super::crawler::SERVICE => CRAWLER,
        _ => S3,
    }
}

/// A service's configuration, as Elastic's connector service registers it.
fn schema(service: &str) -> Value {
    let mut out = Map::new();
    for (i, d) in schema_of(service).iter().enumerate() {
        let (ty, display) = match d.ty {
            Ty::Str if d.key == "ssl_ca" => ("str", "textarea"),
            Ty::Str => ("str", "text"),
            Ty::Int => ("int", "numeric"),
            Ty::Bool => ("bool", "toggle"),
            Ty::List => ("list", "textarea"),
        };
        let depends: Vec<Value> = d.depends_on.iter().map(|(k, v)| obj! { "field" => *k, "value" => *v }).collect();
        let value = (d.value)();
        out.insert(
            d.key.into(),
            obj! {
                "default_value" => if value.as_str() == Some("") { Value::Null } else { value.clone() },
                "depends_on" => Value::Arr(depends),
                "display" => display,
                "label" => d.label,
                "options" => Value::Arr(vec![]),
                "order" => i + 1,
                "required" => d.required,
                "sensitive" => d.sensitive,
                "tooltip" => d.tooltip.map_or(Value::Null, Value::from),
                "type" => ty,
                "ui_restrictions" => Value::Arr(vec![]),
                "validations" => Value::Arr(vec![]),
                "value" => value,
            },
        );
    }
    Value::Obj(out)
}

/// Gives a connector roll now runs its service's configuration schema when it has none (and
/// `needs_configuration` when it was only `created`), as Elastic's connector service does.
pub fn fill_schema(m: &mut Map) {
    if !runs(&Value::Obj(m.clone())) {
        return;
    }
    if m.get("configuration").and_then(Value::as_object).is_some_and(|c| !c.is_empty()) {
        return;
    }
    let service = m.get("service_type").and_then(Value::as_str).unwrap_or_default().to_string();
    m.insert("configuration".into(), schema(&service));
    if m.get("status").and_then(Value::as_str) == Some("created") {
        m.insert("status".into(), "needs_configuration".into());
    }
}

fn sensitive(entry: &Value) -> bool {
    entry.get("sensitive").and_then(Value::as_bool) == Some(true)
}

/// Whether roll's schema for `service` marks field `k` sensitive.
fn schema_secret(service: &str, k: &str) -> bool {
    schema_of(service).iter().any(|d| d.key == k && d.sensitive)
}

/// The service of a connector roll runs (its schema decides what is sensitive), else `None`.
fn run_service(c: &Value) -> Option<String> {
    runs(c).then(|| c.get("service_type").and_then(Value::as_str).unwrap_or_default().to_string())
}

/// Whether field `k` (`entry`) of a connector is sensitive: for one roll runs (`service`), what
/// roll's schema says counts whatever flag a client stored; otherwise the stored flag.
fn secret_field(service: Option<&str>, k: &str, entry: &Value) -> bool {
    sensitive(entry) || service.is_some_and(|s| schema_secret(s, k))
}

/// Puts back the `sensitive` flag of every field roll's schema for `service` marks sensitive:
/// whoever writes a configuration can't decide a password isn't one.
fn force_flags(m: &mut Map, service: &str) {
    let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { return };
    for (k, entry) in cfg.iter_mut() {
        if schema_secret(service, k)
            && let Value::Obj(e) = entry
        {
            e.insert("sensitive".into(), true.into());
        }
    }
}

/// A sensitive field's value as text (a number or anything else written there counts too):
/// `None` when empty.
fn secret_text(entry: &Value) -> Option<String> {
    value_text(entry.get("value")?)
}

/// [`secret_text`] of a value itself.
fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Str(s) if s.is_empty() => None,
        Value::Str(s) => Some(s.clone()),
        other => Some(other.to_es_string()),
    }
}

/// What a connector's sensitive field is sealed as: bound to the cluster (its catalog id), the
/// connector and the field, so a sealed value moved into another connector's or field's slot, or
/// carried to another cluster's state (a branch's copy of its parent), doesn't open. A branch
/// adopting an inherited connector gives its credentials again ([`protect`]).
fn kind_of(cluster: &str, connector: &str, field: &str) -> String {
    format!("{SECRET_KIND}:{cluster}:{connector}:{field}")
}

/// Whether connector `c` (one roll runs) holds a sensitive value in the clear: written before
/// roll ran syncs, when they were stored as given, or by an earlier build that let a writer mark
/// a password not sensitive.
pub fn holds_clear_secret(c: &Value) -> bool {
    let Some(service) = run_service(c) else { return false };
    c.get("configuration").and_then(Value::as_object).is_some_and(|cfg| {
        cfg.iter().any(|(k, e)| secret_field(Some(&service), k, e) && secret_text(e).is_some_and(|v| v != REDACTED && !crate::seal::is_sealed(&v)))
    })
}

/// Seals in place the sensitive values a connector holds in the clear ([`holds_clear_secret`]),
/// a number too, through the one sealing path ([`crate::seal::seal_given_with`]); where that
/// refuses (no key, no opt-in), clears them (the connector then needs configuration).
pub fn seal_in_place(m: &mut Map, cluster: &str, sealer: Option<&crate::seal::Sealer>, unsealed: bool) {
    let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let service = run_service(&Value::Obj(m.clone()));
    let mut cleared = false;
    let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { return };
    for (k, entry) in cfg.iter_mut() {
        if !secret_field(service.as_deref(), k, entry) {
            continue;
        }
        let Some(v) = secret_text(entry).filter(|v| v != REDACTED && !crate::seal::is_sealed(v)) else { continue };
        let next = match crate::seal::seal_given_with(sealer, unsealed, &kind_of(cluster, &id, k), &v) {
            Ok(s) => Value::from(s),
            Err(_) => {
                cleared = true;
                Value::from("")
            }
        };
        if let Value::Obj(e) = entry {
            e.insert("value".into(), next);
        }
    }
    if cleared {
        m.insert("status".into(), "needs_configuration".into());
    }
}

/// The fields a credential is sent to, per service: changing one without giving the credential
/// again clears it, so a stored password is never sent to a host its owner didn't pair it with.
fn target_fields(service: &str) -> &'static [&'static str] {
    match service {
        "postgresql" => &["host", "port", "username", "database", "ssl_enabled", "ssl_ca"],
        super::crawler::SERVICE => &["auth_origin"],
        _ => &["endpoint", "region", "aws_access_key_id", "buckets"],
    }
}

fn value_of<'a>(m: &'a Map, field: &str) -> Option<&'a Value> {
    m.get("configuration")?.get(field)?.get("value")
}

/// Keeps a connector roll runs safe after any update (`before` is the stored connector, empty for
/// a new one):
/// - When roll stops running it (made self-managed, given a service roll doesn't run, deleted),
///   the values it sealed are cleared: nothing sealed survives a stretch where its
///   configuration (a host included) changes unguarded.
/// - When roll starts running it (created native, or made native, or given a service roll runs),
///   every non-empty sensitive value is sealed as given, and one that is a sealed box is cleared
///   instead: roll cleared its own when it stopped, so a sealed box then was written by a client
///   (copied from this or another connector) and is never opened.
/// - Otherwise a written sensitive value is sealed; [`REDACTED`] or the stored value written back
///   keeps the stored one, and only when the stored entry was sensitive too (a value stored in
///   the clear under a non-sensitive entry is never taken for the sealed one).
/// - A change to where the credential goes ([`target_fields`]), filling in an empty one too,
///   without giving it again clears it and puts the connector back to `needs_configuration`, as
///   Elastic's UI asks for the password again.
/// - A change of service starts from the new service's schema (`needs_configuration`): nothing
///   of the old service's configuration, its credentials included, carries over, so fields one
///   service doesn't guard can't be set aside for the other.
/// - A write that makes a waiting connector the cluster's own (one a branch inherited, or one
///   from before an upgrade: [`super::waits_for_adoption`]; `adopting` names the fields the write
///   gave) is treated as roll starting to run it, and keeps only the credentials it gave: every
///   other sensitive value is cleared, sealed or in the clear, so its syncs only ever use
///   credentials this cluster's owner entered.
///
/// - A node without a sealing key (`ROLL_SECRET_KEY`) stores no sensitive value of a connector
///   roll runs: a write that would is refused (`Err` names the field), never kept in the clear,
///   unless the node opted in for local development (`unsealed`). The decision is
///   [`crate::seal::seal_given_with`]'s, as for every secret search keeps.
///
/// `cluster` is the cluster's catalog id, which seals are bound to ([`kind_of`]).
pub fn protect(m: &mut Map, before: &Map, sealer: Option<&crate::seal::Sealer>, unsealed: bool, cluster: &str, adopting: Option<&[String]>) -> Result<(), String> {
    let ran = runs(&Value::Obj(before.clone()));
    if !runs(&Value::Obj(m.clone())) {
        if ran {
            clear_secrets(m);
        }
        return Ok(());
    }
    let service = m.get("service_type").and_then(Value::as_str).unwrap_or_default().to_string();
    if before.get("service_type") != m.get("service_type") {
        m.insert("configuration".into(), schema(&service));
        m.insert("status".into(), "needs_configuration".into());
        return Ok(());
    }
    // What is sensitive is roll's schema's call, never the writer's (a value under a schema
    // field marked otherwise is guarded all the same, and the flag put back).
    force_flags(m, &service);
    let started = !ran || adopting.is_some();
    let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let had_secret = before.get("configuration").and_then(Value::as_object).is_some_and(|c| c.iter().any(|(k, e)| secret_field(Some(&service), k, e) && secret_text(e).is_some()));
    let retargeted = !started && had_secret && target_fields(&service).iter().any(|f| value_of(m, f) != value_of(before, f));
    let old = before.get("configuration").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut cleared = false;
    let seal = |k: &str, v: &str| crate::seal::seal_given_with(sealer, unsealed, &kind_of(cluster, &id, k), v).map(Value::from).map_err(|_| k.to_string());
    let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { return Ok(()) };
    for (k, entry) in cfg.iter_mut() {
        if !secret_field(Some(&service), k, entry) {
            continue;
        }
        let Some(v) = secret_text(entry) else { continue };
        let Value::Obj(e) = entry else { continue };
        if started {
            let given = adopting.is_none_or(|g| g.iter().any(|f| f == k));
            let next = if !given || v == REDACTED || crate::seal::is_sealed(&v) {
                cleared = true;
                Value::from("")
            } else {
                seal(k, &v)?
            };
            e.insert("value".into(), next);
            continue;
        }
        let stored = old.get(k).filter(|o| secret_field(Some(&service), k, o)).and_then(|o| o.get("value")).cloned();
        let unchanged = v == REDACTED || stored.as_ref().and_then(Value::as_str) == Some(v.as_str());
        let next = if unchanged && retargeted {
            cleared = true;
            Value::from("")
        } else if unchanged {
            match stored.as_ref().and_then(value_text) {
                // Stored in the clear (an earlier build, or before a key): sealed now, or with no
                // key to seal it, cleared.
                Some(s) if s != REDACTED && !crate::seal::is_sealed(&s) => match sealer {
                    Some(_) => seal(k, &s)?,
                    None => {
                        cleared = true;
                        Value::from("")
                    }
                },
                // The placeholder with nothing stored keeps nothing.
                _ => stored.unwrap_or_else(|| Value::from("")),
            }
        } else {
            seal(k, &v)?
        };
        e.insert("value".into(), next);
    }
    if cleared {
        m.insert("status".into(), "needs_configuration".into());
    }
    Ok(())
}

/// Empties every sensitive value, and any sealed one, of a connector roll stops running.
fn clear_secrets(m: &mut Map) {
    let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { return };
    for entry in cfg.values_mut() {
        let secret = sensitive(entry);
        if let Value::Obj(e) = entry
            && let Some(Value::Str(v)) = e.get("value")
            && !v.is_empty()
            && (secret || crate::seal::is_sealed(v))
        {
            e.insert("value".into(), Value::from(""));
        }
    }
}

/// A connector (or a job's snapshot of one) as it is shown: the sensitive values of one roll
/// runs replaced by [`REDACTED`], and a sealed value of any connector too (one roll ran before it
/// became self-managed: its ciphertext means nothing to a client).
pub fn redact(c: &mut Value) {
    let all = runs(c);
    redact_where(c, |v| all || crate::seal::is_sealed(v));
}

/// Replaces every non-empty sensitive value of `c`'s configuration.
pub fn redact_configuration(c: &mut Value) {
    redact_where(c, |_| true);
}

fn redact_where(c: &mut Value, hide: impl Fn(&str) -> bool) {
    let service = run_service(c);
    let Some(Value::Obj(cfg)) = (match c {
        Value::Obj(m) => m.get_mut("configuration"),
        _ => None,
    }) else {
        return;
    };
    for (k, entry) in cfg.iter_mut() {
        // For a connector roll runs, roll's schema decides what is sensitive too.
        let secret = secret_field(service.as_deref(), k, entry);
        let Some(v) = secret_text(entry) else { continue };
        if (crate::seal::is_sealed(&v) || (secret && hide(&v)))
            && let Value::Obj(e) = entry
        {
            e.insert("value".into(), REDACTED.into());
        }
    }
}

/// A list value of a connector's configuration as stored (an array, or a comma-separated
/// string): for fields that aren't sensitive.
pub fn listed(c: &Value, key: &str) -> Vec<String> {
    list_of(c.get("configuration").and_then(|cfg| cfg.get(key)).and_then(|e| e.get("value")))
}

fn list_of(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Value::Str(s)) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// A connector's configuration values, the sensitive ones opened.
pub struct Values {
    values: Map,
    /// Every opened secret, to scrub from messages.
    secrets: Vec<String>,
}

impl Values {
    pub fn open(cluster: &Cluster, connector: &Value) -> EsResult<Values> {
        let id = connector.get("id").and_then(Value::as_str).unwrap_or_default();
        let me = crate::branch::cluster_id(cluster);
        let service = run_service(connector);
        let mut values = Map::new();
        let mut secrets = Vec::new();
        for (k, e) in connector.get("configuration").and_then(Value::as_object).into_iter().flatten() {
            let v = e.get("value").cloned().unwrap_or(Value::Null);
            let v = match secret_field(service.as_deref(), k, e).then(|| secret_text(e)).flatten() {
                Some(s) => {
                    // roll stores a sensitive value only sealed: one in the clear (written past
                    // the guard, or before it) is never used.
                    if service.is_some() && !crate::seal::is_sealed(&s) {
                        return Err(crate::error::EsError::illegal_state(format!("the connector's [{k}] isn't sealed: give it again")));
                    }
                    let plain = crate::seal::open(cluster, &kind_of(&me, id, k), &s)?;
                    secrets.push(plain.clone());
                    Value::from(plain)
                }
                None => v,
            };
            values.insert(k.clone(), v);
        }
        Ok(Values { values, secrets })
    }

    /// A text value; empty and missing are `None`.
    pub fn text(&self, key: &str) -> Option<String> {
        match self.values.get(key)? {
            Value::Null => None,
            Value::Str(s) if s.trim().is_empty() => None,
            Value::Str(s) => Some(s.trim().to_string()),
            other => Some(other.to_es_string()),
        }
    }

    /// A required text value.
    pub fn need(&self, key: &str) -> Result<String, String> {
        self.text(key).ok_or_else(|| format!("the connector's configuration has no [{key}]"))
    }

    pub fn int(&self, key: &str, default: i64) -> Result<i64, String> {
        match self.text(key) {
            None => Ok(default),
            Some(s) => s.parse::<i64>().map_err(|_| format!("the connector's [{key}] isn't a number")),
        }
    }

    pub fn bool(&self, key: &str, default: bool) -> bool {
        match self.values.get(key) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Str(s)) => s.eq_ignore_ascii_case("true"),
            _ => default,
        }
    }

    /// A list value: an array, or a comma-separated string.
    pub fn list(&self, key: &str) -> Vec<String> {
        list_of(self.values.get(key))
    }

    /// `message` with every secret this configuration holds blanked out.
    pub fn scrub(&self, message: &str) -> String {
        let mut out = message.to_string();
        for s in self.secrets.iter().filter(|s| s.len() >= 3) {
            out = out.replace(s.as_str(), "[redacted]");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(service: &str, values: &[(&str, &str)]) -> Map {
        let mut cfg = schema(service);
        if let Value::Obj(c) = &mut cfg {
            for (k, v) in values {
                if let Some(Value::Obj(e)) = c.get_mut(*k) {
                    e.insert("value".into(), Value::from(*v));
                }
            }
        }
        let c = obj! { "id" => "c", "is_native" => true, "service_type" => service, "status" => "connected", "configuration" => cfg };
        c.as_object().unwrap().clone()
    }

    fn secret(m: &Map, field: &str) -> String {
        value_of(m, field).map(Value::to_es_string).unwrap_or_default()
    }

    /// Filling in an empty target (an S3 endpoint, a CA) after the secret was stored is a
    /// retarget too: the stored secret is cleared.
    #[test]
    fn filling_a_target_in_clears_a_stored_secret() {
        let before = connector("s3", &[("buckets", "docs"), ("aws_secret_access_key", "sealed-secret")]);
        let mut after = before.clone();
        if let Some(Value::Obj(c)) = after.get_mut("configuration")
            && let Some(Value::Obj(e)) = c.get_mut("endpoint")
        {
            e.insert("value".into(), "https://elsewhere.example".into());
        }
        protect(&mut after, &before, None, false, "c", None).unwrap();
        assert_eq!(secret(&after, "aws_secret_access_key"), "");
        assert_eq!(after.get("status").and_then(Value::as_str), Some("needs_configuration"));
        // The same write giving the secret again keeps it (sealed).
        let mut again = after.clone();
        if let Some(Value::Obj(c)) = again.get_mut("configuration")
            && let Some(Value::Obj(e)) = c.get_mut("aws_secret_access_key")
        {
            e.insert("value".into(), "new-secret".into());
        }
        if let Some(Value::Obj(c)) = again.get_mut("configuration")
            && let Some(Value::Obj(e)) = c.get_mut("endpoint")
        {
            e.insert("value".into(), "https://other.example".into());
        }
        let sealer = crate::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-protect").unwrap();
        protect(&mut again, &after, Some(sealer.as_ref()), false, "c", None).unwrap();
        assert!(crate::seal::is_sealed(&secret(&again, "aws_secret_access_key")), "kept, sealed");

        // Adopting keeps only what the write gave, a value stored in the clear included.
        let mut clear = again.clone();
        if let Some(Value::Obj(c)) = clear.get_mut("configuration")
            && let Some(Value::Obj(e)) = c.get_mut("aws_secret_access_key")
        {
            e.insert("value".into(), "stored-in-the-clear".into());
        }
        let mut adopted = clear.clone();
        protect(&mut adopted, &clear, Some(sealer.as_ref()), false, "c", Some(&[])).unwrap();
        assert_eq!(secret(&adopted, "aws_secret_access_key"), "");
        let mut adopted = clear.clone();
        protect(&mut adopted, &clear, Some(sealer.as_ref()), false, "c", Some(&["aws_secret_access_key".to_string()])).unwrap();
        assert!(crate::seal::is_sealed(&secret(&adopted, "aws_secret_access_key")));
    }
}
