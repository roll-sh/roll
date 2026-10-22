//! The mirror scheduler: thousands of landings become one fast-forward push.

use code_github::mirror::{Blocked, BlockedKind};
use code_github::{Mirror, MirrorAction, MirrorConfig, PushFailure};
use std::time::{Duration, Instant};

const MS: Duration = Duration::from_millis(1);

fn config() -> MirrorConfig {
    MirrorConfig {
        debounce: 2000 * MS,
        max_delay: 10_000 * MS,
        backoff_initial: 1000 * MS,
        backoff_max: 8000 * MS,
    }
}

/// Linear history c0 → c1 → … ; plus `x*` commits that only GitHub has, on top of `c3`.
fn ancestry(a: &str, d: &str) -> bool {
    let n = |s: &str| s[1..].parse::<u32>().unwrap();
    match (&a[..1], &d[..1]) {
        ("c", "c") => n(a) <= n(d),
        ("c", "x") => n(a) <= 3,
        ("x", "x") => n(a) <= n(d),
        // c10+ descends from x1 once roll ingested GitHub's commits.
        ("x", "c") => n(d) >= 10,
        _ => false,
    }
}

#[test]
fn coalesces_a_burst_into_one_push() {
    let t0 = Instant::now();
    let mut m = Mirror::new(config(), Some("c0".into()));
    assert_eq!(m.poll(t0), MirrorAction::Idle);
    for i in 1..=1000u32 {
        m.landed(format!("c{i}"), t0 + i * MS);
    }
    // Still landing: wait for the debounce after the last landing.
    assert_eq!(m.poll(t0 + 1000 * MS), MirrorAction::Wait(2000 * MS));
    let push = m.poll(t0 + 3000 * MS);
    assert_eq!(
        push,
        MirrorAction::Push {
            from: Some("c0".into()),
            to: "c1000".into()
        }
    );
    // One push at a time.
    assert_eq!(m.poll(t0 + 3001 * MS), MirrorAction::Idle);
    m.push_succeeded("c1000", t0 + 3100 * MS);
    assert_eq!(m.poll(t0 + 3200 * MS), MirrorAction::Idle);
    assert_eq!(m.status(t0).mirrored.as_deref(), Some("c1000"));
}

#[test]
fn max_delay_bounds_staleness_under_constant_landing() {
    let t0 = Instant::now();
    let mut m = Mirror::new(config(), Some("c0".into()));
    let mut pushed = None;
    // A landing every 500ms never lets the debounce expire; max_delay forces a push at 10s.
    for i in 1..=40u32 {
        let now = t0 + i * 500 * MS;
        m.landed(format!("c{i}"), now);
        if let MirrorAction::Push { to, .. } = m.poll(now) {
            pushed = Some((i, to));
            break;
        }
    }
    let (i, to) = pushed.expect("pushed within max_delay");
    assert_eq!(i, 21, "first landing at 0.5s + 10s max delay");
    assert_eq!(to, "c21");
}

#[test]
fn landings_during_a_push_go_in_the_next_one() {
    let t0 = Instant::now();
    let mut m = Mirror::new(config(), None);
    m.landed("c1", t0);
    assert_eq!(
        m.poll(t0 + 2000 * MS),
        MirrorAction::Push {
            from: None,
            to: "c1".into()
        }
    );
    m.landed("c2", t0 + 2100 * MS);
    m.landed("c3", t0 + 2200 * MS);
    m.push_succeeded("c1", t0 + 2300 * MS);
    assert_eq!(m.poll(t0 + 2300 * MS), MirrorAction::Wait(1900 * MS));
    assert_eq!(
        m.poll(t0 + 4200 * MS),
        MirrorAction::Push {
            from: Some("c1".into()),
            to: "c3".into()
        }
    );
}

#[test]
fn transient_failures_back_off_exponentially() {
    let t0 = Instant::now();
    let mut m = Mirror::new(config(), Some("c0".into()));
    m.landed("c1", t0);
    let mut now = t0 + 2000 * MS;
    let mut waits = vec![];
    for _ in 0..5 {
        assert!(matches!(m.poll(now), MirrorAction::Push { .. }));
        m.push_failed(PushFailure::Transient("502".into()), now, &ancestry);
        let MirrorAction::Wait(w) = m.poll(now) else {
            panic!()
        };
        waits.push(w.as_millis());
        now += w;
    }
    assert_eq!(waits, [1000, 2000, 4000, 8000, 8000]);
    assert_eq!(m.status(now).failures, 5);
    assert_eq!(m.status(now).last_error.as_deref(), Some("502"));
    // A landing during backoff doesn't skip it.
    m.landed("c2", now - 10 * MS);
    assert!(matches!(m.poll(now), MirrorAction::Push { to, .. } if to == "c2"));
    m.push_succeeded("c2", now);
    let s = m.status(now);
    assert_eq!((s.failures, s.last_error, s.retry_in), (0, None, None));
}

#[test]
fn stale_view_after_lost_response_retries_immediately() {
    let t0 = Instant::now();
    let mut m = Mirror::new(config(), Some("c0".into()));
    m.landed("c5", t0);
    assert!(matches!(m.poll(t0 + 2000 * MS), MirrorAction::Push { .. }));
    m.landed("c6", t0 + 2100 * MS);
    // Our push of c5 reached GitHub but the response was lost: GitHub is at c5, not c0.
    m.push_failed(
        PushFailure::Transient("timeout".into()),
        t0 + 2200 * MS,
        &ancestry,
    );
    let now = t0 + 3200 * MS;
    assert!(
        matches!(m.poll(now), MirrorAction::Push { from, to } if from.as_deref() == Some("c0") && to == "c6")
    );
    m.push_failed(
        PushFailure::Rejected {
            remote: Some("c5".into()),
        },
        now,
        &ancestry,
    );
    assert_eq!(
        m.poll(now),
        MirrorAction::Push {
            from: Some("c5".into()),
            to: "c6".into()
        }
    );
}

#[test]
fn github_ahead_blocks_until_ingested() {
    let t0 = Instant::now();
    let mut m = Mirror::new(config(), Some("c3".into()));
    m.landed("c4", t0);
    // Someone pushed x1 straight to GitHub on top of c3.
    assert!(matches!(m.poll(t0 + 2000 * MS), MirrorAction::Push { .. }));
    m.push_failed(
        PushFailure::Rejected {
            remote: Some("x1".into()),
        },
        t0 + 2000 * MS,
        &ancestry,
    );
    let blocked = MirrorAction::Blocked(Blocked {
        remote: "x1".into(),
        kind: BlockedKind::Diverged,
    });
    assert_eq!(m.poll(t0 + 60_000 * MS), blocked);

    // Before roll landed anything new, GitHub moving ahead is just "remote ahead".
    let mut m2 = Mirror::new(config(), Some("c3".into()));
    m2.remote_observed(Some("x2"), &ancestry);
    assert_eq!(
        m2.poll(t0),
        MirrorAction::Blocked(Blocked {
            remote: "x2".into(),
            kind: BlockedKind::RemoteAhead
        })
    );

    // roll ingests GitHub's commits and lands on top of them; pushing resumes.
    m.landed("c10", t0 + 61_000 * MS);
    assert!(m.resolve(&ancestry));
    assert_eq!(m.status(t0).mirrored.as_deref(), Some("x1"));
    assert_eq!(
        m.poll(t0 + 63_000 * MS),
        MirrorAction::Push {
            from: Some("x1".into()),
            to: "c10".into()
        }
    );
}

#[test]
fn remote_observations() {
    let t0 = Instant::now();
    let mut m = Mirror::new(config(), Some("c2".into()));
    // Our own echo (push webhook for what we pushed) changes nothing.
    m.remote_observed(Some("c2"), &ancestry);
    assert_eq!(m.poll(t0), MirrorAction::Idle);
    // The ref was deleted on GitHub: recreate it.
    m.remote_observed(None, &ancestry);
    assert_eq!(
        m.poll(t0),
        MirrorAction::Push {
            from: None,
            to: "c2".into()
        }
    );
    // Before any landing, whatever GitHub has is the baseline.
    let mut fresh = Mirror::new(config(), None);
    fresh.remote_observed(Some("c7"), &ancestry);
    assert_eq!(fresh.status(t0).mirrored.as_deref(), Some("c7"));
}
