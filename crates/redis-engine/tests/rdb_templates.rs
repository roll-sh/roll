//! Hash templates in RDB files: `data/redis8_templates.rdb` was written by redis:8-alpine (8.10.2)
//! after `HIMPORT` (template records, then keys referencing them), and the load-time conversion
//! of plain hashes (`hash-rdb-load-*-template-*`) over `data/redis8.rdb`.

use redis_engine::db::Db;
use redis_engine::rdb::{self, LoadItem, RdbError};
use redis_engine::types::template::{Registry, Stats};
use redis_engine::{Config, Entry, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

const TEMPLATES: &[u8] = include_bytes!("data/redis8_templates.rdb");
const PLAIN: &[u8] = include_bytes!("data/redis8.rdb");

type Keys = BTreeMap<(usize, String), Entry>;

fn read(file: &[u8], cfg: &Config, reg: &Arc<Registry>) -> Result<(Keys, Vec<(usize, String)>), RdbError> {
    let (mut keys, mut untemplate) = (Keys::new(), Vec::new());
    rdb::load_with(file, cfg, Some(reg), |it| {
        match it {
            LoadItem::Entry { db, key, entry } => {
                keys.insert((db, String::from_utf8_lossy(&key).into_owned()), entry);
            }
            LoadItem::Untemplate { db, key } => untemplate.push((db, String::from_utf8_lossy(&key).into_owned())),
            _ => {}
        }
        Ok(())
    })?;
    untemplate.sort();
    Ok((keys, untemplate))
}

fn hash(keys: &Keys, db: usize, k: &str) -> (&'static str, Vec<(String, String)>) {
    let Value::Hash(h) = &keys[&(db, k.to_string())].value else { panic!("{k} is not a hash") };
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (h.encoding(), h.iter().map(|(f, v, _)| (s(f), s(v))).collect())
}

fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
    p.iter().map(|(f, v)| (f.to_string(), v.to_string())).collect()
}

#[test]
fn loads_redis_template_records() {
    let reg = Registry::new();
    let (keys, untemplate) = read(TEMPLATES, &Config::default(), &reg).unwrap();
    assert!(untemplate.is_empty());
    assert_eq!(hash(&keys, 0, "t1"), ("template-listpack", pairs(&[("a", "2"), ("b", "1"), ("dd", "4"), ("ccc", "3")])));
    assert_eq!(hash(&keys, 0, "t2").1, pairs(&[("a", "x"), ("b", "w"), ("dd", "z"), ("ccc", "y")]));
    assert_eq!(hash(&keys, 2, "t4").1, pairs(&[("a", "6"), ("b", "5"), ("dd", "8"), ("ccc", "7")]));
    let (enc, t3) = hash(&keys, 0, "t3");
    assert_eq!((enc, t3[0].clone(), t3[1].1.len()), ("template-array", ("beta".into(), "short".into()), 100));
    assert_eq!(hash(&keys, 0, "plain").0, "listpack");
    // The records' holds are gone with the load: the keys keep the templates.
    assert_eq!((reg.stats().templates, reg.stats().keys), (2, 4));
    drop(keys);
    assert_eq!((reg.stats().templates, reg.stats().keys), (0, 0));
}

#[test]
fn saves_template_records_and_loads_them_back() {
    let reg = Registry::new();
    let (keys, _) = read(TEMPLATES, &Config::default(), &reg).unwrap();
    let mut dbs = [Db::default(), Db::default(), Db::default()];
    for ((db, k), e) in &keys {
        dbs[*db].insert_new(k.as_bytes(), e.clone());
    }
    let mut file = Vec::new();
    rdb::save(dbs.iter().enumerate(), &[], &[], &mut file).unwrap();
    let again = Registry::new();
    let (keys2, _) = read(&file, &Config::default(), &again).unwrap();
    assert_eq!(keys2.len(), keys.len());
    for (db, k) in keys.keys() {
        assert_eq!(hash(&keys2, *db, k), hash(&keys, *db, k), "{k}");
    }
    assert_eq!((again.stats().templates, again.stats().keys), (2, 4));
    // Without a registry template content is unsupported.
    assert!(matches!(rdb::load(&file[..], &Config::default(), |_| Ok(())), Err(RdbError::Unsupported(_))));
}

#[test]
fn rejects_bad_template_records() {
    let reg = Registry::new();
    // A key referencing a template id that has no record.
    let mut bad = TEMPLATES.to_vec();
    let at = bad.windows(4).position(|w| w == b"\x1e\x02t1").unwrap();
    bad[at + 8 + 1] = 9; // the leading template id of t1's listpack
    let n = bad.len();
    bad[n - 8..].fill(0);
    assert!(matches!(read(&bad, &Config::default(), &reg), Err(RdbError::Corrupt(_))));
    // Unsorted template fields.
    let mut bad = TEMPLATES.to_vec();
    let at = bad.windows(6).position(|w| w == b"\x02\x04beta").unwrap();
    bad[at + 2..at + 6].copy_from_slice(b"zeta");
    let n = bad.len();
    bad[n - 8..].fill(0);
    assert!(read(&bad, &Config::default(), &reg).is_ok());
    let at = bad.windows(4).position(|w| w == b"zeta").unwrap();
    bad[at - 1..at + 10].copy_from_slice(b"\x05alpha\x04zeta");
    assert!(matches!(read(&bad, &Config::default(), &reg), Err(RdbError::Corrupt(_))));
    assert_eq!(reg.stats(), Stats { templates: 0, keys: 0, mem: reg.stats().mem });
}

#[test]
fn converts_plain_hashes_while_loading() {
    let cfg = Config { hash_max_listpack_entries: 128, hash_rdb_load_min_template_entries: 1, ..Config::default() };
    let reg = Registry::new();
    let (keys, untemplate) = read(PLAIN, &cfg, &reg).unwrap();
    assert!(untemplate.is_empty());
    for (k, enc) in [("hlp", "template-listpack"), ("hht", "template-array"), ("hlpex", "listpackex"), ("hhtex", "hashtable")] {
        assert_eq!(hash(&keys, 0, k).0, enc, "{k}");
    }
    // Template order: shorter field names first.
    let fields: Vec<String> = hash(&keys, 0, "hlp").1.into_iter().map(|(f, _)| f).collect();
    let mut sorted = fields.clone();
    sorted.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
    assert_eq!(fields, sorted);
    drop(keys);

    // Bounded by hash-rdb-load-max-template-entries.
    let cfg2 = Config { hash_rdb_load_max_template_entries: 1, ..cfg.clone() };
    let (keys, _) = read(PLAIN, &cfg2, &reg).unwrap();
    assert_eq!(hash(&keys, 0, "hlp").0, "listpack");
    drop(keys);

    // Templates with fewer keys than the disassembly threshold are undone at the end.
    let cfg3 = Config { hash_rdb_load_template_disassembly_threshold: 2, ..cfg };
    let (keys, untemplate) = read(PLAIN, &cfg3, &reg).unwrap();
    assert_eq!(untemplate, [(0, "hht".to_string()), (0, "hlp".to_string())]);
    let mut db = Db::default();
    for ((_, k), e) in keys {
        db.insert_new(k.as_bytes(), e);
    }
    let lim = redis_engine::types::hash::Limits::of(&cfg3);
    for (_, k) in &untemplate {
        db.untemplate(k.as_bytes(), lim);
    }
    let enc = |k: &str| match db.keys.get(k.as_bytes()).unwrap().value() {
        redis_engine::value::ValueRef::Hash(h) => h.encoding(),
        _ => unreachable!(),
    };
    assert_eq!((enc("hlp"), enc("hht")), ("listpack", "hashtable"));
    assert_eq!(reg.stats().templates, 0);

    // A file that has template records is loaded as is.
    let (keys, _) = read(TEMPLATES, &cfg3, &reg).unwrap();
    assert_eq!(hash(&keys, 0, "plain").0, "listpack");
}
