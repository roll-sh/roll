//! Query DSL tests: parse and print round trips and errors.

use super::*;

fn parse(src: &str) -> EsResult<Query> {
    let body = format!("{{\"query\":{src}}}");
    let v = crate::json::parse(body.as_bytes()).unwrap().unwrap();
    let q = v.get("query").unwrap().clone();
    let (q, _) = parse_query(&q, Some(body.as_bytes()), &[Seg::Key("query")])?;
    Ok(q)
}

#[test]
fn term_round_trip() {
    let q = parse(r#"{"term":{"kw":"x"}}"#).unwrap();
    assert_eq!(q.to_value().to_json(), r#"{"term":{"kw":{"value":"x"}}}"#);
}
