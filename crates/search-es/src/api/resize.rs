//! `_shrink`, `_split` and `_clone`: a new index created from an existing, write-blocked one
//! with a different number of shards (`TransportResizeAction`,
//! `MetadataCreateIndexService.prepareResizeIndexSettings`).
//!
//! The target takes the source's mapping and (copyable) settings, the request's settings and
//! aliases on top, and the source's routing shard count (so documents keep routing to the
//! right shard after a split or shrink). Validation and error texts follow Elasticsearch: the
//! source must be write-blocked, shard counts must divide (or multiply) cleanly, and some
//! settings can't change. The documents of the source are the engine's to copy: on a node this
//! is a segment-level copy (clone), or a filtered copy per target shard (split/shrink).

use super::indices::routing_num_shards;
use crate::error::{EsError, EsResult};
use crate::json::locate::key_location;
use crate::meta::blocks::{self, Level};
use crate::meta::index_settings;
use crate::meta::names::{self, random_uuid};
use crate::meta::resolve::resolve_date_math;
use crate::meta::settings::Settings;
use crate::meta::{AliasMeta, ClusterState, IndexMeta, IndexState};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::ByteSize;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ResizeType {
    Shrink,
    Split,
    Clone,
}

/// Settings a resize never copies from the source (they describe the source itself, or are
/// set anew).
const NOT_COPIED: &[&str] = &[
    "index.number_of_shards",
    "index.number_of_replicas",
    "index.uuid",
    "index.creation_date",
    "index.provided_name",
    "index.version.created",
    "index.routing_partition_size",
    "index.verified_before_close",
    "index.resize.source.name",
    "index.resize.source.uuid",
    "index.number_of_routing_shards",
];

/// Settings a resize can't change (`UNMODIFIABLE_SETTINGS_DURING_RESIZE`).
const UNMODIFIABLE: &[&str] = &[
    "index.mode",
    "index.mapping.source.mode",
    "index.recovery.use_synthetic_source",
    "index.sort.field",
    "index.sort.order",
    "index.sort.mode",
    "index.sort.missing",
];

struct ResizeRequest {
    settings: Settings,
    aliases: Vec<(String, AliasMeta)>,
    max_primary_shard_size: Option<ByteSize>,
}

fn parse(ctx: &Ctx) -> EsResult<ResizeRequest> {
    let mut r = ResizeRequest { settings: Settings::new(), aliases: Vec::new(), max_primary_shard_size: None };
    let Some(body) = ctx.body_map()? else { return Ok(r) };
    let raw = ctx.raw_json();
    for (k, v) in &body {
        match k.as_str() {
            "settings" => {
                r.settings = Settings::from_json(v)?;
                r.settings.normalize_prefix("index.");
            }
            "aliases" => r.aliases = crate::meta::aliases::parse_aliases(v)?,
            "max_primary_shard_size" => r.max_primary_shard_size = Some(ByteSize::parse(&v.to_es_string(), "max_primary_shard_size")?),
            other => {
                let (l, c) = raw.and_then(|b| key_location(b, &[], other)).unwrap_or((1, 1));
                return Err(EsError::x_content_parse(l, c, format!("[resize_request] unknown field [{other}]")));
            }
        }
    }
    Ok(r)
}

/// `IndexMetadata.getRoutingFactor`.
fn routing_factor(source: u32, target: u32) -> EsResult<u32> {
    if source < target {
        let f = target / source;
        if f * source != target || f <= 1 {
            return Err(EsError::illegal_argument(format!("the number of source shards [{source}] must be a factor of [{target}]")));
        }
        Ok(f)
    } else if source > target {
        let f = source / target;
        if f * target != source || f <= 1 {
            return Err(EsError::illegal_argument(format!("the number of source shards [{source}] must be a multiple of [{target}]")));
        }
        Ok(f)
    } else {
        Ok(1)
    }
}

/// Creates the resized index in `st`.
fn resize(st: &mut ClusterState, kind: ResizeType, source: &str, target: &str, req: ResizeRequest, now_ms: i64) -> EsResult<()> {
    let src = st.metadata.indices.get(source).cloned().ok_or_else(|| EsError::index_not_found_bare(source))?;
    let source_shards = src.number_of_shards();
    let requested: Option<u32> = match req.settings.get("index.number_of_shards") {
        Some(v) => Some(v.parse().map_err(|_| EsError::illegal_argument(format!("Failed to parse value [{v}] for setting [index.number_of_shards]")))?),
        None => None,
    };
    let target_shards = match kind {
        ResizeType::Shrink => match (requested, &req.max_primary_shard_size) {
            (Some(_), Some(_)) => return Err(EsError::illegal_argument("Cannot set both index.number_of_shards and max_primary_shard_size for the target index")),
            (Some(n), None) => n,
            // The source's store size decides the count; an empty store fits in one shard.
            (None, Some(_)) | (None, None) => 1,
        },
        ResizeType::Split => requested.unwrap_or(source_shards),
        ResizeType::Clone => requested.unwrap_or(source_shards),
    };
    if req.settings.contains("index.routing_partition_size") {
        return Err(EsError::illegal_argument("cannot provide a routing partition size value when resizing an index"));
    }
    if req.settings.contains("index.number_of_routing_shards") && !(kind == ResizeType::Split && source_shards == 1) {
        return Err(EsError::illegal_argument("cannot provide index.number_of_routing_shards on resize"));
    }
    if src.settings.get("index.soft_deletes.enabled") != Some("false") && req.settings.get("index.soft_deletes.enabled") == Some("false") {
        return Err(EsError::illegal_argument("Can't disable [index.soft_deletes.enabled] setting on resize"));
    }
    names::validate_index_name(target)?;
    // `validateResize`.
    if let Some(existing) = st.metadata.indices.get(target) {
        return Err(EsError::index_already_exists(target, &existing.uuid));
    }
    if let Some(ds) = st.metadata.data_stream_of(source)
        && ds.write_index() == Some(source)
    {
        return Err(EsError::illegal_argument(format!("cannot resize the write index [{source}] for data stream [{}]", ds.name)));
    }
    if blocks::check([src.as_ref()], Level::Write).is_ok() {
        return Err(EsError::illegal_state(format!("index {source} must be read-only to resize index. use \"index.blocks.write=true\"")));
    }
    routing_factor(source_shards, target_shards)?;
    for s in UNMODIFIABLE {
        if req.settings.get_value(s).is_some_and(|v| *v != crate::meta::settings::SettingValue::Null) {
            return Err(EsError::illegal_argument(format!("can't change setting [{s}] during resize")));
        }
    }
    match kind {
        ResizeType::Shrink => {
            if source_shards < target_shards {
                return Err(EsError::illegal_argument(format!(
                    "the number of target shards [{target_shards}] must be less that the number of source shards [{source_shards}]"
                )));
            }
            routing_factor(source_shards, target_shards)?;
            if source_shards == 1 {
                return Err(EsError::illegal_argument("can't shrink an index with only one shard"));
            }
        }
        ResizeType::Split => {
            routing_factor(source_shards, target_shards)?;
            if source_shards > target_shards {
                return Err(EsError::illegal_argument(format!(
                    "the number of source shards [{source_shards}] must be less that the number of target shards [{target_shards}]"
                )));
            }
            let routing = if source_shards == 1 { target_shards } else { src.routing_num_shards };
            if routing % target_shards != 0 {
                return Err(EsError::illegal_state(format!(
                    "the number of routing shards [{routing}] must be a multiple of the target shards [{target_shards}]"
                )));
            }
        }
        ResizeType::Clone => {
            if source_shards != target_shards {
                return Err(EsError::illegal_argument(format!(
                    "the number of target shards ({target_shards}) must be the same as the number of  source shards ( {source_shards})"
                )));
            }
        }
    }
    // Settings: the source's copyable ones, then the request's.
    let mut settings = Settings::new();
    for (k, v) in src.settings.iter() {
        if !NOT_COPIED.contains(&k.as_str()) && !k.starts_with("index.routing.allocation.initial_recovery.") {
            settings.insert(k.clone(), v.clone());
        }
    }
    settings.overlay_keeping_nulls(&req.settings);
    index_settings::validate_user_settings_in(st.dialect, &req.settings, true)?;
    settings.set("index.number_of_shards", target_shards.to_string());
    if !settings.contains("index.number_of_replicas") {
        settings.set("index.number_of_replicas", "1");
    }
    let routing_shards = if source_shards == 1 {
        match req.settings.get("index.number_of_routing_shards").and_then(|v| v.parse::<u32>().ok()) {
            Some(r) => r,
            None => routing_num_shards(target_shards),
        }
    } else {
        src.routing_num_shards
    };
    settings.remove("index.number_of_routing_shards");
    let uuid = random_uuid();
    settings.set("index.routing_partition_size", src.settings.get("index.routing_partition_size").unwrap_or("1"));
    settings.set("index.provided_name", target);
    settings.set("index.creation_date", now_ms.to_string());
    settings.set("index.uuid", uuid.clone());
    settings.set("index.version.created", src.settings.get("index.version.created").unwrap_or(crate::version::INDEX_VERSION_ID));
    crate::meta::auto_expand::apply(&mut settings);
    crate::analysis::validate_settings(&settings, Some(target))?;
    let mut aliases = BTreeMap::new();
    for (k, v) in req.aliases {
        let k = resolve_date_math(&k, now_ms)?;
        names::validate_alias_name(&k)?;
        aliases.insert(k, v);
    }
    // Primary terms start above every term the source shards had.
    let term = src.primary_terms.iter().copied().max().unwrap_or(1) + 1;
    let idx = IndexMeta {
        name: target.to_string(),
        uuid,
        state: IndexState::Open,
        settings,
        mapping: src.mapping.clone(),
        aliases,
        version: 1,
        mapping_version: 1,
        settings_version: 1,
        aliases_version: 1,
        routing_num_shards: routing_shards,
        primary_terms: vec![term; target_shards as usize],
        system: src.system,
        recovery_source: None,
        dialect: src.dialect,
    };
    st.metadata.indices.insert(target.to_string(), Arc::new(idx));
    st.check_write_indices()?;
    st.check_hidden_aliases()?;
    st.check_unique_names()?;
    Ok(())
}

pub async fn handle(ctx: &Ctx<'_>, kind: ResizeType) -> EsResult<Reply> {
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch && ctx.has_query("copy_settings") && !ctx.bool_param("copy_settings", true)? {
        return Err(EsError::illegal_argument("parameter [copy_settings] can not be explicitly set to [false]"));
    }
    ctx.validate_timeouts()?;
    let req = parse(ctx)?;
    // `ResizeRequest.validate`.
    let mut errors = Vec::new();
    if req.settings.keys().any(|k| k.starts_with("index.sort.")) {
        errors.push("can't override index sort when resizing an index".to_string());
    }
    if kind == ResizeType::Split && !req.settings.contains("index.number_of_shards") {
        errors.push("index.number_of_shards is required for split operations".to_string());
    }
    if req.max_primary_shard_size.is_some_and(|b| b.bytes() <= 0) {
        errors.push("max_primary_shard_size must be greater than 0".to_string());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let now = ctx.now_ms;
    let source = resolve_date_math(ctx.path_param("index").unwrap_or_default(), now)?;
    let target = resolve_date_math(ctx.path_param("target").unwrap_or_default(), now)?;
    let t = target.clone();
    ctx.cluster.update(move |st| resize(st, kind, &source, &t, req, now)).await?;
    Reply::ok(obj! { "acknowledged" => true, "shards_acknowledged" => true, "index" => target })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_factors() {
        assert_eq!(routing_factor(4, 2).unwrap(), 2);
        assert_eq!(routing_factor(2, 8).unwrap(), 4);
        assert_eq!(routing_factor(3, 3).unwrap(), 1);
        assert_eq!(routing_factor(4, 3).unwrap_err().reason(), "the number of source shards [4] must be a multiple of [3]");
        assert_eq!(routing_factor(4, 6).unwrap_err().reason(), "the number of source shards [4] must be a factor of [6]");
    }
}
