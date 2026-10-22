//! Runs a `painless_test` script given on the command line and prints its result (or the
//! error Elasticsearch would answer): `cargo run --example run -- "return 1 + 1"`.
use search_painless::{Context, Engine, NoHost, Settings};

fn main() {
    let src = std::env::args().nth(1).expect("script source");
    let params: serde_json::Value = std::env::args().nth(2).map(|p| serde_json::from_str(&p).expect("params JSON")).unwrap_or(serde_json::json!({}));
    // PAINLESS_ALLOC=<bytes>: with allocation tracking
    let alloc = std::env::var("PAINLESS_ALLOC").ok().and_then(|l| l.parse().ok()).map(|l| vec![(Context::PainlessTest, l)]).unwrap_or_default();
    let engine = Engine::new(Settings { max_compilations_rate: None, max_allocation_bytes: alloc, ..Settings::default() });
    match engine.compile(&src, Context::PainlessTest) {
        Err(e) => println!("{}", e.to_response()),
        Ok(s) => {
            let mut inst = s.instance();
            inst.set_params(&params);
            match inst.execute(&mut NoHost) {
                Ok(v) => println!("{}", v.to_java_string()),
                Err(e) => println!("{}", e.to_response()),
            }
        }
    }
}
