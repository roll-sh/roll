use crate::{Client, Engine, Reply};
use bytes::Bytes;

fn run(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
    let args: Vec<Bytes> = redis_proto::split_args(line.as_bytes()).expect("quoting").into_iter().map(Bytes::from).collect();
    e.call(c, &args).unwrap_reply()
}

fn bulk(s: &str) -> Reply {
    Reply::bulk(s)
}

fn err(r: &Reply) -> &str {
    match r {
        Reply::Error(e) => e,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// Every line of the compat script runs (no hangs, no crashes). Set ROLL_SCRIPT_TRACE=1 to print
/// the replies.
#[test]
fn compat_script_runs() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../redis-server/tests/compat/scripting.txt");
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    let trace = std::env::var("ROLL_SCRIPT_TRACE").is_ok();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') || line.contains('{') && line.contains("*") {
            continue;
        }
        let line = line.strip_prefix('!').map_or(line, |l| l.split_once(' ').map_or(l, |(_, rest)| rest));
        if trace {
            eprintln!("> {line}");
        }
        let r = run(&mut e, &mut c, line);
        if trace {
            eprintln!("{r:?}");
        }
    }
}

#[test]
fn eval_basics() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    assert_eq!(run(&mut e, &mut c, "EVAL \"return 1\" 0"), Reply::Int(1));
    assert_eq!(run(&mut e, &mut c, "EVAL \"return {KEYS[1], ARGV[1]}\" 1 k a"), Reply::Array(vec![bulk("k"), bulk("a")]));
    assert_eq!(run(&mut e, &mut c, "EVAL \"return redis.call('SET', KEYS[1], ARGV[1])\" 1 k v"), Reply::Ok);
    assert_eq!(run(&mut e, &mut c, "GET k"), bulk("v"));
    assert_eq!(run(&mut e, &mut c, "EVAL \"return 3.99\" 0"), Reply::Int(3));
    assert_eq!(run(&mut e, &mut c, "EVAL \"return nil\" 0"), Reply::Nil);
    let r = run(&mut e, &mut c, "EVAL \"error('boom')\" 0");
    assert!(err(&r).starts_with("ERR user_script:1: boom script: "), "{r:?}");
    let r = run(&mut e, &mut c, "EVAL \"return redis.call('LPUSH', 'k', 'x')\" 0");
    assert!(err(&r).starts_with("WRONGTYPE "), "{r:?}");
}

#[test]
fn script_cache() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    let sha = run(&mut e, &mut c, "SCRIPT LOAD \"return 'x'\"");
    let Reply::Bulk(sha) = sha else { panic!() };
    let sha = String::from_utf8(sha.to_vec()).unwrap();
    assert_eq!(run(&mut e, &mut c, &format!("EVALSHA {sha} 0")), bulk("x"));
    assert_eq!(run(&mut e, &mut c, &format!("EVALSHA {} 0", sha.to_uppercase())), bulk("x"));
    assert_eq!(run(&mut e, &mut c, &format!("SCRIPT EXISTS {sha} nope")), Reply::Array(vec![Reply::Int(1), Reply::Int(0)]));
    assert_eq!(run(&mut e, &mut c, "SCRIPT FLUSH"), Reply::Ok);
    assert!(err(&run(&mut e, &mut c, &format!("EVALSHA {sha} 0"))).starts_with("NOSCRIPT"));
}

#[test]
fn eval_scripts_are_evicted_after_500() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    let loaded = run(&mut e, &mut c, "SCRIPT LOAD \"return 'kept'\"");
    for i in 0..600 {
        run(&mut e, &mut c, &format!("EVAL \"return {i}\" 0"));
    }
    assert_eq!(e.scripting.cached_scripts(), 501);
    assert_eq!(e.stats.evicted_scripts, 100);
    let Reply::Bulk(sha) = loaded else { panic!() };
    let sha = String::from_utf8(sha.to_vec()).unwrap();
    assert_eq!(run(&mut e, &mut c, &format!("EVALSHA {sha} 0")), bulk("kept"));
}

#[test]
fn script_writes_replicate_as_multi_exec() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    e.take_effects();
    run(&mut e, &mut c, "EVAL \"redis.call('SET', 'a', 1) redis.call('INCR', 'a') return 1\" 0");
    let effects: Vec<Vec<Bytes>> = e.take_effects().into_iter().map(|f| f.args).collect();
    let names: Vec<String> = effects.iter().map(|a| String::from_utf8_lossy(&a[0]).to_lowercase()).collect();
    assert_eq!(names, ["multi", "set", "incr", "exec"]);

    run(&mut e, &mut c, "EVAL \"redis.call('SET', 'b', 1) return 1\" 0");
    let effects = e.take_effects();
    assert_eq!(effects.len(), 1, "a single write isn't wrapped");

    run(&mut e, &mut c, "EVAL \"redis.set_repl(redis.REPL_NONE) redis.call('SET', 'c', 1) return 1\" 0");
    assert!(e.take_effects().is_empty());
    assert_eq!(run(&mut e, &mut c, "GET c"), bulk("1"));

    run(&mut e, &mut c, "EVAL \"return redis.call('GET', 'a')\" 0");
    assert!(e.take_effects().is_empty(), "reads don't replicate");
}

#[test]
fn read_only_scripts() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    let r = run(&mut e, &mut c, "EVAL_RO \"return redis.call('SET', 'k', 'v')\" 0");
    assert!(err(&r).starts_with("ERR Write commands are not allowed from read-only scripts."), "{r:?}");
    let r = run(&mut e, &mut c, "EVAL \"#!lua flags=no-writes\nreturn redis.call('PUBLISH', 'c', 'm')\" 0");
    assert!(err(&r).starts_with("ERR Write commands are not allowed"), "{r:?}");
    let r = run(&mut e, &mut c, "EVAL_RO \"#!lua\nreturn 1\" 0");
    assert_eq!(err(&r), "ERR Can not execute a script with write flag using *_ro command.");
    assert_eq!(run(&mut e, &mut c, "EVAL_RO \"#!lua flags=no-writes\nreturn 1\" 0"), Reply::Int(1));
}

#[test]
fn replica_and_oom_flags() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    e.replica = true;
    assert_eq!(err(&run(&mut e, &mut c, "EVAL \"#!lua\nreturn 1\" 0")), "READONLY You can't write against a read only replica.");
    assert_eq!(run(&mut e, &mut c, "EVAL \"#!lua flags=no-writes\nreturn 1\" 0"), Reply::Int(1));
    let r = run(&mut e, &mut c, "EVAL \"return redis.call('SET', 'k', 'v')\" 0");
    assert!(err(&r).starts_with("READONLY"), "{r:?}");
    e.replica = false;

    run(&mut e, &mut c, "SET filler xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    run(&mut e, &mut c, "CONFIG SET maxmemory 1");
    assert!(err(&run(&mut e, &mut c, "EVAL \"#!lua\nreturn 1\" 0")).starts_with("OOM"));
    assert_eq!(run(&mut e, &mut c, "EVAL \"#!lua flags=allow-oom\nreturn 1\" 0"), Reply::Int(1));
    let r = run(&mut e, &mut c, "EVAL \"return redis.call('SET', 'k', 'v')\" 0");
    assert!(err(&r).starts_with("OOM"), "{r:?}");
    // Reads and DEL still work.
    assert_eq!(run(&mut e, &mut c, "EVAL \"return redis.call('DEL', 'filler')\" 0"), Reply::Int(1));
    run(&mut e, &mut c, "CONFIG SET maxmemory 0");
}

#[test]
fn sandbox() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    let r = run(&mut e, &mut c, "EVAL \"x = 1\" 0");
    assert!(err(&r).contains("Attempt to modify a readonly table"), "{r:?}");
    let r = run(&mut e, &mut c, "EVAL \"return nosuch\" 0");
    assert!(err(&r).contains("Script attempted to access nonexistent global variable 'nosuch'"), "{r:?}");
    let r = run(&mut e, &mut c, "EVAL \"return type(io)\" 0");
    assert!(err(&r).contains("nonexistent global variable 'io'"), "{r:?}");
}

#[test]
fn functions() {
    let mut e = Engine::standalone();
    let mut c = Client::new(1);
    let lib = "FUNCTION LOAD \"#!lua name=mylib\nredis.register_function('myfunc', function(keys, args) return {keys[1], args[1]} end)\nredis.register_function{function_name='ro', callback=function() return redis.call('GET', 'k') end, flags={'no-writes'}}\"";
    assert_eq!(run(&mut e, &mut c, lib), bulk("mylib"));
    assert_eq!(run(&mut e, &mut c, "FCALL myfunc 1 a b"), Reply::Array(vec![bulk("a"), bulk("b")]));
    assert_eq!(run(&mut e, &mut c, "FCALL MYFUNC 1 a b"), Reply::Array(vec![bulk("a"), bulk("b")]));
    assert_eq!(run(&mut e, &mut c, "FCALL_RO ro 0"), Reply::Nil);
    assert_eq!(err(&run(&mut e, &mut c, "FCALL_RO myfunc 0")), "ERR Can not execute a script with write flag using *_ro command.");
    assert_eq!(err(&run(&mut e, &mut c, "FCALL nope 0")), "ERR Function not found");
    assert_eq!(err(&run(&mut e, &mut c, lib)), "ERR Library 'mylib' already exists");
    assert_eq!(run(&mut e, &mut c, &lib.replace("LOAD", "LOAD REPLACE")), bulk("mylib"));
    assert_eq!(e.scripting.functions_count(), 2);
    let effects = e.take_effects();
    assert!(effects.iter().any(|f| String::from_utf8_lossy(&f.args[0]).eq_ignore_ascii_case("function")), "FUNCTION LOAD replicates");
    assert_eq!(run(&mut e, &mut c, "FUNCTION DELETE mylib"), Reply::Ok);
    assert_eq!(err(&run(&mut e, &mut c, "FCALL myfunc 0")), "ERR Function not found");
}
