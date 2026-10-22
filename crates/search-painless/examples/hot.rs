//! Runs one score script in a loop for profiling.
use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Settings};
struct Docs(i64);
impl Host for Docs {
    fn score(&mut self) -> f64 { 1.5 }
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        out.clear(if field == "price" { DocKind::Double } else { DocKind::Long });
        if field == "price" { out.doubles.push((self.0 % 97) as f64) } else { out.longs.push(self.0 % 1000) }
        Ok(true)
    }
}
fn main() {
    let src = std::env::args().nth(1).unwrap();
    let n: i64 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(50_000_000);
    let engine = Engine::new(Settings::default());
    let script = engine.compile(&src, Context::Score).unwrap();
    let mut inst = script.instance();
    inst.set_params(&serde_json::json!({"factor": 1.2}));
    let mut host = Docs(0);
    let t = std::time::Instant::now();
    let mut acc = 0.0;
    for d in 0..n { host.0 = d; acc += inst.execute_f64(&mut host).unwrap(); }
    println!("{:.1} ns/doc {acc}", t.elapsed().as_nanos() as f64 / n as f64);
}
