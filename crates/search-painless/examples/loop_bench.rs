//! Throughput of script shapes that leave the straight-line fast path (loops, multi-valued
//! fields, map lookups, strings): one instance, one document per call.

fn cpu_ns() -> u64 {
    std::fs::read_to_string("/proc/thread-self/schedstat").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0)
}

use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Settings};

struct Docs {
    doc: i64,
}

impl Host for Docs {
    fn score(&mut self) -> f64 {
        1.5
    }
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        match field {
            "vals" => {
                out.clear(DocKind::Double);
                for i in 0..8 {
                    out.doubles.push(((self.doc + i) % 97) as f64 * 0.5);
                }
                Ok(true)
            }
            "likes" => {
                out.clear(DocKind::Long);
                out.longs.push(self.doc % 1000);
                Ok(true)
            }
            "cat" => {
                out.clear(DocKind::String);
                out.push_str(["a", "b", "c", "d"][(self.doc % 4) as usize]);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

fn main() {
    let engine = Engine::new(Settings::default());
    let scripts = [
        "int s = 0; for (int i = 0; i < 100; ++i) { s += i; } return s;",
        "double s = 0; for (def v : doc['vals']) { s += v } return s;",
        "double s = 0; for (int i = 0; i < doc['vals'].size(); ++i) { s += doc['vals'].get(i) } return s;",
        "double s = 0; for (double v : doc['vals']) { s += v } return s;",
        "doc['cat'].value == params.c ? 2.0 : 1.0",
        "def w = params.weights[doc['cat'].value]; return w == null ? 1.0 : w",
        "double m = 0; for (def v : doc['vals']) { m = Math.max(m, v) } return m * _score;",
        "long x = doc['likes'].value; int b = 0; while (x > 0) { b += (int) (x & 1); x >>= 1 } return b;",
    ];
    let params = serde_json::json!({ "c": "b", "weights": {"a": 1.5, "b": 2.5} });
    // scripts on the command line replace the built-in set
    let args: Vec<String> = std::env::args().skip(1).collect();
    let scripts: Vec<&str> = if args.is_empty() { scripts.to_vec() } else { args.iter().map(|s| s.as_str()).collect() };
    for src in scripts {
        let script = engine.compile(src, Context::Score).unwrap();
        let mut inst = script.instance();
        inst.set_params(&params);
        let mut host = Docs { doc: 0 };
        let n = 300_000;
        let mut best = f64::MAX;
        let mut acc = 0.0;
        for _ in 0..5 {
            acc = 0.0;
            let t = cpu_ns();
            for d in 0..n {
                host.doc = d;
                acc += inst.execute_f64(&mut host).unwrap();
            }
            best = best.min((cpu_ns() - t) as f64 / n as f64);
        }
        let path = if inst.fast_path_active() { "fast" } else { "vm" };
        println!("{best:>8.1} ns/doc  {path:<4}  {src}  ({acc:.1})");
    }
}
