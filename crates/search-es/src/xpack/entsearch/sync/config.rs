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

/// Seals the sensitive values a configuration write left in the clear (a connector roll runs):
/// [`REDACTED`] or the stored sealed value written back keeps what was stored.
pub fn seal_written(m: &mut Map, before: &Map, sealer: Option<&crate::seal::Sealer>) {
    if !runs(&Value::Obj(m.clone())) {
        return;
    }
    let old = before.get("configuration").and_then(Value::as_object).cloned().unwrap_or_default();
    let Some(Value::Obj(cfg)) = m.get_mut("configuration") else { return };
    for (k, entry) in cfg.iter_mut() {
        if !sensitive(entry) {
            continue;
        }
        let Value::Obj(e) = entry else { continue };
        let stored = old.get(k).and_then(|o| o.get("value")).cloned();
        let v = match e.get("value") {
            Some(Value::Str(s)) => s.clone(),
            _ => continue,
        };
        if v.is_empty() {
            continue;
        }
        let kept = (v == REDACTED || stored.as_ref().and_then(Value::as_str) == Some(v.as_str())).then_some(stored).flatten();
        let next = match kept {
            Some(s) => s,
            // Writing the placeholder with nothing stored keeps nothing.
            None if v == REDACTED => Value::from(""),
            None => Value::from(crate::seal::seal_given_with(sealer, SECRET_KIND, &v)),
        };
        e.insert("value".into(), next);
    }
}

/// A connector (or a job's snapshot of one) as it is shown: the sensitive values of one roll
/// runs replaced by [`REDACTED`].
pub fn redact(c: &mut Value) {
    if runs(c) {
        redact_configuration(c);
    }
}

/// Replaces every non-empty sensitive value of `c`'s configuration.
pub fn redact_configuration(c: &mut Value) {
    let Some(Value::Obj(cfg)) = (match c {
        Value::Obj(m) => m.get_mut("configuration"),
        _ => None,
    }) else {
        return;
    };
    for entry in cfg.values_mut() {
        if !sensitive(entry) {
            continue;
        }
        if let Value::Obj(e) = entry
            && e.get("value").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
        {
            e.insert("value".into(), REDACTED.into());
        }
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
        let mut values = Map::new();
        let mut secrets = Vec::new();
        for (k, e) in connector.get("configuration").and_then(Value::as_object).into_iter().flatten() {
            let v = e.get("value").cloned().unwrap_or(Value::Null);
            let v = match (&v, sensitive(e)) {
                (Value::Str(s), true) if !s.is_empty() => {
                    let plain = crate::seal::open(cluster, SECRET_KIND, s)?;
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
        match self.values.get(key) {
            Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            Some(Value::Str(s)) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
            _ => Vec::new(),
        }
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
