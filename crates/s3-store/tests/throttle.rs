//! The throttle of failed attempts (wrong MFA codes, wrong sign-in passwords): durable through
//! the metadata log (it survives a restart and holds across a takeover), counted with relaxed
//! writes, and costing a success no write when there's nothing to forget.

mod common;

use common::*;
use s3_store::LogStats;
use std::time::Duration;

const MAX: u32 = 10;
const WINDOW: i64 = 300_000;

/// Until the relaxed writes so far are committed to object storage.
async fn committed(s: &s3_store::Store, before: &LogStats) {
    for _ in 0..500 {
        if s.stats().unwrap().log.commits > before.commits {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the relaxed writes were never committed");
}

#[tokio::test]
async fn failures_survive_a_restart_and_a_takeover() {
    let (backend, clock, s) = env("store-throttle").await;
    let a = s.default_account_id();
    let key = "mfa:alice";
    for _ in 0..MAX - 1 {
        s.record_failure(&a, key, WINDOW).unwrap();
    }
    assert!(!s.throttled(&a, key, MAX, WINDOW).unwrap());
    let before = s.stats().unwrap().log;
    s.record_failure(&a, key, WINDOW).unwrap();
    assert!(s.throttled(&a, key, MAX, WINDOW).unwrap(), "the tenth failure locks it");
    assert!(!s.throttled(&a, "mfa:bob", MAX, WINDOW).unwrap(), "only that key");
    committed(&s, &before).await;

    // A takeover: another node opens the log (the first never closed) and has the count.
    let s2 = open(&backend, &clock).await;
    assert!(s2.throttled(&a, key, MAX, WINDOW).unwrap(), "a takeover keeps it");
    // The window passes; a new one starts with the next failure.
    clock.advance(WINDOW);
    assert!(!s2.throttled(&a, key, MAX, WINDOW).unwrap());
    let before = s2.stats().unwrap().log;
    for _ in 0..MAX {
        s2.record_failure(&a, key, WINDOW).unwrap();
    }
    committed(&s2, &before).await;
    // A restart (clean this time) keeps it too.
    s2.close().await;
    let s3 = open(&backend, &clock).await;
    assert!(s3.throttled(&a, key, MAX, WINDOW).unwrap(), "a restart keeps it");

    // A success forgets it; with nothing to forget, it writes nothing at all.
    s3.clear_failures(&a, key).unwrap();
    assert!(!s3.throttled(&a, key, MAX, WINDOW).unwrap());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let quiet = s3.stats().unwrap().log;
    for _ in 0..20 {
        s3.clear_failures(&a, key).unwrap();
        assert!(!s3.throttled(&a, key, MAX, WINDOW).unwrap());
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(s3.stats().unwrap().log.seq, quiet.seq, "checks and successes without failures write nothing");
    // Accounts don't share counts.
    let other = s3.create_account("other", None).await.unwrap();
    s3.record_failure(&other.id, key, WINDOW).unwrap();
    assert!(!s3.throttled(&a, key, 1, WINDOW).unwrap());
    assert!(s3.throttled(&other.id, key, 1, WINDOW).unwrap());
}
