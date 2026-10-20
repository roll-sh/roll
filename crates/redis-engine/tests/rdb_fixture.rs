//! Load `data/redis8.rdb`, written by `SAVE` in redis:8-alpine (8.10.2) with
//! `hash-max-listpack-entries 128`: every type and encoding, expiries, three databases and a
//! function library. Then save it again and load our own file.

use redis_engine::db::Db;
use redis_engine::rdb::{self, LoadInfo, LoadItem};
use redis_engine::types::stream::StreamId;
use redis_engine::{Config, Entry, Value};
use std::collections::BTreeMap;

const FIXTURE: &[u8] = include_bytes!("data/redis8.rdb");
const FAR: i64 = 4102444800000;

fn cfg() -> Config {
    Config { hash_max_listpack_entries: 128, ..Config::default() }
}

type Keys = BTreeMap<(usize, Vec<u8>), Entry>;
type Read = (LoadInfo, Keys, Vec<(String, String)>, Vec<Vec<u8>>);

fn read(file: &[u8]) -> Read {
    let (mut keys, mut aux, mut funcs) = (Keys::new(), Vec::new(), Vec::new());
    let info = rdb::load(file, &cfg(), |it| {
        match it {
            LoadItem::Entry { db, key, entry } => {
                assert!(keys.insert((db, key.to_vec()), entry).is_none(), "duplicate key");
            }
            LoadItem::Aux { key, value } => {
                aux.push((String::from_utf8_lossy(key).into_owned(), String::from_utf8_lossy(value).into_owned()))
            }
            LoadItem::Function(code) => funcs.push(code),
            LoadItem::ResizeDb { .. } | LoadItem::Untemplate { .. } | LoadItem::ModuleAux { .. } => {}
        }
        Ok(())
    })
    .unwrap();
    (info, keys, aux, funcs)
}

fn get<'a>(keys: &'a Keys, db: usize, k: &str) -> &'a Entry {
    keys.get(&(db, k.as_bytes().to_vec())).unwrap_or_else(|| panic!("missing {db}/{k}"))
}

#[test]
fn loads_a_real_redis_rdb() {
    let (info, keys, aux, funcs) = read(FIXTURE);
    assert_eq!(info, LoadInfo { version: 15, keys: 19, expired: 0, empty_skipped: 0, functions: 1 });
    assert!(aux.contains(&("redis-ver".into(), "8.10.2".into())));
    assert!(aux.contains(&("aof-base".into(), "0".into())));
    assert_eq!(funcs.len(), 1);
    assert!(funcs[0].starts_with(b"#!lua name=mylib\n"));
    // The library loads and its function runs.
    let mut eng = redis_engine::Engine::standalone();
    eng.scripting.restore_libraries(&funcs, redis_engine::commands::scripting::RestorePolicy::Append).unwrap();
    assert_eq!(eng.scripting.libraries()[0].name, "mylib");
    let r = eng.call(&mut redis_engine::Client::new(1), &[bytes::Bytes::from_static(b"FUNCTION"), bytes::Bytes::from_static(b"LIST")]);
    assert!(format!("{:?}", r.unwrap_reply()).contains("mylib"));

    let enc = |db, k| get(&keys, db, k).value.encoding();
    for (k, e) in [
        ("list", "listpack"),
        ("quicklist", "quicklist"),
        ("intset", "intset"),
        ("setlp", "listpack"),
        ("setht", "hashtable"),
        ("zlp", "listpack"),
        ("zskip", "skiplist"),
        ("hlp", "listpack"),
        ("hht", "hashtable"),
        ("hlpex", "listpackex"),
        ("hhtex", "hashtable"),
        ("stream", "stream"),
    ] {
        assert_eq!(enc(0, k), e, "{k}");
    }
    for (db, k) in [(0, "ttl"), (0, "list"), (1, "db1set")] {
        assert_eq!(get(&keys, db, k).expire, FAR, "{k}");
    }
    assert_eq!(get(&keys, 0, "str").expire, 0);
    assert!(matches!(&get(&keys, 0, "str").value, Value::Str(s) if &s[..] == b"hello world"));
    assert!(matches!(&get(&keys, 0, "int").value, Value::Str(s) if &s[..] == b"-12345"));
    assert!(matches!(&get(&keys, 0, "lzf").value, Value::Str(s) if s[..] == *"abc".repeat(40).as_bytes()));
    assert!(matches!(&get(&keys, 3, "db3key").value, Value::Str(s) if &s[..] == b"3"));
    let Value::List(l) = &get(&keys, 0, "quicklist").value else { panic!() };
    assert_eq!((l.len(), l.first(), l.last()), (1200, Some(&b"item-00000"[..]), Some(&b"item-01199"[..])));
    let Value::ZSet(z) = &get(&keys, 0, "zskip").value else { panic!() };
    assert_eq!(z.score(b"z149"), Some(37.25));
    let Value::Hash(h) = &get(&keys, 0, "hhtex").value else { panic!() };
    assert_eq!((h.expire_of(b"f1"), h.expire_of(b"f3"), h.subexpiry()), (Some(FAR), Some(0), FAR));
    let Value::Stream(s) = &get(&keys, 0, "stream").value else { panic!() };
    let g = s.groups.get(b"g").unwrap();
    assert_eq!((s.entries.len(), g.pel_len(), g.nacked_count()), (5, 2, 1));
    assert_eq!(g.by_time().next(), Some(StreamId::new(2, 0)));
    assert_eq!(g.consumer(b"c1").unwrap().pending(), 1);
}

#[test]
fn saves_what_it_loaded() {
    let (_, keys, _, funcs) = read(FIXTURE);
    let mut dbs: Vec<Db> = (0..16).map(|_| Db::default()).collect();
    for ((db, k), e) in &keys {
        dbs[*db].insert_new(k, e.clone());
    }
    let mut file = Vec::new();
    rdb::save(dbs.iter().enumerate(), &[], &funcs, &mut file).unwrap();
    let (info, again, _, funcs2) = read(&file);
    assert_eq!((info.keys, funcs2), (keys.len() as u64, funcs));
    for (k, e) in &keys {
        let e2 = &again[k];
        assert_eq!(e2.expire, e.expire);
        // Hashtables iterate in hash order.
        if e.value.encoding() != "hashtable" {
            assert_eq!(rdb::dump(e2.value.view(), 0), rdb::dump(e.value.view(), 0), "{}", String::from_utf8_lossy(&k.1));
        }
    }
}
