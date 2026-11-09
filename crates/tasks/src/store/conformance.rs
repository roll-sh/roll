//! Behaviour every store must have. Run against SQLite always and DynamoDB when configured.

use super::*;
use crate::ids::new_id;
use serde_json::json;
use std::sync::Arc;

fn message(lane: &str, due_at: Ms) -> Message {
    let now = now_ms();
    Message {
        id: new_id("msg"),
        target: Target::Http { url: "http://example.test/hook".into(), method: "POST".into(), headers: vec![] },
        queue: None,
        lane: lane.into(),
        state: MsgState::Pending,
        due_at,
        created_at: now,
        updated_at: now,
        attempts: 0,
        retries: 3,
        backoff: Backoff::default(),
        timeout_ms: 30_000,
        callback: None,
        failure_callback: None,
        dedup_id: None,
        group: None,
        flows: vec![],
        topic: None,
        schedule_id: None,
        run_id: None,
        label: None,
        content_type: None,
        body_size: 5,
        lease: None,
        deferrals: 0,
        last: None,
        reason: None,
        expires_at: None,
        version: 0,
    }
}

pub async fn run(s: Arc<dyn Store>) {
    let suffix = crate::ids::ulid();
    messages(&*s, &suffix).await;
    groups(&*s, &suffix).await;
    listing(&*s, &suffix).await;
    kv(&*s, &suffix).await;
    docs(&*s, &suffix).await;
    runs(&*s, &suffix).await;
    events_and_waits(&*s, &suffix).await;
    usage(&*s).await;
}

async fn messages(s: &dyn Store, sfx: &str) {
    let lane = format!("t:{sfx}:a");
    let now = now_ms();
    let a = message(&lane, now - 10);
    let b = message(&lane, now - 5);
    let later = message(&lane, now + 60_000);
    for m in [&a, &b, &later] {
        s.msg_insert(m, b"hello").await.unwrap();
    }
    let got = s.msg_get(&a.id).await.unwrap().unwrap();
    assert_eq!(got.version, 1, "insert stores version 1");
    assert_eq!(got.target, a.target);
    assert_eq!(s.msg_body(&a.id).await.unwrap().unwrap(), b"hello");
    assert!(s.msg_get("msg_missing").await.unwrap().is_none());

    let due = s.msg_due(&lane, now, 10).await.unwrap();
    assert_eq!(due.ids, vec![a.id.clone(), b.id.clone()], "due in due-time order");
    assert_eq!(due.next_at, Some(later.due_at));
    let due = s.msg_due(&lane, now, 1).await.unwrap();
    assert_eq!(due.ids, vec![a.id.clone()], "limit");

    // Lease a: it leaves the due set until the lease lapses.
    let mut leased = got.clone();
    leased.state = MsgState::Active;
    leased.due_at = now + 30_000;
    assert!(s.msg_update(&leased, 1).await.unwrap());
    assert!(!s.msg_update(&leased, 1).await.unwrap(), "stale version loses");
    let due = s.msg_due(&lane, now, 10).await.unwrap();
    assert_eq!(due.ids, vec![b.id.clone()]);
    assert_eq!(due.next_at, Some(now + 30_000));
    let due = s.msg_due(&lane, now + 31_000, 10).await.unwrap();
    assert_eq!(due.ids, vec![b.id.clone(), a.id.clone()], "a lapsed lease is due again");
    assert_eq!(s.lane_depth(&lane, now).await.unwrap(), (1, 1, 1));

    // Final states leave the index.
    let mut done = s.msg_get(&a.id).await.unwrap().unwrap();
    assert_eq!(done.version, 2);
    done.state = MsgState::Delivered;
    done.expires_at = Some(now + 1000);
    assert!(s.msg_update(&done, 2).await.unwrap());
    assert_eq!(s.msg_due(&lane, now + 31_000, 10).await.unwrap().ids, vec![b.id.clone()]);

    // Moving lanes.
    let mut moved = s.msg_get(&b.id).await.unwrap().unwrap();
    moved.lane = format!("t:{sfx}:b");
    assert!(s.msg_update(&moved, 1).await.unwrap());
    assert!(s.msg_due(&lane, now, 10).await.unwrap().ids.is_empty());
    assert_eq!(s.msg_due(&moved.lane, now, 10).await.unwrap().ids, vec![b.id.clone()]);

    // Attempts.
    for n in 1..=3 {
        s.attempt_add(&Attempt {
            message_id: a.id.clone(),
            n,
            at: now + n as i64,
            duration_ms: 5,
            status: Some(500),
            error: None,
            response: Some("x".into()),
            next_at: None,
        })
        .await
        .unwrap();
    }
    let att = s.attempts(&a.id).await.unwrap();
    assert_eq!(att.iter().map(|a| a.n).collect::<Vec<_>>(), vec![1, 2, 3]);

    // Expiry.
    s.sweep(now + 2000).await.unwrap();
    if s.describe().starts_with("sqlite") {
        assert!(s.msg_get(&a.id).await.unwrap().is_none(), "expired message swept");
        assert!(s.attempts(&a.id).await.unwrap().is_empty());
    }
    s.msg_delete(&b.id).await.unwrap();
    assert!(s.msg_get(&b.id).await.unwrap().is_none());
    assert!(s.msg_body(&b.id).await.unwrap().is_none());
}

async fn groups(s: &dyn Store, sfx: &str) {
    let group = format!("g:{sfx}");
    let lane = format!("t:{sfx}:g");
    let now = now_ms();
    let mut ms = Vec::new();
    for _ in 0..3 {
        let mut m = message(&lane, now);
        m.group = Some(group.clone());
        s.msg_insert(&m, b"").await.unwrap();
        ms.push(m);
    }
    assert_eq!(s.msg_group_head(&group).await.unwrap(), Some(ms[0].id.clone()));
    let mut first = s.msg_get(&ms[0].id).await.unwrap().unwrap();
    first.state = MsgState::Failed;
    assert!(s.msg_update(&first, 1).await.unwrap());
    assert_eq!(s.msg_group_head(&group).await.unwrap(), Some(ms[1].id.clone()));
    for m in &ms[1..] {
        let mut m = s.msg_get(&m.id).await.unwrap().unwrap();
        let v = m.version;
        m.state = MsgState::Canceled;
        assert!(s.msg_update(&m, v).await.unwrap());
    }
    assert_eq!(s.msg_group_head(&group).await.unwrap(), None);
}

async fn listing(s: &dyn Store, sfx: &str) {
    let queue = format!("q{sfx}");
    let lane = format!("t:{sfx}:l");
    let mut ids = Vec::new();
    for i in 0..5 {
        let mut m = message(&lane, now_ms());
        m.queue = Some(queue.clone());
        m.label = Some(if i % 2 == 0 { "even" } else { "odd" }.into());
        if i == 4 {
            m.state = MsgState::Failed;
        }
        s.msg_insert(&m, b"").await.unwrap();
        ids.push(m.id);
    }
    let q = MessageQuery { queue: Some(queue.clone()), limit: 2, ..Default::default() };
    let p1 = s.msg_list(&q).await.unwrap();
    assert_eq!(p1.items.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), vec![ids[4].clone(), ids[3].clone()]);
    let p2 = s.msg_list(&MessageQuery { cursor: p1.cursor.clone(), ..q.clone() }).await.unwrap();
    assert_eq!(p2.items.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), vec![ids[2].clone(), ids[1].clone()]);
    let p3 = s.msg_list(&MessageQuery { cursor: p2.cursor.clone(), ..q.clone() }).await.unwrap();
    assert_eq!(p3.items.len(), 1);
    assert!(p3.cursor.is_none());

    let failed = s
        .msg_list(&MessageQuery { queue: Some(queue.clone()), state: Some(MsgState::Failed), limit: 10, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(failed.items.len(), 1);
    let pending = s
        .msg_list(&MessageQuery { queue: Some(queue.clone()), state: Some(MsgState::Pending), limit: 10, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(pending.items.len(), 4);
    let even = s
        .msg_list(&MessageQuery { queue: Some(queue), label: Some("even".into()), limit: 10, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(even.items.len(), 3);
}

async fn kv(s: &dyn Store, sfx: &str) {
    let k = format!("test:{sfx}");
    assert!(s.kv_get(&k).await.unwrap().is_none());
    assert!(s.kv_put(&k, &json!({"n": 1}), Expect::Absent, None).await.unwrap());
    assert!(!s.kv_put(&k, &json!({"n": 2}), Expect::Absent, None).await.unwrap());
    let v = s.kv_get(&k).await.unwrap().unwrap();
    assert_eq!(v.value, json!({"n": 1}));
    assert!(s.kv_put(&k, &json!({"n": 2}), Expect::Version(v.version), None).await.unwrap());
    assert!(!s.kv_put(&k, &json!({"n": 3}), Expect::Version(v.version), None).await.unwrap(), "stale CAS");
    assert!(!s.kv_put(&format!("{k}:nope"), &json!(1), Expect::Version(1), None).await.unwrap());
    let v2 = s.kv_get(&k).await.unwrap().unwrap();
    assert!(v2.version > v.version);
    assert!(!s.kv_delete(&k, Expect::Version(v.version)).await.unwrap());
    assert!(s.kv_delete(&k, Expect::Version(v2.version)).await.unwrap());
    assert!(s.kv_get(&k).await.unwrap().is_none());

    // Expired records read as absent and can be claimed again.
    let e = format!("test:{sfx}:exp");
    assert!(s.kv_put(&e, &json!(1), Expect::Absent, Some(now_ms() - 1)).await.unwrap());
    assert!(s.kv_get(&e).await.unwrap().is_none());
    assert!(s.kv_put(&e, &json!(2), Expect::Absent, Some(now_ms() + 60_000)).await.unwrap());
    assert_eq!(s.kv_get(&e).await.unwrap().unwrap().value, json!(2));
    assert!(s.kv_put(&e, &json!(3), Expect::Any, None).await.unwrap());
    assert!(s.kv_delete(&e, Expect::Any).await.unwrap());
}

async fn docs(s: &dyn Store, sfx: &str) {
    let kind = format!("test{sfx}");
    assert!(s.doc_put(&kind, "b", &json!({"x": 1}), Expect::Absent).await.unwrap());
    assert!(!s.doc_put(&kind, "b", &json!({"x": 2}), Expect::Absent).await.unwrap());
    assert!(s.doc_put(&kind, "a", &json!({"x": 0}), Expect::Any).await.unwrap());
    let b = s.doc_get(&kind, "b").await.unwrap().unwrap();
    assert!(s.doc_put(&kind, "b", &json!({"x": 2}), Expect::Version(b.version)).await.unwrap());
    assert!(!s.doc_put(&kind, "b", &json!({"x": 3}), Expect::Version(b.version)).await.unwrap());
    assert!(s.doc_put(&kind, "b", &json!({"x": 4}), Expect::Any).await.unwrap());
    let all = s.doc_list(&kind).await.unwrap();
    assert_eq!(all.iter().map(|d| d.value.clone()).collect::<Vec<_>>(), vec![json!({"x": 0}), json!({"x": 4})]);
    assert!(s.doc_delete(&kind, "a").await.unwrap());
    assert!(!s.doc_delete(&kind, "a").await.unwrap());
    assert_eq!(s.doc_list(&kind).await.unwrap().len(), 1);
    assert!(s.doc_get(&kind, "a").await.unwrap().is_none());
    s.doc_delete(&kind, "b").await.unwrap();
}

async fn runs(s: &dyn Store, sfx: &str) {
    let f = format!("fn{sfx}");
    let mut ids = Vec::new();
    for i in 0..3 {
        let run = Run {
            id: new_id("run"),
            function_id: f.clone(),
            app_id: "app".into(),
            status: if i == 2 { RunStatus::Completed } else { RunStatus::Queued },
            events: vec![json!({"name": "e"})],
            output: None,
            error: None,
            created_at: now_ms(),
            started_at: None,
            ended_at: None,
            invocation: None,
            dirty: false,
            pending: vec![],
            step_attempts: Default::default(),
            parent: None,
            keys: Default::default(),
            invocations: 0,
            version: 0,
            expires_at: None,
        };
        assert!(s.run_put(&run, Expect::Absent).await.unwrap());
        assert!(!s.run_put(&run, Expect::Absent).await.unwrap());
        ids.push(run.id);
    }
    let mut r = s.run_get(&ids[0]).await.unwrap().unwrap();
    assert_eq!(r.version, 1);
    r.status = RunStatus::Running;
    assert!(s.run_put(&r, Expect::Version(1)).await.unwrap());
    assert!(!s.run_put(&r, Expect::Version(1)).await.unwrap());
    let r = s.run_get(&ids[0]).await.unwrap().unwrap();
    assert_eq!((r.status, r.version), (RunStatus::Running, 2));

    let page = s.run_list(&RunQuery { function_id: Some(f.clone()), limit: 10, ..Default::default() }).await.unwrap();
    assert_eq!(page.items.iter().map(|r| r.id.clone()).rev().collect::<Vec<_>>(), ids);
    let page = s
        .run_list(&RunQuery { function_id: Some(f.clone()), status: Some(RunStatus::Completed), limit: 10, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    let p1 = s.run_list(&RunQuery { function_id: Some(f.clone()), limit: 2, ..Default::default() }).await.unwrap();
    assert!(p1.cursor.is_some());
    let p2 = s.run_list(&RunQuery { function_id: Some(f), limit: 2, cursor: p1.cursor, ..Default::default() }).await.unwrap();
    assert_eq!(p2.items.len(), 1);

    for (i, id) in ["b", "a"].iter().enumerate() {
        s.step_put(&Step {
            run_id: ids[0].clone(),
            id: id.to_string(),
            name: None,
            op: "run".into(),
            data: Some(json!(i)),
            error: None,
            at: now_ms() + i as i64,
            attempts: 1,
        })
        .await
        .unwrap();
    }
    s.step_put(&Step {
        run_id: ids[0].clone(),
        id: "b".into(),
        name: None,
        op: "run".into(),
        data: Some(json!("again")),
        error: None,
        at: now_ms(),
        attempts: 2,
    })
    .await
    .unwrap();
    let steps = s.steps(&ids[0]).await.unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps.iter().find(|s| s.id == "b").unwrap().data, Some(json!("again")));
}

async fn events_and_waits(s: &dyn Store, sfx: &str) {
    let name = format!("test/{sfx}");
    let mut ids = Vec::new();
    for i in 0..3 {
        let e = Event {
            id: new_id("evt"),
            name: name.clone(),
            dedup_id: None,
            data: json!({"i": i}),
            user: None,
            ts: now_ms(),
            runs: vec![],
            expires_at: None,
        };
        s.event_put(&e).await.unwrap();
        ids.push(e.id);
    }
    let got = s.event_get(&ids[1]).await.unwrap().unwrap();
    assert_eq!(got.data, json!({"i": 1}));
    let page = s.event_list(&EventQuery { name: Some(name.clone()), limit: 2, ..Default::default() }).await.unwrap();
    assert_eq!(page.items[0].id, ids[2]);
    assert!(page.cursor.is_some());

    let w = |id: &str, key: Option<&str>| Wait {
        id: id.into(),
        event: name.clone(),
        key: key.map(String::from),
        kind: WaitKind::Step,
        run_id: "run_x".into(),
        step_id: Some("s".into()),
        if_: None,
        origin: json!({}),
        expires_at: now_ms() + 60_000,
    };
    s.wait_put(&w("w1", None)).await.unwrap();
    s.wait_put(&w("w2", Some("data.id=\"7\""))).await.unwrap();
    s.wait_put(&w("w3", Some("data.id=\"7\""))).await.unwrap();
    let mut expired = w("w4", None);
    expired.expires_at = now_ms() - 1;
    s.wait_put(&expired).await.unwrap();
    assert_eq!(s.waits(&name, None).await.unwrap().iter().map(|w| w.id.clone()).collect::<Vec<_>>(), vec!["w1"]);
    assert_eq!(s.waits(&name, Some("data.id=\"7\"")).await.unwrap().len(), 2);
    assert!(s.waits(&name, Some("data.id=\"8\"")).await.unwrap().is_empty());
    s.wait_delete(&w("w2", Some("data.id=\"7\""))).await.unwrap();
    assert_eq!(s.waits(&name, Some("data.id=\"7\"")).await.unwrap().len(), 1);
}

async fn usage(s: &dyn Store) {
    let day = 1_000_000 + (crate::ids::random_u128() % 1_000_000) as i64;
    s.usage_add(day, &Usage { published: 2, bytes_in: 10, ..Default::default() }).await.unwrap();
    s.usage_add(day, &Usage { published: 3, delivered: 1, ..Default::default() }).await.unwrap();
    s.usage_add(day + 1, &Usage { events: 7, ..Default::default() }).await.unwrap();
    let u = s.usage(day).await.unwrap();
    let u: Vec<_> = u.into_iter().filter(|(d, _)| *d == day || *d == day + 1).collect();
    assert_eq!(u.len(), 2);
    assert_eq!(u[0].1, Usage { published: 5, delivered: 1, bytes_in: 10, ..Default::default() });
    assert_eq!(u[1].1.events, 7);
}
