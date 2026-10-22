//! Index blocks (`index.blocks.*` and closed indices) and the levels they block, as
//! Elasticsearch's `ClusterBlock`s.

use super::IndexMeta;
use crate::error::EsError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Read,
    Write,
    MetadataRead,
    MetadataWrite,
}

pub struct Block {
    pub setting: &'static str,
    pub id: u8,
    pub status: &'static str,
    pub code: u16,
    pub description: &'static str,
    pub levels: &'static [Level],
}

impl Block {
    /// The levels as the cluster state prints them.
    pub fn level_names(&self) -> Vec<&'static str> {
        self.levels
            .iter()
            .map(|l| match l {
                Level::Read => "read",
                Level::Write => "write",
                Level::MetadataRead => "metadata_read",
                Level::MetadataWrite => "metadata_write",
            })
            .collect()
    }
}

/// The blocks set on an index (by its `index.blocks.*` settings), in id order.
pub fn index_blocks(idx: &IndexMeta) -> Vec<&'static Block> {
    BLOCKS.iter().filter(|b| idx.bool_setting(b.setting)).collect()
}

const BLOCKS: &[Block] = &[
    Block { setting: "index.blocks.read_only", id: 5, status: "FORBIDDEN", code: 403, description: "index read-only (api)", levels: &[Level::Write, Level::MetadataWrite] },
    Block { setting: "index.blocks.read", id: 7, status: "FORBIDDEN", code: 403, description: "index read (api)", levels: &[Level::Read] },
    Block { setting: "index.blocks.write", id: 8, status: "FORBIDDEN", code: 403, description: "index write (api)", levels: &[Level::Write] },
    Block { setting: "index.blocks.metadata", id: 9, status: "FORBIDDEN", code: 403, description: "index metadata (api)", levels: &[Level::MetadataRead, Level::MetadataWrite] },
    Block {
        setting: "index.blocks.read_only_allow_delete",
        id: 12,
        status: "TOO_MANY_REQUESTS",
        code: 429,
        description: "disk usage exceeded flood-stage watermark, index has read-only-allow-delete block; for more information, see https://www.elastic.co/docs/troubleshoot/elasticsearch/fix-watermark-errors",
        levels: &[Level::Write],
    },
];

/// The `_block/{block}` names.
pub fn block_setting(name: &str) -> Option<&'static str> {
    match name {
        "metadata" => Some("index.blocks.metadata"),
        "read" => Some("index.blocks.read"),
        "read_only" => Some("index.blocks.read_only"),
        "write" => Some("index.blocks.write"),
        _ => None,
    }
}

/// Fails with `cluster_block_exception` if any of `indices` is blocked at `level`.
pub fn check<'a>(indices: impl IntoIterator<Item = &'a IndexMeta>, level: Level) -> Result<(), EsError> {
    let mut msg = String::new();
    let mut status = 0u16;
    for idx in indices {
        let mut descs = Vec::new();
        if idx.is_closed() && matches!(level, Level::Read | Level::Write) {
            descs.push("FORBIDDEN/4/index closed".to_string());
            status = status.max(403);
        }
        for b in BLOCKS {
            if b.levels.contains(&level) && idx.bool_setting(b.setting) {
                descs.push(format!("{}/{}/{}", b.status, b.id, b.description));
                // Elasticsearch reports 429 only when every block is retryable (429).
                status = if status == 0 || status == b.code { b.code } else { 403 };
            }
        }
        if !descs.is_empty() {
            msg.push_str(&format!("index [{}] blocked by: [{}];", idx.name, descs.join(", ")));
        }
    }
    if msg.is_empty() {
        return Ok(());
    }
    Err(EsError::new(status, "cluster_block_exception", msg))
}
