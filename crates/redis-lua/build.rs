//! Builds the vendored Lua 5.1 (Valkey's `deps/lua`, with Redis' string hash and cjson array
//! metatables) and our glue into one static library.

fn main() {
    let mut b = cc::Build::new();
    for entry in std::fs::read_dir("lua").expect("lua sources") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "c") {
            println!("cargo:rerun-if-changed={}", path.display());
            b.file(path);
        }
    }
    println!("cargo:rerun-if-changed=glue.c");
    b.file("glue.c")
        .include("lua")
        .define("LUA_ANSI", None)
        .define("ENABLE_CJSON_GLOBAL", None)
        .define("LUA_USE_MKSTEMP", None)
        .opt_level(2)
        .warnings(false)
        .compile("lua51redis");
}
