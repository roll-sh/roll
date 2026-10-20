//! Ad-hoc: run SQL statements (args or stdin) against a fresh in-memory engine database.
#[path = "../tests/common/mod.rs"]
mod common;
use std::io::Read;

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = common::engine_session(common::open_store(dir.path()), "q");
    let mut src = std::env::args().skip(1).collect::<Vec<_>>().join(";\n");
    if src.is_empty() {
        std::io::stdin().read_to_string(&mut src).unwrap();
    }
    for stmt in common::statements(&src) {
        let ev = common::engine_query(&mut s, &stmt);
        print!("{}", common::render(&stmt, &ev));
    }
}
