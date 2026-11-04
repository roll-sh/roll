mod common;
#[test]
fn caab_domain_check_takes_replay_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = common::engine_session(common::open_store(dir.path()), "cursor_replay_probe");
    for sql in [
        "CREATE TABLE t (id int primary key)",
        "INSERT INTO t VALUES (1), (2), (3)",
        "CREATE FUNCTION noticing(int) RETURNS bool IMMUTABLE LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'domain check %', $1; RETURN true; END $$",
        "CREATE DOMAIN noticing_int AS int CHECK (noticing(value))",
        "BEGIN",
        "DECLARE p SCROLL CURSOR FOR SELECT id FROM t WHERE id::noticing_int > 0",
    ] { s.query(sql).unwrap(); }
    let forward = common::engine_query(&mut s, "FETCH ALL FROM p");
    let retained = s.state.cursors.borrow()["p"].rows.len();
    eprintln!("caab domain projection rows retained after forward ALL: {retained}; forward: {forward:?}");
    assert_eq!(retained, 0, "probe must exercise caab replay path");
    let backward = common::engine_query(&mut s, "FETCH BACKWARD 1 FROM p");
    eprintln!("first backward fetch: {backward:?}");
    s.query("ROLLBACK").unwrap();
}
