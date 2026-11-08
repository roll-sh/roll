//! Usage limits as providers report them to their own clients, mapped onto
//! [`agent_proto::UsageLimits`]. Ported from T3 Code (MIT): `apps/server/src/provider/
//! codexUsageLimits.ts`, `claudeUsageLimits.ts` and `packages/provider-muse/src/server/
//! usage.ts` (its live `usage/changed` windows; T3's history read from Muse's session logs isn't
//! ported: roll never reads a provider's files).
//!
//! Only what a provider's CLI or protocol tells its client is read here: Codex's
//! `account/rateLimits/read` and `account/rateLimits/updated`, Claude Code's `rate_limit_event`,
//! Muse's `usage/changed`.
//! T3's readers for Cursor, OpenCode, Grok and Antigravity (and Claude's reset credits) read the
//! CLI's stored login and call the vendor's API with it; roll never reads a credential, so those
//! providers report no limits.

use agent_proto::{Millis, UsageLimits, UsageWindow, UsageWindowKind};
use serde::Deserialize;

const SESSION_MINS: u32 = 5 * 60;
const WEEK_MINS: u32 = 7 * 24 * 60;
const MONTH_MINS: u32 = 30 * 24 * 60;

/// Now, in milliseconds since the epoch.
pub fn now() -> Millis {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as Millis).unwrap_or(0)
}

/// Epoch seconds (as providers send them) in milliseconds; nothing for zero or less.
fn from_secs(secs: Option<f64>) -> Option<Millis> {
    secs.filter(|s| s.is_finite() && *s > 0.0).map(|s| (s * 1000.0) as Millis)
}

// ---------------------------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------------------------

/// Codex's `RateLimitSnapshot` (the parts limits need).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSnapshot {
    #[serde(default)]
    pub limit_id: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub primary: Option<CodexWindow>,
    #[serde(default)]
    pub secondary: Option<CodexWindow>,
    #[serde(default)]
    pub individual_limit: Option<CodexIndividual>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexWindow {
    pub used_percent: f64,
    #[serde(default)]
    pub resets_at: Option<f64>,
    #[serde(default)]
    pub window_duration_mins: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexIndividual {
    pub remaining_percent: f64,
    #[serde(default)]
    pub resets_at: Option<f64>,
}

/// `account/rateLimits/read`'s answer and `account/rateLimits/updated`'s params.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRateLimits {
    #[serde(default)]
    pub rate_limits: Option<CodexSnapshot>,
    #[serde(default)]
    pub rate_limits_by_limit_id: Option<std::collections::BTreeMap<String, CodexSnapshot>>,
}

fn kind_for(mins: u32) -> (UsageWindowKind, &'static str) {
    if mins >= MONTH_MINS {
        (UsageWindowKind::Monthly, "Monthly")
    } else if mins >= WEEK_MINS {
        (UsageWindowKind::Weekly, "Weekly")
    } else {
        (UsageWindowKind::Session, "Session")
    }
}

/// T3's `codexRateLimitsToWindows`: `primary` and `secondary` are positions; without a length,
/// paid plans have 5-hour and weekly windows and Free/Go a monthly one. Model-specific snapshots
/// (`limitId` other than `codex`) aren't the account's allowance.
fn codex_windows(s: &CodexSnapshot) -> Vec<UsageWindow> {
    if s.limit_id.as_deref().is_some_and(|id| id != "codex") {
        return Vec::new();
    }
    let monthly_plan = matches!(s.plan_type.as_deref(), Some("free" | "go"));
    let mut windows = Vec::new();
    for (id, window, fallback) in
        [("primary", &s.primary, if monthly_plan { MONTH_MINS } else { SESSION_MINS }), ("secondary", &s.secondary, WEEK_MINS)]
    {
        let Some(w) = window else { continue };
        if !w.used_percent.is_finite() {
            continue;
        }
        let mins = w.window_duration_mins.unwrap_or(fallback);
        let (kind, label) = kind_for(mins);
        windows.push(UsageWindow { resets_at: from_secs(w.resets_at), window_minutes: Some(mins), ..UsageWindow::new(id, kind, label, w.used_percent) });
    }
    if let Some(i) = &s.individual_limit
        && i.remaining_percent.is_finite()
    {
        windows.push(UsageWindow {
            resets_at: from_secs(i.resets_at),
            ..UsageWindow::new("individual", UsageWindowKind::Monthly, "Monthly credit limit", 100.0 - i.remaining_percent)
        });
    }
    windows
}

/// Codex's limits (the `codex` snapshot when it names several), or nothing when it reports none.
pub fn codex(limits: &CodexRateLimits, checked_at: Millis) -> Option<UsageLimits> {
    let snapshot = limits.rate_limits_by_limit_id.as_ref().and_then(|m| m.get("codex")).or(limits.rate_limits.as_ref())?;
    let windows = codex_windows(snapshot);
    if windows.is_empty() {
        return None;
    }
    let mut out = UsageLimits::new(checked_at, windows);
    out.plan = snapshot.plan_type.clone().filter(|p| !p.is_empty() && p != "unknown");
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------------------------------

/// `rate_limit_event`'s `rate_limit_info`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRateLimitInfo {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub rate_limit_type: Option<String>,
    /// 0 to 1.
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<f64>,
    #[serde(default)]
    pub overage_status: Option<String>,
    #[serde(default)]
    pub is_using_overage: Option<bool>,
}

impl ClaudeRateLimitInfo {
    /// Rejected, and no overage to fall back on: the turn hit the limit (T3's
    /// `ClaudeAdapterV2`: a rejection blocks only when overage isn't allowed).
    pub fn blocks(&self) -> bool {
        self.status.as_deref() == Some("rejected") && self.is_using_overage != Some(true) && self.overage_status.as_deref() != Some("allowed")
    }
}

/// T3's `claudeRateLimitEventToUpdate`: one window per event, for the 5-hour and weekly buckets
/// (unnamed overage buckets are dropped, as T3 does until a usage probe names them).
pub fn claude(info: &ClaudeRateLimitInfo, checked_at: Millis) -> Option<UsageLimits> {
    let utilization = info.utilization.filter(|u| u.is_finite())?;
    let (id, kind, label, mins) = match info.rate_limit_type.as_deref()? {
        "five_hour" => ("five_hour", UsageWindowKind::Session, "Session", SESSION_MINS),
        "seven_day" => ("seven_day", UsageWindowKind::Weekly, "Weekly", WEEK_MINS),
        _ => return None,
    };
    let window = UsageWindow { resets_at: from_secs(info.resets_at), window_minutes: Some(mins), ..UsageWindow::new(id, kind, label, utilization * 100.0) };
    Some(UsageLimits::new(checked_at, vec![window]))
}

// ---------------------------------------------------------------------------------------------
// Muse
// ---------------------------------------------------------------------------------------------

/// The latest instant JavaScript (and so Muse) can name.
const LATEST_MS: f64 = 8.64e15;

/// Muse's `usage/changed` (T3's `MuseSubscriptionUsage`): the rolling window and the week, as
/// percentages Meta reports verbatim (they may pass 100), with reset times in milliseconds.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MuseUsage {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub window: Option<MuseWindow>,
    #[serde(default)]
    pub weekly: Option<MuseWindow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MuseWindow {
    pub used_percent: f64,
    #[serde(default)]
    pub window_duration_mins: Option<f64>,
    #[serde(default)]
    pub resets_at_ms: Option<f64>,
}

fn muse_reset(ms: Option<f64>) -> Option<Millis> {
    ms.filter(|ms| ms.is_finite() && *ms > 0.0 && *ms <= LATEST_MS).map(|ms| ms as Millis)
}

impl MuseUsage {
    /// The reset times of windows at or over their limit (T3's `museUsageLimitResetAt` waits for
    /// all of them).
    pub fn exhausted(&self) -> Vec<Millis> {
        [&self.window, &self.weekly].into_iter().flatten().filter(|w| w.used_percent >= 100.0).filter_map(|w| muse_reset(w.resets_at_ms)).collect()
    }
}

/// Muse's windows: the rolling one ("Session") and the week.
pub fn muse(usage: &MuseUsage, checked_at: Millis) -> Option<UsageLimits> {
    let mut windows = Vec::new();
    if let Some(w) = usage.window.as_ref().filter(|w| w.used_percent.is_finite() && w.used_percent >= 0.0) {
        let mins = w.window_duration_mins.filter(|m| m.is_finite() && *m > 0.0 && *m < f64::from(u32::MAX)).map(|m| m.round() as u32);
        windows.push(UsageWindow { resets_at: muse_reset(w.resets_at_ms), window_minutes: mins, ..UsageWindow::new("window", UsageWindowKind::Session, "Session", w.used_percent) });
    }
    if let Some(w) = usage.weekly.as_ref().filter(|w| w.used_percent.is_finite() && w.used_percent >= 0.0) {
        windows.push(UsageWindow { resets_at: muse_reset(w.resets_at_ms), window_minutes: Some(WEEK_MINS), ..UsageWindow::new("weekly", UsageWindowKind::Weekly, "Weekly", w.used_percent) });
    }
    if windows.is_empty() {
        return None;
    }
    let mut out = UsageLimits::new(checked_at, windows);
    out.plan = usage.tier.clone().filter(|t| !t.trim().is_empty());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_snapshots_map_like_t3() {
        // A real `account/rateLimits/updated` (tests/fixtures/codex/thread_fork_native_prior_turn.1.ndjson).
        let p: CodexRateLimits = serde_json::from_value(json!({"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":61,"windowDurationMins":300,"resetsAt":1777429830},"secondary":{"usedPercent":23,"windowDurationMins":10080,"resetsAt":1777959590},"credits":null,"planType":"unknown","rateLimitReachedType":null}})).unwrap();
        let l = codex(&p, 7).unwrap();
        assert_eq!(l.plan, None);
        let got: Vec<_> = l.windows.iter().map(|w| (w.id.as_str(), w.kind, w.label.as_str(), w.used_percent, w.resets_at, w.window_minutes)).collect();
        assert_eq!(
            got,
            [
                ("primary", UsageWindowKind::Session, "Session", 61.0, Some(1_777_429_830_000), Some(300)),
                ("secondary", UsageWindowKind::Weekly, "Weekly", 23.0, Some(1_777_959_590_000), Some(10080)),
            ]
        );

        // Free plans' primary window is monthly; the individual limit counts down.
        let p: CodexRateLimits = serde_json::from_value(json!({"rateLimits":{"planType":"free","primary":{"usedPercent":10},"individualLimit":{"remainingPercent":25,"resetsAt":0}}})).unwrap();
        let l = codex(&p, 7).unwrap();
        assert_eq!(l.plan.as_deref(), Some("free"));
        let got: Vec<_> = l.windows.iter().map(|w| (w.id.as_str(), w.kind, w.used_percent, w.resets_at)).collect();
        assert_eq!(got, [("individual", UsageWindowKind::Monthly, 75.0, None), ("primary", UsageWindowKind::Monthly, 10.0, None)]);

        // The `codex` snapshot wins over another model's; a model's own isn't the account's.
        let p: CodexRateLimits = serde_json::from_value(json!({"rateLimits":{"limitId":"gpt-x","primary":{"usedPercent":90}},"rateLimitsByLimitId":{"codex":{"limitId":"codex","primary":{"usedPercent":5}}}})).unwrap();
        assert_eq!(codex(&p, 7).unwrap().windows[0].used_percent, 5.0);
        let p: CodexRateLimits = serde_json::from_value(json!({"rateLimits":{"limitId":"gpt-x","primary":{"usedPercent":90}}})).unwrap();
        assert_eq!(codex(&p, 7), None);
        assert_eq!(codex(&CodexRateLimits::default(), 7), None);
    }

    #[test]
    fn claude_events_map_like_t3() {
        let info: ClaudeRateLimitInfo =
            serde_json::from_value(json!({"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.82,"resetsAt":1777959590})).unwrap();
        let l = claude(&info, 3).unwrap();
        let w = &l.windows[0];
        assert_eq!((w.id.as_str(), w.kind, w.label.as_str(), w.window_minutes), ("seven_day", UsageWindowKind::Weekly, "Weekly", Some(10080)));
        assert!((w.used_percent - 82.0).abs() < 1e-9);
        assert!(!info.blocks());

        let rejected: ClaudeRateLimitInfo = serde_json::from_value(json!({"status":"rejected","rateLimitType":"five_hour","utilization":1.0})).unwrap();
        assert!(rejected.blocks());
        assert_eq!(claude(&rejected, 3).unwrap().windows[0].used_percent, 100.0);
        let overage: ClaudeRateLimitInfo = serde_json::from_value(json!({"status":"rejected","rateLimitType":"five_hour","isUsingOverage":true})).unwrap();
        assert!(!overage.blocks());
        // No utilization, or an unnamed bucket: nothing to show.
        assert_eq!(claude(&overage, 3), None);
        let other: ClaudeRateLimitInfo = serde_json::from_value(json!({"rateLimitType":"seven_day_overage_included","utilization":0.5})).unwrap();
        assert_eq!(claude(&other, 3), None);
    }

    #[test]
    fn muse_windows_map_like_t3() {
        let u: MuseUsage = serde_json::from_value(json!({"observedAtMs":1791444565612u64,"tier":"plus","window":{"usedPercent":112.5,"windowDurationMins":300,"resetsAtMs":1791450000000u64},"weekly":{"usedPercent":40,"resetsAtMs":1791900000000u64}})).unwrap();
        let l = muse(&u, 9).unwrap();
        assert_eq!(l.plan.as_deref(), Some("plus"));
        let got: Vec<_> = l.windows.iter().map(|w| (w.id.as_str(), w.kind, w.label.as_str(), w.used_percent, w.resets_at, w.window_minutes)).collect();
        assert_eq!(
            got,
            [
                ("window", UsageWindowKind::Session, "Session", 100.0, Some(1_791_450_000_000), Some(300)),
                ("weekly", UsageWindowKind::Weekly, "Weekly", 40.0, Some(1_791_900_000_000), Some(10080)),
            ]
        );
        // Only the window over its limit counts toward when Muse can be used again.
        assert_eq!(u.exhausted(), [1_791_450_000_000]);
        assert_eq!(muse(&MuseUsage::default(), 9), None);
        let bad: MuseUsage = serde_json::from_value(json!({"window":{"usedPercent":5,"resetsAtMs":1e300}})).unwrap();
        assert_eq!(muse(&bad, 9).unwrap().windows[0].resets_at, None);
    }
}
