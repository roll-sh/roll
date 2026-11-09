//! What idle databases cost (`cargo test --release -p redis-server --test idle -- --ignored
//! --nocapture`): CPU used by a process holding many loaded databases that nobody uses.

use redis_engine::Config;
use redis_server::Database;

/// This process's CPU time so far (seconds), as `ps` reports it.
fn cpu_seconds() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "time=", "-p", &std::process::id().to_string()]).output().unwrap();
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let parts: Vec<f64> = t.split(':').map(|p| p.parse().unwrap_or(0.0)).collect();
    parts.iter().fold(0.0, |acc, p| acc * 60.0 + p)
}

#[test]
#[ignore]
fn idle_cost() {
    let n: usize = std::env::var("ROLL_IDLE_DBS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async {
        let dbs: Vec<_> = (0..n)
            .map(|i| {
                let db = Database::new(format!("db{i}"), Config::default(), 1);
                db.start_cron();
                db
            })
            .collect();
        // Past the quiet period.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let t0 = cpu_seconds();
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let used = cpu_seconds() - t0;
        eprintln!("idle: {} databases, {:.1} ms of CPU per second", dbs.len(), used * 100.0);
    });
}
