//! Snapshot repositories and the snapshots they hold.
//!
//! A repository registration (`PUT _snapshot/{name}`) names a type and settings; what the
//! repository contains lives at its *location* (`fs` `location`, `url` `url`, object store
//! bucket and base path), independently of any registration: unregistering a repository and
//! registering another one at the same location finds the same repository UUID and snapshots,
//! as Elasticsearch's blob store repositories do. roll keeps that content with the cluster's
//! metadata, keyed by the location, so it survives unload and is branched with the cluster.
//!
//! A snapshot records what restoring needs: the metadata of each index (settings, mappings,
//! aliases), the data streams, and with `include_global_state` the templates, pipelines,
//! stored scripts and persistent settings, plus what each shard snapshot holds
//! ([`ShardFiles`]: the files it references and those it added, for `_snapshot/_status` and
//! `index_details`). The shard contents themselves are kept by `api::snapshots`.

use crate::json::{Map, Value};
use crate::meta::settings::Settings;
use crate::obj;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A registered repository.
#[derive(Clone, Debug, PartialEq)]
pub struct Repository {
    pub ty: String,
    pub settings: Settings,
}

impl Repository {
    /// Where the repository's content lives: one key per type and location.
    pub fn location_key(&self) -> String {
        let s = &self.settings;
        let loc = match self.ty.as_str() {
            "fs" => s.get("location").unwrap_or_default().to_string(),
            "url" => s.get("url").unwrap_or_default().to_string(),
            "s3" | "gcs" => format!("{}/{}", s.get("bucket").unwrap_or_default(), s.get("base_path").unwrap_or_default()),
            "azure" => format!("{}/{}", s.get("container").unwrap_or_default(), s.get("base_path").unwrap_or_default()),
            _ => String::new(),
        };
        format!("{}:{loc}", self.ty)
    }

    /// `readonly: true` (only that spelling; `url` repositories are read-only by nature).
    pub fn read_only(&self) -> bool {
        self.ty == "url" || self.settings.get("readonly") == Some("true")
    }

    pub fn to_stored(&self) -> Value {
        obj! { "type" => self.ty.clone(), "settings" => self.settings.to_flat_json() }
    }

    pub fn from_stored(v: &Value) -> Option<Repository> {
        Some(Repository { ty: v.get("type")?.as_str()?.to_string(), settings: Settings::from_json(v.get("settings")?).ok()? })
    }
}

/// One snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub name: String,
    pub uuid: String,
    pub indices: Vec<String>,
    pub data_streams: Vec<String>,
    pub include_global_state: bool,
    pub metadata: Option<Map>,
    pub state: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub total_shards: i64,
    pub successful_shards: i64,
    pub feature_states: Vec<Value>,
    /// Stored index metadata by index name (as `IndexMeta::to_json` writes it).
    pub index_meta: BTreeMap<String, Value>,
    /// Data streams (stored form) by name.
    pub data_stream_meta: BTreeMap<String, Value>,
    /// The global state when `include_global_state`: templates, pipelines, scripts, settings.
    pub global: Option<Value>,
    /// Each shard's files by index name (shard order). Empty for snapshots recorded before
    /// shard files were.
    pub shard_files: BTreeMap<String, Vec<ShardFiles>>,
}

/// One shard snapshot's files (Elasticsearch's `IndexShardSnapshotStatus` counts): every file
/// it references (`total`) and those this snapshot added, the rest being shared with an
/// earlier snapshot of the same shard (`incremental`), plus when it was taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShardFiles {
    pub file_count: u64,
    pub size_in_bytes: u64,
    pub incremental_file_count: u64,
    pub incremental_size_in_bytes: u64,
    /// Segments in the shard (`max_segments_per_shard`).
    pub segments: u32,
    pub start_ms: i64,
    pub time_ms: i64,
}

impl ShardFiles {
    /// What a snapshot recorded before shard files were reports: one small commit point.
    pub const UNRECORDED: ShardFiles =
        ShardFiles { file_count: 1, size_in_bytes: 227, incremental_file_count: 1, incremental_size_in_bytes: 227, segments: 0, start_ms: 0, time_ms: 0 };

    fn to_stored(self) -> Value {
        Value::Arr(
            [self.file_count as i64, self.size_in_bytes as i64, self.incremental_file_count as i64, self.incremental_size_in_bytes as i64, self.segments as i64, self.start_ms, self.time_ms]
                .into_iter()
                .map(Value::from)
                .collect(),
        )
    }

    fn from_stored(v: &Value) -> Option<ShardFiles> {
        let a = v.as_array()?;
        let n = |i: usize| a.get(i).and_then(Value::as_i64);
        Some(ShardFiles {
            file_count: n(0)? as u64,
            size_in_bytes: n(1)? as u64,
            incremental_file_count: n(2)? as u64,
            incremental_size_in_bytes: n(3)? as u64,
            segments: n(4)? as u32,
            start_ms: n(5)?,
            time_ms: n(6)?,
        })
    }
}

impl Snapshot {
    /// The files of index `index`'s `shards` shards, as recorded or (older snapshots) as
    /// [`ShardFiles::UNRECORDED`] taken over the snapshot's whole duration.
    pub fn files(&self, index: &str, shards: usize) -> std::borrow::Cow<'_, [ShardFiles]> {
        match self.shard_files.get(index) {
            Some(f) if f.len() == shards => std::borrow::Cow::Borrowed(f),
            _ => std::borrow::Cow::Owned(vec![ShardFiles { start_ms: self.start_ms, time_ms: (self.end_ms - self.start_ms).max(0), ..ShardFiles::UNRECORDED }; shards]),
        }
    }

    pub fn to_stored(&self) -> Value {
        obj! {
            "name" => self.name.clone(), "uuid" => self.uuid.clone(),
            "indices" => Value::from(self.indices.clone()), "data_streams" => Value::from(self.data_streams.clone()),
            "include_global_state" => self.include_global_state,
            "metadata" => self.metadata.clone().map(Value::Obj).unwrap_or(Value::Null),
            "state" => self.state.clone(), "start" => self.start_ms, "end" => self.end_ms,
            "total_shards" => self.total_shards, "successful_shards" => self.successful_shards,
            "feature_states" => Value::Arr(self.feature_states.clone()),
            "index_meta" => Value::Obj(self.index_meta.clone().into_iter().collect()),
            "data_stream_meta" => Value::Obj(self.data_stream_meta.clone().into_iter().collect()),
            "global" => self.global.clone().unwrap_or(Value::Null),
            "shard_files" => Value::Obj(self.shard_files.iter().map(|(k, v)| (k.clone(), Value::Arr(v.iter().map(|f| f.to_stored()).collect()))).collect()),
        }
    }

    pub fn from_stored(v: &Value) -> Option<Snapshot> {
        let strings = |k: &str| v.get(k).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
        let map = |k: &str| v.get(k).and_then(Value::as_object).map(|m| m.iter().map(|(a, b)| (a.clone(), b.clone())).collect()).unwrap_or_default();
        Some(Snapshot {
            name: v.get("name")?.as_str()?.to_string(),
            uuid: v.get("uuid")?.as_str()?.to_string(),
            indices: strings("indices"),
            data_streams: strings("data_streams"),
            include_global_state: v.get("include_global_state").and_then(Value::as_bool).unwrap_or(true),
            metadata: v.get("metadata").and_then(Value::as_object).cloned(),
            state: v.get("state").and_then(Value::as_str).unwrap_or("SUCCESS").to_string(),
            start_ms: v.get("start").and_then(Value::as_i64).unwrap_or(0),
            end_ms: v.get("end").and_then(Value::as_i64).unwrap_or(0),
            total_shards: v.get("total_shards").and_then(Value::as_i64).unwrap_or(0),
            successful_shards: v.get("successful_shards").and_then(Value::as_i64).unwrap_or(0),
            feature_states: v.get("feature_states").and_then(Value::as_array).cloned().unwrap_or_default(),
            index_meta: map("index_meta"),
            data_stream_meta: map("data_stream_meta"),
            global: v.get("global").filter(|g| !g.is_null()).cloned(),
            shard_files: v
                .get("shard_files")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, a)| (k.clone(), a.as_array().map(|a| a.iter().filter_map(ShardFiles::from_stored).collect()).unwrap_or_default())).collect())
                .unwrap_or_default(),
        })
    }
}

/// The content of one repository location.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepositoryData {
    pub uuid: String,
    /// Bumped on every change (`RepositoryData` generation).
    pub generation: i64,
    pub snapshots: Vec<Arc<Snapshot>>,
}

impl RepositoryData {
    pub fn to_stored(&self) -> Value {
        obj! {
            "uuid" => self.uuid.clone(), "generation" => self.generation,
            "snapshots" => Value::Arr(self.snapshots.iter().map(|s| s.to_stored()).collect()),
        }
    }

    pub fn from_stored(v: &Value) -> Option<RepositoryData> {
        Some(RepositoryData {
            uuid: v.get("uuid")?.as_str()?.to_string(),
            generation: v.get("generation").and_then(Value::as_i64).unwrap_or(0),
            snapshots: v.get("snapshots").and_then(Value::as_array).map(|a| a.iter().filter_map(Snapshot::from_stored).map(Arc::new).collect()).unwrap_or_default(),
        })
    }

    pub fn find(&self, name: &str) -> Option<&Arc<Snapshot>> {
        self.snapshots.iter().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_identify_content() {
        let mut s = Settings::default();
        s.set("location", "loc");
        let a = Repository { ty: "fs".into(), settings: s.clone() };
        s.set("read_only", "true");
        let c = Repository { ty: "fs".into(), settings: s.clone() };
        s.set("readonly", "true");
        let b = Repository { ty: "fs".into(), settings: s };
        assert_eq!(a.location_key(), b.location_key());
        assert!(b.read_only() && !a.read_only() && !c.read_only());
        let r = Repository::from_stored(&a.to_stored()).unwrap();
        assert_eq!(r, a);
    }

    #[test]
    fn snapshot_round_trip() {
        let snap = Snapshot {
            name: "s".into(),
            uuid: "u".into(),
            indices: vec!["i".into()],
            data_streams: vec![],
            include_global_state: true,
            metadata: None,
            state: "SUCCESS".into(),
            start_ms: 1,
            end_ms: 2,
            total_shards: 1,
            successful_shards: 1,
            feature_states: vec![],
            index_meta: BTreeMap::new(),
            data_stream_meta: BTreeMap::new(),
            global: None,
            shard_files: BTreeMap::from([("i".to_string(), vec![ShardFiles { file_count: 3, size_in_bytes: 900, incremental_file_count: 1, incremental_size_in_bytes: 100, segments: 2, start_ms: 1, time_ms: 1 }])]),
        };
        let data = RepositoryData { uuid: "r".into(), generation: 3, snapshots: vec![Arc::new(snap)] };
        assert_eq!(RepositoryData::from_stored(&data.to_stored()).unwrap(), data);
    }
}
