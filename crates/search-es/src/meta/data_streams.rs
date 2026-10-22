//! Data streams: append-only targets backed by hidden indices (`.ds-<name>-<yyyy.MM.dd>-<gen>`),
//! created from composable templates with a `data_stream` section.
//!
//! A data stream owns an ordered list of backing indices; the last one is the write index.
//! Rolling over adds a new backing index with the next generation (the generation also grows
//! when a backing index is removed, so names never repeat). Failure-store indices (`.fs-…`)
//! are tracked the same way. Data stream aliases point at data streams, not indices, and may
//! name one write data stream.
//!
//! Everything a data stream's settings, mappings, lifecycle and options come from is resolved
//! from its matching template at creation and rollover time, as in Elasticsearch
//! (`MetadataCreateDataStreamService`, `MetadataRolloverService`); data-stream-level settings
//! and mappings overrides (`PUT _data_stream/<name>/_settings|_mappings`) apply on top.

use super::settings::Settings;
use super::{ClusterState, IndexMeta, Metadata};
use crate::error::EsError;
use crate::json::{Map, Value};
use crate::obj;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const BACKING_INDEX_PREFIX: &str = ".ds-";
pub const FAILURE_STORE_PREFIX: &str = ".fs-";
pub const TIMESTAMP_FIELD: &str = "@timestamp";

/// One data stream.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DataStream {
    pub name: String,
    /// Backing indices, oldest first; the last is the write index.
    pub indices: Vec<String>,
    pub generation: u64,
    pub meta: Option<Map>,
    pub hidden: bool,
    pub system: bool,
    pub replicated: bool,
    pub allow_custom_routing: bool,
    /// `index.mode` of the template it was created from (`standard`, `time_series`, `logsdb`).
    pub index_mode: Option<String>,
    /// The data lifecycle (`{"enabled": true, "data_retention": "7d", …}`).
    pub lifecycle: Option<Value>,
    /// The next write rolls the data stream over first (lazy rollover).
    pub rollover_on_write: bool,
    pub failure_indices: Vec<String>,
    pub failure_rollover_on_write: bool,
    /// `data_stream_options` (`{"failure_store": {"enabled": …, "lifecycle": …}}`).
    pub options: Option<Value>,
    /// Data-stream-level settings applied to new backing indices on top of the template's.
    pub settings: Settings,
    /// Data-stream-level mappings applied to new backing indices on top of the template's.
    pub mappings: Option<Map>,
}

impl DataStream {
    pub fn write_index(&self) -> Option<&str> {
        self.indices.last().map(String::as_str)
    }

    pub fn failure_write_index(&self) -> Option<&str> {
        self.failure_indices.last().map(String::as_str)
    }

    pub fn contains(&self, index: &str) -> bool {
        self.indices.iter().any(|i| i == index) || self.failure_indices.iter().any(|i| i == index)
    }

    /// Whether the failure store is enabled by the data stream's options.
    pub fn failure_store_enabled(&self) -> Option<bool> {
        self.options.as_ref().and_then(|o| o.get("failure_store")).and_then(|f| f.get("enabled")).and_then(Value::as_bool)
    }

    pub fn to_stored(&self) -> Value {
        let mut m = Map::new();
        m.insert("indices".into(), Value::from(self.indices.clone()));
        m.insert("generation".into(), Value::from(self.generation));
        if let Some(meta) = &self.meta {
            m.insert("_meta".into(), Value::Obj(meta.clone()));
        }
        m.insert("hidden".into(), self.hidden.into());
        m.insert("system".into(), self.system.into());
        m.insert("replicated".into(), self.replicated.into());
        m.insert("allow_custom_routing".into(), self.allow_custom_routing.into());
        if let Some(mode) = &self.index_mode {
            m.insert("index_mode".into(), Value::from(mode.clone()));
        }
        if let Some(l) = &self.lifecycle {
            m.insert("lifecycle".into(), l.clone());
        }
        m.insert("rollover_on_write".into(), self.rollover_on_write.into());
        m.insert("failure_indices".into(), Value::from(self.failure_indices.clone()));
        m.insert("failure_rollover_on_write".into(), self.failure_rollover_on_write.into());
        if let Some(o) = &self.options {
            m.insert("options".into(), o.clone());
        }
        if !self.settings.is_empty() {
            m.insert("settings".into(), self.settings.to_flat_json());
        }
        if let Some(mp) = &self.mappings {
            m.insert("mappings".into(), Value::Obj(mp.clone()));
        }
        Value::Obj(m)
    }

    pub fn from_stored(name: &str, v: &Value) -> Result<DataStream, EsError> {
        let strings = |k: &str| v.get(k).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
        let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        Ok(DataStream {
            name: name.to_string(),
            indices: strings("indices"),
            generation: v.get("generation").and_then(Value::as_i64).unwrap_or(1) as u64,
            meta: v.get("_meta").and_then(Value::as_object).cloned(),
            hidden: flag("hidden"),
            system: flag("system"),
            replicated: flag("replicated"),
            allow_custom_routing: flag("allow_custom_routing"),
            index_mode: v.get("index_mode").and_then(Value::as_str).map(str::to_string),
            lifecycle: v.get("lifecycle").cloned(),
            rollover_on_write: flag("rollover_on_write"),
            failure_indices: strings("failure_indices"),
            failure_rollover_on_write: v.get("failure_rollover_on_write").and_then(Value::as_bool).unwrap_or(true),
            options: v.get("options").cloned(),
            settings: Settings::from_json(v.get("settings").unwrap_or(&Value::Null))?,
            mappings: v.get("mappings").and_then(Value::as_object).cloned(),
        })
    }
}

/// An alias of data streams.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DataStreamAlias {
    /// The data streams it points at, sorted.
    pub data_streams: Vec<String>,
    pub write_data_stream: Option<String>,
    /// Filters per data stream.
    pub filters: BTreeMap<String, Value>,
}

impl DataStreamAlias {
    pub fn to_stored(&self) -> Value {
        let mut m = Map::new();
        m.insert("data_streams".into(), Value::from(self.data_streams.clone()));
        if let Some(w) = &self.write_data_stream {
            m.insert("write_data_stream".into(), Value::from(w.clone()));
        }
        if !self.filters.is_empty() {
            m.insert("filters".into(), Value::Obj(self.filters.iter().map(|(k, v)| (k.clone(), v.clone())).collect()));
        }
        Value::Obj(m)
    }

    pub fn from_stored(v: &Value) -> DataStreamAlias {
        DataStreamAlias {
            data_streams: v.get("data_streams").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default(),
            write_data_stream: v.get("write_data_stream").and_then(Value::as_str).map(str::to_string),
            filters: v.get("filters").and_then(Value::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default(),
        }
    }
}

/// `MetadataCreateDataStreamService.validateDataStreamName` (after the existence checks).
pub fn validate_name(name: &str) -> Result<(), EsError> {
    if let Some(p) = super::names::name_problem(name) {
        return Err(EsError::illegal_argument(format!("data_stream [{name}] {p}")));
    }
    if name.to_lowercase() != name {
        return Err(EsError::illegal_argument(format!("data_stream [{name}] must be lowercase")));
    }
    if name.starts_with(BACKING_INDEX_PREFIX) {
        return Err(EsError::illegal_argument(format!("data_stream [{name}] must not start with '{BACKING_INDEX_PREFIX}'")));
    }
    if name.starts_with(FAILURE_STORE_PREFIX) {
        return Err(EsError::illegal_argument(format!("data_stream [{name}] must not start with '{FAILURE_STORE_PREFIX}'")));
    }
    Ok(())
}

/// `DataStream.getDefaultBackingIndexName`: `.ds-<name>-<uuuu.MM.dd>-<generation:06>`.
pub fn backing_index_name(prefix: &str, data_stream: &str, generation: u64, now_ms: i64) -> String {
    let date = crate::time::DateFormatter::new("uuuu.MM.dd").expect("valid pattern").format_millis(now_ms);
    format!("{prefix}{data_stream}-{date}-{generation:06}")
}

impl Metadata {
    /// The data stream an index backs (or is a failure-store index of).
    pub fn data_stream_of(&self, index: &str) -> Option<&Arc<DataStream>> {
        self.data_streams.values().find(|d| d.contains(index))
    }

    /// `DataStreamAlias.update`: adds `data_stream` to `alias` (creating it), with its write
    /// flag and filter.
    pub fn add_data_stream_alias(&mut self, alias: &str, data_stream: &str, is_write: Option<bool>, filter: Option<Value>) {
        let a = self.data_stream_aliases.entry(alias.to_string()).or_default();
        if !a.data_streams.iter().any(|d| d == data_stream) {
            a.data_streams.push(data_stream.to_string());
            a.data_streams.sort();
        }
        match is_write {
            Some(true) => a.write_data_stream = Some(data_stream.to_string()),
            Some(false) if a.write_data_stream.as_deref() == Some(data_stream) => a.write_data_stream = None,
            _ => {}
        }
        match filter {
            Some(f) => {
                a.filters.insert(data_stream.to_string(), f);
            }
            None => {
                a.filters.remove(data_stream);
            }
        }
    }

    /// Removes `data_stream` from `alias` (dropping the alias when it points at nothing).
    pub fn remove_data_stream_alias(&mut self, alias: &str, data_stream: &str) -> bool {
        let Some(a) = self.data_stream_aliases.get_mut(alias) else { return false };
        let before = a.data_streams.len();
        a.data_streams.retain(|d| d != data_stream);
        a.filters.remove(data_stream);
        if a.write_data_stream.as_deref() == Some(data_stream) {
            a.write_data_stream = None;
        }
        let removed = a.data_streams.len() != before;
        if a.data_streams.is_empty() {
            self.data_stream_aliases.remove(alias);
        }
        removed
    }

    /// Data stream aliases naming `data_stream`.
    pub fn data_stream_aliases_of(&self, data_stream: &str) -> Vec<(&String, &DataStreamAlias)> {
        self.data_stream_aliases.iter().filter(|(_, a)| a.data_streams.iter().any(|d| d == data_stream)).collect()
    }
}

/// The health of a data stream (`status` in `GET _data_stream`): the worst of its indices'.
pub fn status(st: &ClusterState, ds: &DataStream) -> &'static str {
    let mut worst = "GREEN";
    for name in ds.indices.iter().chain(&ds.failure_indices) {
        let Some(idx) = st.metadata.indices.get(name) else { continue };
        let s = index_status(idx);
        if s == "RED" {
            return "RED";
        }
        if s == "YELLOW" {
            worst = "YELLOW";
        }
    }
    worst
}

fn index_status(idx: &IndexMeta) -> &'static str {
    if idx.is_closed() {
        return "GREEN";
    }
    if idx.number_of_replicas() > 0 { "YELLOW" } else { "GREEN" }
}

/// The JSON of `GET _data_stream` for one data stream.
pub struct DataStreamView<'a> {
    pub ds: &'a DataStream,
    pub template: Option<String>,
    /// `index.lifecycle.name` of the template, if any.
    pub ilm_policy: Option<String>,
    /// `index.lifecycle.prefer_ilm` of the template (default `true`).
    pub prefer_ilm: bool,
    pub failure_store_enabled: bool,
    pub verbose: bool,
    /// The index mode the data stream's next backing index would get (its current template's).
    pub index_mode: Option<String>,
}

impl DataStreamView<'_> {
    /// Whether the failure store's lifecycle is enabled (it is unless its options disable it).
    fn failures_lifecycle_enabled(&self) -> bool {
        let configured = self.ds.options.as_ref().and_then(|o| o.get("failure_store")).and_then(|f| f.get("lifecycle"));
        configured.and_then(|l| l.get("enabled")).and_then(Value::as_bool).unwrap_or(true)
    }

    fn managed_by(&self, idx: Option<&IndexMeta>, failure: bool) -> &'static str {
        let policy = idx.and_then(|i| i.settings.get("index.lifecycle.name")).filter(|p| !p.is_empty());
        let prefer_ilm = idx.and_then(|i| i.settings.get("index.lifecycle.prefer_ilm")).map(|v| v != "false").unwrap_or(true);
        let dsl = if failure {
            self.failures_lifecycle_enabled()
        } else {
            self.ds.lifecycle.as_ref().is_some_and(|l| l.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        };
        match (policy.is_some(), dsl) {
            (true, true) if prefer_ilm => "Index Lifecycle Management",
            (true, false) => "Index Lifecycle Management",
            (_, true) => "Data stream lifecycle",
            _ => "Unmanaged",
        }
    }

    fn next_generation_managed_by(&self) -> &'static str {
        let dsl = self.ds.lifecycle.as_ref().is_some_and(|l| l.get("enabled").and_then(Value::as_bool).unwrap_or(true));
        match (self.ilm_policy.is_some(), dsl) {
            (true, true) if self.prefer_ilm => "Index Lifecycle Management",
            (true, false) => "Index Lifecycle Management",
            (_, true) => "Data stream lifecycle",
            _ => "Unmanaged",
        }
    }

    fn indices_value(&self, st: &ClusterState, names: &[String], failure: bool) -> Value {
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let idx = st.metadata.indices.get(n).map(|i| i.as_ref());
            let mut m = Map::new();
            m.insert("index_name".into(), Value::from(n.clone()));
            m.insert("index_uuid".into(), Value::from(idx.map(|i| i.uuid.clone()).unwrap_or_default()));
            if let Some(i) = idx {
                m.insert("managed_by".into(), Value::from(self.managed_by(Some(i), failure)));
                let policy = i.settings.get("index.lifecycle.name").filter(|p| !p.is_empty());
                let prefer = i.settings.get("index.lifecycle.prefer_ilm").map(|v| v != "false").unwrap_or(true);
                if failure {
                    if let Some(p) = policy {
                        m.insert("prefer_ilm".into(), prefer.into());
                        m.insert("ilm_policy".into(), Value::from(p));
                    }
                } else {
                    m.insert("prefer_ilm".into(), prefer.into());
                    if let Some(p) = policy {
                        m.insert("ilm_policy".into(), Value::from(p));
                    }
                    m.insert("index_mode".into(), Value::from(i.settings.get("index.mode").unwrap_or("standard")));
                }
            }
            out.push(Value::Obj(m));
        }
        Value::Arr(out)
    }

    pub fn to_value(&self, st: &ClusterState) -> Value {
        let ds = self.ds;
        let mut m = Map::new();
        m.insert("name".into(), Value::from(ds.name.clone()));
        m.insert("timestamp_field".into(), obj! { "name" => TIMESTAMP_FIELD });
        m.insert("indices".into(), self.indices_value(st, &ds.indices, false));
        m.insert("generation".into(), Value::from(ds.generation));
        if let Some(meta) = &ds.meta {
            m.insert("_meta".into(), Value::Obj(meta.clone()));
        }
        m.insert("status".into(), Value::from(status(st, ds)));
        if let Some(t) = &self.template {
            m.insert("template".into(), Value::from(t.clone()));
        }
        if let Some(l) = &ds.lifecycle {
            m.insert("lifecycle".into(), lifecycle_value(l));
        }
        if let Some(p) = &self.ilm_policy {
            m.insert("ilm_policy".into(), Value::from(p.clone()));
        }
        m.insert("next_generation_managed_by".into(), Value::from(self.next_generation_managed_by()));
        m.insert("prefer_ilm".into(), self.prefer_ilm.into());
        m.insert("hidden".into(), ds.hidden.into());
        m.insert("system".into(), ds.system.into());
        m.insert("allow_custom_routing".into(), ds.allow_custom_routing.into());
        m.insert("replicated".into(), ds.replicated.into());
        m.insert("rollover_on_write".into(), ds.rollover_on_write.into());
        if self.verbose {
            m.insert("maximum_timestamp".into(), Value::from(0));
        }
        let mode = self.index_mode.clone().or_else(|| ds.index_mode.clone());
        m.insert("index_mode".into(), Value::from(mode.map(|m| m.to_ascii_lowercase()).unwrap_or_else(|| "standard".into())));
        if ds.index_mode.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("time_series")) {
            m.insert("time_series".into(), obj! { "temporal_ranges" => temporal_ranges(st, ds) });
        }
        m.insert("settings".into(), ds.settings.to_nested_json());
        m.insert("mappings".into(), Value::Obj(ds.mappings.clone().unwrap_or_default()));
        let mut fs = Map::new();
        fs.insert("enabled".into(), self.failure_store_enabled.into());
        fs.insert("rollover_on_write".into(), ds.failure_rollover_on_write.into());
        fs.insert("indices".into(), self.indices_value(st, &ds.failure_indices, true));
        if self.failure_store_enabled {
            let lifecycle = ds
                .options
                .as_ref()
                .and_then(|o| o.get("failure_store"))
                .and_then(|f| f.get("lifecycle"))
                .cloned()
                .unwrap_or_else(|| obj! { "enabled" => true });
            fs.insert("lifecycle".into(), lifecycle_value(&lifecycle));
        }
        m.insert("failure_store".into(), Value::Obj(fs));
        Value::Obj(m)
    }
}

/// The time ranges a time series data stream's backing indices cover, adjacent ranges merged.
fn temporal_ranges(st: &ClusterState, ds: &DataStream) -> Value {
    let mut ranges: Vec<(String, String)> = Vec::new();
    for n in &ds.indices {
        let Some(idx) = st.metadata.indices.get(n) else { continue };
        let (Some(s), Some(e)) = (idx.settings.get("index.time_series.start_time"), idx.settings.get("index.time_series.end_time")) else { continue };
        match ranges.last_mut() {
            Some(last) if last.1 == s => last.1 = e.to_string(),
            _ => ranges.push((s.to_string(), e.to_string())),
        }
    }
    Value::Arr(ranges.into_iter().map(|(s, e)| obj! { "start" => s, "end" => e }).collect())
}

/// A lifecycle as `DataStreamLifecycle.toXContent` prints it: `enabled`, then `data_retention`,
/// `downsampling`, `downsampling_method`, `frozen_after`.
pub fn lifecycle_value(l: &Value) -> Value {
    let mut m = Map::new();
    m.insert("enabled".into(), l.get("enabled").and_then(Value::as_bool).unwrap_or(true).into());
    for k in ["data_retention", "downsampling", "downsampling_method", "frozen_after"] {
        if let Some(v) = l.get(k).filter(|v| !v.is_null()) {
            m.insert(k.into(), v.clone());
        }
    }
    Value::Obj(m)
}

/// Composes template lifecycles (later ones override fields of earlier ones, `null` resets).
pub fn compose_lifecycle(parts: &[&Value]) -> Option<Value> {
    let mut out: Option<Map> = None;
    for p in parts {
        let Some(pm) = p.as_object() else {
            if p.is_null() {
                out = None;
            }
            continue;
        };
        let acc = out.get_or_insert_with(Map::new);
        for (k, v) in pm {
            if v.is_null() {
                acc.shift_remove(k);
            } else {
                acc.insert(k.clone(), v.clone());
            }
        }
    }
    out.map(Value::Obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_elasticsearch() {
        assert_eq!(backing_index_name(BACKING_INDEX_PREFIX, "logs", 1, 1_792_525_469_350), ".ds-logs-2026.10.20-000001");
        assert_eq!(backing_index_name(FAILURE_STORE_PREFIX, "logs", 1_000_000, 0), ".fs-logs-1970.01.01-1000000");
        assert_eq!(validate_name("Logs").unwrap_err().reason(), "data_stream [Logs] must be lowercase");
        assert_eq!(validate_name(".ds-x").unwrap_err().reason(), "data_stream [.ds-x] must not start with '.ds-'");
        assert_eq!(validate_name("_x").unwrap_err().reason(), "data_stream [_x] must not start with '_', '-', or '+'");
        assert!(validate_name("logs-app-default").is_ok());
    }

    #[test]
    fn stored_round_trip() {
        let ds = DataStream {
            name: "logs".into(),
            indices: vec![".ds-logs-2026.10.20-000001".into()],
            generation: 3,
            hidden: true,
            lifecycle: Some(obj! { "enabled" => true, "data_retention" => "7d" }),
            failure_rollover_on_write: true,
            ..Default::default()
        };
        assert_eq!(DataStream::from_stored("logs", &ds.to_stored()).unwrap(), ds);
    }

    #[test]
    fn lifecycle_composition() {
        let a = obj! { "data_retention" => "7d" };
        let b = obj! { "enabled" => true };
        let c = obj! { "data_retention" => Value::Null };
        assert_eq!(compose_lifecycle(&[&a, &b]).unwrap().to_json(), r#"{"data_retention":"7d","enabled":true}"#);
        assert_eq!(compose_lifecycle(&[&a, &c]).unwrap().to_json(), "{}");
    }
}
