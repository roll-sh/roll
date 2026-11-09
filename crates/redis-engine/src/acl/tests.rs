use super::*;
use crate::commands::{lookup, lookup_sub};
use crate::{Engine, Outcome};

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

fn args(line: &str) -> Vec<Bytes> {
    redis_proto::split_args(line.as_bytes()).unwrap().into_iter().map(Bytes::from).collect()
}

fn rules(ops: &str) -> Rules {
    let mut r = Rules::new();
    for op in merge_selectors(&args(ops)).unwrap() {
        r.apply(&op).unwrap_or_else(|e| panic!("{}: {e:?}", String::from_utf8_lossy(&op)));
    }
    r
}

fn describe(r: &Rules) -> String {
    String::from_utf8(r.describe()).unwrap()
}

/// Resolve a command line like the engine does and check it.
fn check(r: &Rules, line: &str) -> Result<(), Denied> {
    let a = args(line);
    let (cmd, _) = Engine::resolve(&a).unwrap_or_else(|e| panic!("{line}: {e:?}"));
    r.check(cmd, &a)
}

fn denied(reason: Reason, pos: usize) -> Result<(), Denied> {
    Err(Denied { reason, pos })
}

#[test]
fn describe_new_and_default_users() {
    assert_eq!(describe(&Rules::new()), "off sanitize-payload resetchannels -@all");
    assert_eq!(describe(&Rules::default_user()), "on nopass sanitize-payload ~* &* +@all");
}

#[test]
fn describe_keeps_rule_order_and_normalises() {
    let r = rules("on >p1 >p1 %R~r* %W~w* ~a* %R~a* &ch* +get +@string -set +config|get +select|0 (~x* +@keyspace)");
    assert_eq!(
        describe(&r),
        format!(
            "on sanitize-payload #{} %R~r* %W~w* ~a* resetchannels &ch* -@all +get +@string -set +config|get +select|0 (~x* resetchannels -@all +@keyspace)",
            hash_password(b"p1")
        )
    );
    // A later rule for the same command replaces the earlier one; a command rule drops its
    // subcommand rules; +@all / -@all reset the list.
    let r = rules("+@all -@string +get -get +set");
    assert_eq!(describe(&r), "off sanitize-payload resetchannels +@all -@string -get +set");
    let r = rules("+config|get +config|set -config");
    assert_eq!(describe(&r), "off sanitize-payload resetchannels -@all -config");
    let r = rules("nocommands +get allcommands");
    assert_eq!(describe(&r), "off sanitize-payload resetchannels +@all");
    let r = rules("-@all +GET +Config|Get +@STRING +SELECT|1 +select|2 +select|1");
    assert_eq!(describe(&r), "off sanitize-payload resetchannels -@all +get +config|get +@string +select|2 +select|1");
    // Pattern flags merge; `%RW` with no `~` is an empty pattern.
    let r = rules("%R~k %W~k %RW &");
    assert_eq!(describe(&r), "off sanitize-payload ~k ~ resetchannels & -@all");
}

#[test]
fn describe_round_trips() {
    for ops in [
        "on >a >b ~k* %R~r %W~w &c* +@all -@dangerous +flushdb -config|set",
        "off nopass skip-sanitize-payload allkeys allchannels -@all +@read +@write -@keyspace +del",
        "on +get +select|0 (~x +set) (&ch +publish %R~y)",
    ] {
        let r = rules(ops);
        let text = describe(&r);
        let again = rules(&text);
        assert_eq!(describe(&again), text, "{ops}");
        for line in ["GET k", "SET x v", "DEL k", "SELECT 0", "SELECT 1", "PUBLISH ch m", "FLUSHDB", "CONFIG SET a b", "GET r"] {
            assert_eq!(check(&r, line), check(&again, line), "{ops}: {line}");
        }
    }
}

#[test]
fn rule_errors() {
    let err = |ops: &str| {
        let mut r = Rules::new();
        let mut last = Ok(());
        for op in args(ops) {
            last = r.apply(&op);
            if last.is_err() {
                break;
            }
        }
        last.unwrap_err()
    };
    assert_eq!(err("bogus"), RuleError::Syntax);
    assert_eq!(err("+nosuch"), RuleError::UnknownCommand);
    assert_eq!(err("-nosuch"), RuleError::UnknownCommand);
    assert_eq!(err("+@nosuch"), RuleError::UnknownCommand);
    assert_eq!(err("+"), RuleError::UnknownCommand);
    assert_eq!(err("allkeys ~a"), RuleError::KeyAfterAll);
    assert_eq!(err("allkeys %R~a"), RuleError::KeyAfterAll);
    assert_eq!(err("allchannels &a"), RuleError::ChannelAfterAll);
    assert_eq!(err("+config|get|x"), RuleError::FirstArgOfSub);
    assert_eq!(err("+get|"), RuleError::Syntax);
    assert_eq!(err("+config|nosuch"), RuleError::UnknownCommand);
    assert_eq!(err("-select|0"), RuleError::UnknownCommand);
    assert_eq!(err("%X~a"), RuleError::Syntax);
    assert_eq!(err("%RR~a"), RuleError::Syntax);
    assert_eq!(err("%~a"), RuleError::Syntax);
    assert_eq!(err("<nope"), RuleError::NoSuchPassword);
    assert_eq!(err("#abc"), RuleError::BadHash);
    assert_eq!(err(&format!("#{}", "A".repeat(64))), RuleError::BadHash);
    assert_eq!(err("(on)"), RuleError::Syntax);
    assert_eq!(err("\"~a b\""), RuleError::Syntax);
}

#[test]
fn set_user_is_atomic_and_reports_redis_errors() {
    let mut acl = Acl::new();
    let e = acl.set_user(b"bob", &args("on +get badrule")).unwrap_err();
    assert_eq!(e, "Error in ACL SETUSER modifier 'badrule': Syntax error");
    assert!(acl.user(b"bob").is_none());
    let e = acl.set_user(b"bob", &args("(~x +get")).unwrap_err();
    assert_eq!(e, "Unmatched parenthesis in acl selector starting at '(~x'.");
    acl.set_user(b"bob", &args("on ( ~x +get )")).unwrap();
    assert_eq!(describe(&acl.user(b"bob").unwrap().rules()), "on sanitize-payload resetchannels -@all (~x resetchannels -@all +get)");
    let e = acl.set_user(b"bob", &args("off (~y +nosuch)")).unwrap_err();
    assert_eq!(e, "Error in ACL SETUSER modifier '(~y +nosuch)': Unknown command or category name in ACL");
    assert!(acl.user(b"bob").unwrap().enabled());
    assert!(!valid_username(b"a b"));
    assert!(!valid_username(b"a\0b"));
    assert_eq!(acl.del_users(&[b("default")]), Err("ERR The 'default' user cannot be removed"));
    assert_eq!(acl.del_users(&[b("bob"), b("nosuch"), b("bob")]), Ok(1));
}

#[test]
fn command_and_subcommand_permissions() {
    let r = rules("+get +config|get +select|0 +client -client|kill +ping");
    assert_eq!(check(&r, "GET k"), denied(Reason::Key, 1)); // no key patterns
    let r = rules("allkeys +get +config|get +select|0 +client -client|kill");
    assert_eq!(check(&r, "GET k"), Ok(()));
    assert_eq!(check(&r, "SET k v"), denied(Reason::Command, 0));
    assert_eq!(check(&r, "CONFIG GET maxmemory"), Ok(()));
    assert_eq!(check(&r, "CONFIG SET maxmemory 0"), denied(Reason::Command, 0));
    assert_eq!(check(&r, "SELECT 0"), Ok(()));
    assert_eq!(check(&r, "SELECT 1"), denied(Reason::Command, 0));
    assert_eq!(check(&r, "CLIENT ID"), Ok(()));
    assert_eq!(check(&r, "CLIENT KILL 1.2.3.4:5"), denied(Reason::Command, 0));
    // Commands flagged no-auth are always allowed.
    assert_eq!(check(&r, "AUTH x"), Ok(()));
    assert_eq!(check(&r, "HELLO"), Ok(()));
    let r = rules("allkeys +@all -@dangerous");
    assert_eq!(check(&r, "GET k"), Ok(()));
    assert_eq!(check(&r, "FLUSHALL"), denied(Reason::Command, 0));
    assert_eq!(check(&r, "CONFIG SET a b"), denied(Reason::Command, 0));
    assert_eq!(check(&r, "CONFIG GET a"), denied(Reason::Command, 0)); // config|get is @admin
}

#[test]
fn key_permissions_follow_key_specs() {
    let r = rules("+@all ~any* %R~r* %W~w*");
    assert_eq!(check(&r, "GET r1"), Ok(()));
    assert_eq!(check(&r, "GET w1"), denied(Reason::Key, 1));
    assert_eq!(check(&r, "SET w1 v"), Ok(())); // SET without GET only writes
    assert_eq!(check(&r, "SET w1 v EX 10"), Ok(()));
    assert_eq!(check(&r, "SET w1 v get"), denied(Reason::Key, 1));
    assert_eq!(check(&r, "SET r1 v"), denied(Reason::Key, 1));
    assert_eq!(check(&r, "INCR w1"), denied(Reason::Key, 1)); // RW ACCESS UPDATE
    assert_eq!(check(&r, "APPEND w1 x"), Ok(())); // RW INSERT
    assert_eq!(check(&r, "EXISTS r1 w1 any1"), Ok(())); // RO without ACCESS: any pattern
    assert_eq!(check(&r, "EXISTS nope"), denied(Reason::Key, 1));
    assert_eq!(check(&r, "MSET any1 a w1 b r1 c"), denied(Reason::Key, 5));
    assert_eq!(check(&r, "MGET r1 any2 w1"), denied(Reason::Key, 3));
    assert_eq!(check(&r, "RENAME any1 w1"), Ok(())); // source RW+DELETE, destination OW
    assert_eq!(check(&r, "RENAME w1 any1"), denied(Reason::Key, 1));
    assert_eq!(check(&r, "COPY r1 w1"), Ok(()));
    assert_eq!(check(&r, "DEL w1"), Ok(()));
    assert_eq!(check(&r, "DEL r1"), denied(Reason::Key, 1));
    assert_eq!(check(&r, "GET Any1"), denied(Reason::Key, 1)); // case-sensitive
}

#[test]
fn channel_permissions() {
    let r = rules("allkeys +@all resetchannels &news.* &chan");
    assert_eq!(check(&r, "PUBLISH chan m"), Ok(()));
    assert_eq!(check(&r, "PUBLISH news.x m"), Ok(()));
    assert_eq!(check(&r, "PUBLISH other m"), denied(Reason::Channel, 1));
    assert_eq!(check(&r, "SUBSCRIBE chan news.a other"), denied(Reason::Channel, 3));
    assert_eq!(check(&r, "PSUBSCRIBE news.*"), Ok(())); // literal match
    assert_eq!(check(&r, "PSUBSCRIBE news.a"), denied(Reason::Channel, 1));
    assert_eq!(check(&r, "SSUBSCRIBE chan"), Ok(()));
    assert_eq!(check(&r, "SPUBLISH other m"), denied(Reason::Channel, 1));
    assert_eq!(check(&r, "UNSUBSCRIBE other"), Ok(()));
}

#[test]
fn selectors_pick_the_most_specific_error() {
    let r = rules("~a* +get (~b* +set) (&ch* +publish)");
    assert_eq!(check(&r, "GET a1"), Ok(()));
    assert_eq!(check(&r, "SET b1 v"), Ok(()));
    assert_eq!(check(&r, "SET a1 v"), denied(Reason::Key, 1));
    assert_eq!(check(&r, "PUBLISH x m"), denied(Reason::Channel, 1));
    assert_eq!(check(&r, "DEL a1"), denied(Reason::Command, 0));
}

#[test]
fn categories_match_redis() {
    let cats = |name: &str| categories_of(lookup(name.as_bytes()).unwrap());
    assert_eq!(cats("get"), ["read", "string", "fast"]);
    assert_eq!(cats("set"), ["write", "string", "slow"]);
    assert_eq!(cats("del"), ["keyspace", "write", "slow"]);
    assert_eq!(cats("flushall"), ["keyspace", "write", "slow", "dangerous"]);
    assert_eq!(cats("acl"), ["slow"]);
    let config = lookup(b"config").unwrap();
    assert_eq!(categories_of(lookup_sub(config, b"get").unwrap()), ["admin", "slow", "dangerous"]);
    assert_eq!(full_name(lookup_sub(config, b"get").unwrap()), Some("config|get"));
    let string = commands_in_category(category_bit(b"STRING").unwrap());
    assert!(string.contains(&"get") && string.contains(&"set") && !string.contains(&"del"));
    let json = commands_in_category(category_bit(b"json").unwrap());
    assert!(json.contains(&"json.get") && json.contains(&"json.set") && json.len() == 25);
    assert_eq!(CATEGORIES.len(), 30);
}

#[test]
fn authentication() {
    let mut acl = Acl::new();
    assert_eq!(acl.authenticate(None, b"x").unwrap_err(), AuthFailure::NoPasswordConfigured);
    assert_eq!(acl.authenticate(Some(b"default"), b"anything").unwrap().name, "default");
    acl.set_user(b"alice", &args("on >pw")).unwrap();
    assert_eq!(acl.authenticate(Some(b"alice"), b"pw").unwrap().name, "alice");
    assert_eq!(acl.authenticate(Some(b"alice"), b"bad").unwrap_err(), AuthFailure::WrongPass);
    assert_eq!(acl.authenticate(Some(b"nosuch"), b"pw").unwrap_err(), AuthFailure::WrongPass);
    acl.set_user(b"alice", &args("off")).unwrap();
    assert_eq!(acl.authenticate(Some(b"alice"), b"pw").unwrap_err(), AuthFailure::WrongPass);
    acl.set_default_password(Some(b"secret"));
    assert!(acl.authenticate(None, b"secret").is_ok());
    assert_eq!(acl.authenticate(None, b"x").unwrap_err(), AuthFailure::WrongPass);
    assert_eq!(
        AuthFailure::WrongPass.to_string(),
        "WRONGPASS invalid username-password pair or user is disabled."
    );
    assert_eq!(hash_password(b"pass1"), "e6c3da5b206634d7f3f3586d747ffdb36b5c675757b380c6a5fe5c570c714349");
}

#[test]
fn log_groups_similar_entries() {
    let mut log = AclLog::default();
    let entry = |object: &str, ctime: i64| LogEntry {
        count: 1,
        reason: LogReason::Key,
        context: Context::Toplevel,
        object: b(object),
        username: b("u"),
        ctime,
        client_info: String::new(),
        entry_id: 0,
        created: ctime,
    };
    log.add(entry("k", 1000));
    log.add(entry("k", 2000));
    log.add(entry("other", 2000));
    log.add(entry("k", 2000 + LOG_GROUPING_MS + 1));
    let e: Vec<_> = log.entries.iter().map(|e| (e.object.clone(), e.count, e.entry_id)).collect();
    assert_eq!(e, [(b("k"), 1, 2), (b("other"), 1, 1), (b("k"), 2, 0)]);
    for i in 0..200 {
        log.add(entry(&format!("k{i}"), 0));
    }
    assert_eq!(log.entries.len(), LOG_MAX_LEN);
    assert_eq!(log.entries[0].entry_id, 202);
    assert_eq!(log.denied[2], 204);
}

fn run(eng: &mut Engine, c: &mut Client, line: &str) -> Reply {
    match eng.call(c, &args(line)) {
        Outcome::Reply(r) => r,
        Outcome::Blocked => panic!("blocked"),
    }
}

#[test]
fn engine_checks_and_logs() {
    let mut eng = Engine::standalone();
    let mut admin = Client::new(1);
    assert_eq!(run(&mut eng, &mut admin, "ACL WHOAMI"), Reply::bulk("default"));
    assert!(Arc::ptr_eq(&admin.user, &eng.acl.read().default_user()));
    assert_eq!(run(&mut eng, &mut admin, "ACL SETUSER alice on >pw ~k* +get +multi +exec +set"), Reply::Ok);
    let mut c = Client::new(2);
    c.user = eng.acl.read().authenticate(Some(b"alice"), b"pw").unwrap();
    assert_eq!(run(&mut eng, &mut c, "GET k1"), Reply::Nil);
    assert_eq!(run(&mut eng, &mut c, "GET x"), Reply::error("NOPERM No permissions to access a key"));
    assert_eq!(run(&mut eng, &mut c, "DEL k1"), Reply::error("NOPERM User alice has no permissions to run the 'del' command"));
    assert_eq!(
        run(&mut eng, &mut c, "CONFIG GET x"),
        Reply::error("NOPERM User alice has no permissions to run the 'config|get' command")
    );
    // Rule changes apply to the connected client.
    run(&mut eng, &mut admin, "ACL SETUSER alice +del");
    assert_eq!(run(&mut eng, &mut c, "DEL k1"), Reply::Int(0));
    // Re-checked at EXEC.
    assert_eq!(run(&mut eng, &mut c, "MULTI"), Reply::Ok);
    assert_eq!(run(&mut eng, &mut c, "SET k1 v"), Reply::status("QUEUED"));
    run(&mut eng, &mut admin, "ACL SETUSER alice -set");
    match run(&mut eng, &mut c, "EXEC") {
        Reply::Array(r) => assert!(matches!(&r[0], Reply::Error(e) if e.starts_with("NOPERM ACLs rules changed"))),
        other => panic!("{other:?}"),
    }
    // Denied EXEC aborts the transaction.
    run(&mut eng, &mut admin, "ACL SETUSER alice -exec");
    assert_eq!(run(&mut eng, &mut c, "MULTI"), Reply::Ok);
    assert_eq!(
        run(&mut eng, &mut c, "EXEC"),
        Reply::error("EXECABORT Transaction discarded because of: NOPERM User alice has no permissions to run the 'exec' command")
    );
    assert!(c.multi.is_none());
    let log = eng.acl.read().log.lock().entries.iter().map(|e| (e.reason, e.context, e.object.clone())).collect::<Vec<_>>();
    assert_eq!(
        log,
        [
            (LogReason::Command, Context::Multi, b("exec")),
            (LogReason::Command, Context::Multi, b("set")),
            (LogReason::Command, Context::Toplevel, b("config|get")),
            (LogReason::Command, Context::Toplevel, b("del")),
            (LogReason::Key, Context::Toplevel, b("x")),
        ]
    );
    // Deleting the user flags its clients for disconnection.
    assert!(!must_disconnect(&c));
    assert_eq!(run(&mut eng, &mut admin, "ACL DELUSER alice"), Reply::Int(1));
    assert!(must_disconnect(&c));
}

#[test]
fn dryrun_and_getuser_replies() {
    let mut eng = Engine::standalone();
    let mut c = Client::new(1);
    run(&mut eng, &mut c, "ACL SETUSER bob on nopass ~k* &ch +get +publish");
    assert_eq!(run(&mut eng, &mut c, "ACL DRYRUN bob get k1"), Reply::Ok);
    assert_eq!(run(&mut eng, &mut c, "ACL DRYRUN bob get x"), Reply::bulk("User bob has no permissions to access the 'x' key"));
    assert_eq!(run(&mut eng, &mut c, "ACL DRYRUN bob publish x m"), Reply::bulk("User bob has no permissions to access the 'x' channel"));
    assert_eq!(run(&mut eng, &mut c, "ACL DRYRUN bob set k v"), Reply::bulk("User bob has no permissions to run the 'set' command"));
    assert_eq!(run(&mut eng, &mut c, "ACL DRYRUN bob config"), Reply::error("ERR wrong number of arguments for 'config' command"));
    assert_eq!(run(&mut eng, &mut c, "ACL DRYRUN bob config x"), Reply::error("ERR Command 'config' not found"));
    assert_eq!(run(&mut eng, &mut c, "ACL DRYRUN nobody get k"), Reply::error("ERR User 'nobody' not found"));
    assert_eq!(
        run(&mut eng, &mut c, "ACL GETUSER bob"),
        Reply::Map(vec![
            (Reply::bulk("flags"), Reply::Set(vec![Reply::bulk("on"), Reply::bulk("nopass"), Reply::bulk("sanitize-payload")])),
            (Reply::bulk("passwords"), Reply::Array(vec![])),
            (Reply::bulk("commands"), Reply::bulk("-@all +get +publish")),
            (Reply::bulk("keys"), Reply::bulk("~k*")),
            (Reply::bulk("channels"), Reply::bulk("&ch")),
            (Reply::bulk("selectors"), Reply::Array(vec![])),
        ])
    );
    match run(&mut eng, &mut c, "ACL GENPASS 5") {
        Reply::Bulk(p) => assert_eq!(p.len(), 2),
        other => panic!("{other:?}"),
    }
    match run(&mut eng, &mut c, "ACL GENPASS") {
        Reply::Bulk(p) => assert!(p.len() == 64 && p.iter().all(|c| c.is_ascii_hexdigit())),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unrestricted_users_take_the_fast_path() {
    let acl = Acl::new();
    assert!(acl.default_user().open.load(Ordering::Relaxed));
    let mut acl = acl;
    acl.set_user(b"x", &args("on nopass ~* &* +@all")).unwrap();
    assert!(acl.user(b"x").unwrap().open.load(Ordering::Relaxed));
    acl.set_user(b"x", &args("-flushall")).unwrap();
    assert!(!acl.user(b"x").unwrap().open.load(Ordering::Relaxed));
}
