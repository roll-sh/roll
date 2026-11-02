#[test]
#[ignore = "explicit disposable fixture copy required"]
fn retained_gather_once_probe() {
    std::thread::Builder::new().stack_size(64 << 20).spawn(|| {
        crate::stack::set_limit(60 << 20);
        let engine = crate::engine::Engine::open(crate::engine::EngineOptions {
            data_dir: std::env::var_os("GATHER_PROOF_DATA").expect("private copy").into(),
            sync: false, cache_bytes: 256 << 20,
            settings: vec![("autovacuum".into(),"off".into())], ..Default::default()
        }).unwrap();
        let db = std::env::var("GATHER_PROOF_DB").unwrap();
        let range = "SELECT id, balance FROM accounts WHERE id BETWEEN $1 AND $1+99 ORDER BY id";
        let point = "SELECT id, owner, balance, note, created FROM accounts WHERE id=$1";
        let mut executions = 0; let mut checked_rows = 0;
        for phase in ["primary", "positive", "zero", "refused"] {
            for (slot,mode) in ["auto","force_custom_plan","force_generic_plan"].iter().enumerate() {
                let mut s = engine.connect(2147000400+slot as i32,&db,"postgres","gather-once-probe").unwrap();
                s.query(&format!("SET plan_cache_mode={mode}; SET min_parallel_table_scan_size=0; SET min_parallel_index_scan_size=0")).unwrap();
                if phase == "positive" || phase == "zero" {
                    s.query(&format!("SET parallel_setup_cost={}; SET parallel_tuple_cost=0", if phase == "positive" {1} else {0})).unwrap();
                }
                if phase == "refused" && slot == 0 {
                    s.query("CREATE STATISTICS gather_once_refusal ON id,balance FROM accounts").unwrap();
                }
                for (sql, rows) in [(range,100), (point,1)] {
                    s.parse_message("checked",sql,&[23]).unwrap();
                    for round in 0..20 {
                        let first = 1000 + round*1000;
                        with_plan(&mut s,sql,&[Value::Int4(first)],|plan,ctx| {
                            let (counts, sites, original) = crate::sql::explain::gather_comparison(plan,ctx);
                            assert_eq!(counts.serial,1);
                            assert_eq!(counts.attempted,1,"{phase} {mode} {sql}");
                            assert_eq!(counts.proved,usize::from(phase=="primary"),"{phase} {mode} {sql}");
                            assert!(sites.is_empty());
                            if phase=="primary" {assert_eq!(counts.relations,1);assert_eq!(counts.partials,0);}
                            else {assert_eq!(counts.relations,original.relations);assert_eq!(counts.partials,original.partials);}
                            if round==0 {println!("retained phase={phase} mode={mode} rows={rows} counts={counts:?} original={original:?}");}
                        });
                        s.bind_message("","checked",vec![Some(first.to_string().into_bytes())],&[],vec![]).unwrap();
                        let (outcome,suspended)=s.execute_portal("",0).unwrap();assert!(!suspended);
                        let Outcome::Done(result)=outcome else {panic!("query rows")};
                        assert_eq!(result.rows.len(),rows);
                        for (offset,row) in result.rows.iter().enumerate() {
                            let id=first+offset as i32;
                            assert_eq!(row[0],Value::Int4(id));
                            let balance_col=if rows==100 {1} else {2};
                            assert_eq!(row[balance_col],Value::Int8(id as i64 * 7919 % 100000));
                        }
                        executions+=1;checked_rows+=rows;s.sync().unwrap();
                    }
                    s.close_statement("checked");
                }
            }
        }
        println!("retained-checked executions={executions} rows={checked_rows}");
        engine.shutdown();
    }).unwrap().join().unwrap();
}
