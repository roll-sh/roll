//! Per-document cost of update and ingest scripts, including building `ctx` from a parsed
//! `_source` and writing the new source back as JSON.
use std::time::Instant;

use search_painless::es::write::{ctx_of, ctx_value, CtxKind};
use search_painless::xcontent::{parse_source, to_xcontent};
use search_painless::{Context, Engine, NoHost, Settings, Value};

fn main() {
    let engine = Engine::new(Settings::default());
    let source = r#"{"name":"widget","counter":41,"tags":["a","b"],"price":12.5,"nested":{"x":1,"y":[1,2,3]}}"#;
    let cases = [
        (Context::Update, CtxKind::Update, "ctx._source.counter += params.count"),
        (Context::Update, CtxKind::Update, "if (ctx._source.tags.contains(params.tag)) { ctx.op = 'none' } else { ctx._source.tags.add(params.tag) }"),
        (Context::Ingest, CtxKind::Ingest, "ctx.name = ctx.name.toUpperCase(); ctx.total = ctx.price * ctx.counter"),
        (Context::Ingest, CtxKind::Ingest, "ctx.tags = ctx.tags.stream().map(t -> t + '!').collect(Collectors.toList())"),
    ];
    let params = serde_json::json!({"count": 1, "tag": "c"});
    for (ctx, kind, src) in cases {
        let script = engine.compile(src, ctx).unwrap();
        let mut inst = script.instance();
        inst.set_params(&params);
        let n = 200_000;
        let t = Instant::now();
        let mut bytes = 0;
        for _ in 0..n {
            let doc = parse_source(source).unwrap();
            let meta: &[(&str, Value)] = if kind == CtxKind::Update {
                &[("_index", Value::str("i")), ("_id", Value::str("1")), ("_version", Value::Long(1)), ("_routing", Value::Null), ("_type", Value::str("_doc")), ("op", Value::str("index")), ("_now", Value::Long(0))]
            } else {
                &[("_index", Value::str("i")), ("_id", Value::str("1")), ("_version", Value::Long(1))]
            };
            let c = ctx_value(kind, doc, meta, None).unwrap();
            inst.set_var("ctx", c.clone());
            inst.execute(&mut NoHost).unwrap();
            let out = ctx_of(&c).unwrap().source();
            bytes += to_xcontent(&out).unwrap().len();
        }
        println!("{:>8.0} ns/doc  {:<8} {src}  ({bytes})", t.elapsed().as_nanos() as f64 / n as f64, ctx.name());
    }
    // the phases of one document
    let n = 200_000;
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(parse_source(source).unwrap());
    }
    println!("{:>8.0} ns/doc  parse_source", t.elapsed().as_nanos() as f64 / n as f64);
    let doc = parse_source(source).unwrap();
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(to_xcontent(&doc).unwrap());
    }
    println!("{:>8.0} ns/doc  to_xcontent", t.elapsed().as_nanos() as f64 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        let d = parse_source(source).unwrap();
        std::hint::black_box(ctx_value(CtxKind::Ingest, d, &[("_index", Value::str("i")), ("_id", Value::str("1")), ("_version", Value::Long(1))], None).unwrap());
    }
    println!("{:>8.0} ns/doc  parse_source + ctx_value", t.elapsed().as_nanos() as f64 / n as f64);
}
