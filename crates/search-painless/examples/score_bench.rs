//! Rough throughput check of the `script_score` hot path: one instance, one document per call.

/// CPU time of this thread in nanoseconds (`/proc/thread-self/schedstat`): unlike wall time it
/// does not count time the thread spent descheduled on a busy machine.
fn cpu_ns() -> u64 {
    std::fs::read_to_string("/proc/thread-self/schedstat").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0)
}

use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Settings};

struct Docs {
    doc: i64,
    vector: Vec<f32>,
    copy: bool,
}

impl Host for Docs {
    fn score(&mut self) -> f64 {
        1.5
    }
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        match field {
            "likes" => {
                out.clear(DocKind::Long);
                out.longs.push(self.doc % 1000);
                Ok(true)
            }
            "price" => {
                out.clear(DocKind::Double);
                out.doubles.push((self.doc % 97) as f64 * 1.25);
                Ok(true)
            }
            "vec" => {
                out.clear(DocKind::DenseVector);
                out.vector.extend_from_slice(&self.vector);
                out.vector[0] = (self.doc % 13) as f32;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    // the vector straight from the host's storage (PAINLESS_COPY=1 measures the copying path)
    fn with_vector(&mut self, field: &str, f: &mut dyn FnMut(&search_painless::VectorView<'_>)) -> Result<Option<bool>, HostError> {
        if field != "vec" || self.copy {
            return Ok(None);
        }
        self.vector[0] = (self.doc % 13) as f32;
        f(&search_painless::VectorView { element: search_painless::VectorElement::Float, floats: &self.vector, bytes: &[], binary: false, magnitude: None });
        Ok(Some(true))
    }
}

fn main() {
    let engine = Engine::new(Settings::default());
    let only = std::env::var("PAINLESS_BENCH").ok();
    let scripts = [
        "return 1.0",
        "return _score",
        "return params.factor",
        "return doc['likes'].value",
        "double s = 0; for (int i = 0; i < 10; ++i) { s += i; } return s;",
        "int s = 0; for (int i = 0; i < 100; ++i) { s += i; } return s;",
        "Math.log(2 + doc['likes'].value) * params.factor",
        "_score * params.factor + doc['price'].value",
        "double s = 0; for (int i = 0; i < 10; ++i) { s += i * doc['price'].value; } return s;",
        "def x = doc['likes'].value; if (x > 500) { return x / 2.0 } return x * params.factor",
        "def x = doc['likes'].value; return x * params.factor",
        "long x = doc['likes'].value; if (x > 500) { return x / 2.0 } return x * params.factor",
        "doc['likes'].value > 500 ? doc['likes'].value / 2.0 : doc['likes'].value * params.factor",
        "doc['likes'].value / 2.0",
        "doc['likes'].value * params.factor",
        "long x = doc['likes'].value; return x",
        "double x = 2.0; return x",
        "decayNumericGauss(10, 5, 0, 0.5, doc['price'].value) * _score",
        "cosineSimilarity(params.qv, 'vec') + 1.0",
        "dotProduct(params.qv, 'vec')",
        "l2norm(params.qv, 'vec')",
    ];
    let qv: Vec<f32> = (0..dims()).map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0).collect();
    let params = serde_json::json!({ "factor": 1.2, "qv": qv });
    let mut cases: Vec<(Context, &str)> = scripts.iter().map(|s| (Context::Score, *s)).collect();
    cases.extend([
        (Context::Filter, "doc['likes'].value > 500"),
        (Context::NumberSort, "doc['price'].value * 2"),
        (Context::Aggs, "doc['price'].value * 2"),
        (Context::DoubleField, "emit(doc['price'].value * 1.1)"),
        (Context::LongField, "emit(doc['likes'].value + 1)"),
        (Context::DoubleField, "for (double v : doc['price']) { emit(v * 1.1) }"),
        (Context::Score, "double s = 0; for (double v : doc['price']) { s += v } return s"),
        (Context::Score, "doc['likes'].size() == 0 ? 0 : doc['likes'].value"),
        (Context::Score, "Math.max(doc['likes'].value, doc['price'].value) / 100.0"),
    ]);
    // PAINLESS_SRC: one score script of your own
    let custom = std::env::var("PAINLESS_SRC").ok();
    if let Some(src) = &custom {
        cases = vec![(Context::Score, src.as_str())];
    }
    for (ctx, src) in cases {
        if only.as_deref().is_some_and(|o| !src.contains(o)) {
            continue;
        }
        let script = engine.compile(src, ctx).unwrap();
        let mut inst = script.instance();
        inst.set_params(&params);
        // PAINLESS_NOFAST=1 measures the interpreter on every script
        if std::env::var("PAINLESS_NOFAST").is_ok() {
            inst.disable_fast_path();
        }
        let mut host = Docs { doc: 0, copy: std::env::var_os("PAINLESS_COPY").is_some(), vector: (0..dims()).map(|i| ((i * 13 % 89) as f32 - 44.0) / 44.0).collect() };
        let n = 1_000_000;
        // best of several rounds: the machine may be busy
        let mut best = f64::MAX;
        let mut acc = 0.0;
        for _ in 0..7 {
            acc = 0.0;
            let t = cpu_ns();
            for d in 0..n {
                host.doc = d;
                acc += match ctx {
                    Context::Filter => inst.execute_bool(&mut host).unwrap() as u8 as f64,
                    Context::DoubleField | Context::LongField => {
                        inst.execute(&mut host).unwrap();
                        inst.emitted().len() as f64
                    }
                    Context::Aggs => inst.execute(&mut host).unwrap().as_f64().unwrap_or(0.0),
                    _ => inst.execute_f64(&mut host).unwrap(),
                };
            }
            best = best.min((cpu_ns() - t) as f64 / n as f64);
        }
        let path = if inst.fast_path_active() { "fast" } else { "vm" };
        println!("{best:>8.1} ns/doc  {path:<4}  {:<12} {src}  ({acc:.1})", ctx.name());
    }
}

/// Vector length of the vector scripts (`PAINLESS_DIMS`, 768).
fn dims() -> usize {
    std::env::var("PAINLESS_DIMS").ok().and_then(|d| d.parse().ok()).unwrap_or(768)
}
