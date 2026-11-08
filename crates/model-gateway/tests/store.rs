//! `gateway.db`, list prices and the standalone config file, through the public API: client
//! keys (and that only their hashes reach the disk), usage roll-ups and cost, the request log
//! and its bodies, retention, and the config file's parsing and hot reload.

use agent_proto::{Attempt, MachineId, ModelPrice, UsageBucket, UsageBy, WireFormat, gateway_limits};
use model_gateway::config;
use model_gateway::price::{cost_micros, price_of};
use model_gateway::store::{BODY_CAP, Store, keep_body};
use model_gateway::{ClientRef, Record, Usage};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;
/// 2026-11-05T00:00:00Z.
const T0: u64 = 1_793_836_800_000;

fn record(at: u64) -> Record {
    Record {
        at,
        client: None,
        project: None,
        thread: None,
        format: WireFormat::Messages,
        model: "claude-sonnet-4-5".into(),
        stream: true,
        status: 200,
        ms: 1200,
        first_byte_ms: Some(300),
        attempts: vec![attempt("anthropic", "claude-sonnet-4-5", 200)],
        usage: Usage { input: 100, output: 10, cache_read: 1000, cache_write: 50 },
        error: None,
        bodies: None,
    }
}

fn attempt(upstream: &str, model: &str, status: u16) -> Attempt {
    Attempt { upstream: upstream.into(), model: model.into(), status, ms: 100, error: (status >= 400).then(|| format!("upstream said {status}")) }
}

fn client(id: &str, machine: &str) -> Option<ClientRef> {
    Some(ClientRef { id: id.into(), machine: Some(MachineId(machine.into())) })
}

/// A price in micro-dollars per million that makes cost easy to check: $1 per token kind.
fn flat(model: &str, input: u64, output: u64, cache_read: u64, cache_write: u64) -> ModelPrice {
    ModelPrice { model: model.into(), input, output, cache_read, cache_write }
}

#[test]
fn keys_verify_until_revoked_and_a_machine_has_one() {
    let store = Store::memory().unwrap();
    let mch = MachineId("mch_laptop".into());
    let (a, key_a) = store.issue("laptop", Some(mch.clone()), T0).unwrap();
    assert!(a.id.starts_with("gwc_") && a.id.len() == 4 + 16 && a.id[4..].bytes().all(|b| b.is_ascii_hexdigit()), "{}", a.id);
    let k = key_a.expose();
    assert!(k.starts_with("rgk_") && k.len() == 4 + 43, "{} bytes", k.len());
    assert!(k[4..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    assert_eq!(format!("{key_a:?}"), "Secret(…)");

    let got = store.verify(k, T0).unwrap().unwrap();
    assert_eq!(got, ClientRef { id: a.id.clone(), machine: Some(mch.clone()) });
    // Near misses are nobody's.
    assert_eq!(store.verify(&k[..k.len() - 1], T0).unwrap(), None);
    assert_eq!(store.verify(&format!("{k}x"), T0).unwrap(), None);
    assert_eq!(store.verify(&format!("rgk_{}", "A".repeat(43)), T0).unwrap(), None);
    assert_eq!(store.verify("", T0).unwrap(), None);

    // Issuing again for the machine revokes its earlier key at once.
    let (b, key_b) = store.issue("laptop again", Some(mch.clone()), T0 + 1).unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(store.verify(k, T0 + 2).unwrap(), None);
    assert_eq!(store.verify(key_b.expose(), T0 + 2).unwrap().unwrap().id, b.id);
    // A key without a machine sits beside it.
    let (c, key_c) = store.issue("by hand", None, T0 + 3).unwrap();
    assert_eq!(store.clients().unwrap().iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), [b.id.as_str(), c.id.as_str()]);

    assert!(store.revoke(&b.id).unwrap());
    assert!(!store.revoke(&b.id).unwrap());
    assert_eq!(store.verify(key_b.expose(), T0 + 4).unwrap(), None);
    assert_eq!(store.verify(key_c.expose(), T0 + 4).unwrap().unwrap(), ClientRef { id: c.id.clone(), machine: None });
    assert_eq!(store.clients().unwrap().len(), 1);

    assert!(store.issue("  ", None, T0).unwrap_err().to_string().contains("name"));
    assert!(store.issue("two\nlines", None, T0).is_err());
}

#[test]
fn clients_are_capped() {
    let store = Store::memory().unwrap();
    for i in 0..gateway_limits::CLIENTS {
        store.issue(&format!("c{i}"), None, T0).unwrap();
    }
    let err = store.issue("one more", None, T0).unwrap_err().to_string();
    assert!(err.contains("revoke one first"), "{err}");
    // A machine's new key replaces its old one, so it fits even at the cap.
    let first = store.clients().unwrap().remove(0);
    store.revoke(&first.id).unwrap();
    let mch = Some(MachineId("mch_a".into()));
    store.issue("a", mch.clone(), T0).unwrap();
    store.issue("a", mch, T0).unwrap();
}

#[test]
fn last_used_is_written_at_most_once_a_minute_and_revocation_crosses_processes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.db");
    let store = Store::open(&path).unwrap();
    let (c, key) = store.issue("laptop", None, T0).unwrap();
    assert!(store.last_used().unwrap().is_empty());
    store.verify(key.expose(), T0 + 1000).unwrap().unwrap();
    store.verify(key.expose(), T0 + 30_000).unwrap().unwrap();
    // This process knows the newest; the file has the first touch only.
    assert_eq!(store.last_used().unwrap(), [(c.id.clone(), T0 + 30_000)]);
    let other = Store::open(&path).unwrap();
    assert_eq!(other.last_used().unwrap(), [(c.id.clone(), T0 + 1000)]);
    store.verify(key.expose(), T0 + 61_000).unwrap().unwrap();
    assert_eq!(other.last_used().unwrap(), [(c.id.clone(), T0 + 61_000)]);

    // `roll gateway client revoke` is another process on the same file: the server's next
    // lookup refuses the key.
    assert!(other.revoke(&c.id).unwrap());
    assert_eq!(store.verify(key.expose(), T0 + 62_000).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn the_database_is_private_and_holds_only_hashes() {
    use sha2::Digest;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data").join("gateway.db");
    let store = Store::open(&path).unwrap();
    let mut keys = Vec::new();
    for i in 0..5 {
        let (c, key) = store.issue(&format!("m{i}"), Some(MachineId(format!("mch_{i}"))), T0).unwrap();
        store.verify(key.expose(), T0 + 70_000).unwrap().unwrap();
        store.record(&Record { client: Some(ClientRef { id: c.id, machine: None }), ..record(T0) }, &[], false).unwrap();
        keys.push(key.expose().to_string());
    }
    let files: Vec<_> = ["", "-wal", "-shm"].iter().map(|s| dir.path().join("data").join(format!("gateway.db{s}"))).collect();
    for f in &files {
        let mode = std::fs::metadata(f).unwrap_or_else(|e| panic!("{}: {e}", f.display())).permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", f.display());
    }
    let mut bytes = Vec::new();
    for f in &files {
        bytes.extend(std::fs::read(f).unwrap());
    }
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    for key in &keys {
        assert!(!has(key.as_bytes()), "a key is on disk");
        assert!(!has(&key.as_bytes()[4..24]), "part of a key is on disk");
        // The scan does see what is there.
        assert!(has(hex::encode(sha2::Sha256::digest(key.as_bytes())).as_bytes()), "its hash isn't on disk");
    }
    drop(store);

    // A file made before (by anything) is made private on open.
    let loose = dir.path().join("loose.db");
    std::fs::write(&loose, b"").unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(Store::open(&loose).unwrap());
    assert_eq!(std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777, 0o600);

    model_gateway::store::remove(&path).unwrap();
    assert!(files.iter().all(|f| !f.exists()));
    model_gateway::store::remove(&path).unwrap();
}

#[test]
fn usage_rolls_up_by_every_group_in_hours_and_days() {
    let store = Store::memory().unwrap();
    let prices = [flat("claude-sonnet-4-5", 1_000_000, 2_000_000, 100_000, 1_250_000), flat("claude-haiku-4-5", 500_000, 500_000, 0, 0)];
    let laptop = || client("gwc_laptop", "mch_laptop");
    let box_ = || client("gwc_box", "mch_box");
    let at = T0 + 10 * HOUR;
    // laptop, prj_a, thr_1: two requests in one hour, a third the next hour.
    let base = || Record { client: laptop(), project: Some("prj_a".into()), thread: Some("thr_1".into()), ..record(at) };
    store.record(&base(), &prices, false).unwrap();
    store.record(&Record { at: at + 5 * 60_000, ..base() }, &prices, false).unwrap();
    store.record(&Record { at: at + HOUR, ..base() }, &prices, false).unwrap();
    // box, prj_b, thr_2: an alias ("fast") that a second upstream answered after the first failed.
    let fast = Record {
        client: box_(),
        project: Some("prj_b".into()),
        thread: Some("thr_2".into()),
        model: "fast".into(),
        attempts: vec![attempt("anthropic", "claude-haiku-4-5", 529), attempt("bedrock", "claude-haiku-4-5", 200)],
        usage: Usage { input: 1000, output: 1000, cache_read: 0, cache_write: 0 },
        ..record(at)
    };
    store.record(&fast, &prices, false).unwrap();
    // A failure no upstream answered, from a client that isn't roll's, the next day.
    let failed = Record { at: at + DAY, status: 502, attempts: vec![], usage: Usage::default(), error: Some("every upstream failed".into()), ..record(0) };
    store.record(&failed, &prices, false).unwrap();

    // Sonnet: 100 in × $1 + 10 out × $2 + 1000 cached × $0.10 + 50 written × $1.25 per
    // million = 282.5 micro-dollars, rounded per request.
    let one = cost_micros("claude-sonnet-4-5", record(0).usage, &prices);
    assert_eq!(one, 283);
    let haiku = cost_micros("claude-haiku-4-5", fast.usage, &prices);
    assert_eq!(haiku, 1000);

    let report = |by, bucket| store.usage(T0, T0 + 3 * DAY, by, bucket).unwrap();
    let rows = |by, bucket| report(by, bucket).rows.into_iter().map(|r| (r.at, r.key, r.requests, r.errors, r.cost_micros)).collect::<Vec<_>>();
    let day = |n: u64| T0 + n * DAY;

    assert_eq!(
        rows(UsageBy::Model, UsageBucket::Day),
        [
            (day(0), "claude-haiku-4-5".to_string(), 1, 0, haiku),
            (day(0), "claude-sonnet-4-5".to_string(), 3, 0, 3 * one),
            (day(1), "claude-sonnet-4-5".to_string(), 1, 1, 0),
        ]
    );
    assert_eq!(
        rows(UsageBy::Model, UsageBucket::Hour),
        [
            (at, "claude-haiku-4-5".to_string(), 1, 0, haiku),
            (at, "claude-sonnet-4-5".to_string(), 2, 0, 2 * one),
            (at + HOUR, "claude-sonnet-4-5".to_string(), 1, 0, one),
            (at + DAY, "claude-sonnet-4-5".to_string(), 1, 1, 0),
        ]
    );
    let keys = |by| rows(by, UsageBucket::Day).into_iter().map(|(at, k, n, e, _)| (at, k, n, e)).collect::<Vec<_>>();
    assert_eq!(keys(UsageBy::Machine), [(day(0), "mch_box".into(), 1, 0), (day(0), "mch_laptop".into(), 3, 0), (day(1), String::new(), 1, 1)]);
    assert_eq!(keys(UsageBy::Project), [(day(0), "prj_a".into(), 3, 0), (day(0), "prj_b".into(), 1, 0), (day(1), String::new(), 1, 1)]);
    assert_eq!(keys(UsageBy::Thread), [(day(0), "thr_1".into(), 3, 0), (day(0), "thr_2".into(), 1, 0), (day(1), String::new(), 1, 1)]);
    assert_eq!(keys(UsageBy::Upstream), [(day(0), "anthropic".into(), 3, 0), (day(0), "bedrock".into(), 1, 0), (day(1), String::new(), 1, 1)]);

    // Token sums.
    let r = report(UsageBy::Machine, UsageBucket::Day);
    let laptop_row = r.rows.iter().find(|r| r.key == "mch_laptop").unwrap();
    assert_eq!((laptop_row.input_tokens, laptop_row.output_tokens, laptop_row.cache_read_tokens, laptop_row.cache_write_tokens), (300, 30, 3000, 150));
    assert!(!r.truncated);
    assert_eq!((r.from, r.to, r.by, r.bucket), (T0, T0 + 3 * DAY, UsageBy::Machine, UsageBucket::Day));

    // The window: hours overlapping [from, to).
    assert_eq!(store.usage(at + HOUR + 1, at + DAY, UsageBy::Model, UsageBucket::Hour).unwrap().rows.len(), 1);
    assert!(store.usage(at + DAY + HOUR, T0 + 3 * DAY, UsageBy::Model, UsageBucket::Hour).unwrap().rows.is_empty());
}

#[test]
fn a_usage_report_keeps_the_newest_rows_when_capped() {
    let store = Store::memory().unwrap();
    let n = gateway_limits::USAGE_ROWS as u64 + 10;
    for i in 0..n {
        store.record(&Record { at: T0 + i * HOUR, ..record(0) }, &[], false).unwrap();
    }
    let r = store.usage(T0, T0 + n * HOUR, UsageBy::Model, UsageBucket::Hour).unwrap();
    assert!(r.truncated);
    assert_eq!(r.rows.len(), gateway_limits::USAGE_ROWS);
    assert_eq!(r.rows.first().unwrap().at, T0 + 10 * HOUR);
    assert_eq!(r.rows.last().unwrap().at, T0 + (n - 1) * HOUR);
}

#[test]
fn cost_is_the_owners_price_first_then_the_list() {
    let u = Usage { input: 2_000_000, output: 1_000_000, cache_read: 4_000_000, cache_write: 400_000 };
    // $3 in, $15 out, $0.30 cached, $3.75 written per million.
    let mine = [flat("claude-sonnet-4-*", 3_000_000, 15_000_000, 300_000, 3_750_000)];
    assert_eq!(cost_micros("claude-sonnet-4-6", u, &mine), 6_000_000 + 15_000_000 + 1_200_000 + 1_500_000);
    // The owner's pattern matches the cloud's spelling of the model too.
    assert_eq!(cost_micros("us.anthropic.claude-sonnet-4-6-v1:0", u, &mine), cost_micros("claude-sonnet-4-6", u, &mine));
    // Exact beats a prefix; a longer prefix beats a shorter one.
    let layered = [flat("*", 1, 0, 0, 0), flat("my-*", 2_000_000, 0, 0, 0), flat("my-model-*", 3_000_000, 0, 0, 0), flat("my-model-x", 5_000_000, 0, 0, 0)];
    let input = |n| Usage { input: n, ..Usage::default() };
    assert_eq!(cost_micros("my-model-x", input(1_000_000), &layered), 5_000_000);
    assert_eq!(cost_micros("my-model-y", input(1_000_000), &layered), 3_000_000);
    assert_eq!(cost_micros("my-other", input(1_000_000), &layered), 2_000_000);
    assert_eq!(cost_micros("else", input(1_000_000), &layered), 1);
    assert_eq!(price_of("my-model-y", &layered).unwrap().input, 3_000_000);
    // Rounded to the nearest micro-dollar: 1 token at $1.50 per million.
    assert_eq!(cost_micros("m", input(1), &[flat("m", 1_500_000, 0, 0, 0)]), 2);
    assert_eq!(cost_micros("m", input(1), &[flat("m", 1_400_000, 0, 0, 0)]), 1);
    // Big numbers don't overflow.
    assert!(cost_micros("m", Usage { input: u64::MAX, ..Usage::default() }, &[flat("m", u64::MAX, 0, 0, 0)]) == u64::MAX);
    // Neither the owner's nor the list's: no price, no cost.
    assert_eq!(price_of("a-model-nobody-lists", &mine), None);
    assert_eq!(cost_micros("a-model-nobody-lists", u, &mine), 0);
    // The list knows the models agents use (numbers are the table's: only their presence here).
    for m in ["claude-opus-4-5", "claude-sonnet-4-5-20250929", "claude-haiku-4-5", "gpt-5", "gpt-5.4-mini", "anthropic.claude-opus-4-5-v1:0"] {
        assert!(price_of(m, &[]).is_some_and(|p| p.input > 0 && p.output > p.input), "{m}");
    }

    // A record is priced by the model the answering upstream was asked for.
    let store = Store::memory().unwrap();
    let prices = [flat("target", 1_000_000, 0, 0, 0)];
    let r = Record { model: "alias".into(), attempts: vec![attempt("u", "target", 200)], usage: input(7), ..record(T0) };
    store.record(&r, &prices, false).unwrap();
    let log = store.log(None, None, 10, false).unwrap();
    assert_eq!(log[0].cost_micros, 7);
    assert_eq!(store.usage(T0, T0 + DAY, UsageBy::Model, UsageBucket::Day).unwrap().rows[0].cost_micros, 7);
}

#[test]
fn the_log_pages_both_ways_and_filters_errors() {
    let store = Store::memory().unwrap();
    // 250 requests: every 10th failed, every 25th failed over and then succeeded.
    for i in 0..250u64 {
        let mut r = Record { at: T0 + i, client: client("gwc_a", "mch_a"), thread: Some(format!("thr_{i}")), ..record(0) };
        if i % 10 == 0 {
            r.status = 500;
        }
        if i % 25 == 1 {
            r.attempts.insert(0, attempt("first", "m", 503));
        }
        store.record(&r, &[], false).unwrap();
    }
    let seqs = |v: Vec<agent_proto::LogEntry>| v.into_iter().map(|e| e.seq).collect::<Vec<_>>();
    let newest = store.log(None, None, 1000, false).unwrap();
    assert_eq!(newest.len(), usize::from(gateway_limits::LOG_PAGE));
    assert!(newest.windows(2).all(|w| w[0].seq > w[1].seq), "newest first");
    let top = newest[0].seq;
    assert_eq!(newest[0].thread.as_deref(), Some("thr_249"));
    assert_eq!(newest[0].machine, Some(MachineId("mch_a".into())));
    assert_eq!(newest[0].client.as_deref(), Some("gwc_a"));
    assert_eq!(newest[0].format, WireFormat::Messages);
    assert_eq!((newest[0].input_tokens, newest[0].cache_read_tokens, newest[0].first_byte_ms), (100, 1000, Some(300)));
    assert_eq!(store.log(None, None, 0, false).unwrap().len(), usize::from(gateway_limits::LOG_PAGE));

    // A live tail: what came after the newest seen, newest first.
    assert!(store.log(Some(top), None, 50, false).unwrap().is_empty());
    assert_eq!(seqs(store.log(Some(top - 3), None, 50, false).unwrap()), [top, top - 1, top - 2]);
    // A tail that fell further behind than a page gets the newest page.
    assert_eq!(seqs(store.log(Some(top - 100), None, 10, false).unwrap()), (top - 9..=top).rev().collect::<Vec<_>>());
    // Paging back.
    let older = store.log(None, Some(top - 199), 200, false).unwrap();
    assert_eq!(older.len(), 50);
    assert_eq!(older[0].seq, top - 200);
    assert_eq!(seqs(store.log(Some(top - 10), Some(top - 5), 200, false).unwrap()), [top - 6, top - 7, top - 8, top - 9]);

    let errors = store.log(None, None, 200, true).unwrap();
    // 25 failed (i % 10 == 0) and 10 failed over (i % 25 == 1), none both.
    assert_eq!(errors.len(), 35);
    assert!(errors.iter().all(|e| e.status >= 400 || e.attempts.len() > 1));
    let failover = errors.iter().find(|e| e.attempts.len() > 1).unwrap();
    assert_eq!(failover.attempts[0].upstream, "first");
    assert_eq!(failover.attempts[0].error.as_deref(), Some("upstream said 503"));
    assert_eq!(failover.status, 200);
    // A gateway error with a 200 can't happen, but an error counts on its own.
    store.record(&Record { error: Some("client went away".into()), ..record(T0 + 300) }, &[], false).unwrap();
    assert_eq!(store.log(None, None, 1, true).unwrap()[0].error.as_deref(), Some("client went away"));
}

#[test]
fn bodies_are_kept_only_when_asked_capped_and_pruned_after_a_day() {
    let store = Store::memory().unwrap();
    let bodies = || Some(("{\"model\":\"m\"}".to_string(), "{\"ok\":true}".to_string()));
    store.record(&Record { bodies: bodies(), ..record(T0) }, &[], false).unwrap();
    store.record(&Record { bodies: bodies(), ..record(T0 + 1) }, &[], true).unwrap();
    // Built the way the server keeps them: up to one byte past the cap.
    let mut big = Vec::new();
    for _ in 0..100 {
        keep_body(&mut big, "é".repeat(1000).as_bytes());
    }
    assert_eq!(big.len(), BODY_CAP + 1);
    let big = String::from_utf8_lossy(&big).into_owned();
    store.record(&Record { bodies: Some((big, "short".into())), ..record(T0 + 2) }, &[], true).unwrap();

    let log = store.log(None, None, 10, false).unwrap();
    assert_eq!(log.iter().map(|e| e.has_body).collect::<Vec<_>>(), [true, true, false]);
    assert_eq!(store.log_body(log[2].seq).unwrap(), None);
    let small = store.log_body(log[1].seq).unwrap().unwrap();
    assert_eq!((small.request.as_str(), small.response.as_str(), small.truncated), ("{\"model\":\"m\"}", "{\"ok\":true}", false));
    let cut = store.log_body(log[0].seq).unwrap().unwrap();
    assert!(cut.truncated);
    assert!(cut.request.len() <= BODY_CAP && cut.request.len() > BODY_CAP - 4, "{}", cut.request.len());
    assert!(cut.request.chars().all(|c| c == 'é'));
    assert_eq!(cut.response, "short");

    // A day on, the bodies go; the log stays.
    store.prune(T0 + DAY - 1, 400).unwrap();
    assert!(store.log_body(log[0].seq).unwrap().is_some());
    store.prune(T0 + DAY + 3, 400).unwrap();
    assert_eq!(store.log_body(log[0].seq).unwrap(), None);
    let after = store.log(None, None, 10, false).unwrap();
    assert_eq!(after.len(), 3);
    assert!(after.iter().all(|e| !e.has_body));
}

#[test]
fn retention_drops_old_usage_and_log_rows() {
    let store = Store::memory().unwrap();
    for d in 0..40u64 {
        store.record(&record(T0 + d * DAY), &[], false).unwrap();
    }
    let now = T0 + 39 * DAY + HOUR;
    store.prune(now, 30).unwrap();
    let usage = store.usage(0, u64::MAX >> 2, UsageBy::Model, UsageBucket::Day).unwrap();
    // Days 9 (its hour starts after now - 30 days) to 39.
    assert_eq!(usage.rows.first().unwrap().at, T0 + 10 * DAY);
    assert_eq!(usage.rows.len(), 30);
    // The log keeps a week.
    let log = store.log(None, None, 200, false).unwrap();
    assert_eq!(log.len(), 7);
    assert_eq!(log.last().unwrap().at, T0 + 33 * DAY);
}

#[test]
fn the_log_keeps_at_most_100k_rows() {
    let store = Store::memory().unwrap();
    for i in 0..100_005u64 {
        store.record(&record(T0 + i), &[], false).unwrap();
    }
    store.prune(T0 + 200_000, 400).unwrap();
    let mut oldest = None;
    let mut count = 0;
    let mut before = None;
    loop {
        let page = store.log(None, before, 200, false).unwrap();
        let Some(last) = page.last() else { break };
        count += page.len();
        before = Some(last.seq);
        oldest = Some(last.at);
    }
    assert_eq!(count, 100_000);
    assert_eq!(oldest, Some(T0 + 5));
    // Usage is roll-ups, untouched by the log's cap: one hour's row holds every request.
    assert_eq!(store.usage(T0, T0 + DAY, UsageBy::Model, UsageBucket::Day).unwrap().rows[0].requests, 100_005);
}

const CONFIG: &str = r#"
port = 18790
log_bodies = true
data_dir = "state"

[[upstreams]]
id = "anthropic"
name = "Anthropic"
kind = "anthropic"

[[upstreams]]
id = "local"
name = "Local model"
kind = "openai_compatible"
base_url = "http://localhost:11434/v1"
allow_private = true
models = ["llama-*"]
headers = [["x-team", "infra"]]

[[routes]]
model = "fast"
targets = [{ upstream = "anthropic", model = "claude-haiku-4-5" }]

[[prices]]
model = "llama-*"
input = 0
output = 0

[keys]
anthropic = "env:HOME"
local = "file:local.key"
"#;

/// What `env:HOME` resolves to (a variable every test run has, so no test sets one).
fn home() -> String {
    std::env::var("HOME").expect("HOME").trim().to_string()
}

#[test]
fn the_config_file_parses_checks_and_resolves_keys() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("local.key"), "  local-secret \n").unwrap();
    let path = dir.path().join("gateway.toml");
    std::fs::write(&path, CONFIG).unwrap();
    {
        let loaded = config::load(&path).unwrap();
        let s = &loaded.settings;
        assert!(s.enabled, "a served file is enabled unless it says otherwise");
        assert_eq!((s.port, s.log_bodies, s.affinity, s.retention_days), (18790, true, true, 400));
        assert_eq!(s.upstreams.len(), 2);
        assert_eq!(s.upstreams[1].headers, [("x-team".to_string(), "infra".to_string())]);
        assert_eq!(s.routes[0].targets[0].model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(s.prices[0].model, "llama-*");
        assert_eq!(loaded.keys["anthropic"].expose(), home());
        assert_eq!(loaded.keys["local"].expose(), "local-secret");
        assert_eq!(loaded.data_dir, dir.path().join("state"));

        let bad = |text: &str| config::parse(text, dir.path()).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(config::parse(&CONFIG.replace("log_bodies = true", "enabled = false"), dir.path()).is_ok_and(|l| !l.settings.enabled));
        // A pasted key is refused, and not echoed.
        let pasted = bad(&CONFIG.replace("env:HOME", "sk-ant-pasted-secret"));
        assert!(pasted.contains("never put the key itself"), "{pasted}");
        assert!(!pasted.contains("pasted-secret"), "{pasted}");
        assert!(bad(&CONFIG.replace("port = 18790", "prot = 18790")).contains("unknown setting \"prot\""));
        assert!(bad(&CONFIG.replace("port = 18790", "port = 80")).contains("1024"));
        assert!(bad(&CONFIG.replace("upstream = \"anthropic\"", "upstream = \"nope\"")).contains("isn't an upstream"));
        assert!(bad(&format!("{CONFIG}nope = \"env:X\"\n")).contains("[keys] names \"nope\""));
        assert!(bad(&CONFIG.replace("env:HOME", "env:ROLL_GATEWAY_TEST_UNSET")).contains("$ROLL_GATEWAY_TEST_UNSET isn't set"));
        assert!(bad(&CONFIG.replace("local.key", "missing.key")).contains("missing.key"));
        assert!(bad(&CONFIG.replace("kind = \"anthropic\"", "kind = \"anthropic\"\nkey_ref = \"op://v/i/f\"")).contains("key_ref"));
        assert!(bad("port = ").contains("TOML"));
        // An absolute data_dir is kept; none means the file's directory.
        let abs = config::parse(&CONFIG.replace("data_dir = \"state\"", "data_dir = \"/var/lib/roll-gateway\""), dir.path()).unwrap();
        assert_eq!(abs.data_dir, Path::new("/var/lib/roll-gateway"));
        assert_eq!(config::parse(&CONFIG.replace("data_dir = \"state\"", ""), dir.path()).unwrap().data_dir, dir.path());
        // An empty file is a gateway with nothing behind it.
        let empty = config::parse("", dir.path()).unwrap();
        assert!(empty.settings.enabled && empty.settings.upstreams.is_empty() && empty.keys.is_empty());
    }
    // Key files are bounded.
    std::fs::write(dir.path().join("local.key"), vec![b'k'; 16 * 1024 + 1]).unwrap();
    let err = format!("{:#}", config::load(&path).err().unwrap());
    assert!(err.contains("bigger than"), "{err}");
    // The client commands find the store without resolving keys.
    std::fs::write(&path, CONFIG.replace("env:HOME", "env:ROLL_GATEWAY_TEST_UNSET")).unwrap();
    assert_eq!(config::data_dir(&path).unwrap(), dir.path().join("state"));
    std::fs::write(&path, "port = 9000\n").unwrap();
    assert_eq!(config::data_dir(&path).unwrap(), dir.path());
}

#[tokio::test]
async fn the_config_file_reloads_and_a_bad_change_keeps_the_last_good_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("local.key"), "local-secret").unwrap();
    let path = dir.path().join("gateway.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let first = config::load(&path).unwrap();
    // What the watcher reported: the port applied, or why a change was kept out.
    let seen: Arc<Mutex<Vec<Result<u16, String>>>> = Arc::default();
    let task = {
        let seen = seen.clone();
        let path = path.clone();
        tokio::spawn(config::watch(path, first.stamp, Duration::from_millis(40), move |r: config::Reload| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(match r {
                    config::Reload::Apply(l) => Ok(l.settings.port),
                    config::Reload::Keep { why } => Err(why),
                })
            }
        }))
    };
    let wait_for = |n: usize| {
        let seen = seen.clone();
        async move {
            for _ in 0..100 {
                if seen.lock().unwrap().len() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("no reload: {:?}", seen.lock().unwrap());
        }
    };
    // Nothing changed: nothing reported.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(seen.lock().unwrap().is_empty());

    std::fs::write(&path, CONFIG.replace("18790", "18791")).unwrap();
    wait_for(1).await;
    // A file that doesn't check, then no file: each kept out with why, and the next good
    // change applies.
    std::fs::write(&path, CONFIG.replace("18790", "80")).unwrap();
    wait_for(2).await;
    std::fs::remove_file(&path).unwrap();
    wait_for(3).await;
    std::fs::write(&path, CONFIG.replace("18790", "18792")).unwrap();
    wait_for(4).await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0], Ok(18791));
    assert!(seen[1].as_ref().is_err_and(|why| why.contains("1024")), "{:?}", seen[1]);
    assert!(seen[2].as_ref().is_err_and(|why| why.contains("is gone")), "{:?}", seen[2]);
    assert_eq!(seen[3], Ok(18792));
    task.abort();
}
