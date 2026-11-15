//! Mutation fuzzer: mutates the scripts of every fixture and runs them, reporting any input
//! that makes the engine panic (reported as an `illegal_state_exception` "internal error"), or
//! that takes longer than a second. `cargo run --release --example fuzz -- [iterations] [seed]`.

use std::time::{Duration, Instant};

use search_painless::es::write::CtxKind;
use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Lang, NoHost, Settings, Value};

/// xorshift64*: deterministic for a seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

const PIECES: &[&str] = &[
    "(", ")", "[", "]", "{", "}", ";", ",", ".", "?.", "?:", "=~", "==~", "->", "::", "+", "-", "*", "/", "%", "<<", ">>>", "&", "|", "^", "!", "~", "==", "===",
    "!=", "<", ">=", "=", "+=", "++", "--", "instanceof ", "new ", "def ", "int ", "long ", "double ", "String ", "List ", "Map ", "return ", "if (", "else ", "while (",
    "for (", "do ", "try {", "catch (Exception e) {", "throw ", "break;", "continue;", "null", "true", "0", "-1", "2147483647", "9223372036854775807L", "1e308", "0x",
    "'a'", "\"s\"", "/a+/", "[:]", "[]", "params.x", "doc['f'].value", "ctx", "this", "x", "e", "Math.max(", "String.valueOf(", ".size()", ".length()",
    ".get(0)", ".stream()", ".map(x -> x)", ".collect(Collectors.toList())", "(int)", "(def)", "(String)", "\\", "\n", "'", "\"", "/", "\u{1F600}",
];

fn mutate(src: &str, rng: &mut Rng) -> String {
    let mut chars: Vec<char> = src.chars().collect();
    for _ in 0..1 + rng.below(3) {
        let n = chars.len();
        match rng.below(6) {
            0 if n > 0 => {
                let a = rng.below(n);
                let b = (a + 1 + rng.below(8)).min(n);
                chars.drain(a..b);
            }
            1 => {
                let p: Vec<char> = PIECES[rng.below(PIECES.len())].chars().collect();
                let at = rng.below(n + 1);
                chars.splice(at..at, p);
            }
            2 if n > 1 => {
                let (a, b) = (rng.below(n), rng.below(n));
                chars.swap(a, b);
            }
            3 if n > 0 => {
                let a = rng.below(n);
                let b = (a + 1 + rng.below(16)).min(n);
                let piece: Vec<char> = chars[a..b].to_vec();
                let at = rng.below(n + 1);
                chars.splice(at..at, piece);
            }
            4 if n > 0 => chars.truncate(rng.below(n)),
            _ => {
                let at = rng.below(n + 1);
                chars.insert(at, char::from_u32(rng.below(128) as u32).unwrap_or(' '));
            }
        }
    }
    chars.into_iter().collect()
}

/// A document with one field of each common kind.
struct Doc;

impl Host for Doc {
    fn score(&mut self) -> f64 {
        1.5
    }
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        match field {
            "l" | "likes" | "n" | "i" => {
                out.clear(DocKind::Long);
                out.longs.extend_from_slice(&[5, 3]);
            }
            "d" | "price" | "f" => {
                out.clear(DocKind::Double);
                out.doubles.push(1.5);
            }
            "k" | "tag" | "tags" => {
                out.clear(DocKind::String);
                out.push_str("b");
                out.push_str("a");
            }
            "dt" | "date" | "timestamp" => {
                out.clear(DocKind::Date);
                out.longs.push(1_577_934_245_678);
            }
            "b" => {
                out.clear(DocKind::Boolean);
                out.longs.push(1);
            }
            "g" | "location" => {
                out.clear(DocKind::GeoPoint);
                out.geo.push([41.12, -71.34]);
            }
            "v" | "vec" => {
                out.clear(DocKind::DenseVector);
                out.vector.extend_from_slice(&[1.0, 2.0, 3.0]);
            }
            "e" => out.clear(DocKind::Long),
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn source(&mut self) -> Option<Value> {
        Some(Value::from_json_map(&serde_json::json!({"a": 1, "b": [1, 2], "c": {"d": "x"}})))
    }
}

fn sources() -> Vec<(String, Context)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "json")
            && let Ok(serde_json::Value::Array(cases)) = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&p).unwrap())
        {
            for c in cases {
                if let Some(s) = c["request"]["script"]["source"].as_str() {
                    let ctx = c["request"]["context"].as_str().and_then(Context::from_name).unwrap_or(Context::PainlessTest);
                    out.push((s.to_string(), ctx));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn main() {
    let iterations: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let seed: u64 = std::env::args().nth(2).and_then(|a| a.parse().ok()).unwrap_or(0x9E37_79B9_7F4A_7C15);
    std::panic::set_hook(Box::new(|_| {}));
    let start = Instant::now();
    let bad = run(iterations, seed, |i, panics, slow| {
        if i % 10_000 == 0 && i > 0 {
            eprintln!("{i} cases, {panics} panics, {slow} slow, {:?}", start.elapsed());
        }
    });
    for b in &bad {
        println!("{b}");
    }
    eprintln!("done: {iterations} cases, {} findings, {:?}", bad.len(), start.elapsed());
}

/// Runs `iterations` mutated scripts; returns the panicking and slow ones.
pub fn run(iterations: usize, seed: u64, mut progress: impl FnMut(usize, usize, usize)) -> Vec<String> {
    let srcs = sources();
    let engine = Engine::new(Settings { max_compilations_rate: None, cache_max_size: 0, max_loop_counter: 20_000, max_object_bytes: 16 << 20, ..Settings::default() });
    let mut rng = Rng(seed | 1);
    let (mut panics, mut slow) = (0, 0);
    let mut bad = Vec::new();
    for i in 0..iterations {
        let (orig, ctx) = &srcs[rng.below(srcs.len())];
        let src = mutate(orig, &mut rng);
        let t = Instant::now();
        let r = match engine.compile_stored(Lang::Painless, &src, *ctx, &[]) {
            Ok(search_painless::Compiled::Painless(s)) => {
                let mut inst = s.instance();
                inst.set_params(&serde_json::json!({"x": 1, "l": [1, 2], "m": {"a": "b"}, "q": [1, 2, 3]}));
                let kind = match ctx {
                    Context::Update => Some(CtxKind::Update),
                    Context::UpdateByQuery => Some(CtxKind::UpdateByQuery),
                    Context::Reindex => Some(CtxKind::Reindex),
                    Context::Ingest => Some(CtxKind::Ingest),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let source = Value::from_json_map(&serde_json::json!({"a": 1, "b": [1, 2], "c": {"d": "x"}}));
                    if let Ok(c) = search_painless::es::write::ctx_value(kind, source, &[("_index", Value::str("i")), ("_id", Value::str("1"))], None) {
                        inst.set_var("ctx", c);
                    }
                }
                if *ctx == Context::PainlessTest { inst.execute(&mut NoHost).err() } else { inst.execute(&mut Doc).err() }
            }
            Ok(_) => None,
            Err(e) => Some(e),
        };
        if let Some(e) = r
            && e.cause_reason.as_deref().is_some_and(|m| m.starts_with("internal error in the script engine"))
        {
            panics += 1;
            bad.push(format!("PANIC {src:?}\n  {}", e.cause_reason.unwrap_or_default()));
        }
        if t.elapsed() > Duration::from_secs(1) {
            slow += 1;
            bad.push(format!("SLOW {:?} {src:?}", t.elapsed()));
        }
        progress(i, panics, slow);
    }
    bad
}
