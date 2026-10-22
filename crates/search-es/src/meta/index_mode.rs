//! Index modes (`index.mode`): `standard`, `time_series` (TSDB), `logsdb`, `lookup`, `columnar`,
//! `logsdb_columnar` and `vectordb_document`, with what Elasticsearch's `IndexMode` decides at
//! the metadata level:
//!
//! - which settings combinations are allowed (`validateWithOtherSettings`): TSDB needs
//!   `index.routing_path` and rejects index sorting and routing partitions, only TSDB may set a
//!   routing path, `lookup` indices have one shard, TSDB time bounds must be ordered;
//! - the mapping every index of the mode starts from (`@timestamp` and `_data_stream_timestamp`
//!   for TSDB and logs modes, optionally `host.name` or the temporality dimension);
//! - mapping rules (`validateMapping` and friends): no required `_routing` in TSDB, `@timestamp`
//!   must be a date, routing path fields must be dimensions, columnar modes have no runtime fields
//!   and only fields rebuildable from doc values, `_source` can't be disabled where it is
//!   synthesized;
//! - mode-dependent setting defaults (`ignore_malformed` and `ignore_above` for logs, the codec,
//!   the `_source` mode, …) as `GET _settings?include_defaults` prints them.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::mapping::{Mapping, MapperRef};
use crate::meta::settings::Settings;
use crate::obj;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMode {
    Standard,
    TimeSeries,
    Logsdb,
    Lookup,
    Columnar,
    LogsdbColumnar,
    VectordbDocument,
}

const TSDB: &str = "[index.mode=time_series]";

impl IndexMode {
    /// The mode of an index's settings (`index.mode`, case-insensitive; `standard` when unset).
    pub fn of(settings: &Settings) -> IndexMode {
        IndexMode::parse(settings.get("index.mode").unwrap_or("standard")).unwrap_or(IndexMode::Standard)
    }

    pub fn parse(s: &str) -> Option<IndexMode> {
        Some(match s.to_ascii_lowercase().as_str() {
            "standard" => IndexMode::Standard,
            "time_series" => IndexMode::TimeSeries,
            "logsdb" => IndexMode::Logsdb,
            "lookup" => IndexMode::Lookup,
            "columnar" => IndexMode::Columnar,
            "logsdb_columnar" => IndexMode::LogsdbColumnar,
            "vectordb_document" => IndexMode::VectordbDocument,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            IndexMode::Standard => "standard",
            IndexMode::TimeSeries => "time_series",
            IndexMode::Logsdb => "logsdb",
            IndexMode::Lookup => "lookup",
            IndexMode::Columnar => "columnar",
            IndexMode::LogsdbColumnar => "logsdb_columnar",
            IndexMode::VectordbDocument => "vectordb_document",
        }
    }

    pub fn is_logs(self) -> bool {
        matches!(self, IndexMode::Logsdb | IndexMode::LogsdbColumnar)
    }

    /// Modes that keep every field in columns and rebuild `_source` from them.
    pub fn is_strict_columnar(self) -> bool {
        matches!(self, IndexMode::Columnar | IndexMode::LogsdbColumnar)
    }

    /// The `_source` mode used when neither the setting nor the mapping chooses one.
    pub fn default_source_mode(self) -> &'static str {
        match self {
            IndexMode::Standard | IndexMode::Lookup | IndexMode::VectordbDocument => "STORED",
            _ => "SYNTHETIC",
        }
    }

    fn supported_source_modes(self) -> &'static [&'static str] {
        if self.is_strict_columnar() { &["SYNTHETIC", "COLUMNAR_STORED"] } else { &["DISABLED", "STORED", "SYNTHETIC"] }
    }

    /// `validateWithOtherSettings` and the related setting validators.
    pub fn validate_settings(self, settings: &Settings) -> EsResult<()> {
        let routing_path = settings.get_list("index.routing_path").unwrap_or_default();
        if let Some(mode) = settings.get("index.mapping.source.mode") {
            let up = mode.to_ascii_uppercase();
            if !self.supported_source_modes().contains(&up.as_str()) {
                return Err(EsError::illegal_argument(format!(
                    "unsupported source mode [{up}] for index mode [{}]; supported values: [{}]",
                    self.name(),
                    self.supported_source_modes().join(", ")
                )));
            }
        }
        match self {
            IndexMode::TimeSeries => {
                if settings.get("index.routing_partition_size").is_some_and(|p| p != "1") {
                    return Err(EsError::illegal_argument(format!("{TSDB} is incompatible with [index.routing_partition_size]")));
                }
                for k in ["index.sort.field", "index.sort.order", "index.sort.mode", "index.sort.missing"] {
                    if settings.contains(k) {
                        return Err(EsError::illegal_argument(format!("{TSDB} is incompatible with [{k}]")));
                    }
                }
                if settings.get("index.slice.enabled") == Some("true") {
                    return Err(EsError::illegal_argument("The setting [index.slice.enabled] cannot be used with [index.mode=time_series]."));
                }
                let dimensions = settings.get_list("index.dimensions").unwrap_or_default();
                if routing_path.is_empty() && dimensions.is_empty() {
                    return Err(EsError::illegal_argument(format!("{TSDB} requires a non-empty [index.routing_path]")));
                }
                let start = settings.get("index.time_series.start_time").map(parse_bound).transpose()?;
                let end = settings.get("index.time_series.end_time").map(parse_bound).transpose()?;
                if let (Some(s), Some(e)) = (start, end)
                    && e <= s
                {
                    return Err(EsError::illegal_argument("index.time_series.end_time must be larger than index.time_series.start_time"));
                }
            }
            IndexMode::Logsdb | IndexMode::LogsdbColumnar => {
                if settings.get("index.logsdb.route_on_sort_fields") != Some("true") && !routing_path.is_empty() {
                    return Err(EsError::illegal_argument(format!("[index.routing_path] requires {TSDB}")));
                }
            }
            IndexMode::Lookup => {
                if let Some(n) = settings.get("index.number_of_shards").filter(|n| *n != "1") {
                    return Err(EsError::illegal_argument(format!(
                        "index with [lookup] mode must have [index.number_of_shards] set to 1 or unset; provided {n}"
                    )));
                }
            }
            IndexMode::VectordbDocument => {
                if settings.get("index.mapping.exclude_source_vectors") == Some("false") {
                    return Err(EsError::illegal_argument(
                        "[index.mapping.exclude_source_vectors] cannot be set to [false] when [index.mode=vectordb_document]",
                    ));
                }
            }
            IndexMode::Standard | IndexMode::Columnar => {
                if !routing_path.is_empty() {
                    return Err(EsError::illegal_argument(format!("[index.routing_path] requires {TSDB}")));
                }
            }
        }
        Ok(())
    }

    /// Settings the mode's settings provider adds to a new index (explicit in its settings).
    pub fn provided_settings(self, settings: &mut Settings) {
        if self == IndexMode::VectordbDocument {
            for (k, v) in [
                ("index.mapping.exclude_source_vectors", "true"),
                ("index.merge.scheduler.auto_throttle", "false"),
                ("index.merge.intra_merge_parallelism_enabled", "true"),
            ] {
                if !settings.contains(k) {
                    settings.set(k, v);
                }
            }
            if !settings.contains("index.store.preload") {
                settings.insert("index.store.preload".to_string(), crate::meta::settings::SettingValue::List(vec!["vex".into(), "veq".into(), "veb".into(), "cenivf".into()]));
            }
        }
    }

    /// The mapping indices of this mode start from (lowest precedence), if any.
    pub fn default_mapping(self, settings: &Settings) -> Option<Map> {
        let timestamp = |extra: Option<(&str, Value)>| {
            let mut props = Map::new();
            // The timestamp field never ignores malformed values, whatever the index default.
            let ts = if self.is_logs() { obj! { "type" => "date", "ignore_malformed" => false } } else { obj! { "type" => "date" } };
            props.insert("@timestamp".into(), ts);
            if let Some((name, v)) = extra {
                props.insert(name.into(), v);
            }
            let mut m = Map::new();
            m.insert("_data_stream_timestamp".into(), obj! { "enabled" => true });
            m.insert("properties".into(), Value::Obj(props));
            m
        };
        match self {
            IndexMode::TimeSeries => {
                let temporality = settings.get("index.time_series.temporality_field").filter(|f| !f.is_empty());
                Some(timestamp(temporality.map(|f| (f, obj! { "type" => "keyword", "time_series_dimension" => true }))))
            }
            IndexMode::Logsdb | IndexMode::LogsdbColumnar => {
                let host = settings.get("index.logsdb.add_host_name_field") == Some("true");
                Some(timestamp(host.then(|| ("host.name", obj! { "type" => "keyword", "ignore_above" => 1024 }))))
            }
            _ => None,
        }
    }

    /// `_source` checks made while the mapping is parsed (reported as mapping parse failures):
    /// modes that synthesize `_source` can't disable it, and a synthetic `_source` (by mode or
    /// `index.mapping.source.mode`) can't be filtered.
    pub fn validate_source(self, mapping: &Mapping, settings: &Settings) -> EsResult<()> {
        let parse_failure = |reason: String| {
            let e = EsError::illegal_argument(reason);
            EsError::mapper_parsing(format!("Failed to parse mapping: {}", e.reason())).caused_by(e)
        };
        let source_mode = settings.get("index.mapping.source.mode").map(str::to_ascii_uppercase).unwrap_or_else(|| self.default_source_mode().to_string());
        let synthesized = matches!(self, IndexMode::TimeSeries | IndexMode::Logsdb | IndexMode::Columnar | IndexMode::LogsdbColumnar);
        if synthesized && (mapping.source.enabled == Some(false) || source_mode == "DISABLED") {
            return Err(parse_failure(format!("_source can not be disabled in index using [{}] index mode", self.name())));
        }
        let filtered = mapping.source.includes.as_ref().is_some_and(|l| !l.is_empty()) || mapping.source.excludes.as_ref().is_some_and(|l| !l.is_empty());
        if filtered && source_mode == "SYNTHETIC" {
            return Err(parse_failure("filtering the stored _source is incompatible with synthetic source".into()));
        }
        Ok(())
    }

    /// The mode's mapping rules, checked on creation and every mapping change.
    pub fn validate_mapping(self, mapping: &Mapping, settings: &Settings, data_stream: bool) -> EsResult<()> {
        if self == IndexMode::TimeSeries && mapping.routing_required == Some(true) {
            return Err(EsError::illegal_argument(format!("routing is forbidden on CRUD operations that target indices in {TSDB}")));
        }
        if self == IndexMode::TimeSeries {
            validate_temporality_field(mapping, settings)?;
        }
        if (self == IndexMode::TimeSeries || data_stream) && mapping.data_stream_timestamp == Some(false) {
            return Err(EsError::java(500, "illegal_state_exception", "[_data_stream_timestamp] meta field has been disabled"));
        }
        if self.is_strict_columnar() {
            if mapping.root.enabled == Some(false) {
                return Err(EsError::mapper_parsing(
                    "[enabled] cannot be set to [false] on the root object in strict columnar index modes; this would prevent all fields from being indexed",
                ));
            }
            if !mapping.runtime.is_empty() {
                return Err(EsError::illegal_argument(format!("mapping-level runtime fields are not allowed in index using [{}] index mode", self.name())));
            }
            if let Some(field) = first_field_without_doc_values(mapping) {
                return Err(EsError::illegal_argument(format!(
                    "field [{field}] cannot reconstruct _source from doc values; every field must be reconstructable from doc values in index using [{}] index mode",
                    self.name()
                )));
            }
        }
        // `validateTimestampFieldMapping`: TSDB always, the others only for data streams.
        if self == IndexMode::TimeSeries || (data_stream && self != IndexMode::Lookup) {
            validate_timestamp_field(mapping)?;
        }
        validate_routing_path(self, mapping, settings)?;
        Ok(())
    }

    /// Alias routing is forbidden in TSDB indices (checked by the alias APIs; aliases given at
    /// index creation aren't).
    pub fn validate_alias(self, index_routing: Option<&str>, search_routing: Option<&str>) -> EsResult<()> {
        if self == IndexMode::TimeSeries && (index_routing.is_some() || search_routing.is_some()) {
            return Err(EsError::illegal_argument(format!("routing is forbidden on CRUD operations that target indices in {TSDB}")));
        }
        Ok(())
    }

    /// Defaults of mode-dependent index settings (`include_defaults`).
    pub fn setting_default(self, key: &str) -> Option<&'static str> {
        let logs = self.is_logs();
        let columnar = matches!(self, IndexMode::TimeSeries | IndexMode::Logsdb | IndexMode::Columnar | IndexMode::LogsdbColumnar);
        Some(match key {
            "index.codec" => {
                if matches!(self, IndexMode::Logsdb | IndexMode::Columnar | IndexMode::LogsdbColumnar) {
                    "best_compression"
                } else {
                    "default"
                }
            }
            "index.mapping.ignore_above" => {
                if logs {
                    "8191"
                } else {
                    "2147483647"
                }
            }
            "index.mapping.ignore_malformed" => {
                if logs {
                    "true"
                } else {
                    "false"
                }
            }
            "index.mapping.total_fields.ignore_dynamic_beyond_limit" => {
                if logs {
                    "true"
                } else {
                    "false"
                }
            }
            "index.mapping.synthetic_source_keep" => {
                if logs {
                    "arrays"
                } else {
                    "none"
                }
            }
            "index.mapping.synthetic_id" => {
                if self == IndexMode::TimeSeries {
                    "true"
                } else {
                    "false"
                }
            }
            "index.mapping.source.mode" => self.default_source_mode(),
            "index.mapping.use_doc_values_skipper" => {
                if columnar {
                    "true"
                } else {
                    "false"
                }
            }
            "index.mapping.use_columnar_id_mode_by_default" => {
                if self.is_strict_columnar() {
                    "true"
                } else {
                    "false"
                }
            }
            "index.mapping.index_disabled_by_default" => {
                if self.is_strict_columnar() {
                    "true"
                } else {
                    "false"
                }
            }
            "index.recovery.use_synthetic_source" => {
                if self.is_strict_columnar() {
                    "true"
                } else {
                    "false"
                }
            }
            "index.time_series.es95_codec.enabled" => {
                if self == IndexMode::TimeSeries {
                    "true"
                } else {
                    "false"
                }
            }
            "index.use_time_series_doc_values_format" => {
                if columnar {
                    "true"
                } else {
                    "false"
                }
            }
            "index.use_time_series_doc_values_format_large_block_size" => {
                if self == IndexMode::TimeSeries {
                    "true"
                } else {
                    "false"
                }
            }
            _ => return None,
        })
    }
}

/// `index.time_series.start_time`/`end_time` as epoch milliseconds.
fn parse_bound(v: &str) -> EsResult<i64> {
    crate::meta::index_settings::parse_date_setting(v)
}

/// `MetadataCreateDataStreamService.validateTimestampFieldMapping`.
pub fn validate_timestamp_field(mapping: &Mapping) -> EsResult<()> {
    match mapping.lookup("@timestamp") {
        None => Err(EsError::illegal_argument("data stream timestamp field [@timestamp] does not exist")),
        Some(MapperRef::Field(f)) if matches!(f.ty, "date" | "date_nanos") => Ok(()),
        Some(m) => Err(EsError::illegal_argument(format!(
            "data stream timestamp field [@timestamp] is of type [{}], but [date,date_nanos] is expected",
            m.type_name()
        ))),
    }
}

fn validate_temporality_field(mapping: &Mapping, settings: &Settings) -> EsResult<()> {
    let Some(name) = settings.get("index.time_series.temporality_field").filter(|f| !f.is_empty()) else { return Ok(()) };
    let Some(m) = mapping.lookup(name) else { return Ok(()) };
    if m.type_name() != "keyword" {
        return Err(EsError::illegal_argument(format!(
            "[index.time_series.temporality_field] field [{name}] must be of type [keyword] but is [{}]",
            m.type_name()
        )));
    }
    let ctx = crate::mapping::MappingCtx::from_settings(settings);
    if let MapperRef::Field(f) = m
        && !f.bool_param("time_series_dimension", &ctx)
    {
        return Err(EsError::illegal_argument(format!("[index.time_series.temporality_field] field [{name}] must be a [time_series_dimension]")));
    }
    Ok(())
}

/// Routing path entries must not name objects, and in TSDB every field they match must be a
/// dimension (or a flattened field listing its dimensions).
fn validate_routing_path(mode: IndexMode, mapping: &Mapping, settings: &Settings) -> EsResult<()> {
    let paths = settings.get_list("index.routing_path").unwrap_or_default();
    if paths.is_empty() {
        return Ok(());
    }
    let ctx = crate::mapping::MappingCtx::from_settings(settings);
    let fields = mapping.field_paths();
    for path in &paths {
        if mode == IndexMode::TimeSeries {
            for (name, m) in &fields {
                if !crate::xcontent::filter::glob_match(path, name) {
                    continue;
                }
                let MapperRef::Field(f) = m else { continue };
                let dimension = f.bool_param("time_series_dimension", &ctx)
                    || (f.ty == "flattened" && f.raw("time_series_dimensions").and_then(Value::as_array).is_some_and(|d| !d.is_empty()));
                if !dimension || f.is_set("script") {
                    return Err(EsError::illegal_argument(format!(
                        "All fields that match routing_path must be configured with [time_series_dimension: true] or flattened fields with a list of dimensions in [time_series_dimensions] and without the [script] parameter. [{name}] was not a dimension."
                    )));
                }
            }
        }
        if let Some(MapperRef::Object(o)) = mapping.lookup(path) {
            let ty = if o.is_nested() { "nested" } else { "object" };
            return Err(EsError::illegal_argument(format!("All fields that match routing_path must be flattened fields. [{path}] was [{ty}].")));
        }
    }
    Ok(())
}

/// The first field (in path order) whose `_source` can't be rebuilt from doc values.
fn first_field_without_doc_values(mapping: &Mapping) -> Option<String> {
    let ctx = crate::mapping::MappingCtx::default();
    for (path, m) in mapping.field_paths() {
        let MapperRef::Field(f) = m else { continue };
        // Multi-fields aren't part of `_source`.
        if path.rsplit_once('.').is_some_and(|(parent, _)| matches!(mapping.lookup(parent), Some(MapperRef::Field(_)))) {
            continue;
        }
        // Text fields keep a doc values copy of their value in columnar modes.
        if matches!(f.ty, "text" | "match_only_text" | "pattern_text") {
            continue;
        }
        let has_dv_param = f.def().param_index("doc_values").is_some();
        let without = match f.ty {
            "completion" | "search_as_you_type" | "percolator" | "token_count" | "rank_feature" | "rank_features" => true,
            // A join field's doc values are per relation (`field#parent`).
            "join" => {
                let parent = f.raw("relations").and_then(Value::as_object).and_then(|r| r.keys().next().cloned()).unwrap_or_default();
                return Some(format!("{path}#{parent}"));
            }
            _ => has_dv_param && !f.bool_param("doc_values", &ctx),
        };
        if without {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(pairs: &[(&str, &str)]) -> Settings {
        let mut s = Settings::new();
        for (k, v) in pairs {
            s.set(*k, *v);
        }
        s
    }

    #[test]
    fn settings_rules() {
        let e = IndexMode::TimeSeries.validate_settings(&st(&[("index.mode", "time_series")])).unwrap_err();
        assert_eq!(e.reason(), "[index.mode=time_series] requires a non-empty [index.routing_path]");
        let e = IndexMode::Standard.validate_settings(&st(&[("index.routing_path", "host")])).unwrap_err();
        assert_eq!(e.reason(), "[index.routing_path] requires [index.mode=time_series]");
        let e = IndexMode::TimeSeries
            .validate_settings(&st(&[("index.routing_path", "h"), ("index.sort.field", "h")]))
            .unwrap_err();
        assert_eq!(e.reason(), "[index.mode=time_series] is incompatible with [index.sort.field]");
        let e = IndexMode::Standard.validate_settings(&st(&[("index.mapping.source.mode", "COLUMNAR_STORED")])).unwrap_err();
        assert_eq!(e.reason(), "unsupported source mode [COLUMNAR_STORED] for index mode [standard]; supported values: [DISABLED, STORED, SYNTHETIC]");
        IndexMode::Lookup.validate_settings(&st(&[("index.number_of_shards", "1")])).unwrap();
        assert!(IndexMode::Lookup.validate_settings(&st(&[("index.number_of_shards", "2")])).is_err());
    }

    #[test]
    fn default_mappings() {
        assert!(IndexMode::Standard.default_mapping(&Settings::new()).is_none());
        let m = IndexMode::Logsdb.default_mapping(&st(&[("index.logsdb.add_host_name_field", "true")])).unwrap();
        assert_eq!(
            Value::Obj(m).to_json(),
            r#"{"_data_stream_timestamp":{"enabled":true},"properties":{"@timestamp":{"type":"date","ignore_malformed":false},"host.name":{"type":"keyword","ignore_above":1024}}}"#
        );
    }
}
