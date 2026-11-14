//! The ingest-geoip database configuration APIs (`/_ingest/geoip/database`,
//! `/_ingest/ip_location/database`, `/_ingest/geoip/stats`).
//!
//! Elasticsearch keeps database configurations in the cluster state (`IngestGeoIpMetadata`) and
//! has a downloader fetch each one from its provider (MaxMind or IPinfo). roll stores the
//! configurations exactly as Elasticsearch validates and reports them, both endpoints sharing
//! one store as they do there; nothing is downloaded (the node has no download task), so the
//! stats report zero downloads and no node databases, as an Elasticsearch node whose
//! downloader has not run yet does.
//!
//! Validation follows `DatabaseConfiguration`: an id may not start with `_`, `-` or `+` nor
//! contain index-name-forbidden characters; exactly one provider object (`maxmind` with an
//! `account_id`, `ipinfo`, or the read-only `web` / `local`); the name must be one the
//! provider supports; and a MaxMind database may be configured once.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};

/// The store family the configurations live in (`Metadata.xpack`).
const FAMILY: &str = "ingest.geoip_database";

/// `DatabaseConfiguration.MAXMIND_NAMES`, in Elasticsearch's order (the error lists them).
const MAXMIND_NAMES: &[&str] =
    &["GeoIP2-Country", "GeoIP2-Anonymous-IP", "GeoIP2-City", "GeoIP2-Connection-Type", "GeoIP2-Domain", "GeoIP2-ISP", "GeoIP2-Enterprise"];
/// `DatabaseConfiguration.IPINFO_NAMES`.
const IPINFO_NAMES: &[&str] = &["country", "standard_privacy", "standard_location", "standard_asn", "asn"];

/// `GET /_ingest/geoip/stats`.
pub fn stats(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(obj! {
        "stats" => obj! {
            "successful_downloads" => 0,
            "failed_downloads" => 0,
            "total_download_time" => 0,
            "databases_count" => 0,
            "skipped_updates" => 0,
            "expired_databases" => 0,
        },
        "nodes" => Map::new(),
    })
}

/// `DatabaseConfiguration.validateId`.
fn validate_id(id: &str) -> Option<String> {
    const BAD: [char; 10] = [' ', '"', '*', ',', '/', '<', '>', '?', '\\', '|'];
    if id.contains(BAD) {
        return Some(format!("invalid database configuration id [{id}]: must not contain the following characters [' ','\"','*',',','/','<','>','?','\\','|']"));
    }
    if id.starts_with(['_', '-', '+']) {
        return Some(format!("invalid database configuration id [{id}]: must not start with '_', '-', or '+'"));
    }
    None
}

/// Parses and validates a configuration body (`DatabaseConfiguration.parse` + `validate`).
fn parse(ctx: &Ctx, id: &str) -> EsResult<Map> {
    let body = ctx.body()?.unwrap_or(Value::Null);
    let Value::Obj(m) = body else {
        let what = if matches!(body, Value::Arr(_)) { "START_ARRAY" } else { "VALUE" };
        return Err(EsError::x_content_parse(1, 1, format!("[database] Expected START_OBJECT but was: {what}")));
    };
    let mut name = None;
    let mut providers: Vec<(&str, &Value)> = Vec::new();
    for (k, v) in &m {
        match k.as_str() {
            "name" => name = v.as_str().map(str::to_string),
            "maxmind" | "ipinfo" | "web" | "local" => providers.push((k.as_str(), v)),
            _ => return Err(crate::xpack::unknown_field(ctx, "database", k)),
        }
    }
    if let Some(("maxmind", v)) = providers.first().copied()
        && v.get("account_id").is_none()
    {
        let (l, c) = crate::xpack::key_loc(ctx, "maxmind");
        return Err(EsError::x_content_parse(l, c, "[database] failed to parse field [maxmind]").caused_by(EsError::illegal_argument("Required [account_id]")));
    }
    if providers.len() != 1 {
        return Err(EsError::x_content_parse_bare("Failed to build [database] after last required field arrived")
            .caused_by(EsError::illegal_argument(format!("Exactly one provider object must be specified, but [{}] were found", providers.len()))));
    }
    let (provider, _) = providers[0];
    // `web` (the default downloader's databases) and `local` (files on the node) are managed by
    // Elasticsearch itself.
    if matches!(provider, "web" | "local") {
        return Err(EsError::illegal_argument(format!("Database {id} is read only")));
    }
    let mut errors = Vec::new();
    if let Some(e) = validate_id(id) {
        errors.push(e);
    }
    let name = name.unwrap_or_default();
    let names = if provider == "maxmind" { MAXMIND_NAMES } else { IPINFO_NAMES };
    if !names.contains(&name.as_str()) {
        errors.push(format!("invalid name [{name}]: must be a supported name ([[{}]])", names.join(", ")));
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    // Field order as Elasticsearch prints it: name, then the provider.
    let mut out = Map::new();
    out.insert("name".into(), Value::from(name));
    for (k, v) in &m {
        if k != "name" {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(out)
}

/// `PUT /_ingest/{geoip,ip_location}/database/{id}`.
pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.param("id").unwrap_or_default().to_string();
    let database = parse(ctx, &id)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            // `TransportPutDatabaseConfigurationAction.validatePrerequisites`: one configuration
            // per MaxMind database.
            if database.contains_key("maxmind") {
                let name = database.get("name").and_then(Value::as_str).unwrap_or_default();
                let dup = st.metadata.xpack.iter(FAMILY).find(|(other, v)| {
                    *other != &id
                        && v.get("database").is_some_and(|d| d.get("maxmind").is_some() && d.get("name").and_then(Value::as_str) == Some(name))
                });
                if let Some((other, _)) = dup {
                    return Err(EsError::illegal_argument(format!("database [{name}] is already being downloaded via configuration [{other}]")));
                }
            }
            // Elasticsearch keeps an unchanged configuration (and its version) as it is.
            let prior = st.metadata.xpack.get(FAMILY, &id).cloned();
            if prior.as_ref().and_then(|p| p.get("database")).and_then(Value::as_object) == Some(&database) {
                return Ok(());
            }
            let version = prior.as_ref().and_then(|p| p.get("version")).and_then(Value::as_i64).map_or(1, |v| v + 1);
            let entry = obj! { "id" => id.clone(), "version" => version, "modified_date_millis" => now, "database" => Value::Obj(database) };
            crate::xpack::store::put(st, FAMILY, &id, entry, now)?;
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `GET /_ingest/{geoip,ip_location}/database[/{id}]`: the named configurations that exist
/// (unknown ids are left out, not errors), or all of them.
pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.cluster.state();
    let ids: Vec<&str> = ctx.path_param("id").map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let mut out = Vec::new();
    if ids.is_empty() || ids == ["*"] || ids == ["_all"] {
        out.extend(st.metadata.xpack.iter(FAMILY).map(|(_, v)| v.clone()));
    } else {
        for id in ids {
            if let Some(v) = st.metadata.xpack.get(FAMILY, id) {
                out.push(v.clone());
            }
        }
    }
    Reply::ok(obj! { "databases" => Value::Arr(out) })
}

/// `DELETE /_ingest/{geoip,ip_location}/database/{id}` (one id; a list is a missing id).
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.param("id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            if crate::xpack::store::remove(st, FAMILY, &id).is_none() {
                return Err(EsError::resource_not_found(format!("Database configuration not found: {id}")));
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_validated_like_elasticsearch() {
        assert_eq!(validate_id("Compat-1"), None);
        assert_eq!(validate_id("_x").unwrap(), "invalid database configuration id [_x]: must not start with '_', '-', or '+'");
        assert_eq!(
            validate_id("a b").unwrap(),
            "invalid database configuration id [a b]: must not contain the following characters [' ','\"','*',',','/','<','>','?','\\','|']"
        );
    }
}
