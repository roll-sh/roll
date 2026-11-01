use super::super::*;
use crate::catalog::shared::SubRel;
use crate::engine::EngineOptions;

/// Order the workers at transaction boundaries, without sleeps or a publisher: q finishes
/// its copy between p's statement snapshot and state update, then finishes sync before p
/// commits. The leader must still apply q's subsequent rows exactly once.
#[test]
fn concurrent_copy_completion_keeps_finished_table_ready() {
    let dir = tempfile::tempdir().unwrap();
    let options = || EngineOptions {
        data_dir: dir.path().to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 4,
        auth_files: Default::default(),
        settings: Vec::new(),
    };
    let e = Engine::open(options()).unwrap();
    {
        let mut s = e.connect(1, "postgres", "postgres", "tablesync-test").unwrap();
        s.query("CREATE TABLE p (id int PRIMARY KEY); CREATE TABLE q (id int PRIMARY KEY)").unwrap();
        s.query("CREATE SUBSCRIPTION sub CONNECTION 'dbname=unused' PUBLICATION pub WITH (connect=false)").unwrap();
        let sub = e.subscriptions().into_iter().find(|s| s.name == "sub").unwrap();
        let from = s.state.db.catalog();
        let id = |name| from.tables.values().find(|t| t.name == name).unwrap().oid;
        let (p, q) = (id("p"), id("q"));
        s.query("BEGIN").unwrap();
        let mut cat = (*from).clone();
        cat.sub_rels.insert(sub.oid, vec![SubRel { relid: p, state: 'd', lsn: 0 }, SubRel { relid: q, state: 'd', lsn: 0 }]);
        s.block_txn().unwrap().borrow_mut().set_catalog_from(cat, &from).unwrap();
        s.query("COMMIT").unwrap();
        let workers = Arc::new(Workers::default());
        let stop = AtomicBool::new(false);
        let mut wp = open_worker(&e, &workers, &sub, &stop, Role::Sync(p)).unwrap();
        let mut wq = open_worker(&e, &workers, &sub, &stop, Role::Sync(q)).unwrap();
        let mut leader = open_worker(&e, &workers, &sub, &stop, Role::Apply).unwrap();
        let origin = format!("pg_{}_{q}", sub.oid);
        wq.use_origin(&origin).unwrap();

        wp.query("BEGIN").unwrap();
        wp.query("SELECT 1").unwrap();
        wq.query("BEGIN").unwrap();
        wq.query("INSERT INTO public.q SELECT generate_series(1,40)").unwrap();
        wq.set_states(&[(q, 'f', 0)]).unwrap();
        wq.query("COMMIT").unwrap();
        wp.set_states(&[(p, 's', 20)]).unwrap();
        wq.query("BEGIN").unwrap();
        wq.set_states(&[(q, 's', 20)]).unwrap();
        wq.query("COMMIT").unwrap();
        wq.repl.origins.session_reset(wq.pid).unwrap();
        wq.repl.origins.drop_origin(&origin, false).unwrap();
        wp.query("COMMIT").unwrap();

        leader.load_states();
        leader.rels.insert(q, RemoteRel { nsp: "public".into(), name: "q".into(), ident: b'd', cols: vec![("id".into(), true)] });
        leader.query("BEGIN").unwrap();
        leader.txn = Some(RemoteTxn { final_lsn: 30, xid: 100, skip: false, skipping: false });
        for id in 41..=43 {
            leader.change(q, Change::Insert(vec![Col::Text(id.to_string().as_bytes())])).unwrap();
        }
        leader.query("COMMIT").unwrap();
        let count = s.query("SELECT count(*) FROM q").unwrap().pop().unwrap().rows;
        assert_eq!(format!("{count:?}"), "[[Int8(43)]]", "committed sync states: {:?}", s.state.db.catalog().sub_rels[&sub.oid]);
        leader.ready_tables(30, true).unwrap();
        let rels = &s.state.db.catalog().sub_rels[&sub.oid];
        assert_eq!(rels.len(), 2, "one catalog row per synchronized table: {rels:?}");
        assert!(rels.iter().all(|r| r.state == 'r'));
    }
    e.shutdown();
    drop(e);
    let e = Engine::open(options()).unwrap();
    let mut s = e.connect(1, "postgres", "postgres", "tablesync-test").unwrap();
    let count = s.query("SELECT count(*) FROM q").unwrap().pop().unwrap().rows;
    assert_eq!(format!("{count:?}"), "[[Int8(43)]]");
    let rels = s.query("SELECT count(*), count(*) FILTER (WHERE srsubstate = 'r') FROM pg_subscription_rel").unwrap().pop().unwrap().rows;
    assert_eq!(format!("{rels:?}"), "[[Int8(2), Int8(2)]]");
    drop(s);
    e.shutdown();
}
