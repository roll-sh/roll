//! Discover's saved searches (Kibana's saved searches, OpenSearch Dashboards' saved queries): a
//! view of a cluster under a name, kept in roll's catalogue (`search_saved_searches`), not in the
//! tenant's cluster. So they survive the indices they read being deleted, a member who may read
//! the cluster sees them (roll's permissions decide, as for every `/v1/search/clusters/{id}/…`
//! route), and nothing in the cluster's own data or `_cat` output changes. A deleted cluster keeps
//! them until it's purged, so a restore brings them back; a branch starts with none of its
//! parent's (a branch is a copy of the data, not of the people's views of it).
//!
//! The spec is the dashboard's view (`apps/dashboard/src/lib/search-discover.ts`): `view`,
//! `time_field`, `mode`, `query`, `range`, `columns`, `sort`, checked here for shape and size only
//! (the cluster decides whether the view and query mean anything when the search is opened).

use crate::control::{Control, ControlError, Result};
use crate::store::{SavedRefused, SavedRow, SearchCluster};
use serde_json::{Map, Value, json};

/// Saved searches a cluster keeps at most.
pub const MAX_SAVED: i64 = 500;
const MAX_NAME: usize = 128;
const MAX_VIEW: usize = 1024;
const MAX_FIELD: usize = 512;
const MAX_QUERY: usize = 16 << 10;
const MAX_COLUMNS: usize = 100;
const KEYS: &[&str] = &["view", "time_field", "mode", "query", "range", "columns", "sort"];

fn bad(why: impl Into<String>) -> ControlError {
    ControlError::Invalid(why.into())
}

/// A saved search's name: trimmed, 1 to 128 characters, no control characters.
pub fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME || n.chars().any(char::is_control) {
        return Err(bad(format!("a saved search's name is 1 to {MAX_NAME} characters, with no control characters")));
    }
    Ok(n.to_string())
}

/// A saved search's spec, checked: what the dashboard's view holds, no more, each part within
/// its size, the mode one of the cluster's flavor's languages. Unknown keys are refused.
pub fn check_spec(spec: &Value, flavor: &str) -> Result<Value> {
    let Value::Object(m) = spec else { return Err(bad("[search] is an object: view, time_field, mode, query, range, columns, sort")) };
    if let Some(k) = m.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(bad(format!("[search] has no field [{k}] (it takes {})", KEYS.join(", "))));
    }
    let text = |k: &str, max: usize, required: bool| -> Result<Option<String>> {
        match m.get(k) {
            None | Some(Value::Null) if !required => Ok(None),
            Some(Value::String(s)) if s.len() <= max && (!required || !s.trim().is_empty()) => Ok(Some(s.clone())),
            _ => Err(bad(format!("[search.{k}] is a string of at most {max} bytes{}", if required { ", not empty" } else { "" }))),
        }
    };
    let mut out = Map::new();
    out.insert("view".into(), text("view", MAX_VIEW, true)?.into());
    out.insert("time_field".into(), text("time_field", MAX_FIELD, false)?.map_or(Value::Null, Value::from));
    let modes: &[&str] = if flavor == "opensearch" { &["dql", "ppl"] } else { &["kql", "esql"] };
    let mode = text("mode", 8, true)?.unwrap_or_default();
    if !modes.contains(&mode.as_str()) {
        return Err(bad(format!("[search.mode] is one of {} on this cluster", modes.join(", "))));
    }
    out.insert("mode".into(), mode.into());
    out.insert("query".into(), text("query", MAX_QUERY, false)?.unwrap_or_default().into());
    let range = match m.get("range") {
        None => json!({ "last": null }),
        Some(Value::Object(r)) if r.len() == 1 && r.contains_key("last") => match &r["last"] {
            Value::Null => json!({ "last": null }),
            Value::String(l) if l.len() <= 8 && l.ends_with(['s', 'm', 'h', 'd']) && l[..l.len() - 1].parse::<u32>().is_ok_and(|n| n > 0) => json!({ "last": l }),
            _ => return Err(bad("[search.range.last] is null (all time) or a number and a unit: 15m, 24h, 7d")),
        },
        Some(Value::Object(r)) if r.len() == 2 => match (r.get("gte").and_then(Value::as_i64), r.get("lt").and_then(Value::as_i64)) {
            (Some(gte), Some(lt)) if gte < lt => json!({ "gte": gte, "lt": lt }),
            _ => return Err(bad("[search.range] with bounds is {gte, lt}: epoch milliseconds, gte below lt")),
        },
        _ => return Err(bad("[search.range] is {last} or {gte, lt}")),
    };
    out.insert("range".into(), range);
    let columns = match m.get("columns") {
        None => Vec::new(),
        Some(Value::Array(a)) if a.len() <= MAX_COLUMNS && a.iter().all(|c| c.as_str().is_some_and(|c| !c.is_empty() && c.len() <= MAX_FIELD)) => a.clone(),
        _ => return Err(bad(format!("[search.columns] is at most {MAX_COLUMNS} field names"))),
    };
    out.insert("columns".into(), Value::Array(columns));
    let sort = match m.get("sort") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Object(s)) if s.len() == 3 => {
            let f = |k: &str| s.get(k).and_then(Value::as_str).filter(|v| !v.is_empty() && v.len() <= MAX_FIELD);
            match (f("column"), f("field"), f("order")) {
                (Some(c), Some(fl), Some(o @ ("asc" | "desc"))) => json!({ "column": c, "field": fl, "order": o }),
                _ => return Err(bad("[search.sort] is null or {column, field, order: asc|desc}")),
            }
        }
        _ => return Err(bad("[search.sort] is null or {column, field, order: asc|desc}")),
    };
    out.insert("sort".into(), sort);
    Ok(Value::Object(out))
}

/// A saved search as the API answers it.
fn answer(r: &SavedRow) -> Value {
    json!({
        "id": r.id,
        "name": r.name,
        "search": serde_json::from_str::<Value>(&r.spec).unwrap_or(Value::Null),
        "made_by": r.made_by,
        "created_at": r.created_at,
        "updated_at": r.updated_at,
    })
}

fn refused(r: SavedRefused, name: &str) -> ControlError {
    match r {
        SavedRefused::Name => ControlError::Conflict(format!("a saved search named [{name}] exists already")),
        SavedRefused::Full => ControlError::Conflict(format!("a cluster keeps at most {MAX_SAVED} saved searches: delete one first")),
        SavedRefused::NotFound => ControlError::NotFound("no such saved search".into()),
    }
}

impl Control {
    /// `GET …/saved-searches`: the cluster's saved searches, by name.
    pub async fn saved_searches(&self, c: &SearchCluster) -> Result<Value> {
        let rows = self.store.saved_searches(&c.id)?;
        Ok(json!({ "saved_searches": rows.iter().map(answer).collect::<Vec<_>>() }))
    }

    pub async fn saved_search(&self, c: &SearchCluster, id: &str) -> Result<Value> {
        self.store.saved_search(&c.id, id)?.map(|r| answer(&r)).ok_or_else(|| ControlError::NotFound("no such saved search".into()))
    }

    /// `POST …/saved-searches` `{name, search}`.
    pub async fn save_search(&self, c: &SearchCluster, name: &str, spec: &Value, made_by: Option<&str>) -> Result<Value> {
        let name = check_name(name)?;
        let spec = check_spec(spec, &c.flavor)?;
        let now = crate::control::now();
        let row = SavedRow { id: format!("ss_{}", crate::control::random_string(12)), name: name.clone(), spec: spec.to_string(), made_by: made_by.map(str::to_string), created_at: now, updated_at: now };
        self.store.add_saved_search(&c.id, &row, MAX_SAVED)?.map_err(|r| refused(r, &name))?;
        Ok(answer(&row))
    }

    /// `PUT …/saved-searches/{search}` `{name?, search?}`: a rename, a new spec, or both.
    pub async fn update_saved_search(&self, c: &SearchCluster, id: &str, name: Option<&str>, spec: Option<&Value>) -> Result<Value> {
        if name.is_none() && spec.is_none() {
            return Err(bad("give name, search or both"));
        }
        let name = name.map(check_name).transpose()?;
        let spec = spec.map(|s| check_spec(s, &c.flavor)).transpose()?.map(|s| s.to_string());
        let row = self.store.update_saved_search(&c.id, id, name.as_deref(), spec.as_deref(), crate::control::now())?.map_err(|r| refused(r, name.as_deref().unwrap_or_default()))?;
        Ok(answer(&row))
    }

    /// `DELETE …/saved-searches/{search}`.
    pub async fn delete_saved_search(&self, c: &SearchCluster, id: &str) -> Result<()> {
        if self.store.delete_saved_search(&c.id, id)? { Ok(()) } else { Err(ControlError::NotFound("no such saved search".into())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_the_dashboards_view_within_their_sizes() {
        let spec = json!({"view": "logs-*", "time_field": "@timestamp", "mode": "kql", "query": "status >= 500", "range": {"last": "24h"}, "columns": ["host"], "sort": {"column": "host", "field": "host", "order": "asc"}});
        assert_eq!(check_spec(&spec, "elasticsearch").unwrap(), spec);
        let minimal = check_spec(&json!({"view": "logs", "mode": "ppl"}), "opensearch").unwrap();
        assert_eq!(minimal, json!({"view": "logs", "time_field": null, "mode": "ppl", "query": "", "range": {"last": null}, "columns": [], "sort": null}));
        for (bad, why) in [
            (json!({"view": "x", "mode": "ppl"}), "kql, esql"),
            (json!({"view": "x", "mode": "kql", "extra": 1}), "no field [extra]"),
            (json!({"view": "", "mode": "kql"}), "not empty"),
            (json!({"view": "x", "mode": "kql", "range": {"gte": 5, "lt": 1}}), "gte below lt"),
            (json!({"view": "x", "mode": "kql", "range": {"last": "-1h"}}), "a number and a unit"),
            (json!({"view": "x", "mode": "kql", "sort": {"column": "a", "field": "a", "order": "up"}}), "asc|desc"),
        ] {
            assert!(check_spec(&bad, "elasticsearch").unwrap_err().to_string().contains(why), "{bad}");
        }
        assert!(check_name("  errors  ").is_ok_and(|n| n == "errors") && check_name(" ").is_err() && check_name("a\nb").is_err());
    }
}
