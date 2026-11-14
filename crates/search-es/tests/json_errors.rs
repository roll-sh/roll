//! Malformed request bodies must fail with Elasticsearch's exact exception chain. The fixture
//! holds bodies sent to `PUT /t1` on Elasticsearch 9.5.5 and its responses.

use search_es::EsError;
use search_es::json::{Value, parse, parse_object};

#[test]
fn jackson_errors_match_elasticsearch() {
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/json_errors.json")).unwrap();
    let cases = parse(&raw).unwrap().unwrap();
    let mut checked = 0;
    for case in cases.as_array().unwrap() {
        let body = case.get("body").and_then(Value::as_str).unwrap();
        let resp = case.get("response").unwrap();
        let Some(err) = resp.get("error").filter(|e| e.as_object().is_some()) else {
            // Elasticsearch accepted it (e.g. a body starting with `[`); so must we parse it.
            assert!(parse_object(body.as_bytes()).is_ok(), "{body:?} should parse");
            continue;
        };
        let kind = err.get("type").and_then(Value::as_str).unwrap();
        if !kind.starts_with("x_content") {
            // A semantic error after a successful parse.
            assert!(parse_object(body.as_bytes()).is_ok(), "{body:?} should parse");
            continue;
        }
        let e: EsError = parse_object(body.as_bytes()).expect_err(body).into();
        let got = e.to_body(false);
        let got_err = got.get("error").unwrap();
        assert_eq!(got_err.get("type"), err.get("type"), "{body:?}");
        assert_eq!(got_err.get("reason"), err.get("reason"), "{body:?}");
        assert_eq!(got_err.get("caused_by"), err.get("caused_by"), "{body:?}");
        assert_eq!(got_err.get("root_cause"), err.get("root_cause"), "{body:?}");
        checked += 1;
    }
    assert!(checked > 40, "{checked}");
}
