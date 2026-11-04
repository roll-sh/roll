//! Scratch (agent 169, not committed): what a store's cached blocks hold, per relation prefix.
//! `a169_blocks <store dir>`
use pg_engine::storage::cache::BlockCache;
use pg_engine::storage::sst::{SstMeta, SstReader};
use std::collections::BTreeMap;

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("dir"));
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("MANIFEST")).unwrap()).unwrap();
    let cache = BlockCache::new(1 << 20);
    // prefix -> (files bytes, blocks, entries, decoded bytes, raw bytes)
    let mut by: BTreeMap<Vec<u8>, [u64; 5]> = BTreeMap::new();
    for lvl in ["l0", "l1"] {
        for f in m[lvl].as_array().unwrap() {
            let meta: SstMeta = serde_json::from_value(f.clone()).unwrap();
            let r = SstReader::open(&dir, meta, cache.clone()).unwrap();
            for b in 0..r.block_count() {
                let blk = r.block_with(b, false).unwrap();
                if std::env::var_os("WALK").is_some() {
                    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut pos = pg_engine::storage::sst::BlockPos::default();
                        blk.at(0, &mut pos);
                        while pos.index() < blk.len() {
                            let _ = blk.value_ref(&pos);
                            blk.step(&mut pos);
                        }
                    }));
                    if ok.is_err() {
                        println!("bad: file {} (commit_ts {:?}) block {b} of {}", r.meta.id, r.meta.commit_ts, r.block_count());
                    }
                }
                let p = blk.last_key().unwrap()[..8.min(blk.last_key().unwrap().len())].to_vec();
                let e = by.entry(p).or_default();
                e[1] += 1;
                e[2] += blk.len() as u64;
                e[3] += blk.mem_bytes() as u64;
            }
            let p = r.meta.first_key[..8.min(r.meta.first_key.len())].to_vec();
            by.entry(p).or_default()[0] += r.meta.size;
        }
    }
    println!("prefix                files_MB blocks   entries  decoded_MB B/entry");
    for (p, e) in &by {
        if e[1] < 50 {
            continue;
        }
        println!(
            "{:<20} {:>9.1} {:>6} {:>9} {:>10.1} {:>7.1}",
            p.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            e[0] as f64 / 1e6,
            e[1],
            e[2],
            e[3] as f64 / 1e6,
            e[3] as f64 / e[2].max(1) as f64
        );
    }
}
