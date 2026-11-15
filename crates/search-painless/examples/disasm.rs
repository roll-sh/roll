fn main() {
    let src = std::env::args().nth(1).unwrap();
    let ctx = std::env::args().nth(2).map(|c| search_painless::Context::from_name(&c).unwrap()).unwrap_or(search_painless::Context::Score);
    // PAINLESS_ALLOC=<bytes>: with allocation tracking for the context
    let mut settings = search_painless::Settings::default();
    if let Some(l) = std::env::var("PAINLESS_ALLOC").ok().and_then(|l| l.parse().ok()) {
        settings.max_allocation_bytes = vec![(ctx, l)];
    }
    let s = search_painless::compile_painless(&src, ctx, &settings).unwrap();
    print!("{}", s.disassemble());
}
#[allow(dead_code)]
fn sizes() {}
