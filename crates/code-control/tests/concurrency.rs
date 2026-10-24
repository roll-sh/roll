//! Two roll code nodes on one control database (the same SQLite file, or the same Postgres
//! schema with `ROLL_CODE_TEST_DATABASE`), racing on every read-modify-write: change and work
//! item numbers, work item claims, merge queue positions and tokens. Each node has its own
//! connections, so on Postgres this is what a cluster does.

mod common;

use axum::http::StatusCode;
use code_control::db::{NewChange, NewWork};
use code_control::model::{Lane, WorkState};
use code_control::{Caller, Code, Config, Error};
use common::{App, app};
use pg_storage::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

const ADMIN: Option<&str> = Some("admin-secret");

/// A second node on `a`'s control database, with its own connections and its own state directory.
fn second_node(a: &App) -> App {
    let control = match a.code.db.sql().pg_schema() {
        Some(schema) => {
            let url = std::env::var(code_sql::TEST_DATABASE_ENV).unwrap();
            code_sql::Db::connect(&url, schema, code_sql::PgOptions { max_connections: 8, ..Default::default() }).unwrap()
        }
        None => code_sql::Db::open(&code_sql::Target::Sqlite(a.dir.path().join("code").join("control.db"))).unwrap(),
    };
    let dir = tempfile::tempdir().unwrap();
    let objects = Objects::new(&StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let mut config = Config::local(dir.path().to_path_buf());
    config.admin_token = ADMIN.map(str::to_string);
    let code = Code::open_with_db(config, objects, control).unwrap();
    let router = code_control::api::router(code.clone());
    App { code, router, dir }
}

/// Two nodes and a repo `acme/app`; its id.
async fn two_nodes() -> (App, App, String) {
    let a = app(ADMIN);
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), ADMIN).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, r) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app" })), ADMIN).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    let b = second_node(&a);
    let id = a.code.find_repo("acme", "app").unwrap().id;
    assert_eq!(b.code.find_repo("acme", "app").unwrap().id, id, "both nodes see the repo");
    (a, b, id)
}

fn change<'a>(title: &'a str) -> NewChange<'a> {
    NewChange { title, body: "", author: "agent", base: "cs_base", head: "cs_head", work_item: None, branch: None, trusted: true, draft: false }
}

/// Changes and work items share one number sequence per repo: created at once from both nodes,
/// they get every number exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn numbers_are_unique_across_nodes() {
    let (a, b, repo) = two_nodes().await;
    const PER_THREAD: usize = 25;
    std::thread::scope(|s| {
        for (i, code) in [&a.code, &b.code, &a.code, &b.code, &a.code, &b.code].into_iter().enumerate() {
            let repo = repo.clone();
            s.spawn(move || {
                for k in 0..PER_THREAD {
                    let title = format!("t{i}-{k}");
                    if k % 2 == 0 {
                        code.db.insert_change(&format!("ch_{i}_{k}"), &repo, &change(&title)).unwrap();
                    } else {
                        let w = NewWork { title: &title, body: "", labels: &[], priority: 2, source: "native", github_url: None, state: WorkState::Open };
                        code.db.insert_work(&format!("wi_{i}_{k}"), &repo, &w).unwrap();
                    }
                }
            });
        }
    });
    let changes = a.code.db.changes(&repo, None, 10_000).unwrap();
    let work = b.code.db.work_items(&repo, None, None, 10_000).unwrap();
    let numbers: Vec<u64> = changes.iter().map(|c| c.number).chain(work.iter().map(|w| w.number)).collect();
    let unique: BTreeSet<u64> = numbers.iter().copied().collect();
    assert_eq!(numbers.len(), 6 * PER_THREAD);
    assert_eq!(unique, (1..=(6 * PER_THREAD) as u64).collect::<BTreeSet<_>>(), "every number once, none skipped");
    // The next one, from either node, follows on.
    b.code.db.insert_change("ch_last", &repo, &change("last")).unwrap();
    assert_eq!(a.code.db.change("ch_last").unwrap().unwrap().number, 6 * PER_THREAD as u64 + 1);
}

/// Agents on both nodes claiming at once: every item goes to exactly one of them, and nobody
/// gets an item twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn each_work_item_has_exactly_one_claimant() {
    let (a, b, _) = two_nodes().await;
    const ITEMS: usize = 24;
    for i in 0..ITEMS {
        let node = if i % 2 == 0 { &a } else { &b };
        let (s, v) = node.req("POST", "/v1/code/repos/acme/app/work", Some(json!({ "title": format!("item {i}"), "labels": ["x"] })), ADMIN).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let (a, b) = (Arc::new(a), Arc::new(b));
    let mut tasks = Vec::new();
    for agent in 0..16 {
        let node = if agent % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move {
            let mut got = Vec::new();
            loop {
                let (s, v) = node.req("POST", "/v1/code/repos/acme/app/work/claim", Some(json!({ "agent": format!("agent-{agent}"), "labels": ["x"] })), ADMIN).await;
                match s {
                    StatusCode::OK => got.push((v["id"].as_str().unwrap().to_string(), v["claimed_by"].as_str().unwrap().to_string())),
                    StatusCode::NO_CONTENT => return (agent, got),
                    s => panic!("claim: {s} {v}"),
                }
            }
        }));
    }
    let mut owner: HashMap<String, String> = HashMap::new();
    for t in tasks {
        let (agent, got) = t.await.unwrap();
        for (id, by) in got {
            assert_eq!(by, format!("agent-{agent}"));
            assert!(owner.insert(id.clone(), by).is_none(), "{id} was claimed twice");
        }
    }
    assert_eq!(owner.len(), ITEMS, "every item was claimed");
    // What the database says agrees, from either node.
    for (id, by) in &owner {
        let w = b.code.db.work(id).unwrap().unwrap();
        assert_eq!((w.state, w.claimed_by.as_deref()), (WorkState::Claimed, Some(by.as_str())));
    }
    let (_, v) = a.req("GET", "/v1/code/repos/acme/app/work?state=open", None, ADMIN).await;
    assert_eq!(v.as_array().map(Vec::len), Some(0), "{v}");
}

/// Changes queued at once from both nodes get distinct positions in one order; a change
/// enqueued on both nodes at once has a single entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_positions_are_unique_across_nodes() {
    let (a, b, repo) = two_nodes().await;
    const CHANGES: usize = 30;
    for i in 0..CHANGES {
        a.code.db.insert_change(&format!("ch_{i}"), &repo, &change(&format!("c{i}"))).unwrap();
    }
    // Every change is enqueued by both nodes, from two threads each.
    let entries: Vec<(String, String, i64)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let (code, repo) = (if t % 2 == 0 { &a.code } else { &b.code }, repo.clone());
                s.spawn(move || {
                    (0..CHANGES)
                        .map(|k| {
                            let i = (k + t * 7) % CHANGES;
                            let lane = if i.is_multiple_of(5) { Lane::Urgent } else { Lane::Normal };
                            let e = code.db.enqueue(&format!("qe_{t}_{i}"), &repo, &format!("ch_{i}"), lane, Some("agent")).unwrap();
                            (e.change_id, e.id, e.seq)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    // One entry per change, whoever enqueued it.
    let mut by_change: HashMap<String, (String, i64)> = HashMap::new();
    for (change, id, seq) in entries {
        let prev = by_change.insert(change.clone(), (id.clone(), seq));
        assert!(prev.is_none_or(|p| p == (id, seq)), "{change} got two entries");
    }
    assert_eq!(by_change.len(), CHANGES);
    let active = b.code.db.active_entries(&repo).unwrap();
    assert_eq!(active.len(), CHANGES);
    let seqs: BTreeSet<i64> = active.iter().map(|e| e.seq).collect();
    assert_eq!(seqs.len(), CHANGES, "distinct positions");
    // Positions are 1..=n in queue order, the same from either node.
    for (i, e) in active.iter().enumerate() {
        assert_eq!(a.code.db.position(e).unwrap(), i as u64 + 1);
        assert_eq!(b.code.db.position(e).unwrap(), i as u64 + 1);
    }
}

/// Tokens made at once on both nodes all work on both, and a revoked one stops working on the
/// other node at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_work_and_revoke_across_nodes() {
    let (a, b, _) = two_nodes().await;
    let made: Vec<Value> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let code = if t % 2 == 0 { a.code.clone() } else { b.code.clone() };
                s.spawn(move || (0..5).map(|k| code.create_token(&Caller::Admin, "acme", &format!("t{t}-{k}")).unwrap()).collect::<Vec<_>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let ids: BTreeSet<&str> = made.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 40, "distinct ids");
    assert_eq!(a.code.list_tokens(&Caller::Admin, "acme").unwrap().len(), 40);
    for (i, t) in made.iter().enumerate() {
        let tok = t["token"].as_str().unwrap();
        let other = if i % 2 == 0 { &b.code } else { &a.code };
        match other.caller(Some(tok)).unwrap() {
            Caller::Owner { name, token, .. } => assert_eq!((name.as_str(), token.as_str()), ("acme", t["name"].as_str().unwrap())),
            c => panic!("{c:?}"),
        }
    }
    let victim = &made[0];
    a.code.revoke_token(&Caller::Admin, "acme", victim["id"].as_str().unwrap()).unwrap();
    assert!(matches!(b.code.caller(Some(victim["token"].as_str().unwrap())), Err(Error::Unauthorized)));
    let (s, _) = b.req("GET", "/v1/code/owners/acme/tokens", None, Some(victim["token"].as_str().unwrap())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
