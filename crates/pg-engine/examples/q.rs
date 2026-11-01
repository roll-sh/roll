//! Ad-hoc: run SQL statements (args or stdin) against a fresh in-memory engine database.
//! `Q_TIME=1`: each statement's wall time on stderr.
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
    let time = std::env::var_os("Q_TIME").is_some();
    for stmt in common::statements(&src) {
        let t = std::time::Instant::now();
        let ev = common::engine_query(&mut s, &stmt);
        if time {
            eprintln!("{:>10.3} ms  {}", t.elapsed().as_secs_f64() * 1e3, stmt.lines().next().unwrap_or(""));
        }
        print!("{}", common::render(&stmt, &ev));
    }
}
