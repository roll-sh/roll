//! Differential fuzzer: random command sequences run against a reference Redis and an in-process
//! roll server, and every reply must be byte-identical (RESP2 and RESP3, exact error texts),
//! apart from the nondeterminism Redis has itself (TTL drift, idle times, random picks, the
//! order of a hashtable's SCAN pages).
//!
//! Commands come from a grammar built from the reference's `COMMAND DOCS` / `COMMAND INFO`
//! (arguments filled from per-name value pools: keys from a small shared keyspace, so commands
//! interact and hit WRONGTYPE, numbers with parsing edge cases, scores/lex/stream-id ranges…),
//! plus hand-written generators: full SCAN iterations compared as sets, DUMP → RESTORE, scripts
//! and functions, MULTI/EXEC/WATCH with a second connection, encoding thresholds via CONFIG
//! SET, arity errors for every command, and a snapshot of every key at the end of a sequence.
//! A sequence stops at its first difference, which is minimised (commands removed while the
//! last one still differs) and reported once per signature.
//!
//! ```text
//! ROLL_FUZZ_REDIS=127.0.0.1:55701 cargo test -p redis-server --test fuzz_diff -- --nocapture
//! ROLL_FUZZ_SEED=7            first seed (default 1); sequence n of seed s is reproducible
//! ROLL_FUZZ_ITERS=500         sequences (default 200)
//! ROLL_FUZZ_LEN=60            commands per sequence (default 50)
//! ROLL_FUZZ_FAMILIES=lists,sets   only these families (strings keys lists sets zsets streams
//!                             hll geo hashes tx scripting server arity config)
//! ROLL_FUZZ_SEQ=12            run only sequence 12 (reproduce a report)
//! ROLL_FUZZ_ORDER=1           also report replies that differ only in element order
//! ROLL_FUZZ_MAX=30            stop after this many distinct failures (default 50)
//! ROLL_FUZZ_SKIP=SINTERCARD,LPOS   don't report differences of these commands (known gaps)
//! ```
//!
//! Without `ROLL_FUZZ_REDIS` the test returns at once. Failures are printed as compat script
//! lines (`tests/compat/*.txt` syntax) to paste into a regression file.

use bytes::BytesMut;
use redis_engine::Config;
use redis_proto::{Reply, decode, encode_command};
use redis_server::{Database, start_single};
use std::collections::{HashMap, HashSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------------------------
// PRNG (splitmix64)

#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        let mut r = Rng(seed ^ 0x5DEE_CE66_D1CE_4E5B);
        r.next();
        r
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i64
    }
}

// ---------------------------------------------------------------------------------------------
// Connections that keep each reply's raw bytes

struct Conn {
    s: TcpStream,
    buf: BytesMut,
}

#[derive(Clone, Debug)]
enum Res {
    Reply(Vec<u8>, Reply),
    Timeout,
    Closed,
}

impl Conn {
    async fn open(addr: &str) -> std::io::Result<Conn> {
        let s = TcpStream::connect(addr).await?;
        s.set_nodelay(true)?;
        Ok(Conn { s, buf: BytesMut::with_capacity(16 * 1024) })
    }

    async fn read(&mut self) -> std::io::Result<(Vec<u8>, Reply)> {
        loop {
            match decode(&self.buf) {
                Ok(Some((r, n))) => return Ok((self.buf.split_to(n).to_vec(), r)),
                Ok(None) => {}
                Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            }
            if self.s.read_buf(&mut self.buf).await? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    async fn call(&mut self, args: &[Vec<u8>], ms: u64) -> Res {
        let mut out = BytesMut::new();
        encode_command(args.iter(), &mut out);
        if self.s.write_all(&out).await.is_err() {
            return Res::Closed;
        }
        match tokio::time::timeout(std::time::Duration::from_millis(ms), self.read()).await {
            Ok(Ok((raw, r))) => Res::Reply(raw, r),
            Ok(Err(_)) => Res::Closed,
            Err(_) => Res::Timeout,
        }
    }

    async fn cmd(&mut self, args: &[&str]) -> Reply {
        let a: Vec<Vec<u8>> = args.iter().map(|s| s.as_bytes().to_vec()).collect();
        match self.call(&a, 30_000).await {
            Res::Reply(_, r) => r,
            other => panic!("fuzz: {args:?}: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The command table (from the reference)

#[derive(Clone, Debug, Default)]
struct Arg {
    name: String,
    typ: String,
    token: Option<String>,
    optional: bool,
    multiple: bool,
    multiple_token: bool,
    args: Vec<Arg>,
}

#[derive(Clone, Debug)]
struct Spec {
    /// `object|encoding`, lowercase.
    name: String,
    /// `["OBJECT", "ENCODING"]`.
    words: Vec<String>,
    arity: i64,
    group: String,
    args: Vec<Arg>,
}

fn text(r: &Reply) -> String {
    match r {
        Reply::Bulk(b) | Reply::Verbatim(_, b) => String::from_utf8_lossy(b).into_owned(),
        Reply::Status(s) => s.clone(),
        Reply::Int(n) => n.to_string(),
        Reply::Ok => "OK".into(),
        _ => String::new(),
    }
}

fn fields(r: &Reply) -> Vec<(String, Reply)> {
    match r {
        Reply::Map(pairs) => pairs.iter().map(|(k, v)| (text(k), v.clone())).collect(),
        Reply::Array(items) => items.chunks(2).filter(|c| c.len() == 2).map(|c| (text(&c[0]), c[1].clone())).collect(),
        _ => Vec::new(),
    }
}

fn items(r: &Reply) -> Vec<Reply> {
    match r {
        Reply::Array(v) | Reply::Set(v) => v.clone(),
        _ => Vec::new(),
    }
}

fn parse_arg(r: &Reply) -> Arg {
    let mut a = Arg::default();
    for (k, v) in fields(r) {
        match k.as_str() {
            "name" => a.name = text(&v),
            "type" => a.typ = text(&v),
            "token" => a.token = Some(text(&v)),
            "flags" => {
                for f in items(&v) {
                    match text(&f).as_str() {
                        "optional" => a.optional = true,
                        "multiple" => a.multiple = true,
                        "multiple_token" => a.multiple_token = true,
                        _ => {}
                    }
                }
            }
            "arguments" => a.args = items(&v).iter().map(parse_arg).collect(),
            _ => {}
        }
    }
    a
}

fn parse_docs(r: &Reply, parent: Option<&str>, arities: &HashMap<String, i64>, out: &mut Vec<Spec>) {
    for (name, doc) in fields(r) {
        let name = name.to_ascii_lowercase();
        let mut spec = Spec {
            words: name.split('|').map(|w| w.to_ascii_uppercase()).collect(),
            arity: *arities.get(&name).unwrap_or(&-1),
            name: name.clone(),
            group: String::new(),
            args: Vec::new(),
        };
        let _ = parent;
        for (k, v) in fields(&doc) {
            match k.as_str() {
                "group" => spec.group = text(&v),
                "arguments" => spec.args = items(&v).iter().map(parse_arg).collect(),
                "subcommands" => parse_docs(&v, Some(&name), arities, out),
                _ => {}
            }
        }
        out.push(spec);
    }
}

fn parse_info(r: &Reply, out: &mut HashMap<String, i64>) {
    for c in items(r) {
        let v = items(&c);
        if v.len() < 2 {
            continue;
        }
        let name = text(&v[0]).to_ascii_lowercase();
        if let Reply::Int(a) = v[1] {
            out.insert(name, a);
        }
        if let Some(subs) = v.get(9) {
            parse_info(subs, out);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Commands and checks

#[derive(Clone, Debug, PartialEq)]
enum Check {
    /// Raw bytes equal.
    Exact,
    /// Equal once arrays/sets/maps are sorted (multisets).
    Sorted,
    /// Same type and length (errors: same text).
    Shape,
    /// Integers within the tolerance (TTL drift between the two servers).
    Tol(i64),
    /// Values after these fields, and millisecond timestamps, masked.
    Mask(&'static [&'static str]),
    /// Bulk strings of the same length (a payload listing a seeded hash table).
    SameLen,
}

#[derive(Clone, Debug)]
enum Kind {
    Plain,
    /// Iterate a SCAN-family command to cursor 0 (the cursor is argument `pos`), compare the
    /// elements as a multiset.
    ScanAll { pos: usize, pairs: bool },
    /// DUMP `src` on the reference, then this RESTORE (the payload at argument 3) on both.
    Restore { src: Vec<u8> },
    /// FUNCTION DUMP on the reference, then FUNCTION RESTORE payload [policy] on both.
    FuncRestore,
    /// TYPE, encoding, value and TTL presence of a key, one reply.
    Snapshot,
    /// DUMP, except that a hashtable's payload (in its seeded hash order) shows as its length.
    Dump,
}

#[derive(Clone, Debug)]
struct Cmd {
    conn: usize,
    kind: Kind,
    args: Vec<Vec<u8>>,
    check: Check,
}

impl Cmd {
    fn plain(args: Vec<Vec<u8>>, check: Check) -> Self {
        Cmd { conn: 0, kind: Kind::Plain, args, check }
    }
    fn line(&self) -> String {
        let mut s = self.args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        match &self.kind {
            Kind::ScanAll { .. } => s = format!("(full scan) {s}"),
            Kind::Restore { src } => s = format!("(payload: DUMP {}) {s}", quote(src)),
            Kind::FuncRestore => s = format!("(payload: FUNCTION DUMP) {s}"),
            Kind::Snapshot => s = format!("(snapshot) {s}"),
            Kind::Dump => {}
            Kind::Plain => {}
        }
        if self.conn != 0 { format!("@conn{} {s}", self.conn + 1) } else { s }
    }
}

fn quote(a: &[u8]) -> String {
    let plain = !a.is_empty() && a.iter().all(|&c| c.is_ascii_graphic() && c != b'"' && c != b'\'' && c != b'\\' && c != b'{');
    if plain {
        return String::from_utf8_lossy(a).into_owned();
    }
    let mut s = String::from("\"");
    for &c in a {
        match c {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            b'\t' => s.push_str("\\t"),
            0x20..=0x7e => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    s.push('"');
    s
}

fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

// ---------------------------------------------------------------------------------------------
// Value pools

const NUMS: &[&str] = &[
    "0", "1", "2", "3", "-1", "-2", "10", "100", "5", "7", "abc", "", " 1", "1 ", "+1", "-0", "0x10", "1e3", "1.5", "01", "9223372036854775807",
    "9223372036854775808", "-9223372036854775808", "-9223372036854775809", "18446744073709551615", "4294967296", "2147483648", "inf",
    "nan", "1.0", "-",
];
const SMALL: &[&str] = &["0", "1", "2", "3", "4", "5", "-1", "-2", "-3", "10", "20", "100", "-100", "abc", "1.5", "9223372036854775807", "-9223372036854775808", "+2", " 3"];
const DOUBLES: &[&str] = &[
    "0", "1", "-1", "1.5", "2", "-2.5", "3.0", "10", "0.1", "1e3", "1e-5", "inf", "+inf", "-inf", "nan", "abc", "", "1e309", "-0", "0x10", "  2", "5e-324",
    "1.7976931348623157e308", "123456789012345678", "3.0000000000000001", "1,5", "(1",
];
const SCORE_RANGES: &[&str] =
    &["-inf", "+inf", "inf", "0", "1", "2", "-1", "1.5", "(1", "(2", "(1.5", "(-inf", "(+inf", "10", "(10", "abc", "", "(", "nan", "3", "(3", "[1", "1e2", "-0"];
const LEX_RANGES: &[&str] = &["-", "+", "[a", "(a", "[b", "(c", "[d", "[", "(", "a", "[1", "(10", "[aa", "(", "[\u{ff}", "+inf", "[zz"];
const MEMBERS: &[&str] = &[
    "a", "b", "c", "d", "e", "1", "2", "3", "10", "-1", "0", "100", "aa", "abc", "1.5", "x y", "", "Ω", "9223372036854775807", "9223372036854775808",
];
const VALUES: &[&str] = &[
    "a", "b", "abc", "1", "2", "0", "-1", "10", "3.5", "", " 1", "+1", "0x10", "1e3", "9223372036854775807", "9223372036854775808",
    "-9223372036854775808", "inf", "-inf", "nan", "1.0", "007", "hello world", "Ω", "\x00\x01", "foo", "bar", "12345678901234567890", "-0", "5.",
];
const PATTERNS: &[&str] = &["*", "k*", "k[12]", "?1", "*1*", "[a-k]*", "\\*", "k\\1", "[^k]*", "", "h*", "nomatch", "*[", "a*", "1*", "[!k]*"];
const STREAM_IDS: &[&str] = &[
    "-", "+", "0", "0-0", "0-1", "1", "1-0", "1-1", "2", "2-0", "3-1", "(1-0", "(2", "(0-0", "5", "10", "abc", "1-", "-1", "18446744073709551615-18446744073709551615",
    "18446744073709551615", "(18446744073709551615-18446744073709551615", "3-18446744073709551615", "1-abc", "$",
];
const TIMEOUTS: &[&str] = &["0.01", "0.01", "0.01", "0.02", "0.001", "-1", "abc", "0.010", "1e-2", "-inf", "9223372036854775807000"];
const LONS: &[&str] = &["0", "13.361389", "15.087269", "-122.27652", "180", "-180", "181", "-180.1", "abc", "2.35", "0.0001"];
const LATS: &[&str] = &["0", "38.115556", "37.502669", "37.80574", "85.05112878", "-85.05112878", "85.06", "-90", "abc", "48.85", "0.0001"];
const RADII: &[&str] = &["0", "1", "10", "100", "200", "500", "1000", "5000000", "-1", "abc", "0.5", "1e10", "inf"];
const GROUPS: &[&str] = &["g1", "g1", "g2", "nogroup"];
const CONSUMERS: &[&str] = &["c1", "c1", "c2", "c3"];
const BITFIELD_TYPES: &[&str] = &["u8", "i8", "u1", "i5", "u16", "i16", "u63", "i64", "u64", "i0", "x8", "u", "i65", "u32", "i32"];
const BITFIELD_OFFSETS: &[&str] = &["0", "1", "5", "8", "15", "100", "#0", "#1", "#3", "-1", "abc", "#", "#-1", "4294967296", "1000"];
const DBS: &[&str] = &["0", "1", "2", "1", "16", "-1", "abc", "3"];
const CONFIG_PARAMS: &[&str] = &[
    "maxmemory", "list-max-listpack-size", "set-max-intset-entries", "zset-max-listpack-entries", "hash-max-listpack-*", "*max*entries*", "nosuch",
    "appendonly", "save", "timeout", "databases", "lua-time-limit", "busy-reply-threshold", "*listpack*", "stream-node-max-*", "notify-keyspace-events",
];
const UNIX_S: &[&str] = &["4102444800", "4102444800", "1", "0", "-1", "abc", "9223372036854775807", "4102444801"];
const UNIX_MS: &[&str] = &["4102444800000", "4102444800000", "1", "0", "-1", "abc", "9223372036854775807", "4102444800500"];
const TTL_S: &[&str] = &["1000", "100000", "5000", "0", "-1", "abc", "9223372036854775807", "922337203685477", "1.5", ""];
const TTL_MS: &[&str] = &["1000003", "100000007", "5000011", "0", "-5", "abc", "9223372036854775807", "9223372036854775", "1.5"];

/// Lua scripts (EVAL / EVAL_RO / SCRIPT LOAD).
const SCRIPTS: &[&str] = &[
    "return 1",
    "return 'x'",
    "return {1,2,{3,'a'},'b'}",
    "return redis.call('GET',KEYS[1])",
    "return redis.call('SET',KEYS[1],ARGV[1])",
    "return redis.pcall('INCR',KEYS[1])",
    "return redis.call('INCR',KEYS[1])",
    "return {err='My Error'}",
    "return redis.error_reply('E2 bad thing')",
    "return redis.error_reply('no code')",
    "return redis.status_reply('FINE')",
    "return 3.99",
    "return -0.5",
    "return true",
    "return false",
    "return nil",
    "return {1,nil,3}",
    "return redis.call('LRANGE',KEYS[1],0,-1)",
    "return redis.call('HGETALL',KEYS[1])",
    "redis.setresp(3); return redis.call('HGETALL',KEYS[1])",
    "redis.setresp(3); return redis.call('ZSCORE',KEYS[1],ARGV[1])",
    "redis.setresp(3); return redis.call('SMEMBERS',KEYS[1])",
    "redis.setresp(3); return {double=3.5}",
    "return {double=3.5}",
    "redis.setresp(3); return {map={a=1}}",
    "redis.setresp(3); return {set={a=true}}",
    "redis.setresp(3); return {big_number='123456789012345678901234567890'}",
    "redis.setresp(3); return redis.call('HELLO')",
    "return redis.call('ZRANGE',KEYS[1],0,-1,'WITHSCORES')",
    "return cjson.encode({1,2,3})",
    "return cjson.decode('[1,2,{\"a\":3}]')[3].a",
    "return cmsgpack.unpack(cmsgpack.pack({1,2,'x'}))",
    "return bit.band(7,3)",
    "return redis.sha1hex('x')",
    "return tostring(1/0)",
    "return 1e20",
    "return #KEYS + #ARGV",
    "return redis.call('TYPE',KEYS[1])",
    "return redis.call('nosuch')",
    "return redis.call('SET',KEYS[1])",
    "return redis.call('EVAL','return 1',0)",
    "return redis.call('MULTI')",
    "syntax error(",
    "return KEYS[1] .. ARGV[1]",
    "error('boom')",
    "error({err='XERR custom'})",
    "return redis.call('EXPIRE',KEYS[1],100000)",
    "return redis.call('PING')",
    "local t = redis.call('SMEMBERS',KEYS[1]); table.sort(t); return t",
    "return redis.call('XRANGE',KEYS[1],'-','+')",
    "return redis.call('OBJECT','ENCODING',KEYS[1])",
    "redis.call('SELECT',1); return redis.call('DBSIZE')",
    "return redis.call('BLPOP',KEYS[1],0)",
    "return string.rep('x', 3)",
    "return redis.REDIS_VERSION",
    "return redis.call('SPOP',KEYS[1],1000)",
    "return redis.call('DEL',KEYS[1],KEYS[2])",
    "return redis.call('GET','undeclared')",
    "return redis.call('LPUSH',KEYS[1],'a','b')",
    "#!lua flags=no-writes\nreturn redis.call('SET',KEYS[1],'x')",
    "#!lua flags=no-writes\nreturn redis.call('GET',KEYS[1])",
    "#!lua\nreturn redis.call('GET',KEYS[1])",
    "#!lua flags=allow-oom,no-cluster\nreturn 1",
    "#!lua flags=bad-flag\nreturn 1",
    "#!lua name=x\nreturn 1",
    "#!js\nreturn 1",
    "return redis.setresp(4)",
    "return redis.call('INCRBYFLOAT',KEYS[1],'1.5')",
    "return tonumber(ARGV[1])",
    "return redis.call('ECHO', ARGV[1])",
    "return {ok='STATUS'}",
    "return redis.call('SCAN', '0')",
    "return redis.pcall('LPOP',KEYS[1])",
    "return pcall(redis.call,'INCR',KEYS[1])",
    "return redis.call('SET',KEYS[1],ARGV[1],'GET')",
    "return redis.acl_check_cmd('set','x','y')",
    "return redis.call('BITFIELD',KEYS[1],'INCRBY','u8',0,1)",
    "local x = 0 for i=1,100 do x = x + i end return x",
    "return redis.call('ZADD',KEYS[1],ARGV[1],'m')",
    "return redis.call('HSET',KEYS[1],'f',ARGV[1])",
    "return redis.call('XADD',KEYS[1],'5-*','f','v')",
    "return redis.call('EXISTS',KEYS[1])",
    "return redis.call('DBSIZE')",
    "return redis.call('COPY',KEYS[1],KEYS[2])",
];

const FUNCTIONS: &[&str] = &[
    "#!lua name=lib1\nredis.register_function('f1', function(keys, args) return redis.call('GET', keys[1]) end)",
    "#!lua name=lib1\nredis.register_function('f1', function(keys, args) return 'v2' end)\nredis.register_function('f1b', function(keys, args) return #keys end)",
    "#!lua name=lib2\nredis.register_function{function_name='f2', callback=function(keys, args) return args[1] end, flags={'no-writes'}}",
    "#!lua name=lib3\nredis.register_function('f3', function(keys, args) return redis.call('SET', keys[1], args[1]) end)",
    "#!lua name=lib4\nredis.register_function{function_name='f4', callback=function(keys, args) return redis.call('INCR', keys[1]) end, flags={'no-writes'}, description='desc'}",
    "#!lua name=bad\nreturn 1",
    "#!lua name=bad2\nredis.register_function('f-1!', function() return 1 end)",
    "#!lua name=bad3\nredis.register_function('f1', function() return 1 end)",
    "#!lua\nredis.register_function('nf', function() return 1 end)",
    "#!js name=x\nfoo",
    "notashebang",
    "#!lua name=lib5\nredis.register_function('f5', function(keys, args) error('oops') end)",
    "#!lua name=lib6\nredis.call('SET','x','y')",
    "#!lua name=lib7\nredis.register_function{function_name='f7', callback=function() return 1 end, flags={'bogus'}}",
    "#!lua name=lib8 foo=bar\nredis.register_function('f8', function() return 8 end)",
];
const FUNCTION_NAMES: &[&str] = &["f1", "f1b", "f2", "f3", "f4", "f5", "f7", "f8", "nosuch", "nf"];
const LIBRARY_NAMES: &[&str] = &["lib1", "lib2", "lib3", "lib4", "lib5", "nolib", "lib*", "*"];

/// CONFIG parameters a sequence may change (reset to the reference's defaults before each).
const CONFIGS: &[(&str, &[&str])] = &[
    ("list-max-listpack-size", &["-2", "-1", "1", "2", "4", "128", "-6", "abc"]),
    ("list-compress-depth", &["0", "1", "2"]),
    ("set-max-intset-entries", &["512", "2", "4"]),
    ("set-max-listpack-entries", &["128", "2", "4"]),
    ("set-max-listpack-value", &["64", "4"]),
    ("zset-max-listpack-entries", &["128", "2", "4", "0"]),
    ("zset-max-listpack-value", &["64", "4", "1"]),
    ("hash-max-listpack-entries", &["128", "2", "0"]),
    ("hash-max-listpack-value", &["64", "4"]),
    ("stream-node-max-entries", &["100", "2", "4", "0"]),
    ("stream-node-max-bytes", &["4096", "64", "0"]),
    ("hll-sparse-max-bytes", &["3000", "50", "0"]),
    ("zset-max-ziplist-entries", &["128", "3"]),
    ("hash-max-ziplist-value", &["64", "3"]),
];

/// Never generated from the grammar (dangerous, connection-changing, time- or server-dependent,
/// module commands, or hash field expiration, which is someone else's job).
fn excluded(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "shutdown", "debug", "monitor", "sync", "psync", "replconf", "replicaof", "slaveof", "failover", "migrate", "wait", "waitaof", "quit", "reset",
        "hello", "auth", "dump", "save", "bgsave", "bgrewriteaof", "lastsave", "time", "info", "lolwut", "pfselftest", "restore", "restore-asking", "scan", "sscan",
        "hscan", "zscan", "eval", "eval_ro", "evalsha", "evalsha_ro", "fcall", "fcall_ro", "subscribe", "psubscribe", "ssubscribe", "unsubscribe",
        "punsubscribe", "sunsubscribe", "trimslots", "hexpire", "hpexpire", "hexpireat", "hpexpireat", "hexpiretime", "hpexpiretime", "hpersist", "httl",
        "hpttl", "hgetex", "hsetex", "role", "spop", "randomkey", "srandmember", "zrandmember", "hrandfield", "xidmprecord", "publish", "spublish", "multi",
    ];
    let top = name.split('|').next().unwrap();
    NAMES.contains(&name)
        || name.contains('.')
        || name.starts_with('_')
        || matches!(
            top,
            "cluster" | "module" | "acl" | "client" | "config" | "latency" | "slowlog" | "memory" | "backup" | "himport" | "hotkeys" | "script" | "function" | "pubsub"
        )
        || name.starts_with('v') && name.chars().nth(1).is_some_and(|c| c.is_ascii_uppercase())
}

// ---------------------------------------------------------------------------------------------
// Generators

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Family {
    Strings,
    Keys,
    Lists,
    Sets,
    Zsets,
    Streams,
    Hll,
    Geo,
    Hashes,
    Tx,
    Scripting,
    Server,
    Arity,
    Config,
}

const FAMILIES: &[(Family, &str)] = &[
    (Family::Strings, "strings"),
    (Family::Keys, "keys"),
    (Family::Lists, "lists"),
    (Family::Sets, "sets"),
    (Family::Zsets, "zsets"),
    (Family::Streams, "streams"),
    (Family::Hll, "hll"),
    (Family::Geo, "geo"),
    (Family::Hashes, "hashes"),
    (Family::Tx, "tx"),
    (Family::Scripting, "scripting"),
    (Family::Server, "server"),
    (Family::Arity, "arity"),
    (Family::Config, "config"),
];

fn family_of(spec: &Spec) -> Option<Family> {
    Some(match spec.group.as_str() {
        "string" | "bitmap" => Family::Strings,
        "generic" => Family::Keys,
        "list" => Family::Lists,
        "set" => Family::Sets,
        "sorted-set" => Family::Zsets,
        "stream" => Family::Streams,
        "hyperloglog" => Family::Hll,
        "geo" => Family::Geo,
        "hash" => Family::Hashes,
        "transactions" => Family::Tx,
        "server" | "connection" => Family::Server,
        _ => return None,
    })
}

struct Table {
    specs: Vec<Spec>,
    by_family: HashMap<Family, Vec<usize>>,
    by_name: HashMap<String, usize>,
    /// SHA1 of each of SCRIPTS (from the reference).
    shas: Vec<String>,
    /// The reference's values of CONFIGS' parameters.
    config_defaults: Vec<(String, String)>,
}

struct Gen<'a> {
    rng: Rng,
    t: &'a Table,
    keys: Vec<String>,
    families: Vec<Family>,
    /// The next stream id XADD tends to use (`ms-seq`).
    stream_ms: u64,
    stream_seq: u64,
    resp3: bool,
    /// A MULTI is open on the first connection (SCAN pages would be queued).
    in_multi: bool,
    /// Sorted sets get score 0 only, and lex ranges are generated (they are only defined
    /// when all scores are equal; otherwise they walk randomly built skiplist levels).
    lex_mode: bool,
}

impl<'a> Gen<'a> {
    fn key(&mut self) -> Vec<u8> {
        if self.rng.chance(0.04) {
            return b(self.rng.pick(&["nokey", "k1{t}", "", "w_1"]));
        }
        b(&self.keys[self.rng.below(self.keys.len())].clone())
    }

    fn pool(&mut self, p: &[&str]) -> Vec<u8> {
        b(self.rng.pick(p))
    }

    fn member(&mut self) -> Vec<u8> {
        if self.rng.chance(0.03) {
            return b(&"m".repeat(70));
        }
        if self.rng.chance(0.25) {
            return self.rng.range(0, 40).to_string().into_bytes();
        }
        self.pool(MEMBERS)
    }

    fn stream_id(&mut self) -> Vec<u8> {
        if self.rng.chance(0.3) {
            let ms = self.rng.range(0, self.stream_ms as i64 + 1);
            if self.rng.chance(0.5) {
                return format!("{ms}-{}", self.rng.range(0, 3)).into_bytes();
            }
            return ms.to_string().into_bytes();
        }
        self.pool(STREAM_IDS)
    }

    fn next_xadd_id(&mut self) -> Vec<u8> {
        if self.rng.chance(0.75) {
            if self.rng.chance(0.7) {
                self.stream_ms += self.rng.range(1, 3) as u64;
                self.stream_seq = 0;
            } else {
                self.stream_seq += 1;
            }
            if self.rng.chance(0.1) {
                return format!("{}-*", self.stream_ms).into_bytes();
            }
            return format!("{}-{}", self.stream_ms, self.stream_seq).into_bytes();
        }
        let p = ["0-0", "0-1", "1-1", "abc", "1-*", "0-*", "18446744073709551615-18446744073709551615", "5", "-1", "3-18446744073709551615", "1-2"];
        self.pool(&p)
    }

    /// A value for an argument named `name` (of `typ`) of command `cmd`.
    fn value(&mut self, cmd: &str, name: &str, typ: &str) -> Vec<u8> {
        let top = cmd.split('|').next().unwrap();
        let stream = top.starts_with('x');
        let lexcmd = cmd.contains("lex");
        match (typ, name) {
            ("key", _) => self.key(),
            ("pattern", _) if top == "sort" || top == "sort_ro" => self.pool(&["w_*", "h_*->f", "#", "nosort", "w_*->f", "*", "nokey_*"]),
            ("pattern", _) => self.pool(PATTERNS),
            ("unix-time", n) if n.contains("milli") => self.pool(UNIX_MS),
            ("unix-time", _) => self.pool(UNIX_S),
            (_, "timeout") => self.pool(TIMEOUTS),
            (_, "milliseconds") if stream => self.pool(&["10", "10", "-1", "abc", "20"]),
            (_, "seconds") => self.pool(TTL_S),
            (_, "milliseconds") => self.pool(TTL_MS),
            (_, "min-idle-time") => self.pool(&["0", "0", "10000000", "abc", "-1"]),
            (_, "count") if matches!(top, "srandmember" | "zrandmember" | "hrandfield") => self.pool(&["1", "3", "5", "-3", "-5", "0", "50", "-50", "9223372036854775807", "-9223372036854775808", "abc"]),
            (_, "count") if top == "spop" => self.pool(&["1000", "0", "-1", "abc", "9223372036854775807"]),
            (_, "count") if top == "xautoclaim" => self.pool(&["1", "2", "10", "0", "-1", "abc", "9223372036854775807", "100"]),
            (_, "count") => self.pool(SMALL),
            ("integer", "offset") if top == "setrange" => self.pool(&["0", "1", "3", "10", "100", "-1", "536870912", "abc", "1000"]),
            ("integer", "offset") if top == "setbit" || top == "getbit" => {
                self.pool(&["0", "1", "7", "8", "15", "31", "100", "1000", "-1", "4294967296", "abc", "9"])
            }
            (_, "offset") if top.starts_with("bitfield") => self.pool(BITFIELD_OFFSETS),
            (_, "encoding") if top.starts_with("bitfield") => self.pool(BITFIELD_TYPES),
            (_, "value" | "increment") if top.starts_with("bitfield") => {
                self.pool(&["0", "1", "-1", "100", "255", "256", "-129", "9223372036854775807", "-9223372036854775808", "abc", "1.5"])
            }
            ("integer", "bit") => self.pool(&["0", "1", "2", "-1", "abc"]),
            ("integer", "value") if top == "setbit" => self.pool(&["0", "1", "2", "-1", "abc"]),
            (_, "db" | "destination-db" | "index" | "index1" | "index2") if matches!(top, "select" | "move" | "swapdb" | "copy") => self.pool(DBS),
            (_, "start" | "end" | "stop" | "rank" | "index") if !stream && typ == "integer" => self.pool(&["0", "1", "2", "-1", "-2", "3", "5", "100", "-100", "abc", "9223372036854775807", "-9223372036854775808"]),
            (_, "start" | "end" | "stop") if !stream => self.pool(&["0", "1", "-1", "2", "-2", "abc"]),
            (_, "id" | "start" | "end" | "stream-id" | "lastid" | "last-id" | "max-deleted-id" | "threshold") if stream => {
                if name == "threshold" && self.rng.chance(0.5) {
                    return self.pool(&["0", "1", "2", "3", "5", "10", "-1", "abc"]);
                }
                if top == "xreadgroup" && name == "id" && self.rng.chance(0.6) {
                    return b(">");
                }
                self.stream_id()
            }
            (_, "group") => self.pool(GROUPS),
            (_, "consumer") => self.pool(CONSUMERS),
            (_, "entriesread" | "entries-added") => self.pool(&["0", "1", "2", "5", "-1", "-2", "abc", "100"]),
            (_, "pid") => self.pool(&["p1", "p2"]),
            (_, "iid") => self.pool(&["i1", "i2", "i3"]),
            (_, "idmp-duration" | "idmp-maxsize") => self.pool(&["1", "100", "1000", "0", "-1", "abc", "100000", "10001"]),
            (_, "min" | "max") if lexcmd => self.pool(LEX_RANGES),
            (_, "min" | "max") => self.pool(SCORE_RANGES),
            ("double", "score") if self.lex_mode => b("0"),
            ("double", "increment") if self.lex_mode => b("0"),
            ("double", "score") | (_, "weight") => self.pool(DOUBLES),
            ("double", "increment") => self.pool(DOUBLES),
            (_, "increment" | "decrement") => self.pool(NUMS),
            (_, "longitude" | "lon") => self.pool(LONS),
            (_, "latitude" | "lat") => self.pool(LATS),
            (_, "radius" | "width" | "height") => self.pool(RADII),
            (_, "member" | "member1" | "member2" | "element" | "pivot" | "field") => self.member(),
            (_, "numkeys" | "numids" | "numfields" | "num_fields" | "num") => self.pool(&["1", "2", "3", "0", "-1", "abc", "100"]),
            (_, "limit") => self.pool(&["0", "1", "2", "3", "5", "-1", "abc", "100"]),
            (_, "len" | "min-match-len") => self.pool(&["0", "1", "2", "3", "-1", "abc"]),
            (_, "maxcount" | "maxsize" | "exactly") => self.pool(&["1", "2", "3", "0", "-1", "abc", "100", "9223372036854775807"]),
            (_, "parameter") => self.pool(CONFIG_PARAMS),
            (_, "command-name") => b(&self.t.specs[self.rng.below(self.t.specs.len())].words[0].to_ascii_lowercase()),
            (_, "category") => self.pool(&["string", "list", "keyspace", "read", "dangerous", "nosuch", "STRING"]),
            (_, "message" | "channel") => self.pool(&["ch1", "m"]),
            (_, "ifeq-value" | "ifne-value") => self.pool(VALUES),
            (_, "ifdeq-digest" | "ifdne-digest") => self.pool(&["0", "abc", "1234567890abcdef", "e8b86bc0d2ebb53a", "0000000000000000", "ffffffffffffffff", "FFFFFFFFFFFFFFFF", "1"]),
            (_, "lowerbound" | "upperbound") => self.pool(&["0", "10", "-10", "100", "1.5", "abc", "-9223372036854775808", "9223372036854775807", "inf"]),
            ("integer", _) => self.pool(SMALL),
            ("double", _) => self.pool(DOUBLES),
            (_, "value") if top == "set" || top == "append" || top == "getset" || top.starts_with("mset") => self.pool(VALUES),
            _ => {
                if self.rng.chance(0.3) {
                    self.member()
                } else {
                    self.pool(VALUES)
                }
            }
        }
    }

    /// Emit one argument (its repetitions); returns how many times it was emitted.
    fn emit(&mut self, cmd: &str, a: &Arg, out: &mut Vec<Vec<u8>>, forced_reps: Option<usize>) -> usize {
        if a.optional && forced_reps.is_none() && !self.rng.chance(0.4) {
            return 0;
        }
        let reps = forced_reps.unwrap_or_else(|| {
            if !a.multiple {
                1
            } else if self.rng.chance(0.03) {
                self.rng.range(10, 40) as usize
            } else {
                self.rng.range(1, 4) as usize
            }
        });
        for i in 0..reps {
            if let Some(t) = &a.token
                && (i == 0 || a.multiple_token)
            {
                out.push(self.case(t));
            }
            match a.typ.as_str() {
                "pure-token" => {}
                "oneof" => {
                    let choices: Vec<&Arg> = a.args.iter().filter(|c| !(cmd == "xadd" && c.name == "auto-id")).collect();
                    let c = choices[self.rng.below(choices.len())].clone();
                    let mut c2 = c.clone();
                    c2.optional = false;
                    self.emit(cmd, &c2, out, None);
                }
                "block" => self.emit_list(cmd, &a.args, out),
                _ => {
                    if cmd == "xadd" && a.name == "id" {
                        let v = self.next_xadd_id();
                        out.push(v);
                    } else {
                        let v = self.value(cmd, &a.name, &a.typ);
                        out.push(v);
                    }
                }
            }
        }
        reps
    }

    /// Arguments in order; a `num…` count before a repeated argument usually matches it, and
    /// repeated keys followed by repeated ids (XREAD STREAMS) repeat as often.
    fn emit_list(&mut self, cmd: &str, args: &[Arg], out: &mut Vec<Vec<u8>>) {
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            let next = args.get(i + 1);
            if a.typ == "integer" && a.name.starts_with("num") && a.token.is_none() && next.is_some_and(|n| n.multiple) && self.rng.chance(0.85) {
                let mut tmp = Vec::new();
                let n = self.emit(cmd, next.unwrap(), &mut tmp, None);
                let n = if n == 0 { 1 } else { n };
                if tmp.is_empty() {
                    self.emit(cmd, next.unwrap(), &mut tmp, Some(n));
                }
                out.push(n.to_string().into_bytes());
                out.extend(tmp);
                i += 2;
                continue;
            }
            if a.multiple && next.is_some_and(|n| n.multiple && n.typ != "key") && a.typ == "key" && self.rng.chance(0.85) {
                let n = self.emit(cmd, a, out, None).max(1);
                self.emit(cmd, next.unwrap(), out, Some(n));
                i += 2;
                continue;
            }
            self.emit(cmd, a, out, None);
            i += 1;
        }
    }

    fn case(&mut self, t: &str) -> Vec<u8> {
        if self.rng.chance(0.15) { b(&t.to_ascii_lowercase()) } else { b(t) }
    }

    /// A command from the grammar.
    fn grammar(&mut self, idx: usize) -> Vec<Vec<u8>> {
        let spec = &self.t.specs[idx];
        let name = spec.name.clone();
        let mut out: Vec<Vec<u8>> = spec.words.iter().map(|w| b(w)).collect();
        if self.rng.chance(0.08) {
            out[0] = b(&spec.words[0].to_ascii_lowercase());
        }
        let args = spec.args.clone();
        self.emit_list(&name, &args, &mut out);
        self.fixup(&name, &mut out);
        if self.rng.chance(0.07) {
            self.mutate(&args, &mut out, spec.words.len());
        }
        out
    }

    /// Context the grammar can't express.
    fn fixup(&mut self, name: &str, out: &mut [Vec<u8>]) {
        let upper: Vec<String> = out.iter().map(|a| String::from_utf8_lossy(a).to_ascii_uppercase()).collect();
        if name == "msetex" {
            // Keys stored already expired stay until the expire cycle (on its own clock on
            // each server) removes them: DBSIZE, KEYS… would race.
            for i in 0..out.len().saturating_sub(1) {
                if (upper[i] == "EXAT" || upper[i] == "PXAT") && out[i + 1] == b"1" {
                    out[i + 1] = b("4102444800");
                }
            }
        }
        if name == "set" && upper.iter().any(|a| a == "GET") {
            // SET … GET with a digest of the wrong length replies twice (Redis does too), which
            // would leave the connection a reply behind.
            for i in 0..out.len().saturating_sub(1) {
                if upper[i] == "IFDEQ" || upper[i] == "IFDNE" {
                    out[i + 1] = b("e8b86bc0d2ebb53a");
                }
            }
        }
        let (lo, hi) = match name {
            "zrange" => (2, 3),
            "zrangestore" => (3, 4),
            _ => return,
        };
        if out.len() <= hi || !self.rng.chance(0.85) {
            return;
        }
        if upper.iter().any(|a| a == "BYLEX") {
            out[lo] = self.pool(LEX_RANGES);
            out[hi] = self.pool(LEX_RANGES);
        } else if upper.iter().any(|a| a == "BYSCORE") {
            out[lo] = self.pool(SCORE_RANGES);
            out[hi] = self.pool(SCORE_RANGES);
        } else {
            out[lo] = self.pool(&["0", "1", "-1", "2", "-2", "100", "-100", "(1", "abc"]);
            out[hi] = self.pool(&["0", "1", "-1", "2", "-2", "100", "-100", "(1", "abc"]);
        }
    }

    /// Drop, duplicate, swap or replace an argument, or add one of the command's tokens.
    fn mutate(&mut self, args: &[Arg], out: &mut Vec<Vec<u8>>, fixed: usize) {
        let mut tokens = Vec::new();
        fn collect(a: &[Arg], t: &mut Vec<String>) {
            for x in a {
                if let Some(tok) = &x.token {
                    t.push(tok.clone());
                }
                collect(&x.args, t);
            }
        }
        collect(args, &mut tokens);
        let n = out.len();
        match self.rng.below(5) {
            0 if n > fixed => {
                let i = fixed + self.rng.below(n - fixed);
                out.remove(i);
            }
            1 if n > fixed => {
                let i = fixed + self.rng.below(n - fixed);
                let v = out[i].clone();
                out.insert(i, v);
            }
            2 if n > fixed + 1 => {
                let i = fixed + self.rng.below(n - fixed - 1);
                out.swap(i, i + 1);
            }
            3 if n > fixed => {
                let i = fixed + self.rng.below(n - fixed);
                out[i] = self.pool(NUMS);
            }
            _ if !tokens.is_empty() => {
                let t = tokens[self.rng.below(tokens.len())].clone();
                let i = fixed + self.rng.below(n - fixed + 1);
                out.insert(i, b(&t));
            }
            _ => {}
        }
    }

    fn check_for(&mut self, args: &[Vec<u8>]) -> Check {
        let w0 = String::from_utf8_lossy(&args[0]).to_ascii_lowercase();
        let w1 = args.get(1).map(|a| String::from_utf8_lossy(a).to_ascii_lowercase()).unwrap_or_default();
        match w0.as_str() {
            "ttl" | "expiretime" => Check::Tol(1),
            "pttl" | "pexpiretime" => Check::Tol(1500),
            "object" if w1 == "idletime" || w1 == "freq" => Check::Shape,
            "keys" => Check::Sorted,
            "command" if args.len() == 1 || matches!(w1.as_str(), "docs" | "info" | "list") => Check::Sorted,
            "xinfo" | "xpending" => Check::Mask(&["seen-time", "active-time", "idle", "inactive", "delivery-time"]),
            "xreadgroup" if args.iter().any(|a| a.eq_ignore_ascii_case(b"CLAIM")) => Check::Mask(&[]),
            "config" => Check::Sorted,
            // Elements tied under BY keep the input order, a set's or sorted set's hash order;
            // LIMIT then picks among them.
            "sort" | "sort_ro" if args.iter().any(|a| a.eq_ignore_ascii_case(b"BY")) => {
                if args.iter().any(|a| a.eq_ignore_ascii_case(b"LIMIT")) { Check::Shape } else { Check::Sorted }
            }
            _ => Check::Exact,
        }
    }

    fn cmd(&mut self, args: Vec<Vec<u8>>) -> Cmd {
        let check = self.check_for(&args);
        Cmd::plain(args, check)
    }

    fn grammar_cmd(&mut self, fam: Family) -> Option<Cmd> {
        let list = self.t.by_family.get(&fam)?;
        if list.is_empty() {
            return None;
        }
        let mut idx = list[self.rng.below(list.len())];
        // HELP pages are fixed text: now and then is enough.
        if self.t.specs[idx].name.ends_with("|help") && self.rng.chance(0.85) {
            idx = list[self.rng.below(list.len())];
        }
        let mut args = self.grammar(idx);
        // Lex ranges only over equal scores.
        for _ in 0..20 {
            let lex = self.t.specs[idx].name.contains("lex") || args.iter().any(|a| a.eq_ignore_ascii_case(b"BYLEX"));
            if self.lex_mode || !lex {
                break;
            }
            idx = list[self.rng.below(list.len())];
            args = self.grammar(idx);
        }
        Some(self.cmd(args))
    }

    /// One step of family `fam` (one or a few commands).
    fn step(&mut self, fam: Family) -> Vec<Cmd> {
        let r = self.rng.below(100);
        match fam {
            Family::Keys => match r {
                0..=5 => vec![self.scan_all("SCAN")],
                6..=9 => vec![self.restore()],
                10..=13 => {
                    let k = self.key();
                    vec![Cmd { conn: 0, kind: Kind::Snapshot, args: vec![b("SNAPSHOT"), k], check: Check::Exact }]
                }
                14..=17 => {
                    let k = self.key();
                    let enc = self.rng.pick(&["ENCODING", "ENCODING", "REFCOUNT", "FREQ", "IDLETIME", "HELP", "NOSUCH"]).to_string();
                    let args = vec![b("OBJECT"), b(&enc), k];
                    vec![self.cmd(args)]
                }
                18..=21 => {
                    let k = self.key();
                    let mut args = vec![b("DUMP"), k];
                    if self.rng.chance(0.05) {
                        args.push(b("x"));
                    }
                    vec![Cmd { conn: 0, kind: Kind::Dump, args, check: Check::Exact }]
                }
                22..=24 => {
                    let mut args = vec![b("RANDOMKEY")];
                    if self.rng.chance(0.1) {
                        args.push(b("x"));
                    }
                    vec![Cmd::plain(args, Check::Shape)]
                }
                25..=30 => vec![self.sort()],
                31..=34 => {
                    // Weights and hash fields for SORT BY / GET patterns.
                    let m = String::from_utf8_lossy(&self.member()).into_owned();
                    let v = self.pool(&["1", "2", "10", "-1", "1.5", "abc", "", "3"]);
                    let args = if self.rng.chance(0.5) { vec![b("SET"), b(&format!("w_{m}")), v] } else { vec![b("HSET"), b(&format!("h_{m}")), b("f"), v] };
                    vec![self.cmd(args)]
                }
                _ => self.grammar_cmd(fam).into_iter().collect(),
            },
            Family::Sets => match r {
                0..=7 => vec![self.scan_all("SSCAN")],
                8..=15 => {
                    let k = self.key();
                    let c = self.rng.pick(&["1000", "0", "-1", "abc", "9223372036854775807"]).to_string();
                    let mut args = vec![b("SPOP"), k];
                    {
                        args.push(b(&c));
                    }
                    vec![Cmd::plain(args, Check::Sorted)]
                }
                16..=23 => vec![self.random_pick("SRANDMEMBER", false)],
                _ => self.grammar_cmd(fam).into_iter().collect(),
            },
            Family::Zsets => match r {
                0..=5 => vec![self.scan_all("ZSCAN")],
                6..=12 => vec![self.random_pick("ZRANDMEMBER", true)],
                _ => self.grammar_cmd(fam).into_iter().collect(),
            },
            Family::Hashes => match r {
                0..=7 => vec![self.scan_all("HSCAN")],
                8..=14 => vec![self.random_pick("HRANDFIELD", true)],
                _ => self.grammar_cmd(fam).into_iter().collect(),
            },
            Family::Lists | Family::Strings | Family::Hll | Family::Geo | Family::Streams => {
                if fam == Family::Streams && r < 8 {
                    // A group to read with, most of the time.
                    let k = self.key();
                    let g = self.pool(GROUPS);
                    let id = self.pool(&["0", "$", "0-0", "1-1"]);
                    let mut args = vec![b("XGROUP"), b("CREATE"), k, g, id];
                    if self.rng.chance(0.6) {
                        args.push(b("MKSTREAM"));
                    }
                    if self.rng.chance(0.2) {
                        args.push(b("ENTRIESREAD"));
                        args.push(self.pool(&["0", "1", "5", "-1"]));
                    }
                    return vec![self.cmd(args)];
                }
                if fam == Family::Streams && r < 20 {
                    let k = self.key();
                    let mut args = vec![b("XADD"), k];
                    if self.rng.chance(0.2) {
                        args.push(b("MAXLEN"));
                        args.push(self.pool(&["~", "=", "1"]));
                        args.push(self.pool(&["1", "2", "3", "5", "0"]));
                    }
                    let id = self.next_xadd_id();
                    args.push(id);
                    for _ in 0..self.rng.range(1, 3) {
                        args.push(self.pool(&["f", "g", "h", "f1"]));
                        args.push(self.pool(VALUES));
                    }
                    return vec![self.cmd(args)];
                }
                if fam == Family::Streams && r < 45 {
                    return vec![self.stream_group_cmd()];
                }
                self.grammar_cmd(fam).into_iter().collect()
            }
            Family::Tx => match r {
                0..=14 => vec![Cmd::plain(vec![b("MULTI")], Check::Exact)],
                15..=29 => vec![Cmd::plain(vec![b(self.rng.pick(&["EXEC", "EXEC", "DISCARD"]))], Check::Exact)],
                30..=44 => {
                    let mut args = vec![b("WATCH")];
                    for _ in 0..self.rng.range(1, 2) {
                        args.push(self.key());
                    }
                    vec![Cmd::plain(args, Check::Exact)]
                }
                45..=49 => vec![Cmd::plain(vec![b("UNWATCH")], Check::Exact)],
                _ => {
                    // A write from the other connection (invalidates WATCH).
                    let fams: Vec<Family> =
                        self.families.iter().copied().filter(|f| !matches!(f, Family::Tx | Family::Arity | Family::Server | Family::Config | Family::Scripting)).collect();
                    let f = if fams.is_empty() { Family::Strings } else { fams[self.rng.below(fams.len())] };
                    let mut cmds = self.step(f);
                    if self.rng.chance(0.6) {
                        for c in &mut cmds {
                            c.conn = 1;
                        }
                    }
                    cmds
                }
            },
            Family::Scripting => self.scripting(),
            Family::Server => match r {
                0..=9 => vec![self.getkeys()],
                10..=14 => {
                    let p = self.pool(CONFIG_PARAMS);
                    vec![Cmd::plain(vec![b("CONFIG"), b("GET"), p], Check::Sorted)]
                }
                _ => self.grammar_cmd(fam).into_iter().collect(),
            },
            Family::Arity => vec![self.arity()],
            Family::Config => {
                let (p, vals) = CONFIGS[self.rng.below(CONFIGS.len())];
                let v = self.pool(vals);
                vec![Cmd::plain(vec![b("CONFIG"), b("SET"), b(p), v], Check::Exact)]
            }
        }
    }

    /// Consumer-group traffic with ids that exist more often than the grammar's.
    fn stream_group_cmd(&mut self) -> Cmd {
        let k = self.key();
        let g = self.pool(&["g1", "g1", "g2"]);
        let c = self.pool(&["c1", "c1", "c2"]);
        let ids = |s: &mut Self, n: usize| (0..n).map(|_| s.stream_id()).collect::<Vec<_>>();
        let mut args = match self.rng.below(9) {
            0..=2 => {
                let mut a = vec![b("XREADGROUP"), b("GROUP"), g, c];
                if self.rng.chance(0.4) {
                    a.push(b("COUNT"));
                    a.push(self.pool(&["1", "2", "5", "0"]));
                }
                if self.rng.chance(0.15) {
                    a.push(b("NOACK"));
                }
                if self.rng.chance(0.15) {
                    a.push(b("CLAIM"));
                    a.push(self.pool(&["0", "10000000"]));
                }
                if self.rng.chance(0.1) {
                    a.push(b(self.rng.pick(&["MAXCOUNT", "MAXSIZE"])));
                    a.push(self.pool(&["1", "2", "100", "0"]));
                }
                if self.rng.chance(0.15) {
                    a.push(b("BLOCK"));
                    a.push(b("10"));
                }
                a.push(b("STREAMS"));
                a.push(k);
                a.push(self.pool(&[">", ">", ">", "0", "0-0", "1-0", "$"]));
                a
            }
            3 => {
                let mut a = vec![b("XPENDING"), k, g];
                if self.rng.chance(0.6) {
                    if self.rng.chance(0.3) {
                        a.push(b("IDLE"));
                        a.push(self.pool(&["0", "10000000"]));
                    }
                    a.push(self.pool(&["-", "0", "(1-0"]));
                    a.push(self.pool(&["+", "10", "(5-0"]));
                    a.push(self.pool(&["10", "1", "2"]));
                    if self.rng.chance(0.4) {
                        a.push(c);
                    }
                }
                a
            }
            4 => {
                let mut a = vec![b("XCLAIM"), k, g, c, self.pool(&["0", "0", "10000000"])];
                let n = self.rng.range(1, 3) as usize;
                a.extend(ids(self, n));
                for _ in 0..self.rng.below(3) {
                    match self.rng.below(5) {
                        0 => a.push(b("JUSTID")),
                        1 => a.extend([b("RETRYCOUNT"), self.pool(&["0", "5", "-1"])]),
                        2 => a.push(b("FORCE")),
                        3 => a.extend([b("IDLE"), self.pool(&["20000000", "0"])]),
                        _ => a.extend([b("LASTID"), self.stream_id()]),
                    }
                }
                a
            }
            5 => {
                let mut a = vec![b("XAUTOCLAIM"), k, g, c, self.pool(&["0", "0", "10000000"]), self.pool(&["0", "0-0", "1-0", "(1"])];
                if self.rng.chance(0.5) {
                    a.extend([b("COUNT"), self.pool(&["1", "2", "100"])]);
                }
                if self.rng.chance(0.3) {
                    a.push(b("JUSTID"));
                }
                a
            }
            6 => {
                let mut a = vec![b("XACK"), k, g];
                let n = self.rng.range(1, 3) as usize;
                a.extend(ids(self, n));
                a
            }
            7 => {
                let ack = self.rng.chance(0.5);
                let mut a = if ack { vec![b("XACKDEL"), k, g] } else { vec![b("XDELEX"), k] };
                if self.rng.chance(0.6) {
                    a.push(b(self.rng.pick(&["KEEPREF", "DELREF", "ACKED"])));
                }
                let n = self.rng.range(1, 3) as usize;
                a.extend([b("IDS"), n.to_string().into_bytes()]);
                a.extend(ids(self, n));
                a
            }
            _ => {
                let mut a = vec![b("XINFO"), b("STREAM"), k];
                if self.rng.chance(0.6) {
                    a.push(b("FULL"));
                    if self.rng.chance(0.4) {
                        a.extend([b("COUNT"), self.pool(&["0", "1", "2"])]);
                    }
                }
                a
            }
        };
        if self.rng.chance(0.03) {
            args.push(b("BOGUS"));
        }
        self.cmd(args)
    }

    fn scan_all(&mut self, which: &str) -> Cmd {
        if self.in_multi {
            // A queued page's cursor depends on the seeded hash.
            return Cmd::plain(vec![b("PING")], Check::Exact);
        }
        let mut args = vec![b(which)];
        let pos;
        if which == "SCAN" {
            pos = 1;
            args.push(b("0"));
        } else {
            args.push(self.key());
            pos = 2;
            args.push(b("0"));
        }
        if self.rng.chance(0.4) {
            args.push(self.case("MATCH"));
            args.push(self.pool(PATTERNS));
        }
        if self.rng.chance(0.4) {
            args.push(b("COUNT"));
            args.push(self.pool(&["1", "2", "10", "1000", "0", "-1", "abc"]));
        }
        if which == "SCAN" && self.rng.chance(0.4) {
            args.push(b("TYPE"));
            args.push(self.pool(&["string", "list", "set", "zset", "hash", "stream", "nosuch", "STRING", "ReJSON-RL"]));
        }
        if (which == "HSCAN" || which == "ZSCAN") && self.rng.chance(0.25) {
            args.push(b("NOVALUES"));
        }
        if self.rng.chance(0.03) {
            args.push(self.pool(&["NOSUCH", "MATCH"]));
        }
        let pairs = which == "HSCAN" && !args.iter().any(|a| a.eq_ignore_ascii_case(b"NOVALUES")) || which == "ZSCAN" && !args.iter().any(|a| a.eq_ignore_ascii_case(b"NOVALUES"));
        if self.rng.chance(0.05) {
            args[pos] = self.pool(&["abc", "-1", "18446744073709551616", "1.5", " 0"]);
            return Cmd::plain(args, Check::Shape);
        }
        Cmd { conn: 0, kind: Kind::ScanAll { pos, pairs }, args, check: Check::Sorted }
    }

    fn random_pick(&mut self, which: &str, scores: bool) -> Cmd {
        let k = self.key();
        let mut args = vec![b(which), k];
        if self.rng.chance(0.8) {
            args.push(self.pool(&["1", "3", "5", "-3", "-5", "0", "50", "-50", "9223372036854775807", "-9223372036854775808", "abc", "1000", "2"]));
            if scores && self.rng.chance(0.4) {
                args.push(b(if which == "ZRANDMEMBER" { "WITHSCORES" } else { "WITHVALUES" }));
            }
        }
        // A positive count at least the cardinality returns everything.
        let all = args.get(2).is_some_and(|c| matches!(&c[..], b"1000" | b"50" | b"9223372036854775807"));
        Cmd::plain(args, if all { Check::Sorted } else { Check::Shape })
    }

    fn restore(&mut self) -> Cmd {
        let src = self.key();
        let dst = self.key();
        let mut args = vec![b("RESTORE"), dst, b(self.rng.pick(&["0", "0", "100000000", "-1", "abc", "4102444800000"])), b("PAYLOAD")];
        if self.rng.chance(0.4) {
            args.push(b("REPLACE"));
        }
        if self.rng.chance(0.2) {
            args.push(b("ABSTTL"));
        }
        if self.rng.chance(0.15) {
            args.push(b("IDLETIME"));
            args.push(self.pool(&["10", "-1", "abc", "1000"]));
        }
        if self.rng.chance(0.15) {
            args.push(b("FREQ"));
            args.push(self.pool(&["10", "-1", "256", "abc"]));
        }
        if self.rng.chance(0.05) {
            args.push(b("BOGUS"));
        }
        Cmd { conn: 0, kind: Kind::Restore { src }, args, check: Check::Exact }
    }

    fn sort(&mut self) -> Cmd {
        let ro = self.rng.chance(0.3);
        let mut args = vec![b(if ro { "SORT_RO" } else { "SORT" }), self.key()];
        if self.rng.chance(0.3) {
            args.push(b("BY"));
            args.push(self.pool(&["w_*", "h_*->f", "nosort", "w_*->f", "nokey_*", "*"]));
        }
        if self.rng.chance(0.3) {
            args.push(b("LIMIT"));
            args.push(self.pool(&["0", "1", "-1", "abc"]));
            args.push(self.pool(&["1", "2", "10", "-1", "0"]));
        }
        for _ in 0..self.rng.below(3) {
            args.push(b("GET"));
            args.push(self.pool(&["#", "w_*", "h_*->f", "nokey_*", "*"]));
        }
        if self.rng.chance(0.3) {
            let o = *self.rng.pick(&["ASC", "DESC"]);
            args.push(self.case(o));
        }
        if self.rng.chance(0.5) {
            args.push(self.case("ALPHA"));
        }
        // Not with BY: ties would be stored in hash order.
        if !ro && !args.iter().any(|a| a == b"BY") && self.rng.chance(0.3) {
            args.push(b("STORE"));
            args.push(self.key());
        }
        if self.rng.chance(0.05) {
            args.push(b("BOGUS"));
        }
        self.cmd(args)
    }

    fn scripting(&mut self) -> Vec<Cmd> {
        let r = self.rng.below(100);
        let nk = self.rng.range(0, 2) as usize;
        let tail = |g: &mut Self, args: &mut Vec<Vec<u8>>| {
            let bad = g.rng.chance(0.08);
            args.push(if bad { g.pool(&["-1", "abc", "5", ""]) } else { nk.to_string().into_bytes() });
            for _ in 0..nk {
                args.push(g.key());
            }
            for _ in 0..g.rng.range(0, 2) {
                let v = g.pool(VALUES);
                args.push(v);
            }
        };
        let cmd = match r {
            0..=39 => {
                let i = self.rng.below(SCRIPTS.len());
                let ro = self.rng.chance(0.2);
                let mut args = vec![b(if ro { "EVAL_RO" } else { "EVAL" }), b(SCRIPTS[i])];
                tail(self, &mut args);
                args
            }
            40..=49 => {
                let i = self.rng.below(SCRIPTS.len());
                let ro = self.rng.chance(0.2);
                let mut args = vec![b(if ro { "EVALSHA_RO" } else { "EVALSHA" }), b(&self.t.shas[i])];
                if self.rng.chance(0.1) {
                    args[1] = self.pool(&["abc", "ffffffffffffffffffffffffffffffffffffffff", ""]);
                }
                tail(self, &mut args);
                args
            }
            50..=56 => {
                let i = self.rng.below(SCRIPTS.len());
                vec![b("SCRIPT"), b("LOAD"), b(SCRIPTS[i])]
            }
            57..=60 => {
                let mut args = vec![b("SCRIPT"), b("EXISTS")];
                for _ in 0..self.rng.range(1, 3) {
                    let i = self.rng.below(SCRIPTS.len());
                    args.push(b(&self.t.shas[i]));
                }
                args
            }
            61..=70 => {
                let mut args = vec![b("FUNCTION"), b("LOAD")];
                if self.rng.chance(0.3) {
                    args.push(b("REPLACE"));
                }
                args.push(self.pool(FUNCTIONS));
                args
            }
            71..=82 => {
                let ro = self.rng.chance(0.25);
                let mut args = vec![b(if ro { "FCALL_RO" } else { "FCALL" }), self.pool(FUNCTION_NAMES)];
                tail(self, &mut args);
                args
            }
            83..=87 => {
                let mut args = vec![b("FUNCTION"), b("LIST")];
                if self.rng.chance(0.4) {
                    args.push(b("WITHCODE"));
                }
                if self.rng.chance(0.4) {
                    args.push(b("LIBRARYNAME"));
                    args.push(self.pool(LIBRARY_NAMES));
                }
                return vec![Cmd::plain(args, Check::Sorted)];
            }
            88..=90 => vec![b("FUNCTION"), b("DELETE"), self.pool(LIBRARY_NAMES)],
            91..=92 => vec![b("FUNCTION"), b("STATS")],
            93..=95 => {
                let mut args = vec![b("FUNCTION"), b("RESTORE"), b("PAYLOAD")];
                if self.rng.chance(0.7) {
                    args.push(self.pool(&["FLUSH", "APPEND", "REPLACE", "BOGUS"]));
                }
                return vec![Cmd { conn: 0, kind: Kind::FuncRestore, args, check: Check::Exact }];
            }
            96 => vec![b("FUNCTION"), b("FLUSH")],
            97 => vec![b("SCRIPT"), b("FLUSH")],
            _ => return vec![Cmd::plain(vec![b("FUNCTION"), b("DUMP")], Check::SameLen)],
        };
        vec![self.cmd(cmd)]
    }

    /// COMMAND GETKEYS / GETKEYSANDFLAGS of a generated command.
    fn getkeys(&mut self) -> Cmd {
        let core: Vec<usize> = (0..self.t.specs.len()).filter(|&i| !self.t.specs[i].name.contains('.') && !self.t.specs[i].name.starts_with('_')).collect();
        let idx = core[self.rng.below(core.len())];
        let inner = self.grammar(idx);
        let mut args = vec![b("COMMAND"), b(if self.rng.chance(0.7) { "GETKEYS" } else { "GETKEYSANDFLAGS" })];
        args.extend(inner);
        Cmd::plain(args, Check::Exact)
    }

    /// A command with an argument count its arity rejects (never executed).
    fn arity(&mut self) -> Cmd {
        loop {
            let s = &self.t.specs[self.rng.below(self.t.specs.len())];
            if s.name.contains('.') || s.name.starts_with('_') || s.name.starts_with("debug") || s.name.starts_with("shutdown") {
                continue;
            }
            let a = s.arity;
            let words = s.words.clone();
            let n: i64 = if a > 0 {
                let mut n = self.rng.range(1.max(words.len() as i64), a + 2);
                if n == a {
                    n += 1;
                }
                n
            } else {
                let min = -a;
                if min <= words.len() as i64 {
                    continue;
                }
                self.rng.range(words.len() as i64, min - 1)
            };
            let mut args: Vec<Vec<u8>> = words.iter().map(|w| b(w)).collect();
            while (args.len() as i64) < n {
                args.push(self.pool(&["a", "1", "k1", "x"]));
            }
            args.truncate(n.max(1) as usize);
            if self.rng.chance(0.1) {
                // An unknown command or subcommand.
                let w = self.pool(&["NOSUCHCMD", "GETT", "x", ""]);
                if args.len() > 1 && words.len() > 1 {
                    args[1] = w;
                } else {
                    args[0] = w;
                }
            }
            return Cmd::plain(args, Check::Exact);
        }
    }

    fn sequence(&mut self, len: usize) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        // Encodings switch at low thresholds now and then.
        if self.rng.chance(0.3) {
            for _ in 0..self.rng.range(1, 3) {
                cmds.extend(self.step(Family::Config));
            }
        }
        while cmds.len() < len {
            let fam = if self.rng.chance(0.12) { Family::Keys } else { self.families[self.rng.below(self.families.len())] };
            let mut step = self.step(fam);
            for c in &mut step {
                // Queued, these would show hash-ordered payloads inside EXEC's reply.
                let func_dump = c.args.len() >= 2 && ((c.args[0].eq_ignore_ascii_case(b"FUNCTION") && c.args[1].eq_ignore_ascii_case(b"DUMP")) || (c.args[0].eq_ignore_ascii_case(b"OBJECT") && c.args[1].eq_ignore_ascii_case(b"REFCOUNT")));
                // So would replies compared loosely (random picks, TTLs): EXEC's is exact.
                if c.conn == 0 && self.in_multi && (!matches!(c.kind, Kind::Plain) || func_dump || c.check != Check::Exact) {
                    *c = Cmd::plain(vec![b("PING")], Check::Exact);
                }
                if c.conn == 0 && matches!(c.kind, Kind::Plain) {
                    let w = String::from_utf8_lossy(&c.args[0]).to_ascii_uppercase();
                    if w == "MULTI" {
                        self.in_multi = true;
                    } else if w == "EXEC" || w == "DISCARD" {
                        self.in_multi = false;
                    }
                }
            }
            cmds.extend(step);
        }
        if self.in_multi {
            cmds.push(Cmd::plain(vec![b("EXEC")], Check::Exact));
        }
        for k in self.keys.clone() {
            cmds.push(Cmd { conn: 0, kind: Kind::Snapshot, args: vec![b("SNAPSHOT"), b(&k)], check: Check::Exact });
        }
        let _ = self.resp3;
        cmds
    }
}

// ---------------------------------------------------------------------------------------------
// Running and comparing

struct Side {
    addr: String,
    conns: Vec<Conn>,
}

impl Side {
    async fn open(addr: &str, resp3: bool) -> Side {
        let mut conns = Vec::new();
        for _ in 0..2 {
            let mut c = Conn::open(addr).await.unwrap();
            if resp3 {
                c.cmd(&["HELLO", "3"]).await;
            }
            conns.push(c);
        }
        Side { addr: addr.to_string(), conns }
    }

    async fn exec(&mut self, cmd: &Cmd, args: &[Vec<u8>]) -> Res {
        let c = &mut self.conns[cmd.conn];
        match &cmd.kind {
            Kind::Plain | Kind::Restore { .. } | Kind::FuncRestore => c.call(args, 3000).await,
            Kind::ScanAll { pos, pairs } => {
                let mut args = args.to_vec();
                let mut all: Vec<Reply> = Vec::new();
                for _ in 0..2000 {
                    let (raw, r) = match c.call(&args, 3000).await {
                        Res::Reply(raw, r) => (raw, r),
                        other => return other,
                    };
                    let v = items(&r);
                    if v.len() != 2 {
                        return Res::Reply(raw, r);
                    }
                    let elems = items(&v[1]);
                    if *pairs {
                        for p in elems.chunks(2) {
                            all.push(Reply::Array(p.to_vec()));
                        }
                    } else {
                        all.extend(elems);
                    }
                    let cur = text(&v[0]);
                    if cur == "0" {
                        all.sort_by_key(|x| format!("{x:?}"));
                        return Res::Reply(Vec::new(), Reply::Array(all));
                    }
                    args[*pos] = b(&cur);
                }
                Res::Reply(Vec::new(), Reply::Error("fuzz: scan did not finish".into()))
            }
            Kind::Dump => {
                let enc = call(c, vec![b"OBJECT", b"ENCODING", &args[1]]).await;
                match c.call(args, 3000).await {
                    Res::Reply(_, Reply::Bulk(p)) if matches!(text(&enc).as_str(), "hashtable" | "stream") => Res::Reply(Vec::new(), Reply::Array(vec![enc, Reply::Int(p.len() as i64)])),
                    other => other,
                }
            }
            Kind::Snapshot => {
                let k = args[1].clone();
                let ty = call(c, vec![b"TYPE", &k]).await;
                let enc = call(c, vec![b"OBJECT", b"ENCODING", &k]).await;
                let val = match text(&ty).as_str() {
                    "string" => call(c, vec![b"GET", &k]).await,
                    "list" => call(c, vec![b"LRANGE", &k, b"0", b"-1"]).await,
                    "set" => sorted(&call(c, vec![b"SMEMBERS", &k]).await),
                    "zset" => call(c, vec![b"ZRANGE", &k, b"0", b"-1", b"WITHSCORES"]).await,
                    "hash" => {
                        let r = call(c, vec![b"HGETALL", &k]).await;
                        let mut pairs: Vec<Reply> = fields(&r).into_iter().map(|(f, v)| Reply::Array(vec![Reply::Status(f), v])).collect();
                        pairs.sort_by_key(|x| format!("{x:?}"));
                        Reply::Array(pairs)
                    }
                    "stream" => {
                        let a = call(c, vec![b"XRANGE", &k, b"-", b"+"]).await;
                        let i = call(c, vec![b"XINFO", b"STREAM", &k]).await;
                        let g = call(c, vec![b"XINFO", b"GROUPS", &k]).await;
                        Reply::Array(vec![a, mask(&i, &[]), g])
                    }
                    _ => Reply::Nil,
                };
                let ttl = match call(c, vec![b"PTTL", &k]).await {
                    Reply::Int(n) if n >= 0 => Reply::Status("ttl".into()),
                    other => other,
                };
                Res::Reply(Vec::new(), Reply::Array(vec![ty, enc, val, ttl]))
            }
        }
    }
}

async fn call(c: &mut Conn, a: Vec<&[u8]>) -> Reply {
    let a: Vec<Vec<u8>> = a.into_iter().map(|x| x.to_vec()).collect();
    match c.call(&a, 3000).await {
        Res::Reply(_, r) => r,
        _ => Reply::Error("fuzz: no reply".into()),
    }
}

fn sorted(r: &Reply) -> Reply {
    match r {
        Reply::Array(items) | Reply::Set(items) => {
            let mut v: Vec<Reply> = items.iter().map(sorted).collect();
            v.sort_by_key(|x| format!("{x:?}"));
            Reply::Array(v)
        }
        Reply::Map(pairs) => {
            let mut v: Vec<(Reply, Reply)> = pairs.iter().map(|(k, v)| (k.clone(), sorted(v))).collect();
            v.sort_by_key(|(k, _)| format!("{k:?}"));
            Reply::Map(v)
        }
        other => other.clone(),
    }
}

fn shape(r: &Reply) -> String {
    match r {
        Reply::Array(v) => format!("array({})", v.len()),
        Reply::Set(v) => format!("set({})", v.len()),
        Reply::Map(v) => format!("map({})", v.len()),
        Reply::Bulk(_) => "bulk".into(),
        Reply::Int(_) => "int".into(),
        Reply::Double(_) => "double".into(),
        other => format!("{other:?}"),
    }
}

/// Values after `names`, and millisecond timestamps, as `*`.
fn mask(r: &Reply, names: &[&str]) -> Reply {
    let is = |x: &Reply| matches!(x, Reply::Bulk(b) if names.iter().any(|n| n.as_bytes() == &b[..])) || matches!(x, Reply::Status(s) if names.contains(&s.as_str()));
    let star = || Reply::Status("*".into());
    match r {
        Reply::Int(n) if (1_500_000_000_000..2_500_000_000_000).contains(n) => star(),
        Reply::Array(v) | Reply::Set(v) | Reply::Push(v) => {
            // XPENDING's entries: [id, consumer, idle, deliveries].
            if v.len() == 4 && matches!(&v[0], Reply::Bulk(b) if b.contains(&b'-')) && matches!(v[1], Reply::Bulk(_)) && matches!(v[2], Reply::Int(_)) {
                return Reply::Array(vec![v[0].clone(), v[1].clone(), star(), v[3].clone()]);
            }
            let mut out = Vec::new();
            let mut hide = false;
            for x in v {
                out.push(if hide { star() } else { mask(x, names) });
                hide = !hide && is(x);
            }
            Reply::Array(out)
        }
        Reply::Map(p) => Reply::Map(p.iter().map(|(k, v)| (k.clone(), if is(k) { star() } else { mask(v, names) })).collect()),
        other => other.clone(),
    }
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Same,
    /// Same elements, another order.
    Order,
    Diff,
}

fn compare(check: &Check, a: &Res, b: &Res) -> Verdict {
    let (Res::Reply(ra, a), Res::Reply(rb, b)) = (a, b) else {
        return match (a, b) {
            (Res::Closed, Res::Closed) | (Res::Timeout, Res::Timeout) => Verdict::Same,
            _ => Verdict::Diff,
        };
    };
    let dbg = |x: &Reply| format!("{x:?}");
    // FUNCTION RESTORE names the first clashing library in the payload's (seeded hash) order.
    let lib_clash = |e: &str| e.starts_with("ERR Library ") && e.ends_with(" already exists");
    if let (Reply::Error(x), Reply::Error(y)) = (a, b)
        && lib_clash(x)
        && lib_clash(y)
    {
        return Verdict::Same;
    }
    let same = match check {
        Check::Exact => (!ra.is_empty() && ra == rb) || (ra.is_empty() && rb.is_empty() && dbg(a) == dbg(b)),
        Check::Sorted => dbg(&sorted(a)) == dbg(&sorted(b)),
        Check::Shape => match (a, b) {
            (Reply::Error(x), Reply::Error(y)) => x == y,
            _ => shape(a) == shape(b),
        },
        Check::Tol(t) => match (a, b) {
            (Reply::Int(x), Reply::Int(y)) => x == y || (*x >= 0 && *y >= 0 && (x - y).abs() <= *t),
            _ => ra == rb,
        },
        Check::Mask(names) => dbg(&mask(a, names)) == dbg(&mask(b, names)),
        Check::SameLen => match (a, b) {
            (Reply::Bulk(x), Reply::Bulk(y)) => x.len() == y.len(),
            _ => ra == rb,
        },
    };
    if same {
        Verdict::Same
    } else if dbg(&sorted(a)) == dbg(&sorted(b)) {
        Verdict::Order
    } else {
        Verdict::Diff
    }
}

struct Env {
    ref_addr: String,
    roll_addr: String,
    table: Table,
    report_order: bool,
    /// ROLL_FUZZ_TRACE=1: print every command and the reference's reply.
    trace: bool,
}

/// What the side's FLUSHALL… setup and a sequence produced: the first differing command.
struct Outcome {
    first: Option<(usize, Verdict, Res, Res)>,
    /// Commands run (a sequence ends early on a timeout or a closed connection).
    ran: usize,
}

async fn setup(addr: &str, t: &Table) {
    let mut c = Conn::open(addr).await.unwrap();
    c.cmd(&["FLUSHALL"]).await;
    c.cmd(&["SCRIPT", "FLUSH"]).await;
    c.cmd(&["FUNCTION", "FLUSH"]).await;
    for (k, v) in &t.config_defaults {
        c.cmd(&["CONFIG", "SET", k, v]).await;
    }
}

async fn run(env: &Env, cmds: &[Cmd], resp3: bool) -> Outcome {
    let (_, _) = tokio::join!(setup(&env.ref_addr, &env.table), setup(&env.roll_addr, &env.table));
    let (mut r, mut o) = tokio::join!(Side::open(&env.ref_addr, resp3), Side::open(&env.roll_addr, resp3));
    let mut ran = 0;
    for (i, cmd) in cmds.iter().enumerate() {
        ran = i + 1;
        let (a, bb) = match &cmd.kind {
            Kind::Restore { src } => {
                // The reference's payload goes to both (DUMP itself is compared by Kind::Dump;
                // hashtables serialise in their seeded hash order).
                let payload = match r.conns[cmd.conn].call(&[b("DUMP"), src.clone()], 3000).await {
                    Res::Reply(_, Reply::Bulk(p)) => p.to_vec(),
                    _ => b("garbage"),
                };
                let _ = o.conns[cmd.conn].call(&[b("DUMP"), src.clone()], 3000).await;
                let mut args = cmd.args.clone();
                args[3] = payload;
                tokio::join!(r.exec(cmd, &args), o.exec(cmd, &args))
            }
            Kind::FuncRestore => {
                let payload = match r.conns[cmd.conn].call(&[b("FUNCTION"), b("DUMP")], 3000).await {
                    Res::Reply(_, Reply::Bulk(p)) => p.to_vec(),
                    _ => b("garbage"),
                };
                let _ = o.conns[cmd.conn].call(&[b("FUNCTION"), b("DUMP")], 3000).await;
                let mut args = cmd.args.clone();
                args[2] = payload;
                tokio::join!(r.exec(cmd, &args), o.exec(cmd, &args))
            }
            _ => tokio::join!(r.exec(cmd, &cmd.args), o.exec(cmd, &cmd.args)),
        };
        let v = compare(&cmd.check, &a, &bb);
        if env.trace {
            eprintln!("{}\n    -> {}", truncate(&cmd.line(), 300), truncate(&show(&a), 300));
        }
        let timeout = matches!(a, Res::Timeout) || matches!(bb, Res::Timeout);
        if v == Verdict::Diff || (v == Verdict::Order && env.report_order) {
            return Outcome { first: Some((i, v, a, bb)), ran: i + 1 };
        }
        if timeout || matches!(a, Res::Closed) {
            break;
        }
    }
    let _ = (&r.addr, &o.addr);
    Outcome { first: None, ran }
}

/// Remove commands while the last one still differs (and nothing before it does).
async fn minimise(env: &Env, cmds: Vec<Cmd>, resp3: bool) -> Vec<Cmd> {
    let mut cur = cmds;
    let mut chunk = (cur.len() / 2).max(1);
    let mut budget = 300;
    loop {
        let mut removed_any = false;
        let mut start = 0;
        while start + 1 < cur.len() && budget > 0 {
            let end = (start + chunk).min(cur.len() - 1);
            if end <= start {
                break;
            }
            let mut cand = cur[..start].to_vec();
            cand.extend_from_slice(&cur[end..]);
            budget -= 1;
            let out = run(env, &cand, resp3).await;
            if matches!(out.first, Some((i, Verdict::Diff, _, _)) if i == cand.len() - 1) {
                cur = cand;
                removed_any = true;
            } else {
                start = end;
            }
        }
        if budget == 0 || (chunk == 1 && !removed_any) {
            break;
        }
        if !removed_any {
            chunk = (chunk / 2).max(1);
        }
    }
    cur
}

fn summary(r: &Res) -> String {
    match r {
        Res::Reply(_, Reply::Error(e)) => {
            let w: Vec<&str> = e.split(' ').take(4).collect();
            format!("err:{}", w.join(" "))
        }
        Res::Reply(_, r) => shape(r).split('(').next().unwrap().to_string(),
        other => format!("{other:?}"),
    }
}

fn show(r: &Res) -> String {
    match r {
        Res::Reply(raw, r) if !raw.is_empty() => {
            let s = String::from_utf8_lossy(raw).replace("\r\n", "\\r\\n").chars().map(|c| if c.is_control() { '.' } else { c }).collect::<String>();
            let s = if s.len() > 1500 { format!("{}…", &s[..s.char_indices().nth(1500).map_or(s.len(), |(i, _)| i)]) } else { s };
            format!("{s}   {:?}", truncate(&format!("{r:?}"), 600))
        }
        Res::Reply(_, r) => truncate(&format!("{r:?}"), 2000),
        other => format!("{other:?}"),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fuzz_diff() {
    let Ok(ref_addr) = std::env::var("ROLL_FUZZ_REDIS") else {
        eprintln!("fuzz: skipped (set ROLL_FUZZ_REDIS=host:port to a throwaway redis:8-alpine)");
        return;
    };
    let env_u64 = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let seed = env_u64("ROLL_FUZZ_SEED", 1);
    let iters = env_u64("ROLL_FUZZ_ITERS", 200);
    let len = env_u64("ROLL_FUZZ_LEN", 50) as usize;
    let max_fails = env_u64("ROLL_FUZZ_MAX", 50) as usize;
    let only_seq = std::env::var("ROLL_FUZZ_SEQ").ok().and_then(|v| v.parse::<u64>().ok());
    let report_order = std::env::var("ROLL_FUZZ_ORDER").is_ok_and(|v| v == "1");
    let skip: HashSet<String> =
        std::env::var("ROLL_FUZZ_SKIP").unwrap_or_default().split(',').filter(|s| !s.is_empty()).map(|s| s.to_ascii_uppercase()).collect();
    let fam_filter: Option<Vec<String>> = std::env::var("ROLL_FUZZ_FAMILIES").ok().map(|v| v.split(',').map(str::to_string).collect());
    let families: Vec<Family> =
        FAMILIES.iter().filter(|(_, n)| fam_filter.as_ref().is_none_or(|f| f.iter().any(|x| x == n))).map(|(f, _)| *f).collect();
    assert!(!families.is_empty(), "fuzz: no family matches ROLL_FUZZ_FAMILIES");

    // The command table, script hashes and config defaults from the reference.
    let mut rc = Conn::open(&ref_addr).await.unwrap();
    rc.cmd(&["HELLO", "3"]).await;
    let mut arities = HashMap::new();
    parse_info(&rc.cmd(&["COMMAND", "INFO"]).await, &mut arities);
    let mut specs = Vec::new();
    parse_docs(&rc.cmd(&["COMMAND", "DOCS"]).await, None, &arities, &mut specs);
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut by_family: HashMap<Family, Vec<usize>> = HashMap::new();
    let mut by_name = HashMap::new();
    for (i, s) in specs.iter().enumerate() {
        by_name.insert(s.name.clone(), i);
        // Containers themselves: their subcommands are listed.
        if s.args.is_empty() && specs.iter().any(|x| x.name.starts_with(&format!("{}|", s.name))) {
            continue;
        }
        if excluded(&s.name) {
            continue;
        }
        if let Some(f) = family_of(s) {
            by_family.entry(f).or_default().push(i);
        }
    }
    let mut shas = Vec::new();
    for s in SCRIPTS {
        shas.push(match rc.cmd(&["SCRIPT", "LOAD", s]).await {
            Reply::Bulk(b) => String::from_utf8_lossy(&b).into_owned(),
            _ => "0000000000000000000000000000000000000000".to_string(),
        });
    }
    let mut config_defaults = Vec::new();
    for (p, _) in CONFIGS {
        if let Some((_, v)) = fields(&rc.cmd(&["CONFIG", "GET", p]).await).into_iter().next() {
            config_defaults.push((p.to_string(), text(&v)));
        }
    }
    drop(rc);

    let db = Database::new("fuzz", Config::default(), 1);
    db.start_cron();
    let roll = start_single(db, None).await.unwrap();
    // Like compat: both sides with RediSearch/TimeSeries keyspace subscriptions.
    for addr in [ref_addr.clone(), roll.to_string()] {
        let mut c = Conn::open(&addr).await.unwrap();
        c.cmd(&["FT.CREATE", "fuzz-subscribe", "ON", "HASH", "PREFIX", "1", "fuzz-subscribe:", "SCHEMA", "t", "TEXT"]).await;
        c.cmd(&["FT.DROPINDEX", "fuzz-subscribe"]).await;
        c.cmd(&["TS.CREATE", "fuzz-subscribe"]).await;
        c.cmd(&["DEL", "fuzz-subscribe"]).await;
    }
    let env = Env { ref_addr, roll_addr: roll.to_string(), table: Table { specs, by_family, by_name, shas, config_defaults }, report_order, trace: std::env::var("ROLL_FUZZ_TRACE").is_ok() };

    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut total_cmds = 0usize;
    let mut reported = 0usize;
    let started = std::time::Instant::now();
    for n in 0..iters {
        if only_seq.is_some_and(|s| s != n) {
            continue;
        }
        let mut rng = Rng::new(seed.wrapping_mul(0x1000_0000_01B3) ^ n.wrapping_mul(0x9E37_79B9));
        let mut fams = Vec::new();
        for _ in 0..rng.range(1, 3) {
            let f = *rng.pick(&families);
            if !fams.contains(&f) {
                fams.push(f);
            }
        }
        let all_keys = ["k1", "k2", "k3", "k4", "k5"];
        let nk = rng.range(2, 5) as usize;
        let resp3 = rng.chance(0.4);
        let lex_mode = !fams.contains(&Family::Geo) && !fams.contains(&Family::Scripting) && rng.chance(0.3);
        let mut g = Gen { rng, t: &env.table, keys: all_keys[..nk].iter().map(|s| s.to_string()).collect(), families: fams.clone(), stream_ms: 0, stream_seq: 0, resp3, in_multi: false, lex_mode };
        let cmds = g.sequence(len);

        let out = run(&env, &cmds, resp3).await;
        total_cmds += out.ran;
        let Some((i, verdict, _, _)) = out.first else { continue };
        let name = {
            let a = &cmds[i].args;
            let w0 = String::from_utf8_lossy(&a[0]).to_ascii_uppercase();
            let container = env.table.by_name.contains_key(&format!("{}|", w0.to_ascii_lowercase()))
                || env.table.specs.iter().any(|s| s.name.starts_with(&format!("{}|", w0.to_ascii_lowercase())));
            if container && a.len() > 1 { format!("{w0} {}", String::from_utf8_lossy(&a[1]).to_ascii_uppercase()) } else { w0 }
        };
        if skip.contains(&name) || skip.contains(name.split(' ').next().unwrap()) {
            continue;
        }
        let min = minimise(&env, cmds[..=i].to_vec(), resp3).await;
        let again = run(&env, &min, resp3).await;
        let Some((j, v2, a, bb)) = again.first else {
            eprintln!("fuzz: seed {seed} seq {n}: difference at {i} ({name}) did not reproduce");
            continue;
        };
        let sig = format!("{name} | {} | {} | {verdict:?}", summary(&a), summary(&bb));
        let count = seen.entry(sig.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            continue;
        }
        reported += 1;
        let mut msg = format!("\n=== fuzz difference #{reported}: {sig}\n# seed {seed} seq {n} (RESP{}) {v2:?}\nFLUSHALL\n", if resp3 { 3 } else { 2 });
        if resp3 {
            msg.push_str("@resp 3\n");
        }
        for (k, c) in min.iter().enumerate() {
            msg.push_str(&c.line());
            msg.push('\n');
            if k == j {
                break;
            }
        }
        msg.push_str(&format!("    redis: {}\n    roll:  {}\n", show(&a), show(&bb)));
        eprintln!("{msg}");
        if reported >= max_fails {
            break;
        }
    }
    eprintln!(
        "fuzz: seed {seed}: {iters} sequences, {total_cmds} commands, {} distinct differences ({} total) in {:.1}s",
        seen.len(),
        seen.values().sum::<usize>(),
        started.elapsed().as_secs_f64()
    );
    for (sig, c) in &seen {
        eprintln!("fuzz:   {c:5}  {sig}");
    }
    if !seen.is_empty() && std::env::var("ROLL_FUZZ_PANIC").is_ok() {
        panic!("fuzz: {} distinct differences", seen.len());
    }
}
