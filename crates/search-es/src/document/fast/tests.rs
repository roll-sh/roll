//! The fast path against the general path: on every document the fast path accepts, both
//! produce the same [`ParsedDocument`]; and it accepts the plain documents it is built for.

use super::*;
use crate::meta::settings::Settings;

fn index(mapping: &str, settings: &[(&str, &str)]) -> IndexMeta {
    let mut s = Settings::new();
    for (k, v) in settings {
        s.set(*k, *v);
    }
    let ctx = MappingCtx::from_settings(&s);
    let v = crate::json::parse(mapping.as_bytes()).unwrap().unwrap();
    let m = crate::mapping::Mapping::parse(v.as_object().unwrap(), &ctx).unwrap();
    IndexMeta {
        name: "i".into(),
        uuid: "u".into(),
        state: crate::meta::IndexState::Open,
        settings: s,
        mapping: Arc::new(m),
        aliases: Default::default(),
        version: 1,
        mapping_version: 1,
        settings_version: 1,
        aliases_version: 1,
        routing_num_shards: 1024,
        primary_terms: vec![1],
        system: false,
        recovery_source: None,
        dialect: Default::default(),
    }
}

/// The general path's result for `src`.
fn general(idx: &IndexMeta, src: &[u8]) -> Option<ParsedDocument> {
    let v = crate::api::document::parse_doc(src).ok()?;
    let mut ctx = idx.mapping_ctx();
    ctx.join_routing = Some(false);
    super::super::parse_document(&idx.mapping, &ctx, &v, Some(src), Some("1")).ok()
}

/// Checks agreement on `src`; returns whether the fast path took it.
fn agree(idx: &IndexMeta, plan: &FastPlan, src: &str) -> bool {
    let fast = plan.parse(src.as_bytes());
    let Some(fast) = fast else { return false };
    let slow = general(idx, src.as_bytes()).unwrap_or_else(|| panic!("fast path accepted a document the general path rejects: {src}"));
    assert_eq!(fast, slow, "documents differ: {src}");
    true
}

const GEONAMES: &str = r#"{"dynamic":"strict","properties":{
    "elevation":{"type":"integer"},
    "name":{"type":"text","fields":{"raw":{"type":"keyword"}}},
    "geonameid":{"type":"long"},
    "feature_class":{"type":"text","fields":{"raw":{"type":"keyword"}}},
    "location":{"type":"geo_point"},
    "cc2":{"type":"text","fields":{"raw":{"type":"keyword"}}},
    "timezone":{"type":"text","fields":{"raw":{"type":"keyword"}}},
    "dem":{"type":"text","fields":{"raw":{"type":"keyword"}}},
    "country_code":{"type":"text","fielddata":true,"fields":{"raw":{"type":"keyword"}}},
    "admin1_code":{"type":"text","fields":{"raw":{"type":"keyword"}}},
    "population":{"type":"long"},
    "alternatenames":{"type":"text","fields":{"raw":{"type":"keyword","ignore_above":20}}}
}}"#;

#[test]
fn geonames_documents_take_the_fast_path() {
    let idx = index(GEONAMES, &[]);
    let plan = FastPlan::new(&idx).expect("plan");
    assert_eq!(plan.source, SourcePlan::Keep);
    for doc in [
        r#"{"geonameid": 2986043, "name": "Pic de Font Blanca", "alternatenames": "Pic de Font Blanca,Pic du Port", "feature_class": "T", "country_code": "AD", "admin1_code": "00", "population": 0, "dem": "2860", "timezone": "Europe/Andorra", "location": [1.53335, 42.64991]}"#,
        r#"{"geonameid":1,"name":"Zürich \"Zurich\"","cc2":["AD","FR"],"elevation":-12,"location":[-180,90]}"#,
        "{\"name\": \"Ünïcödé 名前\", \"alternatenames\": \"short\", \"dem\": 7 , \"cc2\": [[\"a\"], [], \"b\", true]}\n",
        r#"{}"#,
        r#"  { "elevation" : 2147483647 , "population" : -9223372036854775807 }  "#,
    ] {
        assert!(agree(&idx, &plan, doc), "not taken: {doc}");
    }
}

const HTTP_LOGS: &str = r#"{"dynamic":"strict","_source":{"enabled":true},"properties":{
    "@timestamp":{"format":"epoch_second","type":"date"},
    "message":{"type":"keyword","index":false,"doc_values":false},
    "clientip":{"type":"ip"},
    "request":{"type":"match_only_text","fields":{"raw":{"ignore_above":256,"type":"keyword"}}},
    "status":{"type":"integer"},
    "size":{"type":"integer"},
    "geoip":{"properties":{"country_name":{"type":"keyword"},"city_name":{"type":"keyword"},"location":{"type":"geo_point"}}}
}}"#;

/// Rally's `http_logs` documents (an `ip`, a `match_only_text` with a keyword multi-field).
#[test]
fn http_logs_documents_take_the_fast_path() {
    let idx = index(HTTP_LOGS, &[]);
    let plan = FastPlan::new(&idx).expect("plan");
    for doc in [
        r#"{"@timestamp": 893964617, "clientip":"40.135.0.0", "request": "GET /images/hm_bg.jpg HTTP/1.0", "status": 200, "size": 24736}"#,
        r#"{"@timestamp": 894146400, "clientip":"2001:db8::1", "request": "GET /english/index.html HTTP/1.0", "status": 304, "size": 0, "message": "m"}"#,
    ] {
        assert!(agree(&idx, &plan, doc), "not taken: {doc}");
    }
    // Plain objects are walked too, in document order, with their fields' full names.
    for doc in [
        r#"{"geoip": {"city_name": "x"}}"#,
        r#"{"status": 200, "geoip": {"country_name": "NL", "location": [4.9, 52.37], "city_name": ["a", "b"]}, "size": 1}"#,
        r#"{"geoip": {}}"#,
        r#"{"geoip": [{"city_name": "x"}, {"country_name": "NL", "city_name": "y"}]}"#,
        r#"{"geoip": [[{"city_name": "x"}], [], {"city_name": "z"}]}"#,
        r#"{"geoip": []}"#,
    ] {
        assert!(agree(&idx, &plan, doc), "not taken: {doc}");
    }
    // Past `index.mapping.array_objects.limit` the general path fails the document.
    let limited = index(HTTP_LOGS, &[("index.mapping.array_objects.limit", "1")]);
    let plan1 = FastPlan::new(&limited).expect("plan");
    assert!(agree(&limited, &plan1, r#"{"geoip": [{"city_name": "x"}]}"#));
    assert!(plan1.parse(br#"{"geoip": [{"city_name": "x"}, {"city_name": "y"}]}"#).is_none());
    // Malformed addresses stay with the general path (its errors), and so do objects it does
    // more with: null (also in an array), unknown or repeated keys, a value where an object is.
    for doc in [
        r#"{"clientip": "1.2.3"}"#,
        r#"{"clientip": 1}"#,
        r#"{"geoip": null}"#,
        r#"{"geoip": [{"city_name": "x"}, null]}"#,
        r#"{"geoip": [{"city_name": "x"}, "y"]}"#,
        r#"{"geoip": {"zip": "x"}}"#,
        r#"{"geoip": {"city_name": "x", "city_name": "y"}}"#,
        r#"{"geoip": "x"}"#,
    ] {
        assert!(plan.parse(doc.as_bytes()).is_none(), "taken: {doc}");
    }
}

const NYC_TAXIS: &str = r#"{"dynamic":"strict","properties":{
    "total_amount":{"type":"scaled_float","scaling_factor":100},
    "trip_distance":{"type":"scaled_float","scaling_factor":100},
    "fare_amount":{"type":"double"},
    "pickup_datetime":{"type":"date","format":"yyyy-MM-dd HH:mm:ss"},
    "pickup_location":{"type":"geo_point"},
    "passenger_count":{"type":"integer"},
    "vendor_id":{"type":"keyword"},
    "f":{"type":"float"},
    "h":{"type":"half_float","fields":{"raw":{"type":"keyword"}}},
    "s":{"type":"scaled_float","scaling_factor":10,"fields":{"f":{"type":"float"}}}
}}"#;

/// Rally's `nyc_taxis` documents: `scaled_float`, `float` and `half_float` values take the
/// fast path and come out as the general path's (Java rounding, float narrowing); values the
/// general path refuses stay with it.
#[test]
fn nyc_taxis_documents_take_the_fast_path() {
    let idx = index(NYC_TAXIS, &[]);
    let plan = FastPlan::new(&idx).expect("plan");
    for doc in [
        r#"{"total_amount": 6.3, "pickup_location": [-73.92259216308594, 40.7545280456543], "pickup_datetime": "2015-01-01 00:34:42", "passenger_count": 1, "fare_amount": 5.0, "trip_distance": 0.88, "vendor_id": "2"}"#,
        r#"{"total_amount": -22.5, "trip_distance": 0.005, "s": 2.25, "f": 0.1, "h": 0.1}"#,
        r#"{"total_amount": 1e-300, "s": -0.05, "f": 16777217, "h": 65504, "trip_distance": [1, 2.675, -0.0]}"#,
        r#"{"f": 3.4028235e38, "h": 1e-8, "s": 9.2e15}"#,
    ] {
        assert!(agree(&idx, &plan, doc), "not taken: {doc}");
    }
    // Past a long, past a float or a half float, or a string: the general path's errors (or
    // coercion).
    for doc in [r#"{"total_amount": 1e300}"#, r#"{"f": 1e39}"#, r#"{"h": 65520}"#, r#"{"total_amount": "6.3"}"#] {
        assert!(plan.parse(doc.as_bytes()).is_none(), "taken: {doc}");
    }
}

#[test]
fn everything_else_takes_the_general_path() {
    let idx = index(GEONAMES, &[]);
    let plan = FastPlan::new(&idx).unwrap();
    for doc in [
        r#"{"unknown": 1}"#,
        r#"{"name": null}"#,
        r#"{"name": "a", "name": "b"}"#,
        r#"{"name": "a"} trailing"#,
        r#"{"name": "a" /* c */}"#,
        r#"{"elevation": 2147483648}"#,
        r#"{"elevation": 1.5}"#,
        r#"{"elevation": "12"}"#,
        r#"{"population": 99999999999999999999}"#,
        r#"{"location": [181, 0]}"#,
        r#"{"location": [1, 2, 3]}"#,
        r#"{"location": "1,2"}"#,
        r#"{"location": {"lat": 1, "lon": 2}}"#,
        r#"{"name": {"a": 1}}"#,
        r#"{"_id": "x"}"#,
        "{\"n\\u0061me\": \"x\"}",
        r#"{"name": "bad"#,
        r#"{"name": 01}"#,
        r#"{"name": truex}"#,
        r#"[1]"#,
        "{\"name\": \"tab\there\"}",
    ] {
        assert!(plan.parse(doc.as_bytes()).is_none(), "taken: {doc}");
    }
}

#[test]
fn ignore_above_records_ignored_fields() {
    let idx = index(GEONAMES, &[]);
    let plan = FastPlan::new(&idx).unwrap();
    let doc = r#"{"alternatenames": ["a very long alternate name list", "x"], "name": "n"}"#;
    assert!(agree(&idx, &plan, doc));
    assert_eq!(plan.parse(doc.as_bytes()).unwrap().ignored, vec!["alternatenames.raw".to_string()]);
}

const TYPES: &str = r#"{"properties":{
    "t":{"type":"text"},
    "k":{"type":"keyword","fields":{"t":{"type":"text"},"n":{"type":"long"}}},
    "kn":{"type":"keyword","ignore_above":4},
    "l":{"type":"long","fields":{"k":{"type":"keyword"}}},
    "i":{"type":"integer"},
    "s":{"type":"short"},
    "b":{"type":"byte"},
    "d":{"type":"double","fields":{"k":{"type":"keyword"}}},
    "bo":{"type":"boolean","fields":{"k":{"type":"keyword"}}},
    "dt":{"type":"date"},
    "de":{"type":"date","format":"epoch_second"},
    "dn":{"type":"date_nanos"},
    "dk":{"type":"keyword","fields":{"d":{"type":"date","format":"epoch_millis"}}},
    "g":{"type":"geo_point"},
    "f":{"type":"float"},
    "o":{"properties":{"x":{"type":"keyword"}}},
    "o2":{"properties":{"p":{"properties":{"y":{"type":"long"}}},"z":{"type":"scaled_float","scaling_factor":10}}},
    "c":{"type":"keyword","copy_to":"t"},
    "ip":{"type":"ip"},
    "mot":{"type":"match_only_text","fields":{"raw":{"type":"keyword","ignore_above":4}}}
}}"#;

/// A deterministic generator (xorshift) instead of a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

fn scalar(r: &mut Rng) -> String {
    let strings = [
        r#""""#,
        r#""abc""#,
        r#""a b c d e""#,
        r#""12""#,
        r#""-3""#,
        r#""2020-01-01""#,
        r#""2020-01-01T10:00:00.123456789Z""#,
        r#""1577836800""#,
        r#""é名""#,
        r#""q\"uote\\""#,
        r#""é😀""#,
        r#""\ud800""#,
        r#""true""#,
        r#""bad date""#,
        r#""192.168.0.1""#,
        r#""::1""#,
        r#""2001:db8::ff00:42:8329""#,
        r#""::ffff:10.0.0.1""#,
        r#""1.2.3""#,
        r#""010.0.0.1""#,
        r#""fe80::1%eth0""#,
    ];
    let nums = [
        "0", "-0", "1", "-1", "127", "128", "-129", "32767", "32768", "2147483647", "-2147483649", "9223372036854775807", "-9223372036854775808",
        "18446744073709551615", "1.0", "1.5", "-2.25", "1e3", "1E-3", "1e400", "-0.0", "1577836800", "1577836800.5", "0.1", "3.4028235e38",
        "1.10", "1E5", "1.5E+3", "10000000.0", "12345678.9", "-0e0",
    ];
    match r.next() % 4 {
        0 => r.pick(&strings).to_string(),
        1 => r.pick(&nums).to_string(),
        2 => r.pick(&["true", "false", "null"]).to_string(),
        _ => r.pick(&["[1.5, 2.5]", "[-200, 0]", "[0, 95]", "[1, 2, 3]", "\"1,2\"", "{\"x\": \"a\"}", "[]", "[\"a\", [\"b\"]]", "[1, \"x\"]", "{\"x\": [\"a\", 1]}", "{\"x\": null}", "{}", "{\"x\": \"a\", \"x\": \"b\"}", "{\"p\": {\"y\": 3}, \"z\": 1.25}", "{\"z\": [0.05, -2.25], \"p\": {}}", "{\"p\": {\"y\": 1.5}}", "[{\"x\": \"a\"}, {\"x\": \"b\"}]", "[{\"p\": {\"y\": 2}}, [{\"z\": 0.5}]]", "[{\"x\": \"a\"}, null]"]).to_string(),
    }
}

#[test]
fn random_documents_agree() {
    let idx = index(TYPES, &[]);
    let plan = FastPlan::new(&idx).unwrap();
    let keys = ["t", "k", "kn", "l", "i", "s", "b", "d", "bo", "dt", "de", "dn", "dk", "g", "f", "o", "o2", "c", "zz", "ip", "mot"];
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let seeds: u64 = std::env::var("FAST_DOCS").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let (mut fast, mut total) = (0, 0);
    for _ in 0..seeds {
        let n = r.next() % 5;
        let mut doc = String::from("{");
        for j in 0..n {
            if j > 0 {
                doc.push_str(", ");
            }
            let k = r.pick(&keys);
            let v = if r.next().is_multiple_of(4) { format!("[{}, {}]", scalar(&mut r), scalar(&mut r)) } else { scalar(&mut r) };
            doc.push_str(&format!("\"{k}\": {v}"));
        }
        doc.push('}');
        total += 1;
        if agree(&idx, &plan, &doc) {
            fast += 1;
        }
    }
    // Most generated documents carry something the fast path leaves alone, but a good share
    // must go through it for the comparison to mean anything.
    assert!(fast * 10 > total, "only {fast} of {total} took the fast path");
}

#[test]
fn plans_refuse_what_they_cannot_reproduce() {
    assert!(FastPlan::new(&index(r#"{"_source":{"mode":"synthetic"},"properties":{"k":{"type":"keyword"}}}"#, &[])).is_none());
    assert!(FastPlan::new(&index(r#"{"_source":{"excludes":["k"]},"properties":{"k":{"type":"keyword"}}}"#, &[])).is_none());
    let p = FastPlan::new(&index(r#"{"_source":{"enabled":false},"properties":{"k":{"type":"keyword"}}}"#, &[])).unwrap();
    assert_eq!(p.source, SourcePlan::Drop);
    // Index-level `ignore_above` reaches keyword fields.
    let idx = index(r#"{"properties":{"k":{"type":"keyword"}}}"#, &[("index.mapping.ignore_above", "3")]);
    let plan = FastPlan::new(&idx).unwrap();
    assert!(agree(&idx, &plan, r#"{"k": "abcd"}"#));
    assert_eq!(plan.parse(br#"{"k": "abcd"}"#).unwrap().ignored, vec!["k".to_string()]);
}

/// Numbers indexed as text keep their text as written, in both paths, for main fields,
/// multi-fields and `copy_to` targets; `ignore_above` counts UTF-16 units (an emoji is two).
/// Expected values from Elasticsearch 9.5.5 (terms aggregations and `_ignored` of this
/// document).
#[test]
fn numbers_index_as_written_like_elasticsearch() {
    let mapping = r#"{"properties":{"kk":{"type":"keyword"},"t":{"type":"text","fields":{"raw":{"type":"keyword"}}},
        "d":{"type":"double","fields":{"k":{"type":"keyword"}}},"c":{"type":"double","copy_to":"ck"},"ck":{"type":"keyword"},
        "l":{"type":"long","fields":{"k":{"type":"keyword"}}},"ia":{"type":"keyword","ignore_above":5}}}"#;
    let idx = index(mapping, &[]);
    let plan = FastPlan::new(&idx).unwrap();
    let keywords = |d: &ParsedDocument, name: &str| -> Vec<String> {
        let mut v: Vec<String> = d
            .fields
            .iter()
            .filter(|f| &*f.name == name)
            .filter_map(|f| match &f.value {
                FieldValue::Keyword(s) | FieldValue::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        v.sort();
        v
    };
    let src = r#"{"kk":[1.10,-0,1e2,10000000.0,123456789012345678901234567890,-0.0,1.5E+3],"t":1E5,"d":[12345678.9,1E5],"c":1.10,"l":[-0,7],"ia":["😀😀😀","abcde","😀😀"]}"#;
    let slow = general(&idx, src.as_bytes()).unwrap();
    assert_eq!(keywords(&slow, "kk"), ["-0", "-0.0", "1.10", "1.5E+3", "10000000.0", "123456789012345678901234567890", "1e2"]);
    assert_eq!(keywords(&slow, "t"), ["1E5"]);
    assert_eq!(keywords(&slow, "t.raw"), ["1E5"]);
    assert_eq!(keywords(&slow, "d.k"), ["12345678.9", "1E5"]);
    assert_eq!(keywords(&slow, "ck"), ["1.10"]);
    assert_eq!(keywords(&slow, "l.k"), ["-0", "7"]);
    assert_eq!(keywords(&slow, "ia"), ["abcde", "😀😀"]);
    assert_eq!(slow.ignored, vec!["ia".to_string()]);
    // The fast path (without the big integer it leaves to the general path, and `copy_to`).
    let src = r#"{"kk":[1.10,-0,1e2,10000000.0,-0.0,1.5E+3],"t":1E5,"d":[12345678.9,1E5],"l":[-0,7],"ia":["😀😀😀","abcde","😀😀"]}"#;
    assert!(agree(&idx, &plan, src), "not taken: {src}");
    let fast = plan.parse(src.as_bytes()).unwrap();
    assert_eq!(keywords(&fast, "kk"), ["-0", "-0.0", "1.10", "1.5E+3", "10000000.0", "1e2"]);
    assert_eq!(keywords(&fast, "d.k"), ["12345678.9", "1E5"]);
    assert_eq!(keywords(&fast, "ia"), ["abcde", "😀😀"]);
    assert_eq!(fast.ignored, vec!["ia".to_string()]);
}

/// Every check above holds in the opensearch dialect too (its general path is the reference
/// there: field names, points and dates parse by its rules).
#[test]
fn the_opensearch_dialect_agrees() {
    crate::opensearch::with(true, || {
        geonames_documents_take_the_fast_path();
        http_logs_documents_take_the_fast_path();
        nyc_taxis_documents_take_the_fast_path();
        everything_else_takes_the_general_path();
        ignore_above_records_ignored_fields();
        random_documents_agree();
        plans_refuse_what_they_cannot_reproduce();
        numbers_index_as_written_like_elasticsearch();
    });
}
