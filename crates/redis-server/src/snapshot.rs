//! The production snapshot format: an RDB file (version [`rdb::RDB_VERSION`]), the same bytes
//! `SAVE` would write in Redis 8.

use crate::Database;
use crate::persist::SnapshotCodec;
use redis_engine::Engine;
#[cfg(doc)]
use redis_engine::rdb;
use std::sync::Arc;

/// [`SnapshotCodec`] over [`rdb`]. Every database of the engine is written with its expiries,
/// plus the idle time (LRU/LRM policies) or the frequency counter (LFU policies) of each key,
/// as Redis does, and the function libraries.
pub struct RdbCodec;

impl SnapshotCodec for RdbCodec {
    fn save(&self, engine: &Engine, out: &mut Vec<u8>) -> anyhow::Result<()> {
        engine.write_rdb(out, redis_engine::util::now_ms())?;
        Ok(())
    }

    fn load(&self, db: &Arc<Database>, data: &[u8]) -> anyhow::Result<()> {
        let mut eng = db.engine(0);
        eng.read_rdb(data, false, redis_engine::util::now_ms()).map_err(|e| anyhow::anyhow!("RDB snapshot: {e}"))
    }
}
