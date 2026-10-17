//! The production snapshot format: an RDB file (version [`rdb::RDB_VERSION`]), the same bytes
//! `SAVE` would write in Redis 8.

use crate::Database;
use crate::persist::SnapshotCodec;
use redis_engine::Engine;
use redis_engine::config::Policy;
use redis_engine::rdb::{self, LoadItem, Lru, RdbError, RdbWriter};
use std::sync::Arc;

/// [`SnapshotCodec`] over [`rdb`]. Every database of the engine is written with its expiries,
/// plus the idle time (LRU/LRM policies) or the frequency counter (LFU policies) of each key,
/// as Redis does, and the function libraries.
pub struct RdbCodec;

impl SnapshotCodec for RdbCodec {
    fn save(&self, engine: &Engine, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let now = redis_engine::util::now_ms();
        let policy = engine.cfg.maxmemory_policy;
        let lfu = policy.is_lfu();
        let idle = matches!(policy, Policy::AllKeysLru | Policy::VolatileLru | Policy::AllKeysLrm | Policy::VolatileLrm);
        let ctime = (now / 1000).to_string();
        let used = engine.used_memory().to_string();
        let aux = [
            ("redis-ver", redis_engine::commands::server::REDIS_VERSION),
            ("redis-bits", "64"),
            ("ctime", &ctime),
            ("used-mem", &used),
            ("aof-base", "0"),
        ];
        let mut w = RdbWriter::new(out, &aux, now)?;
        for code in engine.scripting.library_codes() {
            w.function(&code)?;
        }
        w.templates(&engine.hub.templates)?;
        for (i, db) in engine.dbs.iter().enumerate() {
            if db.is_empty() {
                continue;
            }
            w.begin_db(i, db.len() as u64, db.expires() as u64)?;
            for (k, e) in db.keys.iter() {
                if e.expired(now) {
                    continue;
                }
                let lru = if lfu {
                    Lru::Freq(engine.lfu_counter(e, now).min(255) as u8)
                } else if idle {
                    Lru::Idle(Engine::idle_secs(e, now))
                } else {
                    Lru::None
                };
                w.entry(k, e, lru)?;
            }
        }
        w.finish()?;
        Ok(())
    }

    fn load(&self, db: &Arc<Database>, data: &[u8]) -> anyhow::Result<()> {
        let now = redis_engine::util::now_ms();
        let mut eng = db.engine(0);
        let cfg = eng.cfg.clone();
        let mut functions = Vec::new();
        let reg = eng.hub.templates.clone();
        let lim = redis_engine::types::hash::Limits::of(&cfg);
        rdb::load_with(data, &cfg, Some(&reg), |it| {
            if let LoadItem::Untemplate { db, key } = &it {
                if let Some(d) = eng.dbs.get_mut(*db) {
                    d.untemplate(key, lim);
                }
            } else if let LoadItem::Function(code) = it {
                functions.push(code);
            } else if let LoadItem::Entry { db, key, entry } = it {
                let Some(d) = eng.dbs.get_mut(db) else { return Err(RdbError::Corrupt("database index out of range")) };
                if entry.expired(now) {
                    return Ok(());
                }
                if d.keys.contains_key(&key[..]) {
                    return Err(RdbError::Corrupt("duplicate key"));
                }
                d.insert_new(&key, entry);
            }
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("RDB snapshot: {e}"))?;
        eng.scripting
            .restore_libraries(&functions, redis_engine::commands::scripting::RestorePolicy::Replace)
            .map_err(|e| anyhow::anyhow!("RDB snapshot functions: {e}"))?;
        Ok(())
    }
}
