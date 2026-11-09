//! Parses one statement per input line (`\n`, `\t`, `\r` escapes decoded) and prints
//! `<line>\tOK` or `<line>\tERR <message>`, the format of the reference probes, for diffing.
//!
//! `cargo run -p nosql-cql --example cql_check -- cassandra|scylla < statements.txt`

use std::io::{BufRead, Write};

fn main() {
    let dialect = match std::env::args().nth(1).as_deref() {
        Some("cassandra") => nosql_cql::Dialect::Cassandra,
        _ => nosql_cql::Dialect::Scylla,
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let q = line.replace("\\n", "\n").replace("\\t", "\t").replace("\\r", "\r");
        let res = match nosql_cql::parse_statement(&q, dialect) {
            Ok(_) => "OK".to_owned(),
            Err(e) if e.code == nosql_cql::ErrorCode::Invalid => format!("ERR[InvalidRequest] {}", e.message),
            Err(e) => format!("ERR {}", e.message.replace('\n', "\\n")),
        };
        let _ = writeln!(out, "{line}\t{res}");
    }
}
