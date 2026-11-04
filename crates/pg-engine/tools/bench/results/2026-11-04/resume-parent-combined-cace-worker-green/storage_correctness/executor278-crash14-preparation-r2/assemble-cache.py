# Cache-only artifact construction. Does not apply patches or execute candidate code.
from pathlib import Path
p=Path(__file__).parent
original=(p/'original/crates/pg-engine/tests/crash_random.rs').read_text()
s=original
s=s.replace('fn open(dir: &Path)', '#[path = "support/crash14_diagnostic.rs"]\nmod diagnostic;\nuse serde_json::json;\n\nfn open(dir: &Path)',1)
s=s.replace('''    std::fs::read_to_string(dir.join("steps"))
        .unwrap_or_default()''','''    let original_read = std::fs::read_to_string(dir.join("steps"));
    diagnostic::event(dir, "main", || json!({"kind":"original_journal_read",
        "result_debug":format!("{original_read:?}")}));
    diagnostic::journal(dir, "read_log");
    original_read.unwrap_or_default()''',1)
# Verify the actual original spelling before replacing.
if 'let original_read' not in s:
 s=s.replace('''    std::fs::read_to_string(dir.join("steps")).unwrap_or_default()''','''    let original_read = std::fs::read_to_string(dir.join("steps"));
    diagnostic::event(dir, "main", || json!({"kind":"original_journal_read",
        "result_debug":format!("{original_read:?}")}));
    diagnostic::journal(dir, "read_log");
    original_read.unwrap_or_default()''',1)
assert 'let original_read' in s
s=s.replace('''            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_micros(us));''','''            if diagnostic::enabled() {
                diagnostic::arm(dir, at, us);
            } else {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_micros(us));''',1)
s=s.replace('''                std::process::Command::new("kill").args(["-9", &me]).status().unwrap();
            });''','''                std::process::Command::new("kill").args(["-9", &me]).status().unwrap();
            });
            }''',1)
s=s.replace('''        let (sql, end, ops) = step(seed, i as u64, m);
        if let Ok(f)''','''        let (sql, end, ops) = step(seed, i as u64, m);
        // Digest existing SQL bytes only: never traverse/format the full model
        // or operations in the kill window. No closure work when disabled.
        diagnostic::event(dir, "main", || json!({"kind":"step_generated","seed":seed,"id":i,
            "end":format!("{end:?}"),"sql":sql.iter().map(|q| json!({
                "bytes":q.len(),"sha256":diagnostic::digest(q.as_bytes())})).collect::<Vec<_>>()}));
        if let Ok(f)''',1)
s=s.replace('''        for q in &sql {
            s.query(q).unwrap_or_else(|e| panic!("seed {seed} step {i}: {q}: {e:?}"));
        }''','''        for (statement, q) in sql.iter().enumerate() {
            diagnostic::event(dir, "main", || json!({"kind":"sql_begin","seed":seed,"id":i,
                "statement":statement,"bytes":q.len(),"sha256":diagnostic::digest(q.as_bytes()),
                "explicit_checkpoint":q == "CHECKPOINT"}));
            let result = s.query(q);
            diagnostic::event(dir, "main", || json!({"kind":"sql_result","seed":seed,"id":i,
                "statement":statement,"ok":result.is_ok(),"explicit_checkpoint":q == "CHECKPOINT"}));
            result.unwrap_or_else(|e| panic!("seed {seed} step {i}: {q}: {e:?}"));
        }''',1)
s=s.replace('''        append_log(dir, i, committed);
        if std::env''','''        append_log(dir, i, committed);
        diagnostic::event(dir, "main", || json!({"kind":"journal_append_synced","id":i,"committed":committed,
            "model_a_len":m.a.len(),"model_b_len":m.b.len()}));
        if std::env''',1)
# Keep append_log itself byte-for-byte original (including original fsync).
s=s.replace('fn settle(s: &mut Session, dir: &Path, seed: u64) -> Model {','''fn settle(s: &mut Session, dir: &Path, seed: u64) -> Model {
    if !diagnostic::enabled() { return settle_original(s, dir, seed); }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| settle_original(s, dir, seed)));
    match result {
        Ok(model) => {
            diagnostic::event(dir, "main", || json!({"kind":"settle_returned_model","model_debug":format!("{model:?}")}));
            model // No snapshot reads before the original first check.
        }
        Err(payload) => {
            diagnostic::best_effort(|| diagnostic::snapshot(s, dir, "after-original-settle-panic"));
            std::panic::resume_unwind(payload)
        }
    }
}

fn settle_original(s: &mut Session, dir: &Path, seed: u64) -> Model {''',1)
s=s.replace('''    // (DDL holds locks that reads of its tables wait for.)''','''    diagnostic::event(dir, "main", || json!({"kind":"settle_generated","seed":seed,"id":i,
        "read_log":log,"end":format!("{end:?}"),"ops_debug":format!("{ops:?}"),
        "model_before_debug":format!("{m:?}")}));
    // (DDL holds locks that reads of its tables wait for.)''',1)
s=s.replace('''    let committed = if prepared.contains(&format!("p{i}"))''','''    diagnostic::event(dir, "main", || json!({"kind":"settle_observed","seed":seed,"id":i,
        "prepared":prepared,"in_txlog":in_log,"ddl":ddl}));
    let committed = if prepared.contains(&format!("p{i}"))''',1)
s=s.replace('''    append_log(dir, i, committed);
    log.push(committed);''','''    diagnostic::event(dir, "main", || json!({"kind":"settle_decision","id":i,"committed":committed}));
    append_log(dir, i, committed);
    log.push(committed);''',1)
# Pass dir for evidence only. Original oracle body remains otherwise unchanged.
s=s.replace('check(s, seed,', 'check_original(s, dir, seed,')
s=s.replace('check(&mut s, seed,', 'check(&mut s, dir, seed,')
s=s.replace('fn check(s: &mut Session, seed: u64, m: &Model, what: &str) {','''fn check(s: &mut Session, dir: &Path, seed: u64, m: &Model, what: &str) {
    if !diagnostic::enabled() { return check_original(s, dir, seed, m, what); }
    diagnostic::event(dir, "main", || json!({"kind":"full_check_begin","what":what,"model_debug":format!("{m:?}")}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check_original(s, dir, seed, m, what)));
    diagnostic::best_effort(|| diagnostic::snapshot(s, dir,
        if result.is_ok() { "after-original-check-pass" } else { "after-original-check-panic" }));
    match result {
        Ok(()) => { diagnostic::event(dir, "main", || json!({"kind":"full_check_passed","what":what})); }
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn check_original(s: &mut Session, dir: &Path, seed: u64, m: &Model, what: &str) {''',1)
s=s.replace('''        if got != want_a {''','''        diagnostic::comparison(dir, "a", how, &got, &want_a);
        if got != want_a {''',1)
s=s.replace('''        if got != want_b {''','''        diagnostic::comparison(dir, "b", how, &got, &want_b);
        if got != want_b {''',1)
s=s.replace('''    let env = |k: &str|''','''    diagnostic::child_watchdog();
    let env = |k: &str|''',1)
s=s.replace('''    let mut m = replay(seed, &log);
    run_steps''','''    let mut m = replay(seed, &log);
    diagnostic::event(dir, "main", || json!({"kind":"child_replay_boundary","model_debug":format!("{m:?}")}));
    // No added child SQL/check/nextval before original workload.
    run_steps''',1)
s=s.replace('''    let out = std::process::Command::new(std::env::current_exe().unwrap())''','''    let requested_delay_us;
    let out = std::process::Command::new(std::env::current_exe().unwrap())''',1)
s=s.replace('''        .env("CRASH_KILL_US", r.below(20_000).to_string())''','''        .env("CRASH_KILL_US", {
            requested_delay_us = r.below(20_000);
            diagnostic::event(dir, "main", || json!({"kind":"round_schedule","seed":seed,"round":round,
                "kill_at":at,"requested_delay_us":requested_delay_us}));
            requested_delay_us.to_string()
        })''',1)
s=s.replace('''    let why = String::from_utf8_lossy(&out.stderr)''','''    diagnostic::before_parent_open(dir, seed, round, &out, at, requested_delay_us);
    if diagnostic::enabled() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(out.status.signal(), Some(9), "diagnostic child was not SIGKILLed");
    }
    let why = String::from_utf8_lossy(&out.stderr)''',1)
s=s.replace('''    for seed in seeds {
        let tmp = tempfile::tempdir().unwrap();
        for round in 0..rounds {
            crash_round(tmp.path(), seed, round);
        }
    }''','''    let diagnostic_root = diagnostic::begin(&seeds, rounds);
    for seed in seeds {
        let temporary = diagnostic_root.is_none().then(|| tempfile::tempdir().unwrap());
        let retained;
        let dir = if let Some(root) = &diagnostic_root {
            retained = diagnostic::seed_dir(root, seed);
            retained.as_path()
        } else { temporary.as_ref().unwrap().path() };
        for round in 0..rounds {
            crash_round(dir, seed, round);
        }
    }''',1)
assert 'diagnostic::arm' in s and 'let result = s.query(q);' in s and 'fn check_original' in s
(p/'candidate/crates/pg-engine/tests/crash_random.rs').write_text(s)
