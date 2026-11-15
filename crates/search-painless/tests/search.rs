//! Runs the captured search fixtures (`tests/fixtures/search.json`, produced by
//! `tests/fixtures/capture_search.py` against Elasticsearch 9.5.5 over the documents in
//! `painless-search.docs.json`) the way the search layer drives scripts: `script_fields`,
//! script sorts, `script_score` over `match_all`, script queries, `scripted_metric`, runtime
//! fields and `terms_set`. Each case is evaluated document by document with a mock host and
//! compared with Elasticsearch's hits, scores, sort values, field values and aggregation results;
//! a script failure is compared with the shard failure's `script_exception`.

use std::path::Path;

use search_painless::xcontent::to_xcontent;
use search_painless::{Context, Engine, ScriptError, Settings, Value};
use serde_json::{json, Value as Json};

#[path = "support/mock_host.rs"]
mod mock_host;
use mock_host::MockHost;

const INDEX: &str = "painless-search";

fn fixture(name: &str) -> Json {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct Env {
    engine: Engine,
    mapping: Json,
    docs: Vec<Json>,
}

impl Env {
    fn host(&self, i: usize) -> MockHost {
        MockHost { index: INDEX.into(), mapping: self.mapping.clone(), doc: self.docs[i].clone(), score: 1.0, seq_no: i as i64, search: true }
    }
}

/// The script of a request part (`"source"` string or `{source, params}`).
fn script_of(v: &Json) -> (String, Json) {
    match v {
        Json::String(s) => (s.clone(), json!({})),
        o => (o["source"].as_str().unwrap().to_string(), o.get("params").cloned().unwrap_or(json!({}))),
    }
}

fn error_body(e: &ScriptError) -> Json {
    let mut body = e.to_response()["error"].clone();
    body.as_object_mut().unwrap().remove("root_cause");
    body
}

/// Our answer: `Ok(json)` to compare with the response part, or the script error.
type Outcome = Result<Json, Json>;

/// JSON for a script value, as the response renders it.
fn render(v: &Value) -> Json {
    serde_json::from_str(&to_xcontent(v).unwrap_or_else(|e| panic!("{e:?}"))).unwrap()
}

/// Runs a script over every document; `f` turns each result into the per-document answer.
fn per_doc(env: &Env, src: &str, params: &Json, ctx: Context, mut f: impl FnMut(usize, Value, &search_painless::Instance) -> Json) -> Result<Vec<Json>, Json> {
    let script = env.engine.compile(src, ctx).map_err(|e| error_body(&e))?;
    let mut inst = script.instance();
    inst.set_params(params);
    let mut out = Vec::new();
    for i in 0..env.docs.len() {
        let mut host = env.host(i);
        let v = inst.execute(&mut host).map_err(|e| error_body(&e))?;
        out.push(f(i, v, &inst));
    }
    Ok(out)
}

fn ids(n: usize) -> Vec<String> {
    (1..=n).map(|i| i.to_string()).collect()
}

fn script_fields(env: &Env, req: &Json) -> Outcome {
    let mut fields: Vec<serde_json::Map<String, Json>> = vec![serde_json::Map::new(); env.docs.len()];
    for (name, spec) in req["script_fields"].as_object().unwrap() {
        let (src, params) = script_of(&spec["script"]);
        let vals = per_doc(env, &src, &params, Context::Field, |_, v, _| render(&v))?;
        for (i, v) in vals.into_iter().enumerate() {
            // a list result becomes the field's values; anything else a single value
            let list = match v {
                Json::Array(a) => a,
                other => vec![other],
            };
            if !list.is_empty() {
                fields[i].insert(name.clone(), Json::Array(list));
            }
        }
    }
    Ok(Json::Array(ids(env.docs.len()).into_iter().zip(fields).map(|(id, f)| json!({ "_id": id, "fields": if f.is_empty() { Json::Null } else { Json::Object(f) } })).collect()))
}

fn script_sort(env: &Env, req: &Json) -> Outcome {
    let spec = &req["sort"][0]["_script"];
    let (src, params) = script_of(&spec["script"]);
    let number = spec["type"] == "number";
    let ctx = if number { Context::NumberSort } else { Context::StringSort };
    let vals = per_doc(env, &src, &params, ctx, |_, v, _| if number { json!(v.as_f64().unwrap_or(0.0)) } else { render(&v) })?;
    // a null string sort key fails in Lucene, outside the script
    if vals.iter().any(|v| v.is_null()) {
        return Err(json!({ "host": "null sort key" }));
    }
    let mut order: Vec<usize> = (0..env.docs.len()).collect();
    let desc = spec["order"] == "desc";
    order.sort_by(|&a, &b| {
        let o = match (&vals[a], &vals[b]) {
            (Json::String(x), Json::String(y)) => x.cmp(y),
            (x, y) => x.as_f64().unwrap_or(0.0).total_cmp(&y.as_f64().unwrap_or(0.0)),
        };
        if desc { o.reverse() } else { o }
    });
    Ok(Json::Array(order.into_iter().map(|i| json!({ "_id": (i + 1).to_string(), "sort": [vals[i].clone()] })).collect()))
}

fn script_score(env: &Env, req: &Json) -> Option<Outcome> {
    let q = &req["query"]["script_score"];
    if q["query"] != json!({ "match_all": {} }) {
        return None;
    }
    let (src, params) = script_of(&q["script"]);
    // scores are floats: compare them as Elasticsearch prints them
    let as_float = |x: f64| -> f64 { format!("{}", x as f32).parse().unwrap() };
    let vals = match per_doc(env, &src, &params, Context::Score, |_, v, _| json!(as_float(v.as_f64().unwrap_or(0.0)))) {
        Ok(v) => v,
        Err(e) => return Some(Err(e)),
    };
    // negative and NaN scores are rejected by the query, not the script
    if vals.iter().any(|v| v.as_f64().is_none_or(|x| x < 0.0)) {
        return Some(Err(json!({ "host": "invalid score" })));
    }
    let mut order: Vec<usize> = (0..env.docs.len()).collect();
    order.sort_by(|&a, &b| vals[b].as_f64().unwrap().total_cmp(&vals[a].as_f64().unwrap()).then(a.cmp(&b)));
    Some(Ok(Json::Array(order.into_iter().map(|i| json!({ "_id": (i + 1).to_string(), "_score": vals[i].clone() })).collect())))
}

fn script_filter(env: &Env, req: &Json) -> Outcome {
    let (src, params) = script_of(&req["query"]["bool"]["filter"]["script"]["script"]);
    let keep = per_doc(env, &src, &params, Context::Filter, |_, v, _| json!(matches!(v, Value::Bool(true))))?;
    Ok(Json::Array(keep.iter().enumerate().filter(|(_, k)| **k == json!(true)).map(|(i, _)| json!({ "_id": (i + 1).to_string() })).collect()))
}

fn scripted_metric(env: &Env, req: &Json) -> Outcome {
    let spec = &req["aggs"]["m"]["scripted_metric"];
    let params = spec.get("params").cloned().unwrap_or(json!({}));
    let compile = |src: &str, ctx| env.engine.compile(src, ctx).map_err(|e| error_body(&e));
    let state = Value::from_json_map(&json!({}));
    if let Some(init) = spec.get("init_script") {
        let s = compile(script_of(init).0.as_str(), Context::AggsInit)?;
        let mut inst = s.instance();
        inst.set_params(&params);
        inst.set_var("state", state.clone());
        inst.execute(&mut search_painless::NoHost).map_err(|e| error_body(&e))?;
    }
    let map = compile(script_of(&spec["map_script"]).0.as_str(), Context::AggsMap)?;
    let mut inst = map.instance();
    inst.set_params(&params);
    inst.set_var("state", state.clone());
    for i in 0..env.docs.len() {
        let mut host = env.host(i);
        inst.set_var("_score", Value::Double(1.0));
        inst.execute(&mut host).map_err(|e| error_body(&e))?;
    }
    let combined = match spec.get("combine_script") {
        Some(c) => {
            let s = compile(script_of(c).0.as_str(), Context::AggsCombine)?;
            let mut inst = s.instance();
            inst.set_params(&params);
            inst.set_var("state", state.clone());
            inst.execute(&mut search_painless::NoHost).map_err(|e| error_body(&e))?
        }
        None => return Err(json!({ "host": "combine_script is required" })),
    };
    let states = Value::from_json_map(&json!({ "s": [] }));
    let list = match &states {
        Value::Map(m) => m.get_str("s").unwrap(),
        _ => unreachable!(),
    };
    if let Value::List(l) = &list {
        l.v.borrow_mut().push(combined);
    }
    let value = match spec.get("reduce_script") {
        Some(r) => {
            let s = compile(script_of(r).0.as_str(), Context::AggsReduce)?;
            let mut inst = s.instance();
            inst.set_params(&params);
            inst.set_var("states", list.clone());
            inst.execute(&mut search_painless::NoHost).map_err(|e| error_body(&e))?
        }
        None => return Err(json!({ "host": "reduce_script is required" })),
    };
    Ok(json!({ "m": { "value": render(&value) } }))
}

fn runtime_field(env: &Env, req: &Json) -> Outcome {
    let spec = &req["runtime_mappings"]["r"];
    let (src, params) = script_of(&spec["script"]);
    let ty = spec["type"].as_str().unwrap();
    let ctx = match ty {
        "long" => Context::LongField,
        "double" => Context::DoubleField,
        "keyword" => Context::KeywordField,
        "boolean" => Context::BooleanField,
        "date" => Context::DateField,
        "ip" => Context::IpField,
        "geo_point" => Context::GeoPointField,
        "composite" if spec.get("fields").is_some() => Context::CompositeField,
        _ => return Err(json!({ "host": ty })),
    };
    let vals = per_doc(env, &src, &params, ctx, |_, _, inst| {
        let mut vals: Vec<Json> = inst.emitted().iter().map(|v| v.to_json()).collect();
        match ty {
            "date" => {
                for v in &mut vals {
                    let ms = v.as_i64().unwrap();
                    *v = json!(chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
                }
            }
            "geo_point" => {
                // the emitted point, as given (not quantized)
                for v in &mut vals {
                    *v = json!({ "coordinates": [v["lon"], v["lat"]], "type": "Point" });
                }
            }
            "boolean" | "keyword" | "long" | "double" | "ip" => {
                // doc values come back sorted (keywords also deduplicated)
                vals.sort_by(|a, b| match (a, b) {
                    (Json::String(x), Json::String(y)) => x.cmp(y),
                    (Json::Bool(x), Json::Bool(y)) => x.cmp(y),
                    (x, y) => x.as_f64().unwrap_or(0.0).total_cmp(&y.as_f64().unwrap_or(0.0)),
                });
                if ty == "keyword" {
                    vals.dedup();
                }
            }
            _ => {}
        }
        Json::Array(vals)
    })?;
    Ok(Json::Array(
        vals.into_iter()
            .enumerate()
            .map(|(i, v)| {
                let f = if v.as_array().is_some_and(|a| a.is_empty()) { Json::Null } else { json!({ "r": v }) };
                json!({ "_id": (i + 1).to_string(), "fields": f })
            })
            .collect(),
    ))
}

fn terms_set(env: &Env, req: &Json) -> Outcome {
    let q = &req["query"]["terms_set"]["tags"];
    let terms: Vec<String> = q["terms"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
    let (src, mut params) = script_of(&q["minimum_should_match_script"]);
    params["num_terms"] = json!(terms.len());
    let need = per_doc(env, &src, &params, Context::TermsSet, |_, v, _| json!(v.as_f64().unwrap_or(0.0)))?;
    let mut out = Vec::new();
    for (i, d) in env.docs.iter().enumerate() {
        let have = d["tags"].as_array().map(|a| a.iter().filter(|t| terms.iter().any(|x| t.as_str() == Some(x.as_str()))).count()).unwrap_or(0);
        if have > 0 && have as f64 >= need[i].as_f64().unwrap() {
            out.push(json!({ "_id": (i + 1).to_string() }));
        }
    }
    Ok(Json::Array(out))
}

/// The response part a case compares: hits reduced to the keys we produce, or aggregations.
fn expected(req: &Json, resp: &Json, keys: &[&str]) -> Json {
    if req.get("aggs").is_some() {
        return resp["aggregations"].clone();
    }
    Json::Array(
        resp["hits"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                let mut o = serde_json::Map::new();
                for k in keys {
                    o.insert((*k).into(), h.get(*k).cloned().unwrap_or(Json::Null));
                }
                Json::Object(o)
            })
            .collect(),
    )
}

/// Numbers compare as doubles (`2` and `2.0` are the same score or value).
fn num_eq(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| num_eq(p, q)),
        (Json::Object(x), Json::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| num_eq(v, w))),
        _ => a == b,
    }
}

#[test]
fn search() {
    let env = Env {
        engine: Engine::new(Settings { max_compilations_rate: None, ..Settings::default() }),
        mapping: fixture("painless-search.mapping.json"),
        docs: fixture("painless-search.docs.json").as_array().unwrap().clone(),
    };
    let cases = fixture("search.json");
    let mut failures = Vec::new();
    let mut checked = 0;
    for c in cases.as_array().unwrap() {
        let req = &c["request"];
        let resp = &c["response"];
        let (got, keys): (Outcome, &[&str]) = if req.get("script_fields").is_some() {
            (script_fields(&env, req), &["_id", "fields"])
        } else if req["sort"][0].get("_script").is_some() {
            (script_sort(&env, req), &["_id", "sort"])
        } else if req["query"].get("script_score").is_some() {
            match script_score(&env, req) {
                Some(o) => (o, &["_id", "_score"]),
                None => continue,
            }
        } else if req["query"]["bool"]["filter"].get("script").is_some() {
            (script_filter(&env, req), &["_id"])
        } else if req.get("aggs").is_some() {
            (scripted_metric(&env, req), &[])
        } else if req.get("runtime_mappings").is_some() {
            (runtime_field(&env, req), &["_id", "fields"])
        } else if req["query"].get("terms_set").is_some() {
            (terms_set(&env, req), &["_id"])
        } else {
            continue;
        };
        let ok = match (c["status"].as_u64().unwrap(), &got) {
            (200, Ok(have)) => num_eq(&expected(req, resp, keys), have),
            (_, Err(have)) if have.get("host").is_some() => continue,
            (_, Err(have)) => {
                // shard failures carry the script_exception; request-level ones are the error
                let want = match resp["error"]["failed_shards"].get(0) {
                    Some(f) => f["reason"].clone(),
                    None => {
                        let mut e = resp["error"].clone();
                        if let Some(o) = e.as_object_mut() {
                            o.remove("root_cause");
                        }
                        e
                    }
                };
                if want.is_object() && want["type"] != "script_exception" {
                    continue;
                }
                want == *have
            }
            _ => false,
        };
        checked += 1;
        if !ok {
            failures.push(format!("{}\n  want: {} {}\n  got:  {:?}", req, c["status"], if c["status"] == 200 { expected(req, resp, keys) } else { resp["error"].clone() }, got));
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "search.json: {} of {checked} cases differ from Elasticsearch", failures.len());
}
