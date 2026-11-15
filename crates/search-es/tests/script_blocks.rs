//! Score scripts over blocks of documents (`DocHost::prefetch` decodes the doc values of a run
//! of consecutive documents ahead) score exactly as document by document: dense and sparse
//! fields, several segments, deletions, missing values that make a script fail, Painless and
//! Lucene expressions, `script_score` queries and `function_score` script functions.

use search_es::json::{self, Value};
use search_es::{Cluster, EsRequest};

async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let r = search_es::handle(c, EsRequest::json(method, uri, body.to_string())).await;
    let v = json::parse(&r.body).unwrap().unwrap_or(Value::Null);
    (r.status, v)
}

fn without_took(mut v: Value) -> Value {
    if let Value::Obj(m) = &mut v {
        m.shift_remove("took");
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_scoring_matches_per_doc_scoring() {
    let c = Cluster::in_memory("blocks").await;
    let (st, _) = call(
        &c,
        "PUT",
        "/b",
        r#"{"settings":{"number_of_shards":2,"refresh_interval":"-1"},"mappings":{"properties":{"pop":{"type":"long"},"price":{"type":"double"},"f":{"type":"float"},"sf":{"type":"scaled_float","scaling_factor":100},"loc":{"type":"geo_point"},"tag":{"type":"keyword"}}}}"#,
    )
    .await;
    assert_eq!(st, 200);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut id = 0;
    // Three segments per shard: all fields dense, then `price` sparse and `pop` multi-valued
    // in a few docs, then dense again; deletions in between.
    for seg in 0..3 {
        let mut body = String::new();
        for _ in 0..700 {
            let r = next();
            let pop = (r % 100_000) as i64 - 10;
            let lat = (r >> 20) as f64 / (1u64 << 44) as f64 * 170.0 - 85.0;
            let lon = (r >> 8 & 0xffff_ffff) as f64 / (1u64 << 32) as f64 * 350.0 - 175.0;
            let mut doc = format!(r#"{{"pop":{pop},"f":{},"sf":{},"loc":[{lon},{lat}],"tag":"t{}""#, (r % 977) as f32 / 7.0, (r % 5000) as f64 / 100.0, r % 3);
            if seg != 1 || r % 5 != 0 {
                doc.push_str(&format!(r#","price":{}"#, (r % 10_000) as f64 / 13.0));
            }
            if seg == 1 && r % 11 == 0 {
                doc = doc.replacen(&format!(r#""pop":{pop}"#), &format!(r#""pop":[{pop},{}]"#, pop + 1), 1);
            }
            doc.push('}');
            body.push_str(&format!("{{\"index\":{{\"_id\":\"{id}\"}}}}\n{doc}\n"));
            id += 1;
        }
        let (st, v) = call(&c, "POST", "/b/_bulk", &body).await;
        assert_eq!(st, 200, "{v:?}");
        assert_eq!(v.get("errors"), Some(&Value::Bool(false)));
        if seg == 1 {
            let mut del = String::new();
            for d in (0..id).step_by(37) {
                del.push_str(&format!("{{\"delete\":{{\"_id\":\"{d}\"}}}}\n"));
            }
            call(&c, "POST", "/b/_bulk", &del).await;
        }
        call(&c, "POST", "/b/_refresh", "").await;
    }
    let scripts = [
        r#"{"source":"Math.log10(doc['pop'].value * 1.2 + 2)"}"#,
        r#"{"source":"doc['price'].value * 2 + doc['f'].value - doc['sf'].value"}"#,
        r#"{"source":"doc['price'].size() == 0 ? 1 : doc['price'].value"}"#,
        r#"{"source":"decayGeoGauss(params.origin, params.scale, params.offset, params.decay, doc['loc'].value)","params":{"origin":"52.37, 4.8951","scale":"500km","offset":"0km","decay":0.1}}"#,
        r#"{"source":"Math.abs(Math.log(Math.abs((int)((List)doc.pop).get(0)) + 1) + (double)(doc.loc.lon) * (double)(doc.loc.lat))/_score"}"#,
        r#"{"source":"abs(ln(abs(doc['pop']) + 1) + doc['loc'].lon + doc['loc'].lat) * _score","lang":"expression"}"#,
        r#"{"source":"doc['price'].value + doc['loc'].lat","lang":"expression"}"#,
    ];
    let queries = [r#"{"match_all":{}}"#, r#"{"term":{"tag":"t1"}}"#, r#"{"range":{"pop":{"gte":5000}}}"#];
    let mut checked = 0;
    for s in scripts {
        for q in queries {
            for body in [
                format!(r#"{{"size":50,"query":{{"script_score":{{"query":{q},"script":{s}}}}}}}"#),
                format!(r#"{{"size":50,"query":{{"function_score":{{"query":{q},"functions":[{{"script_score":{{"script":{s}}}}}]}}}}}}"#),
                format!(r#"{{"size":0,"query":{{"function_score":{{"query":{q},"functions":[{{"script_score":{{"script":{s}}}}}],"min_score":3}}}}}}"#),
            ] {
                search_es::search::scripts::set_prefetch(true);
                let (st1, a) = call(&c, "POST", "/b/_search", &body).await;
                search_es::search::scripts::set_prefetch(false);
                let (st2, b) = call(&c, "POST", "/b/_search", &body).await;
                search_es::search::scripts::set_prefetch(true);
                assert_eq!(st1, st2, "{body}");
                assert_eq!(without_took(a.clone()), without_took(b), "{body}");
                if st1 == 200 {
                    checked += 1;
                }
            }
        }
    }
    // Most combinations score (the sparse `price` makes some fail, which must fail alike).
    assert!(checked >= 40, "{checked}");
}
