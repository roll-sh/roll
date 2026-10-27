//! Property tests on random input: parsing never panics, finishes quickly, and whatever parses
//! prints back to CQL that parses to the same tree.

use nosql_cql::{Dialect, parse, parse_statement};

/// xorshift64*: deterministic, no dependencies.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const CORPORA: [&str; 3] =
    [include_str!("data/common.cql"), include_str!("data/cassandra.cql"), include_str!("data/scylla.cql")];

const PIECES: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "(", ")", "[", "]", "{", "}", ",", ";", ".", "..", "?", ":x", "'s'", "\"q\"", "$$d$$",
    "1", "-1", "1.5", "1e", "0x", "NaN", "-Infinity", "1h", "P1D", "uuid", "550e8400-e29b-41d4-a716-446655440000", "=",
    "<", "<=", "!=", "+", "-", "*", "/", "%", "+=", "-=", "IN", "NOT", "NULL", "IS", "CONTAINS", "KEY", "TOKEN", "CAST",
    "AS", "int", "frozen", "<", ">", "map", "list", "tuple", "vector", "IF", "EXISTS", "USING", "TTL", "TIMEOUT", "BYPASS",
    "CACHE", "BEGIN", "BATCH", "APPLY", "INSERT", "INTO", "VALUES", "JSON", "UPDATE", "SET", "DELETE", "CREATE", "TABLE",
    "PRIMARY", "WITH", "CLUSTERING", "ORDER", "BY", "GRANT", "ON", "TO", "ROLE", "DESCRIBE", "SERVICE", "LEVEL", "LIST",
    "ALL", "expr(", "--", "/*", "\n", "\t", "é", "@", "'", "\"", "$", "!", "\\", "\0",
];

fn check(src: &str) {
    for d in [Dialect::Cassandra, Dialect::Scylla] {
        if let Ok(st) = parse_statement(src, d) {
            let printed = st.to_string();
            let again = parse_statement(&printed, d)
                .unwrap_or_else(|e| panic!("{d:?}: {src:?}\n  printed {printed:?}\n  fails: {e}"));
            assert_eq!(st.kind, again.kind, "{d:?}: {src:?}\n  printed {printed:?}");
        }
        let _ = parse(src, d);
    }
}

#[test]
fn random_token_soup() {
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    for _ in 0..20_000 {
        let n = 1 + rng.below(16);
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(PIECES[rng.below(PIECES.len())]);
            if rng.below(3) > 0 {
                s.push(' ');
            }
        }
        check(&s);
    }
}

#[test]
fn random_bytes() {
    let mut rng = Rng(0x0dd_ba11);
    for _ in 0..20_000 {
        let n = rng.below(40);
        let bytes: Vec<u8> = (0..n).map(|_| (rng.next() & 0x7f) as u8).collect();
        let s = String::from_utf8_lossy(&bytes);
        check(&s);
    }
}

#[test]
fn mutated_statements() {
    let mut rng = Rng(42);
    let lines: Vec<&str> = CORPORA.iter().flat_map(|c| c.lines()).filter(|l| !l.trim().is_empty()).collect();
    for line in &lines {
        // Every truncation, at every char boundary.
        for (i, _) in line.char_indices() {
            check(&line[..i]);
        }
        for _ in 0..30 {
            let mut s: String = line.to_string();
            match rng.below(4) {
                0 => {
                    let at = s.char_indices().nth(rng.below(s.chars().count())).map_or(0, |(i, _)| i);
                    s.insert_str(at, PIECES[rng.below(PIECES.len())]);
                }
                1 => {
                    let words: Vec<&str> = s.split(' ').collect();
                    let skip = rng.below(words.len());
                    s = words.iter().enumerate().filter(|(i, _)| *i != skip).map(|(_, w)| *w).collect::<Vec<_>>().join(" ");
                }
                2 => {
                    let mut words: Vec<String> = s.split(' ').map(str::to_owned).collect();
                    let at = rng.below(words.len());
                    words[at] = PIECES[rng.below(PIECES.len())].to_owned();
                    s = words.join(" ");
                }
                _ => {
                    let other = lines[rng.below(lines.len())];
                    let cut = other.char_indices().nth(rng.below(other.chars().count())).map_or(0, |(i, _)| i);
                    s.push_str(&other[cut..]);
                }
            }
            check(&s);
        }
    }
}
