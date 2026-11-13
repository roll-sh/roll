//! Compilation latency: distinct sources (no cache hits), typical script shapes.
use std::time::Instant;

use search_painless::{Context, Engine, Settings};

fn main() {
    let engine = Engine::new(Settings { max_compilations_rate: None, cache_max_size: 0, ..Settings::default() });
    let shapes = [
        (Context::Score, "Math.log(2 + doc['likes'].value) * params.factor + {i}"),
        (Context::Update, "if (ctx._source.counter == null) { ctx._source.counter = {i} } else { ctx._source.counter += params.count }"),
        (Context::Ingest, "def parts = ctx.message.splitOnToken(' '); ctx.level = parts[0]; ctx.code = Integer.parseInt(parts[1]) + {i}; ctx.tags = ['a', 'b'].stream().map(x -> x + '{i}').collect(Collectors.toList())"),
        (Context::LongField, "emit(doc['timestamp'].value.toInstant().toEpochMilli() / 1000 + {i})"),
    ];
    for (ctx, shape) in shapes {
        let n = 2000;
        let t = Instant::now();
        for i in 0..n {
            let src = shape.replace("{i}", &i.to_string());
            engine.compile(&src, ctx).unwrap();
        }
        println!("{:>8.1} µs/compile  {:<12} {shape}", t.elapsed().as_secs_f64() * 1e6 / n as f64, ctx.name());
    }
}
