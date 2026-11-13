//! Stored objects of the x-pack APIs: users, roles, API keys, ML jobs, transforms, connectors,
//! policies and the rest, kept in the cluster metadata.
//!
//! Elasticsearch keeps most of these as documents of system indices (`.security-7`,
//! `.ml-config`, `.elastic-connectors-v1`, ...) and a few in the cluster state (ILM and enrich
//! policies, ES|QL views). roll keeps all of them in the metadata ([`XpackState`], persisted
//! with the rest of the cluster state, small and rarely written), grouped by *family*
//! (`security.user`, `ml.job`, ...), each a sorted map of id to the object's JSON. For the
//! families Elasticsearch keeps in a system index, the first write also creates that index
//! (hidden, empty, with its alias) the way Elasticsearch does, so index listings,
//! `_features/_reset` and test-cluster cleanup see what they see on Elasticsearch; deleting
//! the index drops the family ([`on_index_deleted`]), as deleting it drops the documents there.
//!
//! The state is an `Arc` per family behind an `Arc`, so a change clones only the family it
//! touches, and readers share the published snapshot without locks.

use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::obj;
use std::collections::BTreeMap;
use std::sync::Arc;

/// One family's objects by id.
pub type Family = BTreeMap<String, Value>;

/// Every family's objects (`Metadata.xpack`).
#[derive(Clone, Debug, Default)]
pub struct XpackState {
    families: BTreeMap<String, Arc<Family>>,
}

/// Where Elasticsearch keeps a family: its system index, the index's alias, and whether the
/// index is a system index (some, like the connectors index, are only hidden).
struct Home {
    prefix: &'static str,
    index: &'static str,
    alias: Option<&'static str>,
}

/// Families by prefix (the first match wins), with Elasticsearch 9.5.5's index names.
const HOMES: &[Home] = &[
    Home { prefix: "security.token", index: ".security-tokens-7", alias: Some(".security-tokens") },
    Home { prefix: "security.profile", index: ".security-profile-8", alias: Some(".security-profile") },
    Home { prefix: "security.", index: ".security-7", alias: Some(".security") },
    Home { prefix: "opendistro_security.", index: ".opendistro_security", alias: None },
    Home { prefix: "ml.job", index: ".ml-config", alias: None },
    Home { prefix: "ml.datafeed", index: ".ml-config", alias: None },
    Home { prefix: "ml.dfa", index: ".ml-config", alias: None },
    Home { prefix: "ml.trained_model", index: ".ml-inference-000005", alias: Some(".ml-inference-native-000002") },
    Home { prefix: "ml.", index: ".ml-meta", alias: None },
    Home { prefix: "connector.sync_job", index: ".elastic-connectors-sync-jobs-v1", alias: Some(".elastic-connectors-sync-jobs") },
    Home { prefix: "connector_secret", index: ".connector-secrets", alias: None },
    Home { prefix: "fleet_secret", index: ".fleet-secrets-7", alias: Some(".fleet-secrets") },
    Home { prefix: "connector", index: ".elastic-connectors-v1", alias: Some(".elastic-connectors") },
    Home { prefix: "query_rules", index: ".query-rules-2", alias: Some(".query-rules") },
    Home { prefix: "search_application", index: ".search-app-1", alias: Some(".search-app") },
    Home { prefix: "transform", index: ".transform-internal-007", alias: Some(".transform-internal-007-alias") },
    Home { prefix: "watcher.watch", index: ".watches", alias: None },
    Home { prefix: "logstash.", index: ".logstash", alias: None },
    Home { prefix: "inference_secret", index: ".secrets-inference", alias: None },
    Home { prefix: "inference", index: ".inference", alias: Some(".inference-alias") },
];

fn home(family: &str) -> Option<&'static Home> {
    HOMES.iter().find(|h| family.starts_with(h.prefix))
}

/// The system index a family's objects live in (`None`: the cluster state).
pub fn home_index(family: &str) -> Option<&'static str> {
    home(family).map(|h| h.index)
}

impl XpackState {
    pub fn family(&self, family: &str) -> Option<&Family> {
        self.families.get(family).map(|f| &**f)
    }

    pub fn get(&self, family: &str, id: &str) -> Option<&Value> {
        self.families.get(family)?.get(id)
    }

    /// The family's objects in id order.
    pub fn iter<'a>(&'a self, family: &str) -> impl Iterator<Item = (&'a String, &'a Value)> + 'a {
        self.families.get(family).into_iter().flat_map(|f| f.iter())
    }

    pub fn len(&self, family: &str) -> usize {
        self.families.get(family).map_or(0, |f| f.len())
    }

    pub fn is_empty(&self) -> bool {
        self.families.values().all(|f| f.is_empty())
    }

    /// The form stored with the cluster state.
    pub fn to_stored(&self) -> Value {
        let mut m = Map::new();
        for (name, f) in &self.families {
            if f.is_empty() {
                continue;
            }
            m.insert(name.clone(), Value::Obj(f.iter().map(|(k, v)| (k.clone(), v.clone())).collect()));
        }
        Value::Obj(m)
    }

    pub fn from_stored(v: Option<&Value>) -> XpackState {
        let mut out = XpackState::default();
        if let Some(Value::Obj(m)) = v {
            for (name, f) in m {
                if let Value::Obj(objs) = f {
                    out.families.insert(name.clone(), Arc::new(objs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()));
                }
            }
        }
        out
    }
}

/// The family to change, cloned out of the published state if shared.
pub fn family_mut<'a>(st: &'a mut ClusterState, family: &str) -> &'a mut Family {
    let x = Arc::make_mut(&mut st.metadata.xpack);
    Arc::make_mut(x.families.entry(family.to_string()).or_default())
}

/// Stores an object, creating its family's system index first; `true` when it is new.
pub fn put(st: &mut ClusterState, family: &str, id: &str, v: Value, now_ms: i64) -> EsResult<bool> {
    ensure_home(st, family, now_ms)?;
    Ok(family_mut(st, family).insert(id.to_string(), v).is_none())
}

/// Removes an object.
pub fn remove(st: &mut ClusterState, family: &str, id: &str) -> Option<Value> {
    st.metadata.xpack.get(family, id)?;
    family_mut(st, family).remove(id)
}

/// Removes every object of a family.
pub fn clear(st: &mut ClusterState, family: &str) {
    if st.metadata.xpack.len(family) > 0 {
        family_mut(st, family).clear();
    }
}

/// Creates the system index Elasticsearch keeps `family` in, if it doesn't exist.
pub fn ensure_home(st: &mut ClusterState, family: &str, now_ms: i64) -> EsResult<()> {
    let Some(h) = home(family) else { return Ok(()) };
    ensure_index(st, h.index, h.alias, now_ms)
}

/// Creates a hidden internal index (with an alias) the way Elasticsearch creates its system
/// indices: one shard, `0-1` replicas, a minimal non-dynamic mapping.
pub fn ensure_index(st: &mut ClusterState, index: &str, alias: Option<&str>, now_ms: i64) -> EsResult<()> {
    ensure_index_with(st, index, alias, None, now_ms)
}

/// [`ensure_index`] with the index's own field mappings (`properties`).
pub fn ensure_index_with(st: &mut ClusterState, index: &str, alias: Option<&str>, properties: Option<Value>, now_ms: i64) -> EsResult<()> {
    if st.metadata.indices.contains_key(index) {
        return Ok(());
    }
    let mut req = crate::api::indices::CreateRequest::default();
    for (k, v) in [("index.hidden", "true"), ("index.number_of_shards", "1"), ("index.auto_expand_replicas", "0-1")] {
        req.settings.set(k, v);
    }
    let mut mapping = obj! { "dynamic" => "false", "_meta" => obj! { "managed_index_mappings_version" => 1, "version" => crate::version::ES_VERSION } };
    if let (Some(p), Value::Obj(m)) = (properties, &mut mapping) {
        m.insert("properties".into(), p);
    }
    if let Value::Obj(m) = mapping {
        req.mappings = Some(m);
    }
    if let Some(a) = alias {
        req.aliases.push((a.to_string(), crate::meta::AliasMeta { is_hidden: Some(true), ..Default::default() }));
    }
    crate::api::indices::create_index(st, index, index, req, now_ms).map(|_| ())
}

/// Adds (or replaces) an alias of an existing index.
pub fn set_alias(st: &mut ClusterState, index: &str, alias: &str, meta: crate::meta::AliasMeta) {
    if let Some(idx) = st.metadata.indices.get_mut(index) {
        let idx = Arc::make_mut(idx);
        idx.aliases.insert(alias.to_string(), meta);
        idx.aliases_version += 1;
    }
}

/// Removes an alias from every index that has it.
pub fn remove_alias(st: &mut ClusterState, alias: &str) {
    let holders: Vec<String> = st.metadata.indices.values().filter(|i| i.aliases.contains_key(alias)).map(|i| i.name.clone()).collect();
    for h in holders {
        if let Some(idx) = st.metadata.indices.get_mut(&h) {
            let idx = Arc::make_mut(idx);
            idx.aliases.remove(alias);
            idx.aliases_version += 1;
        }
    }
}

/// Drops the families an index held (its deletion deletes their documents on Elasticsearch).
pub fn on_index_deleted(st: &mut ClusterState, index: &str) {
    if st.metadata.xpack.is_empty() || !index.starts_with('.') {
        return;
    }
    let families: Vec<String> = st.metadata.xpack.families.keys().filter(|f| home(f).is_some_and(|h| h.index == index)).cloned().collect();
    for f in families {
        clear(st, &f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_homes() {
        let mut st = ClusterState::new("u");
        assert!(put(&mut st, "security.user", "juan", obj! { "a" => 1 }, 0).unwrap());
        assert!(!put(&mut st, "security.user", "juan", obj! { "a" => 2 }, 0).unwrap());
        assert!(st.metadata.indices.contains_key(".security-7"));
        assert!(st.metadata.indices[".security-7"].aliases.contains_key(".security"));
        put(&mut st, "ml.calendar", "c", obj! {}, 0).unwrap();
        assert!(st.metadata.indices.contains_key(".ml-meta"));
        let stored = st.metadata.xpack.to_stored();
        let back = XpackState::from_stored(Some(&stored));
        assert_eq!(back.get("security.user", "juan"), Some(&obj! { "a" => 2 }));
        on_index_deleted(&mut st, ".security-7");
        assert_eq!(st.metadata.xpack.len("security.user"), 0);
        assert_eq!(st.metadata.xpack.len("ml.calendar"), 1);
        assert!(remove(&mut st, "ml.calendar", "c").is_some());
        assert!(remove(&mut st, "ml.calendar", "c").is_none());
    }

    #[test]
    fn cluster_state_families_have_no_index() {
        let mut st = ClusterState::new("u");
        put(&mut st, "ilm.policy", "p", obj! {}, 0).unwrap();
        assert!(st.metadata.indices.is_empty());
    }
}
