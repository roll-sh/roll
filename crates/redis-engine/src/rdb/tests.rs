use super::encode::{put_len, put_string};
use super::listpack::{Item, LpWriter};
use super::*;
use crate::client::Client;
use crate::engine::Engine;
use crate::types::stream::StreamId;
use crate::util::Rng;
use bytes::Bytes;
use redis_proto::Reply;

#[path = "redis_cases.rs"]
mod redis_cases;
use redis_cases::{CAPTURED_AT, ENCODINGS, REDIS};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn cfg() -> Config {
    Config { hash_max_listpack_entries: 128, ..Config::default() }
}

/// Wrap an object (type byte + body) as a DUMP payload.
fn payload(obj: &[u8]) -> Vec<u8> {
    let mut p = obj.to_vec();
    p.extend_from_slice(&RDB_VERSION.to_le_bytes());
    let crc = crc64(0, &p);
    p.extend_from_slice(&crc.to_le_bytes());
    p
}

fn redis(name: &str) -> Vec<u8> {
    unhex(REDIS.iter().find(|(n, _)| *n == name).unwrap().1)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// The commands that built each case in `redis_cases.rs` (mirrors the capture script).
fn commands() -> Vec<(&'static str, Vec<Vec<String>>)> {
    let mut c: Vec<(&'static str, Vec<Vec<String>>)> = Vec::new();
    let set = |k: &str, v: String| vec![vec!["SET".into(), k.to_string(), v]];
    c.push(("s_small", set("s_small", "hello".into())));
    c.push(("s_int8", set("s_int8", "-100".into())));
    c.push(("s_int16", set("s_int16", "-129".into())));
    c.push(("s_int32", set("s_int32", "2147483647".into())));
    c.push(("s_big_int", set("s_big_int", "4294967296".into())));
    c.push(("s_lzf", set("s_lzf", "a".repeat(30))));
    c.push(("s_empty", set("s_empty", String::new())));
    c.push(("s_noncanon", set("s_noncanon", "007".into())));
    c.push(("s_incompressible", set("s_incompressible", "abcdefghijklmnopqrstuvwxyz0123456789ABCD".into())));
    c.push(("s_large", set("s_large", "0123456789".repeat(2000))));

    c.push(("l_small", vec![s(&["RPUSH", "l_small", "a", "b", "1", "2"])]));
    let mut l = s(&["RPUSH", "l_ints", "-5", "127", "128", "-4096", "4095", "70000", "-70000", "9000000", "3000000000"]);
    l.extend(["-9223372036854775808".to_string(), "x".repeat(70), "y".repeat(5000)]);
    c.push(("l_ints", vec![l]));
    let mut l = s(&["RPUSH", "l_large"]);
    l.extend((0..900).map(|i| format!("element-number-{i}")));
    l.extend((0..100).map(|i| i.to_string()));
    c.push(("l_large", vec![l]));

    c.push(("set_int16", vec![s(&["SADD", "set_int16", "3", "1", "2", "-7"])]));
    c.push(("set_int32", vec![s(&["SADD", "set_int32", "1", "100000", "-5"])]));
    c.push(("set_int64", vec![s(&["SADD", "set_int64", "5000000000", "1"])]));
    let mut l = s(&["SADD", "set_lp", "b", "a", "c", "1"]);
    l.push("x".repeat(40));
    c.push(("set_lp", vec![l]));
    let mut l = s(&["SADD", "set_ht"]);
    l.extend((0..200).map(|i| format!("member:{i}")));
    c.push(("set_ht", vec![l]));

    let z = ["1", "a", "2.5", "b", "-3", "c", "1e20", "d", "inf", "e", "-inf", "f", "4611686018427387904", "g", "0.1", "h"];
    let mut l = s(&["ZADD", "z_lp"]);
    l.extend(s(&z));
    c.push(("z_lp", vec![l]));
    let mut l = s(&["ZADD", "z_skip"]);
    for i in 0..200 {
        l.push((i as f64 * 1.5 - 30.0).to_string());
        l.push(format!("member-{i}"));
    }
    c.push(("z_skip", vec![l]));
    c.push(("z_zero", vec![s(&["ZADD", "z_zero", "-0", "y", "0", "x"])]));

    let mut l = s(&["HSET", "h_lp", "f1", "v1", "f2", "123", "n", "-7", "long"]);
    l.push("z".repeat(30));
    c.push(("h_lp", vec![l]));
    let fields = |k: &str, n: usize| {
        let mut l = s(&["HSET", k]);
        for i in 0..n {
            l.push(format!("field:{i}"));
            l.push(format!("value:{i}"));
        }
        l
    };
    c.push(("h_ht", vec![fields("h_ht", 200)]));
    c.push((
        "h_lpex",
        vec![
            s(&["HSET", "h_lpex", "a", "1", "b", "2", "c", "3", "d", "4"]),
            s(&["HPEXPIREAT", "h_lpex", "4102444800000", "FIELDS", "1", "b"]),
            s(&["HPEXPIREAT", "h_lpex", "4102444801000", "FIELDS", "1", "a"]),
            s(&["HPEXPIREAT", "h_lpex", "4102444800000", "FIELDS", "1", "d"]),
        ],
    ));
    for (name, n) in [("h_lpex_big", 100), ("h_htex", 200)] {
        c.push((
            name,
            vec![
                fields(name, n),
                s(&["HPEXPIREAT", name, "4102444800000", "FIELDS", "2", "field:3", "field:7"]),
                s(&["HPEXPIREAT", name, "4102444900000", "FIELDS", "1", "field:9"]),
            ],
        ));
    }

    c.push((
        "x_plain",
        vec![
            s(&["XADD", "x_plain", "1-1", "f", "v"]),
            s(&["XADD", "x_plain", "1-2", "f", "w"]),
            s(&["XADD", "x_plain", "2-0", "a", "1", "b", "2"]),
            s(&["XADD", "x_plain", "3-5", "f", "12345"]),
            s(&["XDEL", "x_plain", "1-2"]),
        ],
    ));
    let mut x: Vec<Vec<String>> = (0..250)
        .map(|i| {
            let id = format!("{}-{}", 1000 + i / 3, i % 3);
            vec!["XADD".into(), "x_large".into(), id, "sensor".into(), "temp".into(), "value".into(), (i * 7).to_string()]
        })
        .collect();
    x.push(s(&["XADD", "x_large", "5000-0", "other", "fields", "here", "x"]));
    c.push(("x_large", x));
    c.push(("x_empty", vec![s(&["XADD", "x_empty", "7-0", "a", "1"]), s(&["XDEL", "x_empty", "7-0"])]));
    c
}

/// Cases whose bytes depend on hash-table order or the clock: compared by content.
const UNORDERED: &[&str] = &["set_ht", "h_ht", "h_htex"];

fn engine() -> (Engine, Client) {
    let mut e = Engine::standalone();
    e.cfg = cfg();
    (e, Client::new(1))
}

fn call(e: &mut Engine, c: &mut Client, args: &[&[u8]]) -> Reply {
    let args: Vec<Bytes> = args.iter().map(|a| Bytes::copy_from_slice(a)).collect();
    e.call(c, &args).unwrap_reply()
}

fn run(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
    let args: Vec<&[u8]> = line.split(' ').map(str::as_bytes).collect();
    call(e, c, &args)
}

/// Everything observable about a value, order-independent where the encoding is unordered.
fn contents(v: &Value) -> String {
    let mut parts: Vec<String> = match v {
        Value::Str(s) => vec![format!("{s:?}")],
        Value::List(l) => return format!("{:?}", l.iter().collect::<Vec<_>>()),
        Value::Set(s) => s.iter_members().map(|m| format!("{m:?}")).collect(),
        Value::ZSet(z) => return format!("{:?}", z.iter().collect::<Vec<_>>()),
        Value::Hash(h) => h.iter().map(|(f, v, at)| format!("{f:?}={v:?}@{at}")).collect(),
        Value::Stream(_) => return format!("{:?}", dump(v, CAPTURED_AT)),
        Value::Array(a) => return format!("{} {:?}", a.insert_idx(), a.iter().collect::<Vec<_>>()),
        Value::Json(j) => return crate::json::ser::compact(j.root()),
        Value::Sketch(x) => return format!("{x:?}"),
        Value::VSet(v) => return format!("{:?}", v.dict.keys().collect::<Vec<_>>()),
        Value::Ts(t) => return format!("{:?}", crate::ts::iter::Samples::new(t, &crate::ts::iter::Range::span(0, u64::MAX), false, false, None).collect::<Vec<_>>()),
    };
    parts.sort();
    format!("{} {}", v.encoding(), parts.join(","))
}

#[test]
fn redis_payloads_restore_and_dump_identically() {
    let cfg = cfg();
    for &(name, hex) in REDIS {
        let p = unhex(hex);
        let v = restore_at(&p, &cfg, CAPTURED_AT).unwrap_or_else(|e| panic!("{name}: {e}"));
        let enc = ENCODINGS.iter().find(|(n, _)| *n == name).unwrap().1;
        // String encodings depend on Redis 8's key embedding; aggregates must match.
        if !name.starts_with("s_") {
            assert_eq!(v.encoding(), enc, "{name}: encoding");
        }
        let again = dump(&v, CAPTURED_AT);
        if UNORDERED.contains(&name) {
            let back = restore_at(&again, &cfg, CAPTURED_AT).unwrap();
            assert_eq!(contents(&back), contents(&v), "{name}");
        } else {
            assert_eq!(again, p, "{name}: re-encoded bytes differ");
        }
    }
}

#[test]
fn engine_dump_matches_redis_bytes() {
    let (mut e, mut c) = engine();
    for (name, cmds) in commands() {
        for cmd in &cmds {
            let args: Vec<&[u8]> = cmd.iter().map(|a| a.as_bytes()).collect();
            let r = call(&mut e, &mut c, &args);
            assert!(!r.is_error(), "{name}: {cmd:?} -> {r:?}");
        }
        let Reply::Bulk(got) = call(&mut e, &mut c, &[b"DUMP", name.as_bytes()]) else { panic!("{name}: no value") };
        let want = redis(name);
        if UNORDERED.contains(&name) {
            let a = restore(&got, &cfg()).unwrap();
            let b = restore(&want, &cfg()).unwrap();
            assert_eq!(contents(&a), contents(&b), "{name}");
        } else {
            assert_eq!(got.as_ref(), &want[..], "{name}: engine DUMP differs from Redis");
        }
    }
}

#[test]
fn streams_with_groups_nack_zone_and_idmp() {
    let cfg = cfg();
    let v = restore_at(&redis("x_groups"), &cfg, CAPTURED_AT).unwrap();
    let Value::Stream(s) = &v else { panic!() };
    assert_eq!(s.entries.len(), 6);
    let names: Vec<&[u8]> = s.groups.iter().map(|(n, _)| n).collect();
    assert_eq!(names, [&b"g0"[..], b"g1", b"g2"]);
    let g1 = s.groups.get(b"g1").unwrap();
    assert_eq!(g1.pel_len(), 4);
    assert_eq!(g1.nacked_count(), 1);
    assert_eq!(g1.by_time().next(), Some(StreamId::new(4, 0)));
    assert!(g1.nack(StreamId::new(4, 0)).unwrap().consumer.is_none());
    assert_eq!(g1.nack(StreamId::new(2, 0)).unwrap().delivery_count, 4);
    let cons: Vec<(&[u8], usize)> = g1.consumers().map(|(n, c)| (n, c.pending())).collect();
    assert_eq!(cons, [(&b"alice"[..], 1), (b"bob", 2), (b"carol", 0)]);
    assert_eq!(g1.consumer(b"carol").unwrap().active_time, -1);
    assert_eq!(s.groups.get(b"g2").unwrap().entries_read, 7);

    let v = restore_at(&redis("x_idmp"), &cfg, CAPTURED_AT).unwrap();
    let Value::Stream(s) = &v else { panic!() };
    assert_eq!((s.idmp_duration, s.idmp_maxsize, s.iids_added, s.iids_duplicates), (300, 50, 4, 1));
    assert_eq!(s.idmp.producers().map(|(_, p)| p.len()).collect::<Vec<_>>(), [2, 1]);
    // Long after the window, IIDs are dropped on load and on save.
    let later = CAPTURED_AT + 301_000;
    let v = restore_at(&redis("x_idmp"), &cfg, later).unwrap();
    let Value::Stream(s) = &v else { panic!() };
    assert!(s.idmp.is_empty());
    let v = restore_at(&redis("x_idmp"), &cfg, CAPTURED_AT).unwrap();
    let late = dump(&v, later);
    let Value::Stream(s) = restore_at(&late, &cfg, CAPTURED_AT).unwrap() else { panic!() };
    assert!(s.idmp.is_empty());
}

#[test]
fn hash_field_ttls_register_subexpiry() {
    let v = restore_at(&redis("h_lpex"), &cfg(), CAPTURED_AT).unwrap();
    let Value::Hash(h) = &v else { panic!() };
    assert_eq!(h.subexpiry(), 4102444800000);
    let order: Vec<(&[u8], i64)> = h.iter().map(|(f, _, at)| (f, at)).collect();
    // A field given the same expiry later goes first (`listpackExAddInternal`).
    assert_eq!(order, [(&b"d"[..], 4102444800000), (b"b", 4102444800000), (b"a", 4102444801000), (b"c", 0)]);
    let v = restore_at(&redis("h_htex"), &cfg(), CAPTURED_AT).unwrap();
    let Value::Hash(h) = &v else { panic!() };
    assert_eq!((h.subexpiry(), h.expire_of(b"field:9")), (4102444800000, Some(4102444900000)));
}

#[test]
fn conversions_follow_config() {
    // Restoring with small limits converts the way rdbLoadObject does.
    let tiny = Config {
        hash_max_listpack_entries: 2,
        set_max_listpack_entries: 2,
        set_max_intset_entries: 2,
        zset_max_listpack_entries: 2,
        ..Config::default()
    };
    for (name, enc) in [
        ("h_lp", "hashtable"),
        ("h_lpex", "hashtable"),
        ("set_lp", "hashtable"),
        ("set_int16", "hashtable"),
        ("z_lp", "skiplist"),
        ("l_small", "listpack"),
    ] {
        let v = restore_at(&redis(name), &tiny, CAPTURED_AT).unwrap();
        assert_eq!(v.encoding(), enc, "{name}");
    }
    // And large encodings shrink back when they fit (types that are rebuilt on load).
    let big = Config { zset_max_listpack_entries: 1000, ..cfg() };
    assert_eq!(restore_at(&redis("z_skip"), &big, CAPTURED_AT).unwrap().encoding(), "listpack");
    let one = Config { list_max_listpack_size: -5, ..cfg() };
    assert_eq!(restore_at(&redis("l_large"), &one, CAPTURED_AT).unwrap().encoding(), "quicklist");
}

fn obj(t: u8, body: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>)) -> Vec<u8> {
    let (mut o, mut z) = (vec![t], Vec::new());
    body(&mut o, &mut z);
    payload(&o)
}

fn listpack(items: &[Item]) -> Vec<u8> {
    let mut b = Vec::new();
    let mut w = LpWriter::new(&mut b);
    for it in items {
        match *it {
            Item::Int(v) => w.int(v),
            Item::Str(s) => w.str(s),
        }
    }
    w.finish().to_vec()
}

#[test]
fn legacy_types_load() {
    let c = cfg();
    let strs = |o: &mut Vec<u8>, z: &mut Vec<u8>, xs: &[&[u8]]| xs.iter().for_each(|x| put_string(o, x, z));
    // RDB_TYPE_LIST
    let p = obj(ty::LIST, |o, z| {
        put_len(o, 3);
        strs(o, z, &[b"a", b"12", b"c"]);
    });
    let v = restore(&p, &c).unwrap();
    assert_eq!(contents(&v), r#"[[97], [49, 50], [99]]"#);
    assert_eq!(v.encoding(), "listpack");
    // RDB_TYPE_SET: integers stay an intset, a string makes it a listpack.
    let p = obj(ty::SET, |o, z| {
        put_len(o, 3);
        strs(o, z, &[b"3", b"1", b"2"]);
    });
    assert_eq!(restore(&p, &c).unwrap().encoding(), "intset");
    let p = obj(ty::SET, |o, z| {
        put_len(o, 3);
        strs(o, z, &[b"3", b"x", b"3"]);
    });
    assert!(matches!(restore(&p, &c), Err(RdbError::Corrupt(_))));
    // RDB_TYPE_ZSET with ASCII scores and the special codes.
    let p = obj(ty::ZSET, |o, z| {
        put_len(o, 3);
        put_string(o, b"a", z);
        o.extend_from_slice(&[3, b'1', b'.', b'5']);
        put_string(o, b"b", z);
        o.push(255);
        put_string(o, b"c", z);
        o.push(254);
    });
    let v = restore(&p, &c).unwrap();
    let Value::ZSet(zs) = &v else { panic!() };
    assert_eq!(zs.iter().map(|(_, s)| s).collect::<Vec<_>>(), [f64::NEG_INFINITY, 1.5, f64::INFINITY]);
    let p = obj(ty::ZSET, |o, z| {
        put_len(o, 1);
        put_string(o, b"a", z);
        o.push(253);
    });
    assert!(restore(&p, &c).is_err(), "NaN score");
    // RDB_TYPE_HASH, converted when a value is too long.
    let long = vec![b'v'; 100];
    let p = obj(ty::HASH, |o, z| {
        put_len(o, 2);
        strs(o, z, &[b"f", b"1", b"g", &long]);
    });
    assert_eq!(restore(&p, &c).unwrap().encoding(), "hashtable");
    // Zipmap.
    let zm = [2u8, 1, b'a', 1, 0, b'1', 2, b'b', b'b', 3, 2, b'x', b'y', b'z', 0, 0, 0xff];
    let p = obj(ty::HASH_ZIPMAP, |o, z| put_string(o, &zm, z));
    let v = restore(&p, &c).unwrap();
    assert_eq!(contents(&v), r#"listpack [97]=[49]@0,[98, 98]=[120, 121, 122]@0"#);
    // Ziplists: list, zset and hash flavours, and a quicklist of ziplists.
    let zl = super::ziplist::tests::build(&[Item::Str(b"x"), Item::Int(5), Item::Str(b"y"), Item::Str(b"2.5")]);
    let v = restore(&obj(ty::LIST_ZIPLIST, |o, z| put_string(o, &zl, z)), &c).unwrap();
    assert_eq!((v.encoding(), contents(&v).as_str()), ("quicklist", "[[120], [53], [121], [50, 46, 53]]"));
    let v = restore(&obj(ty::ZSET_ZIPLIST, |o, z| put_string(o, &zl, z)), &c).unwrap();
    assert_eq!(contents(&v), r#"[([121], 2.5), ([120], 5.0)]"#);
    let v = restore(&obj(ty::HASH_ZIPLIST, |o, z| put_string(o, &zl, z)), &c).unwrap();
    assert_eq!(v.encoding(), "listpack");
    let v = restore(
        &obj(ty::LIST_QUICKLIST, |o, z| {
            put_len(o, 2);
            put_string(o, &zl, z);
            put_string(o, &zl, z);
        }),
        &c,
    )
    .unwrap();
    // Several nodes stay a quicklist (only a single small node becomes a listpack).
    assert_eq!((v.encoding(), contents(&v).len() > 10), ("quicklist", true));
    // Pre-GA hash field TTLs: absolute times, no min-expire prefix.
    let p = obj(ty::HASH_METADATA_PRE_GA, |o, z| {
        put_len(o, 2);
        put_len(o, 0);
        strs(o, z, &[b"a", b"1"]);
        put_len(o, 4102444800000);
        strs(o, z, &[b"b", b"2"]);
    });
    let Value::Hash(h) = restore(&p, &c).unwrap() else { panic!() };
    assert_eq!((h.is_listpack_ex(), h.expire_of(b"b"), h.subexpiry()), (true, Some(4102444800000), 4102444800000));
    let lp = listpack(&[Item::Str(b"a"), Item::Str(b"1"), Item::Int(4102444800000), Item::Str(b"b"), Item::Str(b"2"), Item::Int(0)]);
    let Value::Hash(h) = restore(&obj(ty::HASH_LISTPACK_EX_PRE_GA, |o, z| put_string(o, &lp, z)), &c).unwrap() else {
        panic!()
    };
    assert_eq!(h.expire_of(b"a"), Some(4102444800000));
    // Expiries out of order are rejected.
    let lp = listpack(&[Item::Str(b"a"), Item::Str(b"1"), Item::Int(0), Item::Str(b"b"), Item::Str(b"2"), Item::Int(9)]);
    assert!(restore(&obj(ty::HASH_LISTPACK_EX_PRE_GA, |o, z| put_string(o, &lp, z)), &c).is_err());
}

/// Rewrite a type-27 stream payload without groups or IDMP producers into an older layout.
fn old_stream(p27: &[u8], t: u8, groups: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>)) -> Vec<u8> {
    // Trailing metadata of the source (all small values): length, last, first, max-deleted,
    // entries-added, 0 groups, idmp duration/max (2 bytes each), 0 producers, iids x2.
    let obj = &p27[..p27.len() - 10];
    let meta = &obj[obj.len() - 16..];
    let mut o = obj[..obj.len() - 16].to_vec();
    o[0] = t;
    o.extend_from_slice(&meta[..3]); // length, last
    if t >= ty::STREAM_LISTPACKS_2 {
        o.extend_from_slice(&meta[3..8]); // first, max deleted, entries added
    }
    let mut z = Vec::new();
    groups(&mut o, &mut z);
    payload(&o)
}

#[test]
fn old_stream_layouts_load() {
    let c = cfg();
    let p = redis("x_plain");
    for t in [ty::STREAM_LISTPACKS, ty::STREAM_LISTPACKS_2] {
        let v = restore(&old_stream(&p, t, |o, _| put_len(o, 0)), &c).unwrap();
        let Value::Stream(s) = &v else { panic!() };
        assert_eq!((s.entries.len(), s.entries.last_id()), (3, StreamId::new(3, 5)));
        assert_eq!(s.entries.first_id(), StreamId::new(1, 1));
        if t == ty::STREAM_LISTPACKS {
            assert_eq!((s.entries.entries_added(), s.entries.max_deleted_id()), (3, StreamId::default()));
        }
    }
    // A group in the v3 layout (active time, no NACK zone), and one in v1 (no entries_read).
    let group = |o: &mut Vec<u8>, z: &mut Vec<u8>, t: u8| {
        put_len(o, 1);
        put_string(o, b"g", z);
        put_len(o, 2);
        put_len(o, 0);
        if t >= ty::STREAM_LISTPACKS_2 {
            put_len(o, 2);
        }
        put_len(o, 1);
        o.extend_from_slice(&StreamId::new(2, 0).to_be_bytes());
        o.extend_from_slice(&1000i64.to_le_bytes());
        put_len(o, 3);
        put_len(o, 1);
        put_string(o, b"alice", z);
        o.extend_from_slice(&900i64.to_le_bytes());
        if t >= ty::STREAM_LISTPACKS_3 {
            o.extend_from_slice(&950i64.to_le_bytes());
        }
        put_len(o, 1);
        o.extend_from_slice(&StreamId::new(2, 0).to_be_bytes());
    };
    for t in [ty::STREAM_LISTPACKS, ty::STREAM_LISTPACKS_3] {
        let v = restore(&old_stream(&p, t, |o, z| group(o, z, t)), &c).unwrap();
        let Value::Stream(s) = &v else { panic!() };
        let g = s.groups.get(b"g").unwrap();
        let alice = g.consumer(b"alice").unwrap();
        assert_eq!((alice.pending(), alice.seen_time), (1, 900));
        assert_eq!(alice.active_time, if t == ty::STREAM_LISTPACKS { 900 } else { 950 });
        assert_eq!(g.nack(StreamId::new(2, 0)).unwrap().delivery_count, 3);
        // v1 has no entries_read: it is estimated (and unknown here).
        assert_eq!(g.entries_read, if t == ty::STREAM_LISTPACKS { -1 } else { 2 });
    }
}

#[test]
fn rejects_bad_payloads() {
    let c = cfg();
    let mut p = redis("s_small");
    let n = p.len();
    p[n - 1] ^= 1;
    assert!(matches!(restore(&p, &c), Err(RdbError::Checksum)));
    let mut p = redis("s_small");
    p[n - 10] = 16;
    assert!(matches!(restore(&p, &c), Err(RdbError::Version(16))));
    assert!(matches!(restore(b"short", &c), Err(RdbError::Checksum)));
    // A zero CRC is not checked.
    let mut p = redis("s_small");
    p[n - 8..].fill(0);
    assert!(restore(&p, &c).is_ok());
    // Modules and other things this engine can't hold.
    for t in [ty::MODULE_PRE_GA, ty::GCRA] {
        assert!(matches!(restore(&payload(&[t, 0]), &c), Err(RdbError::Unsupported(_))), "type {t}");
    }
    // Template hashes referencing a file's template record can't be in a DUMP payload.
    for t in [ty::HASH_TMPL_LP_REF, ty::HASH_TMPL_ARRAY_REF] {
        assert!(matches!(restore(&payload(&[t, 0]), &c), Err(RdbError::Corrupt(_))), "type {t}");
    }
    assert!(matches!(restore(&payload(&[8, 0]), &c), Err(RdbError::Corrupt(_))));
    // An empty array is corrupt (not an empty key), as is a bad cursor flag or element tag.
    assert!(matches!(restore(&payload(&[ty::ARRAY, 0]), &c), Err(RdbError::Corrupt(_))));
    assert!(matches!(restore(&payload(&[ty::ARRAY, 1, 2]), &c), Err(RdbError::Corrupt(_))));
    assert!(matches!(restore(&payload(&[ty::ARRAY, 1, 0, 0, 9]), &c), Err(RdbError::Corrupt(_))));
    let Value::Array(a) = restore(&payload(&[ty::ARRAY, 1, 1, 7, 3, 3, 2, b'h', b'i']), &c).unwrap() else { panic!() };
    assert_eq!((a.count(), a.insert_idx(), a.get(3).map(|v| v.text().into_owned())), (1, 7, Some(b"hi".to_vec())));
    // Empty aggregates.
    for t in [ty::LIST, ty::SET, ty::ZSET_2, ty::HASH, ty::LIST_QUICKLIST_2] {
        assert!(matches!(restore(&payload(&[t, 0]), &c), Err(RdbError::EmptyKey)), "type {t}");
    }
}

/// Every truncation and thousands of random corruptions (with a valid CRC, so they reach the
/// decoder) must fail cleanly or decode to something that re-encodes: never panic.
#[test]
fn fuzz_never_panics() {
    let c = cfg();
    let mut rng = Rng::seeded(7);
    for &(name, hex) in REDIS {
        let p = unhex(hex);
        let obj = &p[..p.len() - 10];
        let step = (obj.len() / 300).max(1);
        for cut in (0..obj.len()).step_by(step) {
            // Reading may run into the footer (as in Redis): only a clean result is required.
            let _ = restore_at(&payload(&obj[..cut]), &c, CAPTURED_AT);
            let mut d = Dec::new(SliceSrc::new(&obj[..cut]), &c, CAPTURED_AT, RDB_VERSION);
            assert!(d.byte().and_then(|t| d.object(t)).is_err(), "{name} cut at {cut}");
        }
        for _ in 0..300 {
            let mut o = obj.to_vec();
            for _ in 0..1 + rng.below(4) {
                let i = rng.below(o.len());
                o[i] = rng.next_u64() as u8;
            }
            if let Ok(v) = restore_at(&payload(&o), &c, CAPTURED_AT) {
                let again = dump(&v, CAPTURED_AT);
                restore_at(&again, &c, CAPTURED_AT).unwrap_or_else(|e| panic!("{name}: re-dump fails: {e}"));
            }
        }
    }
    // Pure noise behind every type byte.
    for t in 0..=255u8 {
        for _ in 0..50 {
            let mut o = vec![t];
            o.extend((0..rng.below(64)).map(|_| rng.next_u64() as u8));
            let _ = restore(&payload(&o), &c);
        }
    }
    // Huge claimed lengths with no data behind them.
    for t in [ty::STRING, ty::LIST, ty::SET, ty::HASH, ty::ZSET_2, ty::STREAM_LISTPACKS_5] {
        let o = [t, 0x81, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xf0];
        assert!(restore(&payload(&o), &c).is_err());
    }
    let lzf_bomb = [ty::STRING, 0xc3, 0x01, 0x81, 0, 0, 0, 0x10, 0, 0, 0, 0, 0];
    assert!(restore(&payload(&lzf_bomb), &c).is_err());
}

#[test]
fn save_and_load_multiple_dbs() {
    let (mut e, mut c) = engine();
    for (_, cmds) in commands() {
        for cmd in &cmds {
            let args: Vec<&[u8]> = cmd.iter().map(|a| a.as_bytes()).collect();
            call(&mut e, &mut c, &args);
        }
    }
    run(&mut e, &mut c, "PEXPIREAT s_small 4102444800000");
    run(&mut e, &mut c, "SELECT 3");
    run(&mut e, &mut c, "SET other 1");
    run(&mut e, &mut c, "PEXPIRE other 100000");
    run(&mut e, &mut c, "SELECT 15");
    run(&mut e, &mut c, "RPUSH last x");
    run(&mut e, &mut c, "SET gone x PX 1");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut file = Vec::new();
    let funcs = vec![b"#!lua name=lib\nredis.register_function('f', function() return 1 end)".to_vec()];
    save(e.dbs.iter().enumerate(), &[("extra", "yes")], &funcs, &mut file).unwrap();
    assert_eq!(&file[..9], b"REDIS0015");

    let mut got: Vec<(usize, Vec<u8>, i64, Vec<u8>)> = Vec::new();
    let mut aux = Vec::new();
    let mut fns = Vec::new();
    let info = load(&file[..], &cfg(), |it| {
        match it {
            LoadItem::Entry { db, key, entry } => got.push((db, key.to_vec(), entry.expire, dump(&entry.value, 0))),
            LoadItem::Aux { key, value } => aux.push((String::from_utf8_lossy(key).into_owned(), String::from_utf8_lossy(value).into_owned())),
            LoadItem::Function(f) => fns.push(f),
            LoadItem::ResizeDb { .. } | LoadItem::Untemplate { .. } => {}
        }
        Ok(())
    })
    .unwrap();
    assert_eq!((info.version, info.functions, info.expired), (15, 1, 0));
    assert_eq!(fns, funcs);
    let keys: Vec<&str> = aux.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["redis-ver", "redis-bits", "ctime", "used-mem", "extra", "aof-base"]);
    let total: usize = e.dbs.iter().map(|d| d.len()).sum();
    assert_eq!(got.len(), total - 1, "the expired key is skipped");
    for (db, key, expire, bytes) in &got {
        let entry = e.dbs[*db].keys.get(key).unwrap();
        assert_eq!(*expire, entry.expire);
        if !UNORDERED.contains(&std::str::from_utf8(key).unwrap()) {
            assert_eq!(*bytes, dump(&entry.value, 0), "{}", String::from_utf8_lossy(key));
        }
    }
    assert!(got.iter().any(|(db, k, _, _)| *db == 15 && k == b"last"));

    // A corrupted byte fails the checksum (or the parse); a future version is refused.
    let mut bad = file.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x55;
    assert!(load(&bad[..], &cfg(), |_| Ok(())).is_err());
    let mut future = file.clone();
    future[5..9].copy_from_slice(b"0016");
    assert!(matches!(load(&future[..], &cfg(), |_| Ok(())), Err(RdbError::Version(16))));
    assert!(matches!(load(&b"RODIS0011"[..], &cfg(), |_| Ok(())), Err(RdbError::Signature)));
    for cut in [10, file.len() / 3, file.len() - 1] {
        assert!(load(&file[..cut], &cfg(), |_| Ok(())).is_err());
    }

    // The low-level writer: repeated database indexes, LRU info.
    let mut w = RdbWriter::new(Vec::new(), &[], 0).unwrap();
    let entry = Entry::new(Value::str(b"v"), 0);
    w.begin_db(0, 1, 0).unwrap();
    w.entry(b"a", &entry, Lru::Idle(30)).unwrap();
    w.begin_db(0, 1, 0).unwrap();
    w.entry(b"b", &entry, Lru::Freq(9)).unwrap();
    let file = w.finish().unwrap();
    let lfu = Config { maxmemory_policy: crate::config::Policy::AllKeysLfu, ..cfg() };
    let mut lrus = Vec::new();
    load(&file[..], &lfu, |it| {
        if let LoadItem::Entry { db, entry, .. } = it {
            lrus.push((db, entry.lru & 255));
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(lrus, [(0, 5), (0, 9)]);
}

#[test]
fn restore_command() {
    let (mut e, mut c) = engine();
    let dumped = |e: &mut Engine, c: &mut Client, k: &str| match run(e, c, &format!("DUMP {k}")) {
        Reply::Bulk(b) => b,
        r => panic!("{r:?}"),
    };
    run(&mut e, &mut c, "RPUSH l a b c");
    let p = dumped(&mut e, &mut c, "l");
    assert_eq!(run(&mut e, &mut c, "DUMP nope"), Reply::Nil);
    let restore = |e: &mut Engine, c: &mut Client, rest: &[&[u8]]| {
        let mut args: Vec<&[u8]> = vec![b"RESTORE"];
        args.extend_from_slice(rest);
        call(e, c, &args)
    };
    let err = |r: Reply| match r {
        Reply::Error(e) => e,
        r => panic!("{r:?}"),
    };
    assert!(err(restore(&mut e, &mut c, &[b"l", b"0", &p])).starts_with("BUSYKEY"));
    e.take_effects();
    assert_eq!(restore(&mut e, &mut c, &[b"l2", b"0", &p]), Reply::Ok);
    assert_eq!(run(&mut e, &mut c, "LRANGE l2 0 -1"), Reply::Array(vec![Reply::bulk("a"), Reply::bulk("b"), Reply::bulk("c")]));
    // A relative TTL is replicated as an absolute one.
    assert_eq!(restore(&mut e, &mut c, &[b"l3", b"100000", &p]), Reply::Ok);
    let fx = e.take_effects();
    let last = &fx.last().unwrap().args;
    assert_eq!((&last[0][..], &last[3][..], &last[4][..]), (&b"RESTORE"[..], &p[..], &b"ABSTTL"[..]));
    let at: i64 = std::str::from_utf8(&last[2]).unwrap().parse().unwrap();
    assert!(at > crate::util::now_ms());
    // Already expired: with REPLACE the old key is deleted (and a DEL replicated).
    assert_eq!(restore(&mut e, &mut c, &[b"l3", b"1", &p, b"REPLACE", b"ABSTTL"]), Reply::Ok);
    assert_eq!(run(&mut e, &mut c, "EXISTS l3"), Reply::Int(0));
    assert_eq!(&e.take_effects().last().unwrap().args[0][..], b"DEL");
    // Option errors.
    assert_eq!(err(restore(&mut e, &mut c, &[b"x", b"-1", &p])), "ERR Invalid TTL value, must be >= 0");
    assert_eq!(err(restore(&mut e, &mut c, &[b"x", b"0", &p, b"IDLETIME", b"-1"])), "ERR Invalid IDLETIME value, must be >= 0");
    assert_eq!(err(restore(&mut e, &mut c, &[b"x", b"0", &p, b"FREQ", b"256"])), "ERR Invalid FREQ value, must be >= 0 and <= 255");
    assert_eq!(err(restore(&mut e, &mut c, &[b"x", b"0", &p, b"FREQ", b"1", b"IDLETIME", b"1"])), "ERR syntax error");
    assert_eq!(err(restore(&mut e, &mut c, &[b"x", b"0", b"garbage"])), "ERR DUMP payload version or checksum are wrong");
    let mut bad = p.to_vec();
    bad[1] = 0x3f;
    let n = bad.len();
    bad[n - 8..].fill(0);
    assert_eq!(err(restore(&mut e, &mut c, &[b"x", b"0", &bad])), "ERR Bad data format");
    // IDLETIME sets the LRU clock.
    assert_eq!(restore(&mut e, &mut c, &[b"idle", b"0", &p, b"IDLETIME", b"1000"]), Reply::Ok);
    let Reply::Int(idle) = run(&mut e, &mut c, "OBJECT IDLETIME idle") else { panic!() };
    assert!((1000..1003).contains(&idle), "{idle}");
}

/// A vector set DUMPed by Redis 8.10 (three Q8 elements, one with attributes, all at level 0):
/// loads with its links and vectors, answers like Redis did, and saves back the same bytes once
/// the list order (reversed by a load) is reversed again, links sorted by id as a load leaves
/// them.
#[test]
fn vector_set_payload_from_redis() {
    const P: &str = "0781bde72da2bb1eb40002030203025001020205016305000503c1207f02080203028001000000020202200202020102813f8b9d850000000102813f5f74823f6aa0bd050162050005030e7f2a02080202028001000000020202200201020302813f10b03d0000000102813f71865b3f74355c05016105077b2278223a317d05037f3f2002080201028001000000020202200202020302813f8b9d850000000102813f5f74823f92a476000f0081e62de6bd62de68";
    let p = unhex(P);
    let (mut e, mut c) = engine();
    assert_eq!(call(&mut e, &mut c, &[b"RESTORE", b"v", b"0", &p]), Reply::Ok);
    assert_eq!(run(&mut e, &mut c, "VCARD v"), Reply::Int(3));
    assert_eq!(run(&mut e, &mut c, "VGETATTR v a"), Reply::bulk("{\"x\":1}"));
    assert_eq!(
        run(&mut e, &mut c, "VSIM v ELE a WITHSCORES"),
        Reply::Array(vec![
            Reply::bulk("a"),
            Reply::Double(0.999440977233462),
            Reply::bulk("b"),
            Reply::Double(0.7842775285243988),
            Reply::bulk("c"),
            Reply::Double(0.45457690954208374),
        ])
    );
    let Reply::Array(raw) = run(&mut e, &mut c, "VEMB v c RAW") else { panic!() };
    assert_eq!(raw[2], Reply::Double(0.9165151715278625));
    assert_eq!(raw[3], Reply::Double(0.8728715181350708));
    let Reply::Array(links) = run(&mut e, &mut c, "VLINKS v a") else { panic!() };
    assert_eq!(links, vec![Reply::Array(vec![Reply::bulk("b"), Reply::bulk("c")])]);
    let Value::VSet(v) = restore(&p, &cfg()).unwrap() else { panic!() };
    let again = restore(&dump(&Value::VSet(v), 0), &cfg()).unwrap();
    // Element c's links were saved as ids [2, 1].
    let mut sorted = p[..p.len() - 10].to_vec();
    let at = sorted.windows(4).position(|w| w == [2, 2, 2, 1]).unwrap();
    sorted[at..at + 4].copy_from_slice(&[2, 1, 2, 2]);
    assert_eq!(dump(&again, 0), payload(&sorted));
}
