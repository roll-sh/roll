//! `UsageLimits`' merge rules, as T3's `packages/provider-core/src/server/usageLimits.ts`.

use agent_proto::{UsageLimits, UsageWindow, UsageWindowKind};

fn window(id: &str, kind: UsageWindowKind, used: f64) -> UsageWindow {
    UsageWindow::new(id, kind, id, used)
}

#[test]
fn an_update_replaces_its_window_and_keeps_what_it_leaves_out() {
    // Windows sort by kind then id, and percentages are clamped.
    let limits = UsageLimits::new(
        1,
        vec![window("b", UsageWindowKind::Weekly, 150.0), window("z", UsageWindowKind::Session, -3.0), window("a", UsageWindowKind::Weekly, f64::NAN)],
    );
    let got: Vec<_> = limits.windows.iter().map(|w| (w.id.as_str(), w.used_percent)).collect();
    assert_eq!(got, [("z", 0.0), ("a", 0.0), ("b", 100.0)]);

    let mut session = window("primary", UsageWindowKind::Session, 10.0);
    session.resets_at = Some(5_000);
    session.window_minutes = Some(300);
    let before = UsageLimits::new(1_000, vec![session, window("secondary", UsageWindowKind::Weekly, 20.0)]);

    // A streamed tick with a new percentage but no reset time keeps the old reset time.
    let tick = UsageLimits::new(2_000, vec![window("primary", UsageWindowKind::Session, 35.0)]);
    let after = UsageLimits::apply(Some(&before), &tick).expect("changed");
    assert_eq!(after.checked_at, 2_000);
    assert_eq!(after.windows.len(), 2);
    let primary = &after.windows[0];
    assert_eq!((primary.used_percent, primary.resets_at, primary.window_minutes), (35.0, Some(5_000), Some(300)));
    assert_eq!(after.windows[1].used_percent, 20.0);

    // The same tick again changes nothing (Codex repeats unchanged limits on token ticks).
    assert_eq!(UsageLimits::apply(Some(&after), &tick), None);
    // An empty update changes nothing.
    assert_eq!(UsageLimits::apply(Some(&after), &UsageLimits::new(3_000, vec![])), None);
    // With nothing before, the update is the limits.
    assert_eq!(UsageLimits::apply(None, &tick).unwrap().windows.len(), 1);
}

#[test]
fn a_probe_keeps_newer_streamed_windows_and_an_empty_probe_keeps_the_last() {
    let streamed = UsageLimits::new(9_000, vec![window("five_hour", UsageWindowKind::Session, 80.0)]);
    let probed = UsageLimits::new(
        5_000,
        vec![window("five_hour", UsageWindowKind::Session, 60.0), window("seven_day", UsageWindowKind::Weekly, 30.0)],
    );
    let kept = UsageLimits::after_probe(Some(streamed.clone()), Some(probed.clone())).unwrap();
    assert_eq!(kept.checked_at, 9_000);
    let got: Vec<_> = kept.windows.iter().map(|w| (w.id.as_str(), w.used_percent)).collect();
    assert_eq!(got, [("five_hour", 80.0), ("seven_day", 30.0)]);

    // A newer probe wins outright.
    let newer = UsageLimits { checked_at: 10_000, ..probed.clone() };
    assert_eq!(UsageLimits::after_probe(Some(streamed.clone()), Some(newer.clone())), Some(newer));
    // A probe that found none keeps what was there.
    assert_eq!(UsageLimits::after_probe(Some(streamed.clone()), None), Some(streamed));
    assert_eq!(UsageLimits::after_probe(None, None), None);
}

#[test]
fn reset_at_is_the_latest_reset_of_the_exhausted_windows() {
    let mut a = window("primary", UsageWindowKind::Session, 100.0);
    a.resets_at = Some(10);
    let mut b = window("secondary", UsageWindowKind::Weekly, 100.0);
    b.resets_at = Some(20);
    let c = window("other", UsageWindowKind::Other, 50.0);
    assert_eq!(UsageLimits::new(1, vec![a.clone(), b.clone(), c.clone()]).reset_at(), Some(20));
    assert_eq!(UsageLimits::new(1, vec![c.clone()]).reset_at(), None);
    // An exhausted window that doesn't say when it resets: unknown.
    let d = window("d", UsageWindowKind::Monthly, 100.0);
    assert_eq!(UsageLimits::new(1, vec![a, d]).reset_at(), None);
}
