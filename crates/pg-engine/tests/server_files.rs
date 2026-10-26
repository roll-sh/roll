//! Server-side files (`--allow-server-files`, `EngineOptions::server_files`): refused for every
//! role by default; when allowed, Postgres's functions and COPY work for the roles Postgres
//! lets in, confined to the directory (relative paths resolve there, absolute ones and `..`
//! must stay inside, and so must symbolic links).

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn open(data: &Path, files: Option<&Path>) -> Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: data.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: files.map(Path::to_path_buf),
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
    })
    .unwrap()
}

/// The statement must return one `true`.
fn check(s: &mut Session, sql: &str) {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let rows: Vec<String> = r.last().unwrap().rows.iter().map(|r| format!("{r:?}")).collect();
    assert_eq!(rows, vec!["[Bool(true)]"], "{sql}");
}

fn run(s: &mut Session, sql: &str) {
    s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// The error: SQLSTATE and message (and the hint, if any, after " HINT: ").
fn err(s: &mut Session, sql: &str) -> String {
    match s.query(sql) {
        Ok(_) => panic!("{sql}: no error"),
        Err(e) => match e.hint {
            Some(h) => format!("{} {} HINT: {h}", e.code, e.message),
            None => format!("{} {}", e.code, e.message),
        },
    }
}

/// Postgres's quoting of a string literal.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[test]
fn refused_by_default() {
    let data = tempfile::tempdir().unwrap();
    let engine = open(data.path(), None);
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, "CREATE TABLE t (a int)");
    let denied = |f: &str| format!("42501 permission denied for function {f}");
    assert_eq!(err(&mut s, "SELECT pg_read_file('x')"), denied("pg_read_file"));
    // The length is checked before the file.
    assert_eq!(
        err(&mut s, "SELECT pg_read_file('x', 0, -1)"),
        "22023 requested length cannot be negative"
    );
    assert_eq!(err(&mut s, "SELECT pg_read_binary_file('x')"), denied("pg_read_binary_file"));
    assert_eq!(err(&mut s, "SELECT pg_stat_file('x')"), denied("pg_stat_file"));
    assert_eq!(err(&mut s, "SELECT pg_ls_dir('.')"), denied("pg_ls_dir"));
    assert_eq!(err(&mut s, "SELECT lo_import('/x')"), denied("lo_import"));
    run(&mut s, "SELECT lo_create(4242)");
    assert_eq!(err(&mut s, "SELECT lo_export(4242, '/x')"), denied("lo_export"));
    let hint = "HINT: Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.";
    assert_eq!(
        err(&mut s, "COPY t FROM '/x'"),
        format!("42501 permission denied to COPY from a file {hint}")
    );
    assert_eq!(
        err(&mut s, "COPY t TO '/x'"),
        format!("42501 permission denied to COPY to a file {hint}")
    );
    assert_eq!(
        err(&mut s, "COPY t TO PROGRAM 'cat'"),
        format!("42501 permission denied to COPY to or from an external program {hint}")
    );
    engine.shutdown();
}

#[test]
fn confined_to_the_directory() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = files.path();
    std::fs::write(root.join("hello.txt"), "hello\n").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/a.txt"), "a").unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link_out")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("dir_out")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("new.txt"), root.join("dangling")).unwrap();
    std::os::unix::fs::symlink(root.join("hello.txt"), root.join("link_in")).unwrap();
    let engine = open(data.path(), Some(root));
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    let abs = |p: &str| lit(&format!("{}/{p}", root.display()));
    let out = |p: &str| lit(&format!("{}/{p}", outside.path().display()));

    // Reading, relative to the directory or by an absolute path inside it.
    check(&mut s, "SELECT pg_read_file('hello.txt') = E'hello\\n'");
    check(&mut s, &format!("SELECT pg_read_file({}) = E'hello\\n'", abs("hello.txt")));
    check(&mut s, "SELECT pg_read_file('hello.txt', 1, 3) = 'ell'");
    check(&mut s, "SELECT pg_read_file('hello.txt', -3, 10) = E'lo\\n'");
    check(&mut s, "SELECT pg_read_file('./sub/../hello.txt') = E'hello\\n'");
    check(&mut s, "SELECT pg_read_file('link_in') = E'hello\\n'");
    check(&mut s, "SELECT pg_read_binary_file('sub/a.txt') = '\\x61'::bytea");
    check(&mut s, "SELECT pg_read_file('missing', true) IS NULL");
    assert_eq!(
        err(&mut s, "SELECT pg_read_file('./missing')"),
        "58P01 could not open file \"missing\" for reading: No such file or directory"
    );
    check(&mut s, "SELECT size = 6 AND NOT isdir AND creation IS NULL FROM pg_stat_file('hello.txt')");
    check(&mut s, "SELECT isdir FROM pg_stat_file('sub/')");
    check(&mut s, "SELECT pg_stat_file('missing', true) IS NULL");
    check(&mut s, "SELECT array_agg(f ORDER BY f) = '{a.txt}' FROM pg_ls_dir('sub') f");
    check(
        &mut s,
        "SELECT count(*) = 2 FROM pg_ls_dir('.', false, true) f WHERE f IN ('.', '..')",
    );
    check(&mut s, "SELECT count(*) = 0 FROM pg_ls_dir('missing', true, false)");
    assert_eq!(
        err(&mut s, "SELECT pg_ls_dir('missing')"),
        "58P01 could not open directory \"missing\": No such file or directory"
    );

    // Outside the directory: Postgres's errors for a disallowed path.
    let absolute = "42501 absolute path not allowed";
    let below = "42501 path must be in or below the data directory";
    assert_eq!(err(&mut s, &format!("SELECT pg_read_file({})", out("secret.txt"))), absolute);
    assert_eq!(err(&mut s, "SELECT pg_read_file('/etc/hosts')"), absolute);
    assert_eq!(err(&mut s, &format!("SELECT pg_read_file({})", abs("../x"))), absolute);
    assert_eq!(err(&mut s, "SELECT pg_read_file('../x')"), below);
    assert_eq!(err(&mut s, "SELECT pg_read_file('sub/../../x')"), below);
    assert_eq!(err(&mut s, "SELECT pg_stat_file('..')"), below);
    assert_eq!(err(&mut s, "SELECT pg_ls_dir('..')"), below);
    // Symbolic links out of it, too (which Postgres would follow).
    assert_eq!(err(&mut s, "SELECT pg_read_file('link_out')"), below);
    assert_eq!(err(&mut s, "SELECT pg_read_file('dir_out/secret.txt')"), below);
    assert_eq!(err(&mut s, "SELECT pg_ls_dir('dir_out')"), below);
    assert_eq!(err(&mut s, &format!("SELECT pg_read_file({})", abs("link_out"))), absolute);

    // COPY to and from a file.
    run(&mut s, "CREATE TABLE t (a int, b text)");
    run(&mut s, "INSERT INTO t VALUES (1, 'one'), (2, NULL)");
    assert_eq!(
        err(&mut s, "COPY t TO 'rel.txt'"),
        "42602 relative path not allowed for COPY to file"
    );
    let r = s.query(&format!("COPY t TO {}", abs("t.txt"))).unwrap();
    assert_eq!(r.last().unwrap().tag, "COPY 2");
    assert_eq!(std::fs::read_to_string(root.join("t.txt")).unwrap(), "1\tone\n2\t\\N\n");
    let r = s.query(&format!("COPY (SELECT a FROM t) TO {} (FORMAT csv, HEADER)", abs("sub/t.csv"))).unwrap();
    assert_eq!(r.last().unwrap().tag, "COPY 2");
    assert_eq!(std::fs::read_to_string(root.join("sub/t.csv")).unwrap(), "a\n1\n2\n");
    run(&mut s, "CREATE TABLE t2 (LIKE t)");
    let r = s.query(&format!("COPY t2 FROM {}", abs("t.txt"))).unwrap();
    assert_eq!(r.last().unwrap().tag, "COPY 2");
    // A relative path reads in the directory.
    let r = s.query("COPY t2 FROM 't.txt' WHERE a > 1").unwrap();
    assert_eq!(r.last().unwrap().tag, "COPY 1");
    check(&mut s, "SELECT count(*) = 3 AND count(b) = 1 FROM t2");
    let hint = "HINT: COPY FROM instructs the PostgreSQL server process to read a file. You may want a client-side facility such as psql's \\copy.";
    assert_eq!(
        err(&mut s, &format!("COPY t2 FROM {}", abs("missing"))),
        format!(
            "58P01 could not open file \"{}/missing\" for reading: No such file or directory {hint}",
            root.display()
        )
    );
    assert_eq!(
        err(&mut s, &format!("COPY t2 FROM {}", abs("sub"))),
        format!("42809 \"{}/sub\" is a directory", root.display())
    );
    let hint = "HINT: COPY TO instructs the PostgreSQL server process to write a file. You may want a client-side facility such as psql's \\copy.";
    assert_eq!(
        err(&mut s, &format!("COPY t TO {}", abs("nodir/x"))),
        format!(
            "58P01 could not open file \"{}/nodir/x\" for writing: No such file or directory {hint}",
            root.display()
        )
    );
    assert_eq!(err(&mut s, &format!("COPY t TO {}", out("x.txt"))), absolute);
    assert_eq!(err(&mut s, &format!("COPY t2 FROM {}", out("secret.txt"))), absolute);
    assert_eq!(err(&mut s, "COPY t2 FROM '../secret.txt'"), below);
    // Writes through a link out of the directory are refused, the target left alone.
    assert_eq!(err(&mut s, &format!("COPY t TO {}", abs("link_out"))), absolute);
    assert_eq!(err(&mut s, &format!("COPY t TO {}", abs("dangling"))), absolute);
    assert_eq!(err(&mut s, &format!("COPY t TO {}", abs("dir_out/y.txt"))), absolute);
    assert_eq!(std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(), "secret");
    assert!(!outside.path().join("new.txt").exists());
    assert!(!outside.path().join("y.txt").exists());
    // Programs stay refused, even for a superuser.
    let hint = "HINT: Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.";
    assert_eq!(
        err(&mut s, "COPY t TO PROGRAM 'cat'"),
        format!("42501 permission denied to COPY to or from an external program {hint}")
    );

    // Server-side large object import and export.
    let r = s.query("SELECT lo_import('hello.txt', 4243)").unwrap();
    assert_eq!(format!("{:?}", r.last().unwrap().rows), "[[Oid(4243)]]");
    check(&mut s, "SELECT lo_get(4243) = '\\x68656c6c6f0a'::bytea");
    let r = s.query(&format!("SELECT lo_export(4243, {})", abs("exported.txt"))).unwrap();
    assert_eq!(format!("{:?}", r.last().unwrap().rows), "[[Int4(1)]]");
    assert_eq!(std::fs::read_to_string(root.join("exported.txt")).unwrap(), "hello\n");
    assert_eq!(
        err(&mut s, "SELECT lo_import('missing')"),
        "58P01 could not open server file \"missing\": No such file or directory"
    );
    assert_eq!(err(&mut s, &format!("SELECT lo_import({})", out("secret.txt"))), absolute);
    assert_eq!(err(&mut s, &format!("SELECT lo_export(4243, {})", out("x.txt"))), absolute);
    assert_eq!(err(&mut s, "SELECT lo_export(4243, 'link_out')"), below);
    assert_eq!(
        err(&mut s, &format!("SELECT lo_export(4243, {})", abs("nodir/x"))),
        format!(
            "58P01 could not create server file \"{}/nodir/x\": No such file or directory",
            root.display()
        )
    );
    assert!(!outside.path().join("x.txt").exists());
    engine.shutdown();
}

#[test]
fn predefined_roles_and_privileges() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    std::fs::write(root.join("in.txt"), "7\n").unwrap();
    let engine = open(data.path(), Some(root));
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    let abs = |p: &str| lit(&format!("{}/{p}", root.display()));
    check(
        &mut s,
        "SELECT array_agg(rolname ORDER BY oid) = '{pg_read_server_files,pg_write_server_files,pg_execute_server_program}' \
         FROM pg_roles WHERE oid IN (4569, 4570, 4571) AND NOT rolcanlogin",
    );
    run(&mut s, "CREATE ROLE regress_sf_user");
    run(&mut s, "CREATE TABLE t (a int)");
    run(&mut s, "GRANT ALL ON t TO regress_sf_user");
    run(&mut s, "SET ROLE regress_sf_user");
    // EXECUTE on the file functions is the superuser's until granted.
    assert_eq!(
        err(&mut s, "SELECT pg_read_file('in.txt')"),
        "42501 permission denied for function pg_read_file"
    );
    assert_eq!(err(&mut s, "SELECT lo_import('in.txt')"), "42501 permission denied for function lo_import");
    let hint = "HINT: Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.";
    assert_eq!(
        err(&mut s, &format!("COPY t FROM {}", abs("in.txt"))),
        format!("42501 permission denied to COPY from a file {hint}")
    );
    assert_eq!(
        err(&mut s, &format!("COPY t TO {}", abs("out.txt"))),
        format!("42501 permission denied to COPY to a file {hint}")
    );
    run(&mut s, "RESET ROLE");
    run(&mut s, "GRANT EXECUTE ON FUNCTION pg_read_file(text) TO regress_sf_user");
    run(&mut s, "GRANT pg_read_server_files TO regress_sf_user");
    run(&mut s, "SET ROLE regress_sf_user");
    check(&mut s, "SELECT pg_read_file('in.txt') = E'7\\n'");
    // pg_read_server_files doesn't lift the confinement.
    assert_eq!(err(&mut s, "SELECT pg_read_file('/etc/hosts')"), "42501 absolute path not allowed");
    let r = s.query(&format!("COPY t FROM {}", abs("in.txt"))).unwrap();
    assert_eq!(r.last().unwrap().tag, "COPY 1");
    assert_eq!(
        err(&mut s, &format!("COPY t TO {}", abs("out.txt"))),
        format!("42501 permission denied to COPY to a file {hint}")
    );
    run(&mut s, "RESET ROLE");
    run(&mut s, "GRANT pg_write_server_files, pg_execute_server_program TO regress_sf_user");
    run(&mut s, "SET ROLE regress_sf_user");
    let r = s.query(&format!("COPY t TO {}", abs("out.txt"))).unwrap();
    assert_eq!(r.last().unwrap().tag, "COPY 1");
    assert_eq!(std::fs::read_to_string(root.join("out.txt")).unwrap(), "7\n");
    // Still no programs.
    assert_eq!(
        err(&mut s, "COPY t FROM PROGRAM 'cat'"),
        format!("42501 permission denied to COPY to or from an external program {hint}")
    );
    run(&mut s, "RESET ROLE");
    // Membership without INHERIT doesn't pass the privilege on (has_privs_of_role).
    run(&mut s, "CREATE ROLE regress_sf_noinherit NOINHERIT");
    run(&mut s, "GRANT ALL ON t TO regress_sf_noinherit");
    run(&mut s, "GRANT pg_read_server_files TO regress_sf_noinherit");
    run(&mut s, "SET ROLE regress_sf_noinherit");
    assert_eq!(
        err(&mut s, &format!("COPY t FROM {}", abs("in.txt"))),
        format!("42501 permission denied to COPY from a file {hint}")
    );
    run(&mut s, "RESET ROLE");
    engine.shutdown();
}
