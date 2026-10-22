//! Cluster metadata: the state of one tenant cluster.
//!
//! [`ClusterState`] is immutable once published; updates clone it, change the clone and swap it
//! in ([`crate::cluster::Cluster::update`]). Indices are behind `Arc`s so a clone copies only the
//! maps, not the mappings.
//!
//! The state serializes to one JSON document ([`ClusterState::to_json`]) for the metadata store;
//! mappings are stored as `GET _mapping` prints them and re-parsed on load, which also proves
//! the printed form round-trips.

pub mod aliases;
pub mod auto_expand;
pub mod blocks;
pub mod cluster_settings;
mod cluster_settings_table_os;
pub mod data_streams;
pub mod desired_nodes;
mod cluster_settings_table;
pub mod index_mode;
pub mod index_settings;
mod index_settings_table_os;
pub mod index_sort;
pub mod names;
pub mod resolve;
pub mod routing;
pub mod settings;
pub mod snapshots;
pub mod synonyms;
pub mod templates;

use crate::error::EsError;
use crate::json::{Map, Value};
use crate::mapping::{Mapping, MappingCtx};
use crate::obj;
use settings::Settings;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexState {
    Open,
    Close,
}

impl IndexState {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexState::Open => "open",
            IndexState::Close => "close",
        }
    }
}

/// An alias as attached to an index.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AliasMeta {
    pub filter: Option<Value>,
    pub index_routing: Option<String>,
    pub search_routing: Option<String>,
    pub is_write_index: Option<bool>,
    pub is_hidden: Option<bool>,
}

impl AliasMeta {
    /// As `GET _alias` prints it.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        if let Some(f) = &self.filter {
            m.insert("filter".into(), f.clone());
        }
        if let Some(r) = &self.index_routing {
            m.insert("index_routing".into(), Value::from(r.clone()));
        }
        if let Some(r) = &self.search_routing {
            m.insert("search_routing".into(), Value::from(r.clone()));
        }
        if let Some(w) = self.is_write_index {
            m.insert("is_write_index".into(), w.into());
        }
        if let Some(h) = self.is_hidden {
            m.insert("is_hidden".into(), h.into());
        }
        Value::Obj(m)
    }

    pub fn from_value(v: &Value) -> AliasMeta {
        AliasMeta {
            filter: v.get("filter").cloned(),
            index_routing: v.get("index_routing").map(Value::to_es_string),
            search_routing: v.get("search_routing").map(Value::to_es_string),
            is_write_index: v.get("is_write_index").and_then(Value::as_bool),
            is_hidden: v.get("is_hidden").and_then(Value::as_bool),
        }
    }
}

/// One index.
#[derive(Clone, Debug)]
pub struct IndexMeta {
    pub name: String,
    pub uuid: String,
    pub state: IndexState,
    /// All index settings, `index.`-prefixed, including the managed ones (`index.uuid`, …).
    pub settings: Settings,
    pub mapping: Arc<Mapping>,
    pub aliases: BTreeMap<String, AliasMeta>,
    pub version: u64,
    pub mapping_version: u64,
    pub settings_version: u64,
    pub aliases_version: u64,
    pub routing_num_shards: u32,
    pub primary_terms: Vec<u64>,
    pub system: bool,
    /// Set when the index was restored from a snapshot: the recovery source `_recovery`
    /// reports (`repository`, `snapshot`, `version`, `index`, `restoreUUID`).
    pub recovery_source: Option<Arc<Map>>,
    /// The dialect of the cluster the index lives in (not persisted): which field types and
    /// mapping parameters its mapping may use.
    pub dialect: crate::cluster::Dialect,
}

impl IndexMeta {
    pub fn number_of_shards(&self) -> u32 {
        self.settings.get("index.number_of_shards").and_then(|s| s.parse().ok()).unwrap_or(1)
    }

    pub fn number_of_replicas(&self) -> u32 {
        self.settings.get("index.number_of_replicas").and_then(|s| s.parse().ok()).unwrap_or(1)
    }

    pub fn creation_date(&self) -> i64 {
        self.settings.get("index.creation_date").and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    pub fn is_hidden(&self) -> bool {
        self.settings.get("index.hidden") == Some("true")
    }

    pub fn is_closed(&self) -> bool {
        self.state == IndexState::Close
    }

    pub fn bool_setting(&self, key: &str) -> bool {
        self.settings.get(key) == Some("true")
    }

    pub fn mapping_ctx(&self) -> MappingCtx {
        MappingCtx::from_settings_in(self.dialect, &self.settings)
    }

    /// The mapping as `GET _mapping` prints it (`{}` when there's none).
    pub fn mapping_json(&self, include_defaults: bool) -> Value {
        self.mapping.to_value(&self.mapping_ctx(), include_defaults)
    }

    /// The stored form (cluster state persistence, snapshots).
    pub fn to_json(&self) -> Value {
        let mut aliases = Map::new();
        for (k, a) in &self.aliases {
            aliases.insert(k.clone(), a.to_value());
        }
        obj! {
            "uuid" => self.uuid.clone(),
            "state" => self.state.as_str(),
            "settings" => self.settings.to_flat_json(),
            "mappings" => self.mapping_json(false),
            "aliases" => Value::Obj(aliases),
            "version" => self.version,
            "mapping_version" => self.mapping_version,
            "settings_version" => self.settings_version,
            "aliases_version" => self.aliases_version,
            "routing_num_shards" => self.routing_num_shards,
            "primary_terms" => self.primary_terms.iter().map(|t| Value::from(*t)).collect::<Vec<_>>(),
            "system" => self.system,
            "recovery_source" => self.recovery_source.as_ref().map(|r| Value::Obj((**r).clone())).unwrap_or(Value::Null),
        }
    }

    pub fn from_json(name: &str, v: &Value) -> Result<IndexMeta, EsError> {
        IndexMeta::from_json_in(name, v, crate::cluster::Dialect::Elasticsearch)
    }

    /// [`IndexMeta::from_json`] for an index of a cluster speaking `dialect`.
    pub fn from_json_in(name: &str, v: &Value, dialect: crate::cluster::Dialect) -> Result<IndexMeta, EsError> {
        let settings = Settings::from_json(v.get("settings").unwrap_or(&Value::Null))?;
        let ctx = MappingCtx::from_settings_in(dialect, &settings);
        let mapping = match v.get("mappings").and_then(Value::as_object) {
            Some(m) if !m.is_empty() => Mapping::parse(m, &ctx)?,
            _ => Mapping::empty(),
        };
        let mut aliases = BTreeMap::new();
        if let Some(Value::Obj(a)) = v.get("aliases") {
            for (k, av) in a {
                aliases.insert(k.clone(), AliasMeta::from_value(av));
            }
        }
        let u = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(1) as u64;
        Ok(IndexMeta {
            name: name.to_string(),
            uuid: v.get("uuid").map(Value::to_es_string).unwrap_or_default(),
            state: if v.get("state").and_then(Value::as_str) == Some("close") { IndexState::Close } else { IndexState::Open },
            settings,
            mapping: Arc::new(mapping),
            aliases,
            version: u("version"),
            mapping_version: u("mapping_version"),
            settings_version: u("settings_version"),
            aliases_version: u("aliases_version"),
            routing_num_shards: v.get("routing_num_shards").and_then(Value::as_i64).unwrap_or(1) as u32,
            primary_terms: v.get("primary_terms").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).map(|t| t as u64).collect()).unwrap_or_default(),
            system: v.get("system").and_then(Value::as_bool).unwrap_or(false),
            recovery_source: v.get("recovery_source").and_then(Value::as_object).map(|m| Arc::new(m.clone())),
            dialect,
        })
    }
}

/// A stored script (`PUT _scripts/<id>`).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredScript {
    pub lang: String,
    /// A string, or for mustache an object/string template.
    pub source: Value,
    pub options: Option<Map>,
}

impl StoredScript {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("lang".into(), Value::from(self.lang.clone()));
        m.insert("source".into(), self.source.clone());
        if let Some(o) = &self.options
            && !o.is_empty()
        {
            m.insert("options".into(), Value::Obj(o.clone()));
        }
        Value::Obj(m)
    }
}

/// Metadata of one cluster.
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub cluster_uuid: String,
    pub version: u64,
    pub indices: BTreeMap<String, Arc<IndexMeta>>,
    pub index_templates: BTreeMap<String, Arc<templates::ComposableTemplate>>,
    pub component_templates: BTreeMap<String, Arc<templates::ComponentTemplate>>,
    pub templates: BTreeMap<String, Arc<templates::LegacyTemplate>>,
    pub persistent: Settings,
    pub transient: Settings,
    pub scripts: BTreeMap<String, StoredScript>,
    pub pipelines: BTreeMap<String, Value>,
    /// Voting configuration exclusions `(node_id, node_name)` (`_absent_` for the unknown one).
    pub voting_exclusions: Vec<(String, String)>,
    /// Data streams by name.
    pub data_streams: BTreeMap<String, Arc<data_streams::DataStream>>,
    /// Aliases of data streams by alias name.
    pub data_stream_aliases: BTreeMap<String, data_streams::DataStreamAlias>,
    /// Synonym sets (`/_synonyms`).
    pub synonyms: Arc<synonyms::Sets>,
    /// Bumped on every synonym set change (analysis chains built before are stale).
    pub synonyms_version: u64,
    /// The latest desired nodes (`/_internal/desired_nodes`).
    pub desired_nodes: Option<Arc<desired_nodes::DesiredNodes>>,
    /// Registered snapshot repositories by name.
    pub repositories: BTreeMap<String, Arc<snapshots::Repository>>,
    /// Repository contents by location ([`snapshots::Repository::location_key`]).
    pub repository_data: BTreeMap<String, Arc<snapshots::RepositoryData>>,
    /// The cluster's dialect (not persisted; the same as [`ClusterState::dialect`]).
    pub dialect: crate::cluster::Dialect,
}

impl Metadata {
    /// A cluster setting's value in effect: transient, then persistent.
    pub fn cluster_setting(&self, key: &str) -> Option<&str> {
        self.transient.get(key).or_else(|| self.persistent.get(key))
    }
}

/// The published state of a cluster.
#[derive(Clone, Debug, Default)]
pub struct ClusterState {
    pub version: u64,
    pub state_uuid: String,
    pub metadata: Metadata,
    /// The cluster's REST dialect (from its configuration; not persisted): what index
    /// creation and other state changes follow where Elasticsearch and OpenSearch differ.
    pub dialect: crate::cluster::Dialect,
}

impl ClusterState {
    pub fn new(cluster_uuid: &str) -> ClusterState {
        ClusterState {
            dialect: Default::default(),
            version: 1,
            state_uuid: names::random_uuid(),
            metadata: Metadata { cluster_uuid: cluster_uuid.to_string(), version: 1, ..Default::default() },
        }
    }

    pub fn index(&self, name: &str) -> Option<&Arc<IndexMeta>> {
        self.metadata.indices.get(name)
    }

    /// Every alias name, with the indices it points at.
    pub fn aliases(&self) -> BTreeMap<&str, Vec<(&IndexMeta, &AliasMeta)>> {
        let mut out: BTreeMap<&str, Vec<(&IndexMeta, &AliasMeta)>> = BTreeMap::new();
        for idx in self.metadata.indices.values() {
            for (a, am) in &idx.aliases {
                out.entry(a.as_str()).or_default().push((idx, am));
            }
        }
        out
    }

    /// At most one write index per alias.
    pub fn check_write_indices(&self) -> Result<(), EsError> {
        for (alias, members) in self.aliases() {
            let mut writers: Vec<&str> = members.iter().filter(|(_, a)| a.is_write_index == Some(true)).map(|(i, _)| i.name.as_str()).collect();
            if writers.len() > 1 {
                writers.sort_unstable();
                return Err(EsError::illegal_state(format!("alias [{alias}] has more than one write index [{}]", writers.join(","))));
            }
        }
        Ok(())
    }

    /// Every alias's `is_hidden` must agree across its indices
    /// (`IndexAbstraction.Alias.validateAliasProperties`).
    pub fn check_hidden_aliases(&self) -> Result<(), EsError> {
        for (alias, members) in self.aliases() {
            let mut hidden: Vec<&str> = members.iter().filter(|(_, a)| a.is_hidden == Some(true)).map(|(i, _)| i.name.as_str()).collect();
            if hidden.is_empty() || hidden.len() == members.len() {
                continue;
            }
            let mut visible: Vec<&str> = members.iter().filter(|(_, a)| a.is_hidden != Some(true)).map(|(i, _)| i.name.as_str()).collect();
            hidden.sort_unstable();
            visible.sort_unstable();
            return Err(EsError::illegal_state(format!(
                "alias [{alias}] has is_hidden set to true on indices [{}] but does not have is_hidden set to true on indices [{}]; alias must have the same is_hidden setting on all indices",
                hidden.join(","),
                visible.join(",")
            )));
        }
        Ok(())
    }

    /// Index, alias and data stream names must not collide (`ProjectMetadata.Builder`'s
    /// duplicate check, for the collisions involving data streams).
    pub fn check_unique_names(&self) -> Result<(), EsError> {
        let md = &self.metadata;
        if md.data_streams.is_empty() && md.data_stream_aliases.is_empty() {
            return Ok(());
        }
        let index_aliases: std::collections::BTreeSet<&str> = md.indices.values().flat_map(|i| i.aliases.keys().map(String::as_str)).collect();
        let mut duplicates = Vec::new();
        let mut alias_vs_ds: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for alias in md.data_stream_aliases.keys() {
            if index_aliases.contains(alias.as_str()) {
                duplicates.push(format!("data stream alias and indices alias have the same name ({alias})"));
            }
            if md.data_streams.contains_key(alias) {
                alias_vs_ds.insert(alias);
            }
        }
        for alias in &index_aliases {
            if md.data_streams.contains_key(*alias) {
                alias_vs_ds.insert(alias);
            }
        }
        for name in md.data_streams.keys() {
            if md.indices.contains_key(name) {
                duplicates.push(format!("data stream [{name}] conflicts with index"));
            }
        }
        for alias in alias_vs_ds {
            let mut reported = false;
            for idx in md.indices.values() {
                if idx.aliases.contains_key(alias) {
                    duplicates.push(format!("{alias} (alias of [{}/{}]) conflicts with data stream", idx.name, idx.uuid));
                    reported = true;
                }
            }
            if !reported {
                duplicates.push(format!("data stream alias and data stream have the same name ({alias})"));
            }
        }
        if duplicates.is_empty() {
            return Ok(());
        }
        Err(EsError::illegal_state(format!(
            "index, alias, data stream, view, and dataset names need to be unique, but the following duplicates were found [{}]",
            duplicates.join(",")
        )))
    }

    pub fn has_alias(&self, name: &str) -> bool {
        self.metadata.indices.values().any(|i| i.aliases.contains_key(name))
    }

    /// The document stored in the metadata store.
    pub fn to_json(&self) -> Value {
        let md = &self.metadata;
        let mut indices = Map::new();
        for (k, i) in &md.indices {
            indices.insert(k.clone(), i.to_json());
        }
        let mut its = Map::new();
        for (k, t) in &md.index_templates {
            its.insert(k.clone(), t.to_value());
        }
        let mut cts = Map::new();
        for (k, t) in &md.component_templates {
            cts.insert(k.clone(), t.to_value());
        }
        let mut lts = Map::new();
        for (k, t) in &md.templates {
            lts.insert(k.clone(), t.to_value(true));
        }
        let mut scripts = Map::new();
        for (k, s) in &md.scripts {
            scripts.insert(k.clone(), s.to_value());
        }
        let mut pipelines = Map::new();
        for (k, p) in &md.pipelines {
            pipelines.insert(k.clone(), p.clone());
        }
        obj! {
            "format" => 1,
            "version" => self.version,
            "state_uuid" => self.state_uuid.clone(),
            "cluster_uuid" => md.cluster_uuid.clone(),
            "metadata_version" => md.version,
            "indices" => Value::Obj(indices),
            "index_templates" => Value::Obj(its),
            "component_templates" => Value::Obj(cts),
            "templates" => Value::Obj(lts),
            "persistent_settings" => md.persistent.to_flat_json(),
            "transient_settings" => md.transient.to_flat_json(),
            "scripts" => Value::Obj(scripts),
            "pipelines" => Value::Obj(pipelines),
            "voting_exclusions" => Value::Arr(md.voting_exclusions.iter().map(|(i, n)| obj! { "node_id" => i.clone(), "node_name" => n.clone() }).collect()),
            "data_streams" => Value::Obj(md.data_streams.iter().map(|(k, d)| (k.clone(), d.to_stored())).collect()),
            "data_stream_aliases" => Value::Obj(md.data_stream_aliases.iter().map(|(k, a)| (k.clone(), a.to_stored())).collect()),
            "synonyms" => Value::Obj(md.synonyms.iter().map(|(k, s)| (k.clone(), s.to_value())).collect()),
            "synonyms_version" => md.synonyms_version,
            "desired_nodes" => md.desired_nodes.as_ref().map(|d| d.to_value(true)).unwrap_or(Value::Null),
            "repositories" => Value::Obj(md.repositories.iter().map(|(k, r)| (k.clone(), r.to_stored())).collect()),
            "repository_data" => Value::Obj(md.repository_data.iter().map(|(k, d)| (k.clone(), d.to_stored())).collect()),
        }
    }

    pub fn from_json(v: &Value) -> Result<ClusterState, EsError> {
        ClusterState::from_json_in(v, crate::cluster::Dialect::Elasticsearch)
    }

    /// [`ClusterState::from_json`] for a cluster speaking `dialect` (mappings parse by its rules).
    pub fn from_json_in(v: &Value, dialect: crate::cluster::Dialect) -> Result<ClusterState, EsError> {
        let mut md = Metadata {
            cluster_uuid: v.get("cluster_uuid").map(Value::to_es_string).unwrap_or_default(),
            version: v.get("metadata_version").and_then(Value::as_i64).unwrap_or(1) as u64,
            dialect,
            ..Default::default()
        };
        if let Some(Value::Obj(m)) = v.get("indices") {
            for (k, iv) in m {
                md.indices.insert(k.clone(), Arc::new(IndexMeta::from_json_in(k, iv, dialect)?));
            }
        }
        if let Some(Value::Obj(m)) = v.get("index_templates") {
            for (k, tv) in m {
                md.index_templates.insert(k.clone(), Arc::new(templates::ComposableTemplate::from_stored(tv)?));
            }
        }
        if let Some(Value::Obj(m)) = v.get("component_templates") {
            for (k, tv) in m {
                md.component_templates.insert(k.clone(), Arc::new(templates::ComponentTemplate::from_stored(tv)?));
            }
        }
        if let Some(Value::Obj(m)) = v.get("templates") {
            for (k, tv) in m {
                md.templates.insert(k.clone(), Arc::new(templates::LegacyTemplate::from_stored(tv)?));
            }
        }
        md.persistent = Settings::from_json(v.get("persistent_settings").unwrap_or(&Value::Null))?;
        md.transient = Settings::from_json(v.get("transient_settings").unwrap_or(&Value::Null))?;
        if let Some(Value::Obj(m)) = v.get("scripts") {
            for (k, sv) in m {
                md.scripts.insert(
                    k.clone(),
                    StoredScript {
                        lang: sv.get("lang").map(Value::to_es_string).unwrap_or_else(|| "painless".into()),
                        source: sv.get("source").cloned().unwrap_or(Value::Null),
                        options: sv.get("options").and_then(Value::as_object).cloned(),
                    },
                );
            }
        }
        if let Some(Value::Arr(a)) = v.get("voting_exclusions") {
            for e in a {
                md.voting_exclusions.push((
                    e.get("node_id").map(Value::to_es_string).unwrap_or_default(),
                    e.get("node_name").map(Value::to_es_string).unwrap_or_default(),
                ));
            }
        }
        if let Some(Value::Obj(m)) = v.get("pipelines") {
            for (k, p) in m {
                md.pipelines.insert(k.clone(), p.clone());
            }
        }
        if let Some(Value::Obj(m)) = v.get("data_streams") {
            for (k, d) in m {
                md.data_streams.insert(k.clone(), Arc::new(data_streams::DataStream::from_stored(k, d)?));
            }
        }
        if let Some(Value::Obj(m)) = v.get("data_stream_aliases") {
            for (k, a) in m {
                md.data_stream_aliases.insert(k.clone(), data_streams::DataStreamAlias::from_stored(a));
            }
        }
        if let Some(Value::Obj(m)) = v.get("synonyms") {
            md.synonyms = Arc::new(m.iter().map(|(k, s)| (k.clone(), Arc::new(synonyms::SynonymSet::from_value(s)))).collect());
        }
        md.synonyms_version = v.get("synonyms_version").and_then(Value::as_i64).unwrap_or(0) as u64;
        md.desired_nodes = v.get("desired_nodes").and_then(desired_nodes::DesiredNodes::from_stored).map(Arc::new);
        if let Some(Value::Obj(m)) = v.get("repositories") {
            md.repositories = m.iter().filter_map(|(k, r)| snapshots::Repository::from_stored(r).map(|r| (k.clone(), Arc::new(r)))).collect();
        }
        if let Some(Value::Obj(m)) = v.get("repository_data") {
            md.repository_data = m.iter().filter_map(|(k, d)| snapshots::RepositoryData::from_stored(d).map(|d| (k.clone(), Arc::new(d)))).collect();
        }
        Ok(ClusterState {
            dialect,
            version: v.get("version").and_then(Value::as_i64).unwrap_or(1) as u64,
            state_uuid: v.get("state_uuid").map(Value::to_es_string).unwrap_or_else(names::random_uuid),
            metadata: md,
        })
    }
}
