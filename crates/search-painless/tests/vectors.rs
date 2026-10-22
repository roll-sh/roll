//! Vector score functions against Elasticsearch 9.5.5 (`tests/fixtures/vectors.json`: random
//! float vectors of 3 to 1000 dimensions and the `dotProduct`, `cosineSimilarity`, `l2norm` and
//! `l1norm` Elasticsearch computed for them, captured after JIT warm-up). The SIMD kernels
//! follow Lucene's Panama implementation; a few results differ in the last ulp because the JIT's
//! reduction order is not fully reproducible, so the test bounds the error and pins the exact
//! agreement count.

use std::path::Path;

use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Settings};
use serde_json::{json, Value as Json};

struct VecHost(Vec<f32>);

impl Host for VecHost {
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        if field != "v" {
            return Ok(false);
        }
        out.clear(DocKind::DenseVector);
        out.vector.extend_from_slice(&self.0);
        Ok(true)
    }
}

#[test]
fn vector_functions_match_elasticsearch() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vectors.json");
    let cases: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let (mut exact, mut total) = (0, 0);
    for c in &cases {
        let v: Vec<f32> = c["v"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect();
        for f in ["dotProduct", "cosineSimilarity", "l2norm", "l1norm"] {
            let want = c["res"][f].as_f64().unwrap();
            let script = engine.compile(&format!("return {f}(params.q, 'v')"), Context::Score).unwrap();
            let mut inst = script.instance();
            inst.set_params(&json!({ "q": c["q"] }));
            let got = inst.execute_f64(&mut VecHost(v.clone())).unwrap();
            total += 1;
            if got == want {
                exact += 1;
            } else {
                let rel = ((got - want) / want.abs().max(1e-30)).abs();
                assert!(rel < 1e-5, "{f} dims={}: {got} vs {want}", c["dims"]);
            }
        }
    }
    assert!(exact * 100 / total >= 85, "only {exact} of {total} vector results equal Elasticsearch's");
    eprintln!("{exact} of {total} vector results equal Elasticsearch's exactly");
}

/// A document with `rank_vectors` fields: `rv` (float, three 2-d vectors), `rb` (byte, two
/// 2-d vectors), `rbit` (bit, one 16-bit vector).
struct RankHost {
    empty: bool,
}

impl Host for RankHost {
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        use search_painless::VectorElement;
        out.clear(DocKind::RankVectors);
        if self.empty {
            return Ok(matches!(field, "rv" | "rb" | "rbit"));
        }
        match field {
            "rv" => {
                out.vector.extend_from_slice(&[1.0, 0.0, 0.0, 1.0, 0.5, 0.5]);
                out.vector_count = 3;
            }
            "rb" => {
                out.element = VectorElement::Byte;
                out.vector_bytes.extend_from_slice(&[1, 2, -3, 4]);
                out.vector_count = 2;
            }
            "rbit" => {
                out.element = VectorElement::Bit;
                out.vector_bytes.extend_from_slice(&[0x80u8 as i8, 0x01]);
                out.vector_count = 1;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[test]
fn rank_vectors_max_sim() {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let run = |src: &str, params: Json, empty: bool| -> Result<Json, Json> {
        let script = engine.compile(src, Context::Score).map_err(|e| e.to_response())?;
        let mut inst = script.instance();
        inst.set_params(&params);
        inst.execute(&mut RankHost { empty }).map(|v| v.to_json()).map_err(|e| e.to_response()["error"]["caused_by"].clone())
    };
    let ok = |src: &str, params: Json| run(src, params, false).unwrap_or_else(|e| panic!("{src}: {e}"));
    let err = |src: &str, params: Json| run(src, params, false).unwrap_err()["reason"].as_str().unwrap_or("").to_string();
    // best of [1, 0, 0.5] plus best of [0, 2, 1]
    assert_eq!(ok("maxSimDotProduct(params.q, 'rv')", json!({"q": [[1, 0], [0, 2]]})), json!(3.0));
    assert_eq!(ok("maxSimDotProduct(params.q, 'rb')", json!({"q": [[1, 1]]})), json!(3.0));
    assert_eq!(ok("maxSimDotProduct(params.q, 'rb')", json!({"q": ["0101"]})), json!(3.0));
    assert_eq!(ok("maxSimInvHamming(params.q, 'rb')", json!({"q": [[1, 2]]})), json!(1.0));
    assert_eq!(ok("maxSimInvHamming(params.q, 'rbit')", json!({"q": ["8001"]})), json!(1.0));
    assert_eq!(ok("maxSimDotProduct(params.q, 'rbit')", json!({"q": [[-1, -1]]})), json!(2.0));
    let ramp: Vec<f64> = (1..=16).map(|x| x as f64 + 0.5).collect();
    assert_eq!(ok("maxSimDotProduct(params.q, 'rbit')", json!({ "q": [ramp] })), json!(1.5 + 16.5));
    assert_eq!(err("maxSimDotProduct(params.q, 'rv')", json!({"q": [[1, 0, 0]]})), "The query vector has a different number of dimensions [3] than the document vectors [2].");
    assert_eq!(err("maxSimDotProduct(params.q, 'rv')", json!({"q": [[1, 0], [1]]})), "The query vector contains inner vectors which have inconsistent number of dimensions.");
    assert_eq!(err("maxSimDotProduct(params.q, 'rv')", json!({"q": []})), "The query vector is empty.");
    assert_eq!(err("maxSimDotProduct(params.q, 'rv')", json!({"q": "0101"})), "Unsupported input object for float vectors: java.lang.String");
    assert_eq!(err("maxSimInvHamming(params.q, 'rv')", json!({"q": [[1, 0]]})), "hamming distance is only supported for byte or bit vectors");
    assert!(err("maxSimDotProduct(params.q, 'rb')", json!({"q": [[1, 300]]})).starts_with("element_type [byte] vectors only support integers between [-128, 127] but found [300.0] at dim [1];"));
    // the field API and doc values
    assert_eq!(ok("field('rv').get().getDims() * 100 + field('rv').get().size()", json!({})), json!(203.0));
    assert_eq!(ok("field('rbit').get().getDims()", json!({})), json!(16.0));
    assert_eq!(ok("doc['rv'].size()", json!({})), json!(3.0));
    assert_eq!(ok("double s = 0; Iterator it = doc['rv'].vectorValues; while (it.hasNext()) { float[] v = it.next(); s += v[0] * 10 + v[1] } return s", json!({})), json!(10.0 + 1.0 + 5.5));
    assert_eq!(ok("float[] m = doc['rb'].magnitudes; return m[0] + m[1]", json!({})), json!((5f32.sqrt() + 5.0) as f64));
    assert_eq!(ok("field('rv').get().isEmpty() ? 1 : 0", json!({})), json!(0.0));
    assert_eq!(err("doc['rv'].get(0) == null ? 1 : 0", json!({})), "accessing a rank-vectors field's value through 'get' or 'value' is not supported, use 'vectorValues' or 'magnitudes' instead.");
    // a document without vectors
    let missing = run("maxSimDotProduct(params.q, 'rv')", json!({"q": [[1, 0]]}), true).unwrap_err();
    assert_eq!(missing["reason"], "rank-vectors value missing for a field, use isEmpty() to check for a missing value");
    assert_eq!(run("field('rv').get().isEmpty() ? 1 : 0", json!({}), true).unwrap(), json!(1.0));
    assert_eq!(run("doc['rv'].size()", json!({}), true).unwrap(), json!(0.0));
}
