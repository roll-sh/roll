//! Document parsing unit tests.

use super::*;
use crate::query::geo::GeoPoint;

fn mapping(props: &str) -> (Mapping, MappingCtx) {
    let ctx = MappingCtx::from_settings(&crate::meta::settings::Settings::default());
    let v = crate::json::parse(props.as_bytes()).unwrap().unwrap();
    let m = Mapping::parse(v.as_object().unwrap(), &ctx).unwrap();
    (m, ctx)
}

#[test]
fn numbers_and_keywords() {
    let (m, ctx) = mapping(r#"{"properties":{"n":{"type":"long"},"k":{"type":"keyword","ignore_above":3}}}"#);
    let src = br#"{"n":["1","2"],"k":"abcd"}"#;
    let doc = crate::json::parse(src).unwrap().unwrap();
    let d = parse_document(&m, &ctx, &doc, Some(src), Some("1")).unwrap();
    assert_eq!(d.fields, vec![DocField { name: "n".into(), value: FieldValue::Long(1) }, DocField { name: "n".into(), value: FieldValue::Long(2) }]);
    assert_eq!(d.ignored, vec!["k".to_string()]);
}

fn parse(props: &str, src: &str) -> ParsedDocument {
    let (m, ctx) = mapping(props);
    let doc = crate::json::parse(src.as_bytes()).unwrap().unwrap();
    parse_document(&m, &ctx, &doc, Some(src.as_bytes()), Some("1")).unwrap()
}

fn values(d: &ParsedDocument, name: &str) -> Vec<FieldValue> {
    d.fields.iter().filter(|f| &*f.name == name).map(|f| f.value.clone()).collect()
}

#[test]
fn numeric_coercion() {
    let props = r#"{"properties":{"l":{"type":"long"},"i":{"type":"integer"},"h":{"type":"half_float"},
        "s":{"type":"scaled_float","scaling_factor":100},"u":{"type":"unsigned_long"},"b":{"type":"boolean"}}}"#;
    let d = parse(props, r#"{"l":["1.7",1.7,"1e3",-2],"i":"12","h":0.1,"s":1.234,"u":18446744073709551615,"b":["true",false]}"#);
    assert_eq!(values(&d, "l"), vec![FieldValue::Long(1), FieldValue::Long(1), FieldValue::Long(1000), FieldValue::Long(-2)]);
    assert_eq!(values(&d, "i"), vec![FieldValue::Long(12)]);
    assert_eq!(values(&d, "h"), vec![FieldValue::Float(0.099975586)]);
    assert_eq!(values(&d, "s"), vec![FieldValue::Long(123)]);
    assert_eq!(values(&d, "u"), vec![FieldValue::UnsignedLong(u64::MAX)]);
    assert_eq!(values(&d, "b"), vec![FieldValue::Boolean(true), FieldValue::Boolean(false)]);
}

#[test]
fn dates_ips_and_points() {
    let props = r#"{"properties":{"d":{"type":"date"},"n":{"type":"date_nanos"},"ip":{"type":"ip"},"g":{"type":"geo_point"}}}"#;
    let d = parse(props, r#"{"d":["2020-01-01",1577836800000],"n":"2020-01-01T00:00:00.000000001Z","ip":["10.0.0.1","::1"],"g":[[1.5,2.5],"3,4"]}"#);
    assert_eq!(values(&d, "d"), vec![FieldValue::Date(1_577_836_800_000), FieldValue::Date(1_577_836_800_000)]);
    assert_eq!(values(&d, "n"), vec![FieldValue::Date(1_577_836_800_000_000_001)]);
    let mut v4 = [0u8; 16];
    v4[10] = 0xff;
    v4[11] = 0xff;
    v4[12..].copy_from_slice(&[10, 0, 0, 1]);
    let mut v6 = [0u8; 16];
    v6[15] = 1;
    assert_eq!(values(&d, "ip"), vec![FieldValue::Ip(v4), FieldValue::Ip(v6)]);
    assert_eq!(
        values(&d, "g"),
        vec![FieldValue::GeoPoint(GeoPoint { lat: 2.5, lon: 1.5 }), FieldValue::GeoPoint(GeoPoint { lat: 3.0, lon: 4.0 })]
    );
}

#[test]
fn nested_multi_fields_and_copy_to() {
    let props = r#"{"properties":{
        "t":{"type":"text","fields":{"raw":{"type":"keyword"}},"copy_to":"all"},"all":{"type":"text"},
        "n":{"type":"nested","include_in_parent":true,"properties":{"v":{"type":"long"}}}}}"#;
    let d = parse(props, r#"{"t":"Hello","n":[{"v":1},{"v":2}]}"#);
    assert_eq!(values(&d, "t"), vec![FieldValue::Text("Hello".into())]);
    assert_eq!(values(&d, "t.raw"), vec![FieldValue::Keyword("Hello".into())]);
    assert_eq!(values(&d, "all"), vec![FieldValue::Text("Hello".into())]);
    assert_eq!(d.nested.len(), 2);
    assert_eq!(d.nested[1].fields, vec![DocField { name: "n.v".into(), value: FieldValue::Long(2) }]);
    // include_in_parent copies the nested values into the root document.
    assert_eq!(values(&d, "n.v"), vec![FieldValue::Long(1), FieldValue::Long(2)]);
}

#[test]
fn dynamic_fields_parse_with_their_new_mapping() {
    let d = parse(r#"{"properties":{}}"#, r#"{"a":1,"b":"x","c":{"d":2.5},"e":[3,"4"]}"#);
    assert!(d.dynamic_update.is_some());
    assert_eq!(values(&d, "a"), vec![FieldValue::Long(1)]);
    assert_eq!(values(&d, "b"), vec![FieldValue::Text("x".into())]);
    assert_eq!(values(&d, "b.keyword"), vec![FieldValue::Keyword("x".into())]);
    assert_eq!(values(&d, "c.d"), vec![FieldValue::Float(2.5)]);
    assert_eq!(values(&d, "e"), vec![FieldValue::Long(3), FieldValue::Long(4)]);
}

#[test]
fn null_values_and_ip_literals() {
    let d = parse(r#"{"properties":{"n":{"type":"long","null_value":5},"k":{"type":"keyword","null_value":"NULL"}}}"#, r#"{"n":[null,1],"k":null}"#);
    assert_eq!(values(&d, "n"), vec![FieldValue::Long(5), FieldValue::Long(1)]);
    assert_eq!(values(&d, "k"), vec![FieldValue::Keyword("NULL".into())]);
    assert!(values::parse_ip("01.2.3.4").is_none());
    assert!(values::parse_ip("1.2.3.256").is_none());
    assert!(values::parse_ip("fe80::1%eth0").is_some());
}

#[test]
fn copy_to_from_nested_reaches_the_targets_document() {
    // `DocumentParser.parseCopyFields`: the value goes to the innermost document whose prefix the
    // target starts with — the root for `all_text`, the nested document for `comments.all`.
    let props = r#"{"properties":{"all_text":{"type":"text"},"comments":{"type":"nested","properties":{
        "text":{"type":"text","copy_to":["all_text","comments.all"]},"all":{"type":"text"}}}}}"#;
    let d = parse(props, r#"{"comments":[{"text":"a"},{"text":"b"}]}"#);
    assert_eq!(values(&d, "all_text"), vec![FieldValue::Text("a".into()), FieldValue::Text("b".into())]);
    assert_eq!(d.nested.len(), 2);
    for (n, want) in d.nested.iter().zip(["a", "b"]) {
        let names: Vec<&str> = n.fields.iter().map(|f| &*f.name).collect();
        assert_eq!(names, vec!["comments.text", "comments.all"]);
        assert!(n.fields.iter().all(|f| f.value == FieldValue::Text(want.into())));
    }
}

#[test]
fn values_without_multi_fields_or_copy_to_are_not_cloned_into_extras() {
    let props = r#"{"properties":{"k":{"type":"keyword","fields":{"raw":{"type":"keyword"}}},"g":{"type":"geo_point"}}}"#;
    let d = parse(props, r#"{"k":"x","g":{"lat":1,"lon":2}}"#);
    let names: Vec<&str> = d.fields.iter().map(|f| &*f.name).collect();
    assert_eq!(names, vec!["k", "k.raw", "g"]);
}

#[test]
fn field_names_are_shared_across_values_and_documents() {
    let (m, ctx) = mapping(r#"{"properties":{"t":{"type":"text","fields":{"raw":{"type":"keyword"}}},"d":{"type":"date","format":"yyyy-MM-dd"}}}"#);
    let parse_one = |src: &[u8]| {
        let doc = crate::json::parse(src).unwrap().unwrap();
        parse_document(&m, &ctx, &doc, Some(src), Some("1")).unwrap()
    };
    let a = parse_one(br#"{"t":["x","y"],"d":"2020-01-01"}"#);
    let b = parse_one(br#"{"t":"z","d":"2021-01-01"}"#);
    let named = |d: &ParsedDocument, n: &str| d.fields.iter().filter(|f| &*f.name == n).map(|f| f.name.clone()).collect::<Vec<_>>();
    let (ta, tb) = (named(&a, "t"), named(&b, "t"));
    assert_eq!(ta.len(), 2);
    assert!(Arc::ptr_eq(&ta[0], &ta[1]) && Arc::ptr_eq(&ta[0], &tb[0]));
    let (ra, rb) = (named(&a, "t.raw"), named(&b, "t.raw"));
    assert!(Arc::ptr_eq(&ra[0], &rb[0]));
    assert_eq!(values(&b, "d"), vec![FieldValue::Date(1_609_459_200_000)]);
}
