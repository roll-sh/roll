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
pub const SERVICES: [&str; 2] = ["postgresql", "s3"];

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

fn schema_of(service: &str) -> &'static [FieldDef] {
    match service {
        "postgresql" => POSTGRES,
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

/// What a connector's sensitive field is sealed as: bound to the connector and the field, so a
/// sealed value moved into another connector's (or another field's) slot doesn't open. Not bound
/// to the cluster: a branch and a snapshot restore carry the connector to a cluster with another
/// uuid, and its syncs must keep working there.
fn kind_of(connector: &str, field: &str) -> String {
    format!("{SECRET_KIND}:{connector}:{field}")
}

/// The fields a credential is sent to, per service: changing one without giving the credential
/// again clears it, so a stored password is never sent to a host its owner didn't pair it with.
fn target_fields(service: &str) -> &'static [&'static str] {
    match service {
        "postgresql" => &["host", "port", "username", "database", "ssl_enabled", "ssl_ca"],
        _ => &["endpoint", "region", "aws_access_key_id", "buckets"],
    }
}

fn value_of<'a>(m: &'a Map, field: &str) -> Option<&'a Value> {
    m.get("configuration")?.get(field)?.get("value")
}

/// Keeps a connector roll runs safe after any update (`before` is the stored connector, empty for
/// a new one):
/// - When roll starts running it (created native, or made native, or given a service roll runs),
///   every non-empty sensitive value is sealed as given, even one that looks sealed: a value
///   sealed elsewhere (another cluster, another connector) is only ever text here.
/// - Otherwise a written sensitive value is sealed; [`REDACTED`] or the stored value written back
///   keeps the stored one.
/// - A change to where the credential goes ([`target_fields`]), filling in an empty one too,
///   without giving it again clears it and puts the connector back to `needs_configuration`, as
///   Elastic's UI asks for the password again.
/// - A change of service starts from the new service's schema (`needs_configuration`): nothing
///   of the old service's configuration, its credentials included, carries over, so fields one
///   service doesn't guard can't be set aside for the other.
pub fn protect(m: &mut Map, before: &Map, sealer: Option<&crate::seal::Sealer>) {
    if !runs(&Value::Obj(m.clone())) {
        return;
    }
    let service = m.get("service_type").and_then(Value::as_str).unwrap_or_default().to_string();
    if before.get("service_type") != m.get("service_type") {
        m.insert("configuration".into(), schema(&service));
        m.insert("status".into(), "needs_configuration".into());
        return;
    }
    let started = !runs(&Value::Obj(before.clone()));
    let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let had_secret = before.get("configuration").and_then(Value::as_object).is_some_and(|c| {
        c.values().any(|e| sensitive(e) && e.get("value").and_then(Value::as_str).is_some_and(|v| !v.is_empty()))
    });
    let retargeted = !started && had_secret && target_fields(&service).iter().any(|f| value_of(m, f) != value_of(before, f));
    let old = before.get("configuration").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut cleared = false;
    let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { return };
    for (k, entry) in cfg.iter_mut() {
        if !sensitive(entry) {
            continue;
        }
        let Value::Obj(e) = entry else { continue };
        let Some(Value::Str(v)) = e.get("value").cloned() else { continue };
        if v.is_empty() {
            continue;
        }
        let stored = (!started).then(|| old.get(k).and_then(|o| o.get("value")).cloned()).flatten();
        let unchanged = v == REDACTED || stored.as_ref().and_then(Value::as_str) == Some(v.as_str());
        let next = if unchanged && retargeted {
            cleared = true;
            Value::from("")
        } else if unchanged {
            // The placeholder with nothing stored keeps nothing.
            stored.unwrap_or_else(|| Value::from(""))
        } else {
            Value::from(crate::seal::seal_given_with(sealer, &kind_of(&id, k), &v))
        };
        e.insert("value".into(), next);
    }
    if cleared {
        m.insert("status".into(), "needs_configuration".into());
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
    let Some(Value::Obj(cfg)) = (match c {
        Value::Obj(m) => m.get_mut("configuration"),
        _ => None,
    }) else {
        return;
    };
    for entry in cfg.values_mut() {
        let Value::Obj(e) = entry else { continue };
        let secret = e.get("sensitive").and_then(Value::as_bool) == Some(true);
        if let Some(Value::Str(v)) = e.get("value")
            && !v.is_empty()
            && (crate::seal::is_sealed(v) || (secret && hide(v)))
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
        let mut values = Map::new();
        let mut secrets = Vec::new();
        for (k, e) in connector.get("configuration").and_then(Value::as_object).into_iter().flatten() {
            let v = e.get("value").cloned().unwrap_or(Value::Null);
            let v = match (&v, sensitive(e)) {
                (Value::Str(s), true) if !s.is_empty() => {
                    let plain = crate::seal::open(cluster, &kind_of(id, k), s)?;
                    secrets.push(plain.clone());
                    Value::from(plain)
                }
                _ => v,
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
        protect(&mut after, &before, None);
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
        protect(&mut again, &after, None);
        assert_eq!(secret(&again, "aws_secret_access_key"), "new-secret", "no sealer: kept as given");
    }
}
