//! Differential fuzzer for the bundled modules (ReJSON, RedisBloom's five types, RedisTimeSeries,
//! vector sets): seed-reproducible random command sequences over a small shared keyspace (so
//! commands meet each other's keys: WRONGTYPE, missing keys, mixed module types, RENAME/COPY/
//! RESTORE between them) run against a reference Redis 8 and an in-process roll. Replies are
//! compared byte for byte (RESP2 or RESP3, error texts, number formatting) and so are the
//! keyspace events each command fires. The first mismatch of a seed is minimised (commands
//! that don't share a key with the failing one dropped, then chunks removed while it still
//! fails) and printed as a compat script.
//!
//! ```text
//! ROLL_FUZZ_REDIS=127.0.0.1:55703 ROLL_FUZZ_CONTAINER=fuzz-ref ROLL_FUZZ_SEED=1 \
//!   ROLL_FUZZ_SEEDS=100 ROLL_FUZZ_ITERS=400 \
//!   cargo test -p redis-server --test fuzz_modules -- --ignored --nocapture
//! ```
//!
//! - `ROLL_FUZZ_REDIS` the reference (required; `redis:8-alpine`, used by nothing else: every
//!   run starts with FLUSHALL and sets `notify-keyspace-events`)
//! - `ROLL_FUZZ_CONTAINER` a Docker container name the fuzzer owns, published on
//!   `ROLL_FUZZ_REDIS`: started afresh for the run (the modules keep state across FLUSHALL),
//!   and when the reference dies, the crash is minimised against fresh ones (replaying what it
//!   was sent) and the run goes on with a new one
//! - `ROLL_FUZZ_LOOSE_MEMORY=1` MEMORY USAGE and TS.INFO's memoryUsage not compared
//! - `ROLL_FUZZ_CHECK="EXISTS k;TYPE k"` also compared after every command, `ROLL_FUZZ_PROBE=…`
//!   shown after a mismatch (finding where hidden state diverged), `ROLL_FUZZ_PRINT=1` only
//!   print the generated commands
//! - `ROLL_FUZZ_SEED` first seed (default: from the clock), `ROLL_FUZZ_SEEDS` how many (1)
//! - `ROLL_FUZZ_ITERS` commands per seed (300)
//! - `ROLL_FUZZ_FAMILY` comma list of `json,bf,cf,cms,topk,td,ts,vset,keys` (default: all)
//! - `ROLL_FUZZ_RESP` `2` or `3` (default: picked per seed)
//! - `ROLL_FUZZ_NOTIFY=0` don't compare keyspace events
//! - `ROLL_FUZZ_SKIP=JSON.MERGE,TS.ADD` never send these commands (known gaps)
//! - `ROLL_FUZZ_MINIMISE=0` report unminimised; `ROLL_FUZZ_TRACE=1` print every command
//!
//! What the reference makes random is compared loosely: HNSW levels (VINFO max-level, VLINKS,
//! a vector set's DUMP bytes), VRANDMEMBER, VSIM ties, the order of a multi-path JSON.GET's
//! object. TopK sketches are only created with decay 1 or 1e-300, where `rand()` can't change
//! the outcome. Sizing arguments stay small (the reference allocates whatever it's asked for).

use bytes::BytesMut;
use redis_engine::Config;
use redis_proto::{Reply, decode, encode_command};
use redis_server::{Database, start_single};
use std::collections::HashSet;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------------------------
// PRNG (splitmix64)

#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    /// True with probability `pct`%.
    fn pct(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i64
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())]
    }
    /// An index into `weights`, chosen by weight.
    fn weighted(&mut self, weights: &[usize]) -> usize {
        let total: usize = weights.iter().sum();
        let mut x = self.below(total);
        for (i, w) in weights.iter().enumerate() {
            if x < *w {
                return i;
            }
            x -= w;
        }
        weights.len() - 1
    }
}

// ---------------------------------------------------------------------------------------------
// Commands and how their replies compare

#[derive(Clone, Debug, PartialEq)]
enum Cmp {
    /// Byte for byte.
    Exact,
    /// Arrays (and maps, sets) sorted at every level.
    Sort,
    /// Maps' entries in any order, at every level (RedisJSON's RESP3 FORMAT EXPAND builds them
    /// from a hash map).
    Maps,
    /// A multi-path JSON.GET: the top-level object's members (or map entries) in any order.
    Json(String),
    /// Errors exactly, anything else by type and length.
    Shape,
    /// Errors exactly, anything else only "not an error".
    ErrOnly,
    /// The values after these field names don't count (VINFO's random levels).
    Mask(Vec<&'static str>),
    /// DUMP: exact, except two vector sets' payloads (random HNSW levels).
    Dump,
    /// VSIM: records of `stride` items (element, score, attributes), ties in any order, those
    /// tied with the last one returned only counted (`count` the request's limit).
    Vsim { stride: usize, scores: bool, count: usize },
    /// EXEC: each element as its queued command's.
    Each(Vec<Cmp>),
}

/// Whose DUMP a RESTORE loads.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DumpOf {
    /// The reference's, on both sides.
    Redis,
    /// Roll's, on both sides (roll's payloads must load in Redis).
    Roll,
    /// Each side its own (a round trip; not through Lua, whose cached argument objects end up
    /// in a series' key name: `MEMORY USAGE` would depend on earlier scripts).
    Own,
}

#[derive(Clone, Debug)]
struct Cmd {
    args: Vec<Vec<u8>>,
    cmp: Cmp,
    /// RESTORE with a DUMP of this key as the payload (args[3]), on both sides.
    cross: Option<(Vec<u8>, DumpOf)>,
}

impl Cmd {
    fn name(&self) -> String {
        String::from_utf8_lossy(&self.args[0]).to_ascii_uppercase()
    }
}

fn quote(a: &[u8]) -> String {
    let plain = !a.is_empty()
        && std::str::from_utf8(a).is_ok_and(|s| s.chars().all(|c| !c.is_control() && c != ' ' && c != '"' && c != '\'' && c != '\\'));
    if plain {
        return String::from_utf8_lossy(a).into_owned();
    }
    let mut out = String::from("\"");
    match std::str::from_utf8(a) {
        Ok(s) => {
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c.is_control() => {
                        let mut b = [0; 4];
                        for x in c.encode_utf8(&mut b).bytes() {
                            out.push_str(&format!("\\x{x:02x}"));
                        }
                    }
                    c => out.push(c),
                }
            }
        }
        Err(_) => {
            for &x in a {
                match x {
                    b'"' => out.push_str("\\\""),
                    b'\\' => out.push_str("\\\\"),
                    0x20..=0x7e => out.push(x as char),
                    _ => out.push_str(&format!("\\x{x:02x}")),
                }
            }
        }
    }
    out.push('"');
    out
}

fn line_of(c: &Cmd) -> String {
    let mut s = c.args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    if let Some((src, from)) = &c.cross {
        let side = match from {
            DumpOf::Redis => "the reference's",
            DumpOf::Roll => "roll's",
            DumpOf::Own => "each side's own",
        };
        s = format!("# RESTORE {} 0 <{side} DUMP {}> REPLACE\n{}", quote(&c.args[1]), quote(src), s);
    }
    let tag: String = match &c.cmp {
        Cmp::Exact | Cmp::Dump => String::new(),
        Cmp::Sort | Cmp::Maps => "!sort ".into(),
        Cmp::Json(_) => "!json ".into(),
        Cmp::Shape | Cmp::Vsim { .. } | Cmp::Each(_) => "!type ".into(),
        Cmp::ErrOnly => "!skip ".into(),
        Cmp::Mask(f) => format!("!mask {} ", f.join(",")),
    };
    format!("{tag}{s}")
}

// ---------------------------------------------------------------------------------------------
// Generators

const JSON_KEYS: &[&str] = &["j1", "j2", "j3"];
const BF_KEYS: &[&str] = &["b1", "b2"];
const CF_KEYS: &[&str] = &["c1", "c2"];
const CMS_KEYS: &[&str] = &["m1", "m2", "m3"];
const TOPK_KEYS: &[&str] = &["k1", "k2"];
const TD_KEYS: &[&str] = &["d1", "d2", "d3"];
const TS_KEYS: &[&str] = &["t1", "t2", "t3", "t4"];
const VSET_KEYS: &[&str] = &["v1", "v2", "v3"];
const MISC_KEYS: &[&str] = &["s", "h", "nx"];

const FAMILIES: &[&str] = &["json", "bf", "cf", "cms", "topk", "td", "ts", "vset", "keys"];

fn pool(fam: &str) -> &'static [&'static str] {
    match fam {
        "json" => JSON_KEYS,
        "bf" => BF_KEYS,
        "cf" => CF_KEYS,
        "cms" => CMS_KEYS,
        "topk" => TOPK_KEYS,
        "td" => TD_KEYS,
        "ts" => TS_KEYS,
        "vset" => VSET_KEYS,
        _ => MISC_KEYS,
    }
}

/// Mutations that replace an argument draw from these (no big numbers: sizing arguments are
/// allocated as given).
const GARBAGE: &[&str] = &["", "0", "-1", "1", "x", "nan", "inf", "-inf", "1.5", "+1", " 1", "1 ", "0x10", "-0", "FILTER", "COUNT", "$", ".", "[", "ITEMS"];
/// Where big numbers are safe (parsed values, not sizes).
const BIG: &[&str] = &["9223372036854775807", "9223372036854775808", "-9223372036854775808", "-9223372036854775809", "18446744073709551615", "1e308", "-1e308", "1e400", "4294967295", "4294967296", "2147483648"];

struct Gen {
    r: Rng,
    fams: Vec<&'static str>,
    resp3: bool,
}

type Args = Vec<Vec<u8>>;

fn args<I: IntoIterator<Item = S>, S: AsRef<[u8]>>(xs: I) -> Args {
    xs.into_iter().map(|x| x.as_ref().to_vec()).collect()
}

/// `FORMAT EXPAND`/`EXPAND1`: RESP3 maps built from a hash map.
fn expands(a: &Args) -> bool {
    a.windows(2).any(|w| w[0].eq_ignore_ascii_case(b"FORMAT") && (w[1].eq_ignore_ascii_case(b"EXPAND") || w[1].eq_ignore_ascii_case(b"EXPAND1")))
}

fn plain(a: Args) -> Cmd {
    Cmd { args: a, cmp: Cmp::Exact, cross: None }
}

fn with(a: Args, cmp: Cmp) -> Cmd {
    Cmd { args: a, cmp, cross: None }
}

impl Gen {
    fn key(&mut self, fam: &str) -> String {
        let x = self.r.below(100);
        if x < 82 || fam == "keys" && x < 92 {
            if fam == "keys" {
                let f = self.r.pick(&self.fams);
                return self.r.pick(pool(f)).to_string();
            }
            return self.r.pick(pool(fam)).to_string();
        }
        if x < 93 {
            let f = self.r.pick(&self.fams);
            return self.r.pick(pool(f)).to_string();
        }
        self.r.pick(MISC_KEYS).to_string()
    }

    fn garbage(&mut self) -> String {
        self.r.pick(GARBAGE).to_string()
    }

    /// One step: a command, or a MULTI block.
    fn step(&mut self) -> Vec<Cmd> {
        if self.r.pct(2) {
            let mut v = vec![plain(args(["MULTI"]))];
            for _ in 0..self.r.int(1, 3) {
                let f = self.r.pick(&self.fams);
                v.push(self.one(f));
            }
            let each: Vec<Cmp> = v[1..].iter().map(|c| c.cmp.clone()).collect();
            v.push(if self.r.pct(90) { with(args(["EXEC"]), Cmp::Each(each)) } else { plain(args(["DISCARD"])) });
            return v;
        }
        let f = self.r.pick(&self.fams);
        if f == "vset" && self.r.pct(15) {
            let k = self.key("vset");
            let dim = Self::vdim(&k);
            return (0..self.r.int(2, 6)).map(|_| self.vadd_plain(k.clone(), dim)).collect();
        }
        vec![self.one(f)]
    }

    fn one(&mut self, fam: &str) -> Cmd {
        let mut c = match fam {
            "json" => self.json(),
            "bf" => self.bf(),
            "cf" => self.cf(),
            "cms" => self.cms(),
            "topk" => self.topk(),
            "td" => self.td(),
            "ts" => self.ts(),
            "vset" => self.vset(),
            _ => self.keys(),
        };
        // Arity and syntax: drop, add, replace or duplicate an argument now and then.
        let name = c.name();
        let sizing = matches!(
            name.as_str(),
            "TOPK.RESERVE" | "BF.RESERVE" | "BF.INSERT" | "CF.RESERVE" | "CF.INSERT" | "CF.INSERTNX" | "CMS.INITBYDIM" | "CMS.INITBYPROB" | "TDIGEST.CREATE" | "TDIGEST.MERGE" | "VADD" | "EVAL" | "RESTORE"
        ) || name.starts_with("TS.");
        // Not mutated: TopK's (random decay, short arrays: see `topk`), SWAPDB/MOVE (core), and
        // TS.CREATERULE, where 7 arguments without AGGREGATION third read past argv in the
        // reference (`_parseAlignmentTS`: `TS.CREATERULE a b x AGGREGATION avg 1` crashes it).
        let frozen = matches!(name.as_str(), "TOPK.RESERVE" | "TOPK.INCRBY" | "SWAPDB" | "MOVE" | "TS.CREATERULE");
        if c.cross.is_none() && name != "EVAL" && self.r.pct(5) && !frozen {
            let n = c.args.len();
            match self.r.below(5) {
                0 if n > 1 => {
                    let i = self.r.int(1, n as i64 - 1) as usize;
                    c.args.remove(i);
                }
                1 => {
                    let i = self.r.int(1, n as i64) as usize;
                    let g = if !sizing && self.r.pct(30) { self.r.pick(BIG).to_string() } else { self.garbage() };
                    c.args.insert(i, g.into_bytes());
                }
                2 if n > 1 => {
                    let i = self.r.int(1, n as i64 - 1) as usize;
                    let g = if !sizing && self.r.pct(30) { self.r.pick(BIG).to_string() } else { self.garbage() };
                    c.args[i] = g.into_bytes();
                }
                3 if n > 1 => {
                    let i = self.r.int(1, n as i64 - 1) as usize;
                    let a = c.args[i].clone();
                    c.args.insert(i, a);
                }
                _ => c.args.truncate(self.r.int(1, n as i64) as usize),
            }
            // A mutated VSIM's limits may have moved: compare its shape only.
            if matches!(c.cmp, Cmp::Vsim { .. }) {
                c.cmp = Cmp::ErrOnly;
            }
            // … and a JSON.GET may now have several paths.
            if name == "JSON.GET" {
                let newline = c.args.windows(2).rev().find(|w| w[0].eq_ignore_ascii_case(b"NEWLINE")).map(|w| String::from_utf8_lossy(&w[1]).into_owned());
                c.cmp = if self.resp3 && expands(&c.args) { Cmp::Maps } else { Cmp::Json(newline.unwrap_or_default()) };
            }
        }
        // TS.INCRBY/DECRBY/ADD without an explicit timestamp would take the clock.
        if name.starts_with("TS.") && c.args.iter().any(|a| a == b"*") {
            c.args.retain(|a| a != b"*");
        }
        if (name == "TS.INCRBY" || name == "TS.DECRBY") && c.args.len() >= 3 && !c.args.iter().skip(3).any(|a| a.eq_ignore_ascii_case(b"TIMESTAMP")) {
            c.args.insert(3, b"TIMESTAMP".to_vec());
            c.args.insert(4, b"0".to_vec());
        }
        // `ROLL_FUZZ_LOOSE_MEMORY=1`: memory sizes not compared (a series' share of the label
        // index depends on how its radix trees were reshaped by earlier series).
        if std::env::var("ROLL_FUZZ_LOOSE_MEMORY").is_ok_and(|v| v == "1") {
            match name.as_str() {
                "MEMORY" => c.cmp = Cmp::Shape,
                "TS.INFO" => c.cmp = Cmp::Mask(vec!["memoryUsage"]),
                _ => {}
            }
        }
        // A negative TIMESTAMP is taken as 2^64 - n (see `ts_ts`).
        if let Some(i) = c.args.iter().position(|a| a.eq_ignore_ascii_case(b"TIMESTAMP"))
            && (name == "TS.INCRBY" || name == "TS.DECRBY")
            && let Some(t) = c.args.get_mut(i + 1)
        {
            t.retain(|b| *b != b'-');
        }
        c
    }

    // ---- JSON ----

    fn jnum(&mut self) -> String {
        if self.r.pct(40) {
            return self.r.int(-20, 20).to_string();
        }
        if self.r.pct(20) {
            return format!("{}.{}", self.r.int(-100, 100), self.r.int(0, 999));
        }
        self.r
            .pick(&[
                "0", "-0", "1", "-1", "42", "3.14", "-2.5", "1e3", "1E-7", "2.5e+10", "1e308", "-1e308", "1.7976931348623157e308", "5e-324", "123456789012345678",
                "9007199254740993", "9223372036854775807", "-9223372036854775808", "18446744073709551615", "18446744073709551616", "0.1", "100.0", "1.0", "-0.0", "0.30000000000000004",
                "1e-320", "12345678901234567890123", "1.5e300", "-1.5e-300", "4294967296", "2147483647",
            ])
            .to_string()
    }

    fn jstr(&mut self) -> String {
        self.r
            .pick(&[
                "\"\"", "\"a\"", "\"hello\"", "\"é\"", "\"\\u00e9\"", "\"\\n\\t\"", "\"\\\"q\\\"\"", "\"\\ud83d\\ude00\"", "\"😀\"", "\"a b\"", "\"\\/\"", "\"\\u0000\"", "\"日本\"",
                "\"\\\\\"", "\"abc\"", "\"x\"", "\"1\"", "\"true\"", "\"\\b\\f\\r\"", "\"\\u001f\"", "\"\\u2028\"",
            ])
            .to_string()
    }

    fn jkey(&mut self) -> &'static str {
        self.r.pick(&["a", "b", "c", "x", "y", "arr", "obj", "n", "s", "é", "a b", "", "0", "$", "a.b"])
    }

    fn json_escape(k: &str) -> String {
        let mut s = String::from("\"");
        for c in k.chars() {
            match c {
                '"' => s.push_str("\\\""),
                '\\' => s.push_str("\\\\"),
                c => s.push(c),
            }
        }
        s.push('"');
        s
    }

    fn ws(&mut self) -> &'static str {
        if self.r.pct(90) { "" } else { self.r.pick(&[" ", "\n", "\t", "  ", "\r\n"]) }
    }

    fn jval(&mut self, depth: usize) -> String {
        let deep = depth < 3;
        match self.r.weighted(&[30, 18, 8, 5, if deep { 16 } else { 0 }, if deep { 20 } else { 0 }]) {
            0 => self.jnum(),
            1 => self.jstr(),
            2 => self.r.pick(&["true", "false"]).to_string(),
            3 => "null".into(),
            4 => {
                let n = self.r.below(5);
                let items: Vec<String> = (0..n).map(|_| self.jval(depth + 1)).collect();
                let (a, b) = (self.ws(), self.ws());
                format!("[{a}{}{b}]", items.join(&format!(",{}", self.ws())))
            }
            _ => {
                let n = self.r.below(5);
                let mut seen = HashSet::new();
                let mut items = Vec::new();
                for _ in 0..n {
                    let k = self.jkey();
                    if !seen.insert(k) && self.r.pct(90) {
                        continue;
                    }
                    let v = self.jval(depth + 1);
                    let w = self.ws();
                    items.push(format!("{}:{w}{v}", Self::json_escape(k)));
                }
                format!("{{{}}}", items.join(","))
            }
        }
    }

    fn jdoc(&mut self) -> String {
        if self.r.pct(4) {
            return self
                .r
                .pick(&[
                    "{", "[1,]", "{\"a\":}", "01", "nan", "'x'", "{\"a\":1}x", "[", "\"unterminated", "tru", "{\"a\" 1}", "1 2", "", "[1,2]]", "{\"a\":1,}", "NaN", "Infinity",
                    "-", "1.", ".5", "+1", "\"\\x\"", "{\"a\":1 /*c*/}", "[01]", "\"\\ud800\"",
                ])
                .to_string();
        }
        match self.r.below(100) {
            0..70 => {
                let mut s = self.jval(0);
                while !s.starts_with('{') {
                    s = self.jval(0);
                }
                s
            }
            70..85 => format!("[{},{},{}]", self.jval(1), self.jval(1), self.jval(2)),
            _ => self.jval(2),
        }
    }

    fn jfilter_val(&mut self) -> String {
        match self.r.below(8) {
            0 | 1 => self.jnum(),
            2 => format!("'{}'", self.r.pick(&["a", "abc", "x", "", "é"])),
            3 => self.jstr(),
            4 => self.r.pick(&["true", "false", "null"]).to_string(),
            5 => format!("@.{}", self.r.pick(&["a", "b", "x"])),
            6 => format!("$.{}", self.r.pick(&["a", "n", "x"])),
            _ => self.r.int(-3, 5).to_string(),
        }
    }

    fn jfilter_atom(&mut self) -> String {
        let lhs = match self.r.below(4) {
            0 => "@".to_string(),
            1 => format!("@.{}", self.r.pick(&["a", "b", "x", "n", "s"])),
            2 => format!("@['{}']", self.r.pick(&["a", "b", "a b"])),
            _ => format!("@.{}.{}", self.r.pick(&["obj", "a"]), self.r.pick(&["x", "a"])),
        };
        match self.r.below(10) {
            0 => lhs,
            1 => format!("{lhs} =~ \"{}\"", self.r.pick(&["^a", "b", "(?i)A", ".*", "[", "a|b"])),
            2 => format!("!{lhs}"),
            _ => {
                let op = self.r.pick(&["==", "!=", "<", "<=", ">", ">=", "=="]);
                let v = self.jfilter_val();
                let w = self.r.pick(&[" ", "", " "]);
                format!("{lhs}{w}{op}{w}{v}")
            }
        }
    }

    fn jfilter(&mut self) -> String {
        let mut s = self.jfilter_atom();
        for _ in 0..self.r.below(3) {
            let op = self.r.pick(&["&&", "||"]);
            let b = self.jfilter_atom();
            s = if self.r.pct(20) { format!("({s}) {op} {b}") } else { format!("{s} {op} {b}") };
        }
        s
    }

    fn jpath(&mut self) -> String {
        let x = self.r.below(100);
        if x < 6 {
            return self
                .r
                .pick(&[
                    "$[", "$.a[", "..", "$..", "$.", ".a.", "$[?(@.a>)]", "[", "a..b", "$['a'", "$.a[1:2:0]", "$[?(@.a==1]", "$a", "$.[0]", "$[1,", "$.a.", "$[?()]",
                    "$['a'b]", "$[\"a]", "@", "$[1:2:3:4]", "$[a]",
                ])
                .to_string();
        }
        if x < 22 {
            return self
                .r
                .pick(&[
                    ".", ".a", "a", ".a.b", ".arr[0]", ".arr[-1]", "[0]", ".obj.x", "a.b", ".a[1]", "['a']", ".b", ".x", ".n", ".s", "[\"a\"]", ".arr", ".obj", ".a[0].b",
                    "[-1]", ".é",
                ])
                .to_string();
        }
        let mut p = String::from("$");
        for _ in 0..self.r.below(4) {
            let seg = match self.r.weighted(&[25, 8, 8, 3, 12, 6, 6, 5, 3, 3, 12]) {
                0 => format!(".{}", self.r.pick(&["a", "b", "c", "x", "y", "arr", "obj", "n", "s", "é"])),
                1 => format!("..{}", self.r.pick(&["a", "b", "x", "arr", "n"])),
                2 => ".*".into(),
                3 => "..*".into(),
                4 => format!("[{}]", self.r.int(-3, 3)),
                5 => "[*]".into(),
                6 => {
                    let a = if self.r.pct(70) { self.r.int(-3, 3).to_string() } else { String::new() };
                    let b = if self.r.pct(70) { self.r.int(-3, 4).to_string() } else { String::new() };
                    if self.r.pct(25) { format!("[{a}:{b}:{}]", self.r.int(-2, 3)) } else { format!("[{a}:{b}]") }
                }
                7 => format!("['{}']", self.jkey()),
                8 => format!("[\"{}\",\"{}\"]", self.jkey(), self.jkey()),
                9 => format!("[{},{}]", self.r.int(-2, 3), self.r.int(-2, 3)),
                _ => format!("[?({})]", self.jfilter()),
            };
            p.push_str(&seg);
        }
        p
    }

    /// A path that targets a document's root often enough for documents to exist.
    fn jset_path(&mut self) -> String {
        if self.r.pct(45) { self.r.pick(&["$", ".", "$", "."]).to_string() } else { self.jpath() }
    }

    fn json(&mut self) -> Cmd {
        let k = self.key("json");
        let w = [18, 14, 4, 3, 4, 2, 4, 4, 2, 2, 4, 3, 5, 4, 4, 3, 4, 3, 3, 3, 3, 3, 4, 3, 2, 1, 2];
        match self.r.weighted(&w) {
            0 => {
                let mut a = args(["JSON.SET".to_string(), k, self.jset_path(), self.jdoc()]);
                if self.r.pct(20) {
                    a.push(self.r.pick(&["NX", "XX", "nx"]).into());
                }
                if self.r.pct(4) {
                    a.push(b"FPHA".to_vec());
                    a.push(self.r.pick(&["FP16", "BF16", "FP32", "FP64", "fp32", "INT8"]).into());
                }
                plain(a)
            }
            1 => {
                let mut a = args(["JSON.GET".to_string(), k]);
                if self.r.pct(20) {
                    for opt in ["INDENT", "NEWLINE", "SPACE"] {
                        if self.r.pct(60) {
                            a.push(opt.into());
                            a.push(self.r.pick(&["\t", "  ", "", "\n", "\r\n", "ab", " "]).into());
                        }
                    }
                }
                if self.r.pct(8) {
                    a.push(b"FORMAT".to_vec());
                    a.push(self.r.pick(&["STRING", "EXPAND1", "EXPAND", "string", "BAD"]).into());
                }
                let n = self.r.weighted(&[20, 60, 15, 5]);
                for _ in 0..n {
                    a.push(self.jpath().into_bytes());
                }
                let newline = a.windows(2).rev().find(|w| w[0].eq_ignore_ascii_case(b"NEWLINE")).map(|w| String::from_utf8_lossy(&w[1]).into_owned());
                let expand = self.resp3 && expands(&a);
                with(a, if expand { Cmp::Maps } else if n > 1 { Cmp::Json(newline.unwrap_or_default()) } else { Cmp::Exact })
            }
            2 => {
                let mut a = args(["JSON.MGET"]);
                for _ in 0..self.r.int(1, 3) {
                    a.push(self.key("json").into_bytes());
                }
                a.push(self.jpath().into_bytes());
                plain(a)
            }
            3 => {
                let mut a = args(["JSON.MSET"]);
                for _ in 0..self.r.int(1, 3) {
                    a.push(self.key("json").into_bytes());
                    a.push(self.jset_path().into_bytes());
                    a.push(self.jdoc().into_bytes());
                }
                plain(a)
            }
            4 | 5 => {
                let mut a = args([if self.r.pct(70) { "JSON.DEL" } else { "JSON.FORGET" }.to_string(), k]);
                if self.r.pct(85) {
                    a.push(self.jpath().into_bytes());
                }
                plain(a)
            }
            6 => self.json_opt_path("JSON.TYPE", k),
            7 => {
                let op = self.r.pick(&["JSON.NUMINCRBY", "JSON.NUMMULTBY", "JSON.NUMINCRBY", "JSON.NUMPOWBY"]);
                let v = if self.r.pct(10) { self.r.pick(BIG).to_string() } else if self.r.pct(5) { self.garbage() } else { self.jnum() };
                plain(args([op.to_string(), k, self.jpath(), v]))
            }
            8 => {
                let mut a = args(["JSON.STRAPPEND".to_string(), k]);
                if self.r.pct(85) {
                    a.push(self.jpath().into_bytes());
                }
                a.push(if self.r.pct(85) { self.jstr() } else { self.jval(2) }.into_bytes());
                plain(a)
            }
            9 => self.json_opt_path("JSON.STRLEN", k),
            10 => {
                let mut a = args(["JSON.ARRAPPEND".to_string(), k]);
                if self.r.pct(90) {
                    a.push(self.jpath().into_bytes());
                }
                for _ in 0..self.r.int(1, 3) {
                    a.push(self.jval(2).into_bytes());
                }
                plain(a)
            }
            11 => {
                let mut a = args(["JSON.ARRINDEX".to_string(), k, self.jpath(), self.jval(2)]);
                if self.r.pct(50) {
                    a.push(self.r.int(-4, 4).to_string().into_bytes());
                    if self.r.pct(50) {
                        a.push(self.r.int(-4, 5).to_string().into_bytes());
                    }
                }
                plain(a)
            }
            12 => {
                let mut a = args(["JSON.ARRINSERT".to_string(), k, self.jpath(), self.r.int(-4, 4).to_string()]);
                for _ in 0..self.r.int(1, 3) {
                    a.push(self.jval(2).into_bytes());
                }
                plain(a)
            }
            13 => self.json_opt_path("JSON.ARRLEN", k),
            14 => {
                let mut a = args(["JSON.ARRPOP".to_string(), k]);
                let format = self.r.pct(8);
                if format {
                    a.push(b"FORMAT".to_vec());
                    a.push(self.r.pick(&["STRINGS", "EXPAND1", "EXPAND", "STRING", "BAD"]).into());
                }
                if self.r.pct(85) {
                    a.push(self.jpath().into_bytes());
                    if self.r.pct(50) {
                        a.push(if self.r.pct(90) { self.r.int(-4, 4).to_string() } else { self.r.pick(BIG).to_string() }.into_bytes());
                    }
                }
                with(a, if format { Cmp::Maps } else { Cmp::Exact })
            }
            15 => plain(args(["JSON.ARRTRIM".to_string(), k, self.jpath(), self.r.int(-4, 4).to_string(), self.r.int(-4, 6).to_string()])),
            16 => self.json_opt_path("JSON.OBJKEYS", k),
            17 => self.json_opt_path("JSON.OBJLEN", k),
            18 => plain(args(["JSON.TOGGLE".to_string(), k, self.jpath()])),
            19 => self.json_opt_path("JSON.CLEAR", k),
            20 | 21 => plain(args(["JSON.MERGE".to_string(), k, self.jset_path(), if self.r.pct(20) { "null".to_string() } else { self.jdoc() }])),
            22 => self.json_opt_path("JSON.RESP", k),
            23 => {
                let mut a = args(["JSON.DEBUG", "MEMORY"]);
                a.push(k.into_bytes());
                if self.r.pct(70) {
                    a.push(self.jpath().into_bytes());
                }
                plain(a)
            }
            24 => plain(if self.r.pct(50) { args(["JSON.DEBUG"]) } else { args(["JSON.DEBUG", self.r.pick(&["HELP", "help", "BAD", "MEMORY"])]) }),
            25 => plain(args(["JSON.GET".to_string(), k])),
            _ => plain(args(["JSON.SET".to_string(), k, "$".to_string(), self.jdoc()])),
        }
    }

    fn json_opt_path(&mut self, cmd: &str, k: String) -> Cmd {
        let mut a = args([cmd.to_string(), k]);
        if self.r.pct(85) {
            a.push(self.jpath().into_bytes());
        }
        plain(a)
    }

    // ---- Bloom ----

    fn item(&mut self) -> String {
        if self.r.pct(50) {
            return format!("i{}", self.r.below(120));
        }
        self.r.pick(&["a", "b", "c", "item1", "item2", "x", "y", "z", "foo", "bar", "", "日本", "1", "2", "3", "a b", "\u{0}"]).to_string()
    }

    fn items(&mut self, a: &mut Args, max: i64) {
        for _ in 0..self.r.int(1, max) {
            a.push(self.item().into_bytes());
        }
    }

    fn scandump_roundtrip(&mut self, cmd: &str, src: String, dst: String) -> Cmd {
        let script = format!(
            "redis.call('DEL', KEYS[2]); local it = 0; local n = 0; while true do local r = redis.call('{cmd}.SCANDUMP', KEYS[1], it); it = r[1]; if it == 0 then break end; redis.call('{cmd}.LOADCHUNK', KEYS[2], it, r[2]); n = n + 1 end; return n"
        );
        plain(args(["EVAL".to_string(), script, "2".into(), src, dst]))
    }

    fn bf(&mut self) -> Cmd {
        let k = self.key("bf");
        match self.r.weighted(&[8, 14, 10, 8, 8, 8, 5, 4, 4, 3, 3, 2]) {
            0 => {
                let mut a = args(["BF.RESERVE".to_string(), k]);
                a.push(self.r.pick(&["0.01", "0.001", "0.1", "0.5", "0", "1", "-1", "x", "0.9999", "1e-5", "0.05"]).into());
                a.push(self.r.pick(&["1", "2", "3", "10", "100", "1000", "0", "-1", "x", "7", "50"]).into());
                if self.r.pct(30) {
                    a.push(b"EXPANSION".to_vec());
                    a.push(self.r.pick(&["1", "2", "4", "0", "-1", "x", "3"]).into());
                }
                if self.r.pct(20) {
                    a.push(b"NONSCALING".to_vec());
                }
                plain(a)
            }
            1 => plain(args(["BF.ADD".to_string(), k, self.item()])),
            2 => {
                let mut a = args(["BF.MADD".to_string(), k]);
                self.items(&mut a, 20);
                plain(a)
            }
            3 => plain(args(["BF.EXISTS".to_string(), k, self.item()])),
            4 => {
                let mut a = args(["BF.MEXISTS".to_string(), k]);
                self.items(&mut a, 8);
                plain(a)
            }
            5 => {
                let mut a = args(["BF.INSERT".to_string(), k]);
                let mut opts: Vec<Args> = Vec::new();
                if self.r.pct(40) {
                    opts.push(args(["CAPACITY", self.r.pick(&["1", "5", "100", "0", "-1", "x"])]));
                }
                if self.r.pct(40) {
                    opts.push(args(["ERROR", self.r.pick(&["0.01", "0.1", "0", "1", "x", "0.001"])]));
                }
                if self.r.pct(20) {
                    opts.push(args(["EXPANSION", self.r.pick(&["1", "2", "0", "x"])]));
                }
                if self.r.pct(20) {
                    opts.push(args(["NOCREATE"]));
                }
                if self.r.pct(15) {
                    opts.push(args(["NONSCALING"]));
                }
                for o in opts {
                    a.extend(o);
                }
                if self.r.pct(95) {
                    a.push(b"ITEMS".to_vec());
                }
                self.items(&mut a, 10);
                plain(a)
            }
            6 => {
                let mut a = args(["BF.INFO".to_string(), k]);
                if self.r.pct(50) {
                    a.push(self.r.pick(&["CAPACITY", "SIZE", "FILTERS", "ITEMS", "EXPANSION", "capacity", "BAD"]).into());
                }
                plain(a)
            }
            7 => plain(args(["BF.CARD".to_string(), k])),
            8 => plain(args(["BF.SCANDUMP".to_string(), k, self.r.pick(&["0", "1", "2", "3", "-1", "x", "100"]).to_string()])),
            9 => {
                let d = self.key("bf");
                self.scandump_roundtrip("BF", k, d)
            }
            10 => plain(args(["BF.LOADCHUNK".to_string(), k, self.r.pick(&["1", "2", "0", "-1", "x"]).to_string(), self.r.pick(&["", "x", "\u{0}\u{0}\u{0}\u{0}", "garbage-garbage-garbage"]).to_string()])),
            _ => plain(args(["BF.DEBUG".to_string(), k])),
        }
    }

    fn cf(&mut self) -> Cmd {
        let k = self.key("cf");
        match self.r.weighted(&[8, 12, 6, 6, 6, 6, 5, 6, 4, 4, 3, 3, 2, 2, 2]) {
            0 => {
                let mut a = args(["CF.RESERVE".to_string(), k]);
                a.push(self.r.pick(&["1", "2", "4", "10", "64", "1000", "0", "-1", "x", "3", "100"]).into());
                if self.r.pct(30) {
                    a.push(b"BUCKETSIZE".to_vec());
                    a.push(self.r.pick(&["1", "2", "4", "255", "0", "256", "x", "3"]).into());
                }
                if self.r.pct(30) {
                    a.push(b"MAXITERATIONS".to_vec());
                    a.push(self.r.pick(&["1", "20", "500", "0", "65536", "65535", "x"]).into());
                }
                if self.r.pct(30) {
                    a.push(b"EXPANSION".to_vec());
                    a.push(self.r.pick(&["0", "1", "2", "4", "32768", "32769", "x", "3"]).into());
                }
                plain(a)
            }
            1 => plain(args(["CF.ADD".to_string(), k, self.item()])),
            2 => plain(args(["CF.ADDNX".to_string(), k, self.item()])),
            3 => plain(args(["CF.COUNT".to_string(), k, self.item()])),
            4 => plain(args(["CF.DEL".to_string(), k, self.item()])),
            5 => plain(args(["CF.EXISTS".to_string(), k, self.item()])),
            6 => {
                let mut a = args(["CF.MEXISTS".to_string(), k]);
                self.items(&mut a, 8);
                plain(a)
            }
            7 | 8 => {
                let mut a = args([if self.r.pct(50) { "CF.INSERT" } else { "CF.INSERTNX" }.to_string(), k]);
                if self.r.pct(40) {
                    a.extend(args(["CAPACITY", self.r.pick(&["1", "4", "100", "0", "-1", "x"])]));
                }
                if self.r.pct(20) {
                    a.push(b"NOCREATE".to_vec());
                }
                if self.r.pct(95) {
                    a.push(b"ITEMS".to_vec());
                }
                self.items(&mut a, 12);
                plain(a)
            }
            9 => plain(args(["CF.INFO".to_string(), k])),
            10 => plain(args(["CF.SCANDUMP".to_string(), k, self.r.pick(&["0", "1", "2", "9", "17", "-1", "x"]).to_string()])),
            11 => {
                let d = self.key("cf");
                self.scandump_roundtrip("CF", k, d)
            }
            12 => plain(args(["CF.COMPACT".to_string(), k])),
            13 => plain(args(["CF.DEBUG".to_string(), k])),
            _ => plain(args(["CF.LOADCHUNK".to_string(), k, self.r.pick(&["1", "2", "0", "-1", "x", "9"]).to_string(), self.r.pick(&["", "x", "\u{0}\u{0}\u{0}\u{0}", "garbage-garbage-garbage"]).to_string()])),
        }
    }

    fn cms(&mut self) -> Cmd {
        let k = self.key("cms");
        match self.r.weighted(&[6, 6, 16, 12, 5, 8]) {
            0 => plain(args(["CMS.INITBYDIM".to_string(), k, self.r.pick(&["1", "2", "10", "100", "2000", "0", "-1", "x"]).into(), self.r.pick(&["1", "2", "5", "10", "0", "-1", "x"]).into()])),
            1 => plain(args(["CMS.INITBYPROB".to_string(), k, self.r.pick(&["0.01", "0.001", "0.1", "0.5", "0", "1", "-1", "x", "2"]).into(), self.r.pick(&["0.01", "0.5", "0.99", "0", "1", "x", "0.001"]).into()])),
            2 => {
                let mut a = args(["CMS.INCRBY".to_string(), k]);
                for _ in 0..self.r.int(1, 5) {
                    a.push(self.item().into_bytes());
                    let v = if self.r.pct(90) { self.r.int(-2, 50).to_string() } else { self.r.pick(BIG).to_string() };
                    a.push(v.into_bytes());
                }
                plain(a)
            }
            3 => {
                let mut a = args(["CMS.QUERY".to_string(), k]);
                self.items(&mut a, 6);
                plain(a)
            }
            4 => plain(args(["CMS.INFO".to_string(), k])),
            _ => {
                let n = self.r.int(1, 3);
                let mut a = args(["CMS.MERGE".to_string(), k, if self.r.pct(95) { n.to_string() } else { self.r.pick(&["0", "-1", "x", "5"]).to_string() }]);
                for _ in 0..n {
                    a.push(self.key("cms").into_bytes());
                }
                if self.r.pct(40) {
                    a.push(b"WEIGHTS".to_vec());
                    for _ in 0..n {
                        a.push(self.r.pick(&["1", "2", "-1", "0", "3", "x", "1.5", "1000000000000"]).into());
                    }
                }
                plain(a)
            }
        }
    }

    fn topk(&mut self) -> Cmd {
        let k = self.key("topk");
        match self.r.weighted(&[6, 14, 8, 8, 6, 6, 4, 1]) {
            0 => {
                let kk = self.r.pick(&["1", "2", "3", "5", "0", "x"]);
                let w = self.r.pick(&["1", "2", "8", "50", "500", "0", "x"]);
                let d = self.r.pick(&["1", "2", "3", "5", "0", "x"]);
                let decay = self.r.pick(&["1", "1e-300", "1", "0", "2", "x"]);
                plain(args(["TOPK.RESERVE", &k, kk, w, d, decay]))
            }
            1 => {
                let mut a = args(["TOPK.ADD".to_string(), k]);
                self.items(&mut a, 10);
                plain(a)
            }
            2 => {
                let mut a = args(["TOPK.INCRBY".to_string(), k]);
                for _ in 0..self.r.int(1, 4) {
                    a.push(self.item().into_bytes());
                    // An invalid increment ends the module's reply short of the array it
                    // announced: only in the last pair, where the array is complete.
                    a.push(self.r.pick(&["1", "2", "5", "10", "100", "0", "100000"]).into());
                }
                if self.r.pct(10) {
                    let n = a.len();
                    a[n - 1] = self.r.pick(&["-1", "100001", "x", ""]).into();
                }
                plain(a)
            }
            3 => {
                let mut a = args(["TOPK.QUERY".to_string(), k]);
                self.items(&mut a, 6);
                plain(a)
            }
            4 => {
                let mut a = args(["TOPK.COUNT".to_string(), k]);
                self.items(&mut a, 6);
                plain(a)
            }
            5 => {
                let mut a = args(["TOPK.LIST".to_string(), k]);
                if self.r.pct(50) {
                    a.push(self.r.pick(&["WITHCOUNT", "withcount", "BAD"]).into());
                }
                plain(a)
            }
            6 => plain(args(["TOPK.INFO".to_string(), k])),
            // The defaults decay at 0.9 (random): their arity only.
            _ => plain(args(["TOPK.RESERVE".to_string(), k, "2".into(), "8".into(), "7".into()])),
        }
    }

    fn tdval(&mut self) -> String {
        if self.r.pct(60) {
            return self.r.int(-50, 100).to_string();
        }
        if self.r.pct(50) {
            return format!("{}.{}", self.r.int(-100, 100), self.r.int(0, 99));
        }
        self.r.pick(&["0", "-0", "1e308", "-1e308", "1e-300", "nan", "inf", "-inf", "x", "1.7976931348623157e308", "5e-324", "0.5", "1e20", "-1e20"]).to_string()
    }

    fn td(&mut self) -> Cmd {
        let k = self.key("td");
        match self.r.weighted(&[6, 16, 5, 8, 6, 6, 6, 5, 5, 4, 3, 3, 3, 2]) {
            0 => {
                let mut a = args(["TDIGEST.CREATE".to_string(), k]);
                if self.r.pct(50) {
                    a.push(b"COMPRESSION".to_vec());
                    a.push(self.r.pick(&["1", "10", "100", "500", "0", "-1", "1000", "x", "1.5", "2"]).into());
                }
                plain(a)
            }
            1 => {
                let mut a = args(["TDIGEST.ADD".to_string(), k]);
                for _ in 0..self.r.int(1, 12) {
                    a.push(self.tdval().into_bytes());
                }
                plain(a)
            }
            2 => {
                let n = self.r.int(1, 3);
                let mut a = args(["TDIGEST.MERGE".to_string(), k, if self.r.pct(95) { n.to_string() } else { self.r.pick(&["0", "-1", "x", "5"]).to_string() }]);
                for _ in 0..n {
                    a.push(self.key("td").into_bytes());
                }
                if self.r.pct(30) {
                    a.push(b"COMPRESSION".to_vec());
                    a.push(self.r.pick(&["1", "50", "100", "0", "-1", "x"]).into());
                }
                if self.r.pct(30) {
                    a.push(b"OVERRIDE".to_vec());
                }
                plain(a)
            }
            3 => {
                let mut a = args(["TDIGEST.QUANTILE".to_string(), k]);
                for _ in 0..self.r.int(1, 4) {
                    a.push(self.r.pick(&["0", "0.1", "0.5", "0.99", "1", "-0.1", "1.1", "0.25", "0.75", "x", "0.001", "0.999"]).into());
                }
                plain(a)
            }
            4 => {
                let mut a = args(["TDIGEST.CDF".to_string(), k]);
                for _ in 0..self.r.int(1, 4) {
                    a.push(self.tdval().into_bytes());
                }
                plain(a)
            }
            5 | 6 => {
                let mut a = args([if self.r.pct(50) { "TDIGEST.RANK" } else { "TDIGEST.REVRANK" }.to_string(), k]);
                for _ in 0..self.r.int(1, 4) {
                    a.push(self.tdval().into_bytes());
                }
                plain(a)
            }
            7 | 8 => {
                let mut a = args([if self.r.pct(50) { "TDIGEST.BYRANK" } else { "TDIGEST.BYREVRANK" }.to_string(), k]);
                for _ in 0..self.r.int(1, 4) {
                    a.push(self.r.pick(&["0", "1", "2", "3", "5", "-1", "100", "10000000000", "x", "1.5", "inf", "9223372036854775807"]).into());
                }
                plain(a)
            }
            9 => plain(args(["TDIGEST.TRIMMED_MEAN".to_string(), k, self.r.pick(&["0", "0.1", "0.25", "0.5", "-0.1", "1", "x"]).into(), self.r.pick(&["1", "0.9", "0.75", "0.5", "0.1", "1.1", "x"]).into()])),
            10 => plain(args(["TDIGEST.MIN".to_string(), k])),
            11 => plain(args(["TDIGEST.MAX".to_string(), k])),
            12 => plain(args(["TDIGEST.INFO".to_string(), k])),
            _ => plain(args(["TDIGEST.RESET".to_string(), k])),
        }
    }

    // ---- TimeSeries ----

    /// Sample timestamps stay small: the reference crashes on an EMPTY aggregation over a
    /// series spanning ~2^63 ms (`TS.ADD t 9223372036854775807 1`, `TS.ADD t 5 1`,
    /// `TS.REVRANGE t - + AGGREGATION avg 33 EMPTY`; `TS.INCRBY t 1 TIMESTAMP -1` stores at
    /// 2^64 - 1 for the same effect).
    fn ts_ts(&mut self) -> String {
        match self.r.below(100) {
            0..70 => self.r.int(0, 300).to_string(),
            70..90 => self.r.int(300, 3000).to_string(),
            _ => self.r.pick(&["0", "-1", "x", "1.5", "18446744073709551616", "-9223372036854775808", "100000", "1e3", "100", "200"]).to_string(),
        }
    }

    fn ts_val(&mut self) -> String {
        match self.r.below(100) {
            0..60 => self.r.int(-10, 30).to_string(),
            60..85 => format!("{}.{}", self.r.int(-20, 20), self.r.int(0, 999)),
            _ => self.r.pick(&["nan", "NaN", "inf", "-inf", "1e308", "-1e308", "x", "0", "-0", "1e-300", "0.1", "1e20", "5e-324"]).to_string(),
        }
    }

    fn ts_range_bound(&mut self) -> String {
        match self.r.below(10) {
            0..3 => "-".into(),
            3..6 => "+".into(),
            6 => self.r.pick(&["x", "-1", "18446744073709551615", "1.5"]).to_string(),
            _ => self.r.int(0, 400).to_string(),
        }
    }

    fn labels(&mut self, a: &mut Args) {
        a.push(b"LABELS".to_vec());
        let lo = if self.r.pct(5) { 0 } else { 1 };
        for _ in 0..self.r.int(lo, 3) {
            let l = self.r.pick(&["area", "type", "n", "name", "x"]);
            let v = self.r.pick(&["us", "eu", "a", "b", "c", "1", "2", "", "a b"]);
            a.push(l.into());
            a.push(v.into());
        }
    }

    fn policy(&mut self) -> &'static str {
        self.r.pick(&["BLOCK", "FIRST", "LAST", "MIN", "MAX", "SUM", "last", "BAD"])
    }

    fn ts_create_opts(&mut self, a: &mut Args, alter: bool, incr: bool) {
        if self.r.pct(30) {
            a.push(b"RETENTION".to_vec());
            a.push(self.r.pick(&["0", "10", "100", "1000", "50", "-1", "x"]).into());
        }
        if !alter && self.r.pct(20) {
            if incr && self.r.pct(50) {
                a.push(b"UNCOMPRESSED".to_vec());
            } else {
                a.push(b"ENCODING".to_vec());
                a.push(self.r.pick(&["COMPRESSED", "UNCOMPRESSED", "compressed", "BAD"]).into());
            }
        }
        // Not on ALTER: a smaller chunk size, then an upsert, loses samples and corrupts the series
        // in the reference (`TS.ADD t 2332 1`, `TS.ALTER t CHUNK_SIZE 64`, `TS.ADD t 128 1`,
        // `TS.DEL t 36 2398`, `TS.ADD t 1072 1` crashes it; roll answers the same up to there).
        if !alter && self.r.pct(20) {
            a.push(b"CHUNK_SIZE".to_vec());
            a.push(self.r.pick(&["48", "64", "128", "256", "4096", "8", "0", "7", "1048576", "1048577", "x"]).into());
        }
        if self.r.pct(25) {
            a.push(b"DUPLICATE_POLICY".to_vec());
            a.push(self.policy().into());
        }
        if self.r.pct(12) {
            a.push(b"IGNORE".to_vec());
            a.push(self.r.pick(&["0", "5", "10", "-1", "x"]).into());
            a.push(self.r.pick(&["0", "0.5", "2", "-1", "x"]).into());
        }
        if self.r.pct(40) {
            self.labels(a);
        }
    }

    fn agg(&mut self) -> &'static str {
        self.r.pick(&["avg", "first", "last", "min", "max", "sum", "range", "count", "std.p", "std.s", "var.p", "var.s", "twa", "countnan", "countall", "AVG", "bad"])
    }

    /// RANGE-family options after `from to`.
    fn ts_range_opts(&mut self, a: &mut Args, multi: bool, nkeys: usize) {
        if self.r.pct(15) {
            a.push(b"LATEST".to_vec());
        }
        if self.r.pct(15) {
            a.push(b"FILTER_BY_TS".to_vec());
            for _ in 0..self.r.int(1, 4) {
                a.push(self.ts_ts().into_bytes());
            }
        }
        if self.r.pct(15) {
            a.push(b"FILTER_BY_VALUE".to_vec());
            a.push(self.ts_val().into_bytes());
            a.push(self.ts_val().into_bytes());
        }
        if multi && self.r.pct(30) {
            if self.r.pct(50) {
                a.push(b"WITHLABELS".to_vec());
            } else {
                a.push(b"SELECTED_LABELS".to_vec());
                for _ in 0..self.r.int(1, 2) {
                    a.push(self.r.pick(&["area", "type", "n", "zz"]).into());
                }
            }
        }
        if self.r.pct(20) {
            a.push(b"COUNT".to_vec());
            a.push(self.r.pick(&["0", "1", "2", "3", "10", "-1", "x"]).into());
        }
        if self.r.pct(45) {
            if self.r.pct(30) {
                a.push(b"ALIGN".to_vec());
                a.push(self.r.pick(&["start", "end", "-", "+", "0", "7", "x", "13", "-5"]).into());
            }
            a.push(b"AGGREGATION".to_vec());
            let nrange = a[0].eq_ignore_ascii_case(b"TS.NRANGE") || a[0].eq_ignore_ascii_case(b"TS.NREVRANGE");
            for _ in 0..nkeys {
                if nrange && self.r.pct(30) {
                    let n = self.r.int(1, 2);
                    let v: Vec<&str> = (0..n).map(|_| self.agg()).collect();
                    a.push(v.join(",").into_bytes());
                } else {
                    a.push(self.agg().into());
                }
            }
            a.push(self.r.pick(&["1", "2", "5", "10", "33", "100", "1000", "0", "-5", "x"]).into());
            if self.r.pct(25) {
                a.push(b"BUCKETTIMESTAMP".to_vec());
                a.push(self.r.pick(&["-", "start", "+", "end", "~", "mid", "x"]).into());
            }
            if self.r.pct(25) {
                a.push(b"EMPTY".to_vec());
            }
        }
    }

    fn ts_filter(&mut self, a: &mut Args) {
        for _ in 0..self.r.int(1, 3) {
            let l = self.r.pick(&["area", "type", "n", "name", "x"]);
            let f = match self.r.below(8) {
                0 | 1 => format!("{l}={}", self.r.pick(&["us", "eu", "a", "b", "1"])),
                2 => format!("{l}!={}", self.r.pick(&["us", "a", "2"])),
                3 => format!("{l}="),
                4 => format!("{l}!="),
                5 => format!("{l}=({},{})", self.r.pick(&["us", "a"]), self.r.pick(&["eu", "b", "c"])),
                6 => format!("{l}!=({},{})", self.r.pick(&["us", "a"]), self.r.pick(&["eu", "b"])),
                _ => self.r.pick(&["bad", "=x", "a==b", "", "a=(", "a=()"]).to_string(),
            };
            a.push(f.into_bytes());
        }
    }

    fn ts(&mut self) -> Cmd {
        let k = self.key("ts");
        match self.r.weighted(&[7, 4, 20, 6, 6, 4, 9, 7, 3, 4, 3, 5, 4, 3, 4, 3, 3, 2]) {
            0 => {
                let mut a = args(["TS.CREATE".to_string(), k]);
                self.ts_create_opts(&mut a, false, false);
                plain(a)
            }
            1 => {
                let mut a = args(["TS.ALTER".to_string(), k]);
                self.ts_create_opts(&mut a, true, false);
                plain(a)
            }
            2 => {
                let mut a = args(["TS.ADD".to_string(), k, self.ts_ts(), self.ts_val()]);
                if self.r.pct(30) {
                    self.ts_create_opts(&mut a, false, false);
                }
                if self.r.pct(15) {
                    a.push(b"ON_DUPLICATE".to_vec());
                    a.push(self.policy().into());
                }
                plain(a)
            }
            3 => {
                let mut a = args(["TS.MADD"]);
                for _ in 0..self.r.int(1, 4) {
                    a.push(self.key("ts").into_bytes());
                    a.push(self.ts_ts().into_bytes());
                    a.push(self.ts_val().into_bytes());
                }
                plain(a)
            }
            4 => {
                let mut a = args([if self.r.pct(50) { "TS.INCRBY" } else { "TS.DECRBY" }.to_string(), k, self.ts_val(), "TIMESTAMP".into(), self.ts_ts().replace('-', "")]);
                if self.r.pct(25) {
                    self.ts_create_opts(&mut a, false, true);
                }
                plain(a)
            }
            5 => plain(args(["TS.DEL".to_string(), k, self.ts_ts(), self.ts_ts()])),
            6 | 7 => {
                let mut a = args([if self.r.pct(50) { "TS.RANGE" } else { "TS.REVRANGE" }.to_string(), k, self.ts_range_bound(), self.ts_range_bound()]);
                self.ts_range_opts(&mut a, false, 1);
                plain(a)
            }
            8 | 9 => {
                let mut a = args([if self.r.pct(50) { "TS.MRANGE" } else { "TS.MREVRANGE" }.to_string(), self.ts_range_bound(), self.ts_range_bound()]);
                self.ts_range_opts(&mut a, true, 1);
                a.push(b"FILTER".to_vec());
                self.ts_filter(&mut a);
                if self.r.pct(30) {
                    a.extend(args(["GROUPBY", self.r.pick(&["area", "type", "n", "zz"]), "REDUCE", self.r.pick(&["sum", "min", "max", "avg", "range", "count", "std.p", "std.s", "var.p", "var.s", "bad", "first", "last"])]));
                }
                plain(a)
            }
            10 => {
                let n = self.r.int(1, 3) as usize;
                let mut a = args([if self.r.pct(50) { "TS.NRANGE" } else { "TS.NREVRANGE" }.to_string(), n.to_string()]);
                for _ in 0..n {
                    a.push(self.key("ts").into_bytes());
                }
                a.push(self.ts_range_bound().into_bytes());
                a.push(self.ts_range_bound().into_bytes());
                self.ts_range_opts(&mut a, false, n);
                plain(a)
            }
            11 => {
                let mut a = args(["TS.GET".to_string(), k]);
                if self.r.pct(30) {
                    a.push(b"LATEST".to_vec());
                }
                plain(a)
            }
            12 => {
                let mut a = args(["TS.MGET"]);
                if self.r.pct(30) {
                    a.push(b"LATEST".to_vec());
                }
                if self.r.pct(40) {
                    if self.r.pct(50) {
                        a.push(b"WITHLABELS".to_vec());
                    } else {
                        a.push(b"SELECTED_LABELS".to_vec());
                        a.push(self.r.pick(&["area", "type", "zz"]).into());
                    }
                }
                a.push(b"FILTER".to_vec());
                self.ts_filter(&mut a);
                plain(a)
            }
            13 => {
                let mut a = args(["TS.QUERYINDEX"]);
                self.ts_filter(&mut a);
                with(a, Cmp::Sort)
            }
            14 => {
                let d = self.key("ts");
                let mut a = args(["TS.CREATERULE".to_string(), k, d, "AGGREGATION".into(), self.agg().into(), self.r.pick(&["1", "5", "10", "50", "0", "-1", "x"]).into()]);
                if self.r.pct(25) {
                    a.push(self.r.pick(&["0", "3", "7", "-1", "x"]).into());
                }
                plain(a)
            }
            15 => {
                let d = self.key("ts");
                plain(args(["TS.DELETERULE".to_string(), k, d]))
            }
            16 => {
                let mut a = args(["TS.INFO".to_string(), k]);
                if self.r.pct(40) {
                    a.push(self.r.pick(&["DEBUG", "debug", "BAD"]).into());
                }
                plain(a)
            }
            _ => {
                if self.r.pct(50) {
                    let mut a = args(["TS.QUERYLABELS".to_string()]);
                    if self.r.pct(50) {
                        a.push(b"LABELS".to_vec());
                    } else {
                        a.push(b"VALUES".to_vec());
                        a.push(self.r.pick(&["area", "type", "zz"]).into());
                    }
                    if self.r.pct(50) {
                        a.push(b"FILTER".to_vec());
                        self.ts_filter(&mut a);
                    }
                    with(a, Cmp::Sort)
                } else {
                    let mut a = args(["TS.READ".to_string(), k, self.ts_ts()]);
                    if self.r.pct(30) {
                        a.push(b"MAX_COUNT".to_vec());
                        a.push(self.r.pick(&["0", "1", "3", "-1", "x"]).into());
                    }
                    plain(a)
                }
            }
        }
    }

    // ---- Vector sets ----

    fn vdim(k: &str) -> usize {
        match k {
            "v1" => 2,
            "v2" => 3,
            "v3" => 2,
            _ => 2,
        }
    }

    fn vcomp(&mut self) -> String {
        match self.r.below(100) {
            0..70 => self.r.int(-3, 3).to_string(),
            70..95 => format!("{}.{}", self.r.int(-2, 2), self.r.int(0, 99)),
            _ => self.r.pick(&["nan", "inf", "x", "1e30", "-0", "1e-30"]).to_string(),
        }
    }

    fn vector(&mut self, a: &mut Args, dim: usize) {
        let d = if self.r.pct(90) { dim } else { self.r.int(1, 4) as usize };
        if self.r.pct(15) {
            let mut blob = Vec::new();
            for _ in 0..d {
                let x = self.r.int(-3, 3) as f32 * if self.r.pct(30) { 0.5 } else { 1.0 };
                blob.extend_from_slice(&x.to_le_bytes());
            }
            if self.r.pct(5) {
                blob.push(0);
            }
            a.push(b"FP32".to_vec());
            a.push(blob);
        } else {
            a.push(b"VALUES".to_vec());
            a.push(if self.r.pct(97) { d.to_string() } else { self.r.pick(&["0", "-1", "x", "5"]).to_string() }.into_bytes());
            for _ in 0..d {
                a.push(self.vcomp().into_bytes());
            }
        }
    }

    fn velem(&mut self) -> String {
        if self.r.pct(90) {
            return format!("e{}", self.r.below(10));
        }
        self.r.pick(&["", "日本", "a b", "e10"]).to_string()
    }

    fn vattr(&mut self) -> String {
        if self.r.pct(10) {
            return self.r.pick(&["", "{", "[1,2]", "1", "\"s\"", "{\"a\":}", "null", "{}"]).to_string();
        }
        let mut parts = Vec::new();
        if self.r.pct(70) {
            parts.push(format!("\"year\":{}", self.r.int(1980, 2025)));
        }
        if self.r.pct(50) {
            parts.push(format!("\"name\":\"{}\"", self.r.pick(&["x", "y", "abc", "", "é"])));
        }
        if self.r.pct(30) {
            parts.push(format!("\"tags\":[\"{}\",\"{}\"]", self.r.pick(&["a", "b"]), self.r.pick(&["c", "d"])));
        }
        if self.r.pct(30) {
            parts.push(format!("\"price\":{}.{}", self.r.int(0, 9), self.r.int(0, 99)));
        }
        if self.r.pct(20) {
            parts.push(format!("\"ok\":{}", self.r.pick(&["true", "false", "null"])));
        }
        if self.r.pct(10) {
            parts.push("\"obj\":{\"x\":1}".into());
        }
        format!("{{{}}}", parts.join(","))
    }

    fn vfilter_atom(&mut self) -> String {
        match self.r.below(12) {
            0 => format!(".year {} {}", self.r.pick(&[">", ">=", "<", "<=", "==", "!="]), self.r.int(1985, 2020)),
            1 => format!(".name == \"{}\"", self.r.pick(&["x", "y", "abc", ""])),
            2 => format!("\"{}\" in .tags", self.r.pick(&["a", "c", "z"])),
            3 => format!(".year in [{}, {}]", self.r.int(1980, 2025), self.r.int(1980, 2025)),
            4 => format!("(.price * 2) >= {}", self.r.int(0, 15)),
            5 => format!(".year + 1 > {}", self.r.int(1990, 2020)),
            6 => format!(".year % {} == 0", self.r.int(1, 4)),
            7 => ".ok".into(),
            8 => "!.ok".to_string(),
            9 => format!(".year ** 2 > {}", self.r.int(3_900_000, 4_100_000)),
            10 => format!(".missing == {}", self.r.int(0, 1)),
            _ => format!(".year - {} < 0", self.r.int(1990, 2020)),
        }
    }

    fn vfilter(&mut self) -> String {
        if self.r.pct(8) {
            return self.r.pick(&["(", ".year >", "and", "..x", ".year >> 1", "\"unterminated", "1 +", "[1,2", ".year == ", ""]).to_string();
        }
        let mut s = self.vfilter_atom();
        for _ in 0..self.r.below(3) {
            let op = self.r.pick(&["and", "or", "&&", "||"]);
            let b = self.vfilter_atom();
            s = if self.r.pct(20) { format!("not ({s}) {op} {b}") } else { format!("{s} {op} {b}") };
        }
        s
    }

    /// A plain VADD of small integers (sets with several elements, so that searches see more
    /// than one; not from a Lua loop: Lua's cached argument objects would end up as elements,
    /// sized by earlier scripts' arguments in MEMORY USAGE).
    fn vadd_plain(&mut self, k: String, dim: usize) -> Cmd {
        let mut a = args(["VADD".to_string(), k, "VALUES".into(), dim.to_string()]);
        for _ in 0..dim {
            a.push(self.r.int(-3, 3).to_string().into_bytes());
        }
        a.push(self.velem().into_bytes());
        plain(a)
    }

    fn vset(&mut self) -> Cmd {
        let k = self.key("vset");
        let dim = Self::vdim(&k);
        match self.r.weighted(&[22, 18, 5, 3, 3, 4, 4, 4, 3, 2, 2, 3, 3, 3]) {
            0 => {
                let mut a = args(["VADD".to_string(), k]);
                if self.r.pct(3) {
                    a.push(b"REDUCE".to_vec());
                    a.push(self.r.pick(&["0", "-1", "x", "10"]).into());
                }
                self.vector(&mut a, dim);
                a.push(self.velem().into_bytes());
                if self.r.pct(15) {
                    a.push(self.r.pick(&["NOQUANT", "BIN", "Q8", "q8"]).into());
                }
                if self.r.pct(10) {
                    a.push(b"EF".to_vec());
                    a.push(self.r.pick(&["1", "50", "500", "0", "-1", "1000001", "x"]).into());
                }
                if self.r.pct(30) {
                    a.push(b"SETATTR".to_vec());
                    a.push(self.vattr().into_bytes());
                }
                if self.r.pct(8) {
                    a.push(b"M".to_vec());
                    a.push(self.r.pick(&["2", "4", "16", "64", "0", "1", "1000", "x"]).into());
                }
                plain(a)
            }
            1 => {
                let mut a = args(["VSIM".to_string(), k]);
                if self.r.pct(40) {
                    a.push(b"ELE".to_vec());
                    a.push(self.velem().into_bytes());
                } else {
                    self.vector(&mut a, dim);
                }
                let scores = self.r.pct(85);
                let attribs = self.r.pct(20);
                if scores {
                    a.push(b"WITHSCORES".to_vec());
                }
                if attribs {
                    a.push(b"WITHATTRIBS".to_vec());
                }
                let mut count = 10;
                if self.r.pct(40) {
                    let c = self.r.pick(&["1", "2", "3", "5", "20", "0", "-1", "x"]);
                    count = c.parse().unwrap_or(10);
                    a.push(b"COUNT".to_vec());
                    a.push(c.into());
                }
                if self.r.pct(15) {
                    a.push(b"EPSILON".to_vec());
                    a.push(self.r.pick(&["0.1", "0.5", "1", "2", "0", "-1", "x"]).into());
                }
                if self.r.pct(15) {
                    a.push(b"EF".to_vec());
                    a.push(self.r.pick(&["1", "10", "100", "0", "-1", "x"]).into());
                }
                if self.r.pct(30) {
                    a.push(b"FILTER".to_vec());
                    a.push(self.vfilter().into_bytes());
                    if self.r.pct(20) {
                        a.push(b"FILTER-EF".to_vec());
                        a.push(self.r.pick(&["1", "10", "100", "0", "-1", "x"]).into());
                    }
                }
                if self.r.pct(40) {
                    a.push(b"TRUTH".to_vec());
                }
                if self.r.pct(30) {
                    a.push(b"NOTHREAD".to_vec());
                }
                let stride = 1 + scores as usize + attribs as usize;
                with(a, Cmp::Vsim { stride, scores, count })
            }
            2 => plain(args(["VREM".to_string(), k, self.velem()])),
            3 => plain(args(["VCARD".to_string(), k])),
            4 => plain(args(["VDIM".to_string(), k])),
            5 => {
                let mut a = args(["VEMB".to_string(), k, self.velem()]);
                if self.r.pct(30) {
                    a.push(b"RAW".to_vec());
                }
                plain(a)
            }
            6 => plain(args(["VGETATTR".to_string(), k, self.velem()])),
            7 => plain(args(["VSETATTR".to_string(), k, self.velem(), self.vattr()])),
            8 => with(args(["VINFO".to_string(), k]), Cmp::Mask(vec!["max-level", "vset-uid"])),
            9 => {
                let mut a = args(["VLINKS".to_string(), k, self.velem()]);
                if self.r.pct(50) {
                    a.push(b"WITHSCORES".to_vec());
                }
                with(a, Cmp::ErrOnly)
            }
            10 => {
                let mut a = args(["VRANDMEMBER".to_string(), k]);
                if self.r.pct(70) {
                    a.push(self.r.pick(&["0", "1", "2", "5", "20", "-1", "-3", "-20", "x"]).into());
                }
                with(a, Cmp::Shape)
            }
            11 => plain(args(["VISMEMBER".to_string(), k, self.velem()])),
            12 => {
                let s = self.r.pick(&["-", "+", "[e1", "(e3", "e1", "[e5", "(e0", "[", "("]);
                let e = self.r.pick(&["+", "-", "[e8", "(e6", "e9", "[e5", "(e9"]);
                let mut a = args(["VRANGE", &k, s, e]);
                if self.r.pct(60) {
                    a.push(self.r.pick(&["0", "1", "3", "10", "-1", "x"]).into());
                }
                plain(a)
            }
            _ => self.vadd_plain(k, dim),
        }
    }

    // ---- Keys (cross-cutting) ----

    fn keys(&mut self) -> Cmd {
        let k = self.key("keys");
        let d = self.key("keys");
        match self.r.weighted(&[5, 4, 6, 4, 6, 4, 5, 3, 2, 2, 6, 6, 6, 3, 3, 2, 3, 2, 2]) {
            0 => plain(args(["DEL".to_string(), k])),
            1 => plain(args(["EXISTS".to_string(), k, d])),
            2 => plain(args(["TYPE".to_string(), k])),
            3 => plain(args(["OBJECT".to_string(), "ENCODING".into(), k])),
            4 => plain(args(["MEMORY".to_string(), "USAGE".into(), k])),
            5 => {
                let mut a = args(["COPY".to_string(), k, d]);
                if self.r.pct(40) {
                    a.push(b"REPLACE".to_vec());
                }
                plain(a)
            }
            6 => plain(args([if self.r.pct(70) { "RENAME" } else { "RENAMENX" }.to_string(), k, d])),
            // No MOVE or SWAPDB once series may exist: RedisTimeSeries indexes series by name across
            // databases and handles neither ("swapdb isn't supported"); the reference then crashes in
            // its index (`IndexMemUsage`) a few commands later.
            7 if !self.fams.contains(&"ts") => plain(args(["MOVE".to_string(), k, "1".into()])),
            8 if !self.fams.contains(&"ts") => plain(args(["SWAPDB", "0", "1"])),
            9 => plain(args(["UNLINK".to_string(), k, d])),
            10 => with(args(["DUMP".to_string(), k]), Cmp::Dump),
            11 => Cmd { args: args(["RESTORE".to_string(), d, "0".into(), String::new(), "REPLACE".into()]), cmp: Cmp::Exact, cross: Some((k.into_bytes(), if self.r.pct(50) { DumpOf::Redis } else { DumpOf::Roll })) },
            12 => Cmd { args: args(["RESTORE".to_string(), d, "0".into(), String::new(), "REPLACE".into()]), cmp: Cmp::Exact, cross: Some((k.into_bytes(), DumpOf::Own)) },
            13 => with(args(["EVAL", "local r = {} for _, k in ipairs(redis.call('KEYS', '*')) do if redis.call('TYPE', k)['ok'] == ARGV[1] then table.insert(r, k) end end return r", "0", self.r.pick(&["ReJSON-RL", "MBbloom--", "MBbloomCF", "CMSk-TYPE", "TopK-TYPE", "TDIS-TYPE", "TSDB-TYPE", "vectorset"])]), Cmp::Sort),
            14 => with(args(["KEYS", "*"]), Cmp::Sort),
            15 => plain(args(["EXPIREAT".to_string(), k, "4000000000".into()])),
            16 => plain(args(["EXPIRETIME".to_string(), k])),
            17 => plain(args(["PERSIST".to_string(), k])),
            _ => {
                if self.r.pct(50) {
                    plain(args(["SET", "s", self.r.pick(&["hello", "1", ""])]))
                } else {
                    plain(args(["HSET", "h", "f", "v"]))
                }
            }
        }
    }
}

fn generate(seed: u64, n: usize, fams: &[&'static str], skip: &[String]) -> (Vec<Cmd>, bool) {
    let mut g = Gen { r: Rng::new(seed), fams: fams.to_vec(), resp3: false };
    g.resp3 = match std::env::var("ROLL_FUZZ_RESP").ok().as_deref() {
        Some("2") => false,
        Some("3") => true,
        _ => g.r.pct(50),
    };
    let mut v = vec![plain(args(["SET", "s", "hello"])), plain(args(["HSET", "h", "f", "v"]))];
    while v.len() < n {
        for c in g.step() {
            v.push(c);
        }
    }
    v.retain(|c| !skip.contains(&c.name()));
    (v, g.resp3)
}

// ---------------------------------------------------------------------------------------------
// Connections

struct Conn {
    s: TcpStream,
    buf: BytesMut,
}

impl Conn {
    async fn open(addr: &str) -> io::Result<Conn> {
        let s = TcpStream::connect(addr).await?;
        s.set_nodelay(true)?;
        Ok(Conn { s, buf: BytesMut::with_capacity(16 * 1024) })
    }

    async fn send(&mut self, a: &[Vec<u8>]) -> io::Result<()> {
        let mut out = BytesMut::new();
        encode_command(a, &mut out);
        self.s.write_all(&out).await
    }

    /// The next reply and its bytes; `None` after `ms` without one.
    async fn read(&mut self, ms: u64) -> io::Result<Option<(Reply, Vec<u8>)>> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
        loop {
            if let Some((r, n)) = decode(&self.buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? {
                let raw = self.buf.split_to(n).to_vec();
                return Ok(Some((r, raw)));
            }
            match tokio::time::timeout_at(deadline, self.s.read_buf(&mut self.buf)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
            }
        }
    }

    async fn call(&mut self, a: &[Vec<u8>]) -> io::Result<Option<(Reply, Vec<u8>)>> {
        self.send(a).await?;
        self.read(10_000).await
    }
}

struct Side {
    main: Conn,
    aux: Conn,
    sub: Option<Conn>,
}

/// One side's outcome of a command: its reply (raw), or what went wrong; and its events.
#[derive(Debug)]
struct Out {
    reply: Result<(Reply, Vec<u8>), String>,
    events: Vec<Vec<u8>>,
    /// Replies before the last one (the reference's double replies).
    extra: Vec<Vec<u8>>,
}

fn esc(raw: &[u8]) -> String {
    let mut s = String::new();
    for &b in raw.iter().take(600) {
        match b {
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    if raw.len() > 600 {
        s.push_str(&format!("… ({} bytes)", raw.len()));
    }
    s
}

fn show(o: &Out) -> String {
    let mut s = match &o.reply {
        Ok((_, raw)) => esc(raw),
        Err(e) => format!("<{e}>"),
    };
    if !o.events.is_empty() {
        s.push_str("\n           events: ");
        s.push_str(&o.events.iter().map(|e| esc(e)).collect::<Vec<_>>().join(" "));
    }
    s
}

// ---------------------------------------------------------------------------------------------
// Comparison

fn is_err(r: &Reply) -> bool {
    matches!(r, Reply::Error(_))
}

fn kind(r: &Reply) -> String {
    match r {
        Reply::Array(v) => format!("array({})", v.len()),
        Reply::Set(v) => format!("set({})", v.len()),
        Reply::Map(v) => format!("map({})", v.len()),
        Reply::Push(v) => format!("push({})", v.len()),
        Reply::Bulk(_) | Reply::Verbatim(..) => "bulk".into(),
        Reply::Int(_) => "int".into(),
        Reply::Error(e) => format!("error {e}"),
        Reply::Status(_) | Reply::Ok => "status".into(),
        Reply::Nil => "nil".into(),
        Reply::NilArray => "nilarray".into(),
        Reply::Double(_) => "double".into(),
        Reply::Bool(_) => "bool".into(),
        other => format!("{other:?}"),
    }
}

fn sort_maps(r: &Reply) -> Reply {
    match r {
        Reply::Array(items) => Reply::Array(items.iter().map(sort_maps).collect()),
        Reply::Map(pairs) => {
            let mut v: Vec<(Reply, Reply)> = pairs.iter().map(|(k, v)| (k.clone(), sort_maps(v))).collect();
            v.sort_by_key(|(k, _)| format!("{k:?}"));
            Reply::Map(v)
        }
        other => other.clone(),
    }
}

fn sort_reply(r: &Reply) -> Reply {
    match r {
        Reply::Array(items) | Reply::Set(items) => {
            let mut v: Vec<Reply> = items.iter().map(sort_reply).collect();
            v.sort_by_key(|x| format!("{x:?}"));
            Reply::Array(v)
        }
        Reply::Map(pairs) => {
            let mut v: Vec<(Reply, Reply)> = pairs.iter().map(|(k, v)| (k.clone(), sort_reply(v))).collect();
            v.sort_by_key(|(k, _)| format!("{k:?}"));
            Reply::Map(v)
        }
        other => other.clone(),
    }
}

/// The members of a top-level JSON object (raw text, trimmed), or `None` if it isn't one.
/// `newline` (JSON.GET NEWLINE) comes before the closing brace, after the last member.
fn json_members(s: &str, newline: &str) -> Option<Vec<String>> {
    let t = s.trim();
    let inner = t.strip_prefix('{')?.strip_suffix('}')?;
    let inner = if newline.is_empty() { inner } else { inner.strip_suffix(newline).unwrap_or(inner) };
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut escp, mut start) = (0i32, false, false, 0usize);
    for (i, c) in inner.char_indices() {
        if in_str {
            match c {
                _ if escp => escp = false,
                '\\' => escp = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if !inner[start..].trim().is_empty() {
        out.push(inner[start..].trim().to_string());
    }
    Some(out)
}

fn json_norm(r: &Reply, newline: &str) -> Reply {
    match r {
        Reply::Bulk(b) => match std::str::from_utf8(b).ok().and_then(|s| json_members(s, newline)) {
            Some(mut m) => {
                m.sort();
                Reply::Bulk(format!("{{{}}}", m.join(",")).into_bytes().into())
            }
            None => r.clone(),
        },
        Reply::Map(_) => sort_reply(r),
        Reply::Array(items) => Reply::Array(items.iter().map(|x| json_norm(x, newline)).collect()),
        other => other.clone(),
    }
}

fn mask(r: &Reply, fields: &[&str]) -> Reply {
    let is_field = |x: &Reply| match x {
        Reply::Bulk(b) => fields.iter().any(|f| f.as_bytes() == &b[..]),
        Reply::Status(s) => fields.iter().any(|f| f == s),
        _ => false,
    };
    match r {
        Reply::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut hide = false;
            for x in items {
                out.push(if hide { Reply::Nil } else { mask(x, fields) });
                hide = !hide && is_field(x);
            }
            Reply::Array(out)
        }
        Reply::Map(pairs) => Reply::Map(pairs.iter().map(|(k, v)| (k.clone(), if is_field(k) { Reply::Nil } else { mask(v, fields) })).collect()),
        other => other.clone(),
    }
}

/// VSIM results as (element, rest) records.
fn vsim_records(r: &Reply, stride: usize) -> Option<Vec<(String, String)>> {
    match r {
        Reply::Map(pairs) => Some(pairs.iter().map(|(k, v)| (format!("{k:?}"), format!("{v:?}"))).collect()),
        Reply::Array(items) if stride > 0 && items.len() % stride == 0 => {
            Some(items.chunks(stride).map(|c| (format!("{:?}", c[0]), c[1..].iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(","))).collect())
        }
        _ => None,
    }
}

/// The score in a record's rest (its first part).
fn score_of(rest: &str) -> String {
    rest.split(',').next().unwrap_or("").to_string()
}

fn vsim_same(a: &Reply, b: &Reply, stride: usize, scores: bool, count: usize) -> bool {
    let (Some(mut x), Some(mut y)) = (vsim_records(a, stride), vsim_records(b, stride)) else { return false };
    if x.len() != y.len() {
        return false;
    }
    if !scores {
        // Without scores, the members only when all of them fit (no boundary to tie at).
        if x.len() >= count.max(1) {
            return true;
        }
        x.sort();
        y.sort();
        return x.iter().map(|r| &r.0).eq(y.iter().map(|r| &r.0));
    }
    let mut sx: Vec<String> = x.iter().map(|r| score_of(&r.1)).collect();
    let mut sy: Vec<String> = y.iter().map(|r| score_of(&r.1)).collect();
    sx.sort();
    sy.sort();
    if sx != sy {
        return false;
    }
    // Records tied with the last one returned may be any of the tied elements.
    let last = x.last().map(|r| score_of(&r.1));
    let full = x.len() < count.max(1);
    x.retain(|r| full || Some(score_of(&r.1)) != last);
    y.retain(|r| full || Some(score_of(&r.1)) != last);
    x.sort();
    y.sort();
    x == y
}

fn same(cmp: &Cmp, a: &(Reply, Vec<u8>), b: &(Reply, Vec<u8>), vset_prefix: &[Vec<u8>]) -> bool {
    a.1 == b.1 || same_decoded(cmp, &a.0, &b.0, vset_prefix)
}

fn same_decoded(cmp: &Cmp, a: &Reply, b: &Reply, vset_prefix: &[Vec<u8>]) -> bool {
    // (Debug: a NaN double equals itself.)
    if a == b || format!("{a:?}") == format!("{b:?}") {
        return true;
    }
    if is_err(a) || is_err(b) {
        return false;
    }
    let (a, b) = (&(a.clone(), Vec::<u8>::new()), &(b.clone(), Vec::<u8>::new()));
    match cmp {
        Cmp::Exact => false,
        Cmp::Each(cmps) => match (&a.0, &b.0) {
            (Reply::Array(x), Reply::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).zip(cmps.iter().chain(std::iter::repeat(&Cmp::Exact))).all(|((x, y), c)| same_decoded(c, x, y, vset_prefix))
            }
            _ => false,
        },
        Cmp::Sort => format!("{:?}", sort_reply(&a.0)) == format!("{:?}", sort_reply(&b.0)),
        Cmp::Maps => format!("{:?}", sort_maps(&a.0)) == format!("{:?}", sort_maps(&b.0)),
        Cmp::Json(nl) => format!("{:?}", json_norm(&a.0, nl)) == format!("{:?}", json_norm(&b.0, nl)),
        Cmp::Shape => kind(&a.0) == kind(&b.0),
        Cmp::ErrOnly => true,
        Cmp::Mask(f) => format!("{:?}", mask(&a.0, f)) == format!("{:?}", mask(&b.0, f)),
        Cmp::Dump => match (&a.0, &b.0) {
            (Reply::Bulk(x), Reply::Bulk(y)) => vset_prefix.iter().any(|p| x.starts_with(p) && y.starts_with(p)),
            _ => false,
        },
        Cmp::Vsim { stride, scores, count } => vsim_same(&a.0, &b.0, *stride, *scores, *count),
    }
}

// ---------------------------------------------------------------------------------------------
// Running sequences

struct Runner {
    ref_addr: String,
    roll_addr: String,
    notify: bool,
    trace: bool,
    vset_prefix: Vec<Vec<u8>>,
    /// The reference's double replies seen: how many, and the first.
    doubles: std::sync::Mutex<(usize, Option<String>)>,
    /// A series' DUMP prefix.
    ts_prefix: Vec<u8>,
    /// Inside EXEC, DUMPs of series too (their uncompressed chunks: see `run`).
    queued_prefixes: Vec<Vec<u8>>,
    /// What the reference was sent, as sent (payloads in), to replay when it dies.
    sent: std::sync::Mutex<Vec<Cmd>>,
}

/// The first mismatch of a run: its index and both sides' outcomes.
struct Mismatch {
    at: usize,
    redis: Out,
    roll: Out,
}

impl Runner {
    async fn side(&self, addr: &str, resp3: bool, flush: bool) -> io::Result<Side> {
        let mut main = Conn::open(addr).await?;
        let aux = Conn::open(addr).await?;
        if flush {
            main.call(&args(["FLUSHALL", "SYNC"])).await?;
        }
        main.call(&args(["CONFIG", "SET", "notify-keyspace-events", if self.notify { "KEA" } else { "" }])).await?;
        if resp3 {
            main.call(&args(["HELLO", "3"])).await?;
        }
        let sub = if self.notify {
            let mut s = Conn::open(addr).await?;
            s.call(&args(["HELLO", "3"])).await?;
            s.call(&args(["PSUBSCRIBE", "__keyevent@*__:*"])).await?;
            Some(s)
        } else {
            None
        };
        Ok(Side { main, aux, sub })
    }

    async fn events(side: &mut Side) -> Vec<Vec<u8>> {
        let Some(sub) = side.sub.as_mut() else { return Vec::new() };
        let mut out = Vec::new();
        if sub.send(&args(["PING"])).await.is_err() {
            return vec![b"<sub closed>".to_vec()];
        }
        loop {
            match sub.read(5000).await {
                Ok(Some((Reply::Status(s), _))) if s == "PONG" => break,
                Ok(Some((_, raw))) => out.push(raw),
                _ => {
                    out.push(b"<no PONG>".to_vec());
                    break;
                }
            }
        }
        out
    }

    async fn exec(&self, side: &mut Side, c: &Cmd, payload: &Option<Vec<u8>>) -> Out {
        let mut a = c.args.clone();
        if let Some(p) = payload {
            a[3] = p.clone();
        }
        let mut reply = match side.main.call(&a).await {
            Ok(Some(r)) => Ok(r),
            Ok(None) => Err("timeout".to_string()),
            Err(e) => Err(format!("io: {e}")),
        };
        // RedisTimeSeries answers a compaction's failed write to its destination on top of the
        // command's own reply (`internalAdd` replies even when asked not to): the extra replies
        // arrive with the last one, which is the command's.
        let mut extra = Vec::new();
        while let Ok(cur) = reply.as_mut()
            && !side.main.buf.is_empty()
        {
            match side.main.read(1000).await {
                Ok(Some(r)) => extra.push(std::mem::replace(cur, r).1),
                _ => break,
            }
        }
        let events = if reply.is_ok() { Self::events(side).await } else { Vec::new() };
        Out { reply, events, extra }
    }

    /// Run `cmds` on fresh connections after FLUSHALL; the first mismatch, if any.
    async fn run(&self, cmds: &[Cmd], resp3: bool) -> io::Result<Option<Mismatch>> {
        self.sent.lock().unwrap().extend([plain(args(["FLUSHALL", "SYNC"])), plain(args(["HELLO", if resp3 { "3" } else { "2" }]))]);
        let mut r = self.side(&self.ref_addr, resp3, true).await?;
        let mut o = self.side(&self.roll_addr, resp3, true).await?;
        // EXEC compares its replies as the commands queued since MULTI (in this run: the minimiser
        // may have dropped some).
        let mut queued: Option<Vec<Cmp>> = None;
        for (i, c) in cmds.iter().enumerate() {
            let cmp = match c.name().as_str() {
                "MULTI" => {
                    queued = Some(Vec::new());
                    c.cmp.clone()
                }
                "EXEC" => queued.take().map_or(Cmp::Exact, Cmp::Each),
                "DISCARD" => {
                    queued = None;
                    c.cmp.clone()
                }
                _ => {
                    if let Some(q) = queued.as_mut() {
                        q.push(c.cmp.clone());
                    }
                    c.cmp.clone()
                }
            };
            if self.trace {
                eprintln!("fuzz: [{i}] {}", line_of(c));
            }
            // RESTORE with one side's payload, on both sides. Both DUMP: it has side effects (a
            // t-digest merges its buffered values first).
            let (pr, po) = match &c.cross {
                Some((src, from)) => {
                    let dump = [b"DUMP".to_vec(), src.clone()];
                    let payload = |x: Option<(Reply, Vec<u8>)>| match x {
                        Some((Reply::Bulk(b), _)) => Some(b.to_vec()),
                        _ => Some(Vec::new()),
                    };
                    let (x, y) = (payload(r.aux.call(&dump).await?), payload(o.aux.call(&dump).await?));
                    match from {
                        DumpOf::Redis => (x.clone(), x),
                        DumpOf::Roll => (y.clone(), y),
                        DumpOf::Own => (x, y),
                    }
                }
                None => (None, None),
            };
            let guarded;
            let c = if reference_hazard(&mut r.aux, c).await? {
                guarded = plain(args([b"JSON.TYPE".to_vec(), c.args[1].clone()]));
                &guarded
            } else {
                c
            };
            {
                let mut a = c.args.clone();
                if let Some(p) = &pr {
                    a[3] = p.clone();
                }
                let mut sent = self.sent.lock().unwrap();
                sent.push(plain(a));
                if sent.len() > 6000 {
                    sent.drain(..1000);
                }
            }
            let a = self.exec(&mut r, c, &pr).await;
            let b = self.exec(&mut o, c, &po).await;
            let mut ok = match (&a.reply, &b.reply) {
                (Ok(x), Ok(y)) => same(&cmp, x, y, if matches!(cmp, Cmp::Each(_)) { &self.queued_prefixes } else { &self.vset_prefix }) && a.events == b.events && b.extra.is_empty(),
                (Err(x), Err(y)) => x.starts_with("io") && y.starts_with("io"),
                _ => false,
            };
            // An uncompressed series' DUMP holds its chunks' whole buffers, uninitialized past
            // the samples in the reference.
            if !ok
                && c.cmp == Cmp::Dump
                && let (Ok((Reply::Bulk(x), _)), Ok((Reply::Bulk(y), _))) = (&a.reply, &b.reply)
                && x.starts_with(&self.ts_prefix)
                && y.starts_with(&self.ts_prefix)
                && let Some((Reply::Array(info), _)) = r.aux.call(&args([b"TS.INFO".to_vec(), c.args[1].clone()])).await?
            {
                ok = info.iter().any(|x| matches!(x, Reply::Status(s) if s == "uncompressed") || matches!(x, Reply::Bulk(b) if &b[..] == b"uncompressed"));
            }
            // `ROLL_FUZZ_CHECK="EXISTS k;TYPE k"`: also compared after every command (to find
            // where hidden state diverges).
            if ok {
                for p in std::env::var("ROLL_FUZZ_CHECK").unwrap_or_default().split(';').filter(|p| !p.trim().is_empty()) {
                    let pa = redis_server::client::args(p);
                    let (x, y) = (r.aux.call(&pa).await?, o.aux.call(&pa).await?);
                    if x.as_ref().map(|v| &v.1) != y.as_ref().map(|v| &v.1) {
                        let show = |v: Option<(Reply, Vec<u8>)>| v.map_or("-".into(), |v| esc(&v.1));
                        eprintln!("check {p} after [{i}] {}\n    redis: {}\n    roll:  {}", line_of(c), show(x), show(y));
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                // `ROLL_FUZZ_PROBE="TYPE k;JSON.GET k"`: what both sides then answer.
                for p in std::env::var("ROLL_FUZZ_PROBE").unwrap_or_default().split(';').filter(|p| !p.trim().is_empty()) {
                    let pa = redis_server::client::args(p);
                    let (x, y) = (r.aux.call(&pa).await?, o.aux.call(&pa).await?);
                    let show = |v: Option<(Reply, Vec<u8>)>| v.map_or("-".into(), |v| esc(&v.1));
                    eprintln!("probe {p}\n    redis: {}\n    roll:  {}", show(x), show(y));
                }
                return Ok(Some(Mismatch { at: i, redis: a, roll: b }));
            }
            if !a.extra.is_empty() {
                let mut d = self.doubles.lock().unwrap();
                d.0 += 1;
                if d.1.is_none() {
                    d.1 = Some(format!("{}\n    redis: {} before {}", line_of(c), a.extra.iter().map(|e| esc(e)).collect::<Vec<_>>().join(" "), show(&a)));
                }
            }
            if a.reply.is_err() {
                // Both closed the connection: nothing more to compare.
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Whether `cmds` fails first at its last command.
    /// Whether `cmds` fails first at its last command, the same way (`sig`).
    async fn fails_last(&self, cmds: &[Cmd], resp3: bool, sig: &str) -> bool {
        matches!(self.run(cmds, resp3).await, Ok(Some(m)) if m.at + 1 == cmds.len() && signature(&cmds[m.at], &m) == sig)
    }

    async fn minimise(&self, mut cmds: Vec<Cmd>, resp3: bool, sig: &str) -> Vec<Cmd> {
        let mut budget = 600usize;
        // Keep only the commands sharing an argument with the failing one (its keys).
        let last = cmds.last().unwrap().clone();
        let mut keys: HashSet<Vec<u8>> = last.args[1..].iter().filter(|a| a.len() <= 3).cloned().collect();
        if let Some((src, _)) = &last.cross {
            keys.insert(src.clone());
        }
        let sliced: Vec<Cmd> =
            cmds.iter().filter(|c| c.args[1..].iter().any(|a| keys.contains(a)) || c.cross.as_ref().is_some_and(|s| keys.contains(&s.0))).cloned().collect();
        if sliced.len() < cmds.len() && self.fails_last(&sliced, resp3, sig).await {
            cmds = sliced;
        }
        // Remove chunks, halving their size.
        let mut chunk = (cmds.len() / 2).max(1);
        loop {
            let mut i = 0;
            while i + 1 < cmds.len() && budget > 0 {
                let end = (i + chunk).min(cmds.len() - 1);
                let mut trial = cmds[..i].to_vec();
                trial.extend_from_slice(&cmds[end..]);
                budget -= 1;
                if self.fails_last(&trial, resp3, sig).await {
                    cmds = trial;
                } else {
                    i += chunk;
                }
            }
            if chunk == 1 || budget == 0 {
                break;
            }
            chunk = (chunk / 2).max(1);
        }
        cmds
    }
}

/// Whether a command would crash the reference given its state (checked there first; such a
/// command is sent as `JSON.TYPE key` to both sides instead). JSON.SET/JSON.MSET below the root:
/// RedisJSON (ijson) panics replacing the only element of a one-number array (`JSON.SET j $ [1]`,
/// `JSON.SET j $[0] {}`), so such a write is sent as JSON.TYPE instead while the document
/// (on the reference) holds one.
async fn reference_hazard(aux: &mut Conn, c: &Cmd) -> io::Result<bool> {
    let name = c.name();
    // A copy of a restored series, once the restored key is overwritten and the copy deleted,
    // leaves the reference with freed memory in use (`RESTORE t <series>`, `COPY t c`,
    // `COPY s t REPLACE` with a string `s`, `DEL c`, `TS.INCRBY t3 1 TIMESTAMP 1`,
    // `TS.RANGE t3 - +` crashes it): no COPY of series.
    if name == "COPY" && c.args.len() > 2 {
        let t = aux.call(&[b"TYPE".to_vec(), c.args[1].clone()]).await?;
        return Ok(matches!(t, Some((Reply::Status(s), _)) if s == "TSDB-TYPE"));
    }
    let pairs: Vec<(&[u8], &[u8])> = match name.as_str() {
        "JSON.SET" if c.args.len() > 3 => vec![(&c.args[1], &c.args[2])],
        "JSON.MSET" => c.args[1..].chunks(3).filter(|t| t.len() == 3).map(|t| (&t[0][..], &t[1][..])).collect(),
        _ => return Ok(false),
    };
    fn singleton(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Array(a) => (a.len() == 1 && a[0].is_number()) || a.iter().any(singleton),
            serde_json::Value::Object(o) => o.values().any(singleton),
            _ => false,
        }
    }
    for (key, path) in pairs {
        if path == b"$" || path == b"." {
            continue;
        }
        if let Some((Reply::Bulk(b), _)) = aux.call(&[b"JSON.GET".to_vec(), key.to_vec(), b"$".to_vec()]).await?
            && serde_json::from_slice::<serde_json::Value>(&b).is_ok_and(|v| singleton(&v))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Start the reference container afresh (`ROLL_FUZZ_CONTAINER`, published on `addr`).
async fn restart_reference(container: &str, addr: &str) {
    let _ = std::process::Command::new("docker").args(["rm", "-f", container]).output();
    let image = std::env::var("ROLL_COMPAT_IMAGE").unwrap_or_else(|_| "redis:8-alpine".into());
    let out = std::process::Command::new("docker").args(["run", "-d", "--name", container, "-p", &format!("{addr}:6379"), &image]).output().expect("docker run");
    assert!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
    for _ in 0..100 {
        if let Ok(mut c) = Conn::open(addr).await
            && matches!(c.call(&args(["PING"])).await, Ok(Some((Reply::Status(s), _))) if s == "PONG")
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("reference container {container} not ready");
}

/// Whether `seq` kills a fresh reference (run on it alone).
async fn ref_crashes(container: &str, addr: &str, seq: &[Cmd]) -> bool {
    restart_reference(container, addr).await;
    let (Ok(mut main), Ok(mut aux)) = (Conn::open(addr).await, Conn::open(addr).await) else { return true };
    for c in seq {
        let mut a = c.args.clone();
        if let Some((src, _)) = &c.cross {
            a[3] = match aux.call(&[b"DUMP".to_vec(), src.clone()]).await {
                Ok(Some((Reply::Bulk(b), _))) => b.to_vec(),
                Ok(_) => Vec::new(),
                Err(_) => return true,
            };
        }
        match reference_hazard(&mut aux, c).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(_) => return true,
        }
        if main.send(&a).await.is_err() {
            return true;
        }
        match main.read(5000).await {
            Ok(Some(_)) => {
                while !main.buf.is_empty() {
                    let _ = main.read(100).await;
                }
            }
            _ => return true,
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    !matches!(aux.call(&args(["PING"])).await, Ok(Some((Reply::Status(s), _))) if s == "PONG")
}

/// The commands of a crash, minimised (ddmin over chunks, against fresh references).
async fn minimise_crash(container: &str, addr: &str, mut seq: Vec<Cmd>) -> Vec<Cmd> {
    if !ref_crashes(container, addr, &seq).await {
        eprintln!("fuzz: the crash doesn't reproduce on a fresh reference");
        return seq;
    }
    let mut chunk = (seq.len() / 2).max(1);
    let mut budget = 400;
    loop {
        let mut i = 0;
        while i < seq.len() && budget > 0 {
            let end = (i + chunk).min(seq.len());
            let trial: Vec<Cmd> = seq[..i].iter().chain(&seq[end..]).cloned().collect();
            budget -= 1;
            if !trial.is_empty() && ref_crashes(container, addr, &trial).await {
                eprintln!("fuzz: crash still with {} commands", trial.len());
                seq = trial;
            } else {
                i += chunk;
            }
        }
        if chunk == 1 || budget == 0 {
            break;
        }
        chunk = (chunk / 2).max(1);
    }
    seq
}

/// What a mismatch is about, to report each kind once.
fn signature(c: &Cmd, m: &Mismatch) -> String {
    let class = |o: &Out| match &o.reply {
        Ok((Reply::Error(e), _)) => e.chars().take(48).map(|c| if c.is_ascii_digit() { '#' } else { c }).collect::<String>(),
        Ok((r, _)) => kind(r).chars().take(6).collect(),
        Err(e) => e.clone(),
    };
    let ev = if m.redis.events != m.roll.events && m.redis.reply.as_ref().ok().map(|x| &x.1) == m.roll.reply.as_ref().ok().map(|x| &x.1) { "events" } else { "" };
    format!("{} | {} | {} {ev}", c.name(), class(&m.redis), class(&m.roll))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn fuzz_modules() {
    let Ok(ref_addr) = std::env::var("ROLL_FUZZ_REDIS") else {
        eprintln!("fuzz: set ROLL_FUZZ_REDIS=host:port (a throwaway redis:8-alpine)");
        return;
    };
    let env_u64 = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let seed0 = env_u64("ROLL_FUZZ_SEED", (redis_engine::util::now_us() % 1_000_000) as u64);
    let seeds = env_u64("ROLL_FUZZ_SEEDS", 1);
    let iters = env_u64("ROLL_FUZZ_ITERS", 300) as usize;
    let fams: Vec<&'static str> = match std::env::var("ROLL_FUZZ_FAMILY") {
        Ok(f) if !f.is_empty() && f != "all" => {
            let want: Vec<String> = f.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
            FAMILIES.iter().copied().filter(|x| want.iter().any(|w| w == x)).collect()
        }
        _ => FAMILIES.to_vec(),
    };
    assert!(!fams.is_empty(), "ROLL_FUZZ_FAMILY: none of {FAMILIES:?}");
    let skip: Vec<String> = std::env::var("ROLL_FUZZ_SKIP").map(|s| s.split(',').map(|x| x.trim().to_ascii_uppercase()).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    let notify = std::env::var("ROLL_FUZZ_NOTIFY").map(|v| v != "0").unwrap_or(true);
    let minimise = std::env::var("ROLL_FUZZ_MINIMISE").map(|v| v != "0").unwrap_or(true);
    let trace = std::env::var("ROLL_FUZZ_TRACE").is_ok();

    // With a container to manage, the reference starts fresh like roll does (the modules keep
    // state across FLUSHALL: RedisTimeSeries' index, Lua's cached argument objects).
    if let Ok(c) = std::env::var("ROLL_FUZZ_CONTAINER") {
        restart_reference(&c, &ref_addr).await;
    }
    let db = Database::new("fuzz", Config::default(), 1);
    db.start_cron();
    let roll = start_single(db, None).await.unwrap().to_string();

    // DUMP prefixes (RDB type and module id) of the types whose payloads differ between
    // servers: a vector set's HNSW levels, a TopK's heap saved as raw memory (item pointers).
    let mut vset_prefix = Vec::new();
    {
        let mut c = Conn::open(&ref_addr).await.expect("reference not reachable");
        for make in [args(["VADD", "fuzz-probe", "VALUES", "1", "1", "x"]), args(["TOPK.RESERVE", "fuzz-probe", "1"])] {
            c.call(&make).await.unwrap();
            if let Ok(Some((Reply::Bulk(b), _))) = c.call(&args(["DUMP", "fuzz-probe"])).await {
                vset_prefix.push(b[..9.min(b.len())].to_vec());
            }
            c.call(&args(["DEL", "fuzz-probe"])).await.unwrap();
        }
    }
    let ts_prefix = {
        let mut c = Conn::open(&ref_addr).await.expect("reference not reachable");
        c.call(&args(["TS.CREATE", "fuzz-probe"])).await.unwrap();
        let Ok(Some((Reply::Bulk(b), _))) = c.call(&args(["DUMP", "fuzz-probe"])).await else { panic!("TS DUMP") };
        c.call(&args(["DEL", "fuzz-probe"])).await.unwrap();
        b[..9].to_vec()
    };
    let queued_prefixes = vset_prefix.iter().cloned().chain([ts_prefix.clone()]).collect();
    let runner = Runner { ref_addr: ref_addr.clone(), roll_addr: roll, notify, trace, vset_prefix, doubles: Default::default(), ts_prefix, queued_prefixes, sent: Default::default() };

    let mut seen: Vec<(String, u64, usize)> = Vec::new();
    let container = std::env::var("ROLL_FUZZ_CONTAINER").ok();
    // A dead reference: without a container to manage, the end; with one, the crash minimised
    // (when a fresh reference replaying what this one was sent dies too), and on with a new one.
    let mut crashes: Vec<String> = Vec::new();
    let died = async |seed: u64, seq: Vec<Cmd>| -> String {
        let Some(c) = &container else {
            let lines: Vec<String> = seq[seq.len().saturating_sub(40)..].iter().map(line_of).collect();
            panic!("fuzz: seed {seed}: the reference died (see `docker logs`; ROLL_FUZZ_CONTAINER=name minimises and goes on) after:\n{}\n", lines.join("\n"));
        };
        let last = seq.last().map(line_of).unwrap_or_default();
        let small = minimise_crash(c, &ref_addr, seq.clone()).await;
        let rep = if small.len() < seq.len() {
            let lines: Vec<String> = small.iter().map(line_of).collect();
            format!("seed {seed}: the reference crashes after:\n{}", lines.join("\n"))
        } else {
            format!("seed {seed}: the reference crashed (not reproduced on a fresh one) at: {last}")
        };
        eprintln!("fuzz: {rep}");
        restart_reference(c, &ref_addr).await;
        rep
    };
    let mut reports = Vec::new();
    let mut total = 0usize;
    for seed in seed0..seed0 + seeds {
        let (cmds, resp3) = generate(seed, iters, &fams, &skip);
        if std::env::var("ROLL_FUZZ_PRINT").is_ok() {
            eprintln!("# seed {seed} resp{}", if resp3 { 3 } else { 2 });
            cmds.iter().for_each(|c| eprintln!("{}", line_of(c)));
            continue;
        }
        let m = match runner.run(&cmds, resp3).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("fuzz: seed {seed}: connection error {e}");
                let sent = std::mem::take(&mut *runner.sent.lock().unwrap());
                crashes.push(died(seed, sent).await);
                continue;
            }
        };
        let Some(m) = m else {
            total += cmds.len();
            continue;
        };
        total += m.at + 1;
        if m.redis.reply.is_err() {
            let alive = match Conn::open(&ref_addr).await {
                Ok(mut c) => matches!(c.call(&args(["PING"])).await, Ok(Some((Reply::Status(s), _))) if s == "PONG"),
                Err(_) => false,
            };
            if !alive {
                let sent = std::mem::take(&mut *runner.sent.lock().unwrap());
                crashes.push(died(seed, sent).await);
                continue;
            }
        }
        let sig = signature(&cmds[m.at], &m);
        if let Some(s) = seen.iter_mut().find(|s| s.0 == sig) {
            s.2 += 1;
            eprintln!("fuzz: seed {seed}: again [{sig}] (first seed {})", s.1);
            continue;
        }
        seen.push((sig.clone(), seed, 1));
        let failing = cmds[..=m.at].to_vec();
        let small = if minimise { runner.minimise(failing.clone(), resp3, &sig).await } else { failing.clone() };
        // Show the minimised run's outcomes (fall back to the original's).
        let shown = match runner.run(&small, resp3).await {
            Ok(Some(mm)) if mm.at + 1 == small.len() => mm,
            _ => m,
        };
        let mut rep = format!("=== seed {seed} resp{} [{sig}]: command {} of {}, minimised to {}\n", if resp3 { 3 } else { 2 }, shown.at + 1, cmds.len(), small.len());
        rep.push_str("FLUSHALL\n");
        if resp3 {
            rep.push_str("@resp 3\n");
        }
        for c in &small {
            rep.push_str(&line_of(c));
            rep.push('\n');
        }
        rep.push_str(&format!("    redis: {}\n    roll:  {}\n", show(&shown.redis), show(&shown.roll)));
        eprintln!("{rep}");
        reports.push(rep);
    }
    eprintln!("fuzz: seeds {seed0}..{} ({} commands, families {fams:?}): {} distinct mismatches", seed0 + seeds, total, seen.len());
    for (sig, seed, n) in &seen {
        eprintln!("    {n:>4}× [{sig}] first at seed {seed}");
    }
    if let (n @ 1.., Some(first)) = &*runner.doubles.lock().unwrap() {
        eprintln!("fuzz: the reference answered {n} commands twice (a reference bug; roll answers once), first:\n    {first}");
    }
    for c in &crashes {
        eprintln!("fuzz: reference crash: {c}");
    }
    if !reports.is_empty() {
        panic!("{} distinct mismatches:\n{}", reports.len(), reports.join("\n"));
    }
}
