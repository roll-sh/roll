//! Attack mode (Vercel's Attack Challenge Mode and its automatic mitigation, Cloudflare's "I'm
//! under attack"; docs/apps/README.md, "Firewall rules"): while it's on, every request to the
//! environment's public names gets the challenge page unless it has this hour's pass, before any
//! firewall rule lets it in. It goes on by hand (`challenge_until`, at most a day ahead) or on its
//! own (`attack_mode: {per_minute, minutes}`): once a minute's requests reach `per_minute`, it's on
//! for `minutes`, said as `attack.mode` (and in the audit log), and it ends on its own. Verified
//! search engines pass, as they pass a challenge rule, and a rule that denies still denies.

use crate::{Apps, Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// When it goes on by itself.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttackMode {
    /// A minute's requests through its public names that turn it on.
    pub per_minute: u32,
    /// How long it stays on (5–1440, default 60).
    #[serde(default = "AttackMode::default_minutes")]
    pub minutes: u32,
}

impl AttackMode {
    fn default_minutes() -> u32 {
        60
    }

    pub fn check(&self) -> Result<()> {
        if !(10..=10_000_000).contains(&self.per_minute) || !(5..=1440).contains(&self.minutes) {
            return Err(Error::BadRequest("attack_mode is {per_minute: 10–10000000 requests a minute, minutes?: 5–1440 (default 60)}".into()));
        }
        Ok(())
    }
}

/// What's kept for an environment: its setting, and until when it's on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Attack {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AttackMode>,
    /// Unix ms; on while it's ahead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

impl Attack {
    pub fn on(&self, now_ms: i64) -> bool {
        self.until.is_some_and(|u| now_ms < u)
    }
}

/// The longest it's put on by hand.
pub const MAX_AHEAD_MS: i64 = 24 * 3_600_000;

fn key(service: &str, env: &str) -> String {
    format!("attack:{service}/{env}")
}

impl Apps {
    pub(crate) async fn attack(self: &Arc<Self>, service: &str, env: &str) -> Result<Attack> {
        let k = key(service, env);
        Ok(self.db(move |s| s.state(&k)).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
    }

    pub(crate) async fn set_attack(self: &Arc<Self>, service: &str, env: &str, a: Attack) -> Result<()> {
        let k = key(service, env);
        match a == Attack::default() {
            true => {
                self.db(move |s| s.take_state(&k)).await?;
            }
            false => {
                let v = serde_json::to_string(&a).map_err(|e| Error::Internal(e.to_string()))?;
                self.db(move |s| s.set_state(&k, &v)).await?;
            }
        }
        self.live(service, env).forget_gate();
        Ok(())
    }

    /// The minute's judgment (on the environment's owner): on by itself once a minute's requests
    /// reach its `per_minute`.
    pub(crate) async fn judge_attack(self: &Arc<Self>, service: &str, env: &str, deployment: &str, last_minute_requests: i64) {
        let Ok(a) = self.attack(service, env).await else { return };
        let (Some(mode), now) = (a.mode, code_sql::now_ms()) else { return };
        if a.on(now) || last_minute_requests < mode.per_minute as i64 {
            return;
        }
        let until = now + mode.minutes as i64 * 60_000;
        if self.set_attack(service, env, Attack { until: Some(until), ..a }).await.is_err() {
            return;
        }
        let data = serde_json::json!({ "requests": last_minute_requests, "per_minute": mode.per_minute, "until": until });
        let (sid, e, dep, d) = (service.to_string(), env.to_string(), deployment.to_string(), data.clone());
        let _ = self.db(move |s| s.add_event("attack.mode", &sid, &e, Some(&dep), &d)).await;
        self.audit("attack_mode.on", service, env, Some("roll".into()), data).await;
        self.live(service, env).log.stream(crate::logs::Stream::System).line(format!("attack mode on for {} minutes: {last_minute_requests} requests in a minute (its threshold is {})", mode.minutes, mode.per_minute));
    }
}
