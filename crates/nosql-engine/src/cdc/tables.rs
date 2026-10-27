//! The CDC stream tables (ScyllaDB):
//!
//! - `system_distributed.cdc_generation_timestamps` and `cdc_streams_descriptions_v2`: the vnode
//!   generation, one row per vnode (`range_end`, its stream);
//! - `system.cdc_timestamps` and `system.cdc_streams`: the stream sets of CDC tables in tablets
//!   keyspaces (`stream_state` 0: the stream is current).

use std::sync::Arc;

use super::{GENERATION_MS, LogSpec};
use crate::error::Result;
use crate::schema::Table;
use crate::system::{Registry, VRow, VirtualContext, VirtualTable, val};

/// Registers the stream tables.
pub fn register(r: &mut Registry) {
    r.add(
        "system_distributed",
        "cdc_generation_timestamps",
        Arc::new(GenerationTimestamps),
    );
    r.add(
        "system_distributed",
        "cdc_streams_descriptions_v2",
        Arc::new(Descriptions),
    );
    r.add("system", "cdc_timestamps", Arc::new(TabletTimestamps));
    r.add("system", "cdc_streams", Arc::new(TabletStreams));
}

fn timestamp(ms: i64) -> Option<bytes::Bytes> {
    val::bigint(ms)
}

struct GenerationTimestamps;

impl VirtualTable for GenerationTimestamps {
    fn rows(&self, _t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
        Ok(vec![vec![
            ("key", val::text("timestamps")),
            ("time", timestamp(GENERATION_MS)),
        ]])
    }
}

struct Descriptions;

impl VirtualTable for Descriptions {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let host = ctx.cfg.host_id();
        Ok(super::generation(ctx.cfg)
            .into_iter()
            .map(|t| {
                let mut salt = host.to_vec();
                salt.extend_from_slice(&t.to_be_bytes());
                let id = super::stream_id(t, &salt);
                vec![
                    ("time", timestamp(GENERATION_MS)),
                    ("range_end", val::bigint(t)),
                    ("streams", Some(nosql_types::pack_collection([&id[..]]))),
                ]
            })
            .collect())
    }
}

/// The CDC tables of tablets keyspaces with their stream sets, oldest first.
fn tablet_logs(ctx: &VirtualContext) -> Vec<(String, String, Vec<super::Streams>)> {
    let mut out = Vec::new();
    for ks in ctx.schema.user_keyspaces() {
        if ks.tablets.is_none() {
            continue;
        }
        for t in ks.tables.values() {
            if LogSpec::for_base(ks, t, ctx.cfg).is_none() {
                continue;
            }
            if let Some(log) = super::log_of(ks, &t.name) {
                out.push((ks.name.clone(), t.name.clone(), super::generations(ks, t, log)));
            }
        }
    }
    out
}

struct TabletTimestamps;

impl VirtualTable for TabletTimestamps {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let mut out = Vec::new();
        for (ks, t, gens) in tablet_logs(ctx) {
            for g in &gens {
                out.push(vec![
                    ("keyspace_name", val::text(&ks)),
                    ("table_name", val::text(&t)),
                    ("timestamp", timestamp(g.timestamp_ms)),
                ]);
            }
        }
        Ok(out)
    }
}

struct TabletStreams;

impl VirtualTable for TabletStreams {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let mut out = Vec::new();
        for (ks, t, gens) in tablet_logs(ctx) {
            // Every generation lists its streams as current (`stream_state` 0).
            for g in &gens {
                for (_, id) in &g.ranges {
                    out.push(vec![
                        ("keyspace_name", val::text(&ks)),
                        ("table_name", val::text(&t)),
                        ("timestamp", timestamp(g.timestamp_ms)),
                        ("stream_state", Some(bytes::Bytes::from_static(&[0]))),
                        ("stream_id", Some(bytes::Bytes::copy_from_slice(id))),
                    ]);
                }
            }
        }
        Ok(out)
    }
}
