//! DEBUG: Redis' introspection and testing hooks. Subcommands that crash or restart Redis restart
//! the database instead (its data is durable), like SHUTDOWN; low-level dumps Redis prints on
//! stdout go to the log.

use super::*;
use crate::commands::replication::ServerAction;
use crate::dict::DictStats;
use crate::engine::Engine;
use crate::types::stream::StreamId;
use crate::value::{Entry, Str, Value};
use sha1::{Digest, Sha1};

/// DEBUG's switches, which Redis keeps in `server` (per engine here).
#[derive(Debug)]
pub struct Switches {
    /// SET-ACTIVE-EXPIRE: background expiry of keys and hash fields.
    pub active_expire: bool,
    /// SET-ALLOW-ACCESS-EXPIRED: logically expired keys and fields are served, not deleted.
    pub allow_access_expired: bool,
    /// SET-SKIP-CHECKSUM-VALIDATION: RESTORE payloads and RDB files aren't checksummed.
    pub skip_checksum: bool,
    /// PAUSE-CRON: no background work.
    pub pause_cron: bool,
    /// SET-DISABLE-DENY-SCRIPTS: scripts may call `noscript` commands.
    pub disable_deny_scripts: bool,
    /// QUICKLIST-PACKED-THRESHOLD: list elements this big get a plain node (0: by the fill).
    pub packed_threshold: usize,
    /// REPLYBUFFER PEAK-RESET-TIME (ms, -1 never) and RESIZING.
    pub reply_peak_reset_ms: i64,
    pub reply_resizing: bool,
}

/// `REPLY_BUFFER_DEFAULT_PEAK_RESET_TIME`.
pub const REPLY_PEAK_RESET_MS: i64 = 5000;

impl Default for Switches {
    fn default() -> Self {
        Switches {
            active_expire: true,
            allow_access_expired: false,
            skip_checksum: false,
            pause_cron: false,
            disable_deny_scripts: false,
            packed_threshold: 0,
            reply_peak_reset_ms: REPLY_PEAK_RESET_MS,
            reply_resizing: true,
        }
    }
}

const HELP: &[&str] = &[
    "DEBUG <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
    "AOF-FLUSH-SLEEP <microsec>",
    "    Server will sleep before flushing the AOF, this is used for testing.",
    "ASSERT",
    "    Crash by assertion failed.",
    "CHANGE-REPL-ID",
    "    Change the replication IDs of the instance.",
    "    Dangerous: should be used only for testing the replication subsystem.",
    "CONFIG-REWRITE-FORCE-ALL",
    "    Like CONFIG REWRITE but writes all configuration options, including",
    "    keywords not listed in original configuration file or default values.",
    "CRASH-AND-RECOVER [<milliseconds>]",
    "    Hard crash and restart after a <milliseconds> delay (default 0).",
    "DIGEST",
    "    Output a hex signature representing the current DB content.",
    "INTERNAL_SECRET",
    "    Return the cluster internal secret (hashed with crc16) or error if not in cluster mode.",
    "DIGEST-VALUE <key> [<key> ...]",
    "    Output a hex signature of the values of all the specified keys.",
    "ERROR <string>",
    "    Return a Redis protocol error with <string> as message. Useful for clients",
    "    unit tests to simulate Redis errors.",
    "LEAK <string>",
    "    Create a memory leak of the input string.",
    "LOG <message>",
    "    Write <message> to the server log.",
    "DEFRAG-FRAG-CACHE-STATS",
    "    Return hits counters for the defrag-side fragmentation cache.",
    "HTSTATS <dbid> [full]",
    "    Return hash table statistics of the specified Redis database.",
    "HTSTATS-KEY <key> [full]",
    "    Like HTSTATS but for the hash table stored at <key>'s value.",
    "KEYSIZES-HIST-ASSERT <0|1>",
    "    Enable/disable keysizes histogram assertion after each command.",
    "KEYMETA-AOF-DUMP <key>",
    "    Return a DUMP payload without KeyMeta for an AOF rewrite.",
    "LOADAOF",
    "    Flush the AOF buffers on disk and reload the AOF in memory.",
    "REPLICATE <string>",
    "    Replicates the provided string to replicas, allowing data divergence.",
    "MALLCTL <key> [<val>]",
    "    Get or set a malloc tuning integer.",
    "MALLCTL-STR <key> [<val>]",
    "    Get or set a malloc tuning string.",
    "OBJECT <key>",
    "    Show low level info about `key` and associated value.",
    "DROP-CLUSTER-PACKET-FILTER <packet-type>",
    "    Drop all packets that match the filtered type. Set to -1 allow all packets.",
    "ENABLE-KEYMETA-RUNTIME-REGISTRATION <0|1>",
    "    Allow keymeta class registration outside server startup (for testing).",
    "OOM",
    "    Crash the server simulating an out-of-memory error.",
    "PANIC",
    "    Crash the server simulating a panic.",
    "POPULATE <count> [<prefix>] [<size>]",
    "    Create <count> string keys named key:<num>. If <prefix> is specified then",
    "    it is used instead of the 'key' prefix. These are not propagated to",
    "    replicas. Cluster slots are not respected so keys not belonging to the",
    "    current node can be created in cluster mode.",
    "PROTOCOL <type>",
    "    Reply with a test value of the specified type. <type> can be: string,",
    "    integer, double, bignum, null, array, set, map, attrib, push, verbatim,",
    "    true, false.",
    "RELOAD [option ...]",
    "    Save the RDB on disk and reload it back to memory. Valid <option> values:",
    "    * MERGE: conflicting keys will be loaded from RDB.",
    "    * NOFLUSH: the existing database will not be removed before load, but",
    "      conflicting keys will generate an exception and kill the server.",
    "    * NOSAVE: the database will be loaded from an existing RDB file.",
    "    Examples:",
    "    * DEBUG RELOAD: verify that the server is able to persist, flush and reload",
    "      the database.",
    "    * DEBUG RELOAD NOSAVE: replace the current database with the contents of an",
    "      existing RDB file.",
    "    * DEBUG RELOAD NOSAVE NOFLUSH MERGE: add the contents of an existing RDB",
    "      file to the database.",
    "RESTART [<milliseconds>]",
    "    Graceful restart: save config, db, restart after a <milliseconds> delay (default 0).",
    "SDSLEN <key>",
    "    Show low level SDS string info representing `key` and value.",
    "SEGFAULT",
    "    Crash the server with sigsegv.",
    "SET-ACTIVE-EXPIRE <0|1>",
    "    Setting it to 0 disables expiring keys (and hash-fields) in background ",
    "    when they are not accessed (otherwise the Redis behavior). Setting it",
    "    to 1 reenables back the default.",
    "SET-ALLOW-ACCESS-EXPIRED <0|1>",
    "    Setting it to 0 prevents access to expired keys (and hash-fields),",
    "    simulating the standard Redis behavior. Setting it to 1 allows",
    "    access to expired keys (and hash-fields) without triggering deletion.",
    "QUICKLIST-PACKED-THRESHOLD <size>",
    "    Sets the threshold for elements to be inserted as plain vs packed nodes",
    "    Default value is 1GB, allows values up to 4GB. Setting to 0 restores to default.",
    "SET-SKIP-CHECKSUM-VALIDATION <0|1>",
    "    Enables or disables checksum checks for RDB files and RESTORE's payload.",
    "SLEEP <seconds>",
    "    Stop the server for <seconds>. Decimals allowed.",
    "STRINGMATCH-TEST",
    "    Run a fuzz tester against the stringmatchlen() function.",
    "STRUCTSIZE",
    "    Return the size of different Redis core C structures.",
    "LISTPACK <key>",
    "    Show low level info about the listpack encoding of <key>.",
    "QUICKLIST <key> [<0|1>]",
    "    Show low level info about the quicklist encoding of <key>.",
    "    The optional argument (0 by default) sets the level of detail",
    "CLIENT-EVICTION",
    "    Show low level client eviction pools info (maxmemory-clients).",
    "PAUSE-CRON <0|1>",
    "    Stop periodic cron job processing.",
    "REPLYBUFFER PEAK-RESET-TIME <NEVER||RESET|time>",
    "    Sets the time (in milliseconds) to wait between client reply buffer peak resets.",
    "    In case NEVER is provided the last observed peak will never be reset",
    "    In case RESET is provided the peak reset time will be restored to the default value",
    "REPLYBUFFER RESIZING <0|1>",
    "    Enable or disable the reply buffer resize cron job",
    "REPLY-COPY-AVOIDANCE <0|1>",
    "    Enable/disable reply copy avoidance optimization.",
    "REPL-PAUSE <clear|after-fork|before-rdb-channel|on-streaming-repl-buf>",
    "    Pause the server's main process during various replication steps.",
    "DICT-RESIZING <0|1>",
    "    Enable or disable the main dict and expire dict resizing.",
    "SCRIPT <LIST|<sha>>",
    "    Output SHA and content of all scripts or of a specific script with its SHA.",
    "MARK-INTERNAL-CLIENT [UNMARK]",
    "    Promote the current connection to an internal connection.",
    "ASM-FAILPOINT <channel> <state>",
    "    Set a fail point for the specified channel and state for cluster atomic slot migration.",
    "ASM-TRIM-METHOD <default|none|active|bg> <active-trim-delay> ",
    "    Disable trimming or force active/background trimming for cluster atomic slot migration.",
    "    Active trim delay is used only when method is 'active'. If it is negative,",
    "    active trim is disabled.",
    "CLUSTERLINK KILL <to|from|all> <node-id>",
    "    Kills the link based on the direction to/from (both) with the provided node.",
    "HELP",
    "    Print this help.",
];

/// `addReplySubcommandSyntaxError`.
fn syntax(args: &[Bytes]) -> Error {
    let sub = &args[1][..args[1].len().min(128)];
    Error::msg(format!("ERR unknown subcommand or wrong number of arguments for '{}'. Try DEBUG HELP.", String::from_utf8_lossy(sub)))
}

/// `atoi`: leading whitespace, a sign and digits; anything else ends the number.
fn atoi(a: &[u8]) -> i64 {
    let s = a.trim_ascii_start();
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let n = digits.iter().take_while(|c| c.is_ascii_digit()).fold(0i64, |n, &c| n.wrapping_mul(10).wrapping_add((c - b'0') as i64));
    (if neg { n.wrapping_neg() } else { n }) as i32 as i64
}

/// `strtod` on the longest numeric prefix (0 when there is none).
fn strtod_prefix(a: &[u8]) -> f64 {
    let s = std::str::from_utf8(a).unwrap_or("").trim_start();
    (1..=s.len()).rev().filter(|&i| s.is_char_boundary(i)).find_map(|i| s[..i].parse::<f64>().ok()).unwrap_or(0.0)
}

/// `getPositiveLongFromObjectOrReply`: one error for anything but a non-negative integer.
fn positive(a: &[u8]) -> Result<u64, Error> {
    match crate::util::parse_i64(a) {
        Some(v) if v >= 0 => Ok(v as u64),
        _ => Err(Error::new("ERR value is out of range, must be positive")),
    }
}

/// A server that just crashed or restarts: the database restarts after `delay_ms`.
fn restart(cx: &mut Cx, delay_ms: u64) -> CmdResult {
    cx.eng.server_actions.push(ServerAction::Restart { delay_ms });
    Ok(Reply::Raw(Bytes::new()))
}

pub fn debug(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !server::protected_allowed(cx.cfg().enable_debug_command, cx.client) {
        return Err(Error::new("ERR DEBUG command not allowed. If the enable-debug-command option is set to \"local\", you can run it from a local connection, otherwise you need to set this option in the configuration file, and then restart the server."));
    }
    let n = args.len();
    let is = |name: &str| eq(&args[1], name);
    // The switch arguments Redis reads with atoi.
    let flag = || atoi(&args[2]) != 0;
    match () {
        _ if n == 2 && is("help") => Ok(help(HELP)),
        _ if is("segfault") || is("panic") || is("assert") || is("oom") => {
            tracing::error!(subcommand = %String::from_utf8_lossy(&args[1]), "DEBUG crash: restarting the database");
            restart(cx, 0)
        }
        _ if is("restart") || is("crash-and-recover") => {
            let delay = if n >= 3 { int(&args[2])?.max(0) as u64 } else { 0 };
            restart(cx, delay)
        }
        _ if (is("keysizes-hist-assert") || is("allocsize-slots-assert") || is("drop-cluster-packet-filter")) && n == 3 => {
            int(&args[2])?;
            Ok(Reply::Ok)
        }
        _ if is("log") && n == 3 => {
            tracing::warn!("DEBUG LOG: {}", String::from_utf8_lossy(&args[2]));
            Ok(Reply::Ok)
        }
        _ if is("leak") && n == 3 => Ok(Reply::Ok),
        _ if is("reload") => reload(cx, args),
        _ if is("loadaof") => loadaof(cx),
        _ if is("object") && n == 3 => object(cx, &args[2]),
        _ if is("keymeta-aof-dump") && n == 3 => {
            let now = cx.now;
            Ok(match cx.eng.dbs[cx.client.db].keys.get(&args[2]) {
                Some(e) => Reply::Bulk(crate::rdb::dump(&e.value, now).into()),
                None => Reply::Nil,
            })
        }
        _ if is("sdslen") && n == 3 => {
            let key = &args[2];
            let Some(e) = cx.eng.dbs[cx.client.db].keys.get(key) else { return Err(NO_KEY) };
            match &e.value {
                Value::Str(s) => crate::object::sdslen(e, key.len(), s).map(Reply::Status).ok_or(Error::new("ERR Not an sds encoded string.")),
                _ => Err(Error::new("ERR Not an sds encoded string.")),
            }
        }
        _ if is("listpack") && n == 3 => {
            let Some(e) = cx.get_entry(&args[2]) else { return Err(NO_KEY) };
            if !matches!(e.value.encoding(), "listpack" | "listpackex") {
                return Err(Error::new("ERR Not a listpack encoded object."));
            }
            Ok(Reply::status("Listpack structure printed on stdout"))
        }
        _ if is("quicklist") && (n == 3 || n == 4) => {
            let Some(e) = cx.get_entry(&args[2]) else { return Err(NO_KEY) };
            if e.value.encoding() != "quicklist" {
                return Err(Error::new("ERR Not a quicklist encoded object."));
            }
            Ok(Reply::status("Quicklist structure printed on stdout"))
        }
        _ if is("populate") && (3..=5).contains(&n) => populate(cx, args),
        _ if is("digest") && n == 2 => Ok(Reply::Status(hex(&dataset_digest(cx.eng)))),
        _ if is("internal_secret") && n == 2 => Err(Error::new("ERR Internal secret is missing")),
        _ if is("digest-value") => {
            let db = &cx.eng.dbs[cx.client.db];
            Ok(Reply::Array(
                args[2..]
                    .iter()
                    .map(|k| {
                        let mut d = [0u8; 20];
                        if let Some(e) = db.keys.get(k) {
                            object_digest(&mut d, e);
                        }
                        Reply::Status(hex(&d))
                    })
                    .collect(),
            ))
        }
        _ if is("protocol") && n == 3 => protocol(cx, &args[2]),
        _ if is("sleep") && n == 3 => {
            let secs = strtod_prefix(&args[2]);
            if secs > 0.0 {
                std::thread::sleep(std::time::Duration::from_micros((secs * 1e6) as u64));
            }
            Ok(Reply::Ok)
        }
        _ if is("set-active-expire") && n == 3 => {
            cx.eng.debug.active_expire = flag();
            Ok(Reply::Ok)
        }
        _ if is("set-allow-access-expired") && n == 3 => {
            cx.eng.debug.allow_access_expired = flag();
            Ok(Reply::Ok)
        }
        _ if is("quicklist-packed-threshold") && n == 3 => {
            match std::str::from_utf8(&args[2]).ok().and_then(crate::config::memtoull) {
                Some(sz) if sz <= (1 << 32) - (1 << 20) => cx.eng.debug.packed_threshold = sz as usize,
                _ => return Err(Error::new("ERR argument must be a memory value bigger than 1 and smaller than 4gb")),
            }
            Ok(Reply::Ok)
        }
        _ if is("set-skip-checksum-validation") && n == 3 => {
            cx.eng.debug.skip_checksum = flag();
            Ok(Reply::Ok)
        }
        _ if (is("enable-keymeta-runtime-registration") || is("aof-flush-sleep") || is("reply-copy-avoidance")) && n == 3 => Ok(Reply::Ok),
        // `dictResizeAllowed`: the keyspace and expires tables stop growing and shrinking.
        _ if is("dict-resizing") && n == 3 => {
            let on = flag();
            for db in &mut cx.eng.dbs {
                db.keys.set_resize(on);
                db.volatile.set_resize(on);
            }
            Ok(Reply::Ok)
        }
        _ if is("replicate") && n >= 3 => {
            cx.eng.server_actions.push(ServerAction::Replicate(args[2..].to_vec()));
            Ok(Reply::Ok)
        }
        _ if is("error") && n == 3 => {
            // Written as is: not counted as an error reply.
            let msg: Vec<u8> = args[2].iter().map(|&c| if c == b'\n' || c == b'\r' { b' ' } else { c }).collect();
            let mut out = Vec::with_capacity(msg.len() + 3);
            out.push(b'-');
            out.extend_from_slice(&msg);
            out.extend_from_slice(b"\r\n");
            Ok(Reply::Raw(out.into()))
        }
        _ if is("structsize") && n == 2 => Ok(Reply::bulk("bits:64 robj:16 dictentry:24 sdshdr5:1 sdshdr8:3 sdshdr16:5 sdshdr32:9 sdshdr64:17 ")),
        _ if is("defrag-frag-cache-stats") => Ok(Reply::Verbatim("txt", Bytes::from_static(b"defrag_frag_cache_hits:0\r\n"))),
        _ if is("htstats") && n >= 3 => {
            let dbid = int(&args[2])?;
            if dbid < 0 || dbid >= cx.eng.dbs.len() as i64 {
                return Err(Error::new("ERR Out of range database"));
            }
            let full = n >= 4 && eq(&args[3], "full");
            let db = &cx.eng.dbs[dbid as usize];
            let mut s = String::from("[Dictionary HT]\n");
            s += &kvstore_stats(db.keys.len(), db.keys.stats(full), full);
            s += "[Expires HT]\n";
            s += &kvstore_stats(db.volatile.len(), db.volatile.stats(full), full);
            Ok(Reply::Verbatim("txt", s.into()))
        }
        _ if is("htstats-key") && n >= 3 => {
            let full = n >= 4 && eq(&args[3], "full");
            let Some(e) = cx.get_entry(&args[2]) else { return Err(NO_KEY) };
            let stats = match &e.value {
                Value::Hash(h) => h.dict_stats(full),
                Value::Set(s) => s.dict_stats(full),
                Value::ZSet(z) => z.dict_stats(full),
                _ => None,
            };
            let Some(stats) = stats else {
                return Err(Error::new("ERR The value stored at the specified key is not represented using an hash table"));
            };
            Ok(Reply::Verbatim("txt", stats.iter().map(|st| st.msg(full)).collect::<String>().into()))
        }
        _ if is("change-repl-id") && n == 2 => {
            cx.eng.server_actions.push(ServerAction::ChangeReplId);
            Ok(Reply::Ok)
        }
        _ if is("stringmatch-test") && n == 2 => Ok(Reply::status("Apparently Redis did not crash: test passed")),
        _ if is("set-disable-deny-scripts") && n == 3 => {
            cx.eng.debug.disable_deny_scripts = flag();
            Ok(Reply::Ok)
        }
        _ if is("config-rewrite-force-all") && n == 2 => Err(Error::new("ERR CONFIG-REWRITE-FORCE-ALL failed: Bad address")),
        _ if is("client-eviction") && n == 2 => client_eviction(cx),
        _ if (is("mallctl") || is("mallctl-str")) && n >= 3 => {
            if is("mallctl") && n > 3 {
                int(&args[3])?;
            }
            Err(Error::new("ERR No such file or directory"))
        }
        _ if is("pause-cron") && n == 3 => {
            cx.eng.debug.pause_cron = flag();
            Ok(Reply::Ok)
        }
        _ if is("replybuffer") && n == 4 => {
            if eq(&args[2], "peak-reset-time") {
                cx.eng.debug.reply_peak_reset_ms = if eq(&args[3], "never") {
                    -1
                } else if eq(&args[3], "reset") {
                    REPLY_PEAK_RESET_MS
                } else {
                    int(&args[3])?
                };
            } else if eq(&args[2], "resizing") {
                cx.eng.debug.reply_resizing = atoi(&args[3]) != 0;
            } else {
                return Err(syntax(args));
            }
            Ok(Reply::Ok)
        }
        _ if is("repl-pause") && n == 3 => {
            if !["clear", "after-fork", "before-rdb-channel", "on-streaming-repl-buf"].iter().any(|p| eq(&args[2], p)) {
                return Err(syntax(args));
            }
            Ok(Reply::Ok)
        }
        _ if is("script") && n == 3 => script(cx, args),
        _ if is("mark-internal-client") && n < 4 => {
            if n == 2 {
                cx.client.internal = true;
            } else if eq(&args[2], "unmark") {
                cx.client.internal = false;
            } else {
                return Err(syntax(args));
            }
            Ok(Reply::Ok)
        }
        _ if is("asm-failpoint") && n == 4 => Err(Error::new("ERR Failed to set ASM fail point")),
        _ if is("asm-trim-method") && n >= 3 => Err(Error::new("ERR Failed to set ASM trim method")),
        _ if is("clusterlink") && n == 5 && eq(&args[2], "kill") => Err(Error::new("ERR Debug option only available for cluster mode enabled setup!")),
        _ => Err(syntax(args)),
    }
}

/// `kvstoreGetStats` of a database's single dict: nothing while it's empty.
fn kvstore_stats(len: usize, stats: Vec<DictStats>, full: bool) -> String {
    if len == 0 { String::new() } else { stats.iter().map(|st| st.msg(full)).collect() }
}

/// DEBUG OBJECT key
fn object(cx: &mut Cx, key: &[u8]) -> CmdResult {
    let now = cx.now;
    let Some(e) = cx.eng.dbs[cx.client.db].keys.get(key) else { return Err(NO_KEY) };
    let mut extra = String::new();
    if let Value::List(l) = &e.value
        && let Some((fill, sizes)) = l.quicklist_info()
    {
        let (nodes, size) = sizes.fold((0usize, 0usize), |(n, s), sz| (n + 1, s + sz));
        extra = format!(
            " ql_nodes:{nodes} ql_avg_node:{:.2} ql_listpack_max:{} ql_compressed:{} ql_uncompressed_size:{size}",
            l.len() as f64 / nodes as f64,
            fill.size,
            (fill.compress != 0) as u8
        );
    }
    Ok(Reply::Status(format!(
        "Value at:{:p} refcount:1 encoding:{} serializedlength:{} lru:{} lru_seconds_idle:{}{extra}",
        e as *const Entry,
        e.encoding(key.len()),
        crate::rdb::saved_len(&e.value, now),
        e.lru & crate::engine::LRU_CLOCK_MAX,
        Engine::idle_secs(e, now),
    )))
}

/// DEBUG POPULATE count [prefix] [size]: string keys `prefix:N` (the name cut to 127 bytes)
/// holding `value:N`, cut or NUL-padded to `size`. Existing keys are left alone; nothing is
/// counted as a change or propagated.
fn populate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let count = positive(&args[2])?;
    let db = cx.client.db;
    cx.eng.dbs[db].keys.expand(count.min(usize::MAX as u64) as usize);
    let size = if args.len() == 5 { positive(&args[4])? as usize } else { 0 };
    let prefix: &[u8] = if args.len() == 3 { b"key" } else { &args[3] };
    for j in 0..count {
        let mut key = prefix.to_vec();
        key.push(b':');
        key.extend_from_slice(itoa::Buffer::new().format(j).as_bytes());
        key.truncate(127);
        if cx.peek(&key).is_some() {
            continue;
        }
        let mut val = format!("value:{j}").into_bytes();
        if size != 0 {
            val.resize(size, 0);
        }
        cx.set_encoded(&key, Value::Str(Str::from_slice(&val)), false, true, crate::object::ENC_STR);
    }
    cx.changes(0);
    Ok(Reply::Ok)
}

/// DEBUG RELOAD [MERGE] [NOFLUSH] [NOSAVE]: the dataset through an RDB image and back. The data
/// is durable, so the "existing file" NOSAVE loads is the current dataset too.
fn reload(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut merge, mut flush, mut save) = (false, true, true);
    for a in &args[2..] {
        if eq(a, "merge") {
            merge = true;
        } else if eq(a, "noflush") {
            flush = false;
        } else if eq(a, "nosave") {
            save = false;
        } else {
            return Err(Error::new("ERR DEBUG RELOAD only supports the MERGE, NOFLUSH and NOSAVE options."));
        }
    }
    cx.eng.backup_failed("debug reload executed");
    let now = cx.now;
    let mut image = Vec::new();
    cx.eng.write_rdb(&mut image, now).map_err(|_| Error::new("ERR"))?;
    if save {
        cx.eng.stats.rdb_saves += 1;
        cx.eng.reset_dirty();
        cx.eng.lastsave = now / 1000;
    }
    load_image(cx, &image, flush, merge, "ERR Error trying to load the RDB dump, check server logs.")
}

/// DEBUG LOADAOF: with the AOF on, the dataset reloaded from it (an image of the current data,
/// which the AOF holds); with it off there is nothing to load, after emptying the dataset.
fn loadaof(cx: &mut Cx) -> CmdResult {
    cx.eng.backup_failed("debug loadaof executed");
    let aof = cx.eng.persist.aof != persist::AofState::Off || cx.eng.host.as_ref().is_some_and(|h| h.aof_enabled());
    let mut image = Vec::new();
    if aof {
        cx.eng.write_rdb(&mut image, cx.now).map_err(|_| Error::new("ERR"))?;
    }
    let err = "ERR Error trying to load the AOF files, check server logs.";
    if !aof {
        empty_data(cx);
        return Err(Error::new(err));
    }
    let r = load_image(cx, &image, true, false, err);
    cx.eng.reset_dirty();
    r
}

/// `emptyData(-1, …)`: every database and the function libraries.
fn empty_data(cx: &mut Cx) {
    cx.eng.flush_all();
    cx.eng.scripting.flush_functions();
}

fn load_image(cx: &mut Cx, image: &[u8], flush: bool, merge: bool, err: &'static str) -> CmdResult {
    if flush {
        empty_data(cx);
    }
    cx.changes(0);
    match cx.eng.read_rdb(image, merge, cx.now) {
        Ok(()) => Ok(Reply::Ok),
        // Redis panics on a key it has already.
        Err(crate::rdb::RdbError::Corrupt("duplicate key")) => {
            tracing::error!("DEBUG RELOAD: duplicated key found in RDB file, restarting the database");
            restart(cx, 0)
        }
        Err(e) => {
            tracing::warn!(error = %e, "DEBUG RELOAD");
            Err(Error::new(err))
        }
    }
}

/// DEBUG PROTOCOL type
fn protocol(cx: &mut Cx, name: &[u8]) -> CmdResult {
    let resp3 = cx.client.resp >= 3;
    let t = |s: &str| name.eq_ignore_ascii_case(s.as_bytes());
    Ok(if t("string") {
        Reply::bulk("Hello World")
    } else if t("integer") {
        Reply::Int(12345)
    } else if t("double") {
        // Redis' test value (not an approximation of π).
        Reply::Double(3141.0 / 1000.0)
    } else if t("bignum") {
        Reply::BigNum("1234567999999999999999999999999999999".into())
    } else if t("null") {
        Reply::Nil
    } else if t("array") {
        Reply::Array((0..3).map(Reply::Int).collect())
    } else if t("set") {
        Reply::Set((0..3).map(Reply::Int).collect())
    } else if t("map") {
        Reply::Map((0..3).map(|i| (Reply::Int(i), Reply::Bool(i == 1))).collect())
    } else if t("attrib") {
        let real = Reply::bulk("Some real reply following the attribute");
        if resp3 {
            // An attribute is not a reply of its own: the real one follows.
            let attr = Bytes::from_static(b"|1\r\n$14\r\nkey-popularity\r\n*2\r\n$7\r\nkey:123\r\n:90\r\n");
            Reply::Seq(vec![Reply::Raw(attr), real])
        } else {
            real
        }
    } else if t("push") {
        if !resp3 {
            return Err(Error::new("ERR RESP2 is not supported by this command"));
        }
        Reply::Seq(vec![
            Reply::Push(vec![Reply::bulk("server-cpu-usage"), Reply::Int(42)]),
            Reply::bulk("Some real reply following the push reply"),
        ])
    } else if t("true") {
        Reply::Bool(true)
    } else if t("false") {
        Reply::Bool(false)
    } else if t("verbatim") {
        Reply::Verbatim("txt", Bytes::from_static(b"This is a verbatim\nstring"))
    } else {
        return Err(Error::new("ERR Wrong protocol type name. Please use one of the following: string|integer|double|bignum|null|array|set|map|attrib|push|verbatim|true|false"));
    })
}

/// DEBUG SCRIPT LIST | sha: the EVAL scripts' bodies go to the log.
fn script(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if cx.cfg().get(b"hide-user-data-from-log").first().is_some_and(|(_, v)| v == "yes") {
        return Err(Error::new("ERR DEBUG SCRIPT is disabled when hide-user-data-from-log is enabled"));
    }
    let a = &args[2];
    if eq(a, "list") {
        for (sha, body) in cx.eng.scripting.eval_scripts() {
            tracing::warn!("SCRIPT SHA: {sha}\n{}", String::from_utf8_lossy(body));
        }
    } else if a.len() == 40 {
        let sha = String::from_utf8_lossy(a);
        let Some(body) = cx.eng.scripting.eval_script(&sha) else {
            return Err(Error::new("NOSCRIPT No matching script. Please use EVAL."));
        };
        tracing::warn!("SCRIPT SHA: {sha}\n{}", String::from_utf8_lossy(body));
    } else {
        return Err(syntax(args));
    }
    Ok(Reply::Ok)
}

/// `CLIENT_MEM_USAGE_BUCKET_MIN_LOG` / `MAX_LOG`.
const BUCKET_MIN_LOG: u32 = 15;
const BUCKET_MAX_LOG: u32 = 33;

/// DEBUG CLIENT-EVICTION: the evictable clients' memory by power-of-two bucket.
fn client_eviction(cx: &mut Cx) -> CmdResult {
    if crate::config::client_eviction_limit(cx.cfg()) == 0 {
        return Err(Error::new("ERR maxmemory-clients is disabled."));
    }
    let buckets = (BUCKET_MAX_LOG - BUCKET_MIN_LOG + 1) as usize;
    let mut sums = vec![(0u64, 0u64); buckets];
    for (_, mem, evictable) in cx.eng.client_memory(false) {
        if evictable {
            let bits = u64::BITS - mem.leading_zeros();
            let b = &mut sums[(bits.clamp(BUCKET_MIN_LOG, BUCKET_MAX_LOG) - BUCKET_MIN_LOG) as usize];
            b.0 += mem;
            b.1 += 1;
        }
    }
    let mut s = String::new();
    for (j, (mem, clients)) in sums.iter().enumerate() {
        if j == 0 {
            s += "bucket          0";
        } else {
            s += &format!("bucket {:>10}", 1u64 << (j as u32 - 1 + BUCKET_MIN_LOG));
        }
        if j == buckets - 1 {
            s += "+            : ";
        } else {
            s += &format!(" - {:>10}: ", (1u64 << (j as u32 + BUCKET_MIN_LOG)) - 1);
        }
        s += &format!("tot-mem: {mem:>10}, clients: {clients}\n");
    }
    Ok(Reply::Verbatim("txt", s.into()))
}

// ---- digests ----

fn hex(d: &[u8; 20]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// `xorDigest`: xor in the SHA1 of `data` (order-independent).
fn xor_digest(d: &mut [u8; 20], data: &[u8]) {
    for (a, b) in d.iter_mut().zip(Sha1::digest(data)) {
        *a ^= b;
    }
}

/// `mixDigest`: SHA1(digest xor SHA1(data)) (order-dependent).
fn mix_digest(d: &mut [u8; 20], data: &[u8]) {
    xor_digest(d, data);
    *d = Sha1::digest(&d[..]).into();
}

/// `computeDatasetDigest`: per database (empty ones skipped) its index, then every key and value,
/// logically expired ones included.
pub fn dataset_digest(eng: &Engine) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (j, db) in eng.dbs.iter().enumerate() {
        if db.is_empty() {
            continue;
        }
        mix_digest(&mut out, &(j as u32).to_be_bytes());
        for (k, e) in db.keys.iter() {
            let mut d = [0u8; 20];
            mix_digest(&mut d, k);
            object_digest(&mut d, e);
            xor_digest(&mut out, &d);
        }
    }
    out
}

/// `xorObjectDigest`: the type, the value (unordered collections xor'ed element digests) and
/// whether the key expires.
fn object_digest(d: &mut [u8; 20], e: &Entry) {
    let ty: u32 = match &e.value {
        Value::Str(_) => 0,
        Value::List(_) => 1,
        Value::Set(_) => 2,
        Value::ZSet(_) => 3,
        Value::Hash(_) => 4,
        Value::Stream(_) => 6,
        Value::Array(_) => 7,
    };
    mix_digest(d, &ty.to_be_bytes());
    match &e.value {
        Value::Str(s) => mix_digest(d, s),
        Value::List(l) => l.iter().for_each(|v| mix_digest(d, v)),
        Value::Set(s) => s.iter_members().for_each(|m| xor_digest(d, &m)),
        Value::ZSet(z) => {
            for (m, score) in z.iter() {
                let mut ed = [0u8; 20];
                mix_digest(&mut ed, m);
                mix_digest(&mut ed, redis_proto::fpconv_dtoa(score).as_bytes());
                xor_digest(d, &ed);
            }
        }
        Value::Hash(h) => {
            for (f, v, ttl) in h.iter() {
                let mut ed = [0u8; 20];
                mix_digest(&mut ed, f);
                mix_digest(&mut ed, v);
                if ttl != 0 {
                    xor_digest(&mut ed, b"!!hexpire!!");
                }
                xor_digest(d, &ed);
            }
        }
        Value::Stream(s) => {
            for entry in s.entries.range(StreamId::MIN, StreamId::MAX) {
                mix_digest(d, format!("{}.{}", entry.id.ms, entry.id.seq).as_bytes());
                for (f, v) in entry.fields() {
                    mix_digest(d, f);
                    mix_digest(d, v);
                }
            }
        }
        Value::Array(a) => {
            let mut next = 0;
            for (idx, v) in a.iter() {
                for _ in next..idx {
                    mix_digest(d, b"(null)");
                }
                mix_digest(d, &v.text());
                next = idx + 1;
            }
            for _ in next..a.len() {
                mix_digest(d, b"(null)");
            }
        }
    }
    if e.expire != 0 {
        xor_digest(d, b"!!expire!!");
    }
}
