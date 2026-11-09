//! Landing throughput: concurrent agents each create a changeset on the head they last saw and
//! land it; some edit shared "hot" files (different lines), which needs a 3-way merge.
//!
//! `CODE_THROUGHPUT_N` sets the number of changes (default 2000; the target for
//! `cargo test --release -p code-store --test throughput -- --nocapture` is 40000).

mod common;

use code_store::{Expect, LandError, NewChangeset};
use common::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const REPO: &str = "repo_bench";
const AGENTS: usize = 64;
const HOT_FILES: usize = 4;
/// Lines between two agents' lines in a hot file, so their edits merge cleanly.
const SPACING: usize = 4;

fn hot_line(agent: usize, rev: usize) -> String {
    format!("agent {agent} rev {rev}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn land_40k_merges() {
    let n: usize = std::env::var("CODE_THROUGHPUT_N").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let hot = |i: usize| (i, lines(AGENTS * SPACING, |l| if l % SPACING == 0 { hot_line(l / SPACING, 0) } else { format!("ctx {l}") }));
    let edits = (0..HOT_FILES).map(|i| write(&format!("hot/{i}.txt"), &hot(i).1)).collect();
    let root = commit(&store, REPO, None, "initial", edits).await;
    store.update_ref(REPO, "main", Expect::Absent, Some(root)).await.unwrap();

    let create_us = Arc::new(AtomicU64::new(0));
    let land_us = Arc::new(AtomicU64::new(0));
    let conflicts = Arc::new(AtomicU64::new(0));
    let merged = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let mut tasks = Vec::new();
    for agent in 0..AGENTS {
        let store = store.clone();
        let (conflicts, merged) = (conflicts.clone(), merged.clone());
        let (create_us, land_us) = (create_us.clone(), land_us.clone());
        let quota = n / AGENTS + usize::from(agent < n % AGENTS);
        tasks.push(tokio::spawn(async move {
            // Each agent tracks its own line in each hot file.
            let mut revs = vec![0usize; HOT_FILES];
            for i in 0..quota {
                let touch_hot = (agent + i) % 10 == 0;
                loop {
                    let t0 = Instant::now();
                    let base = store.get_ref(REPO, "main").unwrap().unwrap();
                    let mut edits = vec![write(&format!("agents/{agent}/{i}.txt"), &format!("change {i} by {agent}\n"))];
                    let mut next = revs.clone();
                    if touch_hot {
                        let f = (agent + i) % HOT_FILES;
                        next[f] = i + 1;
                        let path = format!("hot/{f}.txt");
                        let text = read(&store, REPO, &base.hex(), &path).await.unwrap();
                        let mut out = String::new();
                        for (l, line) in text.lines().enumerate() {
                            out.push_str(if l == agent * SPACING { hot_line(agent, i + 1) } else { line.to_string() }.as_str());
                            out.push('\n');
                        }
                        edits.push(write(&path, &out));
                    }
                    let cs = store
                        .create_changeset(REPO, NewChangeset {
                            base: Some(base),
                            other_parents: vec![],
                            author: sig(),
                            committer: None,
                            message: format!("agent {agent} change {i}"),
                            edits,
                        })
                        .await
                        .unwrap();
                    let t1 = Instant::now();
                    create_us.fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
                    let landed = store.land(REPO, "main", cs).await;
                    land_us.fetch_add(t1.elapsed().as_micros() as u64, Ordering::Relaxed);
                    match landed {
                        Ok(l) => {
                            if !l.merged_paths.is_empty() {
                                merged.fetch_add(1, Ordering::Relaxed);
                            }
                            revs = next;
                            break;
                        }
                        Err(LandError::Conflict(_)) => {
                            conflicts.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => panic!("{e:#}"),
                    }
                }
            }
            revs
        }));
    }
    let mut final_revs = Vec::new();
    for t in tasks {
        final_revs.push(t.await.unwrap());
    }
    let elapsed = start.elapsed();
    let rate = n as f64 / elapsed.as_secs_f64();
    println!(
        "landed {n} changes from {AGENTS} agents in {:.2}s: {rate:.0} merges/s ({} needed a 3-way merge, {} conflicts retried); 40k/day = 0.46/s",
        elapsed.as_secs_f64(),
        merged.load(Ordering::Relaxed),
        conflicts.load(Ordering::Relaxed),
    );
    println!(
        "per change: create {:.2}ms, land (queue + lander) {:.2}ms",
        create_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0,
        land_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0
    );

    let stats = store.stats(REPO).unwrap();
    assert_eq!(stats.landed_total, n as u64);
    let head = store.get_ref(REPO, "main").unwrap().unwrap();
    let meta = store.meta(REPO, &head).unwrap().unwrap();
    assert_eq!(meta.depth, n as u64, "every change is one changeset on main");
    // Every agent's last edit to every hot file survived the merges.
    for f in 0..HOT_FILES {
        let text = read(&store, REPO, "main", &format!("hot/{f}.txt")).await.unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (agent, revs) in final_revs.iter().enumerate() {
            assert_eq!(lines[agent * SPACING], hot_line(agent, revs[f]), "hot/{f}.txt agent {agent}");
        }
    }
}
