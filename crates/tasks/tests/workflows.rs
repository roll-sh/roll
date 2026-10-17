//! Durable workflows: steps, sleeps, events, invokes, retries, cancellation and flow control.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::{Value, json};
use std::time::Duration;
use tasks::model::now_ms;

fn run_id(v: &Value) -> String {
    v["run_id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn steps_are_memoised_and_run_once() {
    let env = env().await;
    let app = MockApp::start("shop").await;
    app.function(json!({ "id": "checkout", "triggers": [{ "event": "order.created" }] }), |ctx| {
        let order = ctx.event["data"]["order"].clone();
        let charge = ctx.run("charge", || Ok(json!({ "charged": 42 })))?;
        ctx.effect("after-charge");
        let email = ctx.run("email", || Ok(json!("sent")))?;
        Ok(json!({ "order": order, "charge": charge, "email": email }))
    });
    let synced = app.sync(&env).await;
    assert_eq!(synced["functions"][0]["id"], "checkout");
    assert_eq!(synced["sdk"], "mock");

    let sent = env.post("/events", json!({ "name": "order.created", "data": { "order": 7 } })).await;
    let event_id = sent["ids"][0].as_str().unwrap().to_string();
    eventually(5000, "a run", || async { !env.runs("checkout").await.is_empty() }).await;
    let run = env.runs("checkout").await.remove(0);
    let run = env.wait_run(run["id"].as_str().unwrap(), 5000).await;
    assert_eq!(run["status"], "completed");
    assert_eq!(run["output"], json!({ "order": 7, "charge": { "charged": 42 }, "email": "sent" }));
    let steps = run["steps"].as_array().unwrap();
    assert_eq!(steps.iter().map(|s| s["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["charge", "email"]);
    // One call per step plus the final one; code after a step runs on every later call.
    assert_eq!(app.calls("checkout").len(), 3);
    assert_eq!(app.effects("after-charge"), 2);
    let ev = env.get(&format!("/events/{event_id}")).await;
    assert_eq!(ev["runs"][0], run["id"]);
    // Every call is signed and carries the run.
    let call = &app.calls("checkout")[2];
    assert_eq!(call["steps"]["charge"]["data"], json!({ "charged": 42 }));
    assert_eq!(call["event"]["name"], "order.created");
    assert_eq!(call["run_id"], run["id"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sleep_resumes_later() {
    let env = env().await;
    let app = MockApp::start("sleepy").await;
    app.function(json!({ "id": "nap", "triggers": [{ "event": "nap" }] }), |ctx| {
        let before = ctx.run("before", || Ok(json!(now_ms())))?;
        ctx.sleep("rest", 600)?;
        let after = ctx.run("after", || Ok(json!(now_ms())))?;
        Ok(json!({ "slept": after.as_i64().unwrap() - before.as_i64().unwrap() }))
    });
    app.sync(&env).await;
    let r = env.post("/functions/nap/invoke", json!({ "data": {} })).await;
    let id = run_id(&r);
    eventually(3000, "the run to sleep", || async { env.get(&format!("/runs/{id}")).await["status"] == "waiting" }).await;
    let run = env.get(&format!("/runs/{id}")).await;
    assert_eq!(run["pending"][0]["op"], "sleep");
    let run = env.wait_run(&id, 5000).await;
    assert_eq!(run["status"], "completed");
    assert!(run["output"]["slept"].as_i64().unwrap() >= 590, "{run}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_for_event_matches_and_times_out() {
    let env = env().await;
    let app = MockApp::start("approvals").await;
    app.function(json!({ "id": "approve", "triggers": [{ "event": "invoice.created" }] }), |ctx| {
        let paid = ctx.wait_for_event("paid", "invoice.paid", Some("data.invoice"), None, 1500)?;
        Ok(json!({ "paid": paid }))
    });
    app.function(json!({ "id": "big", "triggers": [{ "event": "deal.created" }] }), |ctx| {
        let e = ctx.wait_for_event("signed", "deal.signed", None, Some("async.data.amount >= event.data.min"), 3000)?;
        Ok(json!(e["data"]["amount"]))
    });
    app.sync(&env).await;
    env.post("/events", json!([
        { "name": "invoice.created", "data": { "invoice": "A" } },
        { "name": "invoice.created", "data": { "invoice": "B" } },
    ]))
    .await;
    eventually(5000, "two waiting runs", || async {
        env.runs("approve").await.iter().filter(|r| r["status"] == "waiting").count() == 2
    })
    .await;
    // An event for another invoice doesn't match; then A's arrives.
    env.post("/events", json!({ "name": "invoice.paid", "data": { "invoice": "Z" } })).await;
    env.post("/events", json!({ "name": "invoice.paid", "data": { "invoice": "A", "amount": 10 } })).await;
    let runs = env.runs("approve").await;
    let by_invoice = |inv: &str| runs.iter().find(|r| r["events"][0]["data"]["invoice"] == inv).unwrap()["id"].as_str().unwrap().to_string();
    let a = env.wait_run(&by_invoice("A"), 5000).await;
    assert_eq!(a["output"]["paid"]["data"], json!({ "invoice": "A", "amount": 10 }));
    // B times out with null.
    let b = env.wait_run(&by_invoice("B"), 5000).await;
    assert_eq!(b["status"], "completed");
    assert_eq!(b["output"]["paid"], Value::Null);

    // `if` expressions compare the run's event with the incoming one.
    env.post("/events", json!({ "name": "deal.created", "data": { "min": 100 } })).await;
    eventually(5000, "a waiting deal", || async { env.runs("big").await.first().is_some_and(|r| r["status"] == "waiting") }).await;
    env.post("/events", json!({ "name": "deal.signed", "data": { "amount": 50 } })).await;
    env.post("/events", json!({ "name": "deal.signed", "data": { "amount": 150 } })).await;
    let id = env.runs("big").await[0]["id"].as_str().unwrap().to_string();
    assert_eq!(env.wait_run(&id, 5000).await["output"], 150);
}

#[tokio::test(flavor = "multi_thread")]
async fn invoke_returns_the_child_result() {
    let env = env().await;
    let app = MockApp::start("math").await;
    app.function(json!({ "id": "double", "triggers": [] }), |ctx| {
        let n = ctx.event["data"]["n"].as_i64().unwrap();
        let v = ctx.run("calc", || Ok(json!(n * 2)))?;
        Ok(v)
    });
    app.function(json!({ "id": "fails", "retries": 0, "triggers": [] }), |_| Err(Stop::Error { message: "nope".into(), retriable: false }));
    app.function(json!({ "id": "parent", "triggers": [{ "event": "calc" }] }), |ctx| {
        let a = ctx.invoke("a", "double", json!({ "n": 21 }), None)?;
        let b = ctx.invoke("b", "fails", json!({}), None)?;
        Ok(json!({ "a": a.ok(), "b_failed": b.is_err(), "b_error": b.err() }))
    });
    app.sync(&env).await;
    env.post("/events", json!({ "name": "calc" })).await;
    eventually(5000, "parent run", || async { !env.runs("parent").await.is_empty() }).await;
    let id = env.runs("parent").await[0]["id"].as_str().unwrap().to_string();
    let run = env.wait_run(&id, 8000).await;
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["output"]["a"], 42);
    assert_eq!(run["output"]["b_failed"], true);
    assert_eq!(run["output"]["b_error"]["message"], "nope");
    let child = env.runs("double").await.remove(0);
    assert_eq!(child["parent"][0], id.as_str());
    assert_eq!(child["events"][0]["name"], "roll/function.invoked");
}

#[tokio::test(flavor = "multi_thread")]
async fn send_event_starts_other_functions() {
    let env = env().await;
    let app = MockApp::start("chain").await;
    app.function(json!({ "id": "first", "triggers": [{ "event": "start" }] }), |ctx| {
        let sent = ctx.send_event("emit", json!([{ "name": "next", "data": { "from": "first" } }]))?;
        Ok(sent)
    });
    app.function(json!({ "id": "second", "triggers": [{ "event": "next" }] }), |ctx| Ok(ctx.event["data"].clone()));
    app.sync(&env).await;
    env.post("/events", json!({ "name": "start" })).await;
    eventually(5000, "second to run", || async {
        env.runs("second").await.first().is_some_and(|r| r["status"] == "completed")
    })
    .await;
    assert_eq!(env.runs("second").await[0]["output"], json!({ "from": "first" }));
    let first = env.wait_run(env.runs("first").await[0]["id"].as_str().unwrap(), 3000).await;
    assert_eq!(first["output"]["ids"].as_array().unwrap().len(), 1);
    assert_eq!(env.runs("second").await.len(), 1, "sent once");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_steps_in_one_call() {
    let env = env().await;
    let app = MockApp::start("fan").await;
    app.function(json!({ "id": "fan", "triggers": [{ "event": "fan" }] }), |ctx| {
        let parts = ctx.parallel(vec![
            ("a", Box::new(|| json!(1))),
            ("b", Box::new(|| json!(2))),
            ("c", Box::new(|| json!(3))),
        ])?;
        Ok(json!(parts.iter().map(|v| v.as_i64().unwrap()).sum::<i64>()))
    });
    app.sync(&env).await;
    let r = env.post("/functions/fan/invoke", json!({ "wait": "5s" })).await;
    assert_eq!(r["status"], "completed");
    assert_eq!(r["output"], 6);
    assert_eq!(app.calls("fan").len(), 2, "three steps reported together");
}

#[tokio::test(flavor = "multi_thread")]
async fn step_retries_then_surface_the_error() {
    let env = env().await;
    let app = MockApp::start("retry").await;
    app.function(json!({ "id": "flaky", "retries": 2, "triggers": [{ "event": "go" }] }), |ctx| {
        ctx.run("before", || Ok(json!(1)))?;
        // Always fails: retried twice, then the error is handed to the function.
        match ctx.try_run("unstable", || Err("boom".to_string()))? {
            Ok(_) => Ok(json!("ok")),
            Err(e) => Ok(json!({ "handled": e["message"] })),
        }
    });
    app.sync(&env).await;
    let r = env.post("/functions/flaky/invoke", json!({ "wait": "8s" })).await;
    assert_eq!(r["status"], "completed", "{r}");
    assert_eq!(r["output"], json!({ "handled": "boom" }));
    let run = env.get(&format!("/runs/{}", r["id"].as_str().unwrap())).await;
    let step = run["steps"].as_array().unwrap().iter().find(|s| s["id"] == "unstable").unwrap().clone();
    assert_eq!(step["attempts"], 3, "first try + 2 retries");
    assert_eq!(run["step_attempts"]["unstable"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn step_backoff_holds_even_when_a_sibling_step_succeeds() {
    let env = env().await;
    let app = MockApp::start("pace").await;
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
    let seen = calls.clone();
    // Fixed 600ms backoff from the function definition (the server default is 50ms in tests).
    let def = json!({ "id": "pair", "retries": 2, "backoff": { "strategy": "fixed", "base_ms": 600, "max_ms": 600, "jitter": 0 }, "triggers": [] });
    app.function(def, move |ctx| {
        if let Some(Err(e)) = ctx.memo("bad") {
            return Ok(json!({ "handled": e["message"] }));
        }
        seen.lock().unwrap().push(now_ms());
        // Both steps run in one call; "ok" succeeds the first time, "bad" always fails.
        let mut ops = Vec::new();
        if ctx.memo("ok").is_none() {
            ops.push(json!({ "op": "run", "id": "ok", "data": 1 }));
        }
        ops.push(json!({ "op": "run", "id": "bad", "error": { "name": "Error", "message": "boom" } }));
        Err(Stop::Ops(ops))
    });
    app.sync(&env).await;
    let r = env.post("/functions/pair/invoke", json!({ "wait": "8s" })).await;
    assert_eq!(r["output"], json!({ "handled": "boom" }), "{r}");
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3, "first try + 2 retries");
    for gap in calls.windows(2).map(|w| w[1] - w[0]) {
        assert!(gap >= 550, "each retry waits out its backoff: {calls:?}");
    }
    // Bad backoff settings are rejected at sync.
    let bad = MockApp::start("bad-backoff").await;
    bad.function(json!({ "id": "nope", "backoff": { "base_ms": 10, "max_ms": 5 }, "triggers": [] }), |_| Ok(json!(0)));
    let (s, _) = env.call(Method::POST, "/apps", Some(json!({ "url": bad.url }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_retry_the_call_then_fail_the_run_and_notify() {
    let env = env().await;
    let app = MockApp::start("fail").await;
    app.function(json!({ "id": "broken", "retries": 2, "triggers": [{ "event": "break" }] }), |_| {
        Err(Stop::Error { message: "kaput".into(), retriable: true })
    });
    app.function(json!({ "id": "on-failure", "triggers": [{ "event": "roll/function.failed", "if": "event.data.function_id == 'broken'" }] }), |ctx| {
        Ok(ctx.event["data"]["error"].clone())
    });
    app.sync(&env).await;
    env.post("/events", json!({ "name": "break" })).await;
    eventually(8000, "the failure handler", || async {
        env.runs("on-failure").await.first().is_some_and(|r| r["status"] == "completed")
    })
    .await;
    let failed = env.runs("broken").await.remove(0);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["message"], "kaput");
    assert_eq!(app.calls("broken").len(), 3, "first call + 2 retries");
    assert_eq!(env.runs("on-failure").await[0]["output"]["message"], "kaput");

    // Non-retriable errors fail at once.
    app.function(json!({ "id": "fatal", "retries": 5, "triggers": [] }), |_| Err(Stop::Error { message: "fatal".into(), retriable: false }));
    app.sync(&env).await;
    let r = env.post("/functions/fatal/invoke", json!({ "wait": "5s" })).await;
    assert_eq!(r["status"], "failed");
    assert_eq!(app.calls("fatal").len(), 1);

    // Invoked runs fail the same way.
    let r = env.post("/functions/broken/invoke", json!({})).await;
    let id = run_id(&r);
    let run = env.wait_run(&id, 8000).await;
    assert_eq!(run["status"], "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_on_events_and_by_hand() {
    let env = env().await;
    let app = MockApp::start("cancel").await;
    app.function(
        json!({ "id": "remind", "triggers": [{ "event": "cart.updated" }], "cancel_on": [{ "event": "cart.checkout", "match": "data.cart" }] }),
        |ctx| {
            ctx.sleep("wait", 60_000)?;
            Ok(json!("reminded"))
        },
    );
    app.sync(&env).await;
    env.post("/events", json!([{ "name": "cart.updated", "data": { "cart": 1 } }, { "name": "cart.updated", "data": { "cart": 2 } }])).await;
    eventually(5000, "two sleeping runs", || async {
        env.runs("remind").await.iter().filter(|r| r["status"] == "waiting").count() == 2
    })
    .await;
    env.post("/events", json!({ "name": "cart.checkout", "data": { "cart": 1 } })).await;
    let runs = env.runs("remind").await;
    let one = runs.iter().find(|r| r["events"][0]["data"]["cart"] == 1).unwrap();
    let two = runs.iter().find(|r| r["events"][0]["data"]["cart"] == 2).unwrap();
    let one = env.wait_run(one["id"].as_str().unwrap(), 3000).await;
    assert_eq!(one["status"], "canceled");
    assert!(one["error"]["message"].as_str().unwrap().contains("cart.checkout"));
    assert_eq!(env.get(&format!("/runs/{}", two["id"].as_str().unwrap())).await["status"], "waiting");

    let c = env.post(&format!("/runs/{}/cancel", two["id"].as_str().unwrap()), json!({})).await;
    assert_eq!(c["status"], "canceled");
    let (s, _) = env.call(Method::POST, &format!("/runs/{}/cancel", two["id"].as_str().unwrap()), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Bulk cancel with a filter.
    env.post("/events", json!([{ "name": "cart.updated", "data": { "cart": 3 } }, { "name": "cart.updated", "data": { "cart": 4 } }])).await;
    eventually(5000, "new runs", || async { env.runs("remind").await.iter().filter(|r| r["status"] == "waiting").count() == 2 }).await;
    let r = env.post("/functions/remind/cancel", json!({ "if": "event.data.cart == 3" })).await;
    assert_eq!(r["canceled"], 1);
    let r = env.post("/functions/remind/cancel", json!({})).await;
    assert_eq!(r["canceled"], 1);
    assert!(env.runs("remind").await.iter().all(|r| r["status"] == "canceled"));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrency_limits_runs() {
    let env = env().await;
    let app = MockApp::start("conc").await;
    let active = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let a2 = active.clone();
    app.function(json!({ "id": "one-at-a-time", "concurrency": [{ "limit": 1 }], "triggers": [{ "event": "job" }] }), move |_| {
        {
            let mut a = a2.lock().unwrap();
            a.0 += 1;
            a.1 = a.1.max(a.0);
        }
        std::thread::sleep(Duration::from_millis(60));
        a2.lock().unwrap().0 -= 1;
        Ok(json!("done"))
    });
    let keyed = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<String, (usize, usize)>::new()));
    let k2 = keyed.clone();
    app.function(
        json!({ "id": "per-user", "concurrency": [{ "limit": 1, "key": "event.data.user" }], "triggers": [{ "event": "user.job" }] }),
        move |ctx| {
            let user = ctx.event["data"]["user"].as_str().unwrap().to_string();
            {
                let mut m = k2.lock().unwrap();
                let e = m.entry(user.clone()).or_default();
                e.0 += 1;
                e.1 = e.1.max(e.0);
            }
            std::thread::sleep(Duration::from_millis(60));
            k2.lock().unwrap().get_mut(&user).unwrap().0 -= 1;
            Ok(json!(user))
        },
    );
    app.sync(&env).await;
    let events: Vec<Value> = (0..5).map(|_| json!({ "name": "job" })).collect();
    env.post("/events", json!(events)).await;
    let events: Vec<Value> = (0..6).map(|i| json!({ "name": "user.job", "data": { "user": if i % 2 == 0 { "ann" } else { "bob" } } })).collect();
    env.post("/events", json!(events)).await;
    eventually(10_000, "all runs", || async {
        env.runs("one-at-a-time").await.iter().filter(|r| r["status"] == "completed").count() == 5
            && env.runs("per-user").await.iter().filter(|r| r["status"] == "completed").count() == 6
    })
    .await;
    assert_eq!(active.lock().unwrap().1, 1);
    let k = keyed.lock().unwrap();
    assert_eq!(k["ann"].1, 1);
    assert_eq!(k["bob"].1, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn debounce_keeps_the_last_event() {
    let env = env().await;
    let app = MockApp::start("deb").await;
    app.function(
        json!({ "id": "sync-profile", "debounce": { "period_ms": 400, "key": "event.data.user" }, "triggers": [{ "event": "profile.changed" }] }),
        |ctx| Ok(ctx.event["data"]["v"].clone()),
    );
    app.sync(&env).await;
    for v in 1..=4 {
        env.post("/events", json!({ "name": "profile.changed", "data": { "user": "u1", "v": v } })).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    env.post("/events", json!({ "name": "profile.changed", "data": { "user": "u2", "v": 99 } })).await;
    eventually(5000, "debounced runs", || async {
        env.runs("sync-profile").await.iter().filter(|r| r["status"] == "completed").count() == 2
    })
    .await;
    let runs = env.runs("sync-profile").await;
    let mut outputs: Vec<i64> = runs.iter().map(|r| r["output"].as_i64().unwrap()).collect();
    outputs.sort();
    assert_eq!(outputs, vec![4, 99]);
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_by_size_and_timeout() {
    let env = env().await;
    let app = MockApp::start("batch").await;
    app.function(
        json!({ "id": "bulk-insert", "batch": { "max_size": 3, "timeout_ms": 500 }, "triggers": [{ "event": "row" }] }),
        |ctx| Ok(json!(ctx.events.iter().map(|e| e["data"]["n"].clone()).collect::<Vec<_>>())),
    );
    app.sync(&env).await;
    let rows: Vec<Value> = (1..=4).map(|n| json!({ "name": "row", "data": { "n": n } })).collect();
    env.post("/events", json!(rows)).await;
    eventually(5000, "two batches", || async {
        env.runs("bulk-insert").await.iter().filter(|r| r["status"] == "completed").count() == 2
    })
    .await;
    let mut outs: Vec<Value> = env.runs("bulk-insert").await.iter().map(|r| r["output"].clone()).collect();
    outs.sort_by_key(|o| o.as_array().unwrap().len());
    assert_eq!(outs, vec![json!([4]), json!([1, 2, 3])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn idempotency_rate_limits_and_trigger_filters() {
    let env = env().await;
    let app = MockApp::start("limits").await;
    app.function(json!({ "id": "welcome", "idempotency": "event.data.user", "triggers": [{ "event": "signup" }] }), |_| Ok(json!(1)));
    app.function(
        json!({ "id": "limited", "rate_limit": { "limit": 2, "period_ms": 60000 }, "triggers": [{ "event": "ping" }] }),
        |_| Ok(json!(1)),
    );
    app.function(json!({ "id": "vip", "triggers": [{ "event": "signup", "if": "event.data.plan == 'pro'" }] }), |_| Ok(json!(1)));
    app.function(json!({ "id": "all-signups", "triggers": [{ "event": "sign*" }] }), |_| Ok(json!(1)));
    app.sync(&env).await;
    env.post("/events", json!([
        { "name": "signup", "data": { "user": "a", "plan": "free" } },
        { "name": "signup", "data": { "user": "a", "plan": "pro" } },
        { "name": "signup", "data": { "user": "b", "plan": "pro" } },
    ]))
    .await;
    for _ in 0..4 {
        env.post("/events", json!({ "name": "ping" })).await;
    }
    // Event-level dedup by id.
    env.post("/events", json!({ "name": "ping", "id": "same" })).await;
    let dup = env.post("/events", json!({ "name": "ping", "id": "same" })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.runs("welcome").await.len(), 2, "one per user");
    assert_eq!(env.runs("limited").await.len(), 2, "rate limited to 2");
    assert_eq!(env.runs("vip").await.len(), 2, "only pro signups");
    assert_eq!(env.runs("all-signups").await.len(), 3, "wildcard trigger");
    assert_eq!(env.get("/events?name=ping").await["items"].as_array().unwrap().len(), 5, "the duplicate isn't stored");
    assert!(dup["ids"][0].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn singleton_skip_and_cancel() {
    let env = env().await;
    let app = MockApp::start("single").await;
    app.function(json!({ "id": "skip", "singleton": { "mode": "skip" }, "triggers": [{ "event": "s" }] }), |ctx| {
        ctx.sleep("hold", 400)?;
        Ok(json!(ctx.event["data"]["n"]))
    });
    app.function(json!({ "id": "replace", "singleton": { "mode": "cancel", "key": "event.data.k" }, "triggers": [{ "event": "s" }] }), |ctx| {
        ctx.sleep("hold", 400)?;
        Ok(json!(ctx.event["data"]["n"]))
    });
    app.sync(&env).await;
    env.post("/events", json!({ "name": "s", "data": { "n": 1, "k": "x" } })).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    env.post("/events", json!({ "name": "s", "data": { "n": 2, "k": "x" } })).await;
    eventually(5000, "runs to finish", || async {
        env.runs("skip").await.iter().all(|r| r["status"] != "waiting" && r["status"] != "running" && r["status"] != "queued")
            && env.runs("replace").await.iter().all(|r| ["completed", "canceled"].contains(&r["status"].as_str().unwrap()))
    })
    .await;
    let skip = env.runs("skip").await;
    assert_eq!(skip.iter().filter(|r| r["status"] == "completed").count(), 1);
    assert_eq!(skip.iter().find(|r| r["status"] == "completed").unwrap()["output"], 1);
    let replace = env.runs("replace").await;
    let done: Vec<_> = replace.iter().filter(|r| r["status"] == "completed").collect();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["output"], 2, "the newer run wins");
}

#[tokio::test(flavor = "multi_thread")]
async fn throttle_delays_runs() {
    let env = env().await;
    let app = MockApp::start("throttle").await;
    app.function(json!({ "id": "slowly", "throttle": { "limit": 2, "period_ms": 1000 }, "triggers": [{ "event": "t" }] }), |_| Ok(json!(now_ms())));
    app.sync(&env).await;
    let start = now_ms();
    let events: Vec<Value> = (0..4).map(|_| json!({ "name": "t" })).collect();
    env.post("/events", json!(events)).await;
    eventually(6000, "all throttled runs", || async {
        env.runs("slowly").await.iter().filter(|r| r["status"] == "completed").count() == 4
    })
    .await;
    let last = env.runs("slowly").await.iter().map(|r| r["output"].as_i64().unwrap()).max().unwrap();
    assert!(last - start >= 900, "4 runs at 2/s finished after {}ms", last - start);
}

#[tokio::test(flavor = "multi_thread")]
async fn cron_triggers_and_timeouts() {
    let env = env().await;
    let app = MockApp::start("cron").await;
    app.function(json!({ "id": "every-second", "triggers": [{ "cron": "* * * * * *" }] }), |ctx| Ok(ctx.event["name"].clone()));
    app.function(json!({ "id": "slow-run", "timeouts": { "finish_ms": 500 }, "triggers": [{ "event": "slow" }] }), |ctx| {
        ctx.sleep("forever", 60_000)?;
        Ok(json!(1))
    });
    app.sync(&env).await;
    let s = env.get("/schedules/fn-every-second-0").await;
    assert_eq!(s["target"]["function_id"], "every-second");
    eventually(4000, "a cron run", || async { env.runs("every-second").await.iter().any(|r| r["status"] == "completed") }).await;
    let r = env.runs("every-second").await.into_iter().find(|r| r["status"] == "completed").unwrap();
    assert_eq!(r["output"], "roll/scheduled.timer");

    env.post("/events", json!({ "name": "slow" })).await;
    eventually(5000, "the run to time out", || async { env.runs("slow-run").await.first().is_some_and(|r| r["status"] == "canceled") }).await;
    assert_eq!(env.runs("slow-run").await[0]["error"]["name"], "FinishTimeout");

    // Pausing a function pauses its cron and its queue.
    env.post("/functions/every-second/pause", json!({})).await;
    assert_eq!(env.get("/schedules/fn-every-second-0").await["paused"], true);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let n = env.runs("every-second").await.len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(env.runs("every-second").await.len(), n);
    let r = env.post("/functions/every-second/invoke", json!({})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.get(&format!("/runs/{}", run_id(&r))).await["status"], "queued", "paused functions queue runs");
    env.post("/functions/every-second/resume", json!({})).await;
    env.wait_run(&run_id(&r), 3000).await;

    // Removing the trigger removes the schedule.
    let app2 = MockApp::start("cron").await;
    app2.function(json!({ "id": "every-second", "triggers": [] }), |_| Ok(json!(1)));
    app2.sync(&env).await;
    let (s, _) = env.call(Method::GET, "/schedules/fn-every-second-0", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_api_rerun_wait_and_apps() {
    let env = env().await;
    let app = MockApp::start("api").await;
    app.function(json!({ "id": "echo", "triggers": [] }), |ctx| Ok(ctx.event["data"].clone()));
    app.sync(&env).await;
    let r = env.post("/functions/echo/invoke", json!({ "data": { "x": 1 }, "wait": "5s" })).await;
    assert_eq!(r["output"], json!({ "x": 1 }));
    let again = env.post(&format!("/runs/{}/rerun", r["id"].as_str().unwrap()), json!({})).await;
    assert_ne!(again["id"], r["id"]);
    let waited = env.get(&format!("/runs/{}/wait?timeout=5s", again["id"].as_str().unwrap())).await;
    assert_eq!(waited["output"], json!({ "x": 1 }));
    // Invoke with a dedup id returns the same run.
    let a = env.post("/functions/echo/invoke", json!({ "data": 1, "id": "once" })).await;
    let b = env.post("/functions/echo/invoke", json!({ "data": 2, "id": "once" })).await;
    assert_eq!(a["run_id"], b["run_id"]);

    let f = env.get("/functions/echo").await;
    assert_eq!(f["app_id"], "api");
    assert_eq!(env.get("/functions").await.as_array().unwrap().len(), 1);
    eventually(5000, "three completed runs", || async {
        env.get("/runs?function=echo&status=completed").await["items"].as_array().unwrap().len() == 3
    })
    .await;
    let (s, _) = env.call(Method::POST, "/functions/missing/invoke", Some(json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Another app can't take over a function id.
    let other = MockApp::start("thief").await;
    other.function(json!({ "id": "echo", "triggers": [] }), |_| Ok(json!(0)));
    let (s, _) = env.call(Method::POST, "/apps", Some(json!({ "url": other.url }))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Bad definitions are rejected.
    let bad = MockApp::start("bad").await;
    bad.function(json!({ "id": "x", "triggers": [{ "event": "e", "if": "event.data ==" }] }), |_| Ok(json!(0)));
    let (s, _) = env.call(Method::POST, "/apps", Some(json!({ "url": bad.url }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Apps can also push their definitions.
    let pushed = env.post("/apps", json!({ "id": "pushed", "url": app.url, "functions": [{ "id": "pushed-fn", "triggers": [{ "event": "p" }] }] })).await;
    assert_eq!(pushed["id"], "pushed");
    assert_eq!(env.get("/apps").await.as_array().unwrap().len(), 2);

    let (s, _) = env.delete("/apps/api").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = env.call(Method::GET, "/functions/echo", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_ingestion_and_events_api() {
    let env = env().await;
    let app = MockApp::start("hooks").await;
    app.function(json!({ "id": "stripe", "triggers": [{ "event": "stripe/charge.succeeded" }] }), |ctx| Ok(ctx.event["data"]["amount"].clone()));
    app.sync(&env).await;
    let resp = env
        .http
        .post(format!("{}/v1/tasks/e/stripe/charge.succeeded", env.api))
        .header("idempotency-key", "evt_1")
        .json(&json!({ "amount": 500 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    env.http.post(format!("{}/v1/tasks/e/stripe/charge.succeeded", env.api)).header("idempotency-key", "evt_1").json(&json!({ "amount": 500 })).send().await.unwrap();
    eventually(5000, "the run", || async { env.runs("stripe").await.first().is_some_and(|r| r["status"] == "completed") }).await;
    assert_eq!(env.runs("stripe").await.len(), 1);
    assert_eq!(env.runs("stripe").await[0]["output"], 500);
    let events = env.get("/events?name=stripe/charge.succeeded").await;
    assert_eq!(events["items"].as_array().unwrap().len(), 1);
    let (s, _) = env.call(Method::POST, "/events", Some(json!({ "name": "" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflows_survive_a_worker_crash() {
    let env = env().await;
    let mut b = env.node(|_| {}).await;
    let app = MockApp::start("durable").await;
    app.function(json!({ "id": "pipeline", "triggers": [{ "event": "p" }] }), |ctx| {
        let a = ctx.run("a", || {
            std::thread::sleep(Duration::from_millis(200));
            Ok(json!(1))
        })?;
        ctx.sleep("pause", 300)?;
        let b = ctx.run("b", || Ok(json!(2)))?;
        Ok(json!(a.as_i64().unwrap() + b.as_i64().unwrap()))
    });
    app.sync(&env).await;
    let events: Vec<Value> = (0..10).map(|i| json!({ "name": "p", "data": { "i": i } })).collect();
    env.post("/events", json!(events)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    b.worker.take().unwrap().kill();
    eventually(15_000, "all pipelines", || async {
        env.runs("pipeline").await.iter().filter(|r| r["status"] == "completed").count() == 10
    })
    .await;
    assert!(env.runs("pipeline").await.iter().all(|r| r["output"] == 3));
}
