//! Tags (TagResource, UntagResource, ListTagsOfResource) and TTL (UpdateTimeToLive,
//! DescribeTimeToLive).
//!
//! Tags live in the table schema, as ScyllaDB keeps them in the `scylla_tags` extension.
//! `system:` keys are reserved except `system:write_isolation` (validated) and
//! `system:initial_tablets`; the TTL attribute is kept apart and never listed.

use super::*;
use crate::service::WriteIsolation;

/// At most this many tags per table.
const MAX_TAGS: usize = 50;

/// The allowed values as ScyllaDB lists them (the iteration order of its hash set).
const ISOLATION_VALUES: &str = r#"{"unsafe_rmw", "unsafe", "u", "o", "always_use_lwt", "always", "only_rmw_uses_lwt", "a", "forbid_rmw", "forbid", "f"}"#;

pub(crate) fn validate_write_isolation(v: &str) -> Result<()> {
    if WriteIsolation::parse(v).is_some() {
        return Ok(());
    }
    Err(Error::validation(format!(
        "Incorrect write isolation tag {v}. Allowed values: {ISOLATION_VALUES}"
    )))
}

fn tag_char_ok(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' | '+' | '-' | '=' | '.' | '_' | ':' | '/'
        )
}

fn check_reserved(key: &str) -> Result<()> {
    if key.starts_with("system:")
        && key != "system:write_isolation"
        && key != "system:initial_tablets"
    {
        return Err(Error::validation(format!(
            "Tag key '{key}' is reserved for internal use"
        )));
    }
    Ok(())
}

/// Validates a list of `{Key, Value}` tags.
fn parse_tags(v: &[Json]) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for t in v {
        if !t.is_object() {
            return Err(Error::validation("invalid tag object"));
        }
        let (Some(k), Some(val)) = (
            t.get("Key").and_then(Json::as_str),
            t.get("Value").and_then(Json::as_str),
        ) else {
            return Err(Error::validation("string Key and Value required"));
        };
        if k.is_empty() {
            return Err(Error::validation("A tag Key cannot be empty"));
        }
        if k.len() > 128 {
            return Err(Error::validation("A tag Key is limited to 128 characters"));
        }
        if !k.chars().all(tag_char_ok) {
            return Err(Error::validation(
                "A tag Key can only contain letters, spaces, and [+-=._:/]",
            ));
        }
        if val.len() > 256 {
            return Err(Error::validation(
                "A tag Value is limited to 256 characters",
            ));
        }
        if !val.chars().all(tag_char_ok) {
            return Err(Error::validation(
                "A tag Value can only contain letters, spaces, and [+-=._:/]",
            ));
        }
        check_reserved(k)?;
        if k == "system:write_isolation" {
            validate_write_isolation(val)?;
        }
        match out.iter_mut().find(|(x, _)| x == k) {
            Some(e) => e.1 = val.to_string(),
            None => out.push((k.to_string(), val.to_string())),
        }
    }
    Ok(out)
}

/// The `Tags` of CreateTable (a non-array is ignored).
pub(crate) fn parse_create_tags(v: Option<&Json>) -> Result<Vec<(String, String)>> {
    match v.and_then(Json::as_array) {
        Some(a) => parse_tags(a),
        None => Ok(Vec::new()),
    }
}

/// Resolves a table ARN as ScyllaDB parses it.
async fn table_of_arn(alt: &Alternator, body: &Json) -> Result<Arc<TableSchema>> {
    let Some(arn) = body.get("ResourceArn").and_then(Json::as_str) else {
        return Err(Error::new(
            ErrorKind::AccessDenied,
            "Incorrect resource identifier",
        ));
    };
    let bad = |kind: ErrorKind, why: &str| {
        Error::new(
            kind,
            format!("ResourceArn: Invalid table ARN `{arn}` {why}"),
        )
    };
    let Some(rest) = arn.strip_prefix("arn:") else {
        return Err(bad(ErrorKind::AccessDenied, " - missing `arn:` prefix"));
    };
    let parts: Vec<&str> = rest.splitn(5, ':').collect();
    if parts.len() < 5 {
        return Err(bad(
            ErrorKind::AccessDenied,
            "- missing arn:<partition>:<service>:<region>:<account-id> prefix",
        ));
    }
    let not_found = || bad(ErrorKind::ResourceNotFound, "- not found");
    let resource = parts[4];
    let Some(res) = resource.strip_prefix("table/") else {
        return Err(not_found());
    };
    let (keyspace, rest) = if parts[0] == "scylla" {
        (parts[2].to_string(), res)
    } else {
        let Some((ks, t)) = res.split_once('@') else {
            return Err(bad(ErrorKind::Validation, "- missing separator `@`"));
        };
        (ks.to_string(), t)
    };
    let (name, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if !tail.is_empty() {
        return Err(bad(
            ErrorKind::Validation,
            &format!("- expected `` after table name `{name}`"),
        ));
    }
    if keyspace != format!("alternator_{name}") {
        return Err(not_found());
    }
    match alt.backend.table(name).await? {
        Some(t) if valid_name_chars(name) => Ok(t),
        _ => Err(not_found()),
    }
}

pub async fn tag_resource(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let schema = table_of_arn(alt, req.body).await?;
    authorize_table(alt, req, crate::backend::Permission::Alter, &schema.name).await?;
    let Some(list) = req.body.get("Tags").and_then(Json::as_array) else {
        return Err(Error::validation("Cannot parse tags"));
    };
    if list.is_empty() {
        return Err(Error::validation("The number of tags must be at least 1"));
    }
    let tags = parse_tags(list)?;
    if schema.tags.len() + list.len() > MAX_TAGS {
        return Err(Error::validation(
            "Number of Tags exceed the current limit for the provided ResourceArn",
        ));
    }
    let mut s = (*schema).clone();
    for (k, v) in tags {
        match s.tags.iter_mut().find(|(x, _)| *x == k) {
            Some(e) => e.1 = v,
            None => s.tags.push((k, v)),
        }
    }
    alt.backend.alter_table(s).await?;
    Ok(Json::Null)
}

pub async fn untag_resource(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let schema = table_of_arn(alt, req.body).await?;
    authorize_table(alt, req, crate::backend::Permission::Alter, &schema.name).await?;
    let Some(keys) = req.body.get("TagKeys").and_then(Json::as_array) else {
        return Err(Error::validation("Cannot parse tag keys"));
    };
    let mut names = Vec::with_capacity(keys.len());
    for k in keys {
        let Some(k) = k.as_str() else {
            return Err(Error::internal(
                "Internal server error: std::runtime_error (rjson::to_string_view expects a string value, got: kNumberType)",
            ));
        };
        check_reserved(k)?;
        names.push(k);
    }
    let mut s = (*schema).clone();
    s.tags.retain(|(k, _)| !names.contains(&k.as_str()));
    alt.backend.alter_table(s).await?;
    Ok(Json::Null)
}

pub async fn list_tags_of_resource(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let schema = table_of_arn(alt, req.body).await?;
    let mut tags: Vec<&(String, String)> = schema.tags.iter().collect();
    tags.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let list = tags
        .into_iter()
        .map(|(k, v)| {
            Json::object()
                .with("Key", Json::str(k.clone()))
                .with("Value", Json::str(v.clone()))
        })
        .collect();
    Ok(Json::object().with("Tags", Json::Array(list)))
}

pub async fn update_time_to_live(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let schema = get_table(alt, req.body).await?;
    authorize_table(alt, req, crate::backend::Permission::Alter, &schema.name).await?;
    let Some(spec) = req
        .body
        .get("TimeToLiveSpecification")
        .filter(|s| s.is_object())
    else {
        return Err(Error::validation(
            "UpdateTimeToLive missing mandatory TimeToLiveSpecification",
        ));
    };
    let Some(enabled) = spec.get("Enabled").and_then(Json::as_bool) else {
        return Err(Error::validation(
            "UpdateTimeToLive requires boolean Enabled",
        ));
    };
    let Some(attr) = spec.get("AttributeName").and_then(Json::as_str) else {
        return Err(Error::validation(
            "UpdateTimeToLive requires string AttributeName",
        ));
    };
    if attr.is_empty() || attr.len() > 255 {
        return Err(Error::validation(
            "The length of AttributeName must be between 1 and 255",
        ));
    }
    let mut s = (*schema).clone();
    match (&s.ttl_attribute, enabled) {
        (Some(_), true) => return Err(Error::validation("TTL is already enabled")),
        (None, false) => return Err(Error::validation("TTL is already disabled")),
        (Some(cur), false) if cur != attr => {
            return Err(Error::validation(format!(
                "Requested to disable TTL on attribute {attr}, but a different attribute {cur} is enabled."
            )));
        }
        _ => {}
    }
    s.ttl_attribute = enabled.then(|| attr.to_string());
    alt.backend.alter_table(s).await?;
    Ok(Json::object().with("TimeToLiveSpecification", spec.clone()))
}

pub async fn describe_time_to_live(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let schema = get_table(alt, req.body).await?;
    let desc = match &schema.ttl_attribute {
        Some(a) => Json::object()
            .with("TimeToLiveStatus", Json::str("ENABLED"))
            .with("AttributeName", Json::str(a.clone())),
        None => Json::object().with("TimeToLiveStatus", Json::str("DISABLED")),
    };
    Ok(Json::object().with("TimeToLiveDescription", desc))
}

/// The write isolation of a table: its tag, else the server default.
pub(crate) fn write_isolation(alt: &Alternator, schema: &TableSchema) -> WriteIsolation {
    schema
        .tag("system:write_isolation")
        .and_then(WriteIsolation::parse)
        .unwrap_or(alt.config().write_isolation)
}
